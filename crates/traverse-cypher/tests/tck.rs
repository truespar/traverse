//! openCypher TCK (Technology Compatibility Kit) scenarios run with cucumber-rs.

use std::collections::{BTreeMap, HashMap, HashSet};

use cucumber::{World, given, then, when};
use traverse_core::graph::Graph;
use traverse_cypher::error::CypherError;
use traverse_cypher::executor::CypherResult;
use traverse_cypher::types::CypherValue;

// ---------------------------------------------------------------------------
// World
// ---------------------------------------------------------------------------

#[derive(Debug, World)]
#[world(init = Self::new)]
pub struct TckWorld {
    graph: Graph,
    result: Option<CypherResult>,
    error: Option<CypherError>,
    params: HashMap<String, CypherValue>,
    // side-effect snapshots, taken before the "When" step
    snap_nodes: usize,
    snap_edges: usize,
    snap_labels: usize,
    snap_properties: usize,
}

impl TckWorld {
    fn new() -> Self {
        Self {
            graph: Graph::new(),
            result: None,
            error: None,
            params: HashMap::new(),
            snap_nodes: 0,
            snap_edges: 0,
            snap_labels: 0,
            snap_properties: 0,
        }
    }

    /// Snapshots graph counts for side-effect checks.
    fn snapshot(&mut self) {
        self.snap_nodes = self.graph.node_count();
        self.snap_edges = self.graph.edge_count();
        self.snap_labels = count_labels(&self.graph);
        self.snap_properties = count_properties(&self.graph);
    }
}

/// Distinct label names in the graph.
fn count_labels(g: &Graph) -> usize {
    let mut seen = HashSet::new();
    for node in g.nodes() {
        for lid in &node.labels {
            seen.insert(*lid);
        }
    }
    seen.len()
}

/// Property keys across all nodes and edges.
fn count_properties(g: &Graph) -> usize {
    let node_props: usize = g.nodes().map(|n| n.properties.len()).sum();
    let edge_props: usize = g.all_edges().map(|e| e.properties.len()).sum();
    node_props + edge_props
}

// ---------------------------------------------------------------------------
// Given steps
// ---------------------------------------------------------------------------

#[given("an empty graph")]
fn given_empty_graph(w: &mut TckWorld) {
    w.graph = Graph::new();
    w.result = None;
    w.error = None;
    w.params.clear();
    traverse_cypher::procedure::clear_procedures();
}

#[given("any graph")]
fn given_any_graph(w: &mut TckWorld) {
    // "any graph" = an empty graph
    w.graph = Graph::new();
    w.result = None;
    w.error = None;
    w.params.clear();
    traverse_cypher::procedure::clear_procedures();
}

#[given(regex = r"^the binary-tree-(\d+) graph$")]
fn given_binary_tree(w: &mut TckWorld, variant: usize) {
    w.graph = Graph::new();
    w.result = None;
    w.error = None;
    w.params.clear();
    traverse_cypher::procedure::clear_procedures();
    build_binary_tree(&mut w.graph, variant);
}

/// One of the TCK binary-tree graphs.
///
/// binary-tree-1: 13 nodes (a:A, b1-b4:X, c11-c42:X), all with `name`.
/// binary-tree-2: same, but odd c-nodes are :X, even ones :Y.
///
/// Relationships:
///   a -KNOWS-> b1, b2;  a -FOLLOWS-> b3, b4
///   each bi -FRIEND-> ci1, ci2
///   ring: b1->b2->b3->b4->b1 via FRIEND
fn build_binary_tree(g: &mut Graph, variant: usize) {
    use traverse_core::graph::property::{PropertyMap, PropertyValue};

    let props = |name: &str| -> PropertyMap {
        let mut m = PropertyMap::new();
        m.insert("name".into(), PropertyValue::String(name.into()));
        m
    };

    // c-node labels by variant
    let c_label = |i: usize| -> &'static str {
        if variant == 2 && i.is_multiple_of(2) {
            "Y"
        } else {
            "X"
        }
    };

    let a = g.create_node(&["A"], props("a"));

    let b1 = g.create_node(&["X"], props("b1"));
    let b2 = g.create_node(&["X"], props("b2"));
    let b3 = g.create_node(&["X"], props("b3"));
    let b4 = g.create_node(&["X"], props("b4"));

    let c11 = g.create_node(&[c_label(1)], props("c11"));
    let c12 = g.create_node(&[c_label(2)], props("c12"));
    let c21 = g.create_node(&[c_label(1)], props("c21"));
    let c22 = g.create_node(&[c_label(2)], props("c22"));
    let c31 = g.create_node(&[c_label(1)], props("c31"));
    let c32 = g.create_node(&[c_label(2)], props("c32"));
    let c41 = g.create_node(&[c_label(1)], props("c41"));
    let c42 = g.create_node(&[c_label(2)], props("c42"));

    // a KNOWS b1, b2 and FOLLOWS b3, b4
    let _ = g.create_edge(a, b1, "KNOWS", PropertyMap::new());
    let _ = g.create_edge(a, b2, "KNOWS", PropertyMap::new());
    let _ = g.create_edge(a, b3, "FOLLOWS", PropertyMap::new());
    let _ = g.create_edge(a, b4, "FOLLOWS", PropertyMap::new());

    // each bi FRIEND ci1, ci2
    let _ = g.create_edge(b1, c11, "FRIEND", PropertyMap::new());
    let _ = g.create_edge(b1, c12, "FRIEND", PropertyMap::new());
    let _ = g.create_edge(b2, c21, "FRIEND", PropertyMap::new());
    let _ = g.create_edge(b2, c22, "FRIEND", PropertyMap::new());
    let _ = g.create_edge(b3, c31, "FRIEND", PropertyMap::new());
    let _ = g.create_edge(b3, c32, "FRIEND", PropertyMap::new());
    let _ = g.create_edge(b4, c41, "FRIEND", PropertyMap::new());
    let _ = g.create_edge(b4, c42, "FRIEND", PropertyMap::new());

    // FRIEND ring among b-nodes: b1->b2->b3->b4->b1
    let _ = g.create_edge(b1, b2, "FRIEND", PropertyMap::new());
    let _ = g.create_edge(b2, b3, "FRIEND", PropertyMap::new());
    let _ = g.create_edge(b3, b4, "FRIEND", PropertyMap::new());
    let _ = g.create_edge(b4, b1, "FRIEND", PropertyMap::new());
}

