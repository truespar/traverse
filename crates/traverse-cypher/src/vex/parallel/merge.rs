//! The merges at the breakers: workers' partial states combined in parallel, in morsel order,
//! into the chunks the next segment reads.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

use crate::error::CypherError;
use crate::types::CypherValue;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::expr::eval::EvalCtx;
use super::super::expr::scalar::compare_values;
use super::super::expr::{CompiledExpr, EvalScratch};
use super::super::ops::Staged;
use super::super::ops::aggregate::{AggState, GroupAcc, merge_group, seal_group};
use super::super::ops::join::BuildTable;
use super::super::ops::{install, move_cols};
use super::super::plan::{AggSpec, PostAgg};
use super::super::values::{JoinKey, join_key};
use super::super::vector::{VECTOR_CAPACITY, Vector};
use super::{Sched, Snapshot};

/// A hash of a row key, for partitioning.
fn hash_key(key: &[JoinKey]) -> u64 {
    let mut h = FxHasher::default();
    key.hash(&mut h);
    h.finish()
}

/// Rows of `snaps` over `groups` as the columns `moves` name, concatenated.
pub(super) fn to_rows(
    sched: &Sched<'_, '_>,
    snaps: Vec<Snapshot>,
    groups: &[GroupId],
    moves: &[(DataPos, DataPos)],
    width: usize,
) -> Result<(Vec<Vector>, usize), CypherError> {
    let moves = move_cols(moves);
    let filled: Vec<bool> = (0..width)
        .map(|c| moves.iter().any(|(_, oc)| *oc as usize == c))
        .collect();
    let work = snaps.len() * VECTOR_CAPACITY;
    let parts: Vec<Vec<Vector>> = sched.map(snaps, work, |groups_| {
        let rs = ResultSet { groups: groups_ };
        let mut st = Staged::new(width);
        rs.for_each_row(groups, |lanes| {
            st.push_row(|cols| {
                for (src, c) in &moves {
                    cols[*c as usize].push(rs.value_at(*src, lanes));
                }
                for (c, ok) in filled.iter().enumerate() {
                    if !ok {
                        cols[c].push_owned(CypherValue::Null);
                    }
                }
            })
        })?;
        st.into_cols()
    })?;
    let (cols, rows) = concat_parts(sched, parts, width);
    sched.charge(cols.iter().map(Vector::heap_bytes).sum::<usize>())?;
    Ok((cols, rows))
}

/// Column-wise concatenation of row parts.
fn concat_parts(
    sched: &Sched<'_, '_>,
    parts: Vec<Vec<Vector>>,
    width: usize,
) -> (Vec<Vector>, usize) {
    let rows: usize = parts.first().map_or(0, |_| {
        parts.iter().map(|p| p.first().map_or(0, Vector::len)).sum()
    });
    if parts.len() == 1 {
        return (parts.into_iter().next().unwrap(), rows);
    }
    let parts = Arc::new(parts);
    let cols: Vec<Vector> = sched
        .for_parts(width, rows, |c| {
            Ok(Vector::concat(parts.iter().map(|p| p[c].clone()).collect()))
        })
        .expect("concatenation cannot fail");
    (cols, rows)
}

// -- Aggregate ---------------------------------------------------------------------------

