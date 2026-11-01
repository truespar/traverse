//! Result containers for the 8 [`OutputKind`](super::OutputKind) variants: each has a `stream()`
//! iterator, a `stats()` summary and a `write_back()` bulk property write, so the procedure adapter
//! builds `.stream` / `.stats` / `.write` rows without per-algorithm code.

use std::collections::BTreeMap;

use traverse_core::graph::types::{EdgeId, NodeId};

use super::AlgorithmError;
use crate::types::CypherValue;

// -- AlgorithmResult trait ------------------------------------------------

/// YIELD column filter: `None` = all columns, `Some(slice)` = only those (any order). Algorithms
/// MAY skip building unwanted columns.
pub type ColumnFilter<'a> = Option<&'a [&'a str]>;

/// Interface of every algorithm output.
pub trait AlgorithmResult: Send + 'static {
    /// `.stream` rows as (column, value) pairs in declared column order.
    fn stream_rows(&self) -> Vec<Vec<(&'static str, CypherValue)>>;

    /// [`stream_rows`](Self::stream_rows) with a YIELD filter, so implementations can skip
    /// expensive unwanted columns. Default: `stream_rows` then filter; override for heavy columns
    /// (lists, embeddings, paths).
    fn stream_rows_filtered(&self, wanted: ColumnFilter) -> Vec<Vec<(&'static str, CypherValue)>> {
        let rows = self.stream_rows();
        match wanted {
            None => rows,
            Some(w) => rows
                .into_iter()
                .map(|row| row.into_iter().filter(|(k, _)| w.contains(k)).collect())
                .collect(),
        }
    }

    /// Aggregate stats for `.stats` (also returned by `.write`).
    fn stats(&self) -> BTreeMap<String, CypherValue>;

    /// Column names in row order; drive YIELD resolution.
    fn columns(&self) -> &'static [&'static str];

    /// Writable via `.write`? `false` for `Path`, `NodePair`, `EntitySet`, `StatsOnly`.
    fn is_writable(&self) -> bool {
        false
    }

    /// Bulk-writes the result to a node property. Default errors for non-writable kinds.
    fn write_back(
        &self,
        _view: &mut dyn traverse_core::tx::view::GraphView,
        _property: &str,
    ) -> Result<WriteStats, AlgorithmError> {
        Err(AlgorithmError::Other(
            "this algorithm's output cannot be written back as a node property".into(),
        ))
    }
}

/// Result of `write_back` after `.write`.
#[derive(Debug, Clone, Default)]
pub struct WriteStats {
    /// Nodes that received the property.
    pub nodes_written: u64,
    /// Property values written; more than `nodes_written` when a node gets several fields.
    pub properties_written: u64,
    /// Write time in ms. Built-in helpers leave `0.0`; the adapter measures `writeMillis` itself.
    pub elapsed_ms: f64,
}

// -- NodeScalarResult (PageRank, Degree, Betweenness, ...) ------------------

/// One `f64` per node, dense by `NodeId.0`; missing nodes are `NaN` (skipped when streaming).
pub struct NodeScalarResult {
    /// Name of the computed value (`"score"`, `"position"`). Informational: the stream column is
    /// always `score`.
    pub field_name: &'static str,
    /// One value per node by `NodeId.0`; `NaN` = outside the projection.
    pub values: Vec<f64>,
}

impl NodeScalarResult {
    /// Wraps a dense per-node score vector.
    pub fn new(field_name: &'static str, values: Vec<f64>) -> Self {
        Self { field_name, values }
    }
}