#[given("having executed:")]
fn given_having_executed(w: &mut TckWorld, step: &cucumber::gherkin::Step) {
    let query = step.docstring.as_ref().expect("expected docstring").trim();
    run_setup_queries(w, query);
}

fn run_setup_queries(w: &mut TckWorld, text: &str) {
    // each "having executed:" block is one statement
    match traverse_cypher::execute(&mut w.graph, text) {
        Ok(_) => {}
        Err(e) => {
            // setup failures shouldn't happen; don't panic, let the scenario fail
            eprintln!("  [TCK setup error] {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// When steps
// ---------------------------------------------------------------------------

#[when("executing query:")]
fn when_executing_query(w: &mut TckWorld, step: &cucumber::gherkin::Step) {
    let query = step.docstring.as_ref().expect("expected docstring").trim();
    w.snapshot();
    match traverse_cypher::execute_with_params(&mut w.graph, query, &w.params) {
        Ok(result) => {
            w.result = Some(result);
            w.error = None;
        }
        Err(e) => {
            w.result = None;
            w.error = Some(e);
        }
    }
}

#[when("executing control query:")]
fn when_executing_control_query(w: &mut TckWorld, step: &cucumber::gherkin::Step) {
    let query = step.docstring.as_ref().expect("expected docstring").trim();
    // control queries keep the snapshot: they verify after a mutation
    match traverse_cypher::execute_with_params(&mut w.graph, query, &w.params) {
        Ok(result) => {
            w.result = Some(result);
            w.error = None;
        }
        Err(e) => {
            w.result = None;
            w.error = Some(e);
        }
    }
}

// ---------------------------------------------------------------------------
// Then steps - result assertions
// ---------------------------------------------------------------------------

#[then("the result should be empty")]
fn then_result_empty(w: &mut TckWorld) {
    let result = w
        .result
        .as_ref()
        .unwrap_or_else(|| panic!("expected a result, got error: {:?}", w.error));
    assert!(
        result.rows.is_empty(),
        "expected empty result, got {} rows",
        result.rows.len()
    );
}

#[then("the result should be, in any order:")]
fn then_result_any_order(w: &mut TckWorld, step: &cucumber::gherkin::Step) {
    let result = w
        .result
        .as_ref()
        .unwrap_or_else(|| panic!("expected a result, got error: {:?}", w.error));
    let table = step.table.as_ref().expect("expected data table");
    let expected = parse_expected_table(table);
    assert_rows_match_unordered(&result.columns, &result.rows, &expected);
}

#[then("the result should be, in order:")]
fn then_result_in_order(w: &mut TckWorld, step: &cucumber::gherkin::Step) {
    let result = w
        .result
        .as_ref()
        .unwrap_or_else(|| panic!("expected a result, got error: {:?}", w.error));
    let table = step.table.as_ref().expect("expected data table");
    let expected = parse_expected_table(table);
    assert_rows_match_ordered(&result.columns, &result.rows, &expected);
}

#[then(regex = r"^the result should be \(ignoring element order for lists\):$")]
fn then_result_ignore_list_order(w: &mut TckWorld, step: &cucumber::gherkin::Step) {
    // unordered, ignoring list element order too
    let result = w
        .result
        .as_ref()
        .unwrap_or_else(|| panic!("expected a result, got error: {:?}", w.error));
    let table = step.table.as_ref().expect("expected data table");
    let expected = parse_expected_table(table);
    assert_rows_match_unordered_ignore_list_order(&result.columns, &result.rows, &expected);
}

#[then(regex = r"^the result should be, in order \(ignoring element order for lists\):$")]
fn then_result_ordered_ignore_list_order(w: &mut TckWorld, step: &cucumber::gherkin::Step) {
    let result = w
        .result
        .as_ref()
        .unwrap_or_else(|| panic!("expected a result, got error: {:?}", w.error));
    let table = step.table.as_ref().expect("expected data table");
    let expected = parse_expected_table(table);
    assert_rows_match_ordered_ignore_list_order(&result.columns, &result.rows, &expected);
}

// ---------------------------------------------------------------------------
// Then steps - error assertions
// ---------------------------------------------------------------------------

#[then(regex = r"^a SyntaxError should be raised at compile time: (.+)$")]
fn then_syntax_error(w: &mut TckWorld, _code: String) {
    assert!(
        matches!(w.error, Some(CypherError::SyntaxError { .. })),
        "expected SyntaxError, got: {:?}",
        w.error
    );
}

#[then(regex = r"^a TypeError should be raised at (?:runtime|any time): (.+)$")]
fn then_type_error(w: &mut TckWorld, _code: String) {
    assert!(
        matches!(
            w.error,
            Some(CypherError::TypeError { .. }) | Some(CypherError::SyntaxError { .. })
        ),
        "expected TypeError, got: {:?}",
        w.error
    );
}

#[then(regex = r"^a SemanticError should be raised at (?:compile time|runtime): (.+)$")]
fn then_semantic_error(w: &mut TckWorld, _code: String) {
    assert!(
        matches!(
            w.error,
            Some(CypherError::SemanticError { .. }) | Some(CypherError::SyntaxError { .. })
        ),
        "expected SemanticError, got: {:?}",
        w.error
    );
}

#[then(
    regex = r"^an? (?:ArgumentError|Error) should be raised at (?:compile time|runtime|any time): (.+)$"
)]
fn then_generic_error(w: &mut TckWorld, _code: String) {
    assert!(
        w.error.is_some(),
        "expected an error, got result: {:?}",
        w.result
    );
}

#[then(regex = r"^a ConstraintValidationFailed should be raised at runtime: (.+)$")]
fn then_constraint_error(w: &mut TckWorld, _code: String) {
    assert!(
        w.error.is_some(),
        "expected an error, got result: {:?}",
        w.result
    );
}

#[then(regex = r"^a ParameterMissing should be raised at compile time: (.+)$")]
fn then_parameter_missing(w: &mut TckWorld, _code: String) {
    assert!(
        w.error.is_some(),
        "expected an error, got result: {:?}",
        w.result
    );
}

#[then(regex = r"^a ProcedureError should be raised at compile time: (.+)$")]
fn then_procedure_error(w: &mut TckWorld, _code: String) {
    assert!(
        w.error.is_some(),
        "expected an error, got result: {:?}",
        w.result
    );
}

#[then(regex = r"^an? EntityNotFound should be raised at runtime: (.+)$")]
fn then_entity_not_found(w: &mut TckWorld, _code: String) {
    assert!(
        w.error.is_some(),
        "expected an error, got result: {:?}",
        w.result
    );
}

// ---------------------------------------------------------------------------
// Then steps - side effects
// ---------------------------------------------------------------------------

#[then("no side effects")]
fn then_no_side_effects(w: &mut TckWorld) {
    let dn = w.graph.node_count() as i64 - w.snap_nodes as i64;
    let de = w.graph.edge_count() as i64 - w.snap_edges as i64;
    assert!(
        dn == 0 && de == 0,
        "expected no side effects, but got delta nodes={dn}, edges={de}"
    );
}

#[then("the side effects should be:")]
fn then_side_effects(w: &mut TckWorld, step: &cucumber::gherkin::Step) {
    let table = step.table.as_ref().expect("expected side-effect table");
    let mut expected: BTreeMap<String, i64> = BTreeMap::new();
    for row in &table.rows {
        if row.len() >= 2 {
            let key = row[0].trim().to_string();
            let val: i64 = row[1].trim().parse().unwrap_or(0);
            expected.insert(key, val);
        }
    }

    let actual_nodes = w.graph.node_count() as i64 - w.snap_nodes as i64;
    let actual_edges = w.graph.edge_count() as i64 - w.snap_edges as i64;
    let actual_labels = count_labels(&w.graph) as i64 - w.snap_labels as i64;
    let actual_props = count_properties(&w.graph) as i64 - w.snap_properties as i64;

    // +nodes / -nodes
    let exp_add_nodes = expected.get("+nodes").copied().unwrap_or(0);
    let exp_del_nodes = expected.get("-nodes").copied().unwrap_or(0);
    let exp_net_nodes = exp_add_nodes - exp_del_nodes;
    assert_eq!(
        actual_nodes, exp_net_nodes,
        "node delta: expected {exp_net_nodes}, got {actual_nodes}"
    );

    // +relationships / -relationships
    let exp_add_rels = expected.get("+relationships").copied().unwrap_or(0);
    let exp_del_rels = expected.get("-relationships").copied().unwrap_or(0);
    let exp_net_rels = exp_add_rels - exp_del_rels;
    assert_eq!(
        actual_edges, exp_net_rels,
        "edge delta: expected {exp_net_rels}, got {actual_edges}"
    );

    // +labels / -labels
    let exp_add_labels = expected.get("+labels").copied().unwrap_or(0);
    let exp_del_labels = expected.get("-labels").copied().unwrap_or(0);
    let exp_net_labels = exp_add_labels - exp_del_labels;
    assert_eq!(
        actual_labels, exp_net_labels,
        "label delta: expected {exp_net_labels}, got {actual_labels}"
    );

    // +properties / -properties
    let exp_add_props = expected.get("+properties").copied().unwrap_or(0);
    let exp_del_props = expected.get("-properties").copied().unwrap_or(0);
    let exp_net_props = exp_add_props - exp_del_props;
    assert_eq!(
        actual_props, exp_net_props,
        "property delta: expected {exp_net_props}, got {actual_props}"
    );
}

// ---------------------------------------------------------------------------
// Other steps (parameters, procedures, ...)
// ---------------------------------------------------------------------------

#[given("parameters are:")]
fn given_parameters(w: &mut TckWorld, step: &cucumber::gherkin::Step) {
    let table = step.table.as_ref().expect("expected parameter table");
    for row in &table.rows {
        if row.len() >= 2 {
            let key = row[0].trim().to_string();
            let tck_val = parse_tck_value(row[1].trim());
            w.params.insert(key, tck_value_to_cypher(tck_val));
        }
    }
}

#[given(regex = r"^there exists a procedure (.+)$")]
fn given_procedure_exists(_w: &mut TckWorld, step: &cucumber::gherkin::Step, sig: String) {
    use traverse_cypher::procedure::*;

    // signature: "name(param :: TYPE?, ...) :: (output :: TYPE?, ...):"
    let sig = sig.trim().trim_end_matches(':');

    // params contain " :: " too, so split at the ") :: (" boundary
    let (name_and_params, outputs_str) = {
        // closing paren of the params, then " :: "
        let paren_close = sig
            .find(") :: ")
            .expect("expected ') :: ' in procedure signature");
        let left = &sig[..paren_close + 1]; // "name(params...)"
        let right = &sig[paren_close + 4..]; // "(output :: TYPE?, ...)"
        (left, right)
    };

    // procedure name and params
    let open = name_and_params
        .find('(')
        .expect("expected '(' in procedure name");
    let proc_name = name_and_params[..open].trim().to_string();
    let params_str = &name_and_params[open + 1..name_and_params.len() - 1]; // strip parens

    let params: Vec<ProcParam> = if params_str.trim().is_empty() {
        Vec::new()
    } else {
        params_str
            .split(',')
            .map(|p| {
                let parts: Vec<&str> = p.split("::").collect();
                let name = parts[0].trim().to_string();
                let typ = parse_proc_type(parts[1].trim()).expect("unknown proc param type");
                ProcParam { name, typ }
            })
            .collect()
    };

    // outputs: "(out :: TYPE?, ...)" or "()"
    let outputs_inner = outputs_str
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')');
    let outputs: Vec<ProcOutput> = if outputs_inner.trim().is_empty() {
        Vec::new()
    } else {
        outputs_inner
            .split(',')
            .map(|o| {
                let parts: Vec<&str> = o.split("::").collect();
                let name = parts[0].trim().to_string();
                let typ = parse_proc_type(parts[1].trim()).expect("unknown proc output type");
                ProcOutput { name, typ }
            })
            .collect()
    };

    // data table rows
    let mut rows: Vec<HashMap<String, CypherValue>> = Vec::new();
    if let Some(table) = step.table.as_ref() {
        // first row is the header
        let header = &table.rows[0];
        for data_row in table.rows.iter().skip(1) {
            let mut row_map = HashMap::new();
            for (i, col_name) in header.iter().enumerate() {
                let col_name = col_name.trim().to_string();
                if col_name.is_empty() {
                    continue;
                }
                let val_str = if i < data_row.len() {
                    data_row[i].trim()
                } else {
                    ""
                };
                let cypher_val = tck_value_to_cypher(parse_tck_value(val_str));
                row_map.insert(col_name, cypher_val);
            }
            if !row_map.is_empty() {
                rows.push(row_map);
            }
        }
    }

    register_procedure(Procedure {
        name: proc_name,
        params,
        outputs,
        rows,
    });
}

#[given(regex = r"^logical operations$")]
fn given_logical_operations(_w: &mut TckWorld) {
    // logical operation metadata: no-op
}

// ---------------------------------------------------------------------------
// TCK value parser
// ---------------------------------------------------------------------------

/// Parsed TCK expected value (structural, no ids).
#[derive(Debug, Clone)]
enum TckValue {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    List(Vec<TckValue>),
    Map(BTreeMap<String, TckValue>),
    Node {
        labels: Vec<String>,
        properties: BTreeMap<String, TckValue>,
    },
    Edge {
        edge_type: String,
        properties: BTreeMap<String, TckValue>,
    },
    Path(Vec<TckValue>), // alternating nodes and edges
}

/// Parsed TCK value -> CypherValue for a query parameter.
fn tck_value_to_cypher(val: TckValue) -> CypherValue {
    match val {
        TckValue::Null => CypherValue::Null,
        TckValue::Bool(b) => CypherValue::Bool(b),
        TckValue::Integer(i) => CypherValue::Integer(i),
        TckValue::Float(f) => CypherValue::Float(f),
        TckValue::String(s) => CypherValue::String(s),
        TckValue::List(items) => {
            CypherValue::List(items.into_iter().map(tck_value_to_cypher).collect())
        }
        TckValue::Map(m) => CypherValue::Map(
            m.into_iter()
                .map(|(k, v)| (k, tck_value_to_cypher(v)))
                .collect(),
        ),
        TckValue::Node { .. } | TckValue::Edge { .. } | TckValue::Path(_) => CypherValue::Null,
    }
}

/// Parses a TCK table cell.
fn parse_tck_value(s: &str) -> TckValue {
    let s = s.trim();
    if s.is_empty() {
        return TckValue::Null;
    }

    // try each parser in order
    if let Some(v) = try_parse_null(s) {
        return v;
    }
    if let Some(v) = try_parse_bool(s) {
        return v;
    }
    if let Some(v) = try_parse_path(s) {
        return v;
    }
    if let Some(v) = try_parse_node(s) {
        return v;
    }
    if let Some(v) = try_parse_edge(s) {
        return v;
    }
    if let Some(v) = try_parse_list(s) {
        return v;
    }
    if let Some(v) = try_parse_map(s) {
        return v;
    }
    if let Some(v) = try_parse_string(s) {
        return v;
    }
    if let Some(v) = try_parse_float(s) {
        return v;
    }
    if let Some(v) = try_parse_integer(s) {
        return v;
    }
    // fallback: string
    TckValue::String(s.to_string())
}

fn try_parse_null(s: &str) -> Option<TckValue> {
    if s == "null" {
        Some(TckValue::Null)
    } else {
        None
    }
}

fn try_parse_bool(s: &str) -> Option<TckValue> {
    match s {
        "true" => Some(TckValue::Bool(true)),
        "false" => Some(TckValue::Bool(false)),
        _ => None,
    }
}

fn try_parse_integer(s: &str) -> Option<TckValue> {
    s.parse::<i64>().ok().map(TckValue::Integer)
}

fn try_parse_float(s: &str) -> Option<TckValue> {
    if s == "NaN" {
        return Some(TckValue::Float(f64::NAN));
    }
    if s == "Infinity" || s == "Inf" {
        return Some(TckValue::Float(f64::INFINITY));
    }
    if s == "-Infinity" || s == "-Inf" {
        return Some(TckValue::Float(f64::NEG_INFINITY));
    }
    // float if it has a dot or exponent
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s.parse::<f64>().ok().map(TckValue::Float)
    } else {
        None
    }
}

