//! GQL abstract syntax tree.
//!
//! Mirrors the ISO/IEC 39075 productions: section 9 (query statements), section 11
//! (expressions), section 14 (path patterns). The tree stays GQL-shaped; `lower.rs`
//! translates it to the executor's representation.
//!
//! Every node carries a [`Span`] (byte offsets into the source), so
//! diagnostics, IDE features and Studio highlighting share one coordinate
//! system.
//!
//! Public enums are `#[non_exhaustive]`, so adding variants doesn't break
//! downstream code.

use crate::lexer::Span;

/// A complete GQL program: a sequence of top-level [`Statement`]s.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Program {
    /// Statements in source order.
    pub statements: Vec<Statement>,
    /// One entry per separator between adjacent statements (`separators.len() ==
    /// statements.len().saturating_sub(1)`). `Semicolon`: the next statement runs independently.
    /// `Next { yields }`: ISO section 9.3 linear composition, the prior statement's rows pipe into the
    /// next; `None` = YIELD * (every column), `Some(list)` keeps only those names.
    pub separators: Vec<StatementSeparator>,
    /// Source span covering the whole program. `0..0` for empty input.
    pub span: Span,
}

/// How adjacent statements in a [`Program`] relate to each other.
#[derive(Debug, Clone, PartialEq)]
pub enum StatementSeparator {
    /// `;`: ends a statement; the next runs in a fresh scope, no row piping.
    Semicolon,
    /// `NEXT [YIELD a, b, ...]`: pipes the prior statement's rows into the next. `yields = None` =
    /// transparent / `YIELD *`; `Some(list)` keeps the listed names (optionally aliased). The
    /// lowerer fuses the chain into nested `CALL` subqueries.
    Next {
        /// Explicit yield list; `None` = implicit YIELD *.
        yields: Option<Vec<NextYieldItem>>,
    },
}

/// One name in a `NEXT YIELD a [AS b]` list.
#[derive(Debug, Clone, PartialEq)]
pub struct NextYieldItem {
    /// Source name (from the prior statement's output).
    pub name: String,
    /// Optional rename; the next statement sees the value under the alias.
    pub alias: Option<String>,
}

impl Program {
    /// Build an empty program (no statements, zero-width span).
    pub fn empty() -> Self {
        Self::default()
    }
}

/// One top-level GQL statement.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Statement {
    /// A query: a sequence of clauses ending in `RETURN` or `FINISH`.
    Query(QueryStatement),
    /// `CREATE GRAPH [IF NOT EXISTS] path [graph-type-body]` (section 7). Both the Cypher-style bare form
    /// `CREATE GRAPH [IF NOT EXISTS] name` (`body = None`) and the GQL form `CREATE GRAPH path
    /// body`, body being `ANY`, a graph-type reference, an inline `{...}` spec, `LIKE other` or
    /// `<type> AS COPY OF other`.
    CreateGraph {
        /// Graph name, possibly multi-segment (`/foo/mygraph` or `mygraph`).
        path: SchemaPath,
        /// `true` if the source said `IF NOT EXISTS`.
        if_not_exists: bool,
        /// Graph-type body; `None` for the bare Cypher-style form (same as `... ANY`).
        body: Option<GraphTypeBody>,
        /// Source span.
        span: Span,
    },
    /// `DROP GRAPH [IF EXISTS] path`.
    DropGraph {
        /// Graph path.
        path: SchemaPath,
        /// `true` if the source said `IF EXISTS`.
        if_exists: bool,
        /// Source span.
        span: Span,
    },
    /// `CREATE GRAPH TYPE path { graph-type-body }` (section 7).
    CreateGraphType {
        /// Graph-type path.
        path: SchemaPath,
        /// Body specification.
        body: GraphTypeBody,
        /// Source span.
        span: Span,
    },
    /// `DROP GRAPH TYPE [IF EXISTS] path`.
    DropGraphType {
        /// Graph-type path.
        path: SchemaPath,
        /// `true` if the source said `IF EXISTS`.
        if_exists: bool,
        /// Source span.
        span: Span,
    },
    /// `CREATE SCHEMA [IF NOT EXISTS] path` - catalog directory (section 7).
    CreateSchema {
        /// Schema path (e.g. `/foo/myschema`).
        path: SchemaPath,
        /// `true` if the source said `IF NOT EXISTS`.
        if_not_exists: bool,
        /// Source span.
        span: Span,
    },
    /// `DROP SCHEMA [IF EXISTS] path`.
    DropSchema {
        /// Schema path.
        path: SchemaPath,
        /// `true` if the source said `IF EXISTS`.
        if_exists: bool,
        /// Source span.
        span: Span,
    },
    /// `CREATE INDEX [name] FOR (var:Label) ON (var.property)` (Cypher-compatible form, accepted in
    /// either dialect).
    CreateIndex {
        /// Optional index name.
        name: Option<String>,
        /// Label the index attaches to.
        label: String,
        /// Property key indexed.
        property: String,
        /// Source span.
        span: Span,
    },
    /// `DROP INDEX name`.
    DropIndex {
        /// Index name.
        name: String,
        /// Source span.
        span: Span,
    },
    /// `CREATE CONSTRAINT [name] FOR (var:Label) REQUIRE var.prop IS
    /// UNIQUE | IS NOT NULL`.
    CreateConstraint {
        /// Optional constraint name.
        name: Option<String>,
        /// Label the constraint attaches to.
        label: String,
        /// Property key constrained.
        property: String,
        /// What the constraint enforces.
        kind: ConstraintKind,
        /// Source span.
        span: Span,
    },
    /// `DROP CONSTRAINT name`.
    DropConstraint {
        /// Constraint name.
        name: String,
        /// Source span.
        span: Span,
    },
    /// `USE graphName`: sets the current graph (section 7.2). The reference can be:
    /// * a simple identifier (`USE social`);
    /// * a multi-segment path (`USE myComposite.myGraph`);
    /// * a graph function call (`USE graph.byName('social')`,
    ///   `USE graph.byElementId(...)`).
    Use {
        /// Primary graph identifier: the bare name, or the first segment of a composite reference,
        /// or the function name of the dynamic form. Callers that don't care about composite refs
        /// read just this.
        graph: String,
        /// Reference shape - `Simple`, `Composite`, or `Dynamic`.
        reference: UseReference,
        /// Source span.
        span: Span,
    },
    /// `START TRANSACTION [READ ONLY|READ WRITE] body COMMIT`: an explicit transaction block around
    /// inner statements (section 9.4). A thin wrapper, so the executor can refuse DDL inside `READ ONLY`
    /// transactions (TCK Create1, scenario 8).
    Transaction {
        /// `true` for `READ ONLY`; `false` for `READ WRITE` or no modifier (ISO 39075 default is
        /// read-write).
        read_only: bool,
        /// Statements between START TRANSACTION and COMMIT / ROLLBACK.
        statements: Vec<Statement>,
        /// `true` when the block ends in `ROLLBACK`: its changes are to be discarded.
        rollback: bool,
        /// Source span.
        span: Span,
    },
    /// ISO 39075 section 15.4 conditional statement, `WHEN cond THEN stmt [ELSE stmt] [END WHEN]`. The
    /// condition is evaluated in an empty binding scope (parameters, `SESSION_USER`, literals) and
    /// the matching branch runs as a normal statement.
    When {
        /// Boolean condition.
        condition: Expression,
        /// `THEN` branch, run when the condition is true.
        then_stmt: Box<Statement>,
        /// Optional `ELSE` branch, run when it's false; without it a false condition emits no rows.
        else_stmt: Option<Box<Statement>>,
        /// Source span.
        span: Span,
    },
}

