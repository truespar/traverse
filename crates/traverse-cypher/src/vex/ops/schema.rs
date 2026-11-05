//! Schema statements (indexes, constraints, `ANALYZE GRAPH`), the `SHOW` listings, and the
//! transaction commands.

use traverse_core::graph::constraint::{ConstraintEntity, ConstraintType};
use traverse_core::tx::view::GraphView;

use crate::ast::TransactionCommandKind;
use crate::error::CypherError;
use crate::executor::inc_stat;
use crate::types::CypherValue;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::expr::{CompiledExpr, EvalScratch};
use super::super::plan::{SchemaOp, SeedRow, ShowOp};
use super::{PhysOperator, Staged, move_cols};

/// Resolves a DDL name that may be a `$parameter` marker (Cypher 25 dynamic DDL names).
pub(crate) fn resolve_ddl_name(name: &str) -> Result<String, CypherError> {
    let Some(pname) = name.strip_prefix("\u{1}param\u{1}") else {
        return Ok(name.to_string());
    };
    match crate::executor::get_query_param(pname) {
        Some(CypherValue::String(s)) if !s.is_empty() => Ok(s),
        Some(other) => Err(CypherError::type_error(format!(
            "dynamic DDL name parameter ${pname} must be a non-empty STRING, got {}",
            other.type_name()
        ))),
        None => Err(CypherError::semantic(format!(
            "expected parameter ${pname} for the dynamic DDL name"
        ))),
    }
}

/// Vector index dimensionality from the first node with the label and a numeric-list property.
fn infer_dimension(graph: &dyn GraphView, label: &str, property: &str) -> Option<usize> {
    use traverse_core::graph::property::PropertyValue;
    for nid in graph.nodes_by_label(label) {
        if let Ok(Some(PropertyValue::List(items))) = graph.get_node_property(nid, property)
            && !items.is_empty()
            && items
                .iter()
                .all(|v| matches!(v, PropertyValue::Float(_) | PropertyValue::Int(_)))
        {
            return Some(items.len());
        }
    }
    None
}

