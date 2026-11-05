//! Grouped aggregation over chunks. Keys and arguments evaluate vectorized when the input's rows
//! vary along one group, row by row otherwise; `count(*)` over a factorized chunk adds the row
//! product without iterating. The state is separate from the operator so parallel workers can
//! aggregate their morsels into partial groups the scheduler merges; finalization follows
//! Cypher's aggregate semantics exactly.

use std::collections::BTreeMap;

use rustc_hash::{FxHashMap, FxHashSet};
use traverse_core::graph::types::NodeId;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::expr::eval::EvalCtx;
use super::super::expr::scalar::compare_values;
use super::super::expr::{CompiledExpr, EvalScratch, Operand};
use super::super::plan::{AggSpec, PostAgg, SeedRow};
use super::super::values::{JoinKey, join_key, join_key_ref};
use super::super::vector::{ValueRef, Vector};
use super::{PhysOperator, Staged};

/// One aggregate's running state.
enum Acc {
    Count(i64),
    Sum {
        int_sum: i64,
        float_sum: f64,
        has_float: bool,
        any: bool,
    },
    Avg {
        sum: f64,
        n: i64,
    },
    Min(Option<CypherValue>),
    Max(Option<CypherValue>),
    Collect(Vec<CypherValue>),
    /// Lists appended end to end (`__concat`: merging partial `collect`s).
    Concat(Vec<CypherValue>),
    /// Every non-null value, finalized at the end (percentiles, deviations, APOC aggregates).
    Values(Vec<CypherValue>),
    /// `(item, value)` pairs for `apoc.agg.maxItems` / `minItems`.
    Pairs(Vec<(CypherValue, CypherValue)>),
}

impl Acc {
    fn new(func: &str) -> Self {
        match func {
            "count" => Acc::Count(0),
            "sum" => Acc::Sum {
                int_sum: 0,
                float_sum: 0.0,
                has_float: false,
                any: false,
            },
            "avg" => Acc::Avg { sum: 0.0, n: 0 },
            "min" => Acc::Min(None),
            "max" => Acc::Max(None),
            "collect" => Acc::Collect(Vec::new()),
            "__concat" => Acc::Concat(Vec::new()),
            "apoc.agg.maxitems" | "apoc.agg.minitems" => Acc::Pairs(Vec::new()),
            _ => Acc::Values(Vec::new()),
        }
    }

    /// Feeds one non-null value (or any value for `count(*)`, which passes `None`).
    fn add(&mut self, v: Option<CypherValue>) -> Result<(), CypherError> {
        match self {
            Acc::Count(c) => *c += 1,
            Acc::Sum {
                int_sum,
                float_sum,
                has_float,
                any,
            } => match v {
                Some(CypherValue::Integer(i)) => {
                    *int_sum = int_sum.wrapping_add(i);
                    *float_sum += i as f64;
                    *any = true;
                }
                Some(CypherValue::Float(f)) => {
                    *float_sum += f;
                    *has_float = true;
                    *any = true;
                }
                Some(other) => {
                    return Err(CypherError::type_error(format!(
                        "sum() requires numeric values, got {}",
                        other.type_name()
                    )));
                }
                None => {}
            },
            Acc::Avg { sum, n } => match v.as_ref().and_then(|v| v.to_f64()) {
                Some(f) => {
                    *sum += f;
                    *n += 1;
                }
                None => {
                    return Err(CypherError::type_error(format!(
                        "avg() requires numeric values, got {}",
                        v.map(|v| v.type_name()).unwrap_or("NULL")
                    )));
                }
            },
            Acc::Min(best) => {
                if let Some(v) = v {
                    match best {
                        Some(b) if compare_values(&v, b) != std::cmp::Ordering::Less => {}
                        _ => *best = Some(v),
                    }
                }
            }
            Acc::Max(best) => {
                if let Some(v) = v {
                    match best {
                        Some(b) if compare_values(&v, b) != std::cmp::Ordering::Greater => {}
                        _ => *best = Some(v),
                    }
                }
            }
            Acc::Collect(items) | Acc::Values(items) => {
                if let Some(v) = v {
                    items.push(v);
                }
            }
            Acc::Concat(items) => {
                if let Some(CypherValue::List(more)) = v {
                    items.extend(more);
                }
            }
            Acc::Pairs(_) => {}
        }
        Ok(())
    }

    /// Feeds one non-null lane without owning it where the type allows.
    #[inline]
    fn add_ref(&mut self, v: ValueRef<'_>) -> Result<(), CypherError> {
        match (&mut *self, v) {
            (Acc::Count(c), _) => *c += 1,
            (
                Acc::Sum {
                    int_sum,
                    float_sum,
                    any,
                    ..
                },
                ValueRef::Int(i),
            ) => {
                *int_sum = int_sum.wrapping_add(i);
                *float_sum += i as f64;
                *any = true;
            }
            (
                Acc::Sum {
                    float_sum,
                    has_float,
                    any,
                    ..
                },
                ValueRef::Float(f),
            ) => {
                *float_sum += f;
                *has_float = true;
                *any = true;
            }
            (Acc::Avg { sum, n }, ValueRef::Int(i)) => {
                *sum += i as f64;
                *n += 1;
            }
            (Acc::Avg { sum, n }, ValueRef::Float(f)) => {
                *sum += f;
                *n += 1;
            }
            (Acc::Min(Some(CypherValue::Integer(b))), ValueRef::Int(i)) => {
                if i < *b {
                    *b = i;
                }
            }
            (Acc::Max(Some(CypherValue::Integer(b))), ValueRef::Int(i)) => {
                if i > *b {
                    *b = i;
                }
            }
            (Acc::Min(Some(CypherValue::String(b))), ValueRef::Str(s)) => {
                if s < b.as_str() {
                    *b = s.to_string();
                }
            }
            (Acc::Max(Some(CypherValue::String(b))), ValueRef::Str(s)) => {
                if s > b.as_str() {
                    *b = s.to_string();
                }
            }
            (Acc::Concat(items), ValueRef::Any(CypherValue::List(more))) => {
                items.extend(more.iter().cloned());
            }
            (Acc::Concat(_), _) => {}
            (acc, v) => acc.add(Some(v.to_owned()))?,
        }
        Ok(())
    }