impl Statement {
    /// Source span of any statement variant.
    pub fn span(&self) -> Span {
        match self {
            Statement::Query(q) => q.span,
            Statement::CreateGraph { span, .. }
            | Statement::DropGraph { span, .. }
            | Statement::CreateGraphType { span, .. }
            | Statement::DropGraphType { span, .. }
            | Statement::CreateSchema { span, .. }
            | Statement::DropSchema { span, .. }
            | Statement::CreateIndex { span, .. }
            | Statement::DropIndex { span, .. }
            | Statement::CreateConstraint { span, .. }
            | Statement::DropConstraint { span, .. }
            | Statement::Use { span, .. }
            | Statement::Transaction { span, .. }
            | Statement::When { span, .. } => *span,
        }
    }
}

/// Catalog path: zero or more `/`-separated name segments, plus whether there's
/// a leading `/` (absolute) or not (relative). Identifies a schema, graph or
/// graph type.
///
/// * `foo` -> `{ absolute: false, segments: ["foo"] }`
/// * `/foo` -> `{ absolute: true, segments: ["foo"] }`
/// * `/foo/bar` -> `{ absolute: true, segments: ["foo", "bar"] }`
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaPath {
    /// `true` if the source had a leading `/`.
    pub absolute: bool,
    /// Path segments in source order.
    pub segments: Vec<String>,
    /// Source span (leading `/` if any, all segments and the `/`s between them).
    pub span: Span,
}

/// The graph-type body: what follows the path in `CREATE GRAPH path ...` (section 7).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum GraphTypeBody {
    /// `ANY`: open graph type, no structural constraint.
    Open {
        /// Source span (covers the `ANY` keyword).
        span: Span,
    },
    /// Bare graph-type name, referring to an earlier `CREATE GRAPH TYPE`.
    Reference {
        /// Referenced type name.
        name: SchemaPath,
        /// Source span.
        span: Span,
    },
    /// Inline `{ ... }` graph-type spec, parsed best-effort: when the body follows ISO 39075 section 17.2's
    /// element-type grammar (NODE/VERTEX/EDGE/RELATIONSHIP declarations with optional labels and
    /// property-type lists) `elements` is filled; otherwise it's `None` and the body is kept opaque
    /// (round-tripped via the span).
    Inline {
        /// Source span of the body content (between `{` and `}`).
        body_span: Span,
        /// Source span of the whole spec including the braces.
        span: Span,
        /// Structured element list; `None` when the body isn't a canonical element list (kept
        /// opaque). `Some(vec![])` is an empty body (`{}`), a valid open graph type per section 17.2.
        elements: Option<Vec<GraphTypeElement>>,
    },
    /// `LIKE other-graph`: copy another graph's type.
    Like {
        /// Graph whose type is copied.
        source: SchemaPath,
        /// Source span.
        span: Span,
    },
    /// `<type-part> AS COPY OF other-graph`: type spec plus data copy. `type_part` is `None` for
    /// the bare `AS COPY OF other` (copies structure and data).
    Copy {
        /// Optional leading type specification.
        type_part: Option<Box<GraphTypeBody>>,
        /// Graph being copied.
        source: SchemaPath,
        /// Source span.
        span: Span,
    },
}

impl GraphTypeBody {
    /// Source span of any body variant.
    pub fn span(&self) -> Span {
        match self {
            GraphTypeBody::Open { span }
            | GraphTypeBody::Reference { span, .. }
            | GraphTypeBody::Inline { span, .. }
            | GraphTypeBody::Like { span, .. }
            | GraphTypeBody::Copy { span, .. } => *span,
        }
    }
}

/// One element-type declaration in a `CREATE GRAPH TYPE { ... }` body: a node (vertex) type or an
/// edge (relationship) type.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphTypeElement {
    /// `NODE` / `VERTEX` element type (ISO 39075 section 17.2).
    Node {
        /// Optional element-type name (the `T` in `NODE T (...)`).
        name: Option<String>,
        /// Label set declared on this element.
        labels: Vec<String>,
        /// Property declarations (`{ propName: TYPE, ... }`).
        properties: Vec<PropertyDecl>,
        /// `true` for the parenthesized node-type-pattern form (`(p:Person {...})`), where the label
        /// expression IS the declared label set, so label-set cardinality limits apply. `false` for
        /// the keyword form (`NODE T`, `ANY NODE`), where no label clause means "unconstrained",
        /// not "empty set".
        pattern_form: bool,
        /// Source span.
        span: Span,
    },
    /// `EDGE` / `RELATIONSHIP` element type.
    Edge {
        /// `Edge` for `EDGE`, `Relationship` for `RELATIONSHIP` (synonyms).
        kind: EdgeKind,
        /// Optional element-type name.
        name: Option<String>,
        /// Label set.
        labels: Vec<String>,
        /// Property declarations.
        properties: Vec<PropertyDecl>,
        /// Source span.
        span: Span,
    },
}

impl GraphTypeElement {
    /// Source span of the element declaration.
    pub fn span(&self) -> Span {
        match self {
            GraphTypeElement::Node { span, .. } | GraphTypeElement::Edge { span, .. } => *span,
        }
    }
}