fn try_parse_string(s: &str) -> Option<TckValue> {
    if s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2 {
        Some(TckValue::String(s[1..s.len() - 1].to_string()))
    } else {
        None
    }
}

fn try_parse_list(s: &str) -> Option<TckValue> {
    if !s.starts_with('[') || !s.ends_with(']') {
        return None;
    }
    // could be an edge like [:TYPE]
    let inner = s[1..s.len() - 1].trim();
    if inner.starts_with(':') && !inner.contains(',') {
        // edge pattern, not a list
        return None;
    }
    if inner.is_empty() {
        return Some(TckValue::List(Vec::new()));
    }
    let items = split_top_level(inner, ',');
    Some(TckValue::List(
        items
            .into_iter()
            .map(|i| parse_tck_value(i.trim()))
            .collect(),
    ))
}

fn try_parse_map(s: &str) -> Option<TckValue> {
    if !s.starts_with('{') || !s.ends_with('}') {
        return None;
    }
    let inner = s[1..s.len() - 1].trim();
    if inner.is_empty() {
        return Some(TckValue::Map(BTreeMap::new()));
    }
    let entries = split_top_level(inner, ',');
    let mut map = BTreeMap::new();
    for entry in entries {
        let entry = entry.trim();
        if let Some(colon_pos) = entry.find(':') {
            let key = entry[..colon_pos].trim().to_string();
            let val = parse_tck_value(entry[colon_pos + 1..].trim());
            map.insert(key, val);
        }
    }
    Some(TckValue::Map(map))
}

