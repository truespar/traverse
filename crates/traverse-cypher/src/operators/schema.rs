//! Schema/DDL operators: indexes, constraints, ANALYZE GRAPH and the SHOW commands.

use traverse_core::graph::constraint::{ConstraintEntity, ConstraintType};
use traverse_core::tx::view::GraphView;

use crate::ast::{Expr, TransactionCommandKind};
use crate::error::CypherError;
use crate::executor::{eval_expr, inc_stat};
use crate::frame::{Frame, SlotValue, SymbolTable, new_frame};
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

use super::Operator;

/// String -> SlotValue.
fn sv_str(s: String) -> SlotValue {
    SlotValue::Heap(Box::new(CypherValue::String(s)))
}

// -- CreateIndexOp -----------------------------------------------------

pub(crate) struct CreateIndexOp<'p> {
    plan: &'p LogicalPlan,
    done: bool,
}

impl<'p> CreateIndexOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self { plan, done: false }
    }
}

impl<'p> Operator for CreateIndexOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        _symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if !self.done {
            self.done = true;
            let LogicalPlan::CreateIndex {
                name,
                label,
                property,
                extra_properties,
            } = self.plan
            else {
                unreachable!();
            };
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
        Ok(None)
    }
}

/// Resolves a DDL name that may be a `$parameter` marker (`\u{1}param\u{1}<name>` from
/// `Parser::parse_ddl_name`, Cypher 25 dynamic DDL names) against the query parameters.
pub(crate) fn resolve_ddl_name(name: &str) -> Result<String, CypherError> {
    let Some(pname) = name.strip_prefix("\u{1}param\u{1}") else {
        return Ok(name.to_string());
    };
    match crate::executor::get_query_param(pname) {
        Some(crate::types::CypherValue::String(s)) if !s.is_empty() => Ok(s),
        Some(other) => Err(CypherError::type_error(format!(
            "dynamic DDL name parameter ${pname} must be a non-empty STRING, got {}",
            other.type_name()
        ))),
        None => Err(CypherError::semantic(format!(
            "expected parameter ${pname} for the dynamic DDL name"
        ))),
    }
}

// -- CreateVectorIndexOp -----------------------------------------------

pub(crate) struct CreateVectorIndexOp<'p> {
    plan: &'p LogicalPlan,
    done: bool,
}

impl<'p> CreateVectorIndexOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self { plan, done: false }
    }
}

impl<'p> Operator for CreateVectorIndexOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        _symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if !self.done {
            self.done = true;
            let LogicalPlan::CreateVectorIndex {
                name,
                label,
                property,
                dimensions,
                similarity,
                quantization,
            } = self.plan
            else {
                unreachable!();
            };
            let sim = traverse_core::graph::vector::Similarity::parse(similarity)
                .ok_or_else(|| {
                    CypherError::semantic(format!(
                        "unknown vector.similarity_function '{similarity}' - expected 'cosine' or 'euclidean'"
                    ))
                })?;
            // dimensionality: the declared one, else the first indexed node's vector (Neo4j 5.23+
            // allows omitting it)
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
        Ok(None)
    }
}

/// Vector index dimensionality from the first node with the label and a numeric-list property.
fn infer_dimension(graph: &dyn GraphView, label: &str, property: &str) -> Option<usize> {
    for nid in graph.nodes_by_label(label) {
        if let Ok(Some(traverse_core::graph::property::PropertyValue::List(items))) =
            graph.get_node_property(nid, property)
            && !items.is_empty()
            && items.iter().all(|v| {
                matches!(
                    v,
                    traverse_core::graph::property::PropertyValue::Float(_)
                        | traverse_core::graph::property::PropertyValue::Int(_)
                )
            })
        {
            return Some(items.len());
        }
    }
    None
}

// -- DropIndexOp -------------------------------------------------------

pub(crate) struct DropIndexOp<'p> {
    plan: &'p LogicalPlan,
    done: bool,
}

impl<'p> DropIndexOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self { plan, done: false }
    }
}

