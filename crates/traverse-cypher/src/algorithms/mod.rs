//! Graph algorithm framework (PageRank, WCC, BFS, centralities, embeddings ...).
//!
//! # Design
//!
//! Each algorithm implements [`GraphAlgorithm`] and runs on an
//! [`AlgorithmGraph`]: a no-copy view over the `GraphView` carrying projection
//! state (label / type filter, orientation, edge weights) and exposing packed
//! CSR-style adjacency via `outgoing_adj` / `incoming_adj`.
//!
//! Three execution modes:
//!
//! * `.stream`: per-node / per-edge / per-path rows into the executor.
//! * `.stats`: aggregate statistics only.
//! * `.write`: run, then bulk-write results as node properties in one write
//!   transaction; only for outputs with stable per-node values.
//!
//! Algorithms live in their own compute-on-demand registry (see [`registry`]),
//! separate from the data-driven `crate::procedure` registry of TCK fixtures.
//!
//! # Hot path
//!
//! Algorithms read via [`AlgorithmGraph::outgoing_adj_slice`], the packed
//! `&[AdjEntry]` slice: no allocation, no hashing. The MVCC snapshot check runs
//! once at projection (see [`projection::AlgorithmGraphView::new`]); reads after
//! that skip delta heads.

pub mod cancellation;
pub mod config;
pub mod materialized;
pub mod node_mask;
pub mod procedure;
pub mod projection;
pub mod registry;
pub mod result;

pub mod centrality;
pub mod community;
pub mod dag;
pub mod embeddings;
pub mod paths;
pub mod similarity;

#[cfg(test)]
mod smoke;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use crate::types::CypherValue;

pub use cancellation::CancellationToken;
pub use config::{BaseConfig, ConfigError, Orientation};
pub use node_mask::NodeMask;
pub use projection::{AlgorithmGraph, AlgorithmGraphView, WeightConfig};
pub use result::{
    AlgorithmResult, EntitySetResult, NodeCommunityResult, NodeMultiResult, NodePairResult,
    NodeScalarResult, NodeVectorResult, PathResult, StatsResult,
};

// -- Error type -----------------------------------------------------------

/// Algorithm errors.
#[derive(Debug, thiserror::Error)]
pub enum AlgorithmError {
    /// Config key missing or of the wrong type.
    #[error("invalid algorithm config: {0}")]
    Config(String),
    /// The query running it stopped (deadline, cancel, terminate).
    #[error("the query {}", .0.as_str())]
    Interrupted(crate::query::Interrupt),
    /// The process went over its memory ceiling.
    #[error("memory limit exceeded")]
    MemoryExhausted,
    /// Required property isn't interned in the graph.
    #[error("unknown property `{0}`")]
    UnknownProperty(String),
    /// Required label isn't interned in the graph.
    #[error("unknown label `{0}`")]
    UnknownLabel(String),
    /// Required edge type isn't interned in the graph.
    #[error("unknown edge type `{0}`")]
    UnknownEdgeType(String),
    /// Error from the graph store.
    #[error("graph error: {0}")]
    Graph(String),
    /// Algorithm-specific failure (cycle in a DAG algorithm, dimension mismatch, ...).
    #[error("{0}")]
    Other(String),
}

impl From<traverse_core::error::Error> for AlgorithmError {
    fn from(e: traverse_core::error::Error) -> Self {
        AlgorithmError::Graph(e.to_string())
    }
}

impl AlgorithmError {
    /// As the error the calling query fails with; `context` prefixes other errors.
    pub fn into_cypher(self, context: &str) -> crate::error::CypherError {
        match self {
            AlgorithmError::Interrupted(reason) => crate::query::interrupted_error(reason),
            AlgorithmError::MemoryExhausted => crate::memory::process_limit_error(),
            other => crate::error::CypherError::runtime(format!("{context}: {other}")),
        }
    }
}

// -- Output kind taxonomy -------------------------------------------------

