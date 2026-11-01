//! Shared infrastructure for the three TCK harnesses (`tck.rs` /
//! `tck_execute.rs` / `tck_correctness.rs`).
//!
//! The main export is [`GqlCatalog`], an in-memory model of the schemas,
//! directories, graphs and graph types the catalog scenarios mutate, with
//! enough ISO/IEC 39075 section 7 semantics for them to run:
//!
//! * Names are absolute paths like `/foo/bar`.
//! * Creating `/a/b/c` implicitly creates the parent directories `/a` and
//!   `/a/b` (the TCK's `+directories | 1` side effect).
//! * A name is one kind of entity only (schema | directory | graph | graph
//!   type); collisions return [`CatalogError::NameCollides`].
//!
//! Not modeled: side-effect deltas. The `Then the side effects should be:`
//! counters aren't tracked, so scenarios checking only side effects pass as
//! zero rows.

#![allow(dead_code)] // shared module; each harness uses a subset

use std::collections::{BTreeSet, HashMap};

use gherkin::Scenario;
use traverse_core::graph::Graph;
use traverse_cypher::execute_statement_with_params_timed;
use traverse_cypher::types::CypherValue;
use traverse_gql::ast::{
    GraphTypeBody, GraphTypeElement, GraphTypeViolation, MAX_NODE_LABELS, MIN_NODE_LABELS,
    validate_graph_type_elements,
};
use traverse_gql::{SchemaPath, Statement, lower, parse};

/// Expands a `Scenario Outline` into one concrete `Scenario` per `Examples:` row; scenarios without
/// examples come back as a one-element Vec. Each `<header>` placeholder in a step's `value` and
/// `docstring` takes the matching row's cell, and the clone's `name` gets a `" [example N]"` suffix
/// so failures stay distinguishable.
pub fn expand_scenarios(scenario: &Scenario) -> Vec<Scenario> {
    let scenario = &preprocess_scenario(scenario);
    if scenario.examples.is_empty() {
        return vec![scenario.clone()];
    }
    let mut out = Vec::new();
    let mut row_idx = 0usize;
    for examples in &scenario.examples {
        let Some(table) = &examples.table else {
            continue;
        };
        let (header, data) = match table.rows.split_first() {
            Some((h, d)) => (h, d),
            None => continue,
        };
        for row in data {
            row_idx += 1;
            let mut sc = scenario.clone();
            sc.name = format!("{} [example {row_idx}]", scenario.name);
            for step in &mut sc.steps {
                step.value = substitute_placeholders(&step.value, header, row);
                if let Some(doc) = &step.docstring {
                    step.docstring = Some(substitute_placeholders(doc, header, row));
                }
            }
            out.push(sc);
        }
    }
    // no usable example rows: keep the original so the caller still sees the scenario
    if out.is_empty() {
        out.push(scenario.clone());
    }
    out
}

/// TCK preprocessing before expansion: `$(randomLabelSet(...))` substitution plus fixups for
/// feature-file typos at the pinned upstream commit (e0700a7).
fn preprocess_scenario(scenario: &Scenario) -> Scenario {
    let mut sc = scenario.clone();

    // -- upstream feature-file bugs (github.com/opengql/tck @ e0700a7) -- both scenarios'
    // When-programs are byte-identical to their passing siblings, so as written they can't raise
    // the expected conditions; restore the intended text. Drop once the submodule moves past a fix.
    let is_when = |step: &gherkin::Step| {
        let lc = step.value.to_lowercase();
        lc.starts_with("executing query")
            || lc.starts_with("executing the query")
            || lc.starts_with("executing program")
            || lc.starts_with("executing the program")
    };
    if sc.name == "[7] Create a schema, if not exists" {
        // tagged @GC02 ("if not exists") and asserts zero side effects on a duplicate create, but
        // the When-program lacks the IF NOT EXISTS its name and tag promise
        for step in &mut sc.steps {
            if is_when(step)
                && let Some(doc) = &step.docstring
            {
                step.docstring =
                    Some(doc.replace("CREATE SCHEMA /foo", "CREATE SCHEMA IF NOT EXISTS /foo"));
            }
        }
    }
    if sc.name == "[6] Creating a single node type with duplicate property names fails" {
        // the property list has three distinct names (same as scenario [5], which must succeed);
        // duplicate one so the scenario tests what its name says
        for step in &mut sc.steps {
            if is_when(step)
                && let Some(doc) = &step.docstring
            {
                step.docstring =
                    Some(doc.replace("age INT, studentID STRING", "age INT, name STRING"));
            }
        }
    }

    // -- `$(randomLabelSet(<expr>))` substitution --
    for step in &mut sc.steps {
        if let Some(doc) = &step.docstring
            && doc.contains("$(randomLabelSet(")
        {
            step.docstring = Some(expand_random_label_sets(doc));
        }
    }
    sc
}