/// Runs a schema statement.
pub(crate) fn run_schema(op: &SchemaOp, graph: &mut dyn GraphView) -> Result<(), CypherError> {
    match op {
        SchemaOp::CreateIndex {
            name,
            label,
            property,
            extra_properties,
        } => {
            let name = resolve_ddl_name(name)?;
            if extra_properties.is_empty() {
                graph
                    .create_index(&name, label, property)
                    .map_err(CypherError::from)?;
            } else {
                let mut props = vec![property.clone()];
                props.extend(extra_properties.iter().cloned());
                graph
                    .create_composite_index(&name, label, &props, false)
                    .map_err(CypherError::from)?;
            }
            inc_stat(|s| s.indexes_added += 1);
        }
        SchemaOp::CreateVectorIndex {
            name,
            label,
            property,
            dimensions,
            similarity,
            quantization,
        } => {
            let sim = traverse_core::graph::vector::Similarity::parse(similarity).ok_or_else(|| {
                CypherError::semantic(format!(
                    "unknown vector.similarity_function '{similarity}' - expected 'cosine' or 'euclidean'"
                ))
            })?;
            let dim = match dimensions {
                Some(d) => *d,
                None => infer_dimension(graph, label, property).ok_or_else(|| {
                    CypherError::semantic(
                        "vector.dimensions not given and no existing vector to infer it from - \
                         specify OPTIONS { indexConfig: { 'vector.dimensions': N } }"
                            .to_string(),
                    )
                })?,
            };
            let mut params = traverse_core::graph::vector::HnswParams::default();
            if let Some(q) = quantization {
                params.quantize = *q;
            }
            graph
                .create_vector_index(&resolve_ddl_name(name)?, label, property, dim, sim, params)
                .map_err(CypherError::from)?;
            inc_stat(|s| s.indexes_added += 1);
        }
        SchemaOp::CreateFulltextIndex {
            name,
            labels,
            properties,
            analyzer,
        } => {
            let az = match analyzer {
                Some(a) => traverse_core::graph::fulltext::Analyzer::from_name(a).ok_or_else(|| {
                    CypherError::semantic(format!(
                        "unknown fulltext analyzer '{a}' - expected standard, english, keyword, \
                         whitespace, or simple"
                    ))
                })?,
                None => traverse_core::graph::fulltext::Analyzer::Standard,
            };
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let prop_refs: Vec<&str> = properties.iter().map(String::as_str).collect();
            graph
                .create_fulltext_index(&resolve_ddl_name(name)?, &label_refs, &prop_refs, az)
                .map_err(CypherError::from)?;
            inc_stat(|s| s.indexes_added += 1);
        }
        SchemaOp::CreatePointIndex {
            name,
            label,
            property,
        } => {
            graph
                .create_point_index(&resolve_ddl_name(name)?, label, property)
                .map_err(CypherError::from)?;
            inc_stat(|s| s.indexes_added += 1);
        }
        SchemaOp::CreateTextIndex {
            name,
            label,
            property,
        } => {
            graph
                .create_text_index(&resolve_ddl_name(name)?, label, property)
                .map_err(CypherError::from)?;
            inc_stat(|s| s.indexes_added += 1);
        }
        SchemaOp::DropIndex { name } => {
            // every index family answers to DROP INDEX: the registry that owns the name drops it
            if graph.has_vector_index(name) {
                graph.drop_vector_index(name).map_err(CypherError::from)?;
            } else if graph.has_fulltext_index(name) {
                graph.drop_fulltext_index(name).map_err(CypherError::from)?;
            } else if graph.has_point_index(name) {
                graph.drop_point_index(name).map_err(CypherError::from)?;
            } else if graph.has_text_index(name) {
                graph.drop_text_index(name).map_err(CypherError::from)?;
            } else {
                graph
                    .drop_index(&resolve_ddl_name(name)?)
                    .map_err(CypherError::from)?;
            }
            inc_stat(|s| s.indexes_removed += 1);
        }
        SchemaOp::AnalyzeGraph { labels } => graph.analyze_statistics(labels),
        SchemaOp::CreateConstraint {
            name,
            label,
            property,
            extra_properties,
            constraint_type,
            is_relationship,
            if_not_exists,
        } => {
            if *if_not_exists
                && graph
                    .constraint_registry()
                    .descriptors()
                    .iter()
                    .any(|d| d.name == resolve_ddl_name(name).unwrap_or_default())
            {
                return Ok(());
            }
            let ctype = if let Some(rest) = constraint_type.strip_prefix("TYPE ") {
                let k = traverse_core::graph::constraint::PropertyTypeKind::from_surface(rest)
                    .ok_or_else(|| {
                        CypherError::syntax(
                            format!("unknown property type `{rest}` in IS :: constraint"),
                            0,
                        )
                    })?;
                ConstraintType::Type(k)
            } else {
                match constraint_type.as_str() {
                    "UNIQUE" => ConstraintType::Unique,
                    "NODE KEY" => ConstraintType::NodeKey,
                    _ => ConstraintType::Exists,
                }
            };
            if !extra_properties.is_empty() {
                let mut props: Vec<String> = Vec::with_capacity(1 + extra_properties.len());
                props.push(property.clone());
                props.extend(extra_properties.iter().cloned());
                graph
                    .create_composite_constraint(
                        &resolve_ddl_name(name)?,
                        label,
                        &props,
                        ctype,
                        *is_relationship,
                    )
                    .map_err(CypherError::from)?;
            } else if *is_relationship {
                graph
                    .create_relationship_constraint(name, label, property, ctype)
                    .map_err(CypherError::from)?;
            } else {
                graph
                    .create_constraint(&resolve_ddl_name(name)?, label, property, ctype)
                    .map_err(CypherError::from)?;
            }
            inc_stat(|s| s.constraints_added += 1);
        }
        SchemaOp::DropConstraint { name } => {
            graph
                .drop_constraint(&resolve_ddl_name(name)?)
                .map_err(CypherError::from)?;
            inc_stat(|s| s.constraints_removed += 1);
        }
        SchemaOp::CreateEdgeIndex {
            name,
            edge_type,
            property,
            extra_properties,
        } => {
            let name = resolve_ddl_name(name)?;
            if extra_properties.is_empty() {
                graph
                    .create_edge_index(&name, edge_type, property.as_deref())
                    .map_err(CypherError::from)?;
            } else {
                let mut props = vec![property.clone().unwrap_or_default()];
                props.extend(extra_properties.iter().cloned());
                graph
                    .create_composite_index(&name, edge_type, &props, true)
                    .map_err(CypherError::from)?;
            }
            inc_stat(|s| s.indexes_added += 1);
        }
        SchemaOp::DropEdgeIndex { name } => {
            graph
                .drop_edge_index(&resolve_ddl_name(name)?)
                .map_err(CypherError::from)?;
            inc_stat(|s| s.indexes_removed += 1);
        }
    }
    Ok(())
}