/// Minimum label-set cardinality of a node type. ISO 39075 leaves both bounds
/// implementation-defined; the TCK derives its `randomLabelSet` sizes from them.
pub const MIN_NODE_LABELS: usize = 1;
/// Maximum label-set cardinality of a node type.
pub const MAX_NODE_LABELS: usize = 64;

/// Structural violation in a graph-type element list. Each variant maps to its own ISO/IEC 39075
/// condition code, so hosts can surface the right GQLSTATUS.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphTypeViolation {
    /// Two property declarations in one element share a name: 42000 (syntax error or access rule
    /// violation).
    DuplicateProperty {
        /// The repeated property name.
        property: String,
    },
    /// A pattern-form node type with fewer than [`MIN_NODE_LABELS`] labels: 22G0N (number of node
    /// labels below supported minimum).
    TooFewLabels {
        /// Number of labels the node type declared.
        count: usize,
    },
    /// A node type with more than [`MAX_NODE_LABELS`] labels: 22G0P (number of node labels exceeds
    /// supported maximum).
    TooManyLabels {
        /// Number of labels the node type declared.
        count: usize,
    },
}

/// Validates the element list of a `CREATE GRAPH TYPE { ... }` / `CREATE GRAPH { ... }` body against
/// ISO 39075 section 17.2 structure plus the label-set limits above.
pub fn validate_graph_type_elements(
    elements: &[GraphTypeElement],
) -> Result<(), GraphTypeViolation> {
    for el in elements {
        let (labels, properties, pattern_form) = match el {
            GraphTypeElement::Node {
                labels,
                properties,
                pattern_form,
                ..
            } => (labels, properties, *pattern_form),
            GraphTypeElement::Edge {
                labels, properties, ..
            } => (labels, properties, false),
        };
        let mut seen = std::collections::BTreeSet::new();
        for p in properties {
            if !seen.insert(p.name.as_str()) {
                return Err(GraphTypeViolation::DuplicateProperty {
                    property: p.name.clone(),
                });
            }
        }
        // the minimum only binds the pattern form (see `pattern_form`), the maximum every form
        if pattern_form && labels.len() < MIN_NODE_LABELS {
            return Err(GraphTypeViolation::TooFewLabels {
                count: labels.len(),
            });
        }
        if labels.len() > MAX_NODE_LABELS {
            return Err(GraphTypeViolation::TooManyLabels {
                count: labels.len(),
            });
        }
    }
    Ok(())
}

/// ISO/IEC 39075 section 7.2 USE reference shape.
#[derive(Debug, Clone, PartialEq)]
pub enum UseReference {
    /// `USE name` - single-segment identifier.
    Simple,
    /// `USE comp.graph.subgraph`: dotted multi-segment path, all segments in order (the first also
    /// in the outer `graph` field).
    Composite {
        /// Dotted path segments in source order.
        segments: Vec<String>,
    },
    /// `USE func(arg1, arg2, ...)`: dynamic graph reference. The function name is also in the outer
    /// `graph` field; the arguments are kept as raw source spans.
    Dynamic {
        /// Possibly-dotted function name (e.g. `graph.byName`).
        function: Vec<String>,
        /// Argument source spans, in order; kept raw rather than lowered to Cypher expressions.
        arg_spans: Vec<Span>,
    },
}

/// `EDGE` vs `RELATIONSHIP` spelling: synonyms in ISO 39075, the keyword kept for round-tripping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// `EDGE` keyword.
    Edge,
    /// `RELATIONSHIP` keyword.
    Relationship,
}

/// One property declaration inside an element-type body
/// (`propName :: TYPE` or `propName TYPE [NOT NULL]`).
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyDecl {
    /// Property key name.
    pub name: String,
    /// The declared value type.
    pub value_type: ValueTypeName,
    /// `true` when the declaration says `NOT NULL`.
    pub not_null: bool,
    /// Source span.
    pub span: Span,
}

/// A predefined value-type name per ISO 39075 section 4.16.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueTypeName {
    /// `BOOLEAN` or `BOOL`.
    Boolean,
    /// `INTEGER`, `SIGNED INTEGER`, or `INT`.
    Integer,
    /// `FLOAT`.
    Float,
    /// `STRING` or `VARCHAR`.
    String,
    /// `DATE`.
    Date,
    /// `LOCAL TIME` / `LOCAL_TIME`.
    LocalTime,
    /// `LOCAL DATETIME` / `LOCAL_DATETIME`.
    LocalDatetime,
    /// `ZONED TIME` / `TIME` / `ZONED_TIME`.
    ZonedTime,
    /// `ZONED DATETIME` / `DATETIME` / `ZONED_DATETIME`.
    ZonedDatetime,
    /// `DURATION`.
    Duration,
    /// `ANY` - any value.
    Any,
    /// Any other type spelling, kept verbatim so the body round-trips.
    Other(String),
    /// `T1 | T2 | ...`: dynamic union of predefined value types (GV66 open / GV67 closed unions,
    /// value-declaration form per ISO section 16.10). The parser flattens nested unions, so these are
    /// always leaf types. Never empty.
    Union(Vec<ValueTypeName>),
}

/// What a [`Statement::CreateConstraint`] enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKind {
    /// `IS UNIQUE` - no two rows may share the same value.
    Unique,
    /// `IS NOT NULL` - the property is required.
    NotNull,
}

/// A query statement: one [`QueryBody`] optionally chained with one
/// or more [`UnionPart`]s via `UNION [ALL | DISTINCT]` (ISO/IEC 39075
/// section 9.6).
#[derive(Debug, Clone, PartialEq)]
pub struct QueryStatement {
    /// The leading query body.
    pub head: QueryBody,
    /// Subsequent `UNION` parts (empty for a single-body query).
    pub unions: Vec<UnionPart>,
    /// Source span (covers head through the last UNION).
    pub span: Span,
}

/// A single linear sequence of clauses ending in `RETURN`.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryBody {
    /// Clauses in source order. A well-formed body ends with a
    /// `RETURN` clause.
    pub clauses: Vec<Clause>,
    /// Source span.
    pub span: Span,
}

/// One `UNION [ALL | DISTINCT] queryBody` continuation.
#[derive(Debug, Clone, PartialEq)]
pub struct UnionPart {
    /// Set semantics: `Distinct` (the default - deduplicate rows) or
    /// `All` (concatenate).
    pub kind: UnionKind,
    /// The right-hand query body.
    pub body: QueryBody,
    /// Source span (covers `UNION` keyword through the body).
    pub span: Span,
}

