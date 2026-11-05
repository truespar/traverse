//! Scalar Cypher semantics: equality, ordering, arithmetic, type predicates and property access
//! on single values. The typed kernels in `kernels` are specializations of these; whenever a
//! lane's types don't fit a kernel, evaluation comes here.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use traverse_core::tx::view::GraphView;

use crate::ast::{BinOp, CmpOp, StringOp};
use crate::error::CypherError;
use crate::status::GqlStatus;
use crate::temporal;
use crate::types::CypherValue;

use super::super::values::VIRTUAL_ID_BASE;

/// Truthiness: null -> false, bool -> itself, else true.
#[inline]
pub fn is_truthy(val: &CypherValue) -> bool {
    match val {
        CypherValue::Null => false,
        CypherValue::Bool(b) => *b,
        _ => true,
    }
}

/// Three-valued boolean of a value: `None` for null.
#[inline]
pub fn as_bool3(val: &CypherValue) -> Option<bool> {
    match val {
        CypherValue::Null => None,
        CypherValue::Bool(b) => Some(*b),
        _ => Some(true),
    }
}

/// Three-valued Cypher equality.
pub fn cypher_eq(left: &CypherValue, right: &CypherValue) -> CypherValue {
    match (left, right) {
        (CypherValue::Null, _) | (_, CypherValue::Null) => CypherValue::Null,
        // NaN equals nothing, itself included
        (CypherValue::Float(a), _) if a.is_nan() => CypherValue::Bool(false),
        (_, CypherValue::Float(b)) if b.is_nan() => CypherValue::Bool(false),
        (CypherValue::List(a), CypherValue::List(b)) => {
            if a.len() != b.len() {
                return CypherValue::Bool(false);
            }
            let mut has_null = false;
            for (ai, bi) in a.iter().zip(b.iter()) {
                match cypher_eq(ai, bi) {
                    CypherValue::Bool(false) => return CypherValue::Bool(false),
                    CypherValue::Null => has_null = true,
                    _ => {}
                }
            }
            if has_null {
                CypherValue::Null
            } else {
                CypherValue::Bool(true)
            }
        }
        (CypherValue::Map(a), CypherValue::Map(b)) => {
            if a.keys().collect::<std::collections::HashSet<_>>()
                != b.keys().collect::<std::collections::HashSet<_>>()
            {
                return CypherValue::Bool(false);
            }
            let mut has_null = false;
            for (key, av) in a {
                if let Some(bv) = b.get(key) {
                    match cypher_eq(av, bv) {
                        CypherValue::Bool(false) => return CypherValue::Bool(false),
                        CypherValue::Null => has_null = true,
                        _ => {}
                    }
                } else {
                    return CypherValue::Bool(false);
                }
            }
            if has_null {
                CypherValue::Null
            } else {
                CypherValue::Bool(true)
            }
        }
        _ => CypherValue::Bool(left == right),
    }
}

/// Three-valued comparison.
pub fn eval_comparison(left: &CypherValue, op: CmpOp, right: &CypherValue) -> CypherValue {
    if left.is_null() || right.is_null() {
        return CypherValue::Null;
    }
    match op {
        CmpOp::Eq => cypher_eq(left, right),
        CmpOp::Neq => match cypher_eq(left, right) {
            CypherValue::Bool(b) => CypherValue::Bool(!b),
            other => other,
        },
        _ => {
            if let (CypherValue::List(la), CypherValue::List(lb)) = (left, right) {
                return list_compare(la, lb, op);
            }
            if !are_order_comparable(left, right) {
                return CypherValue::Null;
            }
            let has_nan = matches!(left, CypherValue::Float(f) if f.is_nan())
                || matches!(right, CypherValue::Float(f) if f.is_nan());
            if has_nan {
                return CypherValue::Bool(false);
            }
            let ord = compare_values(left, right);
            CypherValue::Bool(ordering_matches(ord, op))
        }
    }
}