/// A schema statement: runs once, yields no rows.
pub struct SchemaOpExec<'p> {
    op: &'p SchemaOp,
    done: bool,
}

impl<'p> SchemaOpExec<'p> {
    pub(crate) fn new(op: &'p SchemaOp, _group: GroupId) -> Self {
        Self { op, done: false }
    }
}

impl PhysOperator for SchemaOpExec<'_> {
    fn next(&mut self, _rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if !self.done {
            self.done = true;
            run_schema(self.op, cx.graph)?;
        }
        Ok(false)
    }

    fn reset(&mut self) {
        self.done = false;
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// `SHOW INDEXES` rows as `(name, type, entityType, labelsOrTypes, properties)`.
fn index_rows(graph: &dyn GraphView) -> Vec<[String; 5]> {
    let strings = graph.strings();
    let mut rows = Vec::new();
    for idx in graph.index_catalog().descriptors() {
        let label = strings
            .try_resolve(idx.label_id.0)
            .unwrap_or("?")
            .to_string();
        let prop = strings
            .try_resolve(idx.property_key.0)
            .unwrap_or("?")
            .to_string();
        // a composite index stores a joined key: its components comma-separated, like Neo4j
        rows.push([
            idx.name.clone(),
            "RANGE".to_string(),
            "NODE".to_string(),
            label,
            prop.replace('\u{1F}', ","),
        ]);
    }
    for desc in graph.edge_index_catalog().descriptors() {
        let etype = strings
            .try_resolve(desc.edge_type_id.0)
            .unwrap_or("?")
            .to_string();
        let prop = desc
            .property_key
            .and_then(|pk| strings.try_resolve(pk.0))
            .unwrap_or("-")
            .to_string();
        let kind = if desc.property_key.is_some() {
            "RANGE"
        } else {
            "LOOKUP"
        };
        rows.push([
            desc.name.clone(),
            kind.to_string(),
            "RELATIONSHIP".to_string(),
            etype,
            prop.replace('\u{1F}', ","),
        ]);
    }
    for (name, label, prop, _dim, _sim) in graph.vector_index_descriptors() {
        rows.push([name, "VECTOR".to_string(), "NODE".to_string(), label, prop]);
    }
    for (name, labels, props, _analyzer) in graph.fulltext_index_descriptors() {
        rows.push([
            name,
            "FULLTEXT".to_string(),
            "NODE".to_string(),
            labels.join(","),
            props.join(","),
        ]);
    }
    for (name, label, prop) in graph.point_index_descriptors() {
        rows.push([name, "POINT".to_string(), "NODE".to_string(), label, prop]);
    }
    for (name, label, prop) in graph.text_index_descriptors() {
        rows.push([name, "TEXT".to_string(), "NODE".to_string(), label, prop]);
    }
    rows
}

/// `SHOW CONSTRAINTS` rows.
fn constraint_rows(graph: &dyn GraphView) -> Vec<[String; 5]> {
    let strings = graph.strings();
    let mut rows = Vec::new();
    for desc in graph.constraint_registry().descriptors() {
        let label = strings
            .try_resolve(desc.label_id.0)
            .unwrap_or("?")
            .to_string();
        let mut prop = strings
            .try_resolve(desc.property_key.0)
            .unwrap_or("?")
            .to_string();
        for pk in &desc.extra_property_keys {
            prop.push_str(", ");
            prop.push_str(strings.try_resolve(pk.0).unwrap_or("?"));
        }
        let is_rel = desc.entity == ConstraintEntity::Relationship;
        let ctype = match &desc.constraint_type {
            ConstraintType::Unique => "UNIQUE".to_string(),
            ConstraintType::Exists => "NOT NULL".to_string(),
            ConstraintType::Type(k) => format!("PROPERTY_TYPE({})", k.surface()),
            ConstraintType::NodeKey if is_rel => "RELATIONSHIP KEY".to_string(),
            ConstraintType::NodeKey => "NODE KEY".to_string(),
        };
        rows.push([
            desc.name.clone(),
            ctype,
            if is_rel { "RELATIONSHIP" } else { "NODE" }.to_string(),
            label,
            prop,
        ]);
    }
    rows
}

/// `(name, category, aggregating)` of the function catalog `SHOW FUNCTIONS` lists.
fn function_catalog() -> Vec<(&'static str, &'static str, bool)> {
    let numeric = [
        "abs", "ceil", "ceiling", "floor", "round", "sign", "rand", "sqrt", "exp", "log", "log10",
        "ln", "e", "pi", "sin", "cos", "tan", "asin", "acos", "atan", "atan2", "cot", "degrees",
        "radians", "haversin", "isNaN",
    ];
    let string = [
        "left",
        "right",
        "ltrim",
        "rtrim",
        "trim",
        "btrim",
        "replace",
        "split",
        "reverse",
        "substring",
        "toLower",
        "toUpper",
        "toString",
        "toStringOrNull",
        "char_length",
        "character_length",
        "normalize",
        "string.join",
        "string.indexOf",
        "string.regexReplace",
    ];
    let list = [
        "size",
        "head",
        "tail",
        "last",
        "range",
        "keys",
        "labels",
        "nodes",
        "relationships",
        "reduce",
        "isEmpty",
        "coll.sort",
        "coll.distinct",
        "coll.flatten",
        "coll.indexOf",
        "coll.min",
        "coll.max",
        "coll.insert",
        "coll.remove",
        "toIntegerList",
        "toFloatList",
        "toStringList",
        "toBooleanList",
    ];
    let scalar = [
        "id",
        "elementId",
        "type",
        "properties",
        "coalesce",
        "nullIf",
        "timestamp",
        "randomUUID",
        "valueType",
        "startNode",
        "endNode",
        "toInteger",
        "toIntegerOrNull",
        "toFloat",
        "toFloatOrNull",
        "toBoolean",
        "toBooleanOrNull",
        "exists",
        "property_exists",
        "path_length",
    ];
    let temporal = [
        "date",
        "time",
        "datetime",
        "localtime",
        "localdatetime",
        "duration",
        "format",
        "duration.between",
        "duration_between",
        "datetime.fromepoch",
        "datetime.fromepochmillis",
    ];
    let spatial = ["point", "point.distance", "point.withinBBox"];
    let vector = [
        "vector",
        "vector_distance",
        "vector_norm",
        "vector_dimension_count",
        "vector.similarity.cosine",
        "vector.similarity.euclidean",
    ];
    let predicate = ["all", "any", "none", "single"];
    let aggregating = [
        "count",
        "sum",
        "avg",
        "min",
        "max",
        "collect",
        "stDev",
        "stDevP",
        "percentileCont",
        "percentileDisc",
    ];
    let mut out: Vec<(&'static str, &'static str, bool)> = Vec::new();
    let groups: [(&[&'static str], &'static str, bool); 9] = [
        (&numeric, "Numeric", false),
        (&string, "String", false),
        (&list, "List", false),
        (&scalar, "Scalar", false),
        (&temporal, "Temporal", false),
        (&spatial, "Spatial", false),
        (&vector, "Vector", false),
        (&predicate, "Predicate", false),
        (&aggregating, "Aggregating", true),
    ];
    for (names, category, agg) in groups {
        for n in names {
            out.push((n, category, agg));
        }
    }
    out.sort_by(|a, b| a.0.cmp(b.0));
    out
}

/// `(name, mode, description)` of the procedure catalog `SHOW PROCEDURES` lists.
fn procedure_catalog() -> Vec<(&'static str, &'static str, &'static str)> {
    let mut out = vec![
        ("db.labels", "READ", "List all labels in the graph."),
        (
            "db.propertyKeys",
            "READ",
            "List all property keys in the graph.",
        ),
        (
            "db.relationshipTypes",
            "READ",
            "List all relationship types in the graph.",
        ),
        (
            "db.schema.visualization",
            "READ",
            "The schema meta-graph (nodes and relationships).",
        ),
        (
            "db.awaitIndexes",
            "READ",
            "Wait for indexes to come online (no-op; synchronous).",
        ),
        (
            "db.awaitIndex",
            "READ",
            "Wait for a named index to come online (no-op).",
        ),
        ("db.ping", "READ", "Check connectivity to the database."),
        (
            "db.index.vector.queryNodes",
            "READ",
            "k-nearest-neighbour search on a vector index.",
        ),
        (
            "db.index.fulltext.queryNodes",
            "READ",
            "BM25 full-text search on a fulltext index.",
        ),
        (
            "dbms.components",
            "DBMS",
            "DBMS name, versions and edition.",
        ),
    ];
    out.sort_by(|a, b| a.0.cmp(b.0));
    out
}

/// The rows of a `SHOW` listing, values in the listing's column order.
fn show_rows(op: ShowOp, graph: &dyn GraphView) -> Vec<Vec<CypherValue>> {
    let s = |v: String| CypherValue::String(v);
    match op {
        ShowOp::Indexes => index_rows(graph)
            .into_iter()
            .map(|r| r.into_iter().map(s).collect())
            .collect(),
        ShowOp::Constraints => constraint_rows(graph)
            .into_iter()
            .map(|r| r.into_iter().map(s).collect())
            .collect(),
        ShowOp::Functions => function_catalog()
            .into_iter()
            .map(|(name, category, aggregating)| {
                vec![
                    s(name.to_string()),
                    s(category.to_string()),
                    s(format!("{category} function `{name}`.")),
                    s(format!("{name}(...)")),
                    CypherValue::Bool(aggregating),
                ]
            })
            .collect(),
        ShowOp::Procedures => procedure_catalog()
            .into_iter()
            .map(|(name, mode, description)| {
                vec![
                    s(name.to_string()),
                    s(description.to_string()),
                    s(format!("{name}(...)")),
                    s(mode.to_string()),
                ]
            })
            .collect(),
        ShowOp::Settings => crate::settings::all_settings()
            .into_iter()
            .map(|st| {
                vec![
                    s(st.name),
                    s(st.value),
                    CypherValue::Bool(st.is_dynamic),
                    s(st.default_value),
                    s(st.description),
                ]
            })
            .collect(),
    }
}

/// A `SHOW` listing.
pub struct ShowOpExec<'p> {
    op: ShowOp,
    cols: &'p [DataPos],
    out_group: GroupId,
    staged: Option<Staged>,
}

impl<'p> ShowOpExec<'p> {
    pub(crate) fn new(op: ShowOp, cols: &'p [DataPos], out_group: GroupId) -> Self {
        Self {
            op,
            cols,
            out_group,
            staged: None,
        }
    }
}

impl PhysOperator for ShowOpExec<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.staged.is_none() {
            let width = rs.group(self.out_group).cols.len();
            let mut st = Staged::new(width);
            for row in show_rows(self.op, cx.view()) {
                st.push_row(|cols| {
                    for (p, v) in self.cols.iter().zip(row) {
                        cols[p.col as usize].push_owned(v);
                    }
                })?;
            }
            self.staged = Some(st);
        }
        self.staged.as_mut().unwrap().emit(rs, self.out_group)
    }

    fn reset(&mut self) {
        self.staged = None;
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// Transaction ids from a STRING or LIST<STRING> (null: none).
fn transaction_ids(value: CypherValue) -> Result<Vec<String>, CypherError> {
    let wrong = |v: &CypherValue| {
        CypherError::type_error(format!(
            "transaction ids must be a STRING or LIST<STRING>, got {}",
            v.type_name()
        ))
    };
    match value {
        CypherValue::Null => Ok(Vec::new()),
        CypherValue::String(s) => Ok(vec![s]),
        CypherValue::List(items) => items
            .into_iter()
            .map(|item| match item {
                CypherValue::String(s) => Ok(s),
                other => Err(wrong(&other)),
            })
            .collect(),
        other => Err(wrong(&other)),
    }
}

/// `SHOW TRANSACTIONS` / `TERMINATE TRANSACTIONS` once per input row.
pub struct TransactionCommandOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    command: TransactionCommandKind,
    ids: Option<&'p CompiledExpr>,
    cols: &'p [DataPos],
    in_groups: &'p [GroupId],
    out_group: GroupId,
    moves: Vec<(DataPos, u16)>,
    scratch: EvalScratch<'p>,
    staged: Option<Staged>,
    done: bool,
}