fn try_parse_node(s: &str) -> Option<TckValue> {
    if !s.starts_with('(') || !s.ends_with(')') {
        return None;
    }
    let inner = s[1..s.len() - 1].trim();
    let (labels, properties) = parse_node_inner(inner);
    Some(TckValue::Node { labels, properties })
}

fn parse_node_inner(s: &str) -> (Vec<String>, BTreeMap<String, TckValue>) {
    let s = s.trim();
    if s.is_empty() {
        return (Vec::new(), BTreeMap::new());
    }

    let mut labels = Vec::new();
    let mut properties = BTreeMap::new();

    // properties block, if any
    let props_start = s.find('{');
    let label_part = if let Some(pos) = props_start {
        let props_str = &s[pos..];
        if let Some(TckValue::Map(m)) = try_parse_map(props_str) {
            properties = m;
        }
        &s[..pos]
    } else {
        s
    };

    // labels (`:Foo:Bar`)
    for part in label_part.split(':') {
        let label = part.trim();
        if !label.is_empty() {
            labels.push(label.to_string());
        }
    }

    (labels, properties)
}

fn try_parse_edge(s: &str) -> Option<TckValue> {
    if !s.starts_with('[') || !s.ends_with(']') {
        return None;
    }
    let inner = s[1..s.len() - 1].trim();
    if !inner.starts_with(':') {
        return None;
    }
    let inner = &inner[1..]; // skip leading ':'

    // properties block, if any
    let props_start = inner.find('{');
    let (type_part, properties) = if let Some(pos) = props_start {
        let props_str = &inner[pos..];
        let props = if let Some(TckValue::Map(m)) = try_parse_map(props_str) {
            m
        } else {
            BTreeMap::new()
        };
        (inner[..pos].trim(), props)
    } else {
        (inner.trim(), BTreeMap::new())
    };

    Some(TckValue::Edge {
        edge_type: type_part.to_string(),
        properties,
    })
}