/// Replaces every `$(randomLabelSet(<expr>))` in `text` with a deterministic label set of the
/// requested size, rendered `:TckLabel1&TckLabel2&...` (empty for size 0). `<expr>` is
/// `minNodeLabels` / `maxNodeLabels` with an optional `+k` / `-k`: the TCK sizes label sets
/// relative to the declared limits. Deterministic rather than random so runs reproduce; only the
/// cardinality matters.
fn expand_random_label_sets(text: &str) -> String {
    const OPEN: &str = "$(randomLabelSet(";
    let mut out = String::new();
    let mut rest = text;
    while let Some(pos) = rest.find(OPEN) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + OPEN.len()..];
        let Some(close) = after.find("))") else {
            // unterminated: emit as-is, stop substituting
            out.push_str(&rest[pos..]);
            return out;
        };
        let expr = &after[..close];
        match eval_label_set_size(expr) {
            Some(n) => {
                for i in 1..=n {
                    out.push_str(if i == 1 { ":" } else { "&" });
                    out.push_str(&format!("TckLabel{i}"));
                }
            }
            None => {
                // unknown expression: leave the text so it fails visibly as a parse error, not a
                // silent drop
                out.push_str(&rest[pos..pos + OPEN.len() + close + 2]);
            }
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

/// `minNodeLabels`/`maxNodeLabels` +/- integer, saturating at zero (the TCK's `minNodeLabels-1` with
/// a declared minimum of 1).
fn eval_label_set_size(expr: &str) -> Option<usize> {
    let expr: String = expr.chars().filter(|c| !c.is_whitespace()).collect();
    let (base, rest) = if let Some(r) = expr.strip_prefix("minNodeLabels") {
        (MIN_NODE_LABELS as i64, r)
    } else {
        let r = expr.strip_prefix("maxNodeLabels")?;
        (MAX_NODE_LABELS as i64, r)
    };
    let n = if rest.is_empty() {
        base
    } else if let Some(k) = rest.strip_prefix('+') {
        base + k.parse::<i64>().ok()?
    } else {
        let k = rest.strip_prefix('-')?;
        base - k.parse::<i64>().ok()?
    };
    Some(n.max(0) as usize)
}

fn substitute_placeholders(template: &str, header: &[String], row: &[String]) -> String {
    let mut s = template.to_string();
    for (col, name) in header.iter().enumerate() {
        let placeholder = format!("<{}>", name);
        let value = row.get(col).map(String::as_str).unwrap_or("");
        s = s.replace(&placeholder, value);
    }
    s
}

/// In-memory catalog model, one per scenario (TCK scenarios assume no state leaks between them).
#[derive(Debug, Default)]
pub struct GqlCatalog {
    schemas: BTreeSet<String>,
    /// Implicit directories: parents of schemas / graphs / graph types not declared as schemas.
    directories: BTreeSet<String>,
    /// Graph path -> normalized type signature (`None` = open/ANY or an unstructured body).
    graphs: std::collections::BTreeMap<String, TypeSig>,
    /// Graph-type path -> normalized type signature.
    graph_types: std::collections::BTreeMap<String, TypeSig>,
}

/// Normalized graph-type signature for equivalence checks (`AS COPY OF` with an explicit closed
/// type must match the source graph's type, G2000 otherwise). `None` = open / unknown. Element and
/// property order is canonicalized, so reordered but equivalent bodies compare equal.
pub type TypeSig = Option<Vec<ElementSig>>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ElementSig {
    pub is_node: bool,
    pub labels: Vec<String>,
    /// (name, rendered type, NOT NULL) triples, sorted by name.
    pub properties: Vec<(String, String, bool)>,
}

/// Canonical signature of one element.
fn element_sig(el: &GraphTypeElement) -> ElementSig {
    let (is_node, labels, properties) = match el {
        GraphTypeElement::Node {
            labels, properties, ..
        } => (true, labels, properties),
        GraphTypeElement::Edge {
            labels, properties, ..
        } => (false, labels, properties),
    };
    let mut labels: Vec<String> = labels.clone();
    labels.sort();
    let mut props: Vec<(String, String, bool)> = properties
        .iter()
        .map(|p| (p.name.clone(), format!("{:?}", p.value_type), p.not_null))
        .collect();
    props.sort();
    ElementSig {
        is_node,
        labels,
        properties: props,
    }
}

fn elements_sig(els: &[GraphTypeElement]) -> Vec<ElementSig> {
    let mut sigs: Vec<ElementSig> = els.iter().map(element_sig).collect();
    sigs.sort();
    sigs
}

/// Catalog mutation failures that aren't panics; the harness maps them to "ran cleanly" (Ok) vs
/// "runtime error" (Err).
#[derive(Debug, Clone)]
pub enum CatalogError {
    /// Name exists as the same kind (e.g. duplicate `CREATE SCHEMA /foo`).
    AlreadyExists { kind: &'static str, path: String },
    /// Drop target doesn't exist.
    NotFound { kind: &'static str, path: String },
    /// Drop target is a non-empty schema.
    NotEmpty { path: String },
    /// Name used by a different entity kind (e.g. a schema at a path that's already a graph).
    NameCollides {
        existing_kind: &'static str,
        path: String,
    },
    /// Mutating statement inside a `START TRANSACTION READ ONLY` block.
    ReadOnlyViolation { what: &'static str },
    /// Two property declarations in one element type share a name.
    DuplicateProperty { property: String },
    /// Node type with fewer labels than the minimum (ISO 22G0N).
    LabelSetTooSmall { count: usize },
    /// Node type with more labels than the maximum (ISO 22G0P).
    LabelSetTooLarge { count: usize },
    /// `AS COPY OF` declared a closed type not equivalent to the source graph's (ISO G2000).
    CopyTypeMismatch { source: String },
    /// Catalog-modifying statement composed with `NEXT`; catalog DDL isn't linear-composable (ISO
    /// 42000).
    CatalogStatementInNextChain,
}

impl From<GraphTypeViolation> for CatalogError {
    fn from(v: GraphTypeViolation) -> Self {
        match v {
            GraphTypeViolation::DuplicateProperty { property } => {
                CatalogError::DuplicateProperty { property }
            }
            GraphTypeViolation::TooFewLabels { count } => CatalogError::LabelSetTooSmall { count },
            GraphTypeViolation::TooManyLabels { count } => CatalogError::LabelSetTooLarge { count },
        }
    }
}

fn is_mutating(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::CreateSchema { .. }
            | Statement::DropSchema { .. }
            | Statement::CreateGraph { .. }
            | Statement::DropGraph { .. }
            | Statement::CreateGraphType { .. }
            | Statement::DropGraphType { .. }
            | Statement::CreateIndex { .. }
            | Statement::DropIndex { .. }
            | Statement::CreateConstraint { .. }
            | Statement::DropConstraint { .. }
    )
}

/// Does a query statement insert, set, remove or delete?
fn writes_data(stmt: &Statement) -> bool {
    let body_writes = |b: &traverse_gql::QueryBody| {
        b.clauses.iter().any(|c| {
            matches!(
                c,
                traverse_gql::Clause::Insert(_)
                    | traverse_gql::Clause::Set(_)
                    | traverse_gql::Clause::Remove(_)
                    | traverse_gql::Clause::Delete(_)
            )
        })
    };
    match stmt {
        Statement::Query(q) => {
            body_writes(&q.head) || q.unions.iter().any(|u| body_writes(&u.body))
        }
        _ => false,
    }
}

fn mutation_label(stmt: &Statement) -> &'static str {
    match stmt {
        Statement::CreateSchema { .. } | Statement::DropSchema { .. } => "schema DDL",
        Statement::CreateGraph { .. } | Statement::DropGraph { .. } => "graph DDL",
        Statement::CreateGraphType { .. } | Statement::DropGraphType { .. } => "graph-type DDL",
        Statement::CreateIndex { .. } | Statement::DropIndex { .. } => "index DDL",
        Statement::CreateConstraint { .. } | Statement::DropConstraint { .. } => "constraint DDL",
        _ => "mutation",
    }
}

impl GqlCatalog {
    pub fn create_schema(
        &mut self,
        path: &SchemaPath,
        if_not_exists: bool,
    ) -> Result<(), CatalogError> {
        let abs = canonicalise(path);
        // `IF NOT EXISTS`: succeed silently if the same kind already exists here; other-kind
        // collisions still error, so DDL can't clobber a graph
        if if_not_exists && self.schemas.contains(&abs) {
            return Ok(());
        }
        self.reject_collision(&abs)?;
        self.ensure_parent_dirs(&abs);
        self.schemas.insert(abs);
        Ok(())
    }

    pub fn drop_schema(&mut self, path: &SchemaPath, if_exists: bool) -> Result<(), CatalogError> {
        let abs = canonicalise(path);
        if !self.schemas.contains(&abs) {
            return if if_exists {
                Ok(())
            } else {
                Err(CatalogError::NotFound {
                    kind: "schema",
                    path: abs,
                })
            };
        }
        // a schema can only be dropped with no graphs / graph types under it
        let prefix = format!("{abs}/");
        if self.graphs.keys().any(|g| g.starts_with(&prefix))
            || self.graph_types.keys().any(|t| t.starts_with(&prefix))
            || self
                .schemas
                .iter()
                .any(|s| s != &abs && s.starts_with(&prefix))
        {
            return Err(CatalogError::NotEmpty { path: abs });
        }
        self.schemas.remove(&abs);
        Ok(())
    }

    pub fn create_graph(&mut self, path: &SchemaPath, sig: TypeSig) -> Result<(), CatalogError> {
        // the headless `CREATE GRAPH ANY AS COPY OF other` has an empty path: a system-assigned
        // anonymous graph, modeled as a placeholder name
        let abs = if path.segments.is_empty() {
            format!("/__anon_graph_{}", self.graphs.len())
        } else {
            canonicalise(path)
        };
        self.reject_collision(&abs)?;
        self.ensure_parent_dirs(&abs);
        self.graphs.insert(abs, sig);
        Ok(())
    }

    /// Signature of an existing graph (`None` for open/unknown or missing).
    pub fn graph_sig(&self, path: &SchemaPath) -> TypeSig {
        self.graphs.get(&canonicalise(path)).cloned().flatten()
    }

    /// Signature of a declared graph type.
    pub fn graph_type_sig(&self, path: &SchemaPath) -> TypeSig {
        self.graph_types.get(&canonicalise(path)).cloned().flatten()
    }

    pub fn drop_graph(&mut self, path: &SchemaPath, if_exists: bool) -> Result<(), CatalogError> {
        let abs = canonicalise(path);
        if !self.graphs.contains_key(&abs) {
            return if if_exists {
                Ok(())
            } else {
                Err(CatalogError::NotFound {
                    kind: "graph",
                    path: abs,
                })
            };
        }
        self.graphs.remove(&abs);
        Ok(())
    }

    pub fn create_graph_type(
        &mut self,
        path: &SchemaPath,
        sig: TypeSig,
    ) -> Result<(), CatalogError> {
        let abs = canonicalise(path);
        self.reject_collision(&abs)?;
        self.ensure_parent_dirs(&abs);
        self.graph_types.insert(abs, sig);
        Ok(())
    }

    pub fn drop_graph_type(
        &mut self,
        path: &SchemaPath,
        if_exists: bool,
    ) -> Result<(), CatalogError> {
        let abs = canonicalise(path);
        if !self.graph_types.contains_key(&abs) {
            return if if_exists {
                Ok(())
            } else {
                Err(CatalogError::NotFound {
                    kind: "graph type",
                    path: abs,
                })
            };
        }
        self.graph_types.remove(&abs);
        Ok(())
    }

    /// Adds any missing intermediate directories above `path`.
    fn ensure_parent_dirs(&mut self, path: &str) {
        let trimmed = path.trim_start_matches('/');
        let parts: Vec<&str> = trimmed.split('/').collect();
        if parts.len() <= 1 {
            return;
        }
        let mut cur = String::new();
        for part in &parts[..parts.len() - 1] {
            cur.push('/');
            cur.push_str(part);
            // a schema counts as a directory (schemas can have children); an existing directory is
            // skipped; anything else was rejected by the collision checks
            if !self.schemas.contains(&cur) && !self.directories.contains(&cur) {
                self.directories.insert(cur.clone());
            }
        }
    }

    /// Rejects `path` if a different kind already uses it. The caller handles "same kind already"
    /// (`AlreadyExists`, not `NameCollides`).
    fn reject_collision(&self, path: &str) -> Result<(), CatalogError> {
        if self.schemas.contains(path) {
            return Err(CatalogError::AlreadyExists {
                kind: "schema",
                path: path.to_string(),
            });
        }
        if self.graphs.contains_key(path) {
            return Err(CatalogError::NameCollides {
                existing_kind: "graph",
                path: path.to_string(),
            });
        }
        if self.graph_types.contains_key(path) {
            return Err(CatalogError::NameCollides {
                existing_kind: "graph type",
                path: path.to_string(),
            });
        }
        if self.directories.contains(path) {
            return Err(CatalogError::NameCollides {
                existing_kind: "directory",
                path: path.to_string(),
            });
        }
        Ok(())
    }
}

/// `SchemaPath` as the canonical absolute key string. Relative paths get a `/` prefix so all keys
/// share one shape (the TCK never uses relative paths in catalog statements, but normalizing keeps
/// the API symmetric).
fn canonicalise(path: &SchemaPath) -> String {
    let mut s = String::new();
    if !path.segments.is_empty() {
        s.push('/');
        s.push_str(&path.segments.join("/"));
    }
    s
}

/// Outcome of one (parsed and, where applicable, lowered) statement: a uniform `Ok` / `Err`
/// whichever subsystem ran it.
pub enum StmtOutcome {
    /// Ran cleanly; rows for queries, always empty for catalog ops.
    Ran {
        rows: Vec<Vec<CypherValue>>,
        columns: Vec<String>,
    },
    /// Catalog ops can't lower to the Cypher AST; this carries the classification for `Then an
    /// exception condition should be raised: ...`.
    CatalogError(CatalogError),
    /// Cypher engine error.
    EngineError(traverse_cypher::error::CypherError),
    /// Lowering failed (NotImplemented for the unhandled grammar tail).
    LowerFailed,
    /// Failed to parse; never silent.
    ParseFailed,
}

impl StmtOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, StmtOutcome::Ran { .. })
    }

    /// Failed outcome -> ISO/IEC 39075 error code (or the nearest SQL / GQL code; the spec inherits
    /// much of SQL's classification). `None` for `Ran`. Catalog errors map to their specific codes;
    /// everything else folds into `42000` ("syntax error or access rule violation").
    pub fn iso_error_code(&self) -> Option<&'static str> {
        match self {
            StmtOutcome::Ran { .. } => None,
            // catalog errors route to a specific ISO code where there is one:
            // * ReadOnlyViolation -> 25G03 (invalid transaction state: a read-only
            //   transaction can't mutate the catalog)
            // * everything else -> 42000 (syntax error or access-rule violation)
            StmtOutcome::CatalogError(CatalogError::ReadOnlyViolation { .. }) => Some("25G03"),
            // * LabelSetTooSmall -> 22G0N (number of node labels below supported minimum)
            // * LabelSetTooLarge -> 22G0P (number of node labels exceeds supported maximum)
            // * CopyTypeMismatch -> G2000 (copied graph doesn't conform to the declared
            //   graph type)
            StmtOutcome::CatalogError(CatalogError::LabelSetTooSmall { .. }) => Some("22G0N"),
            StmtOutcome::CatalogError(CatalogError::LabelSetTooLarge { .. }) => Some("22G0P"),
            StmtOutcome::CatalogError(CatalogError::CopyTypeMismatch { .. }) => Some("G2000"),
            // all other failures collapse into 42000 (syntax error or access rule
            // violation):
            // * parse / lower failures = syntax
            // * catalog conflicts (already-exists, name-collides, ...) = access rule
            //   violation
            // * engine errors = either (the executor reports semantic failures as
            //   CypherError)
            StmtOutcome::ParseFailed
            | StmtOutcome::LowerFailed
            | StmtOutcome::CatalogError(_)
            | StmtOutcome::EngineError(_) => Some("42000"),
        }
    }
}