impl AlgorithmResult for NodeScalarResult {
    fn columns(&self) -> &'static [&'static str] {
        &["nodeId", "score"]
    }

    fn stream_rows(&self) -> Vec<Vec<(&'static str, CypherValue)>> {
        self.stream_rows_filtered(None)
    }

    fn stream_rows_filtered(&self, wanted: ColumnFilter) -> Vec<Vec<(&'static str, CypherValue)>> {
        // dense N-node outputs (pageRank, betweenness, ...): building every row serially dominates
        // stream time; rows are independent, so par_iter
        use rayon::prelude::*;
        let want_id = wanted.is_none_or(|w| w.contains(&"nodeId"));
        let want_score = wanted.is_none_or(|w| w.contains(&"score"));
        self.values
            .par_iter()
            .enumerate()
            .filter_map(|(i, &v)| {
                if v.is_nan() {
                    return None;
                }
                let mut row: Vec<(&'static str, CypherValue)> = Vec::with_capacity(2);
                if want_id {
                    row.push(("nodeId", CypherValue::Integer(i as i64)));
                }
                if want_score {
                    row.push(("score", CypherValue::Float(v)));
                }
                Some(row)
            })
            .collect()
    }

    fn stats(&self) -> BTreeMap<String, CypherValue> {
        // one parallel reduce over values: count, sum, min, max in a single sweep
        use rayon::prelude::*;
        let (count, sum, min, max) = self
            .values
            .par_iter()
            .filter(|x| !x.is_nan())
            .fold(
                || (0u64, 0.0_f64, f64::INFINITY, f64::NEG_INFINITY),
                |(c, s, mn, mx), &v| (c + 1, s + v, mn.min(v), mx.max(v)),
            )
            .reduce(
                || (0u64, 0.0_f64, f64::INFINITY, f64::NEG_INFINITY),
                |(c1, s1, mn1, mx1), (c2, s2, mn2, mx2)| {
                    (c1 + c2, s1 + s2, mn1.min(mn2), mx1.max(mx2))
                },
            );
        let mut stats = BTreeMap::new();
        if count == 0 {
            stats.insert("nodeCount".into(), CypherValue::Integer(0));
            return stats;
        }
        let mean = sum / count as f64;
        stats.insert("nodeCount".into(), CypherValue::Integer(count as i64));
        stats.insert("min".into(), CypherValue::Float(min));
        stats.insert("max".into(), CypherValue::Float(max));
        stats.insert("mean".into(), CypherValue::Float(mean));
        stats
    }

    fn is_writable(&self) -> bool {
        true
    }

    fn write_back(
        &self,
        view: &mut dyn traverse_core::tx::view::GraphView,
        property: &str,
    ) -> Result<WriteStats, AlgorithmError> {
        write_back_scalar(view, property, &self.values)
    }
}

// -- NodeCommunityResult (WCC, Louvain, LPA, ...) ---------------------------

/// One community / group id per node; `u64::MAX` = unassigned (e.g. outside the projection).
pub struct NodeCommunityResult {
    /// Name of the assigned id (`"componentId"`). Informational: the stream column is always
    /// `communityId`.
    pub field_name: &'static str,
    /// One community id per node by `NodeId.0`; `u64::MAX` = unassigned.
    pub values: Vec<u64>,
    /// Extra algorithm-specific stats columns (e.g. louvain's `modularity`, `ranLevels`) merged
    /// into `stats()`.
    pub extra_stats: Vec<(&'static str, CypherValue)>,
}

impl NodeCommunityResult {
    /// Wraps a dense per-node community assignment, no extra stats.
    pub fn new(field_name: &'static str, values: Vec<u64>) -> Self {
        Self {
            field_name,
            values,
            extra_stats: Vec::new(),
        }
    }

    /// Adds algorithm-specific stats columns.
    pub fn with_extra_stats(mut self, extra: Vec<(&'static str, CypherValue)>) -> Self {
        self.extra_stats = extra;
        self
    }
}

impl AlgorithmResult for NodeCommunityResult {
    fn columns(&self) -> &'static [&'static str] {
        &["nodeId", "communityId"]
    }

    fn stream_rows(&self) -> Vec<Vec<(&'static str, CypherValue)>> {
        self.stream_rows_filtered(None)
    }