impl<'p> TransactionCommandOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        command: TransactionCommandKind,
        ids: Option<&'p CompiledExpr>,
        cols: &'p [DataPos],
        in_groups: &'p [GroupId],
        out_group: GroupId,
        moves: &'p [(DataPos, DataPos)],
    ) -> Self {
        Self {
            input,
            command,
            ids,
            cols,
            in_groups,
            out_group,
            moves: move_cols(moves),
            scratch: EvalScratch::new(),
            staged: None,
            done: false,
        }
    }
}

impl PhysOperator for TransactionCommandOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if let Some(st) = &mut self.staged {
                if st.emit(rs, self.out_group)? {
                    return Ok(true);
                }
                self.staged = None;
            }
            if self.done || !self.input.next(rs, cx)? {
                self.done = true;
                return Ok(false);
            }
            let width = rs.group(self.out_group).cols.len();
            let mut st = Staged::new(width);
            let total = rs.row_count(self.in_groups);
            let mut lanes = Vec::new();
            for row in 0..total {
                rs.row_lanes(self.in_groups, row, &mut lanes);
                if row & 0xFFF == 0xFFF {
                    cx.check()?;
                }
                let ids = match self.ids {
                    Some(e) => Some(transaction_ids(e.eval_row(
                        rs,
                        cx.view(),
                        &lanes,
                        &mut self.scratch,
                    )?)?),
                    None => None,
                };
                // the engine's entry points always enter a query; a bare operator test may not
                let caller = crate::query::current().unwrap_or_else(|| {
                    crate::query::QueryContext::new(crate::query::QueryOptions::default())
                });
                let values: Vec<Vec<CypherValue>> = match self.command {
                    TransactionCommandKind::Show => crate::transactions::list(&caller)
                        .iter()
                        .filter(|s| {
                            ids.as_ref()
                                .is_none_or(|ids| ids.contains(&s.transaction_id))
                        })
                        .map(crate::transactions::row)
                        .collect(),
                    TransactionCommandKind::Terminate => ids
                        .unwrap_or_default()
                        .iter()
                        .map(|id| crate::transactions::terminate_row(id, caller.principal()))
                        .collect(),
                };
                for vals in values {
                    st.push_row(|cols| {
                        for (src, c) in &self.moves {
                            cols[*c as usize].push(rs.value_at(*src, &lanes));
                        }
                        for (p, v) in self.cols.iter().zip(vals) {
                            cols[p.col as usize].push_owned(v);
                        }
                    })?;
                }
            }
            cx.tick()?;
            self.staged = Some(st);
        }
    }

    fn reset(&mut self) {
        self.staged = None;
        self.done = false;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}
