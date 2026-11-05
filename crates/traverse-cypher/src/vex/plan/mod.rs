//! The physical plan: what the lowering of a [`LogicalPlan`](crate::planner::LogicalPlan)
//! produces and the operators are built from.
//!
//! Every variable reference is a [`DataPos`] resolved against the layout at that point of the
//! pipeline, every expression is compiled, and every choice the planner made (access path,
//! flattening, where a row-consuming operator places its outputs) is explicit. The plan is
//! immutable and shared (`Send + Sync`): the plan cache hands the same plan to concurrent
//! statements and parallel workers build their own operator instances from it.

pub mod explain;
pub mod lower;
pub mod subquery;

use std::sync::Arc;

use traverse_core::graph::types::Direction;

use crate::ast::{CmpOp, OnErrorBehavior, RelDirection, TransactionCommandKind};
use crate::planner::TextPredicateKind;
use crate::types::CypherValue;

use super::chunk::{DataPos, GroupId};
use super::expr::CompiledExpr;
use super::layout::Layout;

pub use lower::{compile, compile_expr_full, compile_with_seeds};

/// A compiled plan ready to run.
#[derive(Debug)]
pub struct PhysicalPlan {
    /// The operator tree.
    pub root: PhysNode,
    /// Columns per group, sizing the result set.
    pub widths: Vec<u16>,
    /// Variable placement after the root.
    pub output: Layout,
    /// Result columns in order, with where each is read from; empty when the statement has no
    /// projection.
    pub columns: Vec<(String, DataPos)>,
    /// Groups the root's rows span.
    pub out_groups: Vec<GroupId>,
    /// The plan changes data or schema.
    pub mutating: bool,
    /// The plan is a schema statement.
    pub schema: bool,
    /// The plan returns rows.
    pub has_projection: bool,
    /// Operator nesting depth, for stack sizing.
    pub depth: usize,
}

impl PhysicalPlan {
    /// A plan that produces nothing.
    pub fn empty() -> Self {
        Self {
            root: PhysNode::NoRows,
            widths: Vec::new(),
            output: Layout::new(),
            columns: Vec::new(),
            out_groups: Vec::new(),
            mutating: false,
            schema: false,
            has_projection: false,
            depth: 1,
        }
    }
}

/// Nesting depth of an operator tree (iterative, so deep plans measure without recursion).
pub fn node_depth(root: &PhysNode) -> usize {
    let mut max = 1;
    let mut stack: Vec<(&PhysNode, usize)> = vec![(root, 1)];
    while let Some((n, d)) = stack.pop() {
        max = max.max(d);
        for c in n.children() {
            stack.push((c, d + 1));
        }
    }
    max
}

/// A subquery plan run per outer row: the outer values it needs arrive in its seed group.
#[derive(Debug)]
pub struct SubPlan {
    /// The operator tree; its source is a [`PhysNode::Seed`] of `seed_group`.
    pub root: PhysNode,
    /// Columns per group of the subquery's own result set.
    pub widths: Vec<u16>,
    /// The seed group.
    pub seed_group: GroupId,
    /// Outer value -> seed column.
    pub seeds: Vec<(SeedSource, DataPos)>,
    /// Output columns in order.
    pub out: Vec<(String, DataPos)>,
    /// Groups the root's rows span.
    pub out_groups: Vec<GroupId>,
    /// Variable placement after the root.
    pub layout: Layout,
}

/// Where a seed value comes from in the enclosing evaluation.
#[derive(Debug, Clone, Copy)]
pub enum SeedSource {
    /// A column of the outer result set.
    Outer(DataPos),
    /// A local binding of the outer expression (a comprehension variable).
    Local(u32),
}

/// An aggregate column.
#[derive(Debug)]
pub struct AggSpec {
    /// Lowercased function name (`count`, `collect`, `apoc.agg.median`, ...).
    pub func: String,
    /// `DISTINCT`?
    pub distinct: bool,
    /// The argument; `None` for `count(*)`.
    pub arg: Option<CompiledExpr>,
    /// Further arguments (percentiles, `apoc.agg.nth` index, ...), evaluated on the group's
    /// first row.
    pub extra: Vec<CompiledExpr>,
    /// Output column.
    pub out: DataPos,
}

/// A projection item whose expression nests aggregates (`count(*) * 2`, `collect(x)[0..3]`):
/// the aggregates are computed into hidden columns, then `expr` runs over the finished group.
#[derive(Debug)]
pub struct PostAgg {
    /// The expression over the hidden aggregate columns and the group keys.
    pub expr: CompiledExpr,
    /// Output column.
    pub out: DataPos,
}

/// A spatial index query with compiled arguments.
#[derive(Debug)]
pub enum PhysSpatialQuery {
    /// Within a radius of a point.
    Distance {
        /// The point.
        center: CompiledExpr,
        /// The radius.
        radius: CompiledExpr,
    },
    /// Inside a bounding box.
    BBox {
        /// Lower-left corner.
        ll: CompiledExpr,
        /// Upper-right corner.
        ur: CompiledExpr,
    },
    /// The `k` nearest.
    Knn {
        /// The point.
        center: CompiledExpr,
        /// Neighbours.
        k: CompiledExpr,
    },
}

