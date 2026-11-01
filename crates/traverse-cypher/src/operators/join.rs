//! Join operators: CrossProduct, HashJoin, OptionalMatch.
//!
//! Two contracts:
//!
//! * **Left before right.** Each operator runs its left input to completion
//!   before touching the right. The planner has no explicit Eager here, so this
//!   is what makes an earlier clause's writes visible to a later clause
//!   (`MATCH () CREATE () WITH * MATCH () CREATE ()` must create 2 + 2x4 nodes,
//!   TCK Create3 [3]). Materializing a side is O(that side); the *output* must
//!   never be materialized.
//! * **Stream the output.** A cross product holds its two inputs
//!   (O(|L| + |R|)), a hash join holds the left side's hash table (O(|L|)) and
//!   streams the right, `OPTIONAL MATCH` holds its input plus the current row's
//!   matches. A `LIMIT` above any of them stops early.

use std::collections::VecDeque;
use std::rc::Rc;

use rustc_hash::FxHashMap;

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::{JoinKey, cypher_to_join_key, eval_expr};
use crate::frame::{Frame, SymbolTable};
use crate::memory::{Charge, RowBuffer, frame_bytes};
use crate::planner::LogicalPlan;

use super::Operator;

/// Overlays the non-null slots of `right` onto a clone of `left`. The two sides bind disjoint
/// variables, so this is the joined row; a slot set on both takes the right side's value.
#[inline]
fn merged(left: &Frame, right: &Frame) -> Frame {
    let mut out = left.clone();
    for (i, val) in right.iter().enumerate() {
        if !val.is_null() {
            out[i] = val.clone();
        }
    }
    out
}

/// Timeout / limit check every 4096 rows.
#[inline]
fn tick(counter: &mut u32) -> Result<(), CypherError> {
    *counter = counter.wrapping_add(1);
    if *counter & 0xFFF == 0 {
        crate::query::check()?;
    }
    Ok(())
}

// -- CrossProductOp ----------------------------------------------------

/// Cartesian product: left input first, then right, then every pair lazily in left-major order.
pub(crate) struct CrossProductOp<'p> {
    plan: &'p LogicalPlan,
    sides: Option<(RowBuffer, RowBuffer)>,
    /// Index into the left side and into the right side.
    li: usize,
    ri: usize,
    emitted: u32,
}

impl<'p> CrossProductOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            sides: None,
            li: 0,
            ri: 0,
            emitted: 0,
        }
    }
}

impl<'p> Operator for CrossProductOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.sides.is_none() {
            let LogicalPlan::CrossProduct { left, right } = self.plan else {
                unreachable!();
            };
            // left to completion before right (see module docs)
            let left_frames = super::exec_via_operators(left, graph, symtab)?;
            let right_frames = if left_frames.is_empty() {
                // empty product whatever the right side holds
                RowBuffer::new()
            } else {
                super::exec_via_operators(right, graph, symtab)?
            };
            self.sides = Some((left_frames, right_frames));
        }
        let (left, right) = self.sides.as_ref().unwrap();
        if right.is_empty() || self.li >= left.len() {
            return Ok(None);
        }
        let out = merged(&left[self.li], &right[self.ri]);
        self.ri += 1;
        if self.ri >= right.len() {
            self.ri = 0;
            self.li += 1;
        }
        tick(&mut self.emitted)?;
        Ok(Some(out))
    }
}

// -- HashJoinOp --------------------------------------------------------

/// Equi-join. The left side runs to completion and is hashed on `left_keys`; the right side
/// streams, each frame probing on `right_keys`. Memory O(left side), charged to the query budget;
/// output is right-major (each right row, then its matching left rows).
pub(crate) struct HashJoinOp<'p> {
    plan: &'p LogicalPlan,
    state: Option<HashState<'p>>,
    emitted: u32,
}

struct HashState<'p> {
    table: FxHashMap<Vec<JoinKey>, Rc<Vec<Frame>>>,
    /// Memory charged for `table`, released with it.
    _charge: Charge,
    probe: Box<dyn Operator + 'p>,
    /// Current probe frame, its matching build frames, next index.
    current: Option<(Frame, Rc<Vec<Frame>>, usize)>,
}