    fn stream_rows_filtered(&self, wanted: ColumnFilter) -> Vec<Vec<(&'static str, CypherValue)>> {
        use rayon::prelude::*;
        let want_id = wanted.is_none_or(|w| w.contains(&"nodeId"));
        let want_comm = wanted.is_none_or(|w| w.contains(&"communityId"));
        self.values
            .par_iter()
            .enumerate()
            .filter_map(|(i, &v)| {
                if v == u64::MAX {
                    return None;
                }
                let mut row: Vec<(&'static str, CypherValue)> = Vec::with_capacity(2);
                if want_id {
                    row.push(("nodeId", CypherValue::Integer(i as i64)));
                }
                if want_comm {
                    row.push(("communityId", CypherValue::Integer(v as i64)));
                }
                Some(row)
            })
            .collect()
    }

    fn stats(&self) -> BTreeMap<String, CypherValue> {
        use rayon::prelude::*;
        use rustc_hash::FxHashMap;
        // parallel fold-reduce: per-worker community size maps merged at the end; a serial hashmap
        // pass dominated `.stats` on large results
        let sizes: FxHashMap<u64, u64> = self
            .values
            .par_iter()
            .fold(FxHashMap::<u64, u64>::default, |mut acc, &v| {
                if v != u64::MAX {
                    *acc.entry(v).or_insert(0) += 1;
                }
                acc
            })
            .reduce(FxHashMap::<u64, u64>::default, |mut a, b| {
                for (k, v) in b {
                    *a.entry(k).or_insert(0) += v;
                }
                a
            });
        let total: u64 = sizes.values().sum();
        let mut s = BTreeMap::new();
        s.insert("nodeCount".into(), CypherValue::Integer(total as i64));
        s.insert(
            "communityCount".into(),
            CypherValue::Integer(sizes.len() as i64),
        );
        if !sizes.is_empty() {
            let max = *sizes.values().max().unwrap();
            let min = *sizes.values().min().unwrap();
            s.insert(
                "largestCommunitySize".into(),
                CypherValue::Integer(max as i64),
            );
            s.insert(
                "smallestCommunitySize".into(),
                CypherValue::Integer(min as i64),
            );
        }
        for (k, v) in &self.extra_stats {
            s.insert((*k).to_string(), v.clone());
        }
        s
    }

    fn is_writable(&self) -> bool {
        true
    }

    fn write_back(
        &self,
        view: &mut dyn traverse_core::tx::view::GraphView,
        property: &str,
    ) -> Result<WriteStats, AlgorithmError> {
        write_back_int(view, property, &self.values)
    }
}

// -- NodeMultiResult (Triangle Count + LCC, HITS, ...) ----------------------

/// Several named scalar fields per node; field count fixed at construction.
pub struct NodeMultiResult {
    /// Column name per field, in inner-vector order.
    pub fields: Vec<&'static str>,
    /// Outer index = node id, inner = field index. NaN = inactive.
    pub values: Vec<Vec<f64>>,
}

impl NodeMultiResult {
    /// Wraps per-node rows aligned with `fields`.
    pub fn new(fields: Vec<&'static str>, values: Vec<Vec<f64>>) -> Self {
        Self { fields, values }
    }
}

impl AlgorithmResult for NodeMultiResult {
    fn columns(&self) -> &'static [&'static str] {
        // multi-field columns are dynamic (registered via metadata().output_columns); only the node
        // id here, as a sentinel
        &["nodeId"]
    }

    fn stream_rows(&self) -> Vec<Vec<(&'static str, CypherValue)>> {
        self.stream_rows_filtered(None)
    }

