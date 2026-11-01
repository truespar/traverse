//! Structured type bodies for `CREATE GRAPH TYPE { ... }`: NODE / VERTEX / EDGE /
//! RELATIONSHIP element types (mandatory 4.13) and predefined value types
//! (4.16).
//!
//! The parser tries a canonical ISO section 17.2 element list first; on success
//! `Inline` carries the structured elements. A non-canonical body falls back to
//! brace counting, so uncodified TCK bodies still round-trip.

use traverse_gql::ast::{EdgeKind, GraphTypeElement, ValueTypeName};
use traverse_gql::{GraphTypeBody, Statement, parse};

fn parse_one_create_graph_type(src: &str) -> Statement {
    let p = parse(src).expect("parse failed");
    assert_eq!(p.statements.len(), 1);
    p.statements.into_iter().next().unwrap()
}

fn body_of(stmt: &Statement) -> &GraphTypeBody {
    match stmt {
        Statement::CreateGraphType { body, .. } => body,
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

fn elements_of(body: &GraphTypeBody) -> &[GraphTypeElement] {
    match body {
        GraphTypeBody::Inline {
            elements: Some(els),
            ..
        } => els.as_slice(),
        other => panic!("expected structured Inline body, got {other:?}"),
    }
}

// -- NODE / VERTEX (mandatory 4.13) --------------------------------

#[test]
fn empty_inline_body_is_structured() {
    let stmt = parse_one_create_graph_type("CREATE GRAPH TYPE /t {}");
    let els = elements_of(body_of(&stmt));
    assert!(els.is_empty());
}

#[test]
fn single_node_element_parses() {
    let stmt = parse_one_create_graph_type("CREATE GRAPH TYPE /t { NODE Person }");
    let els = elements_of(body_of(&stmt));
    assert_eq!(els.len(), 1);
    match &els[0] {
        GraphTypeElement::Node {
            name,
            labels,
            properties,
            ..
        } => {
            assert_eq!(name.as_deref(), Some("Person"));
            assert!(labels.is_empty());
            assert!(properties.is_empty());
        }
        other => panic!("expected Node element, got {other:?}"),
    }
}

#[test]
fn vertex_is_a_synonym_for_node() {
    let stmt = parse_one_create_graph_type("CREATE GRAPH TYPE /t { VERTEX Person }");
    let els = elements_of(body_of(&stmt));
    assert_eq!(els.len(), 1);
    assert!(matches!(&els[0], GraphTypeElement::Node { .. }));
}

#[test]
fn any_node_form_is_accepted() {
    let stmt = parse_one_create_graph_type("CREATE GRAPH TYPE /t { ANY NODE Person }");
    let els = elements_of(body_of(&stmt));
    assert_eq!(els.len(), 1);
    assert!(matches!(&els[0], GraphTypeElement::Node { .. }));
}

#[test]
fn node_with_label_set_parses() {
    let stmt =
        parse_one_create_graph_type("CREATE GRAPH TYPE /t { NODE Person :Account & Customer }");
    let els = elements_of(body_of(&stmt));
    match &els[0] {
        GraphTypeElement::Node { labels, .. } => {
            assert_eq!(labels, &["Account".to_string(), "Customer".to_string()]);
        }
        _ => panic!("expected Node"),
    }
}

// -- EDGE / RELATIONSHIP (mandatory 4.13) --------------------------

#[test]
fn edge_element_parses() {
    let stmt = parse_one_create_graph_type("CREATE GRAPH TYPE /t { EDGE Knows }");
    let els = elements_of(body_of(&stmt));
    match &els[0] {
        GraphTypeElement::Edge {
            kind: EdgeKind::Edge,
            name,
            ..
        } => {
            assert_eq!(name.as_deref(), Some("Knows"));
        }
        other => panic!("expected Edge element, got {other:?}"),
    }
}

#[test]
fn relationship_synonym_preserves_kind() {
    let stmt = parse_one_create_graph_type("CREATE GRAPH TYPE /t { RELATIONSHIP Knows }");
    let els = elements_of(body_of(&stmt));
    match &els[0] {
        GraphTypeElement::Edge {
            kind: EdgeKind::Relationship,
            name,
            ..
        } => {
            assert_eq!(name.as_deref(), Some("Knows"));
        }
        other => panic!("expected Edge(Relationship), got {other:?}"),
    }
}

// -- Property declarations + predefined value types (4.16) ---------

#[test]
fn property_with_string_type() {
    let stmt =
        parse_one_create_graph_type("CREATE GRAPH TYPE /t { NODE Person { name :: STRING } }");
    let els = elements_of(body_of(&stmt));
    match &els[0] {
        GraphTypeElement::Node { properties, .. } => {
            assert_eq!(properties.len(), 1);
            assert_eq!(properties[0].name, "name");
            assert_eq!(properties[0].value_type, ValueTypeName::String);
            assert!(!properties[0].not_null);
        }
        _ => panic!("expected Node"),
    }
}

#[test]
fn varchar_is_synonym_for_string() {
    let stmt =
        parse_one_create_graph_type("CREATE GRAPH TYPE /t { NODE Person { name :: VARCHAR } }");
    let props = match &elements_of(body_of(&stmt))[0] {
        GraphTypeElement::Node { properties, .. } => properties.clone(),
        _ => panic!("expected Node"),
    };
    assert_eq!(props[0].value_type, ValueTypeName::String);
}

#[test]
fn signed_integer_is_recognised() {
    let stmt = parse_one_create_graph_type(
        "CREATE GRAPH TYPE /t { NODE Person { age :: SIGNED INTEGER } }",
    );
    let props = match &elements_of(body_of(&stmt))[0] {
        GraphTypeElement::Node { properties, .. } => properties.clone(),
        _ => panic!(),
    };
    assert_eq!(props[0].value_type, ValueTypeName::Integer);
}

#[test]
fn int_is_synonym_for_integer() {
    let stmt = parse_one_create_graph_type("CREATE GRAPH TYPE /t { NODE Person { age :: INT } }");
    let props = match &elements_of(body_of(&stmt))[0] {
        GraphTypeElement::Node { properties, .. } => properties.clone(),
        _ => panic!(),
    };
    assert_eq!(props[0].value_type, ValueTypeName::Integer);
}

#[test]
fn bool_is_synonym_for_boolean() {
    let stmt = parse_one_create_graph_type("CREATE GRAPH TYPE /t { NODE Flag { on :: BOOL } }");
    let props = match &elements_of(body_of(&stmt))[0] {
        GraphTypeElement::Node { properties, .. } => properties.clone(),
        _ => panic!(),
    };
    assert_eq!(props[0].value_type, ValueTypeName::Boolean);
}

#[test]
fn not_null_is_captured() {
    let stmt = parse_one_create_graph_type(
        "CREATE GRAPH TYPE /t { NODE Person { name :: STRING NOT NULL } }",
    );
    let props = match &elements_of(body_of(&stmt))[0] {
        GraphTypeElement::Node { properties, .. } => properties.clone(),
        _ => panic!(),
    };
    assert!(props[0].not_null);
}

#[test]
fn multiple_properties_parse() {
    let stmt = parse_one_create_graph_type(
        "CREATE GRAPH TYPE /t { NODE Person { name :: STRING, age :: INT, dob :: DATE } }",
    );
    let props = match &elements_of(body_of(&stmt))[0] {
        GraphTypeElement::Node { properties, .. } => properties.clone(),
        _ => panic!(),
    };
    assert_eq!(props.len(), 3);
    assert_eq!(props[0].value_type, ValueTypeName::String);
    assert_eq!(props[1].value_type, ValueTypeName::Integer);
    assert_eq!(props[2].value_type, ValueTypeName::Date);
}

#[test]
fn temporal_value_types_recognised() {
    let stmt = parse_one_create_graph_type(
        "CREATE GRAPH TYPE /t { NODE T {
             d :: DATE,
             lt :: LOCAL TIME,
             ldt :: LOCAL DATETIME,
             zt :: ZONED TIME,
             zdt :: ZONED DATETIME,
             dur :: DURATION
         } }",
    );
    let props = match &elements_of(body_of(&stmt))[0] {
        GraphTypeElement::Node { properties, .. } => properties.clone(),
        _ => panic!(),
    };
    assert_eq!(props[0].value_type, ValueTypeName::Date);
    assert_eq!(props[1].value_type, ValueTypeName::LocalTime);
    assert_eq!(props[2].value_type, ValueTypeName::LocalDatetime);
    assert_eq!(props[3].value_type, ValueTypeName::ZonedTime);
    assert_eq!(props[4].value_type, ValueTypeName::ZonedDatetime);
    assert_eq!(props[5].value_type, ValueTypeName::Duration);
}

