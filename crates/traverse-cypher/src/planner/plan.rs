//! The logical plan: [`LogicalPlan`] and the seek descriptions it carries.

use traverse_core::graph::types::Direction;

use crate::ast::{
    CmpOp, Expr, Pattern, ProjectionItem, RelDirection, RemoveItem, SetClause, SetItem, SortItem,
};

/// Spatial predicate a [`LogicalPlan::PointIndexScan`] accelerates. Point / radius / bbox corners /
/// `k` stay [`Expr`]s evaluated at run time (they may be parameters), so no constant ends up in the
/// cached plan.
#[derive(Debug, Clone)]
pub enum SpatialQuery {
    /// `point.distance(n.prop, center) < radius` (or `<=`) -> within-radius.
    Distance {
        /// The query point expression.
        center: Expr,
        /// Radius (metres if geographic, else coordinate units).
        radius: Expr,
    },
    /// `point.withinBBox(n.prop, ll, ur)` -> points inside the box.
    BBox {
        /// Lower-left corner point.
        ll: Expr,
        /// Upper-right corner point.
        ur: Expr,
    },
    /// `ORDER BY point.distance(n.prop, center) LIMIT k` -> the `k` nearest.
    Knn {
        /// Query point.
        center: Expr,
        /// Neighbours to return.
        k: Expr,
    },
}

/// String predicate a [`LogicalPlan::TextIndexScan`] accelerates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextPredicateKind {
    /// `n.p = pattern`
    Equals,
    /// `n.p STARTS WITH pattern`
    StartsWith,
    /// `n.p ENDS WITH pattern`
    EndsWith,
    /// `n.p CONTAINS pattern`
    Contains,
}

