//! openCypher AST: produced by the parser, consumed by the planner.

/// Execution mode for a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    /// Normal execution.
    Normal,
    /// EXPLAIN: return the query plan without executing.
    Explain,
    /// PROFILE: execute and return the query plan with timing.
    Profile,
}

/// A complete Cypher statement.
#[derive(Debug, Clone)]
pub struct Statement {
    /// Execution mode (EXPLAIN / PROFILE / normal).
    pub mode: QueryMode,
    /// The query body.
    pub query: Query,
}

/// Top-level query.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // one value per query; boxing would only add indirection
pub enum Query {
    /// A regular (non-schema) query.
    Regular(RegularQuery),
    /// CREATE INDEX name FOR (n:Label) ON (n.property)
    CreateIndex {
        /// Index name.
        name: String,
        /// Label to index.
        label: String,
        /// Property key to index.
        property: String,
        /// Additional property keys for composite `ON (n.a, n.b)` indexes.
        extra_properties: Vec<String>,
    },
    /// DROP INDEX name
    DropIndex {
        /// Index name to drop.
        name: String,
    },
    /// CREATE VECTOR INDEX name FOR (n:Label) ON (n.prop)
    /// OPTIONS { indexConfig: { 'vector.dimensions': N,
    ///                          'vector.similarity_function': 'cosine' } }
    CreateVectorIndex {
        /// Index name.
        name: String,
        /// Node label to index.
        label: String,
        /// Vector property to index.
        property: String,
        /// Declared vector dimensionality (`None` = infer from data).
        dimensions: Option<usize>,
        /// Similarity function: `"cosine"` (default) or `"euclidean"`.
        similarity: String,
        /// int8 quantization (`vector.quantization.enabled`). `None` =
        /// engine default (on).
        quantization: Option<bool>,
    },
    /// CREATE FULLTEXT INDEX name FOR (n:Label|Label2) ON EACH [n.p1, n.p2]
    /// OPTIONS { indexConfig: { `fulltext.analyzer`: 'english' } }
    CreateFulltextIndex {
        /// Index name.
        name: String,
        /// One or more node labels to index.
        labels: Vec<String>,
        /// One or more string properties to index.
        properties: Vec<String>,
        /// Analyzer name (`None` = `standard`).
        analyzer: Option<String>,
    },
    /// CREATE POINT INDEX name FOR (n:Label) ON (n.prop) - an R-tree over a
    /// spatial POINT property, transparently accelerating `point.distance`,
    /// `point.withinBBox`, and `ORDER BY point.distance ... LIMIT k`.
    CreatePointIndex {
        /// Index name.
        name: String,
        /// Node label to index.
        label: String,
        /// Point property to index.
        property: String,
    },
    /// CREATE TEXT INDEX name FOR (n:Label) ON (n.prop) - a trigram index
    /// over a STRING property, transparently accelerating `CONTAINS`,
    /// `ENDS WITH`, `STARTS WITH`, and `=` string predicates.
    CreateTextIndex {
        /// Index name.
        name: String,
        /// Node label to index.
        label: String,
        /// String property to index.
        property: String,
    },
    /// ANALYZE GRAPH [LABEL label1, label2]
    AnalyzeGraph {
        /// Labels to analyze (empty = all labels).
        labels: Vec<String>,
    },
    /// CREATE CONSTRAINT name FOR (n:Label) REQUIRE n.prop IS UNIQUE/NOT NULL
    CreateConstraint {
        /// Constraint name.
        name: String,
        /// Label (node constraint) or relationship type (rel constraint).
        label: String,
        /// Property key (the FIRST key of a composite constraint).
        property: String,
        /// Additional keys of a composite constraint (`REQUIRE (n.a, n.b)
        /// IS NODE KEY` / `IS UNIQUE`); empty for the single-property case.
        extra_properties: Vec<String>,
        /// Constraint type: "UNIQUE" / "NOT NULL" / "NODE KEY" / "TYPE `<T>`".
        constraint_type: String,
        /// `true` for `FOR ()-[r:TYPE]-()` (relationship constraint);
        /// `false` for `FOR (n:Label)` (node constraint).
        is_relationship: bool,
        /// `IF NOT EXISTS` - silently no-op when the name is taken.
        if_not_exists: bool,
    },
    /// DROP CONSTRAINT name
    DropConstraint {
        /// Constraint name.
        name: String,
    },
    /// CREATE EDGE INDEX ON :EdgeType(property) or CREATE EDGE INDEX ON :EdgeType
    CreateEdgeIndex {
        /// Index name.
        name: String,
        /// Edge type.
        edge_type: String,
        /// Property key (None = type-only index).
        property: Option<String>,
        /// Additional property keys for composite `ON (r.a, r.b)` indexes.
        extra_properties: Vec<String>,
    },
    /// DROP EDGE INDEX name
    DropEdgeIndex {
        /// Index name.
        name: String,
    },
    /// SHOW INDEXES
    ShowIndexes,
    /// SHOW CONSTRAINTS
    ShowConstraints,
    /// SHOW FUNCTIONS
    ShowFunctions,
    /// SHOW PROCEDURES
    ShowProcedures,
    /// `SHOW TRANSACTIONS` / `TERMINATE TRANSACTIONS`, alone or chained (each but the last ends in
    /// `YIELD`), with an optional trailing `RETURN`.
    TransactionCommands {
        /// The commands, in order.
        commands: Vec<TransactionCommand>,
        /// Trailing `RETURN ...`.
        return_clause: Option<ReturnClause>,
    },
    /// SHOW SETTINGS - the process settings registry.
    ShowSettings,
    /// A composable SHOW command: `SHOW <thing> [YIELD ...] [WHERE ...] [RETURN ...]`
    /// (Cypher 25 / Neo4j 2026.05). Lowers to the SHOW leaf feeding a normal
    /// projection/filter/sort/page pipeline.
    ShowComposable {
        /// Which SHOW source.
        kind: ShowKind,
        /// `YIELD items [WHERE][ORDER BY][SKIP][LIMIT]`, modelled as a WITH.
        yield_clause: Option<WithClause>,
        /// The bare `SHOW <thing> WHERE pred` form (no YIELD).
        where_only: Option<Expr>,
        /// Trailing `RETURN ...`.
        return_clause: Option<ReturnClause>,
        /// Internal pre-filter applied directly on the SHOW leaf's columns,
        /// BEFORE any user YIELD/WHERE - used by the typed forms
        /// (`SHOW FULLTEXT INDEXES` -> `type = 'FULLTEXT'`).
        pre_filter: Option<Expr>,
    },
}