/// Set semantics for a `UNION` continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnionKind {
    /// `UNION` or `UNION DISTINCT` - deduplicate.
    Distinct,
    /// `UNION ALL` - concatenate (keep duplicates).
    All,
}

/// A clause inside a query.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Clause {
    /// `MATCH ...` or `OPTIONAL MATCH ...`, optionally followed by
    /// `WHERE ...`.
    Match(MatchClause),
    /// `WITH ...` intermediate projection. Same shape as `RETURN` plus
    /// an optional `WHERE` predicate that filters the projected rows.
    With(WithClause),
    /// `UNWIND expr AS alias` - explode a list-valued expression into
    /// one row per element.
    Unwind(UnwindClause),
    /// `FOR x IN list` - iteration form (section 9.9) used in comprehensions.
    /// Semantically equivalent to `UNWIND list AS x`; kept as a
    /// distinct clause to preserve the spec's surface syntax through
    /// pretty-printing and IDE features.
    For(ForClause),
    /// `FILTER [WHERE] predicate` - standalone post-projection filter
    /// (section 9.10). Both `FILTER WHERE expr` and the elided `FILTER expr`
    /// are accepted; the AST records only the predicate.
    Filter(FilterClause),
    /// `LET name = expression [, name = expression ...]` - bind one or
    /// more expressions to names (ISO/IEC 39075 section 9.3, optional
    /// feature GQ09). Semantically `WITH *, expr AS name, ...` -
    /// pass-through every prior binding plus the new ones.
    Let(LetClause),
    /// `INSERT pattern [, pattern ...]` - write new nodes/edges (section 8).
    Insert(InsertClause),
    /// `SET item [, item ...]` - mutate properties or labels (section 8.3).
    Set(SetClause),
    /// `REMOVE item [, item ...]` - drop properties or labels (section 8.4).
    Remove(RemoveClause),
    /// `[DETACH] DELETE target [, target ...]` - delete nodes/edges
    /// (section 8.5). `detach` removes the node's incident edges first.
    Delete(DeleteClause),
    /// `FINISH` - terminator for a write-only query that produces no
    /// rows (section 9.5). Carries only a span.
    Finish(Span),
    /// `CALL proc.name(args) [YIELD ...]` - procedure invocation.
    Call(CallClause),
    /// `CALL [(import1, import2, ...)] { ... }` - inline procedure
    /// subquery (ISO/IEC 39075 section G.1, optional features GP01 / GP03).
    /// The brace-delimited body is a self-contained query; the
    /// optional parenthesised import list is the explicit
    /// variable-scope form (GP03) - only the named outer variables
    /// are visible inside the subquery body.
    CallSubquery(CallSubqueryClause),
    /// `RETURN ...` projection.
    Return(ReturnClause),
}

/// `[OPTIONAL] MATCH [<path_mode>] [<path_selector>] pattern[, pattern ...] [WHERE expr]`
/// (section 14, section 9.4 + ISO/IEC 39075 path-search prefixes).
#[derive(Debug, Clone, PartialEq)]
pub struct MatchClause {
    /// `true` if the source said `OPTIONAL MATCH`.
    pub optional: bool,
    /// Path mode (revisitation semantics) - `Trail` is the default
    /// when none is given. The full grammar lives in section 14.5.
    pub path_mode: PathMode,
    /// Path selector - `All` is the default (every matching path).
    /// The shortest-path / counted forms restrict which paths are
    /// returned. section 14.6.
    pub path_selector: PathSelector,
    /// Comma-separated path patterns. At least one is required.
    pub patterns: Vec<PathPattern>,
    /// Optional `WHERE expr` predicate filtering the match.
    pub where_clause: Option<Expression>,
    /// Source span (covers the leading keyword through the end of
    /// the WHERE clause if present).
    pub span: Span,
}

/// Path-revisitation mode (ISO/IEC 39075 section 14.5). Controls whether the
/// matching machinery may revisit nodes / edges within a single path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathMode {
    /// No explicit mode - defaults to `Trail` per ISO.
    Default,
    /// `WALK` - no restriction; nodes and edges may repeat.
    Walk,
    /// `TRAIL` - no edge may repeat (the default).
    Trail,
    /// `ACYCLIC` - no node may repeat.
    Acyclic,
}

/// Path selector (ISO/IEC 39075 section 14.6). Filters which matching paths
/// the clause yields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSelector {
    /// No explicit selector - yields every matching path.
    All,
    /// `ANY` - one arbitrary matching path.
    Any,
    /// `ANY k` - `k` arbitrary matching paths.
    AnyN(u32),
    /// `SHORTEST` - one shortest path (length = fewest edges).
    Shortest,
    /// `ANY SHORTEST` - same as `SHORTEST`; ISO grammar accepts
    /// either spelling.
    AnyShortest,
    /// `ALL SHORTEST` - every path of minimum length.
    AllShortest,
    /// `SHORTEST k` - the `k` shortest paths.
    ShortestN(u32),
    /// `SHORTEST k GROUPS` - paths in the `k` shortest length
    /// groups (i.e. all paths whose length is one of the first `k`
    /// distinct lengths).
    ShortestGroups(u32),
}

/// `WITH [DISTINCT] item, item ... [WHERE expr] [ORDER BY ...] [SKIP n]
/// [LIMIT n]` - an intermediate projection step (section 9.7).
#[derive(Debug, Clone, PartialEq)]
pub struct WithClause {
    /// `true` if the source said `WITH DISTINCT ...`.
    pub distinct: bool,
    /// The projection items.
    pub items: Vec<ReturnItem>,
    /// Optional post-projection `WHERE` predicate.
    pub where_clause: Option<Expression>,
    /// `ORDER BY` sort keys (empty when absent).
    pub order_by: Vec<OrderItem>,
    /// `SKIP expr` offset (None when absent).
    pub skip: Option<Expression>,
    /// `LIMIT expr` row cap (None when absent).
    pub limit: Option<Expression>,
    /// Source span.
    pub span: Span,
}

/// `UNWIND expr AS alias` - explode a list-valued expression to one
/// row per element, binding each to `alias` (section 9.8).
#[derive(Debug, Clone, PartialEq)]
pub struct UnwindClause {
    /// The list-valued expression being exploded.
    pub expr: Expression,
    /// The variable name bound to each element.
    pub alias: String,
    /// Source span.
    pub span: Span,
}