/// Merges the workers' partial groups and emits the finished groups, in the order the
/// sequential operator would have produced them (first appearance in morsel order).
pub(super) fn aggregate(
    sched: &Sched<'_, '_>,
    partials: Vec<Vec<GroupAcc>>,
    keys: &[(CompiledExpr, DataPos)],
    aggs: &[AggSpec],
    post: &[PostAgg],
    out_group: GroupId,
) -> Result<Vec<Snapshot>, CypherError> {
    let total: usize = partials.iter().map(Vec::len).sum();
    sched.charge(total * 160)?;
    let mut groups: Vec<GroupAcc> = if keys.is_empty() {
        let mut acc: Option<GroupAcc> = None;
        for part in partials {
            for g in part {
                match &mut acc {
                    Some(a) => merge_group(a, g, aggs)?,
                    None => acc = Some(g),
                }
            }
        }
        vec![acc.unwrap_or_else(|| AggState::default_group(aggs, true))]
    } else {
        let parts = sched.parts_for(total);
        // every group goes to its key's partition, morsel order kept within each
        let mut buckets: Vec<Vec<GroupAcc>> = (0..parts).map(|_| Vec::new()).collect();
        let mut seq = 0u64;
        for part in partials {
            for mut g in part {
                g.first = seq;
                seq += 1;
                let keyv: Vec<JoinKey> = g.keys.iter().map(join_key).collect();
                let p = if parts == 1 {
                    0
                } else {
                    (hash_key(&keyv) % parts as u64) as usize
                };
                buckets[p].push(g);
            }
        }
        let merged: Vec<Vec<GroupAcc>> = sched.map(buckets, total, |bucket| {
            let mut index: FxHashMap<Vec<JoinKey>, usize> = FxHashMap::default();
            let mut out: Vec<GroupAcc> = Vec::new();
            for g in bucket {
                let keyv: Vec<JoinKey> = g.keys.iter().map(join_key).collect();
                match index.get(&keyv) {
                    Some(&i) => merge_group(&mut out[i], g, aggs)?,
                    None => {
                        index.insert(keyv, out.len());
                        out.push(g);
                    }
                }
            }
            Ok(out)
        })?;
        let mut all: Vec<GroupAcc> = merged.into_iter().flatten().collect();
        if parts > 1 {
            all.par_sort_by_key(|g| g.first);
        }
        all
    };
    for g in &mut groups {
        seal_group(g, aggs)?;
    }
    let width = sched.plan.widths[out_group as usize] as usize;
    let groups = Arc::new(groups);
    let n = groups.len();
    let chunks = n.div_ceil(VECTOR_CAPACITY).max(1);
    let widths = &sched.plan.widths;
    sched.for_parts(chunks, n, |c| {
        let start = c * VECTOR_CAPACITY;
        let end = (start + VECTOR_CAPACITY).min(n);
        let st = AggState::finalize_rows(&groups[start..end], keys, aggs, post, width)?;
        let cols = st.into_cols()?;
        let mut rs = ResultSet::new(widths);
        install(
            &mut rs,
            out_group,
            end - start,
            cols.into_iter()
                .enumerate()
                .map(|(i, v)| (i as u16, v))
                .collect(),
        );
        if !post.is_empty() {
            sched.in_worker(|cx| {
                let n = rs.group(out_group).len;
                let mut scratch = EvalScratch::new();
                let mut cols = Vec::with_capacity(post.len());
                {
                    let ecx = EvalCtx::new(&rs, cx.view(), Some(out_group));
                    for p in post {
                        let v = p.expr.eval(&ecx, &mut scratch)?;
                        cols.push((p.out.col, v.into_vector(n)));
                    }
                }
                for (c, v) in cols {
                    rs.group_mut(out_group).cols[c as usize] = v;
                }
                Ok(())
            })?;
        }
        sched.charge(super::snapshot_bytes(&rs.groups))?;
        Ok(rs.groups)
    })
}

// -- Sort --------------------------------------------------------------------------------