/// Which catalog a `SHOW` command lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowKind {
    /// `SHOW INDEXES`
    Indexes,
    /// `SHOW CONSTRAINTS`
    Constraints,
    /// `SHOW FUNCTIONS`
    Functions,
    /// `SHOW PROCEDURES`
    Procedures,
    /// `SHOW SETTINGS`
    Settings,
}

/// One `SHOW TRANSACTIONS` or `TERMINATE TRANSACTIONS` command.
#[derive(Debug, Clone)]
pub struct TransactionCommand {
    /// Which command.
    pub kind: TransactionCommandKind,
    /// Transaction ids: a STRING or LIST<STRING> (comma-separated ids parse to a list). Required
    /// for TERMINATE; SHOW lists every transaction without.
    pub ids: Option<Expr>,
    /// `YIELD items [WHERE][ORDER BY][SKIP][LIMIT]`, modelled as a WITH.
    pub yield_clause: Option<WithClause>,
    /// `WHERE pred` without YIELD.
    pub where_only: Option<Expr>,
}

/// Which transaction command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionCommandKind {
    /// `SHOW TRANSACTIONS`
    Show,
    /// `TERMINATE TRANSACTIONS`
    Terminate,
}

impl TransactionCommandKind {
    /// Every column the command produces (`YIELD *`).
    pub fn columns(self) -> &'static [&'static str] {
        match self {
            TransactionCommandKind::Show => &crate::transactions::ALL_COLUMNS,
            TransactionCommandKind::Terminate => &crate::transactions::TERMINATE_COLUMNS,
        }
    }

    /// The columns it returns without `YIELD`.
    pub fn default_columns(self) -> &'static [&'static str] {
        match self {
            TransactionCommandKind::Show => &crate::transactions::DEFAULT_COLUMNS,
            TransactionCommandKind::Terminate => &crate::transactions::TERMINATE_COLUMNS,
        }
    }

    /// `SHOW TRANSACTIONS` / `TERMINATE TRANSACTIONS`.
    pub fn name(self) -> &'static str {
        match self {
            TransactionCommandKind::Show => "SHOW TRANSACTIONS",
            TransactionCommandKind::Terminate => "TERMINATE TRANSACTIONS",
        }
    }
}

/// A regular query consisting of a single query optionally UNIONed with others.
#[derive(Debug, Clone)]
pub struct RegularQuery {
    /// The first single query.
    pub single: SingleQuery,
    /// Optional UNION parts.
    pub unions: Vec<Union>,
}

/// A UNION clause joining two single queries.
#[derive(Debug, Clone)]
pub struct Union {
    /// If `true`, this is UNION ALL (keep duplicates).
    pub all: bool,
    /// The right-hand query of the union.
    pub query: SingleQuery,
}