/// Parses and dispatches a multi-statement program: each statement lowers to the Cypher engine or
/// goes through the in-memory catalog. Returns the **last** statement's outcome (the TCK takes a
/// program's result from its final query).
pub fn run_program(graph: &mut Graph, catalog: &mut GqlCatalog, query: &str) -> StmtOutcome {
    let program = match parse(query) {
        Ok(p) => p,
        Err(_) => return StmtOutcome::ParseFailed,
    };
    // all queries joined by `NEXT`: hand the whole program to the lowerer, whose fuser splices it
    // into one Cypher statement (`RETURN->WITH` at each seam, filtered by any YIELD list); mixed
    // programs (catalog DDL or `;`-separated) run statement by statement so the test catalog sees
    // each CREATE GRAPH / DROP SCHEMA / ...
    let all_queries_with_next = program
        .statements
        .iter()
        .all(|s| matches!(s, traverse_gql::Statement::Query(_)))
        && program
            .separators
            .iter()
            .all(|s| matches!(s, traverse_gql::ast::StatementSeparator::Next { .. }));
    if program.statements.len() > 1 && all_queries_with_next {
        return run_fused(graph, &program);
    }
    // ISO 39075: `NEXT` composes linear *data* statements; a catalog-modifying statement in a NEXT
    // chain is a syntax violation (TCK schemas/Create1 [9], 42000). Plain multi-statement programs
    // (newline / `;`) are fine.
    let has_next = program
        .separators
        .iter()
        .any(|s| matches!(s, traverse_gql::ast::StatementSeparator::Next { .. }));
    if has_next && program.statements.iter().any(is_mutating) {
        return StmtOutcome::CatalogError(CatalogError::CatalogStatementInNextChain);
    }
    let mut last = StmtOutcome::Ran {
        rows: Vec::new(),
        columns: Vec::new(),
    };
    for stmt in &program.statements {
        last = run_one(graph, catalog, stmt);
        if !last.is_ok() {
            return last;
        }
    }
    last
}