/// `FOR x IN list` - bind each element of `list` to the loop variable
/// in turn. Lowers to `UNWIND list AS x` on the Cypher engine; the
/// distinct clause type lets us round-trip the GQL surface syntax.
#[derive(Debug, Clone, PartialEq)]
pub struct ForClause {
    /// Loop variable name.
    pub variable: String,
    /// List-valued expression being iterated.
    pub list: Expression,
    /// Source span.
    pub span: Span,
}

/// `FILTER [WHERE] predicate` - keep rows for which `predicate`
/// evaluates to true. Both surface forms are normalised to the same
/// AST node.
#[derive(Debug, Clone, PartialEq)]
pub struct FilterClause {
    /// The filter predicate.
    pub predicate: Expression,
    /// Source span.
    pub span: Span,
}

/// `LET name = expression [, name = expression ...]` - comma-separated
/// bindings. Each binding adds a name to the current row's scope.
#[derive(Debug, Clone, PartialEq)]
pub struct LetClause {
    /// Bindings in source order. Each is `(name, expression)`.
    pub bindings: Vec<(String, Expression)>,
    /// Source span.
    pub span: Span,
}

/// `INSERT pattern [, pattern ...]` - creates the listed nodes/edges
/// (section 8.2). Patterns share their grammar with `MATCH`.
#[derive(Debug, Clone, PartialEq)]
pub struct InsertClause {
    /// Path patterns to create.
    pub patterns: Vec<PathPattern>,
    /// Source span.
    pub span: Span,
}

/// `SET item [, item ...]` - mutate one or more properties / labels.
#[derive(Debug, Clone, PartialEq)]
pub struct SetClause {
    /// Set items in source order.
    pub items: Vec<SetItem>,
    /// Source span.
    pub span: Span,
}

/// One item in a `SET` clause.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SetItem {
    /// `target = value` where `target` is a property access (`n.prop`)
    /// or a bare variable. Replaces the existing value.
    Property {
        /// Left-hand side - typically a [`Expression::Property`] or
        /// [`Expression::Variable`].
        target: Expression,
        /// Right-hand side.
        value: Expression,
        /// Source span.
        span: Span,
    },
    /// `target += map` - merges `map`'s entries into `target`,
    /// keeping existing entries that aren't overwritten.
    Merge {
        /// Variable whose property map is being updated.
        target: Expression,
        /// Map of additions / overrides (`Expression::List`, `Expression::FunctionCall { name:
        /// "map", ... }`, ...); lowering validates the shape.
        value: Expression,
        /// Source span.
        span: Span,
    },
    /// `target:Label[:Label ...]` - adds one or more labels.
    Labels {
        /// The node variable being labelled.
        target: Expression,
        /// Labels to add.
        labels: Vec<String>,
        /// Source span.
        span: Span,
    },
}

/// `REMOVE item [, item ...]` - drops properties / labels.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveClause {
    /// Items in source order.
    pub items: Vec<RemoveItem>,
    /// Source span.
    pub span: Span,
}

/// One item in a `REMOVE` clause.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RemoveItem {
    /// `target.prop` - drop a property.
    Property {
        /// A property access expression.
        target: Expression,
        /// Source span.
        span: Span,
    },
    /// `target:Label[:Label ...]` - drop one or more labels.
    Labels {
        /// The node variable.
        target: Expression,
        /// Labels to drop.
        labels: Vec<String>,
        /// Source span.
        span: Span,
    },
}

/// `[DETACH] DELETE target [, target ...]` - delete nodes / edges.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteClause {
    /// `true` for `DETACH DELETE` - drop incident edges first.
    pub detach: bool,
    /// Expressions identifying what to delete.
    pub targets: Vec<Expression>,
    /// Source span.
    pub span: Span,
}

/// `CALL [(imports)] { body }` - inline procedure subquery.
/// `imports` is the explicit-scope form (GP03): when `Some`, only
/// the named outer variables are visible inside the body; `None`
/// means transparent scope (every outer binding is visible).
#[derive(Debug, Clone, PartialEq)]
pub struct CallSubqueryClause {
    /// Optional explicit import list. `None` => transparent scope.
    pub imports: Option<Vec<String>>,
    /// The subquery body.
    pub body: Box<QueryBody>,
    /// Source span (covers `CALL { ... }` including any leading
    /// import list).
    pub span: Span,
}

/// `CALL ns.proc(args) [YIELD field [AS alias] [, ...]]` - invoke a
/// stored procedure / built-in operator (section 13).
#[derive(Debug, Clone, PartialEq)]
pub struct CallClause {
    /// The procedure name, split on `.` into its parts (e.g.
    /// `CALL traverse.graph.labels(g)` -> `["traverse","graph","labels"]`).
    pub name: Vec<String>,
    /// Argument expressions. Empty if the call form is `CALL name`
    /// (no parentheses) or `CALL name()` (empty argument list).
    pub args: Vec<Expression>,
    /// `YIELD` projections (empty when absent).
    pub yields: Vec<YieldItem>,
    /// Source span.
    pub span: Span,
}

/// One `YIELD field [AS alias]` item.
#[derive(Debug, Clone, PartialEq)]
pub struct YieldItem {
    /// Field name as produced by the procedure.
    pub field: String,
    /// Optional rename.
    pub alias: Option<String>,
    /// Source span.
    pub span: Span,
}

/// `RETURN [DISTINCT] item, item ... [ORDER BY ...] [SKIP n] [LIMIT n]`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnClause {
    /// `true` if the source said `RETURN DISTINCT ...`.
    pub distinct: bool,
    /// The projection items.
    pub items: Vec<ReturnItem>,
    /// `ORDER BY` sort keys (empty when absent).
    pub order_by: Vec<OrderItem>,
    /// `SKIP expr` offset (None when absent).
    pub skip: Option<Expression>,
    /// `LIMIT expr` row cap (None when absent).
    pub limit: Option<Expression>,
    /// Source span.
    pub span: Span,
}

/// One item in a `RETURN` projection: `expr [AS alias]`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnItem {
    /// The projected expression.
    pub expr: Expression,
    /// Optional `AS alias` binding.
    pub alias: Option<String>,
    /// Source span.
    pub span: Span,
}