    fn stream_rows_filtered(&self, wanted: ColumnFilter) -> Vec<Vec<(&'static str, CypherValue)>> {
        // skip filtered-out fields entirely (YIELD subsets of N fields cut CypherValue allocations
        // per row accordingly); rows built in parallel
        use rayon::prelude::*;
        let want_id = wanted.is_none_or(|w| w.contains(&"nodeId"));
        let field_mask: Vec<bool> = self
            .fields
            .iter()
            .map(|f| wanted.is_none_or(|w| w.contains(f)))
            .collect();
        let field_mask_ref: &[bool] = &field_mask;
        let fields_ref: &[&'static str] = &self.fields;
        self.values
            .par_iter()
            .enumerate()
            .filter_map(|(i, fields)| {
                if fields.iter().all(|x| x.is_nan()) {
                    return None;
                }
                let mut row: Vec<(&'static str, CypherValue)> =
                    Vec::with_capacity(1 + fields.len());
                if want_id {
                    row.push(("nodeId", CypherValue::Integer(i as i64)));
                }
                for (fi, &v) in fields.iter().enumerate() {
                    if !field_mask_ref[fi] {
                        continue;
                    }
                    let name = fields_ref[fi];
                    if v.is_nan() {
                        row.push((name, CypherValue::Null));
                    } else {
                        row.push((name, CypherValue::Float(v)));
                    }
                }
                Some(row)
            })
            .collect()
    }

    fn stats(&self) -> BTreeMap<String, CypherValue> {
        let mut s = BTreeMap::new();
        s.insert(
            "nodeCount".into(),
            CypherValue::Integer(self.values.len() as i64),
        );
        s.insert(
            "fieldCount".into(),
            CypherValue::Integer(self.fields.len() as i64),
        );
        s
    }

    fn is_writable(&self) -> bool {
        true
    }

    fn write_back(
        &self,
        view: &mut dyn traverse_core::tx::view::GraphView,
        property_prefix: &str,
    ) -> Result<WriteStats, AlgorithmError> {
        // per (field, value) write `<prefix>_<field>` as Float, prefix from the user's
        // `writeProperty`; skip all-NaN rows (inactive) and individual NaNs
        let field_names: Vec<String> = self
            .fields
            .iter()
            .map(|f| format!("{property_prefix}_{f}"))
            .collect();
        let field_pids: Vec<PropertyKeyId> = field_names
            .iter()
            .map(|n| view.intern_property_key(n))
            .collect();
        let mut nodes_written: u64 = 0;
        let mut props_written: u64 = 0;
        for (i, fields) in self.values.iter().enumerate() {
            if fields.iter().all(|x| x.is_nan()) {
                continue;
            }
            let props: Vec<(&str, PropertyKeyId, PropertyValue)> = fields
                .iter()
                .enumerate()
                .filter(|(_, v)| !v.is_nan())
                .map(|(fi, &v)| {
                    (
                        field_names[fi].as_str(),
                        field_pids[fi],
                        PropertyValue::Float(v),
                    )
                })
                .collect();
            if !props.is_empty() {
                props_written += props.len() as u64;
                nodes_written += 1;
                set_node_props(view, CoreNodeId(i as u64), props)?;
            }
        }
        Ok(WriteStats {
            nodes_written,
            properties_written: props_written,
            elapsed_ms: 0.0,
        })
    }
}

// -- NodeVectorResult (embeddings) ----------------------------------------

/// One fixed-length `f32` vector per node (embedding algorithms). Streams `nodeId`, `embedding`;
/// writable as a list property.
pub struct NodeVectorResult {
    /// Vector name (always `"embedding"`). Informational: the stream column is always `embedding`.
    pub field_name: &'static str,
    /// Vector length.
    pub dim: usize,
    /// Flat: `values[i*dim .. (i+1)*dim]` = embedding of node `i`.
    pub values: Vec<f32>,
    /// `valid[i] = false` -> node inactive.
    pub valid: Vec<bool>,
}

impl AlgorithmResult for NodeVectorResult {
    fn columns(&self) -> &'static [&'static str] {
        &["nodeId", "embedding"]
    }

    fn stream_rows(&self) -> Vec<Vec<(&'static str, CypherValue)>> {
        self.stream_rows_filtered(None)
    }

    fn stream_rows_filtered(&self, wanted: ColumnFilter) -> Vec<Vec<(&'static str, CypherValue)>> {
        use rayon::prelude::*;
        let want_id = wanted.is_none_or(|w| w.contains(&"nodeId"));
        let want_emb = wanted.is_none_or(|w| w.contains(&"embedding"));
        let values_ref: &[f32] = &self.values;
        let dim = self.dim;
        (0..self.valid.len())
            .into_par_iter()
            .filter(|&i| self.valid[i])
            .map(|i| {
                let mut row: Vec<(&'static str, CypherValue)> = Vec::with_capacity(2);
                if want_id {
                    row.push(("nodeId", CypherValue::Integer(i as i64)));
                }
                if want_emb {
                    let start = i * dim;
                    let slice = &values_ref[start..start + dim];
                    let v: Vec<CypherValue> = slice
                        .iter()
                        .map(|x| CypherValue::Float(*x as f64))
                        .collect();
                    row.push(("embedding", CypherValue::List(v)));
                }
                row
            })
            .collect()
    }

    fn stats(&self) -> BTreeMap<String, CypherValue> {
        let mut s = BTreeMap::new();
        let count = self.valid.iter().filter(|&&v| v).count();
        s.insert("nodeCount".into(), CypherValue::Integer(count as i64));
        s.insert("dimension".into(), CypherValue::Integer(self.dim as i64));
        s
    }

    fn is_writable(&self) -> bool {
        true
    }

    fn write_back(
        &self,
        view: &mut dyn traverse_core::tx::view::GraphView,
        property: &str,
    ) -> Result<WriteStats, AlgorithmError> {
        // one List<Float> property per node: ~9 bytes per dimension per node in storage
        // (PropertyValue::List widens floats to 64-bit); a dedicated embedding store would be
        // leaner if this gets hot
        let pid = view.intern_property_key(property);
        let mut nodes_written: u64 = 0;
        for i in 0..self.valid.len() {
            if !self.valid[i] {
                continue;
            }
            let start = i * self.dim;
            let slice = &self.values[start..start + self.dim];
            let list: Vec<PropertyValue> = slice
                .iter()
                .map(|x| PropertyValue::Float(*x as f64))
                .collect();
            set_node_props(
                view,
                CoreNodeId(i as u64),
                vec![(property, pid, PropertyValue::List(list))],
            )?;
            nodes_written += 1;
        }
        Ok(WriteStats {
            nodes_written,
            properties_written: nodes_written,
            elapsed_ms: 0.0,
        })
    }
}

// -- PathResult (Dijkstra, BFS, A*, ...) ------------------------------------

/// One path from a path-finding algorithm.
#[derive(Debug, Clone)]
pub struct PathRecord {
    /// First node.
    pub source: NodeId,
    /// Last node.
    pub target: NodeId,
    /// Sum of edge weights along the path (hop count when unweighted).
    pub total_cost: f64,
    /// Nodes from `source` to `target`, both included.
    pub node_ids: Vec<NodeId>,
    /// Edges in traversal order; one fewer than `node_ids`.
    pub edge_ids: Vec<EdgeId>,
    /// Cumulative cost at each node of `node_ids`, `0.0` at the source.
    pub costs: Vec<f64>,
}

/// Paths from a path-finding algorithm, one stream row each.
pub struct PathResult {
    /// Paths in production order.
    pub paths: Vec<PathRecord>,
}

impl AlgorithmResult for PathResult {
    fn columns(&self) -> &'static [&'static str] {
        &[
            "sourceNodeId",
            "targetNodeId",
            "totalCost",
            "nodeIds",
            "edgeIds",
            "costs",
        ]
    }

    fn stream_rows(&self) -> Vec<Vec<(&'static str, CypherValue)>> {
        self.stream_rows_filtered(None)
    }

    fn stream_rows_filtered(&self, wanted: ColumnFilter) -> Vec<Vec<(&'static str, CypherValue)>> {
        // skip the heavy list columns YIELD doesn't want (they dominate stream time); paths built
        // in parallel
        use rayon::prelude::*;
        let want = |c: &'static str| wanted.is_none_or(|w| w.contains(&c));
        let w_src = want("sourceNodeId");
        let w_tgt = want("targetNodeId");
        let w_cost = want("totalCost");
        let w_nodes = want("nodeIds");
        let w_edges = want("edgeIds");
        let w_costs = want("costs");
        self.paths
            .par_iter()
            .map(|p| {
                let mut row: Vec<(&'static str, CypherValue)> = Vec::with_capacity(6);
                if w_src {
                    row.push(("sourceNodeId", CypherValue::Integer(p.source.0 as i64)));
                }
                if w_tgt {
                    row.push(("targetNodeId", CypherValue::Integer(p.target.0 as i64)));
                }
                if w_cost {
                    row.push(("totalCost", CypherValue::Float(p.total_cost)));
                }
                if w_nodes {
                    row.push((
                        "nodeIds",
                        CypherValue::List(
                            p.node_ids
                                .iter()
                                .map(|n| CypherValue::Integer(n.0 as i64))
                                .collect(),
                        ),
                    ));
                }
                if w_edges {
                    row.push((
                        "edgeIds",
                        CypherValue::List(
                            p.edge_ids
                                .iter()
                                .map(|e| CypherValue::Integer(e.0 as i64))
                                .collect(),
                        ),
                    ));
                }
                if w_costs {
                    row.push((
                        "costs",
                        CypherValue::List(p.costs.iter().map(|c| CypherValue::Float(*c)).collect()),
                    ));
                }
                row
            })
            .collect()
    }

    fn stats(&self) -> BTreeMap<String, CypherValue> {
        let mut s = BTreeMap::new();
        s.insert(
            "pathCount".into(),
            CypherValue::Integer(self.paths.len() as i64),
        );
        s
    }
}

// -- NodePairResult (similarity, link prediction) -------------------------

/// Scored node pair from a similarity or link-prediction algorithm.
#[derive(Debug, Clone)]
pub struct NodePairScore {
    /// First node; streamed as `sourceNodeId`.
    pub node1: NodeId,
    /// Second node; streamed as `targetNodeId`.
    pub node2: NodeId,
    /// Score; streamed as `score`.
    pub similarity: f64,
}

/// Scored pairs from a similarity or link-prediction algorithm.
pub struct NodePairResult {
    /// Scored pairs, one stream row each.
    pub pairs: Vec<NodePairScore>,
}

impl AlgorithmResult for NodePairResult {
    fn columns(&self) -> &'static [&'static str] {
        // canonical names from `OutputKind::default_columns(NodePair)`, used by Studio's NodePair
        // extractor; `score` shared with NodeScalar so styling treats both alike
        &["sourceNodeId", "targetNodeId", "score"]
    }

    fn stream_rows(&self) -> Vec<Vec<(&'static str, CypherValue)>> {
        use rayon::prelude::*;
        self.pairs
            .par_iter()
            .map(|p| {
                vec![
                    ("sourceNodeId", CypherValue::Integer(p.node1.0 as i64)),
                    ("targetNodeId", CypherValue::Integer(p.node2.0 as i64)),
                    ("score", CypherValue::Float(p.similarity)),
                ]
            })
            .collect()
    }

    fn stats(&self) -> BTreeMap<String, CypherValue> {
        let mut s = BTreeMap::new();
        s.insert(
            "pairCount".into(),
            CypherValue::Integer(self.pairs.len() as i64),
        );
        if !self.pairs.is_empty() {
            let mean =
                self.pairs.iter().map(|p| p.similarity).sum::<f64>() / self.pairs.len() as f64;
            s.insert("meanSimilarity".into(), CypherValue::Float(mean));
        }
        s
    }
}

// -- EntitySetResult (Bridges, Articulation Points, Cliques) --------------

/// Nodes and edges an algorithm picks out (bridges, articulation points, spanning / Steiner trees);
/// one `kind`/`id` row per member.
pub struct EntitySetResult {
    /// Nodes; streamed with `kind = "node"`.
    pub nodes: Vec<NodeId>,
    /// Edges; streamed with `kind = "edge"`.
    pub edges: Vec<EdgeId>,
}

impl AlgorithmResult for EntitySetResult {
    fn columns(&self) -> &'static [&'static str] {
        &["kind", "id"]
    }