/// A single query consists of multiple parts separated by WITH clauses,
/// followed by a final part.
#[derive(Debug, Clone)]
pub struct SingleQuery {
    /// Intermediate parts, each ending with a WITH clause.
    pub parts: Vec<QueryPart>,
    /// The final part (clauses + optional RETURN).
    pub final_part: FinalPart,
}

/// An intermediate query part: reading/updating clauses followed by WITH.
#[derive(Debug, Clone)]
pub struct QueryPart {
    /// Reading and updating clauses before the WITH.
    pub clauses: Vec<Clause>,
    /// The WITH clause that closes this part.
    pub with: WithClause,
}

/// The final part of a single query.
#[derive(Debug, Clone)]
pub struct FinalPart {
    /// Reading and updating clauses.
    pub clauses: Vec<Clause>,
    /// Optional RETURN clause.
    pub return_clause: Option<ReturnClause>,
}

/// A reading or updating clause.
#[derive(Debug, Clone)]
pub enum Clause {
    /// MATCH clause.
    Match(MatchClause),
    /// OPTIONAL MATCH clause.
    OptionalMatch(MatchClause),
    /// CREATE clause.
    Create(CreateClause),
    /// MERGE clause.
    Merge(MergeClause),
    /// DELETE or DETACH DELETE clause.
    Delete(DeleteClause),
    /// SET clause.
    Set(SetClause),
    /// REMOVE clause.
    Remove(RemoveClause),
    /// UNWIND clause.
    Unwind(UnwindClause),
    /// LOAD CSV clause.
    LoadCsv(LoadCsvClause),
    /// CALL clause.
    Call(CallClause),
    /// ISO 39075 section 9.4 / openCypher 5 inline procedure subquery (`CALL [(imports)] { body }`):
    /// `body` runs per input row with the imported variables (all outer ones when `imports` is
    /// `None`) seeded; its rows cross-join back onto the outer record.
    CallSubquery(CallSubqueryClause),
    /// openCypher `FOREACH (var IN list | update_clauses)`. Runs the
    /// update clauses once per element of `list` with `var` bound to
    /// the element, without changing the outer row cardinality.
    ForEach(ForEachClause),
}

/// `FOREACH (var IN list | update_clauses)`. The body may contain only
/// updating clauses - `CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE`, and
/// nested `FOREACH` (no `MATCH`/`WITH`/`RETURN`/`CALL`, matching Neo4j).
#[derive(Debug, Clone)]
pub struct ForEachClause {
    /// The loop variable bound to each list element inside the body.
    pub variable: String,
    /// The list expression iterated over.
    pub list: Expr,
    /// The updating clauses executed per element.
    pub updates: Vec<Clause>,
}

/// A MATCH clause with a pattern and optional WHERE filter.
#[derive(Debug, Clone)]
pub struct MatchClause {
    /// The graph pattern to match.
    pub pattern: Pattern,
    /// Optional WHERE predicate.
    pub where_clause: Option<Expr>,
    /// `USING INDEX` / `USING SCAN` hints on this MATCH. Variables and (label, property) are
    /// validated; the planner may still pick another plan if costs say so, as in Neo4j.
    pub hints: Vec<MatchHint>,
    /// Optional `SEARCH` subclause (Cypher 25, Neo4j 2026.02+): the binding variable comes from the
    /// top-k ANN hits of a vector index; label/property predicates on it and `WHERE` post-filter
    /// the hits.
    pub search: Option<SearchClause>,
}

/// The `SEARCH` subclause of a MATCH (Cypher 25):
/// `SEARCH var IN (VECTOR INDEX name FOR query_vector [WHERE pred] LIMIT k)
///  [SCORE AS alias]`.
#[derive(Debug, Clone)]
pub struct SearchClause {
    /// The pattern variable the search binds (must be bound by the pattern).
    pub binding: String,
    /// The vector index name (a literal identifier, not parameterizable).
    pub index_name: String,
    /// The query vector expression (`LIST<FLOAT|INTEGER>`); may reference
    /// variables from earlier clauses but not the binding variable.
    pub query_vector: Expr,
    /// Optional in-index filter: the top-k is computed *among* entities
    /// satisfying this predicate (filtered kNN), unlike the post-filter
    /// `WHERE` which trims the k results afterwards.
    pub index_where: Option<Expr>,
    /// The number of nearest neighbours to return (`LIMIT k`).
    pub limit: Expr,
    /// `SCORE AS alias` - bind the similarity score (FLOAT in [0, 1],
    /// descending) to a new variable.
    pub score_alias: Option<String>,
}

