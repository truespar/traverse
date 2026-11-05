//! Typed kernels over operands: comparison, arithmetic, logic, null tests and string
//! predicates. Each kernel takes a fast path when both operands are typed vectors (or one is a
//! typed scalar) and otherwise runs the scalar semantics lane by lane.

use std::borrow::Cow;

use crate::ast::{BinOp, CmpOp, StringOp};
use crate::error::CypherError;
use crate::types::CypherValue;

use super::super::vector::{ValueRef, Vector, VectorBuilder};
use super::eval::Operand;
use super::scalar;

/// An owned or borrowed value of a lane, for the scalar semantics.
#[inline]
pub fn owned(v: ValueRef<'_>) -> Cow<'_, CypherValue> {
    match v {
        ValueRef::Any(cv) => Cow::Borrowed(cv),
        other => Cow::Owned(other.to_owned()),
    }
}

/// A three-valued boolean vector from `f(lane) -> Option<bool>`.
#[inline]
pub fn bool3_from<F: FnMut(usize) -> Option<bool>>(n: usize, mut f: F) -> Vector {
    let mut vals = Vec::with_capacity(n);
    let mut valid = Vec::with_capacity(n);
    for i in 0..n {
        match f(i) {
            Some(b) => {
                vals.push(b);
                valid.push(true);
            }
            None => {
                vals.push(false);
                valid.push(false);
            }
        }
    }
    Vector::Bool { vals, valid }
}

/// Three-valued boolean of a lane.
#[inline]
pub fn lane_bool3(v: ValueRef<'_>) -> Option<bool> {
    match v {
        ValueRef::Null => None,
        ValueRef::Bool(b) => Some(b),
        ValueRef::Any(CypherValue::Null) => None,
        ValueRef::Any(CypherValue::Bool(b)) => Some(*b),
        _ => Some(true),
    }
}

/// Comparison of two operands over `n` lanes.
pub fn compare(op: CmpOp, l: &Operand<'_>, r: &Operand<'_>, n: usize) -> Vector {
    // typed fast paths
    match (l.typed(), r.typed()) {
        (
            Some(Typed::Int(Lanes::Col {
                vals,
                valid,
                sel: None,
            })),
            Some(Typed::Int(Lanes::Scalar(Some(k)))),
        ) if vals.len() == n => {
            let out: Vec<bool> = match op {
                CmpOp::Eq => vals.iter().map(|v| *v == k).collect(),
                CmpOp::Neq => vals.iter().map(|v| *v != k).collect(),
                CmpOp::Lt => vals.iter().map(|v| *v < k).collect(),
                CmpOp::Lte => vals.iter().map(|v| *v <= k).collect(),
                CmpOp::Gt => vals.iter().map(|v| *v > k).collect(),
                CmpOp::Gte => vals.iter().map(|v| *v >= k).collect(),
            };
            return Vector::Bool {
                vals: out,
                valid: valid.to_vec(),
            };
        }
        (
            Some(Typed::Float(Lanes::Col {
                vals,
                valid,
                sel: None,
            })),
            Some(Typed::Float(Lanes::Scalar(Some(k)))),
        ) if vals.len() == n && !k.is_nan() => {
            let out: Vec<bool> = match op {
                CmpOp::Eq => vals.iter().map(|v| *v == k).collect(),
                CmpOp::Neq => vals.iter().map(|v| *v != k).collect(),
                CmpOp::Lt => vals.iter().map(|v| *v < k).collect(),
                CmpOp::Lte => vals.iter().map(|v| *v <= k).collect(),
                CmpOp::Gt => vals.iter().map(|v| *v > k).collect(),
                CmpOp::Gte => vals.iter().map(|v| *v >= k).collect(),
            };
            // a NaN lane compares the scalar way: unequal to everything
            let out: Vec<bool> = out
                .into_iter()
                .zip(vals)
                .map(|(b, v)| if v.is_nan() { op == CmpOp::Neq } else { b })
                .collect();
            return Vector::Bool {
                vals: out,
                valid: valid.to_vec(),
            };
        }
        (Some(Typed::Int(li)), Some(Typed::Int(ri))) => {
            return bool3_from(n, |i| match (li.get(i), ri.get(i)) {
                (Some(a), Some(b)) => Some(scalar::ordering_matches(a.cmp(&b), op)),
                _ => None,
            });
        }
        (Some(Typed::Int(li)), Some(Typed::Float(rf))) => {
            return bool3_from(n, |i| match (li.get(i), rf.get(i)) {
                (Some(a), Some(b)) => cmp_f64(a as f64, b, op),
                _ => None,
            });
        }
        (Some(Typed::Float(lf)), Some(Typed::Int(ri))) => {
            return bool3_from(n, |i| match (lf.get(i), ri.get(i)) {
                (Some(a), Some(b)) => cmp_f64(a, b as f64, op),
                _ => None,
            });
        }
        (Some(Typed::Float(lf)), Some(Typed::Float(rf))) => {
            return bool3_from(n, |i| match (lf.get(i), rf.get(i)) {
                (Some(a), Some(b)) => cmp_f64(a, b, op),
                _ => None,
            });
        }
        (Some(Typed::Str(ls)), Some(Typed::Str(rs))) => {
            return bool3_from(n, |i| match (ls.get(i), rs.get(i)) {
                (Some(a), Some(b)) => Some(scalar::ordering_matches(a.cmp(b), op)),
                _ => None,
            });
        }
        (Some(Typed::Bool(lb)), Some(Typed::Bool(rb))) => {
            return bool3_from(n, |i| match (lb.get(i), rb.get(i)) {
                (Some(a), Some(b)) => Some(scalar::ordering_matches(a.cmp(&b), op)),
                _ => None,
            });
        }
        (Some(Typed::Node(ln)), Some(Typed::Node(rn))) if matches!(op, CmpOp::Eq | CmpOp::Neq) => {
            return bool3_from(n, |i| match (ln.get(i), rn.get(i)) {
                (Some(a), Some(b)) => Some((a == b) == (op == CmpOp::Eq)),
                _ => None,
            });
        }
        _ => {}
    }
    // generic: scalar semantics per lane
    bool3_from(n, |i| {
        let a = owned(l.get(i));
        let b = owned(r.get(i));
        scalar::as_bool3(&scalar::eval_comparison(&a, op, &b))
    })
}

#[inline]
fn cmp_f64(a: f64, b: f64, op: CmpOp) -> Option<bool> {
    if a.is_nan() || b.is_nan() {
        return Some(matches!(op, CmpOp::Neq));
    }
    match op {
        CmpOp::Eq => Some(a == b),
        CmpOp::Neq => Some(a != b),
        CmpOp::Lt => Some(a < b),
        CmpOp::Lte => Some(a <= b),
        CmpOp::Gt => Some(a > b),
        CmpOp::Gte => Some(a >= b),
    }
}

/// Arithmetic over `n` lanes.
pub fn arith(op: BinOp, l: &Operand<'_>, r: &Operand<'_>, n: usize) -> Result<Vector, CypherError> {
    match (l.typed(), r.typed()) {
        (Some(Typed::Int(li)), Some(Typed::Int(ri)))
            if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul) =>
        {
            let mut vals = Vec::with_capacity(n);
            let mut valid = Vec::with_capacity(n);
            for i in 0..n {
                match (li.get(i), ri.get(i)) {
                    (Some(a), Some(b)) => {
                        let v = match op {
                            BinOp::Add => a.checked_add(b),
                            BinOp::Sub => a.checked_sub(b),
                            _ => a.checked_mul(b),
                        };
                        match v {
                            Some(v) => {
                                vals.push(v);
                                valid.push(true);
                            }
                            None => {
                                return Err(CypherError::type_error("long overflow".to_string())
                                    .with_status(crate::status::GqlStatus::NumericOutOfRange));
                            }
                        }
                    }
                    _ => {
                        vals.push(0);
                        valid.push(false);
                    }
                }
            }
            return Ok(Vector::Int { vals, valid });
        }
        (Some(Typed::Float(lf)), Some(Typed::Float(rf)))
            if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div) =>
        {
            let mut vals = Vec::with_capacity(n);
            let mut valid = Vec::with_capacity(n);
            for i in 0..n {
                match (lf.get(i), rf.get(i)) {
                    (Some(a), Some(b)) => {
                        vals.push(match op {
                            BinOp::Add => a + b,
                            BinOp::Sub => a - b,
                            BinOp::Mul => a * b,
                            _ => a / b,
                        });
                        valid.push(true);
                    }
                    _ => {
                        vals.push(0.0);
                        valid.push(false);
                    }
                }
            }
            return Ok(Vector::Float { vals, valid });
        }
        _ => {}
    }
    let mut b = VectorBuilder::with_capacity(n);
    for i in 0..n {
        let a = owned(l.get(i));
        let c = owned(r.get(i));
        b.push_owned(scalar::eval_binary_op(&a, op, &c)?);
    }
    Ok(b.finish())
}