    /// Folds another worker's state for the same aggregate into this one; `other` came later
    /// in morsel order, so order-sensitive states append.
    fn merge(&mut self, other: Acc) -> Result<(), CypherError> {
        match (self, other) {
            (Acc::Count(a), Acc::Count(b)) => *a = a.saturating_add(b),
            (
                Acc::Sum {
                    int_sum,
                    float_sum,
                    has_float,
                    any,
                },
                Acc::Sum {
                    int_sum: i2,
                    float_sum: f2,
                    has_float: h2,
                    any: a2,
                },
            ) => {
                *int_sum = int_sum.wrapping_add(i2);
                *float_sum += f2;
                *has_float |= h2;
                *any |= a2;
            }
            (Acc::Avg { sum, n }, Acc::Avg { sum: s2, n: n2 }) => {
                *sum += s2;
                *n += n2;
            }
            (a @ Acc::Min(_), Acc::Min(b)) | (a @ Acc::Max(_), Acc::Max(b)) => a.add(b)?,
            (Acc::Collect(a), Acc::Collect(b))
            | (Acc::Concat(a), Acc::Concat(b))
            | (Acc::Values(a), Acc::Values(b)) => a.extend(b),
            (Acc::Pairs(a), Acc::Pairs(b)) => a.extend(b),
            _ => unreachable!("partial states of one aggregate share a shape"),
        }
        Ok(())
    }
}

/// Integer keys within this many of the first one are looked up by offset, not by hash.
const DIRECT_INT_WINDOW: usize = 4096;

/// Group lookup by key: one typed key avoids building a key value per row, and a single
/// integer key near the first one seen (ages, years, small codes) is an array index.
#[derive(Default)]
struct Index {
    /// Single non-string key.
    one: FxHashMap<JoinKey, usize>,
    /// Single integer key by offset from `direct_base`: the group index plus one, 0 = none.
    direct: Vec<u32>,
    direct_base: i64,
    /// Single string key, looked up by slice.
    strs: FxHashMap<Box<str>, usize>,
    /// Several keys.
    many: FxHashMap<Vec<JoinKey>, usize>,
}

impl Index {
    #[inline]
    fn direct_slot(&self, i: i64) -> Option<usize> {
        if self.direct.is_empty() {
            return None;
        }
        let off = i.wrapping_sub(self.direct_base);
        (0..self.direct.len() as i64)
            .contains(&off)
            .then_some(off as usize)
    }

    #[inline]
    fn find_one(&self, v: ValueRef<'_>) -> Option<usize> {
        match v {
            ValueRef::Int(i) => match self.direct_slot(i) {
                Some(slot) => {
                    let g = self.direct[slot];
                    (g != 0).then(|| g as usize - 1)
                }
                None => self.one.get(&JoinKey::Integer(i)).copied(),
            },
            ValueRef::Str(s) => self.strs.get(s).copied(),
            ValueRef::Any(CypherValue::String(s)) => self.strs.get(s.as_str()).copied(),
            other => self.one.get(&join_key_ref(other)).copied(),
        }
    }

    fn insert_one(&mut self, v: ValueRef<'_>, idx: usize) {
        match v {
            ValueRef::Int(i) => {
                if self.direct.is_empty() && self.one.is_empty() {
                    self.direct = vec![0; DIRECT_INT_WINDOW];
                    self.direct_base = i.saturating_sub(DIRECT_INT_WINDOW as i64 / 2);
                }
                match self.direct_slot(i) {
                    Some(slot) => self.direct[slot] = idx as u32 + 1,
                    None => {
                        self.one.insert(JoinKey::Integer(i), idx);
                    }
                }
            }
            ValueRef::Str(s) => {
                self.strs.insert(Box::from(s), idx);
            }
            ValueRef::Any(CypherValue::String(s)) => {
                self.strs.insert(Box::from(s.as_str()), idx);
            }
            other => {
                self.one.insert(join_key_ref(other), idx);
            }
        }
    }

    fn clear(&mut self) {
        self.one.clear();
        self.direct = Vec::new();
        self.strs.clear();
        self.many.clear();
    }
}

/// A group's state. A partial group (a worker's) keeps a DISTINCT aggregate's distinct values
/// in a list instead of feeding them, since the merge has to deduplicate across workers first.
pub(crate) struct GroupAcc {
    /// Key values.
    pub(crate) keys: Vec<CypherValue>,
    accs: Vec<Acc>,
    distinct: Vec<Option<FxHashSet<JoinKey>>>,
    /// Extra arguments, evaluated on the group's first row.
    extras: Vec<Vec<CypherValue>>,
    /// Position of the group's first row across the workers, for the merged output order.
    pub(crate) first: u64,
}

/// Folds partial group `b` into `a` (`a` saw its first row earlier).
pub(crate) fn merge_group(
    a: &mut GroupAcc,
    b: GroupAcc,
    aggs: &[AggSpec],
) -> Result<(), CypherError> {
    for (i, (spec, acc)) in aggs.iter().zip(b.accs).enumerate() {
        if spec.distinct {
            let Acc::Collect(values) = acc else {
                unreachable!("a partial DISTINCT aggregate lists its values");
            };
            let set = a.distinct[i]
                .as_mut()
                .expect("a DISTINCT aggregate has its set");
            for v in values {
                if set.insert(join_key(&v)) {
                    a.accs[i].add(Some(v))?;
                }
            }
        } else {
            a.accs[i].merge(acc)?;
        }
    }
    Ok(())
}

/// Turns a merged partial group into a final one: DISTINCT aggregates get their values fed.
pub(crate) fn seal_group(g: &mut GroupAcc, aggs: &[AggSpec]) -> Result<(), CypherError> {
    for (i, spec) in aggs.iter().enumerate() {
        if !spec.distinct {
            continue;
        }
        let values = match std::mem::replace(&mut g.accs[i], Acc::new(&spec.func)) {
            Acc::Collect(v) => v,
            _ => unreachable!("a partial DISTINCT aggregate lists its values"),
        };
        for v in values {
            g.accs[i].add(Some(v))?;
        }
    }
    Ok(())
}

/// The aggregation state: groups by key with their accumulators.
pub(crate) struct AggState<'p> {
    keys: &'p [(CompiledExpr, DataPos)],
    aggs: &'p [AggSpec],
    scratch: EvalScratch<'p>,
    index: Index,
    groups: Vec<GroupAcc>,
    /// A worker's state: DISTINCT values are listed, not fed.
    partial: bool,
    /// For argument slot `2j` (the value) and `2j + 1` (the item) of aggregate `j`: the slot
    /// whose expression it shares, by printed form, so a chunk evaluates each expression once.
    shared: Vec<usize>,
}