impl<'p> HashJoinOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            state: None,
            emitted: 0,
        }
    }

    fn init(&mut self, graph: &mut dyn GraphView, symtab: &SymbolTable) -> Result<(), CypherError> {
        let LogicalPlan::HashJoin {
            left,
            right,
            left_keys,
            ..
        } = self.plan
        else {
            unreachable!();
        };
        // build: left side to completion, hashed; buckets are `Rc`-shared so a probe frame holds
        // its matches without re-hashing per output row
        let mut table: FxHashMap<Vec<JoinKey>, Rc<Vec<Frame>>> = FxHashMap::default();
        let mut charge = Charge::new();
        let mut build_op = super::build_operator_tree(left);
        let mut count: u32 = 0;
        while let Some(frame) = build_op.next(graph, symtab)? {
            let key: Vec<JoinKey> = left_keys
                .iter()
                .map(|k| cypher_to_join_key(&eval_expr(k, &frame, &*graph, symtab)?))
                .collect::<Result<_, _>>()?;
            // frame plus the key's share of the table entry
            charge.add(frame_bytes(&frame) + 32 * key.len() + 48)?;
            let bucket = table.entry(key).or_insert_with(|| Rc::new(Vec::new()));
            Rc::get_mut(bucket)
                .expect("hash-join bucket is uniquely owned during the build phase")
                .push(frame);
            tick(&mut count)?;
        }
        charge.flush()?;
        self.state = Some(HashState {
            table,
            _charge: charge,
            probe: super::build_operator_tree(right),
            current: None,
        });
        Ok(())
    }
}

impl<'p> Operator for HashJoinOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.state.is_none() {
            self.init(graph, symtab)?;
        }
        let LogicalPlan::HashJoin { right_keys, .. } = self.plan else {
            unreachable!();
        };
        let st = self.state.as_mut().unwrap();
        if st.table.is_empty() {
            return Ok(None);
        }
        loop {
            if let Some((probe_frame, matches, idx)) = st.current.as_mut() {
                if *idx < matches.len() {
                    let out = merged(&matches[*idx], probe_frame);
                    *idx += 1;
                    tick(&mut self.emitted)?;
                    return Ok(Some(out));
                }
                st.current = None;
            }
            // probe: next right frame, look up its key
            let Some(probe_frame) = st.probe.next(graph, symtab)? else {
                return Ok(None);
            };
            let key: Vec<JoinKey> = right_keys
                .iter()
                .map(|k| cypher_to_join_key(&eval_expr(k, &probe_frame, &*graph, symtab)?))
                .collect::<Result<_, _>>()?;
            if let Some(matches) = st.table.get(&key) {
                st.current = Some((probe_frame, Rc::clone(matches), 0));
            }
            tick(&mut self.emitted)?;
        }
    }
}

// -- OptionalMatchOp ---------------------------------------------------

/// `OPTIONAL MATCH`: input to completion first (left before right), then per input frame the
/// matches seeded with it (or the frame itself when there are none), streamed one row at a time.
/// Holds the input plus the current row's matches.
pub(crate) struct OptionalMatchOp<'p> {
    plan: &'p LogicalPlan,
    input: Option<RowBuffer>,
    next_input: usize,
    pending: VecDeque<Frame>,
    seen: u32,
}

impl<'p> OptionalMatchOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            input: None,
            next_input: 0,
            pending: VecDeque::new(),
            seen: 0,
        }
    }
}

impl<'p> Operator for OptionalMatchOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        let LogicalPlan::OptionalMatch {
            input,
            match_plan,
            new_variables: _,
        } = self.plan
        else {
            unreachable!();
        };
        if self.input.is_none() {
            self.input = Some(super::exec_via_operators(input, graph, symtab)?);
        }
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return Ok(Some(frame));
            }
            let input_frames = self.input.as_mut().unwrap();
            if self.next_input >= input_frames.len() {
                return Ok(None);
            }
            let frame = std::mem::take(&mut input_frames[self.next_input]);
            self.next_input += 1;
            let matches =
                super::exec_via_operators_seeded(match_plan, frame.clone(), graph, symtab)?;
            if matches.is_empty() {
                self.pending.push_back(frame);
            } else {
                self.pending.extend(matches);
            }
            tick(&mut self.seen)?;
        }
    }
}