/// `NOT`.
pub fn not(e: &Operand<'_>, n: usize) -> Vector {
    bool3_from(n, |i| lane_bool3(e.get(i)).map(|b| !b))
}

/// Unary minus.
pub fn neg(e: &Operand<'_>, n: usize) -> Result<Vector, CypherError> {
    if let Some(Typed::Int(ti)) = e.typed() {
        let mut vals = Vec::with_capacity(n);
        let mut valid = Vec::with_capacity(n);
        for i in 0..n {
            match ti.get(i) {
                Some(v) => match v.checked_neg() {
                    Some(nv) => {
                        vals.push(nv);
                        valid.push(true);
                    }
                    None => {
                        return Err(CypherError::type_error("long overflow".to_string())
                            .with_status(crate::status::GqlStatus::NumericOutOfRange));
                    }
                },
                None => {
                    vals.push(0);
                    valid.push(false);
                }
            }
        }
        return Ok(Vector::Int { vals, valid });
    }
    let mut b = VectorBuilder::with_capacity(n);
    for i in 0..n {
        b.push_owned(scalar::negate(&owned(e.get(i)))?);
    }
    Ok(b.finish())
}

/// `IS NULL` / `IS NOT NULL`.
pub fn is_null(e: &Operand<'_>, n: usize, negated: bool) -> Vector {
    let mut vals = Vec::with_capacity(n);
    for i in 0..n {
        vals.push(e.get(i).is_null() != negated);
    }
    Vector::bools(vals)
}