/// Shape of an algorithm's values. Decides which modes exist (all have
/// `.stream`/`.stats`, writable kinds `.write`) and how Studio renders results
/// (size/gradient for `NodeScalar`, colors for `NodeCommunity`, highlights for
/// `Path`, ...). Covers the GDS catalog; a new kind needs the procedure adapter
/// and Studio renderers updated too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// One `f64` per node. *PageRank, Article Rank, Degree, Eigenvector,
    /// Betweenness, Closeness, Harmonic, CELF.*
    NodeScalar,
    /// One community / group `u64` per node. *WCC, SCC, Louvain, Leiden,
    /// Label Propagation, K-Core, K-Means, K-1 Coloring, HDBSCAN.*
    NodeCommunity,
    /// Multiple named scalar fields per node. *Triangle Count + LCC, HITS
    /// (hub + authority), HDBSCAN (cluster + probability).*
    NodeMulti,
    /// Dense fixed-dimension embedding per node. *FastRP, Node2Vec, HashGNN,
    /// GraphSAGE.*
    NodeVector,
    /// One or more paths. *Dijkstra, A\*, Yen's, Bellman-Ford, BFS, DFS,
    /// MST, Steiner trees, Max Flow.*
    Path,
    /// Score per ordered or unordered pair of nodes. *Node Similarity, kNN,
    /// topological link prediction functions.*
    NodePair,
    /// Set of nodes and/or edges. *Bridges, Articulation Points, Cliques.*
    EntitySet,
    /// Aggregate statistics only. *Modularity metric, Conductance, Clique counting summary.*
    StatsOnly,
}

impl OutputKind {
    /// Has a natural `.write` (bulk property write)? Only stable per-node values qualify.
    pub fn writable(self) -> bool {
        matches!(
            self,
            OutputKind::NodeScalar
                | OutputKind::NodeCommunity
                | OutputKind::NodeMulti
                | OutputKind::NodeVector
        )
    }

    /// Standard columns for kinds whose algorithms all share a schema. `None` for NodeMulti: each
    /// multi-field algorithm (Triangle Count, HITS, HDBSCAN) sets
    /// `AlgorithmMetadata::output_columns` itself.
    pub fn default_columns(self) -> Option<&'static [&'static str]> {
        match self {
            OutputKind::NodeScalar => Some(&["nodeId", "score"]),
            OutputKind::NodeCommunity => Some(&["nodeId", "communityId"]),
            OutputKind::NodeVector => Some(&["nodeId", "embedding"]),
            OutputKind::NodePair => Some(&["sourceNodeId", "targetNodeId", "score"]),
            OutputKind::Path => Some(&[
                "sourceNodeId",
                "targetNodeId",
                "totalCost",
                "nodeIds",
                "edgeIds",
                "costs",
            ]),
            OutputKind::EntitySet => Some(&["nodeIds", "relationshipIds"]),
            OutputKind::NodeMulti | OutputKind::StatsOnly => None,
        }
    }

    /// Standard `.stats` summary columns: `AlgorithmResult::stats()` keys per kind plus
    /// `computeMillis`. `None` for NodeMulti and StatsOnly, whose keys depend on the algorithm.
    pub fn default_stats_columns(self) -> Option<&'static [&'static str]> {
        match self {
            OutputKind::NodeScalar => Some(&["nodeCount", "min", "max", "mean", "computeMillis"]),
            OutputKind::NodeCommunity => Some(&[
                "nodeCount",
                "communityCount",
                "largestCommunitySize",
                "smallestCommunitySize",
                "computeMillis",
            ]),
            OutputKind::NodeVector => Some(&["nodeCount", "dimension", "computeMillis"]),
            OutputKind::Path => Some(&["pathCount", "computeMillis"]),
            OutputKind::NodePair => Some(&["pairCount", "meanSimilarity", "computeMillis"]),
            OutputKind::EntitySet => Some(&["nodeCount", "edgeCount", "computeMillis"]),
            OutputKind::NodeMulti => Some(&["nodeCount", "fieldCount", "computeMillis"]),
            OutputKind::StatsOnly => None,
        }
    }

    /// Standard `.write` summary columns: stats columns plus the adapter's write counters
    /// (`writeMillis`, `nodePropertiesWritten`, `propertiesWritten`). `None` for non-writable
    /// kinds.
    pub fn default_write_columns(self) -> Option<&'static [&'static str]> {
        if !self.writable() {
            return None;
        }
        match self {
            OutputKind::NodeScalar => Some(&[
                "nodeCount",
                "min",
                "max",
                "mean",
                "computeMillis",
                "writeMillis",
                "nodePropertiesWritten",
                "propertiesWritten",
            ]),
            OutputKind::NodeCommunity => Some(&[
                "nodeCount",
                "communityCount",
                "largestCommunitySize",
                "smallestCommunitySize",
                "computeMillis",
                "writeMillis",
                "nodePropertiesWritten",
                "propertiesWritten",
            ]),
            OutputKind::NodeVector => Some(&[
                "nodeCount",
                "dimension",
                "computeMillis",
                "writeMillis",
                "nodePropertiesWritten",
                "propertiesWritten",
            ]),
            OutputKind::NodeMulti => Some(&[
                "nodeCount",
                "fieldCount",
                "computeMillis",
                "writeMillis",
                "nodePropertiesWritten",
                "propertiesWritten",
            ]),
            _ => None,
        }
    }

    /// Stable id for the metadata endpoint.
    pub fn as_str(self) -> &'static str {
        match self {
            OutputKind::NodeScalar => "nodeScalar",
            OutputKind::NodeCommunity => "nodeCommunity",
            OutputKind::NodeMulti => "nodeMulti",
            OutputKind::NodeVector => "nodeVector",
            OutputKind::Path => "path",
            OutputKind::NodePair => "nodePair",
            OutputKind::EntitySet => "entitySet",
            OutputKind::StatsOnly => "statsOnly",
        }
    }
}