/// Runs an all-query NEXT chain through the lowerer in one go; the fuser yields a single Cypher
/// statement.
fn run_fused(graph: &mut Graph, program: &traverse_gql::Program) -> StmtOutcome {
    let mut cy_stmt = match lower::lower(program) {
        Ok(s) => s,
        Err(_) => return StmtOutcome::LowerFailed,
    };
    let path_mode = lower::dominant_path_mode(program);
    if let Some(n) = lower::dominant_path_selector_limit(program) {
        lower::apply_path_selector_limit(&mut cy_stmt, n);
    }
    let _path_mode_guard = traverse_cypher::session::set_path_mode(path_mode);
    match execute_statement_with_params_timed(graph, cy_stmt, &HashMap::new()) {
        Ok((r, _, _, _)) => StmtOutcome::Ran {
            rows: r.rows,
            columns: r.columns,
        },
        Err(e) => StmtOutcome::EngineError(e),
    }
}

/// Structured elements of an inline body, also through the `<type> AS COPY OF ...` wrapper. `None`
/// for open / reference / opaque bodies.
fn inline_elements(body: &GraphTypeBody) -> Option<&[GraphTypeElement]> {
    match body {
        GraphTypeBody::Inline {
            elements: Some(els),
            ..
        } => Some(els),
        GraphTypeBody::Copy {
            type_part: Some(tp),
            ..
        } => inline_elements(tp),
        _ => None,
    }
}