impl<'p> AggState<'p> {
    pub(crate) fn new(
        keys: &'p [(CompiledExpr, DataPos)],
        aggs: &'p [AggSpec],
        partial: bool,
    ) -> Self {
        let mut sigs: Vec<Option<String>> = Vec::with_capacity(aggs.len() * 2);
        for a in aggs {
            let is_pair = matches!(a.func.as_str(), "apoc.agg.maxitems" | "apoc.agg.minitems");
            let (value, item) = if is_pair {
                (a.extra.first(), a.arg.as_ref())
            } else {
                (a.arg.as_ref(), None)
            };
            sigs.push(value.map(|e| format!("{:?}", e.source)));
            sigs.push(item.map(|e| format!("{:?}", e.source)));
        }
        let shared: Vec<usize> = (0..sigs.len())
            .map(|i| match &sigs[i] {
                Some(sig) => (0..i)
                    .find(|&j| sigs[j].as_deref() == Some(sig.as_str()))
                    .unwrap_or(i),
                None => i,
            })
            .collect();
        Self {
            keys,
            aggs,
            scratch: EvalScratch::new(),
            index: Index::default(),
            groups: Vec::new(),
            partial,
            shared,
        }
    }

    fn acc_for(spec: &AggSpec, partial: bool) -> Acc {
        if partial && spec.distinct {
            Acc::Collect(Vec::new())
        } else {
            Acc::new(&spec.func)
        }
    }

    /// The one group of a keyless aggregate over no rows (`partial`: in a worker's shape).
    pub(crate) fn default_group(aggs: &[AggSpec], partial: bool) -> GroupAcc {
        GroupAcc {
            keys: Vec::new(),
            accs: aggs.iter().map(|a| Self::acc_for(a, partial)).collect(),
            distinct: aggs
                .iter()
                .map(|a| a.distinct.then(FxHashSet::default))
                .collect(),
            extras: aggs.iter().map(|_| Vec::new()).collect(),
            first: 0,
        }
    }

    /// The groups, consumed.
    pub(crate) fn into_groups(self) -> Vec<GroupAcc> {
        self.groups
    }

    fn clear(&mut self) {
        self.index.clear();
        self.groups.clear();
    }

    fn group_for(
        &mut self,
        key_vals: Vec<CypherValue>,
        extras: impl FnOnce() -> Result<Vec<Vec<CypherValue>>, CypherError>,
    ) -> Result<usize, CypherError> {
        match key_vals.len() {
            0 => {
                if let Some(g) = self.groups.first() {
                    debug_assert!(g.keys.is_empty());
                    return Ok(0);
                }
            }
            1 => {
                if let Some(idx) = self.index.find_one(ValueRef::of(&key_vals[0])) {
                    return Ok(idx);
                }
            }
            _ => {
                let hash: Vec<JoinKey> = key_vals.iter().map(join_key).collect();
                if let Some(&idx) = self.index.many.get(&hash) {
                    return Ok(idx);
                }
            }
        }
        let extras = extras()?;
        Ok(self.new_group(key_vals, extras))
    }

    /// Adds a group for `key_vals`, indexing it.
    fn new_group(&mut self, key_vals: Vec<CypherValue>, extras: Vec<Vec<CypherValue>>) -> usize {
        let idx = self.groups.len();
        match key_vals.len() {
            0 => {}
            1 => self.index.insert_one(ValueRef::of(&key_vals[0]), idx),
            _ => {
                let hash: Vec<JoinKey> = key_vals.iter().map(join_key).collect();
                self.index.many.insert(hash, idx);
            }
        }
        let partial = self.partial;
        self.groups.push(GroupAcc {
            keys: key_vals,
            accs: self
                .aggs
                .iter()
                .map(|a| Self::acc_for(a, partial))
                .collect(),
            distinct: self
                .aggs
                .iter()
                .map(|a| a.distinct.then(FxHashSet::default))
                .collect(),
            extras,
            first: 0,
        });
        idx
    }

    /// Feeds one row's aggregate values into group `g`.
    fn feed(
        &mut self,
        g: usize,
        values: Vec<Option<CypherValue>>,
        pair_items: Vec<Option<CypherValue>>,
    ) -> Result<(), CypherError> {
        let group = &mut self.groups[g];
        for (i, spec) in self.aggs.iter().enumerate() {
            let v = values[i].clone();
            match &mut group.accs[i] {
                Acc::Count(c) if spec.arg.is_none() => {
                    *c += 1;
                    continue;
                }
                Acc::Pairs(pairs) => {
                    // apoc.agg.maxItems / minItems: (item, value) with a non-null value
                    if let (Some(item), Some(val)) = (pair_items[i].clone(), v)
                        && !val.is_null()
                    {
                        pairs.push((item, val));
                    }
                    continue;
                }
                _ => {}
            }
            let Some(v) = v else { continue };
            if v.is_null() {
                continue;
            }
            if let Some(set) = &mut group.distinct[i]
                && !set.insert(join_key(&v))
            {
                continue;
            }
            group.accs[i].add(Some(v))?;
        }
        Ok(())
    }