// -- Stability tier -------------------------------------------------------

/// Maturity badge for Studio, like GDS's production/beta/alpha tiers. New algorithms are `Beta`
/// until benchmarked and validated, then `Stable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stability {
    /// Verified, benchmarked, API frozen.
    Stable,
    /// Complete; minor API churn possible.
    Beta,
    /// In development; no API guarantees.
    Experimental,
}

impl Stability {
    /// Lowercase name in `/api/algorithms` (`stable`, `beta`, `experimental`).
    pub fn as_str(self) -> &'static str {
        match self {
            Stability::Stable => "stable",
            Stability::Beta => "beta",
            Stability::Experimental => "experimental",
        }
    }
}

// -- Category -------------------------------------------------------------

/// Top-level group in Studio's drawer and `/api/algorithms`, as in GDS's catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Node importance: PageRank, betweenness, closeness, ...
    Centrality,
    /// Grouping/clustering: components, Louvain, Leiden, label propagation.
    Community,
    /// Shortest paths, traversals and spanning or Steiner trees.
    Paths,
    /// DAG algorithms (topological sort, longest path).
    Dag,
    /// Pairwise similarity (node similarity, kNN, cosine).
    Similarity,
    /// Likelihood of two nodes being connected.
    LinkPrediction,
    /// Node embeddings (FastRP, node2vec, GraphSAGE, HashGNN).
    Embeddings,
}

impl Category {
    /// Name in `/api/algorithms` (`centrality`, `linkPrediction`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Centrality => "centrality",
            Category::Community => "community",
            Category::Paths => "paths",
            Category::Dag => "dag",
            Category::Similarity => "similarity",
            Category::LinkPrediction => "linkPrediction",
            Category::Embeddings => "embeddings",
        }
    }
}

// -- Config-schema metadata -----------------------------------------------

/// One config key, published via `/api/algorithms` for Studio's form generator; algorithms declare
/// these next to `metadata()`.
#[derive(Debug, Clone)]
pub struct ConfigParam {
    /// Key as written in the procedure call (`maxIterations`).
    pub key: &'static str,
    /// Value type; picks the Studio widget.
    pub ty: ConfigParamType,
    /// Default when absent; `null` if none.
    pub default: CypherValue,
    /// One-line help shown in Studio.
    pub description: &'static str,
    /// Hide behind Studio's "Advanced" toggle.
    pub advanced: bool,
    /// The algorithm fails without it (e.g. a property name with no sensible default); Studio
    /// disables Run until it's set. Use `..ConfigParam::default()` at call sites.
    pub required: bool,
}

impl Default for ConfigParam {
    fn default() -> Self {
        Self {
            key: "",
            ty: ConfigParamType::String,
            default: CypherValue::Null,
            description: "",
            advanced: false,
            required: false,
        }
    }
}