/// Normalized signature of a graph-type body: inline bodies from their element list, references and
/// LIKE/COPY from the catalog. `None` = open (ANY), opaque or unresolvable.
fn sig_of_body(body: &GraphTypeBody, catalog: &GqlCatalog) -> TypeSig {
    match body {
        GraphTypeBody::Open { .. } => None,
        GraphTypeBody::Inline {
            elements: Some(els),
            ..
        } => Some(elements_sig(els)),
        GraphTypeBody::Inline { elements: None, .. } => None,
        GraphTypeBody::Reference { name, .. } => catalog.graph_type_sig(name),
        GraphTypeBody::Like { source, .. } => catalog.graph_sig(source),
        GraphTypeBody::Copy {
            type_part, source, ..
        } => match type_part {
            Some(tp) => sig_of_body(tp, catalog),
            None => catalog.graph_sig(source),
        },
        // other body shapes: open
        _ => None,
    }
}

fn run_one(graph: &mut Graph, catalog: &mut GqlCatalog, stmt: &Statement) -> StmtOutcome {
    match stmt {
        Statement::CreateSchema {
            path,
            if_not_exists,
            ..
        } => match catalog.create_schema(path, *if_not_exists) {
            Ok(()) => empty_ran(),
            Err(e) => StmtOutcome::CatalogError(e),
        },
        Statement::DropSchema {
            path, if_exists, ..
        } => match catalog.drop_schema(path, *if_exists) {
            Ok(()) => empty_ran(),
            Err(e) => StmtOutcome::CatalogError(e),
        },
        Statement::CreateGraph { path, body, .. } => {
            if let Some(b) = body {
                if let Some(els) = inline_elements(b)
                    && let Err(v) = validate_graph_type_elements(els)
                {
                    return StmtOutcome::CatalogError(v.into());
                }
                // `<closed type> AS COPY OF src`: the declared type must equal the source graph's
                // (TCK graphs/Create2 [7], G2000); open or opaque types on either side skip the
                // check
                if let GraphTypeBody::Copy {
                    type_part: Some(tp),
                    source,
                    ..
                } = b
                    && let Some(target_sig) = sig_of_body(tp, catalog)
                    && let Some(src_sig) = catalog.graph_sig(source)
                    && src_sig != target_sig
                {
                    return StmtOutcome::CatalogError(CatalogError::CopyTypeMismatch {
                        source: canonicalise(source),
                    });
                }
            }
            let sig = body.as_ref().and_then(|b| sig_of_body(b, catalog));
            match catalog.create_graph(path, sig) {
                Ok(()) => empty_ran(),
                Err(e) => StmtOutcome::CatalogError(e),
            }
        }
        Statement::DropGraph {
            path, if_exists, ..
        } => match catalog.drop_graph(path, *if_exists) {
            Ok(()) => empty_ran(),
            Err(e) => StmtOutcome::CatalogError(e),
        },
        Statement::CreateGraphType { path, body, .. } => {
            if let Some(els) = inline_elements(body)
                && let Err(v) = validate_graph_type_elements(els)
            {
                return StmtOutcome::CatalogError(v.into());
            }
            let sig = sig_of_body(body, catalog);
            match catalog.create_graph_type(path, sig) {
                Ok(()) => empty_ran(),
                Err(e) => StmtOutcome::CatalogError(e),
            }
        }
        Statement::DropGraphType {
            path, if_exists, ..
        } => match catalog.drop_graph_type(path, *if_exists) {
            Ok(()) => empty_ran(),
            Err(e) => StmtOutcome::CatalogError(e),
        },
        // `USE graph` is session state; the in-memory catalog has no current graph, so it's a no-op
        Statement::Use { .. } => empty_ran(),
        // ISO section 15.4 statement-level conditional, condition evaluated in an empty binding scope
        // (literals, parameters, `SESSION_USER`-style nouns); a small foldable subset is resolved
        // here
        Statement::When {
            condition,
            then_stmt,
            else_stmt,
            ..
        } => match eval_when_condition(condition) {
            Some(true) => run_one(graph, catalog, then_stmt),
            Some(false) => match else_stmt {
                Some(e) => run_one(graph, catalog, e),
                None => empty_ran(),
            },
            None => StmtOutcome::EngineError(traverse_cypher::error::CypherError::semantic(
                "WHEN condition must reduce to a boolean from \
                         literals / parameters at this stage"
                    .to_string(),
            )),
        },
        // `START TRANSACTION [READ ONLY|WRITE] ... COMMIT`: inner statements one at a
        // time; under `READ ONLY` any mutating statement is rejected (ISO 39075 section 9.4)
        // `START TRANSACTION ... COMMIT | ROLLBACK`, checked up front like `traverse_gql::execute`:
        // READ ONLY refuses catalog changes (ISO 39075 section 9.4, 25G03), and a ROLLBACK block can't
        // run changes it couldn't discard
        Statement::Transaction {
            read_only,
            rollback,
            statements,
            ..
        } => {
            if let Some(inner) = statements
                .iter()
                .find(|s| (*read_only || *rollback) && (is_mutating(s) || writes_data(s)))
            {
                return if *read_only {
                    StmtOutcome::CatalogError(CatalogError::ReadOnlyViolation {
                        what: mutation_label(inner),
                    })
                } else {
                    StmtOutcome::EngineError(traverse_cypher::error::CypherError::semantic(
                        "a transaction block ending in ROLLBACK can't run changes",
                    ))
                };
            }
            for inner in statements {
                let out = run_one(graph, catalog, inner);
                if !out.is_ok() {
                    return out;
                }
            }
            empty_ran()
        }
        // everything else lowers to Cypher and runs on the engine
        _ => {
            // `lower::lower` takes a `&Program`, so wrap the single statement in a pseudo-program
            let pseudo = traverse_gql::Program {
                statements: vec![stmt.clone()],
                separators: Vec::new(),
                span: stmt.span(),
            };
            let mut cy_stmt = match lower::lower(&pseudo) {
                Ok(s) => s,
                Err(_) => return StmtOutcome::LowerFailed,
            };
            // install the matcher's path-mode thread-local for this statement; add a top-level
            // LIMIT for ANY / ANY k selectors
            let path_mode = lower::dominant_path_mode(&pseudo);
            if let Some(n) = lower::dominant_path_selector_limit(&pseudo) {
                lower::apply_path_selector_limit(&mut cy_stmt, n);
            }
            let _path_mode_guard = traverse_cypher::session::set_path_mode(path_mode);
            match execute_statement_with_params_timed(graph, cy_stmt, &HashMap::new()) {
                Ok((r, _, _, _)) => StmtOutcome::Ran {
                    rows: r.rows,
                    columns: r.columns,
                },
                Err(e) => StmtOutcome::EngineError(e),
            }
        }
    }
}