/// XOR.
pub fn xor(l: &Operand<'_>, r: &Operand<'_>, n: usize) -> Vector {
    bool3_from(n, |i| match (lane_bool3(l.get(i)), lane_bool3(r.get(i))) {
        (Some(a), Some(b)) => Some(a ^ b),
        _ => None,
    })
}

/// A string predicate over `n` lanes: null unless both sides are strings.
pub fn string_op(
    op: StringOp,
    l: &Operand<'_>,
    r: &Operand<'_>,
    n: usize,
) -> Result<Vector, CypherError> {
    let mut vals = Vec::with_capacity(n);
    let mut valid = Vec::with_capacity(n);
    for i in 0..n {
        match (l.get(i), r.get(i)) {
            (ValueRef::Str(a), ValueRef::Str(b)) => {
                vals.push(scalar::string_op_str(a, op, b)?);
                valid.push(true);
            }
            (ValueRef::Any(CypherValue::String(a)), ValueRef::Str(b)) => {
                vals.push(scalar::string_op_str(a, op, b)?);
                valid.push(true);
            }
            (ValueRef::Str(a), ValueRef::Any(CypherValue::String(b))) => {
                vals.push(scalar::string_op_str(a, op, b)?);
                valid.push(true);
            }
            (ValueRef::Any(CypherValue::String(a)), ValueRef::Any(CypherValue::String(b))) => {
                vals.push(scalar::string_op_str(a, op, b)?);
                valid.push(true);
            }
            _ => {
                vals.push(false);
                valid.push(false);
            }
        }
    }
    Ok(Vector::Bool { vals, valid })
}

/// `item IN list` over `n` lanes.
pub fn in_list(item: &Operand<'_>, list: &Operand<'_>, n: usize) -> Result<Vector, CypherError> {
    // a constant list of scalars: hash set membership for typed items
    if let Operand::Scalar(CypherValue::List(items)) = list
        && !items.is_empty()
        && items.iter().all(|v| matches!(v, CypherValue::Integer(_)))
        && let Some(Typed::Int(ti)) = item.typed()
    {
        let set: rustc_hash::FxHashSet<i64> = items.iter().filter_map(|v| v.as_integer()).collect();
        return Ok(bool3_from(n, |i| ti.get(i).map(|v| set.contains(&v))));
    }
    if let Operand::Scalar(CypherValue::List(items)) = list
        && !items.is_empty()
        && items.iter().all(|v| matches!(v, CypherValue::String(_)))
        && let Some(Typed::Str(ts)) = item.typed()
    {
        let set: rustc_hash::FxHashSet<&str> = items.iter().filter_map(|v| v.as_str()).collect();
        return Ok(bool3_from(n, |i| ts.get(i).map(|v| set.contains(v))));
    }
    let mut b = VectorBuilder::with_capacity(n);
    for i in 0..n {
        let it = owned(item.get(i));
        let li = owned(list.get(i));
        b.push_owned(scalar::in_list(&it, &li)?);
    }
    Ok(b.finish())
}