    fn stream_rows(&self) -> Vec<Vec<(&'static str, CypherValue)>> {
        let mut rows = Vec::with_capacity(self.nodes.len() + self.edges.len());
        for n in &self.nodes {
            rows.push(vec![
                ("kind", CypherValue::String("node".into())),
                ("id", CypherValue::Integer(n.0 as i64)),
            ]);
        }
        for e in &self.edges {
            rows.push(vec![
                ("kind", CypherValue::String("edge".into())),
                ("id", CypherValue::Integer(e.0 as i64)),
            ]);
        }
        rows
    }

    fn stats(&self) -> BTreeMap<String, CypherValue> {
        let mut s = BTreeMap::new();
        s.insert(
            "nodeCount".into(),
            CypherValue::Integer(self.nodes.len() as i64),
        );
        s.insert(
            "edgeCount".into(),
            CypherValue::Integer(self.edges.len() as i64),
        );
        s
    }
}

// -- StatsResult (Modularity metric, Conductance, ...) ----------------------

/// One map of summary stats, for metric algorithms without per-node output (modularity,
/// conductance).
pub struct StatsResult {
    /// Stats by name, one column each.
    pub stats: BTreeMap<String, CypherValue>,
}

impl AlgorithmResult for StatsResult {
    fn columns(&self) -> &'static [&'static str] {
        // each scalar is a top-level .stats column; .stream emits the whole map as one row
        &[]
    }