/// Schema and catalog statements, run once.
#[derive(Debug, Clone)]
pub enum SchemaOp {
    /// `CREATE INDEX`.
    CreateIndex {
        /// Name.
        name: String,
        /// Label.
        label: String,
        /// Property.
        property: String,
        /// Further keys of a composite index.
        extra_properties: Vec<String>,
    },
    /// `CREATE VECTOR INDEX`.
    CreateVectorIndex {
        /// Name.
        name: String,
        /// Label.
        label: String,
        /// Property.
        property: String,
        /// Dimensions.
        dimensions: Option<usize>,
        /// Similarity.
        similarity: String,
        /// Quantization.
        quantization: Option<bool>,
    },
    /// `CREATE FULLTEXT INDEX`.
    CreateFulltextIndex {
        /// Name.
        name: String,
        /// Labels.
        labels: Vec<String>,
        /// Properties.
        properties: Vec<String>,
        /// Analyzer.
        analyzer: Option<String>,
    },
    /// `CREATE POINT INDEX`.
    CreatePointIndex {
        /// Name.
        name: String,
        /// Label.
        label: String,
        /// Property.
        property: String,
    },
    /// `CREATE TEXT INDEX`.
    CreateTextIndex {
        /// Name.
        name: String,
        /// Label.
        label: String,
        /// Property.
        property: String,
    },
    /// `DROP INDEX`.
    DropIndex {
        /// Name.
        name: String,
    },
    /// `ANALYZE GRAPH`.
    AnalyzeGraph {
        /// Labels; empty = all.
        labels: Vec<String>,
    },
    /// `CREATE CONSTRAINT`.
    CreateConstraint {
        /// Name.
        name: String,
        /// Label or type.
        label: String,
        /// Property.
        property: String,
        /// Further keys.
        extra_properties: Vec<String>,
        /// Kind.
        constraint_type: String,
        /// Relationship constraint?
        is_relationship: bool,
        /// `IF NOT EXISTS`.
        if_not_exists: bool,
    },
    /// `DROP CONSTRAINT`.
    DropConstraint {
        /// Name.
        name: String,
    },
    /// `CREATE EDGE INDEX`.
    CreateEdgeIndex {
        /// Name.
        name: String,
        /// Type.
        edge_type: String,
        /// Property.
        property: Option<String>,
        /// Further keys.
        extra_properties: Vec<String>,
    },
    /// `DROP EDGE INDEX`.
    DropEdgeIndex {
        /// Name.
        name: String,
    },
}

/// A `SHOW` listing.
#[derive(Debug, Clone, Copy)]
pub enum ShowOp {
    /// `SHOW INDEXES`.
    Indexes,
    /// `SHOW CONSTRAINTS`.
    Constraints,
    /// `SHOW FUNCTIONS`.
    Functions,
    /// `SHOW PROCEDURES`.
    Procedures,
    /// `SHOW SETTINGS`.
    Settings,
}

/// The `property = value` a label-less [`PhysNode::NodeScan`] can answer from the label indexes.
#[derive(Debug)]
pub struct NodeSeek {
    /// Property.
    pub property: String,
    /// Key, constant for the scan.
    pub value: CompiledExpr,
}