/// One item in an `ORDER BY` clause: `expr [ASC | DESC]`.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    /// The sort key expression.
    pub expr: Expression,
    /// Sort direction (defaults to `Asc` when neither keyword is
    /// given).
    pub direction: SortDirection,
    /// Source span.
    pub span: Span,
}

/// Sort direction for an `ORDER BY` item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    /// Ascending (default).
    Asc,
    /// Descending.
    Desc,
}

// -- Path patterns (ISO/IEC 39075 section 14) -----------------------------

/// A path pattern: `node (edge node)*`, optionally bound to a path
/// variable via `path_var = pattern` (ISO/IEC 39075 section 16.9, optional
/// feature G004).
#[derive(Debug, Clone, PartialEq)]
pub struct PathPattern {
    /// Optional path-variable binding. When `Some(name)`, the source
    /// said `name = pattern` and the variable carries the whole path
    /// (as a `PATH` value) into the enclosing scope.
    pub variable: Option<String>,
    /// The starting node pattern.
    pub start: NodePattern,
    /// Subsequent path elements: a flat `(edge, node)` step or a parenthesized quantified
    /// subpattern (ISO section 16.11).
    pub chain: Vec<PathElement>,
    /// Inner `WHERE pred` filter, only set for parenthesised path
    /// patterns (`((a)-[r]->(b) WHERE pred)` - ISO section 16.7, optional
    /// features G050 / G051). Lowering hoists this into the
    /// enclosing `MatchClause.where_clause` via `AND`.
    pub inner_where: Option<Expression>,
    /// Source span covering the whole pattern.
    pub span: Span,
}

/// One element along a path pattern's chain. A path is a starting
/// node followed by a sequence of these. `Step` is the flat
/// `-[edge]->(node)` continuation; `Group` is a parenthesised
/// quantified subpattern from ISO section 16.11.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // parse-time only; boxing would only add indirection
pub enum PathElement {
    /// Flat step: an edge followed by a node.
    Step {
        /// The edge pattern.
        edge: EdgePattern,
        /// The node it terminates at.
        node: NodePattern,
    },
    /// Parenthesised quantified subpattern: `((sub)){m,n}` - the
    /// inner `sub` is itself a [`PathPattern`] that may contain
    /// further `Step`s or nested `Group`s. The `quantifier`
    /// bounds how many times the inner pattern repeats; absent
    /// bounds use `None` to mean unconstrained.
    Group {
        /// The inner path pattern (no path variable).
        sub: Box<PathPattern>,
        /// Quantifier - `(*) = (1,inf)`, `(*1..n) = (1,n)`, `({m,n}) = (m,n)`.
        quantifier: PathQuantifier,
        /// Span of the entire `((sub)){m,n}` form.
        span: Span,
    },
}

/// Repetition bounds for a [`PathElement::Group`]. `min`/`max` are
/// inclusive; `max = None` means unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathQuantifier {
    /// Lower bound - `0` allows the group to be skipped entirely.
    pub min: usize,
    /// Upper bound - `None` means no cap.
    pub max: Option<usize>,
}

/// A node pattern: `(variable? (`:` label_expression)? (`{` property_map `}`)?)`.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    /// Bound variable name (`None` if the node is anonymous).
    pub variable: Option<String>,
    /// Label predicate from the `:...` annotation. `None` when no `:`
    /// is present. Supports the full section 14.7 expression grammar:
    /// conjunction (`A&B`), disjunction (`A|B`), negation (`!A`),
    /// wildcard (`%`), and grouped subexpressions.
    pub label_expr: Option<LabelExpression>,
    /// Inline property constraints (`{k: v, ...}`). `None` when absent.
    pub properties: Option<PropertyMap>,
    /// Source span (includes the parentheses).
    pub span: Span,
}

/// An edge pattern in any of the three direction forms:
/// `-[...]->`, `<-[...]-`, `-[...]-`.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgePattern {
    /// Bound variable for the edge (`None` for anonymous).
    pub variable: Option<String>,
    /// Edge-type predicate from the `:...` annotation. Uses the same
    /// expression grammar as node labels.
    pub label_expr: Option<LabelExpression>,
    /// Direction of the edge in the pattern.
    pub direction: EdgeDirection,
    /// Variable-length quantifier (`*`, `*n`, `*n..`, `*..m`, `*n..m`).
    /// `None` for a single-hop edge.
    pub quantifier: Option<EdgeQuantifier>,
    /// Inline property constraints. `None` when absent.
    pub properties: Option<PropertyMap>,
    /// Source span (includes the dashes / arrows on both sides).
    pub span: Span,
}

/// A label predicate (section 14.7).
///
/// Precedence (highest to lowest): `Negation` > `Conjunction` >
/// `Disjunction`. Parentheses override.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum LabelExpression {
    /// `Foo` - a single label or edge-type spelling.
    Label {
        /// The label name (case-sensitive, as written).
        name: String,
        /// Source span.
        span: Span,
    },
    /// `%` - matches any label.
    Wildcard {
        /// Source span.
        span: Span,
    },
    /// `A & B [& ...]` - flat n-ary conjunction.
    Conjunction {
        /// Conjuncts (always at least two; the parser flattens nested
        /// conjunctions into a single list).
        operands: Vec<LabelExpression>,
        /// Source span.
        span: Span,
    },
    /// `A | B [| ...]` - flat n-ary disjunction.
    Disjunction {
        /// Disjuncts (always at least two).
        operands: Vec<LabelExpression>,
        /// Source span.
        span: Span,
    },
    /// `!expr` - negation.
    Negation {
        /// The negated expression.
        operand: Box<LabelExpression>,
        /// Source span (covers the `!` and the operand).
        span: Span,
    },
}

impl LabelExpression {
    /// Source span of any label expression.
    pub fn span(&self) -> Span {
        match self {
            LabelExpression::Label { span, .. }
            | LabelExpression::Wildcard { span }
            | LabelExpression::Conjunction { span, .. }
            | LabelExpression::Disjunction { span, .. }
            | LabelExpression::Negation { span, .. } => *span,
        }
    }
}

/// A variable-length quantifier on an edge pattern (section 14.6).
///
/// `None` / `None` means `*` (any length). One or both bounds may be
/// set: `*1..` is `min=Some(1), max=None`; `*..3` is
/// `min=None, max=Some(3)`; `*2..5` is both; `*3` (exact) is
/// `min=Some(3), max=Some(3)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeQuantifier {
    /// Lower bound (inclusive). `None` means "no lower bound".
    pub min: Option<u32>,
    /// Upper bound (inclusive). `None` means "no upper bound".
    pub max: Option<u32>,
    /// Source span (covers the `*` and bounds).
    pub span: Span,
}

