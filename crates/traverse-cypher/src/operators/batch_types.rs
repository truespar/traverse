//! PropertyValue-native types for batch scan / aggregate operations.

use crate::error::CypherError;
use std::cmp::Ordering;
use traverse_core::graph::property::PropertyValue;

/// Hashable group key straight from PropertyValue, skipping PropertyValue -> CypherValue -> JoinKey.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum PropJoinKey {
    Null,
    Bool(bool),
    Integer(i64),
    Float(u64), // f64 bits
    Str(String),
    Other(String), // Debug fallback
}

/// `Option<PropertyValue>` -> `PropJoinKey` directly: Int is a match + copy; strings skip the 6
/// `strip_prefix` checks `CypherValue::from_property()` does.
#[inline(always)]
pub(crate) fn prop_to_join_key(pv: &Option<PropertyValue>) -> PropJoinKey {
    match pv {
        None | Some(PropertyValue::Null) => PropJoinKey::Null,
        Some(PropertyValue::Bool(b)) => PropJoinKey::Bool(*b),
        Some(PropertyValue::Int(i)) => PropJoinKey::Integer(*i),
        Some(PropertyValue::Float(f)) => {
            // whole floats -> Integer (as cypher_to_join_key)
            if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
                PropJoinKey::Integer(*f as i64)
            } else {
                PropJoinKey::Float(f.to_bits())
            }
        }
        Some(PropertyValue::String(s)) => PropJoinKey::Str(s.clone()),
        Some(other) => PropJoinKey::Other(format!("{other:?}")),
    }
}