/// Physical operator; see the module docs.
#[derive(Debug)]
pub enum PhysNode {
    /// One row without columns.
    EmptyRow {
        /// The (empty) group it activates.
        group: GroupId,
    },
    /// Rows supplied at run time (subquery seeds, FOREACH iterations, batch bodies).
    Seed {
        /// The group.
        group: GroupId,
        /// Its columns, in the order the rows' values come.
        cols: Vec<DataPos>,
    },
    /// Every node, optionally with a label.
    NodeScan {
        /// Output.
        out: DataPos,
        /// Label.
        label: Option<String>,
        /// Without a label: the property equality a filter above keeps, answered from the label
        /// indexes when they cover every node (else every node is scanned).
        seek: Option<NodeSeek>,
    },
    /// Node lookup by internal id.
    NodeByIdSeek {
        /// Output.
        out: DataPos,
        /// Label the node must have.
        label: Option<String>,
        /// The id.
        id: CompiledExpr,
    },
    /// Node ids in `[lower, upper)`.
    NodeByIdRange {
        /// Output.
        out: DataPos,
        /// Label.
        label: Option<String>,
        /// Inclusive lower bound.
        lower: Option<CompiledExpr>,
        /// Exclusive upper bound.
        upper: Option<CompiledExpr>,
    },
    /// Equality seek in a property index.
    IndexSeek {
        /// Output.
        out: DataPos,
        /// Label.
        label: String,
        /// Property.
        property: String,
        /// Key.
        value: CompiledExpr,
    },
    /// Range scan of a property index, enumerating the hits.
    IndexRange {
        /// Output.
        out: DataPos,
        /// Label.
        label: String,
        /// Property.
        property: String,
        /// Comparison.
        op: CmpOp,
        /// Bound.
        value: CompiledExpr,
    },
    /// Trigram candidates for a string predicate (a superset; the filter above refines).
    TextIndexScan {
        /// Output.
        out: DataPos,
        /// Label.
        label: String,
        /// Property.
        property: String,
        /// Predicate kind.
        kind: TextPredicateKind,
        /// Pattern.
        pattern: CompiledExpr,
    },
    /// R-tree candidates for a spatial predicate.
    PointIndexScan {
        /// Output.
        out: DataPos,
        /// Label.
        label: String,
        /// Property.
        property: String,
        /// Query.
        query: PhysSpatialQuery,
    },
    /// Every edge of the given types, binding both endpoints.
    EdgeTypeScan {
        /// Start variable.
        from: DataPos,
        /// Relationship variable.
        rel: Option<DataPos>,
        /// End variable.
        to: DataPos,
        /// Types; never empty.
        edge_types: Vec<String>,
        /// Direction as written.
        direction: Direction,
    },
    /// Runs `inner` to completion for every chunk of `input` (the input's groups stay active, so
    /// the rows multiply): how a seeded body's first source joins its seed.
    Nest {
        /// Outer.
        input: Box<PhysNode>,
        /// Inner, a source-rooted tree.
        inner: Box<PhysNode>,
    },
    /// A count straight from the storage structure, one row with the count in every output.
    CountSource {
        /// What is counted.
        kind: CountKind,
        /// Output columns (every one gets the count).
        outs: Vec<DataPos>,
        /// Output group.
        out_group: GroupId,
    },
    /// Iterates the selected lanes of `group` one at a time, re-running the operators above per
    /// lane.
    Flatten {
        /// Input.
        input: Box<PhysNode>,
        /// The group.
        group: GroupId,
    },
    /// Keeps the lanes of the evaluation group where the predicate is true.
    Filter {
        /// Input.
        input: Box<PhysNode>,
        /// Predicate.
        pred: CompiledExpr,
        /// The group the predicate varies over; `None` when every referenced group is flat (a
        /// scalar predicate that keeps or drops the whole chunk).
        group: Option<GroupId>,
    },
    /// Computes an expression into a new column of its evaluation group.
    AddColumn {
        /// Input.
        input: Box<PhysNode>,
        /// Expression.
        expr: CompiledExpr,
        /// Output; in the evaluation group, or in a one-lane scalar group.
        out: DataPos,
    },
    /// Row-consuming projection: the rows of `in_groups` become the flat group `out_group`.
    Project {
        /// Input.
        input: Box<PhysNode>,
        /// Items.
        items: Vec<(CompiledExpr, DataPos)>,
        /// Columns copied through.
        moves: Vec<(DataPos, DataPos)>,
        /// Groups the input's rows span.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Deduplicate output rows.
        distinct: bool,
        /// `WITH ... WHERE`, evaluated over the output row with the old bindings still visible.
        where_: Option<CompiledExpr>,
    },
    /// Single hop expansion from a flat source into a new group.
    Expand {
        /// Input.
        input: Box<PhysNode>,
        /// Source node.
        from: DataPos,
        /// Relationship output.
        rel: DataPos,
        /// Neighbor output.
        to: DataPos,
        /// Output group.
        out_group: GroupId,
        /// Direction.
        dir: Direction,
        /// Types; empty = all.
        edge_types: Vec<String>,
        /// Edges bound earlier that this hop must not reuse.
        prior_edges: Vec<DataPos>,
        /// The target when it is already bound (a cycle): expand into it.
        bound_to: Option<DataPos>,
        /// The source group is unflat and the hop fills it in place: its lanes replicated per
        /// edge, the relationship and neighbor appended, a chunk of sources per step.
        in_place: bool,
    },
    /// The edges every lane of `from` has (in `dir`, of `edge_types`, to a node with `to_label`
    /// when given) as an integer column of its group: a counted expansion whose rows nothing
    /// reads, so none are built.
    ExpandCount {
        /// Input.
        input: Box<PhysNode>,
        /// Source node.
        from: DataPos,
        /// The count, in the source's group.
        out: DataPos,
        /// Direction.
        dir: Direction,
        /// Types; empty = all.
        edge_types: Vec<String>,
        /// Label the far end must have.
        to_label: Option<String>,
        /// Edges bound earlier in the row that the hop must not reuse.
        prior_edges: Vec<DataPos>,
    },
    /// Path counts along a chain of hops as `(node, count)` rows at the chain's end (or one
    /// total), carried hop by hop without building a row per path; `exact` is the ordinary plan
    /// for the same rows, run when the carried counts can't be exact.
    ChainWeights {
        /// The end node.
        out: DataPos,
        /// Paths ending there.
        weight: DataPos,
        /// Output group.
        out_group: GroupId,
        /// Label of the chain's first node.
        from_label: Option<String>,
        /// The hops in order.
        hops: Vec<ChainHop>,
        /// Only the total is wanted.
        count_only: bool,
        /// The exact plan.
        exact: Box<PhysNode>,
        /// Where the exact plan puts the node and the count.
        exact_cols: (DataPos, DataPos),
    },
    /// Variable-length expansion from a flat source.
    VarExpand {
        /// Input.
        input: Box<PhysNode>,
        /// Source.
        from: DataPos,
        /// Edge-list output.
        rel: DataPos,
        /// End-node output.
        to: DataPos,
        /// Output group.
        out_group: GroupId,
        /// Direction.
        dir: Direction,
        /// Types.
        edge_types: Vec<String>,
        /// Minimum hops.
        min: usize,
        /// Maximum hops.
        max: usize,
        /// Edges bound earlier.
        prior_edges: Vec<DataPos>,
        /// The target when already bound.
        bound_to: Option<DataPos>,
    },
    /// Distinct multi-hop targets of a flat source, or of every input row's source at once
    /// (`multi_source`).
    DistinctExpand {
        /// Input.
        input: Box<PhysNode>,
        /// Source.
        from: DataPos,
        /// Target output.
        to: DataPos,
        /// Output group.
        out_group: GroupId,
        /// Minimum hops.
        min: usize,
        /// Maximum hops.
        max: usize,
        /// Direction.
        dir: Direction,
        /// Types.
        edge_types: Vec<String>,
        /// Label the target must have.
        target_label: Option<String>,
        /// One seen-set across the whole input.
        global_dedup: bool,
        /// `Some(input groups)`: the sources of every input row are expanded together, the
        /// source group left unflat. Under a global dedup with nothing above reading the input
        /// row (the target filter reads the target alone), the targets at `d` hops from a set
        /// of sources are the union of each source's, so one frontier walk from all of them
        /// replaces one walk per source. The input groups leave the row product while the
        /// targets are emitted.
        multi_source: Option<Vec<GroupId>>,
        /// Filter on the target, evaluated over the output group.
        target_filter: Option<CompiledExpr>,
    },
    /// Elements of a list as rows of a new group (input groups flat).
    Unwind {
        /// Input.
        input: Box<PhysNode>,
        /// The list.
        list: CompiledExpr,
        /// Element output.
        out: DataPos,
        /// Output group.
        out_group: GroupId,
    },
    /// Index seek per input row.
    IndexLookupJoin {
        /// Input.
        input: Box<PhysNode>,
        /// Key, evaluated per (flat) input row.
        key: CompiledExpr,
        /// Output.
        out: DataPos,
        /// Output group.
        out_group: GroupId,
        /// Label.
        label: String,
        /// Property.
        property: String,
    },
    /// `SEARCH` subclause: top-k vector hits per input row.
    VectorSearch {
        /// Input.
        input: Box<PhysNode>,
        /// Node output.
        out: DataPos,
        /// Score output.
        score: Option<DataPos>,
        /// Output group.
        out_group: GroupId,
        /// Index.
        index_name: String,
        /// Query vector.
        query: CompiledExpr,
        /// In-index filter, over the output group.
        index_where: Option<CompiledExpr>,
        /// `k`.
        limit: CompiledExpr,
    },
    /// At most `count` rows. With at most one unflat input group the rows are cut by narrowing
    /// that group's selection and the layout is unchanged; otherwise (or when the input must run
    /// to completion first) the rows are consumed into a flat group.
    Limit {
        /// Input.
        input: Box<PhysNode>,
        /// The count.
        count: CompiledExpr,
        /// Groups the rows span.
        in_groups: Vec<GroupId>,
        /// The input's groups must run to completion first (writes below).
        materialize: bool,
        /// Row-consuming form: the output group and the columns copied into it.
        consume: Option<Consume>,
    },
    /// All but the first `count` rows; forms as for [`PhysNode::Limit`].
    Skip {
        /// Input.
        input: Box<PhysNode>,
        /// The count.
        count: CompiledExpr,
        /// Groups the rows span.
        in_groups: Vec<GroupId>,
        /// Row-consuming form.
        consume: Option<Consume>,
    },
    /// Sorted rows, materialized.
    Sort {
        /// Input.
        input: Box<PhysNode>,
        /// Keys and ascending flags.
        keys: Vec<(CompiledExpr, bool)>,
        /// Groups the input's rows span.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Columns copied through.
        moves: Vec<(DataPos, DataPos)>,
        /// A `LIMIT` right above: only this many rows are kept while sorting.
        limit: Option<CompiledExpr>,
    },
    /// Grouped aggregation.
    Aggregate {
        /// Input.
        input: Box<PhysNode>,
        /// Grouping keys.
        keys: Vec<(CompiledExpr, DataPos)>,
        /// Aggregates.
        aggs: Vec<AggSpec>,
        /// Items computed over the finished group.
        post: Vec<PostAgg>,
        /// Groups the input's rows span.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
    },
    /// Runs the input to completion before emitting anything.
    Eager {
        /// Input.
        input: Box<PhysNode>,
        /// Groups the input's rows span.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Columns copied through.
        moves: Vec<(DataPos, DataPos)>,
    },
    /// Rows of the left then the right plan, deduplicated unless `all`.
    Union {
        /// Left.
        left: Box<PhysNode>,
        /// Right.
        right: Box<PhysNode>,
        /// Keep duplicates.
        all: bool,
        /// Output group.
        out_group: GroupId,
        /// Left columns -> output.
        moves_left: Vec<(DataPos, DataPos)>,
        /// Right columns -> output.
        moves_right: Vec<(DataPos, DataPos)>,
        /// Left row groups.
        left_groups: Vec<GroupId>,
        /// Right row groups.
        right_groups: Vec<GroupId>,
    },
    /// Cartesian product, left-major; both sides materialize.
    CrossProduct {
        /// Left.
        left: Box<PhysNode>,
        /// Right.
        right: Box<PhysNode>,
        /// Left row groups.
        left_groups: Vec<GroupId>,
        /// Right row groups.
        right_groups: Vec<GroupId>,
        /// Group the left rows are replayed in.
        left_out: GroupId,
        /// Left columns -> replay columns.
        moves_left: Vec<(DataPos, DataPos)>,
        /// Group the right rows are replayed in.
        right_out: GroupId,
        /// Right columns -> replay columns.
        moves_right: Vec<(DataPos, DataPos)>,
    },
    /// Equi-join: the left side is hashed, the right streams.
    HashJoin {
        /// Build side.
        left: Box<PhysNode>,
        /// Probe side.
        right: Box<PhysNode>,
        /// Build keys.
        left_keys: Vec<CompiledExpr>,
        /// Probe keys.
        right_keys: Vec<CompiledExpr>,
        /// Left row groups.
        left_groups: Vec<GroupId>,
        /// Right row groups.
        right_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Left columns -> output.
        moves_left: Vec<(DataPos, DataPos)>,
        /// Right columns -> output.
        moves_right: Vec<(DataPos, DataPos)>,
    },
    /// `OPTIONAL MATCH`: per input row the matches seeded with it, or the row with nulls.
    OptionalMatch {
        /// Input.
        input: Box<PhysNode>,
        /// The match, seeded per input row.
        body: Arc<SubPlan>,
        /// Input row groups.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Input columns -> output.
        moves: Vec<(DataPos, DataPos)>,
        /// Body output columns -> output.
        body_out: Vec<(DataPos, DataPos)>,
    },
    /// Binds a path variable from node and edge columns of its evaluation group.
    BindPath {
        /// Input.
        input: Box<PhysNode>,
        /// Output.
        out: DataPos,
        /// Node positions along the path.
        node_vars: Vec<DataPos>,
        /// Edge positions along the path.
        edge_vars: Vec<DataPos>,
    },
    /// Binds the per-hop source and target lists of a quantified group.
    BindQppGroups {
        /// Input.
        input: Box<PhysNode>,
        /// Anchor.
        anchor: DataPos,
        /// Edge list.
        rel: DataPos,
        /// Source-list output.
        src: Option<DataPos>,
        /// Target-list output.
        dst: Option<DataPos>,
    },
    /// Binds list variables from member columns.
    BindVarLists {
        /// Input.
        input: Box<PhysNode>,
        /// `(output, members)`.
        groups: Vec<(DataPos, Vec<Option<DataPos>>)>,
    },
    /// Endpoints of a pre-bound variable-length edge list; rows whose list isn't connected or
    /// contradicts bound endpoints are dropped.
    BindPathEndpoints {
        /// Input.
        input: Box<PhysNode>,
        /// The list.
        rel: DataPos,
        /// Start output.
        from: DataPos,
        /// End output.
        to: DataPos,
        /// Direction.
        direction: RelDirection,
        /// Output group.
        out_group: GroupId,
        /// Input row groups.
        in_groups: Vec<GroupId>,
        /// Input columns -> output.
        moves: Vec<(DataPos, DataPos)>,
        /// Start already bound before this operator.
        from_bound: Option<DataPos>,
        /// End already bound before this operator.
        to_bound: Option<DataPos>,
    },
    /// Endpoints of a pre-bound single edge (both orientations when undirected).
    BindEdgeEndpoints {
        /// Input.
        input: Box<PhysNode>,
        /// The edge.
        edge: DataPos,
        /// Start output.
        from: DataPos,
        /// End output.
        to: DataPos,
        /// Direction.
        direction: RelDirection,
        /// Types the edge must have.
        edge_types: Vec<String>,
        /// Output group.
        out_group: GroupId,
        /// Input row groups.
        in_groups: Vec<GroupId>,
        /// Input columns -> output.
        moves: Vec<(DataPos, DataPos)>,
        /// Start already bound before this operator.
        from_bound: Option<DataPos>,
        /// End already bound before this operator.
        to_bound: Option<DataPos>,
    },
    /// `CALL proc(...) YIELD ...`.
    CallProcedure {
        /// Input.
        input: Box<PhysNode>,
        /// Name as written.
        name: String,
        /// Arguments; `None` = implicit from parameters.
        args: Option<Vec<crate::ast::Expr>>,
        /// `(output column, variable)`.
        yield_columns: Vec<(String, String)>,
        /// Input row groups.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Layout before the call, for the procedure boundary.
        in_layout: Layout,
        /// Layout after the call.
        out_layout: Layout,
    },
    /// `CALL { body }` per input row.
    CallSubquery {
        /// Input.
        input: Box<PhysNode>,
        /// The body.
        body: Arc<SubPlan>,
        /// Input row groups.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Input columns -> output.
        moves: Vec<(DataPos, DataPos)>,
        /// Body output columns -> output.
        body_out: Vec<(DataPos, DataPos)>,
    },
    /// `CALL { body } IN TRANSACTIONS`.
    CallInTransactions {
        /// Input.
        input: Box<PhysNode>,
        /// The body, seeded per row.
        body: Arc<SubPlan>,
        /// Batch size.
        batch_size: Option<CompiledExpr>,
        /// Error policy.
        on_error: OnErrorBehavior,
        /// `REPORT STATUS AS` output.
        report_status: Option<DataPos>,
        /// Input row groups.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Input columns -> output.
        moves: Vec<(DataPos, DataPos)>,
        /// Body output columns -> output.
        body_out: Vec<(DataPos, DataPos)>,
    },
    /// `FOREACH`: the body per list element, input rows pass through.
    ForEach {
        /// Input.
        input: Box<PhysNode>,
        /// The list.
        list: CompiledExpr,
        /// The body, seeded with the input row and the element.
        body: Arc<SubPlan>,
        /// Input row groups.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Input columns -> output.
        moves: Vec<(DataPos, DataPos)>,
    },
    /// Shortest paths between two bound endpoints.
    ShortestPath {
        /// Input.
        input: Box<PhysNode>,
        /// Start.
        from: DataPos,
        /// End.
        to: DataPos,
        /// Types.
        edge_types: Vec<String>,
        /// Direction.
        direction: RelDirection,
        /// Minimum depth.
        min_depth: usize,
        /// Maximum depth.
        max_depth: usize,
        /// Path output.
        path: Option<DataPos>,
        /// Edge-list output.
        rel: Option<DataPos>,
        /// All shortest paths.
        all: bool,
        /// `SHORTEST k`.
        k: Option<usize>,
        /// `SHORTEST k GROUPS`.
        k_groups: bool,
        /// Output group.
        out_group: GroupId,
    },
    /// Weighted shortest path between two bound endpoints.
    WeightedShortestPath {
        /// Input.
        input: Box<PhysNode>,
        /// Start.
        from: DataPos,
        /// End.
        to: DataPos,
        /// Types.
        edge_types: Vec<String>,
        /// Direction.
        direction: RelDirection,
        /// Weight property.
        weight_property: String,
        /// Path output.
        path: Option<DataPos>,
        /// Edge-list output.
        rel: Option<DataPos>,
        /// Cost output.
        cost: Option<DataPos>,
        /// Output group.
        out_group: GroupId,
    },
    /// `LOAD CSV`.
    LoadCsv {
        /// Input.
        input: Box<PhysNode>,
        /// File or URL.
        url: CompiledExpr,
        /// Header row?
        with_headers: bool,
        /// Row output.
        row: DataPos,
        /// Hidden context index output.
        ctx: DataPos,
        /// Output group.
        out_group: GroupId,
        /// Field terminator.
        field_terminator: Option<char>,
    },
    /// `CREATE (n ...)`.
    CreateNode {
        /// Input.
        input: Box<PhysNode>,
        /// Output.
        out: Option<DataPos>,
        /// Static labels.
        labels: Vec<String>,
        /// Dynamic labels.
        dynamic_labels: Vec<CompiledExpr>,
        /// Properties.
        properties: Option<CompiledExpr>,
        /// Input row groups.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Input columns -> output.
        moves: Vec<(DataPos, DataPos)>,
    },
    /// `CREATE ()-[r]->()`.
    CreateRel {
        /// Input.
        input: Box<PhysNode>,
        /// Output.
        out: Option<DataPos>,
        /// Start.
        from: DataPos,
        /// End.
        to: DataPos,
        /// Type.
        edge_type: String,
        /// Dynamic type.
        dynamic_edge_type: Option<CompiledExpr>,
        /// Properties.
        properties: Option<CompiledExpr>,
        /// Input row groups.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Input columns -> output.
        moves: Vec<(DataPos, DataPos)>,
    },
    /// `SET`.
    SetProps {
        /// Input.
        input: Box<PhysNode>,
        /// Items.
        items: Vec<SetAction>,
        /// Input row groups.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Input columns -> output.
        moves: Vec<(DataPos, DataPos)>,
    },
    /// `REMOVE`.
    Remove {
        /// Input.
        input: Box<PhysNode>,
        /// Items.
        items: Vec<RemoveAction>,
        /// Input row groups.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Input columns -> output.
        moves: Vec<(DataPos, DataPos)>,
    },
    /// `DELETE` / `DETACH DELETE`; a barrier.
    Delete {
        /// Input.
        input: Box<PhysNode>,
        /// What to delete.
        exprs: Vec<CompiledExpr>,
        /// Detach.
        detach: bool,
        /// Input row groups.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Input columns -> output.
        moves: Vec<(DataPos, DataPos)>,
    },
    /// `MERGE`.
    Merge {
        /// Input.
        input: Box<PhysNode>,
        /// The pattern.
        pattern: crate::ast::Pattern,
        /// `ON CREATE SET`.
        on_create: Option<Vec<SetAction>>,
        /// `ON MATCH SET`.
        on_match: Option<Vec<SetAction>>,
        /// Variables the pattern binds: `(name, output)`.
        binds: Vec<(String, DataPos)>,
        /// Layout after the merge, for the per-row evaluation.
        out_layout: Layout,
        /// Input row groups.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Input columns -> output.
        moves: Vec<(DataPos, DataPos)>,
    },
    /// A schema statement.
    Schema {
        /// The statement.
        op: SchemaOp,
        /// The (empty) group it activates for the empty result.
        group: GroupId,
    },
    /// A `SHOW` listing.
    Show {
        /// What.
        op: ShowOp,
        /// Output columns in order.
        cols: Vec<DataPos>,
        /// Output group.
        out_group: GroupId,
    },
    /// `SHOW TRANSACTIONS` / `TERMINATE TRANSACTIONS` per input row.
    TransactionCommand {
        /// Input.
        input: Box<PhysNode>,
        /// Which.
        command: TransactionCommandKind,
        /// Ids.
        ids: Option<CompiledExpr>,
        /// Output columns in order.
        cols: Vec<DataPos>,
        /// Input row groups.
        in_groups: Vec<GroupId>,
        /// Output group.
        out_group: GroupId,
        /// Input columns -> output.
        moves: Vec<(DataPos, DataPos)>,
    },
    /// Bound-entity capture at a projection boundary: the entities the rows hold, for result
    /// provenance.
    Capture {
        /// Input.
        input: Box<PhysNode>,
        /// Entity positions.
        positions: Vec<DataPos>,
    },
    /// Emits nothing (a predicate folded to `false`).
    NoRows,
}