impl<'p> Operator for DropIndexOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        _symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if !self.done {
            self.done = true;
            let LogicalPlan::DropIndex { name } = self.plan else {
                unreachable!();
            };
            // DROP INDEX covers property, vector, full-text and point indexes; dispatch by which
            // registry owns the name
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
        Ok(None)
    }
}

// -- AnalyzeGraphOp ---------------------------------------------------

pub(crate) struct AnalyzeGraphOp<'p> {
    plan: &'p LogicalPlan,
    done: bool,
}

impl<'p> AnalyzeGraphOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self { plan, done: false }
    }
}

impl<'p> Operator for AnalyzeGraphOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        _symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if !self.done {
            self.done = true;
            let LogicalPlan::AnalyzeGraph { labels } = self.plan else {
                unreachable!();
            };
            graph.analyze_statistics(labels);
        }
        Ok(None)
    }
}

// -- CreateConstraintOp -----------------------------------------------

pub(crate) struct CreateConstraintOp<'p> {
    plan: &'p LogicalPlan,
    done: bool,
}

impl<'p> CreateConstraintOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self { plan, done: false }
    }
}

impl<'p> Operator for CreateConstraintOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        _symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if !self.done {
            self.done = true;
            let LogicalPlan::CreateConstraint {
                name,
                label,
                property,
                extra_properties,
                constraint_type,
                is_relationship,
                if_not_exists,
            } = self.plan
            else {
                unreachable!();
            };
            if *if_not_exists
                && graph
                    .constraint_registry()
                    .descriptors()
                    .iter()
                    .any(|d| d.name == resolve_ddl_name(name).unwrap_or_default())
            {
                return Ok(None);
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
                // composite UNIQUE / KEY: all keys, tuple semantics
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
        Ok(None)
    }
}

// -- DropConstraintOp -------------------------------------------------

pub(crate) struct DropConstraintOp<'p> {
    plan: &'p LogicalPlan,
    done: bool,
}

impl<'p> DropConstraintOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self { plan, done: false }
    }
}

impl<'p> Operator for DropConstraintOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        _symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if !self.done {
            self.done = true;
            let LogicalPlan::DropConstraint { name } = self.plan else {
                unreachable!();
            };
            graph
                .drop_constraint(&resolve_ddl_name(name)?)
                .map_err(CypherError::from)?;
            inc_stat(|s| s.constraints_removed += 1);
        }
        Ok(None)
    }
}

// -- CreateEdgeIndexOp -----------------------------------------------

pub(crate) struct CreateEdgeIndexOp<'p> {
    plan: &'p LogicalPlan,
    done: bool,
}

impl<'p> CreateEdgeIndexOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self { plan, done: false }
    }
}

impl<'p> Operator for CreateEdgeIndexOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        _symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if !self.done {
            self.done = true;
            let LogicalPlan::CreateEdgeIndex {
                name,
                edge_type,
                property,
                extra_properties,
            } = self.plan
            else {
                unreachable!();
            };
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
        Ok(None)
    }
}

// -- DropEdgeIndexOp -------------------------------------------------

pub(crate) struct DropEdgeIndexOp<'p> {
    plan: &'p LogicalPlan,
    done: bool,
}

impl<'p> DropEdgeIndexOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self { plan, done: false }
    }
}

impl<'p> Operator for DropEdgeIndexOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        _symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if !self.done {
            self.done = true;
            let LogicalPlan::DropEdgeIndex { name } = self.plan else {
                unreachable!();
            };
            graph
                .drop_edge_index(&resolve_ddl_name(name)?)
                .map_err(CypherError::from)?;
            inc_stat(|s| s.indexes_removed += 1);
        }
        Ok(None)
    }
}

// -- ShowIndexesOp --------------------------------------------------

pub(crate) struct ShowIndexesOp {
    rows: Vec<Frame>,
    pos: usize,
}

impl ShowIndexesOp {
    pub(crate) fn new() -> Self {
        Self {
            rows: Vec::new(),
            pos: 0,
        }
    }
}