    fn stream_rows(&self) -> Vec<Vec<(&'static str, CypherValue)>> {
        // one row mirroring the stats keys; arbitrary keys can't be `&'static str`, so the adapter
        // special-cases StatsOnly outputs
        Vec::new()
    }

    fn stats(&self) -> BTreeMap<String, CypherValue> {
        self.stats.clone()
    }
}

// -- Bulk property write-back helpers --------------------------------------
// One pass over the result, skipping inactive entries (NaN for scalars,
// `u64::MAX` for communities), setting the property via GraphView. The key is
// interned once, so the per-node loop is a single
// `node.properties.insert(pid, pval)`: no string lookups, no allocations.
// Everything lands in the adapter's write transaction, atomic for the caller's
// session.

use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::NodeId as CoreNodeId;
use traverse_core::graph::types::PropertyKeyId;
use traverse_core::tx::view::GraphView as CoreGraphView;

/// Writes `props` onto a node the way `SET n.p = v` does: index entries out and back in around
/// the change, which also puts it in the redo log. A bare `get_node_mut` write is neither indexed
/// nor durable in `wal` mode.
fn set_node_props(
    view: &mut dyn CoreGraphView,
    nid: CoreNodeId,
    props: Vec<(&str, PropertyKeyId, PropertyValue)>,
) -> Result<(), AlgorithmError> {
    for (key, ..) in &props {
        view.unindex_node_prop(nid, key);
    }
    let keys: Vec<&str> = props.iter().map(|p| p.0).collect();
    let node = view.get_node_mut(nid).map_err(AlgorithmError::from)?;
    for (_, pid, value) in props {
        node.properties.insert(pid, value);
    }
    for key in keys {
        view.index_node_prop(nid, key);
    }
    Ok(())
}

