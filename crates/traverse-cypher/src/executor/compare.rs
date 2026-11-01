//! Comparison and equality with Cypher's null and type rules, and the total order ORDER BY uses.

use crate::ast::CmpOp;
use crate::types::CypherValue;

pub(crate) fn eval_comparison(left: &CypherValue, op: CmpOp, right: &CypherValue) -> CypherValue {
    if left.is_null() || right.is_null() {
        return CypherValue::Null;
    }
    match op {
        CmpOp::Eq => cypher_eq(left, right),
        CmpOp::Neq => match cypher_eq(left, right) {
            CypherValue::Bool(b) => CypherValue::Bool(!b),
            other => other, // null propagates
        },
        _ => {
            // list comparison with null propagation
            if let (CypherValue::List(la), CypherValue::List(lb)) = (left, right) {
                return list_compare(la, lb, op);
            }
            // incomparable types order to null
            if !are_order_comparable(left, right) {
                return CypherValue::Null;
            }
            // NaN compares to nothing (IEEE 754), only within numeric types
            let has_nan = matches!(left, CypherValue::Float(f) if f.is_nan())
                || matches!(right, CypherValue::Float(f) if f.is_nan());
            if has_nan {
                return CypherValue::Bool(false);
            }
            let ord = compare_values(left, right);
            let result = match op {
                CmpOp::Lt => ord == std::cmp::Ordering::Less,
                CmpOp::Lte => ord != std::cmp::Ordering::Greater,
                CmpOp::Gt => ord == std::cmp::Ordering::Greater,
                CmpOp::Gte => ord != std::cmp::Ordering::Less,
                _ => unreachable!(),
            };
            CypherValue::Bool(result)
        }
    }
}

/// Element-wise list comparison with null propagation.
fn list_compare(la: &[CypherValue], lb: &[CypherValue], op: CmpOp) -> CypherValue {
    let mut has_null = false;
    for (a, b) in la.iter().zip(lb.iter()) {
        let cmp = eval_comparison(a, op, b);
        if let CypherValue::Null = &cmp {
            has_null = true;
            continue;
        }
        // equal element: next
        let eq = eval_comparison(a, CmpOp::Eq, b);
        match eq {
            CypherValue::Bool(true) => continue,
            CypherValue::Null => {
                has_null = true;
                continue;
            }
            _ => {
                // first unequal element decides
                return cmp;
            }
        }
    }
    // all compared elements equal (or null)
    if la.len() == lb.len() {
        if has_null {
            return CypherValue::Null;
        }
        // same length, all equal
        return CypherValue::Bool(matches!(op, CmpOp::Lte | CmpOp::Gte));
    }
    // different lengths: longer list is greater
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

/// Three-valued Cypher equality: Bool(true), Bool(false) or Null.
pub(super) fn cypher_eq(left: &CypherValue, right: &CypherValue) -> CypherValue {
    match (left, right) {
        (CypherValue::Null, _) | (_, CypherValue::Null) => CypherValue::Null,
        // NaN equals nothing, itself included
        (CypherValue::Float(a), _) if a.is_nan() => CypherValue::Bool(false),
        (_, CypherValue::Float(b)) if b.is_nan() => CypherValue::Bool(false),
        // List equality with null propagation
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
        // Map equality with null propagation
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
        // everything else: PartialEq
        _ => CypherValue::Bool(left == right),
    }
}

/// Structural equality for procedure row matching; unlike `cypher_eq`, null == null is true.
pub(crate) fn proc_values_equal(a: &CypherValue, b: &CypherValue) -> bool {
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
fn are_order_comparable(a: &CypherValue, b: &CypherValue) -> bool {
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

pub(crate) fn compare_values(a: &CypherValue, b: &CypherValue) -> std::cmp::Ordering {
    // cross-type order (ascending): Map < Node < Edge < List < Path < String < Bool < Number < NaN
    // < Null
    fn type_rank(v: &CypherValue) -> u8 {
        match v {
            CypherValue::Bytes(_) => 0, // alongside maps at the low end
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
            | CypherValue::Duration(_) => 7, // temporal sorts with numbers
        }
    }
    // lists compare recursively with cross-type ordering
    if let (CypherValue::List(la), CypherValue::List(lb)) = (a, b) {
        for (ai, bi) in la.iter().zip(lb.iter()) {
            let ord = compare_values(ai, bi);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        return la.len().cmp(&lb.len());
    }
    // same-type comparison first
    if let Some(ord) = a.partial_cmp(b) {
        return ord;
    }
    type_rank(a).cmp(&type_rank(b))
}