/// A typed view of an operand, when it has one.
pub enum Typed<'a> {
    /// Integers.
    Int(Lanes<'a, i64>),
    /// Floats.
    Float(Lanes<'a, f64>),
    /// Booleans.
    Bool(Lanes<'a, bool>),
    /// Strings.
    Str(StrLanes<'a>),
    /// Nodes.
    Node(Lanes<'a, traverse_core::graph::types::NodeId>),
}

/// Typed lanes of a vector, through a selection, or a broadcast scalar.
pub enum Lanes<'a, T: Copy> {
    /// A column and its selection.
    Col {
        /// Values.
        vals: &'a [T],
        /// Validity.
        valid: &'a [bool],
        /// Selection, if any.
        sel: Option<&'a [u32]>,
    },
    /// One value for every lane.
    Scalar(Option<T>),
}

impl<T: Copy> Lanes<'_, T> {
    /// Value at selected position `i`, `None` for null.
    #[inline]
    pub fn get(&self, i: usize) -> Option<T> {
        match self {
            Lanes::Col { vals, valid, sel } => {
                let lane = match sel {
                    Some(s) => s[i] as usize,
                    None => i,
                };
                if valid[lane] { Some(vals[lane]) } else { None }
            }
            Lanes::Scalar(v) => *v,
        }
    }
}

/// String lanes of a vector, through a selection, or a broadcast scalar.
pub enum StrLanes<'a> {
    /// A string column and its selection.
    Col {
        /// Boundaries.
        offsets: &'a [u32],
        /// Bytes.
        data: &'a [u8],
        /// Validity.
        valid: &'a [bool],
        /// Selection, if any.
        sel: Option<&'a [u32]>,
    },
    /// One value for every lane.
    Scalar(Option<&'a str>),
}

impl<'a> StrLanes<'a> {
    /// Value at selected position `i`.
    #[inline]
    pub fn get(&self, i: usize) -> Option<&'a str> {
        match self {
            StrLanes::Col {
                offsets,
                data,
                valid,
                sel,
            } => {
                let lane = match sel {
                    Some(s) => s[i] as usize,
                    None => i,
                };
                if valid[lane] {
                    let bytes = &data[offsets[lane] as usize..offsets[lane + 1] as usize];
                    // SAFETY: string lanes are pushed from `&str`
                    Some(unsafe { std::str::from_utf8_unchecked(bytes) })
                } else {
                    None
                }
            }
            StrLanes::Scalar(v) => *v,
        }
    }
}

impl<'a> Operand<'a> {
    /// The typed view of this operand, if its vector (or scalar) has one.
    pub fn typed(&self) -> Option<Typed<'_>> {
        match self {
            Operand::Scalar(v) => match v {
                CypherValue::Integer(i) => Some(Typed::Int(Lanes::Scalar(Some(*i)))),
                CypherValue::Float(f) => Some(Typed::Float(Lanes::Scalar(Some(*f)))),
                CypherValue::Bool(b) => Some(Typed::Bool(Lanes::Scalar(Some(*b)))),
                CypherValue::String(s) => Some(Typed::Str(StrLanes::Scalar(Some(s)))),
                CypherValue::NodeRef(id) => Some(Typed::Node(Lanes::Scalar(Some(*id)))),
                _ => None,
            },
            Operand::Ref(v, sel) => typed_of(v, *sel),
            Operand::Owned(v) => typed_of(v, None),
        }
    }
}

fn typed_of<'a>(v: &'a Vector, sel: Option<&'a [u32]>) -> Option<Typed<'a>> {
    match v {
        Vector::Int { vals, valid } => Some(Typed::Int(Lanes::Col { vals, valid, sel })),
        Vector::Float { vals, valid } => Some(Typed::Float(Lanes::Col { vals, valid, sel })),
        Vector::Bool { vals, valid } => Some(Typed::Bool(Lanes::Col { vals, valid, sel })),
        Vector::Node { ids, valid } => Some(Typed::Node(Lanes::Col {
            vals: ids,
            valid,
            sel,
        })),
        Vector::Str {
            offsets,
            data,
            valid,
        } => Some(Typed::Str(StrLanes::Col {
            offsets,
            data,
            valid,
            sel,
        })),
        _ => None,
    }
}