    /// Aggregates the rows of the current chunk of `rs` over `in_groups`. `post_empty`: the
    /// operator computes nothing over the finished groups (lets `count(*)` take the row product).
    pub(crate) fn consume_chunk(
        &mut self,
        rs: &ResultSet,
        cx: &ExecCtx<'_>,
        in_groups: &[GroupId],
        post_empty: bool,
    ) -> Result<(), CypherError> {
        let unflat: Vec<GroupId> = in_groups
            .iter()
            .copied()
            .filter(|&g| rs.group(g).active && rs.group(g).flat.is_none())
            .collect();
        // count(*) only, no keys: the row product
        if self.keys.is_empty()
            && self
                .aggs
                .iter()
                .all(|a| a.func == "count" && a.arg.is_none() && !a.distinct)
            && post_empty
        {
            let rows = rs.row_count(in_groups);
            let n = self.aggs.len();
            let g = self.group_for(Vec::new(), || Ok((0..n).map(|_| Vec::new()).collect()))?;
            for acc in &mut self.groups[g].accs {
                if let Acc::Count(c) = acc {
                    *c = c.saturating_add(rows.min(i64::MAX as usize) as i64);
                }
            }
            return Ok(());
        }
        if unflat.len() <= 1 {
            // vectorized: one operand per key and argument over the varying group
            let group = unflat.first().copied();
            let ecx = EvalCtx::new(rs, cx.view(), group);
            let n = ecx.n;
            let mut key_ops: Vec<Operand<'_>> = Vec::with_capacity(self.keys.len());
            for (k, _) in self.keys {
                key_ops.push(k.eval(&ecx, &mut self.scratch)?);
            }
            // one evaluation per distinct argument expression: `min(x), max(x), sum(x)` read
            // `x` once
            let mut evaluated: Vec<(usize, Operand<'_>)> = Vec::new();
            let shared = &self.shared;
            let mut eval_shared = |e: &'p CompiledExpr,
                                   slot: usize,
                                   sc: &mut EvalScratch<'p>|
             -> Result<usize, CypherError> {
                let owner = shared[slot];
                if let Some(i) = evaluated.iter().position(|(o, _)| *o == owner) {
                    return Ok(i);
                }
                let op = e.eval(&ecx, sc)?;
                evaluated.push((owner, op));
                Ok(evaluated.len() - 1)
            };
            let mut arg_at: Vec<Option<usize>> = Vec::with_capacity(self.aggs.len());
            let mut item_at: Vec<Option<usize>> = Vec::with_capacity(self.aggs.len());
            for (j, a) in self.aggs.iter().enumerate() {
                let is_pair = matches!(a.func.as_str(), "apoc.agg.maxitems" | "apoc.agg.minitems");
                if is_pair {
                    // value from the second argument, item from the first
                    item_at.push(match &a.arg {
                        Some(e) => Some(eval_shared(e, 2 * j + 1, &mut self.scratch)?),
                        None => None,
                    });
                    arg_at.push(match a.extra.first() {
                        Some(e) => Some(eval_shared(e, 2 * j, &mut self.scratch)?),
                        None => None,
                    });
                } else {
                    item_at.push(None);
                    arg_at.push(match &a.arg {
                        Some(e) => Some(eval_shared(e, 2 * j, &mut self.scratch)?),
                        None => None,
                    });
                }
            }
            let arg_ops: Vec<Option<&Operand<'_>>> =
                arg_at.iter().map(|i| i.map(|i| &evaluated[i].1)).collect();
            let item_ops: Vec<Option<&Operand<'_>>> =
                item_at.iter().map(|i| i.map(|i| &evaluated[i].1)).collect();
            let aggs = self.aggs;
            // keyless simple aggregates over a typed column fold the whole chunk at once
            if key_ops.is_empty() && self.bulk_fold(&arg_ops, n)? {
                return Ok(());
            }
            // count(*) by one integer key: the lanes straight into the direct table
            if key_ops.len() == 1
                && self.aggs.iter().all(|a| {
                    a.func == "count" && a.arg.is_none() && !a.distinct && a.extra.is_empty()
                })
            {
                let ints: Option<(Vec<i64>, Vec<bool>)> = match &key_ops[0] {
                    Operand::Owned(Vector::Int { vals, valid }) if vals.len() == n => {
                        Some((vals.clone(), valid.clone()))
                    }
                    Operand::Ref(Vector::Int { vals, valid }, Some(sel)) if sel.len() == n => {
                        Some((
                            sel.iter().map(|&l| vals[l as usize]).collect(),
                            sel.iter().map(|&l| valid[l as usize]).collect(),
                        ))
                    }
                    Operand::Ref(Vector::Int { vals, valid }, None) if vals.len() == n => {
                        Some((vals.clone(), valid.clone()))
                    }
                    _ => None,
                };
                if let Some((vals, valid)) = ints {
                    let n_aggs = self.aggs.len();
                    for i in 0..n {
                        let g = if valid[i] {
                            match self.index.find_one(ValueRef::Int(vals[i])) {
                                Some(g) => g,
                                None => self.new_group(
                                    vec![CypherValue::Integer(vals[i])],
                                    (0..n_aggs).map(|_| Vec::new()).collect(),
                                ),
                            }
                        } else {
                            match self.index.find_one(ValueRef::Null) {
                                Some(g) => g,
                                None => self.new_group(
                                    vec![CypherValue::Null],
                                    (0..n_aggs).map(|_| Vec::new()).collect(),
                                ),
                            }
                        };
                        for acc in &mut self.groups[g].accs {
                            if let Acc::Count(c) = acc {
                                *c += 1;
                            }
                        }
                    }
                    return Ok(());
                }
            }
            let extras_at = |sc: &mut EvalScratch<'p>,
                             i: usize|
             -> Result<Vec<Vec<CypherValue>>, CypherError> {
                let mut out = Vec::with_capacity(aggs.len());
                for a in aggs {
                    let mut ex = Vec::with_capacity(a.extra.len());
                    for e in &a.extra {
                        ex.push(e.eval_at(&ecx, sc, i)?);
                    }
                    out.push(ex);
                }
                Ok(out)
            };
            for i in 0..n {
                // the group: one typed key needs no owned value for a hit
                let g = match key_ops.len() {
                    0 => match self.groups.first() {
                        Some(_) => 0,
                        None => {
                            let extras = extras_at(&mut self.scratch, i)?;
                            self.new_group(Vec::new(), extras)
                        }
                    },
                    1 => {
                        let v = key_ops[0].get(i);
                        match self.index.find_one(v) {
                            Some(g) => g,
                            None => {
                                let extras = extras_at(&mut self.scratch, i)?;
                                self.new_group(vec![v.to_owned()], extras)
                            }
                        }
                    }
                    _ => {
                        let hash: Vec<JoinKey> =
                            key_ops.iter().map(|o| join_key_ref(o.get(i))).collect();
                        match self.index.many.get(&hash) {
                            Some(&g) => g,
                            None => {
                                let extras = extras_at(&mut self.scratch, i)?;
                                let vals = key_ops.iter().map(|o| o.get(i).to_owned()).collect();
                                self.new_group(vals, extras)
                            }
                        }
                    }
                };
                let group = &mut self.groups[g];
                for (j, spec) in aggs.iter().enumerate() {
                    match &mut group.accs[j] {
                        Acc::Count(c) if spec.arg.is_none() => {
                            *c += 1;
                            continue;
                        }
                        Acc::Pairs(pairs) => {
                            if let (Some(item), Some(val)) = (item_ops[j], arg_ops[j]) {
                                let val = val.get(i);
                                if !val.is_null() {
                                    pairs.push((item.get(i).to_owned(), val.to_owned()));
                                }
                            }
                            continue;
                        }
                        _ => {}
                    }
                    let Some(op) = arg_ops[j] else { continue };
                    let v = op.get(i);
                    if v.is_null() {
                        continue;
                    }
                    if let Some(set) = &mut group.distinct[j]
                        && !set.insert(join_key_ref(v))
                    {
                        continue;
                    }
                    group.accs[j].add_ref(v)?;
                }
            }
            return Ok(());
        }
        // several unflat groups: row by row
        let total = rs.row_count(in_groups);
        let mut lanes = Vec::new();
        for row in 0..total {
            rs.row_lanes(in_groups, row, &mut lanes);
            let mut key_vals = Vec::with_capacity(self.keys.len());
            for (k, _) in self.keys {
                key_vals.push(k.eval_row(rs, cx.view(), &lanes, &mut self.scratch)?);
            }
            let g = {
                let found = match key_vals.len() {
                    0 => self.groups.first().map(|_| 0),
                    1 => self.index.find_one(ValueRef::of(&key_vals[0])),
                    _ => {
                        let hash: Vec<JoinKey> = key_vals.iter().map(join_key).collect();
                        self.index.many.get(&hash).copied()
                    }
                };
                match found {
                    Some(idx) => idx,
                    None => {
                        let mut extras = Vec::with_capacity(self.aggs.len());
                        for a in self.aggs {
                            let mut ex = Vec::with_capacity(a.extra.len());
                            for e in &a.extra {
                                ex.push(e.eval_row(rs, cx.view(), &lanes, &mut self.scratch)?);
                            }
                            extras.push(ex);
                        }
                        self.new_group(key_vals, extras)
                    }
                }
            };
            let mut values = Vec::with_capacity(self.aggs.len());
            let mut items = Vec::with_capacity(self.aggs.len());
            for a in self.aggs {
                let is_pair = matches!(a.func.as_str(), "apoc.agg.maxitems" | "apoc.agg.minitems");
                if is_pair {
                    items.push(match &a.arg {
                        Some(e) => Some(e.eval_row(rs, cx.view(), &lanes, &mut self.scratch)?),
                        None => None,
                    });
                    values.push(match a.extra.first() {
                        Some(e) => Some(e.eval_row(rs, cx.view(), &lanes, &mut self.scratch)?),
                        None => None,
                    });
                } else {
                    items.push(None);
                    values.push(match &a.arg {
                        Some(e) => Some(e.eval_row(rs, cx.view(), &lanes, &mut self.scratch)?),
                        None => None,
                    });
                }
            }
            self.feed(g, values, items)?;
            if row & 0xFFF == 0xFFF {
                cx.check()?;
            }
        }
        Ok(())
    }

    /// Folds a whole chunk into the one group of a keyless aggregate when every aggregate is a
    /// plain count, sum, avg, min or max over a typed integer or float column (or `count(*)`);
    /// `false` when a lane-by-lane pass is needed instead.
    fn bulk_fold(
        &mut self,
        arg_ops: &[Option<&Operand<'_>>],
        rows: usize,
    ) -> Result<bool, CypherError> {
        enum Col<'a> {
            Ints(&'a [i64], &'a [bool]),
            Floats(&'a [f64], &'a [bool]),
            Rows,
            /// Lanes with a value, already counted.
            NonNull(usize),
            /// The non-null ids (node ids when the flag is set) of a DISTINCT count.
            DistinctIds(Vec<i64>, bool),
        }
        // a column read through a selection is taken into its selected lanes first
        let taken: Vec<Option<Vector>> = arg_ops
            .iter()
            .map(|op| match op {
                Some(Operand::Ref(v @ (Vector::Int { .. } | Vector::Float { .. }), sel)) => {
                    Some(match sel {
                        Some(s) => v.take(s),
                        None => (*v).clone(),
                    })
                }
                _ => None,
            })
            .collect();
        let mut cols: Vec<Col<'_>> = Vec::with_capacity(self.aggs.len());
        for ((spec, op), own) in self.aggs.iter().zip(arg_ops).zip(&taken) {
            let op: Option<&Operand<'_>> = *op;
            let owned_vec: Option<&Vector> = match (own, op) {
                (Some(v), _) => Some(v),
                (None, Some(Operand::Owned(v))) => Some(v),
                _ => None,
            };
            if !spec.extra.is_empty()
                || !matches!(spec.func.as_str(), "count" | "sum" | "avg" | "min" | "max")
            {
                return Ok(false);
            }
            if spec.distinct {
                // count(DISTINCT x) over ids or integers: the ids straight into the set
                if spec.func != "count" {
                    return Ok(false);
                }
                let all = |v: &Vector| -> Option<(Vec<i64>, bool)> {
                    match v {
                        Vector::Node { ids, valid } => Some((
                            ids.iter()
                                .zip(valid)
                                .filter(|(_, ok)| **ok)
                                .map(|(id, _)| id.0 as i64)
                                .collect(),
                            true,
                        )),
                        Vector::Int { vals, valid } => Some((
                            vals.iter()
                                .zip(valid)
                                .filter(|(_, ok)| **ok)
                                .map(|(v, _)| *v)
                                .collect(),
                            false,
                        )),
                        _ => None,
                    }
                };
                let picked = |v: &Vector, sel: &[u32]| -> Option<(Vec<i64>, bool)> {
                    match v {
                        Vector::Node { ids, valid } => Some((
                            sel.iter()
                                .filter(|&&l| valid[l as usize])
                                .map(|&l| ids[l as usize].0 as i64)
                                .collect(),
                            true,
                        )),
                        Vector::Int { vals, valid } => Some((
                            sel.iter()
                                .filter(|&&l| valid[l as usize])
                                .map(|&l| vals[l as usize])
                                .collect(),
                            false,
                        )),
                        _ => None,
                    }
                };
                let found = match op {
                    Some(Operand::Owned(v)) => all(v),
                    Some(Operand::Ref(v, None)) => all(v),
                    Some(Operand::Ref(v, Some(sel))) => picked(v, sel),
                    _ => None,
                };
                let Some((ids, node)) = found else {
                    return Ok(false);
                };
                cols.push(Col::DistinctIds(ids, node));
                continue;
            }
            match (op, owned_vec) {
                (None, _) => {
                    if spec.func != "count" {
                        return Ok(false);
                    }
                    cols.push(Col::Rows);
                }
                (_, Some(Vector::Int { vals, valid })) if vals.len() == rows => {
                    cols.push(Col::Ints(vals, valid));
                }
                (_, Some(Vector::Float { vals, valid })) if vals.len() == rows => {
                    cols.push(Col::Floats(vals, valid));
                }
                // count(x) over anything else: its non-null lanes
                (Some(Operand::Scalar(v)), _) if spec.func == "count" => {
                    cols.push(Col::NonNull(if v.is_null() { 0 } else { rows }));
                }
                (Some(Operand::Owned(v)), _) if spec.func == "count" => {
                    cols.push(Col::NonNull(v.non_null_count(None)));
                }
                (Some(Operand::Ref(v, sel)), _) if spec.func == "count" => {
                    cols.push(Col::NonNull(v.non_null_count(*sel)));
                }
                _ => return Ok(false),
            }
        }
        let n = rows;
        let g = self.group_for(Vec::new(), || {
            Ok(self.aggs.iter().map(|_| Vec::new()).collect())
        })?;
        let group = &mut self.groups[g];
        let (accs, distinct) = (&mut group.accs, &mut group.distinct);
        for (i, (acc, col)) in accs.iter_mut().zip(cols).enumerate() {
            match (acc, col) {
                (Acc::Count(c), Col::Rows) => *c = c.saturating_add(n as i64),
                (Acc::Count(c), Col::NonNull(k)) => *c = c.saturating_add(k as i64),
                (acc, Col::DistinctIds(ids, node)) => {
                    let set = distinct[i]
                        .as_mut()
                        .expect("a DISTINCT aggregate has its set");
                    set.reserve(ids.len());
                    for id in ids {
                        if set.insert(JoinKey::Integer(id)) {
                            acc.add(Some(if node {
                                CypherValue::NodeRef(NodeId(id as u64))
                            } else {
                                CypherValue::Integer(id)
                            }))?;
                        }
                    }
                }
                (Acc::Count(c), Col::Ints(_, valid)) | (Acc::Count(c), Col::Floats(_, valid)) => {
                    *c = c.saturating_add(valid.iter().filter(|ok| **ok).count() as i64);
                }
                (
                    Acc::Sum {
                        int_sum,
                        float_sum,
                        any,
                        ..
                    },
                    Col::Ints(vals, valid),
                ) => {
                    let mut s = 0i64;
                    let mut seen = false;
                    for (v, ok) in vals.iter().zip(valid) {
                        if *ok {
                            s = s.wrapping_add(*v);
                            seen = true;
                        }
                    }
                    *int_sum = int_sum.wrapping_add(s);
                    *float_sum += s as f64;
                    *any |= seen;
                }
                (
                    Acc::Sum {
                        float_sum,
                        has_float,
                        any,
                        ..
                    },
                    Col::Floats(vals, valid),
                ) => {
                    let mut seen = false;
                    for (v, ok) in vals.iter().zip(valid) {
                        if *ok {
                            *float_sum += *v;
                            seen = true;
                        }
                    }
                    *has_float |= seen;
                    *any |= seen;
                }
                (Acc::Avg { sum, n: cnt }, Col::Ints(vals, valid)) => {
                    for (v, ok) in vals.iter().zip(valid) {
                        if *ok {
                            *sum += *v as f64;
                            *cnt += 1;
                        }
                    }
                }
                (Acc::Avg { sum, n: cnt }, Col::Floats(vals, valid)) => {
                    for (v, ok) in vals.iter().zip(valid) {
                        if *ok {
                            *sum += *v;
                            *cnt += 1;
                        }
                    }
                }
                (Acc::Min(best), Col::Ints(vals, valid)) => {
                    if let Some(m) = vals
                        .iter()
                        .zip(valid)
                        .filter(|(_, ok)| **ok)
                        .map(|(v, _)| *v)
                        .min()
                    {
                        let v = CypherValue::Integer(m);
                        match best {
                            Some(b) if compare_values(&v, b) != std::cmp::Ordering::Less => {}
                            _ => *best = Some(v),
                        }
                    }
                }
                (Acc::Max(best), Col::Ints(vals, valid)) => {
                    if let Some(m) = vals
                        .iter()
                        .zip(valid)
                        .filter(|(_, ok)| **ok)
                        .map(|(v, _)| *v)
                        .max()
                    {
                        let v = CypherValue::Integer(m);
                        match best {
                            Some(b) if compare_values(&v, b) != std::cmp::Ordering::Greater => {}
                            _ => *best = Some(v),
                        }
                    }
                }
                (acc @ Acc::Min(_), Col::Floats(vals, valid))
                | (acc @ Acc::Max(_), Col::Floats(vals, valid)) => {
                    for (v, ok) in vals.iter().zip(valid) {
                        if *ok {
                            acc.add(Some(CypherValue::Float(*v)))?;
                        }
                    }
                }
                _ => unreachable!("the shapes were checked"),
            }
        }
        Ok(true)
    }

    /// The finished groups as staged output rows: keys, then the aggregates, then placeholders
    /// for the post items (computed per emitted chunk).
    pub(crate) fn finalize_rows(
        groups: &[GroupAcc],
        keys: &[(CompiledExpr, DataPos)],
        aggs: &[AggSpec],
        post: &[PostAgg],
        width: usize,
    ) -> Result<Staged, CypherError> {
        let mut st = Staged::new(width);
        for group in groups {
            let mut finals: Vec<CypherValue> = Vec::with_capacity(aggs.len());
            for (i, spec) in aggs.iter().enumerate() {
                finals.push(finalize(&spec.func, &group.accs[i], &group.extras[i])?);
            }
            st.push_row(|cols| {
                for ((_, pos), v) in keys.iter().zip(&group.keys) {
                    cols[pos.col as usize].push_owned(v.clone());
                }
                for (spec, v) in aggs.iter().zip(finals) {
                    cols[spec.out.col as usize].push_owned(v);
                }
                for p in post {
                    cols[p.out.col as usize].push_owned(CypherValue::Null);
                }
            })?;
        }
        st.finish()?;
        Ok(st)
    }
}

/// The aggregation operator.
pub struct AggregateOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    state: AggState<'p>,
    keys: &'p [(CompiledExpr, DataPos)],
    aggs: &'p [AggSpec],
    post: &'p [PostAgg],
    in_groups: &'p [GroupId],
    out_group: GroupId,
    scratch: EvalScratch<'p>,
    staged: Option<Staged>,
}