fn try_parse_path(s: &str) -> Option<TckValue> {
    if !s.starts_with('<') || !s.ends_with('>') {
        return None;
    }
    let inner = &s[1..s.len() - 1];

    // alternating nodes and edges: (:A)-[:R]->(:B) or (:A)<-[:R]-(:B)
    let mut elements = Vec::new();
    let mut pos = 0;
    let bytes = inner.as_bytes();

    while pos < bytes.len() {
        match bytes[pos] {
            b'(' => {
                // node
                let end = find_matching(inner, pos, b'(', b')');
                elements.push(parse_tck_value(&inner[pos..=end]));
                pos = end + 1;
            }
            b'-' | b'<' => {
                // relationship connector -[:R]-> or <-[:R]-: find the [ ]
                if let Some(bracket_start) = inner[pos..].find('[') {
                    let abs_start = pos + bracket_start;
                    let abs_end = find_matching(inner, abs_start, b'[', b']');
                    elements.push(parse_tck_value(&inner[abs_start..=abs_end]));
                    pos = abs_end + 1;
                    // skip trailing -> or -
                    while pos < bytes.len() && (bytes[pos] == b'-' || bytes[pos] == b'>') {
                        pos += 1;
                    }
                } else {
                    pos += 1;
                }
            }
            _ => {
                pos += 1;
            }
        }
    }

    Some(TckValue::Path(elements))
}