/// Config parameter type (the JSON Studio expects).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigParamType {
    /// `true` / `false`.
    Bool,
    /// A whole number.
    Integer,
    /// A floating-point number; integers are accepted too.
    Float,
    /// Free text, such as a property name.
    String,
    /// Fixed choice, e.g. orientation, similarity metric.
    Enum,
    /// Multi-select string list (e.g. node labels).
    StringList,
    /// List of integers (e.g. multiple node ids).
    IntegerList,
}

impl ConfigParamType {
    /// Type name in the JSON for Studio (`bool`, `stringList`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigParamType::Bool => "bool",
            ConfigParamType::Integer => "integer",
            ConfigParamType::Float => "float",
            ConfigParamType::String => "string",
            ConfigParamType::Enum => "enum",
            ConfigParamType::StringList => "stringList",
            ConfigParamType::IntegerList => "integerList",
        }
    }
}

// -- Algorithm metadata (consumed by `/api/algorithms`) -------------------

/// Static metadata of one algorithm, from `GraphAlgorithm::metadata()`; `/api/algorithms` exposes
/// it so Studio builds its picker without hard-coding the catalog.
#[derive(Debug, Clone)]
pub struct AlgorithmMetadata {
    /// Unqualified name (`"pageRank"`); procedures are `traverse.<name>.<mode>`.
    pub name: &'static str,
    /// Display name (`"PageRank"`).
    pub display_name: &'static str,
    /// Catalog group.
    pub category: Category,
    /// Result shape: fixes stream columns and whether `.write` exists.
    pub output_kind: OutputKind,
    /// Maturity badge.
    pub stability: Stability,
    /// One-line tooltip.
    pub description: &'static str,
    /// Algorithm-specific params (orientation/label/type filters come from the framework).
    pub params: Vec<ConfigParam>,
    /// `.stream` columns in row order. Published via `/api/algorithms` so Studio can
    /// build `YIELD <cols> RETURN <cols>` per CALL. Must match the output type's
    /// `AlgorithmResult::columns()`; the executor binds rows by these names. Kinds
    /// with uniform columns (NodeScalar / NodeCommunity / NodePair / EntitySet)
    /// usually reuse [`OutputKind::default_columns`].
    pub output_columns: &'static [&'static str],
    /// `.stats` summary row columns; empty = the algorithm declares its own (NodeMulti /
    /// StatsOnly).
    pub stats_columns: &'static [&'static str],
    /// `.write` summary row columns; empty for non-writable algorithms.
    pub write_columns: &'static [&'static str],
}

// -- The core trait -------------------------------------------------------

/// A graph algorithm: pure compute over an [`AlgorithmGraph`]. The framework handles projection,
/// registration, parallel execution, the stream/stats/write modes and cancellation.
pub trait GraphAlgorithm: Send + Sync + 'static {
    /// Algorithm config, parsed from a Cypher map.
    type Config: 'static + Send + Sync;

    /// Result type; [`AlgorithmResult`] lets the framework stream rows, summarize and write back.
    type Output: AlgorithmResult;

    /// Catalog entry (name, category, stability, params).
    fn metadata(&self) -> AlgorithmMetadata;

    /// Parses the algorithm config from the Cypher map. `BaseConfig` (orientation, labels, types,
    /// weight, concurrency) is parsed by the framework and arrives through the projection.
    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<Self::Config, AlgorithmError>;

    /// Runs on a projected graph; checks `cancel` at iteration boundaries.
    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        config: &Self::Config,
        cancel: &CancellationToken,
    ) -> Result<Self::Output, AlgorithmError>;
}

// -- Top-level registration -----------------------------------------------

/// Registers every built-in algorithm. Once per thread using the algorithms layer (the registry is
/// thread-local, like `crate::procedure`).
pub fn register_builtins() {
    centrality::register();
    community::register();
    paths::register();
    dag::register();
    similarity::register();
    embeddings::register();
}

thread_local! {
    /// Per-thread first-touch flag, like [`procedure`]'s dispatch, exposed so callers outside the
    /// executor (e.g. `/api/algorithms`) fill the registry once per worker thread.
    static ENSURE_REGISTERED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Registers the built-ins on this thread's first call; afterwards one `Cell::get` branch.
pub fn ensure_registered() {
    ENSURE_REGISTERED.with(|cell| {
        if !cell.get() {
            register_builtins();
            cell.set(true);
        }
    });
}