/// Does `ord` satisfy an ordering comparison?
#[inline]
pub fn ordering_matches(ord: Ordering, op: CmpOp) -> bool {
    match op {
        CmpOp::Lt => ord == Ordering::Less,
        CmpOp::Lte => ord != Ordering::Greater,
        CmpOp::Gt => ord == Ordering::Greater,
        CmpOp::Gte => ord != Ordering::Less,
        CmpOp::Eq => ord == Ordering::Equal,
        CmpOp::Neq => ord != Ordering::Equal,
    }
}

fn list_compare(la: &[CypherValue], lb: &[CypherValue], op: CmpOp) -> CypherValue {
    let mut has_null = false;
    for (a, b) in la.iter().zip(lb.iter()) {
        let cmp = eval_comparison(a, op, b);
        if let CypherValue::Null = &cmp {
            has_null = true;
            continue;
        }
        let eq = eval_comparison(a, CmpOp::Eq, b);
        match eq {
            CypherValue::Bool(true) => continue,
            CypherValue::Null => {
                has_null = true;
                continue;
            }
            _ => return cmp,
        }
    }
    if la.len() == lb.len() {
        if has_null {
            return CypherValue::Null;
        }
        return CypherValue::Bool(matches!(op, CmpOp::Lte | CmpOp::Gte));
    }
    if has_null {
        return CypherValue::Null;
    }
    let result = match op {
        CmpOp::Lt => la.len() < lb.len(),
        CmpOp::Lte => la.len() <= lb.len(),
        CmpOp::Gt => la.len() > lb.len(),
        CmpOp::Gte => la.len() >= lb.len(),
        _ => unreachable!(),
    };
    CypherValue::Bool(result)
}

/// Structural equality for procedure row matching; unlike `cypher_eq`, null == null is true.
pub fn proc_values_equal(a: &CypherValue, b: &CypherValue) -> bool {
    match (a, b) {
        (CypherValue::Null, CypherValue::Null) => true,
        (CypherValue::Null, _) | (_, CypherValue::Null) => false,
        (CypherValue::Integer(x), CypherValue::Integer(y)) => x == y,
        (CypherValue::Float(x), CypherValue::Float(y)) => x == y,
        (CypherValue::String(x), CypherValue::String(y)) => x == y,
        (CypherValue::Bool(x), CypherValue::Bool(y)) => x == y,
        _ => a == b,
    }
}

/// Order-comparable (for <, >, <=, >=)? Different type groups aren't and yield null.
pub fn are_order_comparable(a: &CypherValue, b: &CypherValue) -> bool {
    fn order_group(v: &CypherValue) -> u8 {
        match v {
            CypherValue::Integer(_) | CypherValue::Float(_) => 0,
            CypherValue::String(_) => 1,
            CypherValue::Bool(_) => 2,
            CypherValue::Bytes(_) => 13,
            CypherValue::List(_) => 3,
            CypherValue::Date(_) => 4,
            CypherValue::LocalTime(_) => 5,
            CypherValue::Time(_) => 6,
            CypherValue::LocalDateTime(_) => 7,
            CypherValue::DateTime(_) => 8,
            CypherValue::Duration(_) => 9,
            CypherValue::Map(_) => 10,
            CypherValue::Node { .. } | CypherValue::NodeRef(_) => 11,
            CypherValue::Edge { .. } | CypherValue::EdgeRef { .. } => 12,
            CypherValue::Path { .. } => 13,
            CypherValue::Null => 14,
        }
    }
    order_group(a) == order_group(b)
}