/// Planner hint attached to a `MATCH` via `USING INDEX ... ` /
/// `USING SCAN`. Hints reference a variable bound elsewhere in the
/// same pattern.
#[derive(Debug, Clone)]
pub enum MatchHint {
    /// `USING INDEX var:Label(prop)` - request an index-backed scan
    /// or seek on that (label, property) pair.
    Index {
        /// Pattern variable the hint applies to.
        variable: String,
        /// Label whose index to use.
        label: String,
        /// Property name on the index.
        property: String,
    },
    /// `USING SCAN var:Label` - request a label scan, suppressing
    /// any index path the optimizer might otherwise pick.
    Scan {
        /// Pattern variable the hint applies to.
        variable: String,
        /// Label to scan.
        label: String,
    },
}

/// A CREATE clause with a pattern to create.
#[derive(Debug, Clone)]
pub struct CreateClause {
    /// The pattern describing nodes/edges to create.
    pub pattern: Pattern,
}

/// A MERGE clause: match-or-create a pattern.
#[derive(Debug, Clone)]
pub struct MergeClause {
    /// The pattern to merge.
    pub pattern: Pattern,
    /// Optional ON CREATE SET actions.
    pub on_create: Option<SetClause>,
    /// Optional ON MATCH SET actions.
    pub on_match: Option<SetClause>,
}

/// A DELETE clause.
#[derive(Debug, Clone)]
pub struct DeleteClause {
    /// If `true`, this is DETACH DELETE.
    pub detach: bool,
    /// Expressions identifying the entities to delete.
    pub exprs: Vec<Expr>,
}

/// A SET clause with one or more set items.
#[derive(Debug, Clone)]
pub struct SetClause {
    /// Individual set operations.
    pub items: Vec<SetItem>,
}

/// A single SET operation.
#[derive(Debug, Clone)]
pub enum SetItem {
    /// Set a property: `n.name = 'value'`
    Property(PropertyAccess, Expr),
    /// Replace all properties: `n = {props}`
    AllProperties(String, Expr),
    /// Merge properties: `n += {props}`
    MergeProperties(String, Expr),
    /// Add label(s): `n:Label1:Label2:$($dynLabel)`. Static names go
    /// in the second slot, dynamic-expression spellings (`$param`
    /// and `$(expr)`) go in the third.
    Label(String, Vec<String>, Vec<Expr>),
    /// Dynamic property assignment: `n[$key] = value`. The key
    /// expression is evaluated per-row to produce a String which
    /// is then used as the property name.
    DynamicProperty {
        /// The container variable (typically a node or edge ref).
        variable: String,
        /// Expression that resolves to the property name (String).
        key: Expr,
        /// The value to assign.
        value: Expr,
    },
}

/// A REMOVE clause with one or more remove items.
#[derive(Debug, Clone)]
pub struct RemoveClause {
    /// Individual remove operations.
    pub items: Vec<RemoveItem>,
}

/// A single REMOVE operation.
#[derive(Debug, Clone)]
pub enum RemoveItem {
    /// Remove a property: `n.name`
    Property(PropertyAccess),
    /// Remove label(s): `n:Label1:Label2:$($dynLabel)`. Static names
    /// in the second slot, dynamic-expression spellings in the third.
    Label(String, Vec<String>, Vec<Expr>),
}

/// An UNWIND clause: expand a list into rows.
#[derive(Debug, Clone)]
pub struct UnwindClause {
    /// The list expression to unwind.
    pub expr: Expr,
    /// The alias for each element.
    pub alias: String,
}

/// A LOAD CSV clause: read CSV rows as list or map values.
#[derive(Debug, Clone)]
pub struct LoadCsvClause {
    /// Whether the CSV has a header row (WITH HEADERS).
    pub with_headers: bool,
    /// The expression evaluating to the file path / URL.
    pub url: Expr,
    /// The alias for each row variable.
    pub row_alias: String,
    /// Optional field terminator (default `,`).
    pub field_terminator: Option<char>,
}

/// A CALL clause: invoke a stored procedure.
#[derive(Debug, Clone)]
pub struct CallClause {
    /// Fully qualified procedure name (e.g. `test.my.proc`).
    pub procedure_name: String,
    /// Explicit arguments. `None` means implicit args from query parameters.
    pub args: Option<Vec<Expr>>,
    /// Optional YIELD items.
    pub yield_items: Option<YieldItems>,
}