fn write_back_scalar(
    view: &mut dyn CoreGraphView,
    property: &str,
    values: &[f64],
) -> Result<WriteStats, AlgorithmError> {
    let pid = view.intern_property_key(property);
    let mut nodes_written: u64 = 0;
    for (i, &v) in values.iter().enumerate() {
        if v.is_nan() {
            continue;
        }
        set_node_props(
            view,
            CoreNodeId(i as u64),
            vec![(property, pid, PropertyValue::Float(v))],
        )?;
        nodes_written += 1;
    }
    Ok(WriteStats {
        nodes_written,
        properties_written: nodes_written,
        elapsed_ms: 0.0,
    })
}

fn write_back_int(
    view: &mut dyn CoreGraphView,
    property: &str,
    values: &[u64],
) -> Result<WriteStats, AlgorithmError> {
    let pid = view.intern_property_key(property);
    let mut nodes_written: u64 = 0;
    for (i, &v) in values.iter().enumerate() {
        if v == u64::MAX {
            continue;
        }
        // community ids fit i64 for practical NodeIds (the `u64::MAX` sentinel is already filtered)
        set_node_props(
            view,
            CoreNodeId(i as u64),
            vec![(property, pid, PropertyValue::Int(v as i64))],
        )?;
        nodes_written += 1;
    }
    Ok(WriteStats {
        nodes_written,
        properties_written: nodes_written,
        elapsed_ms: 0.0,
    })
}