/// Direction encoded in an edge pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDirection {
    /// `-[...]->` from the preceding node to the following node.
    Outgoing,
    /// `<-[...]-` from the following node to the preceding node.
    Incoming,
    /// `-[...]-` either direction.
    Undirected,
}

/// An inline property constraint map: `{key: expr, ...}`.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyMap {
    /// `(key, value)` pairs in source order.
    pub entries: Vec<(String, Expression)>,
    /// Source span (includes the braces).
    pub span: Span,
}

// -- Expressions (ISO/IEC 39075 section 11) -------------------------------

/// An expression. Built from a Pratt parser - see
/// `parser::parse_expression` for the precedence ladder.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Expression {
    /// Integer literal (in `i64` range).
    IntegerLit {
        /// Value.
        value: i64,
        /// Source span.
        span: Span,
    },
    /// Floating-point literal.
    FloatLit {
        /// Value (finite - `NaN` / infinity are rejected by the lexer).
        value: f64,
        /// Source span.
        span: Span,
    },
    /// String literal (escape sequences already resolved).
    StringLit {
        /// Value.
        value: String,
        /// Source span.
        span: Span,
    },
    /// Boolean literal.
    BoolLit {
        /// Value.
        value: bool,
        /// Source span.
        span: Span,
    },
    /// `NULL`.
    NullLit {
        /// Source span.
        span: Span,
    },
    /// A bound variable reference (e.g. `n`).
    Variable {
        /// Variable name.
        name: String,
        /// Source span.
        span: Span,
    },
    /// Property access on an expression (`x.name`).
    Property {
        /// The expression on the left of the `.`.
        base: Box<Expression>,
        /// The property name.
        name: String,
        /// Source span covering the whole `base.name`.
        span: Span,
    },
    /// `$name` query parameter reference.
    Parameter {
        /// Parameter name (without the `$`).
        name: String,
        /// Source span.
        span: Span,
    },
    /// A binary operator application.
    Binary {
        /// Operator.
        op: BinaryOp,
        /// Left operand.
        lhs: Box<Expression>,
        /// Right operand.
        rhs: Box<Expression>,
        /// Source span.
        span: Span,
    },
    /// A unary operator application.
    Unary {
        /// Operator.
        op: UnaryOp,
        /// Operand.
        operand: Box<Expression>,
        /// Source span.
        span: Span,
    },
    /// `(expr)` parenthesised expression. Preserved in the AST so the
    /// pretty-printer can round-trip user-written grouping.
    Paren {
        /// The inner expression.
        inner: Box<Expression>,
        /// Source span (includes the parentheses).
        span: Span,
    },
    /// `name(arg, arg, ...)` - a function call. The optional `distinct`
    /// flag carries the `DISTINCT` modifier permitted by certain
    /// aggregates (`count(DISTINCT x)`).
    FunctionCall {
        /// Function name.
        name: String,
        /// `true` for `name(DISTINCT ...)`.
        distinct: bool,
        /// Argument expressions in source order.
        args: Vec<Expression>,
        /// Source span (covers the name through the closing `)`).
        span: Span,
    },
    /// `[expr, expr, ...]` - a list literal.
    List {
        /// List elements.
        items: Vec<Expression>,
        /// Source span (includes the brackets).
        span: Span,
    },
    /// `{key: expr, key: expr, ...}` - a map literal in expression
    /// position. Distinct from [`PropertyMap`] (used in pattern
    /// syntax) so the lowering pass can render either form
    /// independently. Empty maps are represented as `entries: vec![]`.
    Map {
        /// Map entries in source order. Keys are strings; the
        /// grammar accepts plain identifiers, delimited identifiers
        /// (`` `key` ``), and keyword-as-name forms.
        entries: Vec<(String, Expression)>,
        /// Source span (includes the braces).
        span: Span,
    },
    /// `CASE` expression (ISO/IEC 39075 section 20.7). Two surface forms,
    /// distinguished by whether the operand is present:
    ///
    /// * Simple:   `CASE operand WHEN v1 THEN r1 [WHEN v2 THEN r2 ...] [ELSE d] END`
    /// * Searched: `CASE WHEN cond1 THEN r1 [WHEN cond2 THEN r2 ...] [ELSE d] END`
    ///
    /// In the simple form, each WHEN value is compared against
    /// `operand` for equality. In the searched form, each WHEN
    /// expression is evaluated as a boolean predicate. The first
    /// matching arm's result is returned; if none match and `else_`
    /// is `None`, the value is null.
    Case {
        /// `Some` for the simple form; `None` for the searched form.
        operand: Option<Box<Expression>>,
        /// `WHEN expr THEN expr` arms in source order. At least one
        /// arm is required by the spec.
        arms: Vec<CaseArm>,
        /// Optional `ELSE expr` default. `None` => null when no arm
        /// matches.
        else_: Option<Box<Expression>>,
        /// Source span (covers `CASE` through `END`).
        span: Span,
    },
    /// `base[index]` - element access.
    Index {
        /// The expression being indexed.
        base: Box<Expression>,
        /// The index expression.
        index: Box<Expression>,
        /// Source span (covers `base[index]`).
        span: Span,
    },
    /// `operand IS [NOT] NULL` - null-test predicate (section 11.4). Modelled
    /// as a dedicated variant rather than a binary op so the
    /// asymmetric grammar (`IS NULL` vs `IS NOT NULL`) stays explicit.
    IsNull {
        /// The expression being tested.
        operand: Box<Expression>,
        /// `true` for `IS NOT NULL`, `false` for `IS NULL`.
        negated: bool,
        /// Source span (covers `operand IS [NOT] NULL`).
        span: Span,
    },
    /// `EXISTS { query }` - existential subquery (ISO/IEC 39075 section 19.4,
    /// also optional feature GQ22). Returns BOOLEAN; true iff the
    /// inner query yields at least one row when evaluated in the
    /// enclosing scope. Outer-bound variables are visible inside the
    /// subquery; subquery-only bindings do not leak out.
    ExistsSubquery {
        /// The inner query body - a regular linear chain of clauses.
        /// The pattern-only shortcut `EXISTS { (a)-[]->(b) }` is
        /// represented as a one-clause body containing a `MATCH` of
        /// that pattern.
        body: Box<QueryBody>,
        /// Source span (covers `EXISTS { ... }`).
        span: Span,
    },
    /// `operand IS [NOT] <typename>`, `operand IS [NOT] LABELED
    /// <label_expr>`, `operand IS [NOT] NORMALIZED [<form>]` - the
    /// value-type / labeled / normalised predicates (ISO section 19.6, section 19.7,
    /// G111). Always returns BOOLEAN, never NULL - per ISO type
    /// predicates do not propagate null (`null IS BOOLEAN` => `false`).
    TypePredicate {
        /// The expression being tested.
        operand: Box<Expression>,
        /// What kind of type-membership is being tested.
        kind: TypeKind,
        /// `true` for the `IS NOT ...` form.
        negated: bool,
        /// Source span.
        span: Span,
    },
}

