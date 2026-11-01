//! One-pass streaming aggregation for the generic operator path.
//!
//! `AggregateOp` has scan- and expand-fused fast paths; anything else would
//! otherwise materialize its whole input first (`MATCH (a) MATCH (b) RETURN
//! count(*)` would hold every pair of the cross product). Here rows aggregate as
//! they arrive: memory is the group table (plus what `collect()` gathers),
//! charged to the query budget, not the input.
//!
//! Covers the incremental aggregates: `count(*)`, `count(x)`, `sum`, `avg`,
//! `min`, `max`, `collect`, non-DISTINCT and as the *whole* projection item
//! (`count(*) * 2`, `collect(x)[0..3]`, `percentileDisc`, `stDev` and DISTINCT
//! forms take the materializing path). Semantics match
//! `executor::eval_direct_aggregate` exactly: nulls skipped, `sum` stays integer
//! until a float shows up, `avg` always float, `min`/`max` by Cypher ordering,
//! `collect` keeps input order.

use rustc_hash::FxHashMap;

use traverse_core::tx::view::GraphView;

use crate::ast::{Expr, ProjectionItem};
use crate::error::CypherError;
use crate::executor::{JoinKey, compare_values, cypher_to_join_key, eval_expr, projection_name};
use crate::frame::{SymbolTable, new_frame};
use crate::memory::{Charge, RowBuffer, value_heap_bytes};
use crate::planner::{LogicalPlan, is_aggregate_function};
use crate::types::CypherValue;

#[derive(Clone, Copy)]
enum AggFn {
    CountStar,
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Collect,
}

/// One aggregate column the streaming path can compute.
pub(crate) struct StreamingAgg<'p> {
    f: AggFn,
    arg: Option<&'p Expr>,
    slot: Option<usize>,
}

/// Recognizes the projection items this path supports; `None` = use the materializing aggregate.
pub(crate) fn try_plan<'p>(
    aggs: &'p [ProjectionItem],
    symtab: &SymbolTable,
) -> Option<Vec<StreamingAgg<'p>>> {
    let mut out = Vec::with_capacity(aggs.len());
    for item in aggs {
        let Expr::FunctionCall {
            name,
            distinct,
            args,
        } = &item.expr
        else {
            return None;
        };
        if *distinct || !is_aggregate_function(name) {
            return None;
        }
        let f = match (name.as_str(), args.len()) {
            ("count", 0) => AggFn::CountStar,
            ("count", 1) => AggFn::Count,
            ("sum", 1) => AggFn::Sum,
            ("avg", 1) => AggFn::Avg,
            ("min", 1) => AggFn::Min,
            ("max", 1) => AggFn::Max,
            ("collect", 1) => AggFn::Collect,
            _ => return None,
        };
        // the argument must not aggregate itself (`sum(count(*))` fails validation anyway)
        if let Some(a) = args.first()
            && contains_aggregate(a)
        {
            return None;
        }
        out.push(StreamingAgg {
            f,
            arg: args.first(),
            slot: symtab.slot(&projection_name(item)),
        });
    }
    Some(out)
}

#[recursive::recursive]
fn contains_aggregate(e: &Expr) -> bool {
    match e {
        Expr::FunctionCall { name, args, .. } => {
            is_aggregate_function(name) || args.iter().any(contains_aggregate)
        }
        Expr::Property(b, _) | Expr::UnaryOp(_, b) | Expr::IsNull(b) | Expr::IsNotNull(b) => {
            contains_aggregate(b)
        }
        Expr::BinaryOp(a, _, b)
        | Expr::In(a, b)
        | Expr::StringOp(a, _, b)
        | Expr::ListIndex(a, b) => contains_aggregate(a) || contains_aggregate(b),
        Expr::Comparison(a, rest) => {
            contains_aggregate(a) || rest.iter().any(|(_, e)| contains_aggregate(e))
        }
        Expr::ListLiteral(items) => items.iter().any(contains_aggregate),
        Expr::MapLiteral(pairs) => pairs.iter().any(|(_, e)| contains_aggregate(e)),
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            operand.as_deref().is_some_and(contains_aggregate)
                || whens
                    .iter()
                    .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || else_expr.as_deref().is_some_and(contains_aggregate)
        }
        Expr::ListSlice(a, b, c) => {
            contains_aggregate(a)
                || b.as_deref().is_some_and(contains_aggregate)
                || c.as_deref().is_some_and(contains_aggregate)
        }
        Expr::IsTyped { expr, .. } => contains_aggregate(expr),
        Expr::Literal(_) | Expr::Variable(_) | Expr::Parameter(_) => false,
        // comprehensions, subqueries, reduce, map projections: inner aggregates are scoped, but
        // stay conservative
        _ => true,
    }
}

enum Acc {
    Count(i64),
    Sum {
        int: i64,
        float: f64,
        all_int: bool,
        any: bool,
    },
    Avg {
        sum: f64,
        n: i64,
    },
    Min(Option<CypherValue>),
    Max(Option<CypherValue>),
    Collect(Vec<CypherValue>),
}

impl Acc {
    fn new(f: AggFn) -> Acc {
        match f {
            AggFn::CountStar | AggFn::Count => Acc::Count(0),
            AggFn::Sum => Acc::Sum {
                int: 0,
                float: 0.0,
                all_int: true,
                any: false,
            },
            AggFn::Avg => Acc::Avg { sum: 0.0, n: 0 },
            AggFn::Min => Acc::Min(None),
            AggFn::Max => Acc::Max(None),
            AggFn::Collect => Acc::Collect(Vec::new()),
        }
    }