impl Operator for ShowIndexesOp {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.rows.is_empty() && self.pos == 0 {
            let strings = graph.strings();

            // node property indexes
            for idx in graph.index_catalog().descriptors() {
                let label = strings
                    .try_resolve(idx.label_id.0)
                    .unwrap_or("?")
                    .to_string();
                let prop = strings
                    .try_resolve(idx.property_key.0)
                    .unwrap_or("?")
                    .to_string();
                let mut frame = new_frame(symtab);
                if let Some(s) = symtab.slot("name") {
                    frame[s] = sv_str(idx.name.clone());
                }
                if let Some(s) = symtab.slot("type") {
                    frame[s] = sv_str("RANGE".to_string());
                }
                if let Some(s) = symtab.slot("entityType") {
                    frame[s] = sv_str("NODE".to_string());
                }
                if let Some(s) = symtab.slot("labelsOrTypes") {
                    frame[s] = sv_str(label);
                }
                // composite indexes store a synthetic joined key: show the components
                // comma-separated, like Neo4j
                if let Some(s) = symtab.slot("properties") {
                    frame[s] = sv_str(prop.replace('\u{1F}', ","));
                }
                self.rows.push(frame);
            }

            // edge indexes
            let edge_descs = graph.edge_index_catalog().descriptors();
            for desc in edge_descs {
                let etype = strings
                    .try_resolve(desc.edge_type_id.0)
                    .unwrap_or("?")
                    .to_string();
                let prop = desc
                    .property_key
                    .and_then(|pk| strings.try_resolve(pk.0))
                    .unwrap_or("-")
                    .to_string();
                let idx_type = if desc.property_key.is_some() {
                    "RANGE"
                } else {
                    "LOOKUP"
                };
                let mut frame = new_frame(symtab);
                if let Some(s) = symtab.slot("name") {
                    frame[s] = sv_str(desc.name.clone());
                }
                if let Some(s) = symtab.slot("type") {
                    frame[s] = sv_str(idx_type.to_string());
                }
                if let Some(s) = symtab.slot("entityType") {
                    frame[s] = sv_str("RELATIONSHIP".to_string());
                }
                if let Some(s) = symtab.slot("labelsOrTypes") {
                    frame[s] = sv_str(etype);
                }
                if let Some(s) = symtab.slot("properties") {
                    frame[s] = sv_str(prop.replace('\u{1F}', ","));
                }
                self.rows.push(frame);
            }

            // vector (HNSW) indexes: type "VECTOR" as in Neo4j
            for (name, label, prop, _dim, _sim) in graph.vector_index_descriptors() {
                let mut frame = new_frame(symtab);
                if let Some(s) = symtab.slot("name") {
                    frame[s] = sv_str(name);
                }
                if let Some(s) = symtab.slot("type") {
                    frame[s] = sv_str("VECTOR".to_string());
                }
                if let Some(s) = symtab.slot("entityType") {
                    frame[s] = sv_str("NODE".to_string());
                }
                if let Some(s) = symtab.slot("labelsOrTypes") {
                    frame[s] = sv_str(label);
                }
                if let Some(s) = symtab.slot("properties") {
                    frame[s] = sv_str(prop);
                }
                self.rows.push(frame);
            }

            // full-text (BM25) indexes: type "FULLTEXT"; may span several labels/properties,
            // comma-joined
            for (name, labels, props, _analyzer) in graph.fulltext_index_descriptors() {
                let mut frame = new_frame(symtab);
                if let Some(s) = symtab.slot("name") {
                    frame[s] = sv_str(name);
                }
                if let Some(s) = symtab.slot("type") {
                    frame[s] = sv_str("FULLTEXT".to_string());
                }
                if let Some(s) = symtab.slot("entityType") {
                    frame[s] = sv_str("NODE".to_string());
                }
                if let Some(s) = symtab.slot("labelsOrTypes") {
                    frame[s] = sv_str(labels.join(","));
                }
                if let Some(s) = symtab.slot("properties") {
                    frame[s] = sv_str(props.join(","));
                }
                self.rows.push(frame);
            }