/// Logical plan node.
#[derive(Debug, Clone)]
pub enum LogicalPlan {
    /// Scan all nodes, optionally filtered by label.
    Scan {
        /// Variable name to bind each node to.
        variable: String,
        /// Optional label filter.
        label: Option<String>,
    },
    /// Fused scan + filter + project, no intermediate `Vec<Frame>` for `MATCH (n:L) WHERE pred
    /// RETURN exprs`.
    ScanFilterProject {
        /// Variable name to bind each node to.
        variable: String,
        /// Optional label filter.
        label: Option<String>,
        /// Optional filter predicate.
        predicate: Option<Expr>,
        /// Projection items.
        items: Vec<ProjectionItem>,
        /// Whether to deduplicate results.
        distinct: bool,
        /// Limit absorbed from a parent Limit.
        limit: Option<usize>,
    },
    /// Expands outgoing edges from a bound node.
    ExpandOut {
        /// Input plan with the source rows.
        input: Box<LogicalPlan>,
        /// Source node variable (bound in input).
        from: String,
        /// Relationship variable (may be empty).
        rel_var: String,
        /// Target node variable.
        to: String,
        /// Edge type constraints.
        edge_types: Vec<String>,
        /// Variable-length range (min, max).
        range: Option<(Option<u32>, Option<u32>)>,
        /// Edge variables bound before this expand (targeted uniqueness checks).
        prior_edge_vars: Vec<String>,
    },
    /// Expands incoming edges to a bound node.
    ExpandIn {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Target node variable.
        from: String,
        /// Relationship variable.
        rel_var: String,
        /// Source node variable.
        to: String,
        /// Edge type constraints.
        edge_types: Vec<String>,
        /// Variable-length range.
        range: Option<(Option<u32>, Option<u32>)>,
        /// Edge variables bound before this expand (targeted uniqueness checks).
        prior_edge_vars: Vec<String>,
    },
    /// Expands edges in both directions.
    ExpandBoth {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// One endpoint variable.
        from: String,
        /// Relationship variable.
        rel_var: String,
        /// Other endpoint variable.
        to: String,
        /// Edge type constraints.
        edge_types: Vec<String>,
        /// Variable-length range.
        range: Option<(Option<u32>, Option<u32>)>,
        /// Edge variables bound before this expand (targeted uniqueness checks).
        prior_edge_vars: Vec<String>,
    },
    /// Multi-hop expand keeping only distinct targets. Replaces a chain of ExpandOut/In/Both when
    /// only distinct target ids matter: BFS frontier, O(|V|) per hop instead of O(degree^hops).
    DistinctExpand {
        /// Input plan with the source rows.
        input: Box<LogicalPlan>,
        /// Source node variable.
        from: String,
        /// Target node variable.
        to: String,
        /// Minimum hops.
        min_hops: usize,
        /// Maximum hops.
        max_hops: usize,
        /// Expansion direction.
        direction: Direction,
        /// Edge type constraints.
        edge_types: Vec<String>,
        /// Optional label filter on the target.
        target_label: Option<String>,
        /// One seen-set across all input rows, so each target is emitted at most once (DISTINCT
        /// pushed into the BFS).
        global_dedup: bool,
        /// Optional property filter on the target, evaluated during BFS so non-matching neighbors
        /// get no frame.
        target_filter: Option<Expr>,
    },
    /// Filters rows by a predicate.
    Filter {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Predicate expression.
        predicate: Expr,
    },
    /// Projects columns.
    Project {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Projection items.
        items: Vec<ProjectionItem>,
        /// Deduplicate?
        distinct: bool,
        /// Optional WHERE (evaluated on the merged old+new scope, then stripped).
        where_clause: Option<Expr>,
    },
    /// Sort rows.
    Sort {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Sort items.
        items: Vec<SortItem>,
    },
    /// Limits the row count.
    Limit {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Limit expression (must be an integer).
        count: Expr,
    },
    /// Skips rows.
    Skip {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Skip expression (must be an integer).
        count: Expr,
    },
    /// Aggregate rows.
    Aggregate {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Grouping key expressions.
        keys: Vec<ProjectionItem>,
        /// Aggregation expressions.
        aggs: Vec<ProjectionItem>,
    },
    /// Union of two plans.
    Union {
        /// Left plan.
        left: Box<LogicalPlan>,
        /// Right plan.
        right: Box<LogicalPlan>,
        /// Keep duplicates (UNION ALL).
        all: bool,
    },
    /// Unwinds a list into rows.
    Unwind {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// List expression.
        expr: Expr,
        /// Alias for each element.
        alias: String,
        /// When `expr` is a plain variable no later clause uses, the operator TAKES the
        /// list out of the slot instead of cloning it, and emitted frames carry a cleared
        /// slot. Removes the dominant cost of `WITH collect(...) AS xs UNWIND xs AS x`,
        /// where every downstream frame clone deep-copied the whole dead list.
        clear_source: bool,
    },
    /// `FOREACH (var IN list | updates)`: runs `body` once per element with `var`
    /// bound. Input rows pass through unchanged; only the body's graph mutations
    /// survive.
    ForEach {
        /// Input plan (rows pass through unchanged).
        input: Box<LogicalPlan>,
        /// Loop variable, bound to each element inside the body.
        variable: String,
        /// List iterated over, evaluated per input row.
        list: Expr,
        /// Update-clause plan rooted at an `EmptyRow` leaf; the executor swaps that leaf for
        /// per-element seed rows.
        body: Box<LogicalPlan>,
    },
    /// Runtime-only leaf emitting fixed pre-built frames. Never planned from source:
    /// `ForEachOp` (and other seeded-write callers) build it to feed write operators
    /// one seed row per iteration. Optimizer/cost passes never see it.
    SeededRows(Vec<crate::frame::Frame>),
    /// Emits exactly one frame: the CURRENT SUBQUERY SEED from the executor's
    /// thread-local. For slot-native pattern-comprehension plans, which are cached
    /// (so can't embed per-row frames like `SeededRows`) but need the outer row's
    /// bindings for lookup-join keys and filters.
    ContextSeed,
    /// Loads rows from a CSV file.
    LoadCsv {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// File path expression.
        url: Expr,
        /// CSV has a header row?
        with_headers: bool,
        /// Row alias variable.
        row_alias: String,
        /// Optional field terminator.
        field_terminator: Option<char>,
    },
    /// Creates a node.
    CreateNode {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Variable name (may be empty).
        variable: Option<String>,
        /// Static labels.
        labels: Vec<String>,
        /// Dynamic label expressions, evaluated per frame for extra label names.
        dynamic_labels: Vec<Expr>,
        /// Properties (MapLiteral).
        properties: Option<Expr>,
    },
    /// Creates a relationship.
    CreateRel {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Variable of the new edge.
        variable: Option<String>,
        /// Source node variable.
        from: String,
        /// Target node variable.
        to: String,
        /// Edge type; empty when `dynamic_edge_type` is set (it wins).
        edge_type: String,
        /// Dynamic edge type, resolved per frame and used instead of `edge_type`.
        dynamic_edge_type: Option<Expr>,
        /// Properties expression.
        properties: Option<Expr>,
    },
    /// Sets properties or labels.
    SetProperty {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Set items.
        items: Vec<SetItem>,
    },
    /// Removes properties or labels.
    RemoveOp {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Remove items.
        items: Vec<RemoveItem>,
    },
    /// Deletes nodes/edges.
    Delete {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Entities to delete.
        exprs: Vec<Expr>,
        /// DETACH DELETE.
        detach: bool,
    },
    /// MERGE: match or create.
    Merge {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Pattern to merge.
        pattern: Pattern,
        /// ON CREATE SET.
        on_create: Option<SetClause>,
        /// ON MATCH SET.
        on_match: Option<SetClause>,
    },
    /// OPTIONAL MATCH: left outer join.
    OptionalMatch {
        /// Input plan (left).
        input: Box<LogicalPlan>,
        /// Match plan (right).
        match_plan: Box<LogicalPlan>,
        /// Variables the match introduces.
        new_variables: Vec<String>,
    },
    /// Cartesian product of two plans.
    CrossProduct {
        /// Left plan.
        left: Box<LogicalPlan>,
        /// Right plan.
        right: Box<LogicalPlan>,
    },
    /// Hash join: build a table on one side, probe from the other. Replaces CrossProduct + equality
    /// filter for equi-joins.
    HashJoin {
        /// Build side (smaller).
        left: Box<LogicalPlan>,
        /// Probe side (larger, scanned once).
        right: Box<LogicalPlan>,
        /// Left join keys.
        left_keys: Vec<Expr>,
        /// Right join keys.
        right_keys: Vec<Expr>,
    },
    /// Runs `input` to completion before emitting rows.
    ///
    /// Inserted by the eagerness analysis (`optimizer::eager`) between a streaming
    /// write and a read that could otherwise see it out of clause order: the same
    /// operator Neo4j's planner adds, shown as `Eager` in `EXPLAIN`. Its buffer is
    /// charged to the query's memory budget.
    Eager {
        /// Input plan.
        input: Box<LogicalPlan>,
    },
    /// Binds a named path variable from node/edge variables.
    BindPath {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Path variable.
        variable: String,
        /// Node variables along the path.
        node_vars: Vec<String>,
        /// Edge variables along the path.
        edge_vars: Vec<String>,
    },
    /// Binds each `var` to the LIST of its per-iteration slot values: ISO section 16.11
    /// group variables of a multi-step quantified group unrolled into the pattern
    /// (GQL lowering). An empty member list binds an empty LIST.
    BindVarLists {
        /// Input plan (binds every member slot).
        input: Box<LogicalPlan>,
        /// `(target_var, [member_slot, ...])` bindings, applied in order.
        groups: Vec<(String, Vec<String>)>,
    },
    /// Binds the per-iteration node lists of a quantified path-pattern group
    /// `((x)-[r]->(y)){m,n}` (ISO section 16.11 / Neo4j group variables): from the anchor
    /// and the var-length edge list, `x` = each hop's source, `y` = each hop's
    /// target. Zero iterations bind empty lists.
    BindQppGroups {
        /// Input plan (binds `anchor_var` and `rel_var`).
        input: Box<LogicalPlan>,
        /// Node the group expands from (traversal origin).
        anchor_var: String,
        /// Var-length relationship variable (LIST of edges in traversal order).
        rel_var: String,
        /// Per-hop source-node list variable (`x`), if named.
        src_var: Option<String>,
        /// Per-hop target-node list variable (`y`), if named.
        dst_var: Option<String>,
    },
    /// Path endpoints from a pre-bound variable-length relationship list.
    BindPathEndpoints {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Pre-bound relationship list variable.
        rel_var: String,
        /// Start node variable (source of the first edge).
        from_var: String,
        /// End node variable (target of the last edge).
        to_var: String,
        /// Path direction.
        direction: RelDirection,
    },
    /// Binds endpoint variables from a pre-bound single edge variable: a relationship
    /// variable bound in an outer scope (e.g. OPTIONAL MATCH reusing one from the
    /// prior MATCH) with unbound endpoints. Avoids a full node scan.
    BindEdgeEndpoints {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Pre-bound edge variable.
        edge_var: String,
        /// Source-side endpoint variable.
        from_var: String,
        /// Target-side endpoint variable.
        to_var: String,
        /// Pattern direction. Outgoing: from = src, to = tgt; Incoming: from = tgt, to = src; Both:
        /// both orientations.
        direction: RelDirection,
        /// Edge type filter the bound edge must match (empty = none).
        edge_types: Vec<String>,
    },
    /// Calls a procedure.
    CallProcedure {
        /// Input plan.
        input: Box<LogicalPlan>,
        /// Fully qualified procedure name.
        procedure_name: String,
        /// Explicit arguments (None = implicit from query params).
        args: Option<Vec<Expr>>,
        /// Yield columns: (output_name, bound_variable).
        yield_columns: Vec<(String, String)>,
    },
    /// One empty row (start for CREATE without MATCH).
    EmptyRow,
    /// Node scan via a property index (O(log n)).
    IndexScan {
        /// Variable for each matching node.
        variable: String,
        /// Label filter.
        label: String,
        /// Property key to look up.
        property: String,
        /// Value to look up.
        value: Expr,
    },
    /// Index seek driven by the INPUT: per input row, evaluate `key` (may use input
    /// variables: `UNWIND $rows AS r MATCH (a:L {id: r.a})`, correlated
    /// `WHERE a.id = m.id + 1`) and join the row with each hit. Replaces
    /// `Filter(var.prop = key) over CrossProduct(input, Scan(var:label))` when
    /// `(label, prop)` is indexed: a lookup join instead of O(input x label).
    IndexLookupJoin {
        /// Driving input.
        input: Box<LogicalPlan>,
        /// Variable for each matching node.
        variable: String,
        /// Label of the sought nodes.
        label: String,
        /// Indexed property key.
        property: String,
        /// Seek key, evaluated per input row.
        key: Expr,
    },
    /// Node ids in a bounded range, optional label check. Replaces
    /// `Scan + Filter(id(n) <cmp> lit)` for `id(n) < N`, `> N`, BETWEEN and
    /// conjunctions of those. `lower` inclusive, `upper` exclusive; `None` =
    /// unbounded, clamped to `[0, node_count)`.
    NodeByIdRange {
        /// Variable for each matching node.
        variable: String,
        /// Optional label each node must have.
        label: Option<String>,
        /// Inclusive lower bound; `None` = unbounded (clamped to `0`). Evaluated at execute time,
        /// so the plan caches across `$param` values.
        lower: Option<Expr>,
        /// Exclusive upper bound; `None` = unbounded (clamped to `node_count`). Evaluated at
        /// execute time like `lower`.
        upper: Option<Expr>,
    },
    /// Every edge of the given types straight from the edge-type index, binding both
    /// endpoints and the rel variable. Replaces `ExpandOut/In/Both { input: Scan(anon,
    /// None) }` for anchorless queries like `MATCH ()-[r:LINK]->() RETURN sum(r.weight)`:
    /// O(|edges_of_T|) instead of O(|V| + |edges_of_T|).
    EdgeTypeScan {
        /// Source node of each edge.
        from_var: String,
        /// The relationship (empty if anonymous).
        rel_var: String,
        /// Target node of each edge.
        to_var: String,
        /// Edge type filter. Never empty: "all edges" is always better served by `all_edges_vec`,
        /// so the rewrite needs a type.
        edge_types: Vec<String>,
        /// Direction as written (Outgoing / Incoming / Both): whether `from_var`/`to_var` map to
        /// source/target or swapped, and whether each edge is emitted once or twice.
        direction: traverse_core::graph::types::Direction,
    },
    /// Direct node lookup by internal id. `convert_filter_to_node_by_id` makes it from
    /// `Scan{var, label} + Filter{id(var) = lit-or-param}` when the predicate pins one
    /// id: an O(1) lookup + label check instead of an O(N) scan.
    NodeById {
        /// Variable for the node.
        variable: String,
        /// Optional label the node must have.
        label: Option<String>,
        /// Integer expression for the internal node id, usually `Expr::Literal(Integer)` or
        /// `Expr::Parameter(_)`.
        id_expr: Expr,
    },
    /// B-tree index range scan (>, >=, <, <=).
    IndexRangeScan {
        /// Variable for each matching node.
        variable: String,
        /// Label filter.
        label: String,
        /// Property key to scan.
        property: String,
        /// Comparison (Gt, Gte, Lt, Lte).
        op: CmpOp,
        /// Boundary value.
        value: Expr,
    },
    /// `SEARCH` subclause (Cypher 25): per input row, bind `variable` to the top-k
    /// ANN hits of a vector index (by descending similarity), optionally the score
    /// to `score_var`. `index_where` makes it filtered kNN (top-k among rows passing
    /// it). Unary: the query vector may use earlier variables.
    VectorSearchScan {
        /// Input plan (EmptyRow when SEARCH comes first).
        input: Box<LogicalPlan>,
        /// Variable for each result node.
        variable: String,
        /// Vector index name.
        index_name: String,
        /// Query vector (must not use `variable`).
        query_vector: Expr,
        /// Optional in-index filter, per candidate.
        index_where: Option<Expr>,
        /// Nearest neighbours to return (`LIMIT k`).
        limit: Expr,
        /// `SCORE AS alias` variable, if requested.
        score_var: Option<String>,
    },
    /// Trigram-accelerated candidates for a string predicate (`CONTAINS` / `ENDS WITH`
    /// / `STARTS WITH` / `=`): a superset, the enclosing `Filter` still runs the exact
    /// predicate. A non-string / null pattern at runtime falls back to the label
    /// scan (still exact).
    TextIndexScan {
        /// Variable for each candidate.
        variable: String,
        /// Label filter.
        label: String,
        /// Indexed string property.
        property: String,
        /// Accelerated predicate.
        kind: TextPredicateKind,
        /// Pattern (evaluated at run time; may be a parameter).
        pattern: Expr,
    },
    /// R-tree-accelerated candidates for a spatial predicate: a superset for
    /// radius/bbox, exactly the k nearest for kNN. The enclosing
    /// `Filter`/`Sort`/`Limit` still runs for exact semantics.
    PointIndexScan {
        /// Variable for each candidate.
        variable: String,
        /// Label filter.
        label: String,
        /// Point property.
        property: String,
        /// Accelerated predicate.
        query: SpatialQuery,
    },
    /// Creates a property index.
    CreateIndex {
        /// Index name.
        name: String,
        /// Label to index.
        label: String,
        /// Property to index.
        property: String,
        /// Further keys (composite `ON (n.a, n.b)`).
        extra_properties: Vec<String>,
    },
    /// Creates a vector (HNSW) index.
    CreateVectorIndex {
        /// Index name.
        name: String,
        /// Label to index.
        label: String,
        /// Vector property.
        property: String,
        /// Dimensionality (`None` = infer from data).
        dimensions: Option<usize>,
        /// Similarity function (`cosine` / `euclidean`).
        similarity: String,
        /// int8 quantization (`None` = engine default).
        quantization: Option<bool>,
    },
    /// Creates a BM25 full-text index.
    CreateFulltextIndex {
        /// Index name.
        name: String,
        /// Labels to index.
        labels: Vec<String>,
        /// String properties to index.
        properties: Vec<String>,
        /// Analyzer name (`None` = `standard`).
        analyzer: Option<String>,
    },
    /// Creates an R-tree point index.
    CreatePointIndex {
        /// Index name.
        name: String,
        /// Label to index.
        label: String,
        /// Point property.
        property: String,
    },
    /// Creates a trigram text index.
    CreateTextIndex {
        /// Index name.
        name: String,
        /// Label to index.
        label: String,
        /// String property.
        property: String,
    },
    /// Drops a property index.
    DropIndex {
        /// Index name.
        name: String,
    },
    /// Collects graph statistics.
    AnalyzeGraph {
        /// Labels to analyze (empty = all).
        labels: Vec<String>,
    },
    /// Creates a constraint.
    CreateConstraint {
        /// Constraint name.
        name: String,
        /// Label (node) or relationship type.
        label: String,
        /// Property (FIRST key of a composite constraint).
        property: String,
        /// Further keys of a composite UNIQUE / KEY constraint.
        extra_properties: Vec<String>,
        /// "UNIQUE" / "NOT NULL" / "NODE KEY" / "TYPE `<T>`".
        constraint_type: String,
        /// Relationship constraint (`FOR ()-[r:T]-()`)?
        is_relationship: bool,
        /// `IF NOT EXISTS`: no-op when the name is taken.
        if_not_exists: bool,
    },
    /// Drops a constraint.
    DropConstraint {
        /// Constraint name.
        name: String,
    },
    /// Creates an edge index.
    CreateEdgeIndex {
        /// Index name.
        name: String,
        /// Edge type.
        edge_type: String,
        /// Property (None = type-only).
        property: Option<String>,
        /// Further keys (composite relationship indexes).
        extra_properties: Vec<String>,
    },
    /// Drops an edge index.
    DropEdgeIndex {
        /// Index name.
        name: String,
    },
    /// SHOW INDEXES: all node and edge indexes.
    ShowIndexes,
    /// SHOW CONSTRAINTS.
    ShowConstraints,
    /// SHOW FUNCTIONS.
    ShowFunctions,
    /// SHOW PROCEDURES.
    ShowProcedures,
    /// `SHOW TRANSACTIONS` / `TERMINATE TRANSACTIONS`: for each input row, lists the live
    /// transactions (those in `ids` when given) or terminates `ids`, appending the command's
    /// columns to the row.
    TransactionCommand {
        /// Rows to run it for: one empty row, or a previous command's.
        input: Box<LogicalPlan>,
        /// Which command.
        command: crate::ast::TransactionCommandKind,
        /// Transaction ids, a STRING or LIST<STRING>.
        ids: Option<Expr>,
    },
    /// SHOW SETTINGS leaf.
    ShowSettings,
    /// Dijkstra weighted shortest path between two bound endpoints.
    WeightedShortestPath {
        /// Input plan with both endpoints.
        input: Box<LogicalPlan>,
        /// Source node variable (bound in input).
        from: String,
        /// Target node variable (bound in input).
        to: String,
        /// Edge type constraints.
        edge_types: Vec<String>,
        /// Relationship direction.
        direction: RelDirection,
        /// Weight property (e.g. "cost").
        weight_property: String,
        /// Optional path variable.
        path_var: Option<String>,
        /// Optional relationship variable.
        rel_var: Option<String>,
        /// Optional total-cost variable.
        cost_var: Option<String>,
    },
    /// BFS shortest path between two bound endpoints.
    ShortestPath {
        /// Input plan with both endpoints.
        input: Box<LogicalPlan>,
        /// Source node variable (bound in input).
        from: String,
        /// Target node variable (bound in input).
        to: String,
        /// Edge type constraints.
        edge_types: Vec<String>,
        /// Relationship direction.
        direction: RelDirection,
        /// Minimum path depth.
        min_depth: usize,
        /// Maximum path depth.
        max_depth: usize,
        /// Optional path variable (`p` in `p = shortestPath(...)`).
        path_var: Option<String>,
        /// Optional relationship variable (`r` in `-[r:KNOWS*]->`).
        rel_var: Option<String>,
        /// `false` = shortestPath (single), `true` = allShortestPaths.
        all: bool,
        /// `Some(k)`: GQL section 14.6 `SHORTEST k`, top-k loopless shortest paths (Yen with BFS inside).
        /// `None`: single/all via `all`, which `Some` ignores.
        k: Option<usize>,
        /// With `k`: `SHORTEST k` (false, exactly `k` paths) vs `SHORTEST k GROUPS` (true, every
        /// path whose length is among the `k` smallest). Ignored without `k`.
        k_groups: bool,
    },
    /// ISO 39075 section 9.4 inline procedure subquery (`CALL [(imports)] { body }`). Per
    /// input row: narrow the outer record to the imports (all of it when `imports`
    /// is `None`), run `body` on it, cross-join each result onto the outer row.
    /// `return_aliases` are the names the body's RETURN brings into scope.
    CallSubquery {
        /// Input plan with outer rows.
        input: Box<LogicalPlan>,
        /// Explicit imports (`CALL (a, b) { ... }`); `None` = transparent scope.
        imports: Option<Vec<String>>,
        /// Pre-planned body, run via `exec_subquery` per outer row.
        body: Box<LogicalPlan>,
        /// Names the body's RETURN brings into the outer scope.
        return_aliases: Vec<String>,
    },
    /// `CALL { ... } IN TRANSACTIONS`: body per input row with writes, committed in
    /// batches. Unlike `CallSubquery` the body runs through the write-capable
    /// operator pipeline (`SeededRows` leaf), not the read-only record interpreter.
    CallInTransactions {
        /// Input plan with outer rows.
        input: Box<LogicalPlan>,
        /// Pre-planned body (its `EmptyRow` leaf is seeded per batch).
        body: Box<LogicalPlan>,
        /// `OF <expr> ROWS` batch size (default 1000).
        batch_size: Option<Expr>,
        /// `ON ERROR` policy.
        on_error: crate::ast::OnErrorBehavior,
        /// `REPORT STATUS AS var`.
        report_status: Option<String>,
        /// Names introduced by the body's RETURN.
        return_aliases: Vec<String>,
    },
}