fn empty_ran() -> StmtOutcome {
    StmtOutcome::Ran {
        rows: Vec::new(),
        columns: Vec::new(),
    }
}

/// Evaluates a `WHEN` condition with no bindings. `Some(bool)` for forms that fold without graph
/// context (boolean literals, integer comparisons of literals, `AND` / `OR` / `NOT` of those);
/// `None` for anything referencing bindings, non-folded function calls or needing a runtime frame.
/// Matches ISO section 15.4's "evaluable at compile time" intent for the statement-level conditional.
fn eval_when_condition(expr: &traverse_gql::Expression) -> Option<bool> {
    use traverse_gql::Expression;
    match expr {
        Expression::BoolLit { value, .. } => Some(*value),
        Expression::NullLit { .. } => Some(false),
        Expression::IntegerLit { value, .. } => Some(*value != 0),
        Expression::Binary { op, lhs, rhs, .. } => {
            use traverse_gql::BinaryOp;
            // integer-literal comparisons fold, so `WHEN 1 = 1 THEN ...` works
            if let (
                Expression::IntegerLit { value: l, .. },
                Expression::IntegerLit { value: r, .. },
            ) = (lhs.as_ref(), rhs.as_ref())
            {
                let cmp = match op {
                    BinaryOp::Eq => Some(l == r),
                    BinaryOp::Neq => Some(l != r),
                    BinaryOp::Lt => Some(l < r),
                    BinaryOp::Lte => Some(l <= r),
                    BinaryOp::Gt => Some(l > r),
                    BinaryOp::Gte => Some(l >= r),
                    _ => None,
                };
                if let Some(b) = cmp {
                    return Some(b);
                }
            }
            let l = eval_when_condition(lhs)?;
            let r = eval_when_condition(rhs)?;
            match op {
                BinaryOp::And => Some(l && r),
                BinaryOp::Or => Some(l || r),
                BinaryOp::Xor => Some(l ^ r),
                _ => None,
            }
        }
        Expression::Unary { op, operand, .. } => {
            use traverse_gql::UnaryOp;
            match op {
                UnaryOp::Not => eval_when_condition(operand).map(|b| !b),
                _ => None,
            }
        }
        _ => None,
    }
}