/// Orders two PropertyValues for min/max; Int/Float compare across (callers skip Null).
#[inline(always)]
pub(crate) fn compare_property_values(a: &PropertyValue, b: &PropertyValue) -> Ordering {
    match (a, b) {
        (PropertyValue::Int(x), PropertyValue::Int(y)) => x.cmp(y),
        (PropertyValue::Float(x), PropertyValue::Float(y)) => {
            x.partial_cmp(y).unwrap_or(Ordering::Equal)
        }
        (PropertyValue::Int(x), PropertyValue::Float(y)) => {
            (*x as f64).partial_cmp(y).unwrap_or(Ordering::Equal)
        }
        (PropertyValue::Float(x), PropertyValue::Int(y)) => {
            x.partial_cmp(&(*y as f64)).unwrap_or(Ordering::Equal)
        }
        (PropertyValue::String(x), PropertyValue::String(y)) => x.cmp(y),
        (PropertyValue::Bool(x), PropertyValue::Bool(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// PropertyValue -> CypherValue, once per group at finalization, not per node.
#[inline(always)]
pub(crate) fn prop_to_cypher(pv: &PropertyValue) -> crate::types::CypherValue {
    crate::types::CypherValue::from_property(pv)
}

// ---------------------------------------------------------------------------
// Column-oriented accumulation
//
// Each function walks one column of the batch buffer. The ScanAcc destructuring
// happens at the call site once per chunk, not per row; `#[inline(always)]`
// inlines the tight loop there.
// ---------------------------------------------------------------------------

/// Non-null values in a column.
#[inline(always)]
pub(crate) fn acc_count_column(
    c: &mut i64,
    buf: &[Option<PropertyValue>],
    stride: usize,
    col_offset: usize,
    count: usize,
) {
    for i in 0..count {
        let val = &buf[i * stride + col_offset];
        if val.is_some() && !matches!(val, Some(PropertyValue::Null)) {
            *c += 1;
        }
    }
}

/// Sum of a column (Int + Float).
#[inline(always)]
pub(crate) fn acc_sum_column(
    int_sum: &mut i64,
    float_sum: &mut f64,
    has_float: &mut bool,
    has_any: &mut bool,
    buf: &[Option<PropertyValue>],
    stride: usize,
    col_offset: usize,
    count: usize,
) -> Result<(), CypherError> {
    for i in 0..count {
        if let Some(pv) = &buf[i * stride + col_offset] {
            match pv {
                PropertyValue::Int(v) => {
                    *has_any = true;
                    *int_sum += v;
                    *float_sum += *v as f64;
                }
                PropertyValue::Float(v) => {
                    *has_any = true;
                    *has_float = true;
                    *float_sum += v;
                }
                PropertyValue::Null => {}
                _ => {
                    return Err(CypherError::type_error(format!(
                        "sum() requires numeric values, got {:?}",
                        pv
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Average of a column (Int + Float).
#[inline(always)]
pub(crate) fn acc_avg_column(
    sum: &mut f64,
    acc_count: &mut i64,
    buf: &[Option<PropertyValue>],
    stride: usize,
    col_offset: usize,
    count: usize,
) -> Result<(), CypherError> {
    for i in 0..count {
        if let Some(pv) = &buf[i * stride + col_offset] {
            match pv {
                PropertyValue::Int(v) => {
                    *sum += *v as f64;
                    *acc_count += 1;
                }
                PropertyValue::Float(v) => {
                    *sum += v;
                    *acc_count += 1;
                }
                PropertyValue::Null => {}
                _ => {
                    return Err(CypherError::type_error(format!(
                        "avg() requires numeric values, got {:?}",
                        pv
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Min of a column.
#[inline(always)]
pub(crate) fn acc_min_column(
    best: &mut Option<PropertyValue>,
    buf: &[Option<PropertyValue>],
    stride: usize,
    col_offset: usize,
    count: usize,
) {
    for i in 0..count {
        if let Some(pv) = &buf[i * stride + col_offset]
            && !matches!(pv, PropertyValue::Null)
            && (best.is_none()
                || compare_property_values(pv, best.as_ref().unwrap()) == Ordering::Less)
        {
            *best = Some(pv.clone());
        }
    }
}

/// Max of a column.
#[inline(always)]
pub(crate) fn acc_max_column(
    best: &mut Option<PropertyValue>,
    buf: &[Option<PropertyValue>],
    stride: usize,
    col_offset: usize,
    count: usize,
) {
    for i in 0..count {
        if let Some(pv) = &buf[i * stride + col_offset]
            && !matches!(pv, PropertyValue::Null)
            && (best.is_none()
                || compare_property_values(pv, best.as_ref().unwrap()) == Ordering::Greater)
        {
            *best = Some(pv.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Typed i64 accumulation
//
// On flat i64 + bool buffers from batch_read_node_properties_i64: no
// PropertyValue construction, plain scalar arithmetic.
// ---------------------------------------------------------------------------

/// Valid (non-null) entries in a column.
#[inline(always)]
pub(crate) fn acc_count_valid(
    c: &mut i64,
    valid: &[bool],
    stride: usize,
    col_offset: usize,
    count: usize,
) {
    for i in 0..count {
        if valid[i * stride + col_offset] {
            *c += 1;
        }
    }
}

/// Sum of a typed i64 column.
#[inline(always)]
pub(crate) fn acc_sum_column_i64(
    int_sum: &mut i64,
    float_sum: &mut f64,
    has_any: &mut bool,
    vals: &[i64],
    valid: &[bool],
    stride: usize,
    col_offset: usize,
    count: usize,
) {
    for i in 0..count {
        let idx = i * stride + col_offset;
        if valid[idx] {
            let v = vals[idx];
            *has_any = true;
            *int_sum += v;
            *float_sum += v as f64;
        }
    }
}

/// Average of a typed i64 column.
#[inline(always)]
pub(crate) fn acc_avg_column_i64(
    sum: &mut f64,
    acc_count: &mut i64,
    vals: &[i64],
    valid: &[bool],
    stride: usize,
    col_offset: usize,
    count: usize,
) {
    for i in 0..count {
        let idx = i * stride + col_offset;
        if valid[idx] {
            *sum += vals[idx] as f64;
            *acc_count += 1;
        }
    }
}

/// Min of a typed i64 column.
#[inline(always)]
pub(crate) fn acc_min_column_i64(
    best: &mut Option<PropertyValue>,
    vals: &[i64],
    valid: &[bool],
    stride: usize,
    col_offset: usize,
    count: usize,
) {
    let mut local_min: Option<i64> = match best {
        Some(PropertyValue::Int(v)) => Some(*v),
        _ => None,
    };
    for i in 0..count {
        let idx = i * stride + col_offset;
        if valid[idx] {
            let v = vals[idx];
            if local_min.is_none_or(|m| v < m) {
                local_min = Some(v);
            }
        }
    }
    if let Some(m) = local_min {
        *best = Some(PropertyValue::Int(m));
    }
}

/// Max of a typed i64 column.
#[inline(always)]
pub(crate) fn acc_max_column_i64(
    best: &mut Option<PropertyValue>,
    vals: &[i64],
    valid: &[bool],
    stride: usize,
    col_offset: usize,
    count: usize,
) {
    let mut local_max: Option<i64> = match best {
        Some(PropertyValue::Int(v)) => Some(*v),
        _ => None,
    };
    for i in 0..count {
        let idx = i * stride + col_offset;
        if valid[idx] {
            let v = vals[idx];
            if local_max.is_none_or(|m| v > m) {
                local_max = Some(v);
            }
        }
    }
    if let Some(m) = local_max {
        *best = Some(PropertyValue::Int(m));
    }
}

/// Is the first non-null value of a column an integer?
#[inline(always)]
pub(crate) fn column_is_integer(
    buf: &[Option<PropertyValue>],
    stride: usize,
    col_offset: usize,
    count: usize,
) -> bool {
    for i in 0..count {
        match &buf[i * stride + col_offset] {
            Some(PropertyValue::Int(_)) => return true,
            Some(PropertyValue::Null) | None => continue,
            _ => return false,
        }
    }
    false // all null: irrelevant, the PropJoinKey path handles it
}