/// `CALL [(imports)] { body }` - inline procedure subquery (ISO
/// 39075 section 9.4 / openCypher 5). The `body` is a complete
/// `SingleQuery`; the executor evaluates it once per input row.
#[derive(Debug, Clone)]
pub struct CallSubqueryClause {
    /// Explicit imports (`CALL (a, b) { ... }`). `None` = transparent scope, every outer binding
    /// visible. With `Some`, only the named variables reach the body; the others are hidden from it
    /// but kept on the output rows.
    pub imports: Option<Vec<String>>,
    /// Subquery body with its own MATCH / WHERE / WITH / RETURN; the final RETURN's columns
    /// cross-join back onto the outer record. A `RegularQuery`, so it may be a UNION (`CALL { ...
    /// UNION ALL ... }`).
    pub body: Box<RegularQuery>,
    /// `IN TRANSACTIONS [OF n ROWS] [ON ERROR ...] [REPORT STATUS AS v]` -
    /// when set, the body runs per input row with writes, batch-committed.
    pub in_transactions: Option<InTransactions>,
}

/// The `IN TRANSACTIONS` modifier on a `CALL { ... }` subquery.
#[derive(Debug, Clone)]
pub struct InTransactions {
    /// `OF <expr> ROWS` - rows per batch (default 1000).
    pub batch_size: Option<Expr>,
    /// `ON ERROR {CONTINUE | BREAK | FAIL}` (default FAIL).
    pub on_error: OnErrorBehavior,
    /// `REPORT STATUS AS var` - emit a per-batch status map.
    pub report_status: Option<String>,
}

/// Batch error-handling policy for `CALL { ... } IN TRANSACTIONS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnErrorBehavior {
    /// Skip the failing batch and continue.
    Continue,
    /// Stop processing further batches (keep prior writes).
    Break,
    /// Abort the whole statement (default).
    Fail,
}

/// YIELD items in a CALL clause.
#[derive(Debug, Clone)]
pub enum YieldItems {
    /// YIELD * - yield all output columns.
    All,
    /// YIELD col1, col2 AS alias, ...
    Explicit(Vec<YieldItem>),
}

/// A single YIELD item with optional alias.
#[derive(Debug, Clone)]
pub struct YieldItem {
    /// The output column name to yield.
    pub name: String,
    /// Optional alias for the yielded column.
    pub alias: Option<String>,
}

/// A RETURN clause with projection, ordering, skip, and limit.
#[derive(Debug, Clone)]
pub struct ReturnClause {
    /// If true, deduplicate result rows.
    pub distinct: bool,
    /// The projection items.
    pub items: ProjectionItems,
    /// Optional ORDER BY.
    pub order_by: Option<Vec<SortItem>>,
    /// Optional SKIP.
    pub skip: Option<Expr>,
    /// Optional LIMIT.
    pub limit: Option<Expr>,
}

/// A WITH clause (like RETURN, but pipes results to the next query part).
#[derive(Debug, Clone)]
pub struct WithClause {
    /// If true, deduplicate result rows.
    pub distinct: bool,
    /// The projection items.
    pub items: ProjectionItems,
    /// Optional ORDER BY.
    pub order_by: Option<Vec<SortItem>>,
    /// Optional SKIP.
    pub skip: Option<Expr>,
    /// Optional LIMIT.
    pub limit: Option<Expr>,
    /// Optional WHERE filter after projection.
    pub where_clause: Option<Expr>,
}

/// Projection items: either wildcard (*) or explicit expressions.
#[derive(Debug, Clone)]
pub enum ProjectionItems {
    /// RETURN * / WITH *
    All,
    /// Explicit list of expressions with optional aliases.
    Explicit(Vec<ProjectionItem>),
}

/// A single projection item: an expression with an optional alias.
#[derive(Debug, Clone)]
pub struct ProjectionItem {
    /// The expression to project.
    pub expr: Expr,
    /// Optional AS alias.
    pub alias: Option<String>,
    /// Original source text of the expression (for column naming).
    pub source_text: Option<String>,
}

/// An ORDER BY item with sort direction.
#[derive(Debug, Clone)]
pub struct SortItem {
    /// The expression to sort by.
    pub expr: Expr,
    /// True for ascending (default), false for descending.
    pub ascending: bool,
}

// -- Patterns ----------------------------------------------------------

/// A graph pattern consisting of one or more comma-separated pattern parts.
#[derive(Debug, Clone)]
pub struct Pattern {
    /// The pattern parts, separated by commas in the source.
    pub parts: Vec<PatternPart>,
}