/// One hop of a counted chain.
#[derive(Debug, Clone)]
pub struct ChainHop {
    /// Direction.
    pub dir: Direction,
    /// Types; empty = all.
    pub edge_types: Vec<String>,
    /// Label the hop's far end must have.
    pub to_label: Option<String>,
}

/// The flat output of a row-consuming operator.
#[derive(Debug)]
pub struct Consume {
    /// Output group.
    pub out_group: GroupId,
    /// Input columns -> output columns.
    pub moves: Vec<(DataPos, DataPos)>,
}

/// What a [`PhysNode::CountSource`] counts.
#[derive(Debug)]
pub enum CountKind {
    /// Nodes, optionally with a label.
    Nodes {
        /// Label.
        label: Option<String>,
    },
    /// Edges of the given types; undirected scans count each twice.
    Edges {
        /// Types.
        edge_types: Vec<String>,
        /// Direction as written.
        direction: Direction,
        /// Label the start node must have.
        from_label: Option<String>,
        /// Label the end node must have.
        to_label: Option<String>,
    },
    /// Entries of a property index in a range.
    IndexRange {
        /// Label.
        label: String,
        /// Property.
        property: String,
        /// Comparison.
        op: CmpOp,
        /// Bound.
        value: CompiledExpr,
    },
    /// Entries of a property index with a value.
    IndexEq {
        /// Label.
        label: String,
        /// Property.
        property: String,
        /// Key.
        value: CompiledExpr,
    },
    /// Edges out of (or into) the nodes of a label whose property compares with a value: the
    /// nodes come off the storage column, their qualifying edges are counted without rows.
    DegreeSum {
        /// Label.
        label: String,
        /// Property.
        property: String,
        /// Comparison.
        op: CmpOp,
        /// Bound.
        value: CompiledExpr,
        /// Direction of the hop.
        dir: Direction,
        /// Types; empty = all.
        edge_types: Vec<String>,
        /// Label the far end must have.
        to_label: Option<String>,
    },
    /// Nodes of a label whose property compares with a value, counted over the storage column.
    ColumnRange {
        /// Label.
        label: String,
        /// Property.
        property: String,
        /// Comparison.
        op: CmpOp,
        /// Bound.
        value: CompiledExpr,
    },
}