    /// Folds in a non-null value. Returns the bytes newly retained (only `collect` keeps any) for
    /// the caller to charge.
    fn add(&mut self, v: CypherValue) -> Result<usize, CypherError> {
        match self {
            Acc::Count(n) => *n += 1,
            Acc::Sum {
                int,
                float,
                all_int,
                any,
            } => {
                *any = true;
                match &v {
                    CypherValue::Integer(i) => {
                        *int = int.checked_add(*i).ok_or_else(|| {
                            CypherError::runtime("integer overflow in sum()")
                                .with_status(crate::status::GqlStatus::NumericOutOfRange)
                        })?;
                        *float += *i as f64;
                    }
                    CypherValue::Float(f) => {
                        *all_int = false;
                        *float += f;
                    }
                    other => {
                        return Err(CypherError::type_error(format!(
                            "sum() requires numeric values, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
            Acc::Avg { sum, n } => match v.to_f64() {
                Some(f) => {
                    *sum += f;
                    *n += 1;
                }
                None => {
                    return Err(CypherError::type_error(format!(
                        "avg() requires numeric values, got {}",
                        v.type_name()
                    )));
                }
            },
            Acc::Min(best) => {
                if best
                    .as_ref()
                    .is_none_or(|b| compare_values(&v, b) == std::cmp::Ordering::Less)
                {
                    *best = Some(v);
                }
            }
            Acc::Max(best) => {
                if best
                    .as_ref()
                    .is_none_or(|b| compare_values(&v, b) == std::cmp::Ordering::Greater)
                {
                    *best = Some(v);
                }
            }
            Acc::Collect(items) => {
                let bytes = std::mem::size_of::<CypherValue>() + value_heap_bytes(&v);
                items.push(v);
                return Ok(bytes);
            }
        }
        Ok(0)
    }

    fn finish(self) -> CypherValue {
        match self {
            Acc::Count(n) => CypherValue::Integer(n),
            Acc::Sum {
                int,
                float,
                all_int,
                any,
            } => {
                if !any {
                    CypherValue::Integer(0)
                } else if all_int {
                    CypherValue::Integer(int)
                } else {
                    CypherValue::Float(float)
                }
            }
            Acc::Avg { sum, n } => {
                if n == 0 {
                    CypherValue::Null
                } else {
                    CypherValue::Float(sum / n as f64)
                }
            }
            Acc::Min(v) | Acc::Max(v) => v.unwrap_or(CypherValue::Null),
            Acc::Collect(items) => CypherValue::List(items),
        }
    }
}

struct Group {
    key_vals: Vec<CypherValue>,
    accs: Vec<Acc>,
}

/// Pulls `input` once into a group table. Output frames are charged like any buffer.
pub(crate) fn exec_streaming_aggregate(
    input: &LogicalPlan,
    keys: &[ProjectionItem],
    specs: &[StreamingAgg<'_>],
    graph: &mut dyn GraphView,
    symtab: &SymbolTable,
) -> Result<RowBuffer, CypherError> {
    let mut op = super::build_operator_tree(input);
    let mut map: FxHashMap<Vec<JoinKey>, usize> = FxHashMap::default();
    let mut groups: Vec<Group> = Vec::new();
    let mut charge = Charge::new();
    let mut rows: u32 = 0;

    while let Some(frame) = op.next(graph, symtab)? {
        rows = rows.wrapping_add(1);
        if rows & 0xFFF == 0 {
            crate::query::check()?;
        }
        let key_vals: Vec<CypherValue> = keys
            .iter()
            .map(|k| eval_expr(&k.expr, &frame, &*graph, symtab))
            .collect::<Result<_, _>>()?;
        let hash_key: Vec<JoinKey> = key_vals
            .iter()
            .map(cypher_to_join_key)
            .collect::<Result<_, _>>()?;
        let idx = match map.get(&hash_key) {
            Some(&i) => i,
            None => {
                let bytes = 96
                    + hash_key.len() * 40
                    + key_vals.iter().map(value_heap_bytes).sum::<usize>()
                    + specs.len() * std::mem::size_of::<Acc>();
                charge.add(bytes)?;
                let idx = groups.len();
                map.insert(hash_key, idx);
                groups.push(Group {
                    key_vals,
                    accs: specs.iter().map(|s| Acc::new(s.f)).collect(),
                });
                idx
            }
        };
        let group = &mut groups[idx];
        for (spec, acc) in specs.iter().zip(group.accs.iter_mut()) {
            let retained = match spec.f {
                AggFn::CountStar => acc.add(CypherValue::Null)?,
                _ => {
                    let v = eval_expr(spec.arg.unwrap(), &frame, &*graph, symtab)?;
                    if v.is_null() {
                        continue;
                    }
                    acc.add(v)?
                }
            };
            if retained > 0 {
                charge.add(retained)?;
            }
        }
    }
    drop(op);

    // no rows and no grouping keys: still one row
    if groups.is_empty() && keys.is_empty() {
        groups.push(Group {
            key_vals: Vec::new(),
            accs: specs.iter().map(|s| Acc::new(s.f)).collect(),
        });
    }

    let key_slots: Vec<Option<usize>> = keys
        .iter()
        .map(|k| symtab.slot(&projection_name(k)))
        .collect();
    let mut out = RowBuffer::with_capacity(groups.len());
    for group in groups {
        let mut row = new_frame(symtab);
        for (slot, v) in key_slots.iter().zip(group.key_vals) {
            if let Some(s) = slot {
                row[*s] = v.into();
            }
        }
        for (spec, acc) in specs.iter().zip(group.accs) {
            if let Some(s) = spec.slot {
                row[s] = acc.finish().into();
            }
        }
        out.push(row)?;
    }
    Ok(out)
}