/// Whether a pattern part is wrapped in `shortestPath()` or `allShortestPaths()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortestPathMode {
    /// Normal pattern (no shortest-path wrapper).
    None,
    /// `shortestPath(...)` - return one shortest path.
    Single,
    /// `allShortestPaths(...)` - return all shortest paths.
    All,
    /// `*wShortest` - single minimum-cost weighted path (Dijkstra).
    WeightedSingle,
    /// GQL section 14.6 `SHORTEST k`: top-k loopless shortest paths between two bound
    /// endpoints (Yen with BFS inside, unweighted), at most `k`, by increasing
    /// length. Unlike `All` (every path of the single minimum length) and unlike an
    /// injected `LIMIT k`, it fills from longer lengths when fewer than `k` paths
    /// share the minimum.
    Counted(usize),
    /// GQL section 14.6 `SHORTEST k GROUPS`: every loopless path whose length is among the
    /// `k` smallest distinct lengths (`k = 1` = `ALL SHORTEST`). DFS-enumerates
    /// loopless paths up to `max_depth`, groups by length, returns the first `k` groups.
    CountedGroups(usize),
}

/// ISO/IEC 39075 section 14.5 path-matching mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathMode {
    /// `TRAIL` (default in openCypher and ISO section 14.5) - each edge
    /// may appear at most once along the matched path.
    #[default]
    Trail,
    /// `WALK` (also `REPEATABLE ELEMENTS`) - no edge or node uniqueness
    /// constraint; a single edge may be traversed multiple times.
    Walk,
    /// `ACYCLIC` - each node may appear at most once along the path
    /// (which transitively implies edge uniqueness too).
    Acyclic,
}

/// A single pattern part, optionally named (e.g. `p = (a)-->(b)`).
#[derive(Debug, Clone)]
pub struct PatternPart {
    /// Optional path variable name.
    pub variable: Option<String>,
    /// The pattern element (nodes and relationships).
    pub element: PatternElement,
    /// Whether this pattern is wrapped in shortestPath/allShortestPaths.
    pub shortest_path: ShortestPathMode,
    /// Weight property for `*wShortest` (e.g. "cost" from `r.cost`).
    pub weight_property: Option<String>,
    /// Total cost variable for `*wShortest` (e.g. "total_cost").
    pub total_cost_var: Option<String>,
    /// ISO section 14.5 path mode (WALK / TRAIL / ACYCLIC). Default is TRAIL.
    pub path_mode: PathMode,
    /// ISO section 16.11 group-variable lists for quantified groups unrolled into this part's chain (GQL
    /// multi-step lowering): each entry binds `var` to the LIST of its per-iteration slots, in
    /// order. Empty for ordinary patterns.
    pub qpp_list_groups: Vec<(String, Vec<String>)>,
    /// ISO section 14.6 path selector: `Some(n)` requests at most `n` matched paths
    /// per starting binding (used by the GQL `ANY` / `ANY k` selectors).
    /// `None` means "unbounded" (the openCypher default).
    pub path_selector_limit: Option<usize>,
}

/// A pattern element: a starting node followed by zero or more relationship-node chains.
#[derive(Debug, Clone)]
pub struct PatternElement {
    /// The starting node pattern.
    pub node: NodePattern,
    /// Chains of relationship -> node.
    pub chain: Vec<PatternChain>,
}

/// A node pattern like `(n:Person {name: 'Alice'})`.
#[derive(Debug, Clone)]
pub struct NodePattern {
    /// Optional variable name.
    pub variable: Option<String>,
    /// Static label constraints (literal label names).
    pub labels: Vec<String>,
    /// Dynamic-label expressions from `:$paramname` or `:$(expr)`
    /// syntax. Resolved to label names at execute time, then applied
    /// as additional label filters alongside the static ones.
    pub dynamic_labels: Vec<Expr>,
    /// Optional property map expression.
    pub properties: Option<Expr>,
    /// Pattern-internal predicate: `(n:L {...} WHERE pred)` (Cypher 5+).
    /// Conjoined with the MATCH's own WHERE; may reference any variable in
    /// scope, including ones bound later in the same pattern.
    pub where_pred: Option<Box<Expr>>,
}

/// A relationship-to-node chain in a pattern.
#[derive(Debug, Clone)]
pub struct PatternChain {
    /// The relationship pattern.
    pub rel: RelPattern,
    /// The target node pattern.
    pub node: NodePattern,
}