/// One `SET` item with its expressions compiled against the row.
#[derive(Debug)]
pub enum SetAction {
    /// `var.key = value`.
    Property {
        /// The entity.
        entity: DataPos,
        /// The key.
        key: String,
        /// The value.
        value: CompiledExpr,
    },
    /// `var = map`.
    AllProperties {
        /// The entity.
        entity: DataPos,
        /// The map.
        value: CompiledExpr,
    },
    /// `var += map`.
    MergeProperties {
        /// The entity.
        entity: DataPos,
        /// The map.
        value: CompiledExpr,
    },
    /// `var:Label...`.
    Labels {
        /// The node.
        entity: DataPos,
        /// Static labels.
        labels: Vec<String>,
        /// Dynamic labels.
        dynamic: Vec<CompiledExpr>,
    },
    /// `var[key] = value`.
    DynamicProperty {
        /// The entity.
        entity: DataPos,
        /// The key.
        key: CompiledExpr,
        /// The value.
        value: CompiledExpr,
    },
    /// The item names a variable the row doesn't bind (a no-op, as a null entity).
    Unbound,
}

/// One `REMOVE` item.
#[derive(Debug)]
pub enum RemoveAction {
    /// `var.key`.
    Property {
        /// The entity.
        entity: DataPos,
        /// The key.
        key: String,
    },
    /// `var:Label...`.
    Labels {
        /// The node.
        entity: DataPos,
        /// Static labels.
        labels: Vec<String>,
        /// Dynamic labels.
        dynamic: Vec<CompiledExpr>,
    },
    /// Unbound variable.
    Unbound,
}