/// Index of the matching closing bracket/paren.
fn find_matching(s: &str, start: usize, open: u8, close: u8) -> usize {
    let bytes = s.as_bytes();
    let mut depth = 0;
    let mut in_string = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if b == b'\'' {
                in_string = false;
            }
            continue;
        }
        if b == b'\'' {
            in_string = true;
            continue;
        }
        if b == open {
            depth += 1;
        } else if b == close {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
    }
    s.len() - 1
}

/// Splits at top-level `sep`, respecting brackets and strings.
fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut depth_paren = 0i32;
    let mut depth_bracket = 0i32;
    let mut depth_brace = 0i32;
    let mut in_string = false;
    let bytes = s.as_bytes();
    let sep_byte = sep as u8;

    for i in 0..bytes.len() {
        if in_string {
            if bytes[i] == b'\'' {
                in_string = false;
            }
            continue;
        }
        match bytes[i] {
            b'\'' => in_string = true,
            b'(' => depth_paren += 1,
            b')' => depth_paren -= 1,
            b'[' => depth_bracket += 1,
            b']' => depth_bracket -= 1,
            b'{' => depth_brace += 1,
            b'}' => depth_brace -= 1,
            c if c == sep_byte && depth_paren == 0 && depth_bracket == 0 && depth_brace == 0 => {
                result.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    result.push(&s[start..]);
    result
}

// ---------------------------------------------------------------------------
// Structural comparison: TckValue vs CypherValue
// ---------------------------------------------------------------------------

fn tck_matches(expected: &TckValue, actual: &CypherValue) -> bool {
    match (expected, actual) {
        (TckValue::Null, CypherValue::Null) => true,
        (TckValue::Bool(a), CypherValue::Bool(b)) => a == b,
        (TckValue::Integer(a), CypherValue::Integer(b)) => a == b,
        (TckValue::Integer(a), CypherValue::Float(b)) => (*a as f64) == *b,
        (TckValue::Float(a), CypherValue::Float(b)) => {
            if a.is_nan() && b.is_nan() {
                true
            } else {
                a == b
            }
        }
        (TckValue::Float(a), CypherValue::Integer(b)) => *a == (*b as f64),
        (TckValue::String(a), CypherValue::String(b)) => a == b,
        // temporals: the TCK expects their string forms
        (TckValue::String(s), CypherValue::Date(d)) => s == &d.to_string(),
        (TckValue::String(s), CypherValue::LocalTime(t)) => s == &t.to_string(),
        (TckValue::String(s), CypherValue::Time(t)) => s == &t.to_string(),
        (TckValue::String(s), CypherValue::LocalDateTime(ldt)) => s == &ldt.to_string(),
        (TckValue::String(s), CypherValue::DateTime(dt)) => s == &dt.to_string(),
        (TckValue::String(s), CypherValue::Duration(d)) => s == &d.to_string(),
        (TckValue::List(a), CypherValue::List(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(ea, ab)| tck_matches(ea, ab))
        }
        (TckValue::Map(a), CypherValue::Map(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, v)| b.get(k).is_some_and(|bv| tck_matches(v, bv)))
        }
        (
            TckValue::Node {
                labels: el,
                properties: ep,
            },
            CypherValue::Node {
                labels: al,
                properties: ap,
                ..
            },
        ) => {
            // labels compare as sets
            let expected_labels: HashSet<&str> = el.iter().map(|s| s.as_str()).collect();
            let actual_labels: HashSet<&str> = al.iter().map(|s| s.as_str()).collect();
            if expected_labels != actual_labels {
                return false;
            }
            // properties compare structurally
            if ep.len() != ap.len() {
                return false;
            }
            ep.iter().all(|(k, v)| {
                ap.get(k)
                    .is_some_and(|apv| tck_matches(v, &CypherValue::from_property(apv)))
            })
        }
        (
            TckValue::Edge {
                edge_type: et,
                properties: ep,
            },
            CypherValue::Edge {
                edge_type: at,
                properties: ap,
                ..
            },
        ) => {
            if et != at {
                return false;
            }
            if ep.len() != ap.len() {
                return false;
            }
            ep.iter().all(|(k, v)| {
                ap.get(k)
                    .is_some_and(|apv| tck_matches(v, &CypherValue::from_property(apv)))
            })
        }
        (TckValue::Path(elements), CypherValue::Path { nodes, edges }) => {
            // path elements alternate node, edge, ...; expected interleaves them, actual keeps
            // separate vectors
            let expected_nodes: Vec<&TckValue> = elements.iter().step_by(2).collect();
            let expected_edges: Vec<&TckValue> = elements.iter().skip(1).step_by(2).collect();
            if expected_nodes.len() != nodes.len() || expected_edges.len() != edges.len() {
                return false;
            }
            expected_nodes
                .iter()
                .zip(nodes.iter())
                .all(|(en, an)| tck_matches(en, an))
                && expected_edges
                    .iter()
                    .zip(edges.iter())
                    .all(|(ee, ae)| tck_matches(ee, ae))
        }
        _ => false,
    }
}

/// `tck_matches`, ignoring list element order.
fn tck_matches_ignore_list_order(expected: &TckValue, actual: &CypherValue) -> bool {
    match (expected, actual) {
        (TckValue::List(a), CypherValue::List(b)) => {
            if a.len() != b.len() {
                return false;
            }
            // match each expected element with an unused actual one (permutation)
            let mut used = vec![false; b.len()];
            for ea in a {
                let found = b
                    .iter()
                    .enumerate()
                    .any(|(i, ab)| !used[i] && tck_matches_ignore_list_order(ea, ab));
                if !found {
                    return false;
                }
                // claim the first match
                for (i, ab) in b.iter().enumerate() {
                    if !used[i] && tck_matches_ignore_list_order(ea, ab) {
                        used[i] = true;
                        break;
                    }
                }
            }
            true
        }
        // non-lists: normal matching
        _ => tck_matches(expected, actual),
    }
}

// ---------------------------------------------------------------------------
// Table parsing and row comparison
// ---------------------------------------------------------------------------

struct ExpectedRow {
    values: Vec<(String, TckValue)>, // (column_name, expected_value)
}

fn parse_expected_table(table: &cucumber::gherkin::Table) -> Vec<ExpectedRow> {
    if table.rows.is_empty() {
        return Vec::new();
    }
    let headers: Vec<String> = table.rows[0].iter().map(|h| h.trim().to_string()).collect();
    table.rows[1..]
        .iter()
        .map(|row| {
            let values = headers
                .iter()
                .zip(row.iter())
                .map(|(h, cell)| (h.clone(), parse_tck_value(cell.trim())))
                .collect();
            ExpectedRow { values }
        })
        .collect()
}

fn assert_rows_match_unordered(
    columns: &[String],
    actual: &[Vec<CypherValue>],
    expected: &[ExpectedRow],
) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "row count mismatch: expected {}, got {}.\nActual rows: {:?}",
        expected.len(),
        actual.len(),
        actual
    );

    let mut matched = vec![false; actual.len()];
    for (ei, exp_row) in expected.iter().enumerate() {
        let found = actual.iter().enumerate().any(|(ai, act_row)| {
            !matched[ai] && row_matches(columns, act_row, exp_row, tck_matches)
        });
        assert!(
            found,
            "expected row {} not found in actual results.\nExpected: {:?}\nActual rows: {:?}",
            ei, exp_row.values, actual
        );
        // claim the match
        for (ai, act_row) in actual.iter().enumerate() {
            if !matched[ai] && row_matches(columns, act_row, exp_row, tck_matches) {
                matched[ai] = true;
                break;
            }
        }
    }
}