/// A relationship pattern like `-[r:KNOWS*1..3]->`
#[derive(Debug, Clone)]
pub struct RelPattern {
    /// Optional variable name.
    pub variable: Option<String>,
    /// Direction of the relationship.
    pub direction: RelDirection,
    /// Static relationship type constraints (e.g. `:KNOWS|:LIKES`).
    pub types: Vec<String>,
    /// Dynamic-type expressions from `:$paramname` or `:$(expr)`
    /// syntax. Resolved at execute time and applied as an additional
    /// edge-type filter alongside the static ones.
    pub dynamic_types: Vec<Expr>,
    /// Optional variable-length range (min, max). `None` means fixed length 1.
    pub range: Option<(Option<u32>, Option<u32>)>,
    /// Optional property map expression.
    pub properties: Option<Expr>,
    /// Weight property for `*wShortest` (e.g. "cost").
    pub wshortest_weight: Option<String>,
    /// Total cost variable for `*wShortest` (e.g. "total_cost").
    pub wshortest_cost_var: Option<String>,
    /// Pattern-internal predicate `[r:T {...} WHERE pred]` (Cypher 5+): a plain conjunct for
    /// fixed-length relationships, PER ELEMENT for var-length / quantified ones (`all(r IN r WHERE
    /// pred)`, Neo4j's QPP semantics).
    pub where_pred: Option<Box<Expr>>,
    /// Inner node patterns when this var-length relationship came from a quantified group
    /// `((x)-[r]->(y)){m,n}`. Per ISO section 16.11 / Neo4j, `x`/`y` are **group variables** binding
    /// per-iteration node LISTS, and inner label/property constraints hold on every iteration. `r`
    /// already binds the per-iteration edge list.
    pub qpp_group: Option<Box<QppGroupInner>>,
}

/// The inner node patterns of a quantified path-pattern group - see
/// [`RelPattern::qpp_group`].
#[derive(Debug, Clone)]
pub struct QppGroupInner {
    /// The group's inner start node (`x` in `((x)-[r]->(y)){m,n}`).
    pub start: NodePattern,
    /// The group's inner end node (`y`).
    pub end: NodePattern,
}

/// Direction of a relationship in a pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelDirection {
    /// `-->` outgoing.
    Outgoing,
    /// `<--` incoming.
    Incoming,
    /// `--` undirected (match both).
    Both,
}

// -- Expressions -------------------------------------------------------

/// An expression in the Cypher language.
#[derive(Debug, Clone)]
pub enum Expr {
    /// A literal value.
    Literal(Literal),
    /// A variable reference.
    Variable(String),
    /// A parameter reference ($param).
    Parameter(String),
    /// Property access: `expr.key`
    Property(Box<Expr>, String),
    /// Binary operation: `expr op expr`
    BinaryOp(Box<Expr>, BinOp, Box<Expr>),
    /// Unary operation: `op expr`
    UnaryOp(UnaryOp, Box<Expr>),
    /// Comparison chain: `a < b <= c`
    Comparison(Box<Expr>, Vec<(CmpOp, Expr)>),
    /// Function call: `name(args)`
    FunctionCall {
        /// Function name (lowercased).
        name: String,
        /// Whether DISTINCT was specified.
        distinct: bool,
        /// Function arguments.
        args: Vec<Expr>,
    },
    /// List literal: `[1, 2, 3]`
    ListLiteral(Vec<Expr>),
    /// Map literal: `{key: value}`
    MapLiteral(Vec<(String, Expr)>),
    /// CASE expression.
    Case {
        /// Optional operand for simple CASE.
        operand: Option<Box<Expr>>,
        /// WHEN ... THEN ... pairs.
        whens: Vec<(Expr, Expr)>,
        /// Optional ELSE expression.
        else_expr: Option<Box<Expr>>,
    },
    /// `expr IN expr`
    In(Box<Expr>, Box<Expr>),
    /// `expr IS NULL`
    IsNull(Box<Expr>),
    /// `expr IS NOT NULL`
    IsNotNull(Box<Expr>),
    /// Type predicate: `expr IS [NOT] :: <TYPE> [NOT NULL]` /
    /// `expr IS [NOT] TYPED <TYPE>`. `type_name` is the normalised type
    /// spelling (e.g. `INTEGER`, `LOCAL DATETIME`, `LIST<INTEGER>`).
    IsTyped {
        /// The value being tested.
        expr: Box<Expr>,
        /// The declared type name.
        type_name: String,
        /// `true` for the `IS NOT ::` / `IS NOT TYPED` form.
        negated: bool,
        /// `true` if the type carries a `NOT NULL` suffix (null then fails).
        not_null: bool,
    },
    /// String predicate: STARTS WITH, ENDS WITH, CONTAINS.
    StringOp(Box<Expr>, StringOp, Box<Expr>),
    /// List indexing: `list[0]`
    ListIndex(Box<Expr>, Box<Expr>),
    /// List slicing: `list[1..3]`
    ListSlice(Box<Expr>, Option<Box<Expr>>, Option<Box<Expr>>),
    /// List comprehension: `[x IN list WHERE cond | map_expr]`
    ListComprehension {
        /// The iteration variable.
        variable: String,
        /// The list expression to iterate over.
        list_expr: Box<Expr>,
        /// Optional filter predicate (WHERE).
        filter: Option<Box<Expr>>,
        /// Optional mapping expression (| expr).
        map_expr: Option<Box<Expr>>,
    },
    /// Existential subquery: EXISTS { MATCH ... }
    ExistentialSubquery(Box<MatchClause>),
    /// Full existential subquery with WITH/RETURN: EXISTS { MATCH ... WITH ... RETURN ... }
    FullExistentialSubquery(Box<SingleQuery>),
    /// Pattern predicate: `(n)-[:REL]->()` used as boolean in WHERE.
    /// Desugared from expression position. All variables must already be bound.
    PatternPredicate(Box<MatchClause>),
    /// COUNT { MATCH ... }
    CountSubquery(Box<MatchClause>),
    /// `COLLECT { ... }`: `LIST` of the body's single-column RETURN values. The body is a full
    /// SingleQuery whose RETURN projects exactly one column; outer variables are visible as in
    /// EXISTS / COUNT.
    CollectSubquery(Box<SingleQuery>),
    /// Quantifier expression: ALL/ANY/NONE/SINGLE(x IN list WHERE predicate)
    QuantifierExpr {
        /// The quantifier kind.
        kind: QuantifierKind,
        /// The iteration variable.
        variable: String,
        /// The list expression to iterate over.
        list_expr: Box<Expr>,
        /// The predicate to evaluate for each element.
        predicate: Box<Expr>,
    },
    /// Pattern comprehension: `[p = (n)-->() | p]` or `[(n)-->() | expr]`
    PatternComprehension {
        /// Optional path variable name.
        variable: Option<String>,
        /// The match clause (pattern + optional WHERE).
        match_clause: Box<MatchClause>,
        /// The projection expression (after `|`).
        projection: Box<Expr>,
    },
    /// `reduce(accumulator = initial, variable IN list | expression)`
    Reduce {
        /// The accumulator variable name.
        accumulator: String,
        /// The initial value expression for the accumulator.
        initial: Box<Expr>,
        /// The iteration variable name.
        variable: String,
        /// The list expression to iterate over.
        list_expr: Box<Expr>,
        /// The update expression evaluated per element.
        update_expr: Box<Expr>,
    },
    /// Map projection: `expr {.prop1, .prop2, var, key: expr, .*}`
    MapProjection {
        /// The base expression (typically a variable).
        expr: Box<Expr>,
        /// The property names to project.
        properties: Vec<String>,
        /// `true` if the projection includes the `.*` all-properties selector.
        all_properties: bool,
        /// Literal entries: `key: <expr>` (computed values) and bare
        /// variable selectors (`x` == `x: x`), evaluated per row. Applied
        /// LAST, so they override `.*` / `.prop` entries of the same key.
        literal_entries: Vec<(String, Expr)>,
    },
}