// -- Multi-element bodies + comma separation -----------------------

#[test]
fn multiple_elements_in_one_body() {
    let stmt = parse_one_create_graph_type(
        "CREATE GRAPH TYPE /t { \
            NODE Person { name :: STRING }, \
            NODE Company { name :: STRING }, \
            EDGE WorksAt \
        }",
    );
    let els = elements_of(body_of(&stmt));
    assert_eq!(els.len(), 3);
    assert!(matches!(&els[0], GraphTypeElement::Node { .. }));
    assert!(matches!(&els[1], GraphTypeElement::Node { .. }));
    assert!(matches!(&els[2], GraphTypeElement::Edge { .. }));
}

// -- union value types in property declarations (GV66 / GV67) --

#[test]
fn property_value_type_can_be_union_of_two() {
    let stmt = parse_one_create_graph_type(
        "CREATE GRAPH TYPE /t { NODE Person { id :: STRING | INTEGER } }",
    );
    let props = match &elements_of(body_of(&stmt))[0] {
        GraphTypeElement::Node { properties, .. } => properties.clone(),
        _ => panic!("expected Node"),
    };
    assert_eq!(
        props[0].value_type,
        ValueTypeName::Union(vec![ValueTypeName::String, ValueTypeName::Integer]),
    );
}