impl PhysNode {
    /// The operator's inputs.
    pub fn children(&self) -> Vec<&PhysNode> {
        match self {
            PhysNode::EmptyRow { .. }
            | PhysNode::Seed { .. }
            | PhysNode::NodeScan { .. }
            | PhysNode::NodeByIdSeek { .. }
            | PhysNode::NodeByIdRange { .. }
            | PhysNode::IndexSeek { .. }
            | PhysNode::IndexRange { .. }
            | PhysNode::TextIndexScan { .. }
            | PhysNode::PointIndexScan { .. }
            | PhysNode::EdgeTypeScan { .. }
            | PhysNode::Schema { .. }
            | PhysNode::Show { .. }
            | PhysNode::NoRows => Vec::new(),
            PhysNode::CountSource { .. } => Vec::new(),
            PhysNode::Nest { input, inner } => vec![input, inner],
            PhysNode::ChainWeights { exact, .. } => vec![exact],
            PhysNode::Flatten { input, .. }
            | PhysNode::Capture { input, .. }
            | PhysNode::Filter { input, .. }
            | PhysNode::AddColumn { input, .. }
            | PhysNode::Project { input, .. }
            | PhysNode::Expand { input, .. }
            | PhysNode::ExpandCount { input, .. }
            | PhysNode::VarExpand { input, .. }
            | PhysNode::DistinctExpand { input, .. }
            | PhysNode::Unwind { input, .. }
            | PhysNode::IndexLookupJoin { input, .. }
            | PhysNode::VectorSearch { input, .. }
            | PhysNode::Limit { input, .. }
            | PhysNode::Skip { input, .. }
            | PhysNode::Sort { input, .. }
            | PhysNode::Aggregate { input, .. }
            | PhysNode::Eager { input, .. }
            | PhysNode::OptionalMatch { input, .. }
            | PhysNode::BindPath { input, .. }
            | PhysNode::BindQppGroups { input, .. }
            | PhysNode::BindVarLists { input, .. }
            | PhysNode::BindPathEndpoints { input, .. }
            | PhysNode::BindEdgeEndpoints { input, .. }
            | PhysNode::CallProcedure { input, .. }
            | PhysNode::CallSubquery { input, .. }
            | PhysNode::CallInTransactions { input, .. }
            | PhysNode::ForEach { input, .. }
            | PhysNode::ShortestPath { input, .. }
            | PhysNode::WeightedShortestPath { input, .. }
            | PhysNode::LoadCsv { input, .. }
            | PhysNode::CreateNode { input, .. }
            | PhysNode::CreateRel { input, .. }
            | PhysNode::SetProps { input, .. }
            | PhysNode::Remove { input, .. }
            | PhysNode::Delete { input, .. }
            | PhysNode::Merge { input, .. }
            | PhysNode::TransactionCommand { input, .. } => vec![input],
            PhysNode::Union { left, right, .. }
            | PhysNode::CrossProduct { left, right, .. }
            | PhysNode::HashJoin { left, right, .. } => vec![left, right],
        }
    }

    /// Does any operator of the tree satisfy `pred`? Iterative.
    pub fn any(&self, pred: &dyn Fn(&PhysNode) -> bool) -> bool {
        let mut stack = vec![self];
        while let Some(n) = stack.pop() {
            if pred(n) {
                return true;
            }
            stack.extend(n.children());
        }
        false
    }
}

/// A literal row for [`PhysNode::Seed`].
pub type SeedRow = Vec<CypherValue>;