/// Quantifier kinds for list predicate expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantifierKind {
    /// ALL(x IN list WHERE pred) - true if predicate holds for all elements.
    All,
    /// ANY(x IN list WHERE pred) - true if predicate holds for at least one element.
    Any,
    /// NONE(x IN list WHERE pred) - true if predicate holds for no elements.
    None,
    /// SINGLE(x IN list WHERE pred) - true if predicate holds for exactly one element.
    Single,
}

/// A literal value.
#[derive(Debug, Clone)]
pub enum Literal {
    /// Null.
    Null,
    /// Boolean.
    Bool(bool),
    /// Integer.
    Integer(i64),
    /// Float.
    Float(f64),
    /// String.
    String(String),
}

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// Addition.
    Add,
    /// Subtraction.
    Sub,
    /// Multiplication.
    Mul,
    /// Division.
    Div,
    /// Modulo.
    Mod,
    /// Exponentiation.
    Pow,
    /// ISO/IEC 39075 section 20.23 concatenation (`||`) of two strings or two lists; no cross-type
    /// coercion. Unlike `Add`, Cypher's lenient `+` that also concatenates / appends.
    Concat,
    /// Logical AND.
    And,
    /// Logical OR.
    Or,
    /// Logical XOR.
    Xor,
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Logical NOT.
    Not,
    /// Numeric negation.
    Neg,
}

/// Comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `=`
    Eq,
    /// `<>`
    Neq,
    /// `<`
    Lt,
    /// `<=`
    Lte,
    /// `>`
    Gt,
    /// `>=`
    Gte,
}

/// String predicate operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringOp {
    /// STARTS WITH
    StartsWith,
    /// ENDS WITH
    EndsWith,
    /// CONTAINS
    Contains,
    /// =~ (regex match)
    Regex,
}

/// A property access like `n.name`.
#[derive(Debug, Clone)]
pub struct PropertyAccess {
    /// The variable or expression being accessed.
    pub expr_or_var: String,
    /// The property key.
    pub key: String,
}