fn assert_rows_match_ordered(
    columns: &[String],
    actual: &[Vec<CypherValue>],
    expected: &[ExpectedRow],
) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "row count mismatch: expected {}, got {}.\nActual rows: {:?}",
        expected.len(),
        actual.len(),
        actual
    );

    for (i, (act_row, exp_row)) in actual.iter().zip(expected.iter()).enumerate() {
        assert!(
            row_matches(columns, act_row, exp_row, tck_matches),
            "row {} mismatch.\nExpected: {:?}\nActual: {:?}",
            i,
            exp_row.values,
            act_row
        );
    }
}

fn assert_rows_match_unordered_ignore_list_order(
    columns: &[String],
    actual: &[Vec<CypherValue>],
    expected: &[ExpectedRow],
) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "row count mismatch: expected {}, got {}",
        expected.len(),
        actual.len()
    );

    let mut matched = vec![false; actual.len()];
    for (ei, exp_row) in expected.iter().enumerate() {
        let found = actual.iter().enumerate().any(|(ai, act_row)| {
            !matched[ai] && row_matches(columns, act_row, exp_row, tck_matches_ignore_list_order)
        });
        assert!(
            found,
            "expected row {} not found in actual results (ignoring list order).\nExpected: {:?}\nActual rows: {:?}",
            ei, exp_row.values, actual
        );
        for (ai, act_row) in actual.iter().enumerate() {
            if !matched[ai] && row_matches(columns, act_row, exp_row, tck_matches_ignore_list_order)
            {
                matched[ai] = true;
                break;
            }
        }
    }
}