            // point (R-tree) indexes: type "POINT"
            for (name, label, prop) in graph.point_index_descriptors() {
                let mut frame = new_frame(symtab);
                if let Some(s) = symtab.slot("name") {
                    frame[s] = sv_str(name);
                }
                if let Some(s) = symtab.slot("type") {
                    frame[s] = sv_str("POINT".to_string());
                }
                if let Some(s) = symtab.slot("entityType") {
                    frame[s] = sv_str("NODE".to_string());
                }
                if let Some(s) = symtab.slot("labelsOrTypes") {
                    frame[s] = sv_str(label);
                }
                if let Some(s) = symtab.slot("properties") {
                    frame[s] = sv_str(prop);
                }
                self.rows.push(frame);
            }

            // text (trigram) indexes: type "TEXT"
            for (name, label, prop) in graph.text_index_descriptors() {
                let mut frame = new_frame(symtab);
                if let Some(s) = symtab.slot("name") {
                    frame[s] = sv_str(name);
                }
                if let Some(s) = symtab.slot("type") {
                    frame[s] = sv_str("TEXT".to_string());
                }
                if let Some(s) = symtab.slot("entityType") {
                    frame[s] = sv_str("NODE".to_string());
                }
                if let Some(s) = symtab.slot("labelsOrTypes") {
                    frame[s] = sv_str(label);
                }
                if let Some(s) = symtab.slot("properties") {
                    frame[s] = sv_str(prop);
                }
                self.rows.push(frame);
            }
        }