/// Concatenates the workers' rows and keys, sorts the row order in parallel (stable, so ties
/// keep morsel order like the sequential sort keeps input order), and emits the rows.
pub(super) fn sort(
    sched: &Sched<'_, '_>,
    partials: Vec<(Vec<Vector>, Vec<CypherValue>, usize)>,
    keys: &[(CompiledExpr, bool)],
    out_group: GroupId,
    limit: Option<usize>,
) -> Result<Vec<Snapshot>, CypherError> {
    let width = sched.plan.widths[out_group as usize] as usize;
    let nk = keys.len();
    let mut key_vals: Vec<CypherValue> = Vec::new();
    let mut parts: Vec<Vec<Vector>> = Vec::with_capacity(partials.len());
    for (cols, kv, _) in partials {
        key_vals.extend(kv);
        parts.push(cols);
    }
    let (cols, rows) = concat_parts(sched, parts, width);
    sched.charge(
        key_vals.len() * 48 + cols.iter().map(Vector::heap_bytes).sum::<usize>() + rows * 4,
    )?;
    let mut order: Vec<u32> = (0..rows as u32).collect();
    let cmp = |&a: &u32, &b: &u32| {
        let ba = a as usize * nk;
        let bb = b as usize * nk;
        for (i, (_, asc)) in keys.iter().enumerate() {
            let ord = compare_values(&key_vals[ba + i], &key_vals[bb + i]);
            let ord = if *asc { ord } else { ord.reverse() };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    };
    if sched.parts_for(rows) > 1 {
        order.par_sort_by(cmp);
    } else {
        order.sort_by(cmp);
    }
    if let Some(k) = limit {
        order.truncate(k);
    }
    sched.rows_to_snapshots(Arc::new(cols), rows, Some(Arc::new(order)), out_group)
}

// -- Distinct ----------------------------------------------------------------------------

/// The first row of every distinct key, in row order: partitioned by key hash, each partition
/// deduplicated on its own.
fn distinct_order(sched: &Sched<'_, '_>, keys: &[Vec<JoinKey>]) -> Vec<u32> {
    let n = keys.len();
    let parts = sched.parts_for(n);
    if parts == 1 {
        let mut seen: FxHashSet<&[JoinKey]> = FxHashSet::default();
        return (0..n as u32)
            .filter(|&r| seen.insert(&keys[r as usize]))
            .collect();
    }
    let hashes: Vec<u64> = keys.par_iter().map(|k| hash_key(k)).collect();
    let kept: Vec<Vec<u32>> = (0..parts)
        .into_par_iter()
        .map(|p| {
            let mut seen: FxHashSet<&[JoinKey]> = FxHashSet::default();
            let mut out = Vec::new();
            for r in 0..n {
                if (hashes[r] % parts as u64) as usize == p && seen.insert(&keys[r]) {
                    out.push(r as u32);
                }
            }
            out
        })
        .collect();
    let mut order: Vec<u32> = kept.into_iter().flatten().collect();
    order.par_sort_unstable();
    order
}

/// Deduplicates the workers' projected rows and emits the survivors.
pub(super) fn distinct(
    sched: &Sched<'_, '_>,
    parts: Vec<(Vec<Vector>, Vec<Vec<JoinKey>>)>,
    width: usize,
    out_group: GroupId,
) -> Result<Vec<Snapshot>, CypherError> {
    let mut keys: Vec<Vec<JoinKey>> = Vec::new();
    let mut cols_parts: Vec<Vec<Vector>> = Vec::with_capacity(parts.len());
    for (cols, k) in parts {
        keys.extend(k);
        cols_parts.push(cols);
    }
    let (cols, rows) = concat_parts(sched, cols_parts, width);
    sched.charge(cols.iter().map(Vector::heap_bytes).sum::<usize>() + rows * 4)?;
    let order = distinct_order(sched, &keys);
    sched.rows_to_snapshots(Arc::new(cols), rows, Some(Arc::new(order)), out_group)
}

// -- Union -------------------------------------------------------------------------------

/// Left rows then right rows, deduplicated unless `all`.
#[allow(clippy::too_many_arguments)]
pub(super) fn union(
    sched: &Sched<'_, '_>,
    lcols: Vec<Vector>,
    lrows: usize,
    rcols: Vec<Vector>,
    rrows: usize,
    all: bool,
    width: usize,
    out_group: GroupId,
) -> Result<Vec<Snapshot>, CypherError> {
    let (cols, rows) = concat_parts(sched, vec![lcols, rcols], width);
    let _ = (lrows, rrows);
    if all {
        return sched.rows_to_snapshots(Arc::new(cols), rows, None, out_group);
    }
    let cols = Arc::new(cols);
    let keys: Vec<Vec<JoinKey>> = {
        let cols = cols.clone();
        let parts: Vec<Vec<Vec<JoinKey>>> =
            sched.for_parts(rows.div_ceil(VECTOR_CAPACITY), rows, |c| {
                let start = c * VECTOR_CAPACITY;
                let end = (start + VECTOR_CAPACITY).min(rows);
                Ok((start..end)
                    .map(|r| {
                        (0..width)
                            .map(|col| super::super::values::join_key_ref(cols[col].get(r)))
                            .collect()
                    })
                    .collect())
            })?;
        parts.into_iter().flatten().collect()
    };
    let order = distinct_order(sched, &keys);
    sched.rows_to_snapshots(cols, rows, Some(Arc::new(order)), out_group)
}

// -- Hash join build ---------------------------------------------------------------------

/// The build side of a hash join as a shared, partitioned table.
pub(super) fn build_table(
    sched: &Sched<'_, '_>,
    snaps: Vec<Snapshot>,
    left_keys: &[CompiledExpr],
    left_groups: &[GroupId],
    moves_left: &[(DataPos, DataPos)],
) -> Result<Arc<BuildTable>, CypherError> {
    let moves = move_cols(moves_left);
    let width = moves.len();
    let work = snaps.len() * VECTOR_CAPACITY;
    let parts: Vec<(Vec<Vec<JoinKey>>, Vec<Vector>)> = sched.map(snaps, work, |groups| {
        sched.in_worker(|cx| {
            let rs = ResultSet { groups };
            let mut st = Staged::new(width);
            let mut keys: Vec<Vec<JoinKey>> = Vec::new();
            let mut scratch = EvalScratch::new();
            let mut lanes = Vec::new();
            let total = rs.row_count(left_groups);
            for row in 0..total {
                rs.row_lanes(left_groups, row, &mut lanes);
                if row & 0xFFF == 0xFFF {
                    cx.check()?;
                }
                let mut key = Vec::with_capacity(left_keys.len());
                for k in left_keys {
                    key.push(join_key(&k.eval_row(
                        &rs,
                        cx.view(),
                        &lanes,
                        &mut scratch,
                    )?));
                }
                keys.push(key);
                st.push_row(|cols| {
                    for (i, (src, _)) in moves.iter().enumerate() {
                        cols[i].push(rs.value_at(*src, &lanes));
                    }
                })?;
            }
            Ok((keys, st.into_cols()?))
        })
    })?;
    let mut keys: Vec<Vec<JoinKey>> = Vec::new();
    let mut cols_parts = Vec::with_capacity(parts.len());
    for (k, c) in parts {
        keys.extend(k);
        cols_parts.push(c);
    }
    let (cols, rows) = concat_parts(sched, cols_parts, width);
    debug_assert_eq!(rows, keys.len());
    let n = keys.len();
    let p = sched.parts_for(n);
    let hashes: Vec<u64> = if p == 1 {
        Vec::new()
    } else {
        keys.par_iter().map(|k| hash_key(k)).collect()
    };
    let maps: Vec<FxHashMap<Vec<JoinKey>, Vec<u32>>> = sched.for_parts(p, n, |part| {
        let mut map: FxHashMap<Vec<JoinKey>, Vec<u32>> = FxHashMap::default();
        for (r, key) in keys.iter().enumerate() {
            if p > 1 && (hashes[r] % p as u64) as usize != part {
                continue;
            }
            map.entry(key.clone()).or_default().push(r as u32);
        }
        Ok(map)
    })?;
    sched.charge(n * 48 + cols.iter().map(Vector::heap_bytes).sum::<usize>())?;
    Ok(Arc::new(BuildTable::new(maps, cols)))
}

// -- Cross product -----------------------------------------------------------------------

/// Both sides of a cartesian product as rows; the segment above replays chunk pairs of them.
pub(crate) struct Product {
    pub(crate) left: Vec<Vector>,
    pub(crate) left_rows: usize,
    pub(crate) right: Vec<Vector>,
    pub(crate) right_rows: usize,
    pub(crate) left_out: GroupId,
    pub(crate) right_out: GroupId,
}

impl Product {
    pub(super) fn new(
        left: Vec<Vector>,
        left_rows: usize,
        right: Vec<Vector>,
        right_rows: usize,
        left_out: GroupId,
        right_out: GroupId,
    ) -> Self {
        Self {
            left,
            left_rows,
            right,
            right_rows,
            left_out,
            right_out,
        }
    }

    /// Left chunks.
    pub(crate) fn left_chunks(&self) -> usize {
        self.left_rows.div_ceil(VECTOR_CAPACITY)
    }

    /// Right chunks.
    pub(crate) fn right_chunks(&self) -> usize {
        self.right_rows.div_ceil(VECTOR_CAPACITY)
    }

    /// Chunk pairs, left-major.
    pub(crate) fn pairs(&self) -> usize {
        self.left_chunks() * self.right_chunks()
    }
}