impl<'p> AggregateOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        keys: &'p [(CompiledExpr, DataPos)],
        aggs: &'p [AggSpec],
        post: &'p [PostAgg],
        in_groups: &'p [GroupId],
        out_group: GroupId,
    ) -> Self {
        Self {
            input,
            state: AggState::new(keys, aggs, false),
            keys,
            aggs,
            post,
            in_groups,
            out_group,
            scratch: EvalScratch::new(),
            staged: None,
        }
    }
}

impl PhysOperator for AggregateOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.staged.is_none() {
            while self.input.next(rs, cx)? {
                self.state
                    .consume_chunk(rs, cx, self.in_groups, self.post.is_empty())?;
                cx.tick()?;
            }
            if self.state.groups.is_empty() && self.keys.is_empty() {
                self.state
                    .groups
                    .push(AggState::default_group(self.aggs, false));
            }
            let width = rs.group(self.out_group).cols.len();
            let st = AggState::finalize_rows(
                &self.state.groups,
                self.keys,
                self.aggs,
                self.post,
                width,
            )?;
            self.state.clear();
            self.staged = Some(st);
        }
        let st = self.staged.as_mut().unwrap();
        if !st.emit(rs, self.out_group)? {
            return Ok(false);
        }
        if !self.post.is_empty() {
            let n = rs.group(self.out_group).len;
            let mut cols = Vec::with_capacity(self.post.len());
            {
                let ecx = EvalCtx::new(rs, cx.view(), Some(self.out_group));
                for p in self.post {
                    let v = p.expr.eval(&ecx, &mut self.scratch)?;
                    cols.push((p.out.col, v.into_vector(n)));
                }
            }
            for (c, v) in cols {
                rs.group_mut(self.out_group).cols[c as usize] = v;
            }
        }
        Ok(true)
    }

    fn reset(&mut self) {
        self.state.clear();
        self.staged = None;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// The result of an aggregate.
fn finalize(func: &str, acc: &Acc, extras: &[CypherValue]) -> Result<CypherValue, CypherError> {
    Ok(match (func, acc) {
        ("count", Acc::Count(c)) => CypherValue::Integer(*c),
        (
            "sum",
            Acc::Sum {
                int_sum,
                float_sum,
                has_float,
                any,
            },
        ) => {
            if !any {
                CypherValue::Integer(0)
            } else if *has_float {
                CypherValue::Float(*float_sum)
            } else {
                CypherValue::Integer(*int_sum)
            }
        }
        ("avg", Acc::Avg { sum, n }) => {
            if *n == 0 {
                CypherValue::Null
            } else {
                CypherValue::Float(sum / *n as f64)
            }
        }
        ("min", Acc::Min(b)) | ("max", Acc::Max(b)) => b.clone().unwrap_or(CypherValue::Null),
        ("collect", Acc::Collect(items)) | ("__concat", Acc::Concat(items)) => {
            CypherValue::List(items.clone())
        }
        ("apoc.agg.maxitems", Acc::Pairs(pairs)) | ("apoc.agg.minitems", Acc::Pairs(pairs)) => {
            let want_max = func.ends_with("maxitems");
            let mut best: Option<CypherValue> = None;
            let mut items: Vec<CypherValue> = Vec::new();
            for (item, value) in pairs {
                match &best {
                    None => {
                        best = Some(value.clone());
                        items = vec![item.clone()];
                    }
                    Some(b) => {
                        let ord = compare_values(value, b);
                        let better = if want_max {
                            ord == std::cmp::Ordering::Greater
                        } else {
                            ord == std::cmp::Ordering::Less
                        };
                        if better {
                            best = Some(value.clone());
                            items = vec![item.clone()];
                        } else if ord == std::cmp::Ordering::Equal {
                            items.push(item.clone());
                        }
                    }
                }
            }
            let mut m = BTreeMap::new();
            m.insert("items".to_string(), CypherValue::List(items));
            m.insert("value".to_string(), best.unwrap_or(CypherValue::Null));
            CypherValue::Map(m)
        }
        (_, Acc::Values(non_null)) => finalize_values(func, non_null, extras)?,
        _ => {
            return Err(CypherError::semantic(format!(
                "unknown aggregate function: {func}"
            )));
        }
    })
}

/// Aggregates finalized from the collected non-null values.
fn finalize_values(
    func: &str,
    non_null: &[CypherValue],
    extras: &[CypherValue],
) -> Result<CypherValue, CypherError> {
    let pct_of = |name: &str| -> Result<f64, CypherError> {
        match extras.first() {
            Some(CypherValue::Float(f)) => Ok(*f),
            Some(CypherValue::Integer(i)) => Ok(*i as f64),
            Some(_) => Err(CypherError::type_error(format!(
                "{name}() requires a numeric percentile"
            ))),
            None => Err(CypherError::type_error(format!(
                "{name}() requires two arguments"
            ))),
        }
    };
    Ok(match func {
        "percentiledisc" => {
            if non_null.is_empty() {
                return Ok(CypherValue::Null);
            }
            let pct = pct_of("percentileDisc")?;
            if !(0.0..=1.0).contains(&pct) {
                return Err(CypherError::semantic(
                    "percentileDisc() percentile must be between 0.0 and 1.0".to_string(),
                ));
            }
            let mut nums: Vec<f64> = Vec::new();
            let mut all_int = true;
            for v in non_null {
                match v {
                    CypherValue::Integer(i) => nums.push(*i as f64),
                    CypherValue::Float(f) => {
                        all_int = false;
                        nums.push(*f);
                    }
                    _ => {
                        return Err(CypherError::type_error(
                            "percentileDisc() requires numeric values".to_string(),
                        ));
                    }
                }
            }
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let idx = if pct == 0.0 {
                0
            } else {
                ((pct * nums.len() as f64).ceil() as usize).saturating_sub(1)
            };
            let idx = idx.min(nums.len() - 1);
            if all_int {
                CypherValue::Integer(nums[idx] as i64)
            } else {
                CypherValue::Float(nums[idx])
            }
        }
        "percentilecont" => {
            if non_null.is_empty() {
                return Ok(CypherValue::Null);
            }
            let pct = pct_of("percentileCont")?;
            if !(0.0..=1.0).contains(&pct) {
                return Err(CypherError::semantic(
                    "percentileCont() percentile must be between 0.0 and 1.0".to_string(),
                ));
            }
            let mut nums: Vec<f64> = Vec::new();
            for v in non_null {
                match v {
                    CypherValue::Integer(i) => nums.push(*i as f64),
                    CypherValue::Float(f) => nums.push(*f),
                    _ => {
                        return Err(CypherError::type_error(
                            "percentileCont() requires numeric values".to_string(),
                        ));
                    }
                }
            }
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            if nums.len() == 1 || pct == 0.0 {
                return Ok(CypherValue::Float(nums[0]));
            }
            if pct == 1.0 {
                return Ok(CypherValue::Float(nums[nums.len() - 1]));
            }
            let rank = pct * (nums.len() - 1) as f64;
            let lower = rank.floor() as usize;
            let upper = rank.ceil() as usize;
            let fraction = rank - lower as f64;
            CypherValue::Float(nums[lower] + fraction * (nums[upper] - nums[lower]))
        }
        "stdev" | "stdevp" => {
            let sample = func == "stdev";
            if (sample && non_null.len() < 2) || non_null.is_empty() {
                return Ok(CypherValue::Float(0.0));
            }
            let mut nums: Vec<f64> = Vec::new();
            for v in non_null {
                match v.to_f64() {
                    Some(f) => nums.push(f),
                    None => {
                        return Err(CypherError::type_error(format!(
                            "{}() requires numeric values, got {}",
                            if sample { "stDev" } else { "stDevP" },
                            v.type_name()
                        )));
                    }
                }
            }
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            let denom = if sample { nums.len() - 1 } else { nums.len() } as f64;
            let variance = nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / denom;
            CypherValue::Float(variance.sqrt())
        }
        "apoc.agg.first" => non_null.first().cloned().unwrap_or(CypherValue::Null),
        "apoc.agg.last" => non_null.last().cloned().unwrap_or(CypherValue::Null),
        "apoc.agg.median" => {
            let mut nums: Vec<f64> = non_null.iter().filter_map(|v| v.to_f64()).collect();
            if nums.is_empty() {
                return Ok(CypherValue::Null);
            }
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let n = nums.len();
            CypherValue::Float(if n % 2 == 1 {
                nums[n / 2]
            } else {
                (nums[n / 2 - 1] + nums[n / 2]) / 2.0
            })
        }
        "apoc.agg.product" => {
            if non_null.is_empty() {
                return Ok(CypherValue::Float(0.0));
            }
            let all_int = non_null
                .iter()
                .all(|v| matches!(v, CypherValue::Integer(_)));
            let prod: f64 = non_null.iter().filter_map(|v| v.to_f64()).product();
            if all_int {
                CypherValue::Integer(prod as i64)
            } else {
                CypherValue::Float(prod)
            }
        }
        "apoc.agg.statistics" => {
            let mut nums: Vec<f64> = non_null.iter().filter_map(|v| v.to_f64()).collect();
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let all_int = non_null
                .iter()
                .all(|v| matches!(v, CypherValue::Integer(_)));
            let as_val = |f: f64| {
                if all_int {
                    CypherValue::Integer(f as i64)
                } else {
                    CypherValue::Float(f)
                }
            };
            let mut m = BTreeMap::new();
            m.insert("total".to_string(), CypherValue::Integer(nums.len() as i64));
            if nums.is_empty() {
                return Ok(CypherValue::Map(m));
            }
            let disc = |p: f64| {
                let idx = if p == 0.0 {
                    0
                } else {
                    ((p * nums.len() as f64).ceil() as usize).saturating_sub(1)
                };
                nums[idx.min(nums.len() - 1)]
            };
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            let var = nums.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / nums.len() as f64;
            m.insert("min".to_string(), as_val(nums[0]));
            m.insert("max".to_string(), as_val(nums[nums.len() - 1]));
            m.insert(
                "minNonZero".to_string(),
                CypherValue::Float(nums.iter().copied().find(|x| *x != 0.0).unwrap_or(f64::MAX)),
            );
            m.insert("mean".to_string(), CypherValue::Float(mean));
            m.insert("stdev".to_string(), CypherValue::Float(var.sqrt()));
            for p in [0.5, 0.75, 0.9, 0.95, 0.99] {
                m.insert(p.to_string(), as_val(disc(p)));
            }
            CypherValue::Map(m)
        }
        "apoc.agg.percentiles" => {
            let mut nums: Vec<f64> = non_null.iter().filter_map(|v| v.to_f64()).collect();
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let all_int = non_null
                .iter()
                .all(|v| matches!(v, CypherValue::Integer(_)));
            let pcts: Vec<f64> = match extras.first() {
                Some(CypherValue::List(l)) => l.iter().filter_map(|v| v.to_f64()).collect(),
                _ => vec![0.5, 0.75, 0.9, 0.95, 0.99],
            };
            CypherValue::List(
                pcts.iter()
                    .map(|p| {
                        if nums.is_empty() {
                            return CypherValue::Null;
                        }
                        let idx = if *p == 0.0 {
                            0
                        } else {
                            ((p * nums.len() as f64).ceil() as usize).saturating_sub(1)
                        };
                        let v = nums[idx.min(nums.len() - 1)];
                        if all_int {
                            CypherValue::Integer(v as i64)
                        } else {
                            CypherValue::Float(v)
                        }
                    })
                    .collect(),
            )
        }
        "apoc.agg.nth" => {
            let n = match extras.first() {
                Some(CypherValue::Integer(i)) => *i,
                _ => 0,
            };
            if n >= 0 && (n as usize) < non_null.len() {
                non_null[n as usize].clone()
            } else {
                CypherValue::Null
            }
        }
        "apoc.agg.slice" => {
            let from = match extras.first() {
                Some(CypherValue::Integer(i)) => (*i).max(0) as usize,
                _ => 0,
            };
            let to = match extras.get(1) {
                Some(CypherValue::Integer(i)) => *i,
                _ => -1,
            };
            let end = if to < 0 {
                non_null.len()
            } else {
                (from + to as usize).min(non_null.len())
            };
            CypherValue::List(
                non_null
                    .get(from.min(non_null.len())..end)
                    .unwrap_or(&[])
                    .to_vec(),
            )
        }
        "apoc.agg.graph" => {
            let mut nodes: Vec<CypherValue> = Vec::new();
            let mut rels: Vec<CypherValue> = Vec::new();
            for v in non_null {
                collect_graph_parts(v, &mut nodes, &mut rels);
            }
            let mut m = BTreeMap::new();
            m.insert("nodes".to_string(), CypherValue::List(dedup_vals(nodes)));
            m.insert(
                "relationships".to_string(),
                CypherValue::List(dedup_vals(rels)),
            );
            CypherValue::Map(m)
        }
        _ => {
            return Err(CypherError::semantic(format!(
                "unknown aggregate function: {func}"
            )));
        }
    })
}

/// Nodes and relationships of `v` (an entity, a path, or lists of them, nested), in order.
pub fn collect_graph_parts(
    v: &CypherValue,
    nodes: &mut Vec<CypherValue>,
    rels: &mut Vec<CypherValue>,
) {
    match v {
        CypherValue::Path { nodes: n, edges: e } => {
            nodes.extend(n.iter().cloned());
            rels.extend(e.iter().cloned());
        }
        CypherValue::Node { .. } | CypherValue::NodeRef(_) => nodes.push(v.clone()),
        CypherValue::Edge { .. } | CypherValue::EdgeRef { .. } => rels.push(v.clone()),
        CypherValue::List(l) => {
            for x in l {
                collect_graph_parts(x, nodes, rels);
            }
        }
        _ => {}
    }
}

fn dedup_vals(v: Vec<CypherValue>) -> Vec<CypherValue> {
    let mut out: Vec<CypherValue> = Vec::new();
    for x in v {
        if !out.contains(&x) {
            out.push(x);
        }
    }
    out
}