        if self.pos < self.rows.len() {
            let frame = self.rows[self.pos].clone();
            self.pos += 1;
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

// -- ShowConstraintsOp ----------------------------------------------

pub(crate) struct ShowConstraintsOp {
    rows: Vec<Frame>,
    pos: usize,
}

impl ShowConstraintsOp {
    pub(crate) fn new() -> Self {
        Self {
            rows: Vec::new(),
            pos: 0,
        }
    }
}

impl Operator for ShowConstraintsOp {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.rows.is_empty() && self.pos == 0 {
            let strings = graph.strings();
            for desc in graph.constraint_registry().descriptors() {
                let label = strings
                    .try_resolve(desc.label_id.0)
                    .unwrap_or("?")
                    .to_string();
                let mut prop = strings
                    .try_resolve(desc.property_key.0)
                    .unwrap_or("?")
                    .to_string();
                // composite constraints list every key
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
                let entity_str = if is_rel { "RELATIONSHIP" } else { "NODE" };
                let mut frame = new_frame(symtab);
                if let Some(s) = symtab.slot("name") {
                    frame[s] = sv_str(desc.name.clone());
                }
                if let Some(s) = symtab.slot("type") {
                    frame[s] = sv_str(ctype);
                }
                if let Some(s) = symtab.slot("entityType") {
                    frame[s] = sv_str(entity_str.to_string());
                }
                if let Some(s) = symtab.slot("labelsOrTypes") {
                    frame[s] = sv_str(label);
                }
                if let Some(s) = symtab.slot("properties") {
                    frame[s] = sv_str(prop);
                }
                self.rows.push(frame);
            }
        }

        if self.pos < self.rows.len() {
            let frame = self.rows[self.pos].clone();
            self.pos += 1;
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

// -- SHOW FUNCTIONS / SHOW PROCEDURES catalog -----------------------------
// Curated catalog of known functions/procedures for `SHOW FUNCTIONS` /
// `SHOW PROCEDURES` (driver/Studio autocomplete). Not exhaustive, grouped by
// category, simplified signatures.

/// (name, category, aggregating) for the function catalog.
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
    for n in numeric {
        out.push((n, "Numeric", false));
    }
    for n in string {
        out.push((n, "String", false));
    }
    for n in list {
        out.push((n, "List", false));
    }
    for n in scalar {
        out.push((n, "Scalar", false));
    }
    for n in temporal {
        out.push((n, "Temporal", false));
    }
    for n in spatial {
        out.push((n, "Spatial", false));
    }
    for n in vector {
        out.push((n, "Vector", false));
    }
    for n in predicate {
        out.push((n, "Predicate", false));
    }
    for n in aggregating {
        out.push((n, "Aggregating", true));
    }
    out.sort_by(|a, b| a.0.cmp(b.0));
    out
}

/// (name, mode, description) for the procedure catalog.
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

// -- ShowFunctionsOp ------------------------------------------------------

pub(crate) struct ShowFunctionsOp {
    rows: Vec<Frame>,
    pos: usize,
}

impl ShowFunctionsOp {
    pub(crate) fn new() -> Self {
        Self {
            rows: Vec::new(),
            pos: 0,
        }
    }
}

impl Operator for ShowFunctionsOp {
    fn next(
        &mut self,
        _graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.rows.is_empty() && self.pos == 0 {
            for (name, category, aggregating) in function_catalog() {
                let mut frame = new_frame(symtab);
                if let Some(s) = symtab.slot("name") {
                    frame[s] = sv_str(name.to_string());
                }
                if let Some(s) = symtab.slot("category") {
                    frame[s] = sv_str(category.to_string());
                }
                if let Some(s) = symtab.slot("description") {
                    frame[s] = sv_str(format!("{category} function `{name}`."));
                }
                if let Some(s) = symtab.slot("signature") {
                    frame[s] = sv_str(format!("{name}(...)"));
                }
                if let Some(s) = symtab.slot("aggregating") {
                    frame[s] = SlotValue::from(crate::types::CypherValue::Bool(aggregating));
                }
                self.rows.push(frame);
            }
        }
        if self.pos < self.rows.len() {
            let frame = self.rows[self.pos].clone();
            self.pos += 1;
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

// -- ShowProceduresOp -----------------------------------------------------

pub(crate) struct ShowProceduresOp {
    rows: Vec<Frame>,
    pos: usize,
}

impl ShowProceduresOp {
    pub(crate) fn new() -> Self {
        Self {
            rows: Vec::new(),
            pos: 0,
        }
    }
}

impl Operator for ShowProceduresOp {
    fn next(
        &mut self,
        _graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.rows.is_empty() && self.pos == 0 {
            for (name, mode, description) in procedure_catalog() {
                let mut frame = new_frame(symtab);
                if let Some(s) = symtab.slot("name") {
                    frame[s] = sv_str(name.to_string());
                }
                if let Some(s) = symtab.slot("description") {
                    frame[s] = sv_str(description.to_string());
                }
                if let Some(s) = symtab.slot("mode") {
                    frame[s] = sv_str(mode.to_string());
                }
                if let Some(s) = symtab.slot("signature") {
                    frame[s] = sv_str(format!("{name}(...)"));
                }
                self.rows.push(frame);
            }
        }
        if self.pos < self.rows.len() {
            let frame = self.rows[self.pos].clone();
            self.pos += 1;
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

// -- TransactionCommandOp -------------------------------------------------

/// `SHOW TRANSACTIONS` / `TERMINATE TRANSACTIONS` once per input row (see
/// [`LogicalPlan::TransactionCommand`]); the registry is [`crate::transactions`].
pub(crate) struct TransactionCommandOp<'p> {
    input: Box<dyn Operator + 'p>,
    command: TransactionCommandKind,
    ids: Option<Expr>,
    pending: std::vec::IntoIter<Frame>,
}

impl<'p> TransactionCommandOp<'p> {
    pub(crate) fn new(
        input: Box<dyn Operator + 'p>,
        command: TransactionCommandKind,
        ids: Option<Expr>,
    ) -> Self {
        Self {
            input,
            command,
            ids,
            pending: Vec::new().into_iter(),
        }
    }
}

impl Operator for TransactionCommandOp<'_> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        loop {
            if let Some(frame) = self.pending.next() {
                return Ok(Some(frame));
            }
            let Some(row) = self.input.next(graph, symtab)? else {
                return Ok(None);
            };
            let ids = match &self.ids {
                Some(expr) => Some(transaction_ids(eval_expr(expr, &row, &*graph, symtab)?)?),
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
            let columns = self.command.columns();
            let frames: Vec<Frame> = values
                .into_iter()
                .map(|vals| {
                    let mut frame = row.clone();
                    for (column, value) in columns.iter().zip(vals) {
                        if let Some(slot) = symtab.slot(column) {
                            frame[slot] = SlotValue::from(value);
                        }
                    }
                    frame
                })
                .collect();
            self.pending = frames.into_iter();
        }
    }
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

// -- ShowSettingsOp -------------------------------------------------------

/// `SHOW SETTINGS` - lists the process settings registry (populated by the
/// hosting server at startup, secrets pre-redacted) plus engine built-ins.
pub(crate) struct ShowSettingsOp {
    rows: Vec<Frame>,
    pos: usize,
    loaded: bool,
}

impl ShowSettingsOp {
    pub(crate) fn new() -> Self {
        Self {
            rows: Vec::new(),
            pos: 0,
            loaded: false,
        }
    }
}

impl Operator for ShowSettingsOp {
    fn next(
        &mut self,
        _graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if !self.loaded {
            self.loaded = true;
            for s in crate::settings::all_settings() {
                let mut frame = new_frame(symtab);
                if let Some(sl) = symtab.slot("name") {
                    frame[sl] = sv_str(s.name);
                }
                if let Some(sl) = symtab.slot("value") {
                    frame[sl] = sv_str(s.value);
                }
                if let Some(sl) = symtab.slot("isDynamic") {
                    frame[sl] = SlotValue::Bool(s.is_dynamic);
                }
                if let Some(sl) = symtab.slot("defaultValue") {
                    frame[sl] = sv_str(s.default_value);
                }
                if let Some(sl) = symtab.slot("description") {
                    frame[sl] = sv_str(s.description);
                }
                self.rows.push(frame);
            }
        }
        if self.pos < self.rows.len() {
            let frame = std::mem::take(&mut self.rows[self.pos]);
            self.pos += 1;
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

// -- CreateFulltextIndexOp ------------------------------------------------

pub(crate) struct CreateFulltextIndexOp<'p> {
    plan: &'p LogicalPlan,
    done: bool,
}

impl<'p> CreateFulltextIndexOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self { plan, done: false }
    }
}

impl<'p> Operator for CreateFulltextIndexOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        _symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if !self.done {
            self.done = true;
            let LogicalPlan::CreateFulltextIndex {
                name,
                labels,
                properties,
                analyzer,
            } = self.plan
            else {
                unreachable!();
            };
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
        Ok(None)
    }
}

// -- CreatePointIndexOp ---------------------------------------------------

pub(crate) struct CreatePointIndexOp<'p> {
    plan: &'p LogicalPlan,
    done: bool,
}

impl<'p> CreatePointIndexOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self { plan, done: false }
    }
}

impl<'p> Operator for CreatePointIndexOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        _symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if !self.done {
            self.done = true;
            let LogicalPlan::CreatePointIndex {
                name,
                label,
                property,
            } = self.plan
            else {
                unreachable!();
            };
            graph
                .create_point_index(&resolve_ddl_name(name)?, label, property)
                .map_err(CypherError::from)?;
            inc_stat(|s| s.indexes_added += 1);
        }
        Ok(None)
    }
}

// -- CreateTextIndexOp ------------------------------------------------

pub(crate) struct CreateTextIndexOp<'p> {
    plan: &'p LogicalPlan,
    done: bool,
}

impl<'p> CreateTextIndexOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self { plan, done: false }
    }
}

impl<'p> Operator for CreateTextIndexOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        _symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if !self.done {
            self.done = true;
            let LogicalPlan::CreateTextIndex {
                name,
                label,
                property,
            } = self.plan
            else {
                unreachable!();
            };
            graph
                .create_text_index(&resolve_ddl_name(name)?, label, property)
                .map_err(CypherError::from)?;
            inc_stat(|s| s.indexes_added += 1);
        }
        Ok(None)
    }
}