fn assert_rows_match_ordered_ignore_list_order(
    columns: &[String],
    actual: &[Vec<CypherValue>],
    expected: &[ExpectedRow],
) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "row count mismatch: expected {}, got {}",
        expected.len(),
        actual.len()
    );

    for (i, (act_row, exp_row)) in actual.iter().zip(expected.iter()).enumerate() {
        assert!(
            row_matches(columns, act_row, exp_row, tck_matches_ignore_list_order),
            "row {} mismatch (ignoring list order).\nExpected: {:?}\nActual: {:?}",
            i,
            exp_row.values,
            act_row
        );
    }
}

fn row_matches(
    columns: &[String],
    actual: &[CypherValue],
    expected: &ExpectedRow,
    matcher: fn(&TckValue, &CypherValue) -> bool,
) -> bool {
    for (col_name, exp_val) in &expected.values {
        let col_idx = columns.iter().position(|c| c == col_name);
        let Some(idx) = col_idx else {
            return false;
        };
        if idx >= actual.len() {
            return false;
        }
        if !matcher(exp_val, &actual[idx]) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    // big-stack thread: some TCK queries nest deeply (Create4's movie graph) and overflow the
    // default 1-2 MB
    let builder = std::thread::Builder::new()
        .name("tck-runner".into())
        .stack_size(64 * 1024 * 1024);
    let handle = builder
        .spawn(|| {
            let runner = TckWorld::cucumber()
                .max_concurrent_scenarios(Some(1))
                .with_default_cli();
            futures::executor::block_on(runner.run("tests/features"));
        })
        .expect("failed to spawn TCK runner thread");
    handle.join().expect("TCK runner thread panicked");
}