impl Expression {
    /// Convenience accessor for the source span of any expression.
    pub fn span(&self) -> Span {
        match self {
            Expression::IntegerLit { span, .. }
            | Expression::FloatLit { span, .. }
            | Expression::StringLit { span, .. }
            | Expression::BoolLit { span, .. }
            | Expression::NullLit { span }
            | Expression::Variable { span, .. }
            | Expression::Property { span, .. }
            | Expression::Parameter { span, .. }
            | Expression::Binary { span, .. }
            | Expression::Unary { span, .. }
            | Expression::Paren { span, .. }
            | Expression::FunctionCall { span, .. }
            | Expression::List { span, .. }
            | Expression::Map { span, .. }
            | Expression::Case { span, .. }
            | Expression::Index { span, .. }
            | Expression::IsNull { span, .. }
            | Expression::TypePredicate { span, .. }
            | Expression::ExistsSubquery { span, .. } => *span,
        }
    }
}

/// What a [`Expression::TypePredicate`] tests. The named scalars
/// correspond to ISO/IEC 39075 section 4.16 predefined types; the
/// structural variants cover section 4.13 object types. `Labeled` and
/// `Normalized` are the two parameterised forms (section 19.6 G111 and
/// section 19.7 respectively).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum TypeKind {
    /// `IS [NOT] BOOLEAN` / `BOOL`.
    Boolean,
    /// `IS [NOT] STRING` / `VARCHAR`.
    String,
    /// `IS [NOT] INTEGER` / `INT` / `SIGNED INTEGER`.
    Integer,
    /// `IS [NOT] FLOAT`.
    Float,
    /// `IS [NOT] NUMBER` - any numeric type (INTEGER or FLOAT).
    Number,
    /// `IS [NOT] NODE` / `VERTEX` / `ANY NODE` / `ANY VERTEX`.
    Node,
    /// `IS [NOT] RELATIONSHIP` / `EDGE` / `ANY RELATIONSHIP` /
    /// `ANY EDGE`.
    Edge,
    /// `IS [NOT] PATH`.
    Path,
    /// `IS [NOT] LIST` / `ARRAY` (any list type).
    List,
    /// `IS [NOT] MAP` (any map type).
    Map,
    /// `IS [NOT] DATE`.
    Date,
    /// `IS [NOT] LOCAL TIME` / `LOCAL_TIME`.
    LocalTime,
    /// `IS [NOT] ZONED TIME` / `ZONED_TIME` / `TIME`.
    ZonedTime,
    /// `IS [NOT] LOCAL DATETIME` / `LOCAL_DATETIME`.
    LocalDatetime,
    /// `IS [NOT] ZONED DATETIME` / `ZONED_DATETIME` / `DATETIME`.
    ZonedDatetime,
    /// `IS [NOT] DURATION`.
    Duration,
    /// `IS [NOT] ANY` - type predicate that's true for any non-null
    /// value (`IS NOT ANY` is true only for null).
    Any,
    /// `IS [NOT] NOTHING` - empty type; true only for null per
    /// GV71 (the GQL "immaterial value type"). Distinct from
    /// `IS NULL` which is its own dedicated AST node.
    Nothing,
    /// `IS [NOT] LABELED <label_expression>` - true iff the
    /// operand is a node/edge whose labels (or edge type) match
    /// the given label expression. The expression supports
    /// conjunction (`A&B`), disjunction (`A|B`), negation (`!A`),
    /// and the wildcard (`%`) - same grammar as a pattern's `:`
    /// annotation.
    Labeled(LabelExpression),
    /// `IS [NOT] NORMALIZED [<form>]` (section 19.7). Form defaults to
    /// NFC when omitted. Accepted: NFC / NFD / NFKC / NFKD,
    /// case-insensitive. Stored as the source spelling for
    /// round-tripping; the executor compares case-insensitively.
    Normalized(Option<String>),
    /// `IS [NOT] T1 | T2 | ...`: dynamic union of value types (GV66 open / GV67 closed). True iff the
    /// operand matches *any* component. The parser flattens nested unions; `Labeled` / `Normalized`
    /// aren't accepted as components (they'd clash with the label-expression `|`). Never empty.
    Union(Vec<TypeKind>),
}

/// One `WHEN expr THEN expr` arm of a [`Expression::Case`]. The
/// shape is identical between simple and searched CASE - the
/// distinction is whether the enclosing `Case` carries an `operand`.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseArm {
    /// In a simple CASE, the value to compare against the operand
    /// (equality). In a searched CASE, the boolean predicate.
    pub when: Expression,
    /// The result returned if this arm matches.
    pub then: Expression,
    /// Source span (covers `WHEN expr THEN expr`).
    pub span: Span,
}

/// Binary operators (ordered by ISO/IEC 39075 section 11 precedence
/// ascending - `Or` is the lowest, `Mod` the highest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `OR` (logical or).
    Or,
    /// `XOR` (logical exclusive or).
    Xor,
    /// `AND` (logical and).
    And,
    /// `=`
    Eq,
    /// `<>` / `!=`
    Neq,
    /// `<`
    Lt,
    /// `<=`
    Lte,
    /// `>`
    Gt,
    /// `>=`
    Gte,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `||` - string concatenation (ISO/IEC 39075 section 20.23). Both
    /// operands must be strings; the result is their juxtaposition.
    /// Null propagates per the standard's two-valued result on
    /// string types.
    Concat,
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Arithmetic negation (`-x`).
    Neg,
    /// Logical negation (`NOT x`).
    Not,
}