#[test]
fn property_value_type_can_be_union_of_three() {
    let stmt = parse_one_create_graph_type(
        "CREATE GRAPH TYPE /t { NODE Event { ts :: DATE | LOCAL_DATETIME | STRING } }",
    );
    let props = match &elements_of(body_of(&stmt))[0] {
        GraphTypeElement::Node { properties, .. } => properties.clone(),
        _ => panic!("expected Node"),
    };
    assert_eq!(
        props[0].value_type,
        ValueTypeName::Union(vec![
            ValueTypeName::Date,
            ValueTypeName::LocalDatetime,
            ValueTypeName::String,
        ]),
    );
}

#[test]
fn property_value_type_union_with_not_null() {
    let stmt = parse_one_create_graph_type(
        "CREATE GRAPH TYPE /t { NODE N { v :: INTEGER | STRING NOT NULL } }",
    );
    let props = match &elements_of(body_of(&stmt))[0] {
        GraphTypeElement::Node { properties, .. } => properties.clone(),
        _ => panic!("expected Node"),
    };
    assert!(matches!(props[0].value_type, ValueTypeName::Union(_)));
    assert!(props[0].not_null);
}

#[test]
fn single_type_does_not_become_a_union() {
    // a single leaf must not be wrapped in Union
    let stmt = parse_one_create_graph_type("CREATE GRAPH TYPE /t { NODE N { x :: INTEGER } }");
    let props = match &elements_of(body_of(&stmt))[0] {
        GraphTypeElement::Node { properties, .. } => properties.clone(),
        _ => panic!("expected Node"),
    };
    assert_eq!(props[0].value_type, ValueTypeName::Integer);
}

// -- fallback: non-canonical bodies still round-trip ---------------

#[test]
fn legacy_opaque_body_falls_back_cleanly() {
    // a body that isn't canonical ISO section 17.2 still parses, with `elements` None; raw token soup the
    // structured parser can't handle
    let stmt = parse_one_create_graph_type("CREATE GRAPH TYPE /t { mystery_keyword foo bar }");
    match body_of(&stmt) {
        GraphTypeBody::Inline { elements, .. } => {
            assert!(elements.is_none(), "expected None for non-canonical body");
        }
        other => panic!("expected Inline, got {other:?}"),
    }
}