/// The total order ORDER BY uses: Map < Node < Edge < List < Path < String < Bool < Number < NaN
/// < Null.
pub fn compare_values(a: &CypherValue, b: &CypherValue) -> Ordering {
    fn type_rank(v: &CypherValue) -> u8 {
        match v {
            CypherValue::Bytes(_) => 0,
            CypherValue::Map(_) => 0,
            CypherValue::Node { .. } | CypherValue::NodeRef(_) => 1,
            CypherValue::Edge { .. } | CypherValue::EdgeRef { .. } => 2,
            CypherValue::List(_) => 3,
            CypherValue::Path { .. } => 4,
            CypherValue::String(_) => 5,
            CypherValue::Bool(_) => 6,
            CypherValue::Integer(_) => 7,
            CypherValue::Float(f) if f.is_nan() => 8,
            CypherValue::Float(_) => 7,
            CypherValue::Null => 9,
            CypherValue::Date(_)
            | CypherValue::LocalTime(_)
            | CypherValue::Time(_)
            | CypherValue::LocalDateTime(_)
            | CypherValue::DateTime(_)
            | CypherValue::Duration(_) => 7,
        }
    }
    if let (CypherValue::List(la), CypherValue::List(lb)) = (a, b) {
        for (ai, bi) in la.iter().zip(lb.iter()) {
            let ord = compare_values(ai, bi);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        return la.len().cmp(&lb.len());
    }
    if let Some(ord) = a.partial_cmp(b) {
        return ord;
    }
    type_rank(a).cmp(&type_rank(b))
}

/// `IN` membership with null propagation.
pub fn in_list(item: &CypherValue, list: &CypherValue) -> Result<CypherValue, CypherError> {
    match list {
        CypherValue::List(items) => {
            if items.is_empty() {
                return Ok(CypherValue::Bool(false));
            }
            if item.is_null() {
                return Ok(CypherValue::Null);
            }
            let mut has_null = false;
            for it in items {
                match cypher_eq(item, it) {
                    CypherValue::Bool(true) => return Ok(CypherValue::Bool(true)),
                    CypherValue::Null => has_null = true,
                    _ => {}
                }
            }
            Ok(if has_null {
                CypherValue::Null
            } else {
                CypherValue::Bool(false)
            })
        }
        CypherValue::Null => Ok(CypherValue::Null),
        _ => Err(CypherError::type_error(format!(
            "IN requires a list on the right side, got {}",
            list.type_name()
        ))),
    }
}

/// STARTS WITH / ENDS WITH / CONTAINS / `=~`: null unless both sides are strings.
pub fn string_op(
    left: &CypherValue,
    op: StringOp,
    right: &CypherValue,
) -> Result<CypherValue, CypherError> {
    match (left, right) {
        (CypherValue::String(a), CypherValue::String(b)) => {
            Ok(CypherValue::Bool(string_op_str(a, op, b)?))
        }
        _ => Ok(CypherValue::Null),
    }
}

/// [`string_op`] on string slices.
#[inline]
pub fn string_op_str(a: &str, op: StringOp, b: &str) -> Result<bool, CypherError> {
    Ok(match op {
        StringOp::StartsWith => a.starts_with(b),
        StringOp::EndsWith => a.ends_with(b),
        StringOp::Contains => a.contains(b),
        StringOp::Regex => regex_full_match(b, a)?,
    })
}

thread_local! {
    /// `=~` patterns anchored at both ends, kept apart from the `apoc.text.*` search cache so the
    /// same text never gets the other form.
    static FULL_MATCH_REGEX_CACHE: std::cell::RefCell<std::collections::HashMap<String, regex::Regex>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Cypher `=~`: does `pattern` match all of `s`?
pub fn regex_full_match(pattern: &str, s: &str) -> Result<bool, CypherError> {
    FULL_MATCH_REGEX_CACHE.with(|cache| {
        let mut c = cache.borrow_mut();
        if let Some(re) = c.get(pattern) {
            return Ok(re.is_match(s));
        }
        let re = regex::Regex::new(&format!("^(?:{pattern})$"))
            .map_err(|e| CypherError::type_error(format!("invalid regex: {e}")))?;
        let hit = re.is_match(s);
        if c.len() > 256 {
            c.clear();
        }
        c.insert(pattern.to_string(), re);
        Ok(hit)
    })
}

/// Arithmetic and concatenation on two values.
pub fn eval_binary_op(
    left: &CypherValue,
    op: BinOp,
    right: &CypherValue,
) -> Result<CypherValue, CypherError> {
    if left.is_null() || right.is_null() {
        return Ok(CypherValue::Null);
    }
    match op {
        BinOp::Add => match (left, right) {
            (CypherValue::Integer(a), CypherValue::Integer(b)) => {
                a.checked_add(*b).map(CypherValue::Integer).ok_or_else(|| {
                    CypherError::type_error("long overflow".to_string())
                        .with_status(GqlStatus::NumericOutOfRange)
                })
            }
            (CypherValue::Float(a), CypherValue::Float(b)) => Ok(CypherValue::Float(a + b)),
            (CypherValue::Integer(a), CypherValue::Float(b)) => {
                Ok(CypherValue::Float(*a as f64 + b))
            }
            (CypherValue::Float(a), CypherValue::Integer(b)) => {
                Ok(CypherValue::Float(a + *b as f64))
            }
            (CypherValue::String(a), CypherValue::String(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            (CypherValue::String(a), CypherValue::Integer(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            (CypherValue::String(a), CypherValue::Float(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            (CypherValue::String(a), CypherValue::Bool(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            (CypherValue::Integer(a), CypherValue::String(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            (CypherValue::Float(a), CypherValue::String(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            (CypherValue::Bool(a), CypherValue::String(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            (CypherValue::List(a), CypherValue::List(b)) => {
                let mut result = a.clone();
                result.extend(b.iter().cloned());
                Ok(CypherValue::List(result))
            }
            (CypherValue::List(a), other) if !matches!(other, CypherValue::Duration(_)) => {
                let mut result = a.clone();
                result.push(other.clone());
                Ok(CypherValue::List(result))
            }
            (other, CypherValue::List(b)) if !matches!(other, CypherValue::Duration(_)) => {
                let mut result = vec![other.clone()];
                result.extend(b.iter().cloned());
                Ok(CypherValue::List(result))
            }
            (_, CypherValue::Duration(d)) => temporal::temporal_add(left, d),
            (CypherValue::Duration(d), _) => temporal::temporal_add(right, d),
            _ => Err(CypherError::type_error(format!(
                "cannot add {} and {}",
                left.type_name(),
                right.type_name()
            ))),
        },
        BinOp::Sub => match (left, right) {
            (_, CypherValue::Duration(d)) => temporal::temporal_sub(left, d),
            _ => numeric_op(left, right, i64::checked_sub, |a, b| a - b),
        },
        BinOp::Mul => match (left, right) {
            (CypherValue::Duration(d), CypherValue::Integer(n)) => {
                Ok(temporal::duration_mul(d, *n as f64))
            }
            (CypherValue::Duration(d), CypherValue::Float(n)) => Ok(temporal::duration_mul(d, *n)),
            (CypherValue::Integer(n), CypherValue::Duration(d)) => {
                Ok(temporal::duration_mul(d, *n as f64))
            }
            (CypherValue::Float(n), CypherValue::Duration(d)) => Ok(temporal::duration_mul(d, *n)),
            _ => numeric_op(left, right, i64::checked_mul, |a, b| a * b),
        },
        BinOp::Div => match (left, right) {
            (CypherValue::Duration(d), CypherValue::Integer(n)) => {
                if *n == 0 {
                    Err(CypherError::type_error("division by zero".to_string())
                        .with_status(GqlStatus::DivisionByZero))
                } else {
                    temporal::duration_div(d, *n as f64)
                }
            }
            (CypherValue::Duration(d), CypherValue::Float(n)) => temporal::duration_div(d, *n),
            (CypherValue::Integer(_), CypherValue::Integer(0)) => {
                Err(CypherError::type_error("division by zero".to_string())
                    .with_status(GqlStatus::DivisionByZero))
            }
            (CypherValue::Integer(a), CypherValue::Integer(b)) => {
                a.checked_div(*b).map(CypherValue::Integer).ok_or_else(|| {
                    CypherError::type_error("integer division overflow".to_string())
                        .with_status(GqlStatus::NumericOutOfRange)
                })
            }
            _ => numeric_op(left, right, i64::checked_div, |a, b| a / b),
        },
        BinOp::Mod => match (left, right) {
            (CypherValue::Integer(a), CypherValue::Integer(b)) => {
                if *b == 0 {
                    Err(CypherError::type_error("modulo by zero".to_string())
                        .with_status(GqlStatus::DivisionByZero))
                } else {
                    a.checked_rem(*b).map(CypherValue::Integer).ok_or_else(|| {
                        CypherError::type_error("integer modulo overflow".to_string())
                            .with_status(GqlStatus::NumericOutOfRange)
                    })
                }
            }
            _ => numeric_op(left, right, i64::checked_rem, |a, b| a % b),
        },
        BinOp::Pow => {
            let a = left.to_f64().ok_or_else(|| {
                CypherError::type_error(format!("cannot exponentiate {}", left.type_name()))
            })?;
            let b = right.to_f64().ok_or_else(|| {
                CypherError::type_error(format!("cannot exponentiate by {}", right.type_name()))
            })?;
            Ok(CypherValue::Float(a.powf(b)))
        }
        BinOp::Concat => match (left, right) {
            (CypherValue::String(a), CypherValue::String(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            (CypherValue::List(a), CypherValue::List(b)) => {
                let mut out = Vec::with_capacity(a.len() + b.len());
                out.extend(a.iter().cloned());
                out.extend(b.iter().cloned());
                Ok(CypherValue::List(out))
            }
            _ => Err(CypherError::type_error(format!(
                "`||` requires two STRING or two LIST operands; got {} and {}",
                left.type_name(),
                right.type_name()
            ))),
        },
        BinOp::Xor => Ok(CypherValue::Bool(is_truthy(left) ^ is_truthy(right))),
        BinOp::And | BinOp::Or => unreachable!("AND / OR evaluate with short-circuit"),
    }
}

fn numeric_op(
    left: &CypherValue,
    right: &CypherValue,
    int_op: fn(i64, i64) -> Option<i64>,
    float_op: fn(f64, f64) -> f64,
) -> Result<CypherValue, CypherError> {
    match (left, right) {
        (CypherValue::Integer(a), CypherValue::Integer(b)) => {
            int_op(*a, *b).map(CypherValue::Integer).ok_or_else(|| {
                CypherError::type_error("long overflow".to_string())
                    .with_status(GqlStatus::NumericOutOfRange)
            })
        }
        (CypherValue::Float(a), CypherValue::Float(b)) => Ok(CypherValue::Float(float_op(*a, *b))),
        (CypherValue::Integer(a), CypherValue::Float(b)) => {
            Ok(CypherValue::Float(float_op(*a as f64, *b)))
        }
        (CypherValue::Float(a), CypherValue::Integer(b)) => {
            Ok(CypherValue::Float(float_op(*a, *b as f64)))
        }
        _ => Err(CypherError::type_error(format!(
            "numeric operation on {} and {}",
            left.type_name(),
            right.type_name()
        ))),
    }
}

/// Unary minus.
pub fn negate(val: &CypherValue) -> Result<CypherValue, CypherError> {
    match val {
        CypherValue::Integer(i) => i.checked_neg().map(CypherValue::Integer).ok_or_else(|| {
            CypherError::type_error("long overflow".to_string())
                .with_status(GqlStatus::NumericOutOfRange)
        }),
        CypherValue::Float(f) => Ok(CypherValue::Float(-f)),
        CypherValue::Null => Ok(CypherValue::Null),
        _ => Err(CypherError::type_error(format!(
            "cannot negate {}",
            val.type_name()
        ))),
    }
}

/// `NOT`.
pub fn not(val: &CypherValue) -> CypherValue {
    if val.is_null() {
        CypherValue::Null
    } else {
        CypherValue::Bool(!is_truthy(val))
    }
}

/// `IS :: TYPE` with Neo4j semantics: null matches any type unless `NOT NULL`; non-null must match
/// structurally; `LIST<T>` checks elements.
pub fn type_predicate_matches(v: &CypherValue, type_name: &str, not_null: bool) -> bool {
    if matches!(v, CypherValue::Null) {
        return !not_null;
    }
    let norm: String = type_name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase();
    let (base, inner) = match norm.split_once('<') {
        Some((b, rest)) => (b.trim(), Some(rest.trim_end_matches('>').trim())),
        None => (norm.as_str(), None),
    };
    let actual = crate::functions::value_base_type(v);
    match base {
        "ANY" => true,
        "NOTHING" | "NULL" => false,
        "LIST" | "ARRAY" => {
            let CypherValue::List(items) = v else {
                return false;
            };
            match inner {
                Some(inner_ty) if !inner_ty.is_empty() => {
                    let inner_ty = inner_ty.trim_end_matches("NOT NULL").trim();
                    items
                        .iter()
                        .all(|e| type_predicate_matches(e, inner_ty, false))
                }
                _ => true,
            }
        }
        "BOOLEAN" | "BOOL" => actual == "BOOLEAN",
        "INTEGER" | "INT" | "SIGNED INTEGER" => actual == "INTEGER",
        "FLOAT" => actual == "FLOAT",
        "STRING" | "VARCHAR" => actual == "STRING",
        "POINT" => actual == "POINT",
        "MAP" => actual == "MAP",
        "NODE" | "VERTEX" => actual == "NODE",
        "RELATIONSHIP" | "EDGE" => actual == "RELATIONSHIP",
        "PATH" => actual == "PATH",
        "DATE" => actual == "DATE",
        "LOCAL TIME" => actual == "LOCAL TIME",
        "ZONED TIME" | "TIME" => actual == "ZONED TIME",
        "LOCAL DATETIME" => actual == "LOCAL DATETIME",
        "ZONED DATETIME" | "DATETIME" => actual == "ZONED DATETIME",
        "DURATION" => actual == "DURATION",
        _ => false,
    }
}

/// Property `key` of a non-entity value (map, materialized entity, temporal); a type error on a
/// scalar.
pub fn get_property(val: &CypherValue, key: &str) -> Result<CypherValue, CypherError> {
    Ok(match val {
        CypherValue::Node { properties, .. } => properties
            .get(key)
            .map(CypherValue::from_property)
            .unwrap_or(CypherValue::Null),
        CypherValue::Edge { properties, .. } => properties
            .get(key)
            .map(CypherValue::from_property)
            .unwrap_or(CypherValue::Null),
        CypherValue::Map(m) => m.get(key).cloned().unwrap_or(CypherValue::Null),
        CypherValue::Date(_)
        | CypherValue::LocalTime(_)
        | CypherValue::Time(_)
        | CypherValue::LocalDateTime(_)
        | CypherValue::DateTime(_)
        | CypherValue::Duration(_) => {
            temporal::get_temporal_property(val, key).unwrap_or(CypherValue::Null)
        }
        CypherValue::Null => CypherValue::Null,
        other => {
            return Err(CypherError::type_error(format!(
                "Type mismatch: expected a map, node, relationship, point or temporal \
                 value for property access, got {}",
                other.type_name()
            )));
        }
    })
}

/// Property `key` of any value, entities read through the graph. A deleted entity is an error.
pub fn property_of(
    val: &CypherValue,
    key: &str,
    graph: &dyn GraphView,
) -> Result<CypherValue, CypherError> {
    match val {
        CypherValue::NodeRef(id) => match graph.get_node_property(*id, key) {
            Ok(Some(pv)) => Ok(CypherValue::from_property(&pv)),
            Ok(None) => Ok(CypherValue::Null),
            Err(_) => Err(deleted_entity()),
        },
        CypherValue::EdgeRef { id, .. } => match graph.get_edge_property(*id, key) {
            Ok(Some(pv)) => Ok(CypherValue::from_property(&pv)),
            Ok(None) => Ok(CypherValue::Null),
            Err(_) => Err(deleted_entity()),
        },
        // virtual entities (APOC vNode/vRelationship, ids in the top range) are pure values
        CypherValue::Node { id, .. } if id.0 < VIRTUAL_ID_BASE && graph.get_node(*id).is_err() => {
            Err(deleted_entity())
        }
        CypherValue::Edge { id, .. } if id.0 < VIRTUAL_ID_BASE && graph.get_edge(*id).is_err() => {
            Err(deleted_entity())
        }
        _ => get_property(val, key),
    }
}

/// The error for reading a deleted node or relationship.
pub fn deleted_entity() -> CypherError {
    CypherError::semantic("deleted entity access".to_string())
}

/// `list[idx]`, `map['key']`, `node['key']`.
pub fn index_value(
    base: &CypherValue,
    idx: &CypherValue,
    graph: &dyn GraphView,
) -> Result<CypherValue, CypherError> {
    match (base, idx) {
        (CypherValue::List(items), CypherValue::Integer(i)) => {
            let index = if *i < 0 { items.len() as i64 + *i } else { *i };
            Ok(if index < 0 {
                CypherValue::Null
            } else {
                items
                    .get(index as usize)
                    .cloned()
                    .unwrap_or(CypherValue::Null)
            })
        }
        (CypherValue::List(_), CypherValue::String(_)) => Err(CypherError::type_error(
            "list indexing requires an integer".to_string(),
        )),
        (CypherValue::NodeRef(_) | CypherValue::EdgeRef { .. }, CypherValue::String(key)) => {
            property_of(base, key, graph)
        }
        (_, CypherValue::String(key)) => get_property(base, key),
        (CypherValue::Null, _) | (_, CypherValue::Null) => Ok(CypherValue::Null),
        _ => Err(CypherError::type_error(
            "list indexing type error".to_string(),
        )),
    }
}

/// `list[from..to]`.
pub fn slice_value(
    base: &CypherValue,
    from: Option<&CypherValue>,
    to: Option<&CypherValue>,
) -> Result<CypherValue, CypherError> {
    match base {
        CypherValue::List(items) => {
            let len = items.len() as i64;
            let start = match from {
                Some(CypherValue::Integer(i)) => {
                    if *i < 0 {
                        (len + i).max(0) as usize
                    } else {
                        *i as usize
                    }
                }
                Some(CypherValue::Null) => return Ok(CypherValue::Null),
                Some(_) => 0,
                None => 0,
            };
            let end = match to {
                Some(CypherValue::Integer(i)) => {
                    if *i < 0 {
                        (len + i).max(0) as usize
                    } else {
                        (*i).min(len) as usize
                    }
                }
                Some(CypherValue::Null) => return Ok(CypherValue::Null),
                Some(_) => items.len(),
                None => items.len(),
            };
            let end = end.min(items.len());
            let start = start.min(end);
            Ok(CypherValue::List(items[start..end].to_vec()))
        }
        CypherValue::Null => Ok(CypherValue::Null),
        _ => Err(CypherError::type_error("slice requires a list".to_string())),
    }
}

/// `labels(n)` / `:Label` test on a value, reading nodes through the graph. Null for a missing
/// node, false for a non-entity.
pub fn has_label(
    val: &CypherValue,
    label: &str,
    graph: &dyn GraphView,
) -> Result<CypherValue, CypherError> {
    Ok(match val {
        CypherValue::Node { labels, .. } => CypherValue::Bool(labels.iter().any(|l| l == label)),
        CypherValue::NodeRef(id) => match graph.strings().get(label) {
            Some(raw) => match graph.has_label(*id, traverse_core::graph::types::LabelId(raw)) {
                Ok(b) => {
                    if !b && graph.get_node(*id).is_err() {
                        CypherValue::Null
                    } else {
                        CypherValue::Bool(b)
                    }
                }
                Err(_) => CypherValue::Null,
            },
            None => {
                if graph.get_node(*id).is_ok() {
                    CypherValue::Bool(false)
                } else {
                    CypherValue::Null
                }
            }
        },
        CypherValue::Edge { edge_type, .. } => CypherValue::Bool(edge_type == label),
        CypherValue::EdgeRef { edge_type_id, .. } => {
            CypherValue::Bool(graph.strings().resolve(edge_type_id.0) == label)
        }
        CypherValue::Null => CypherValue::Null,
        _ => CypherValue::Bool(false),
    })
}

/// A map projection's `.*` entries of a value.
pub fn all_properties_of(
    base: &CypherValue,
    graph: &dyn GraphView,
) -> BTreeMap<String, CypherValue> {
    let mut map = BTreeMap::new();
    let materialized = match base {
        CypherValue::NodeRef(_) | CypherValue::EdgeRef { .. } => {
            super::super::values::resolve_ref(base.clone(), graph)
        }
        other => other.clone(),
    };
    match materialized {
        CypherValue::Node { properties, .. } | CypherValue::Edge { properties, .. } => {
            for (k, pv) in properties.iter() {
                map.insert(k.clone(), CypherValue::from_property(pv));
            }
        }
        CypherValue::Map(m) => {
            for (k, v) in m {
                map.insert(k, v);
            }
        }
        _ => {}
    }
    map
}
