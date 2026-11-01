//! `apoc.number.*`, `apoc.math.*`, `apoc.bitwise.*` and `apoc.scoring.*`.

use crate::error::CypherError;
use crate::types::CypherValue;

use super::{int_arg, num_of, str_arg, with_str};

/// The pure functions of this module; `None` for other names.
pub(super) fn eval(name: &str, args: &[CypherValue]) -> Option<Result<CypherValue, CypherError>> {
    use CypherValue as V;
    let r = match name {
        "apoc.number.parseint" => number_parse_int(args),
        "apoc.number.parsefloat" => number_parse_float(args),
        "apoc.number.format" => number_format(args),
        "apoc.math.sinh" => math1(args, f64::sinh),
        "apoc.math.cosh" => math1(args, f64::cosh),
        "apoc.math.tanh" => math1(args, f64::tanh),
        "apoc.math.coth" => math1(args, |x| f64::cosh(x) / f64::sinh(x)),
        "apoc.math.sech" => math1(args, |x| 1.0 / f64::cosh(x)),
        "apoc.math.csch" => math1(args, |x| 1.0 / f64::sinh(x)),
        "apoc.math.sigmoid" => math1(args, |x| 1.0 / (1.0 + (-x).exp())),
        "apoc.math.sigmoidprime" => math1(args, |x| {
            let s = 1.0 / (1.0 + (-x).exp());
            s * (1.0 - s)
        }),
        "apoc.math.maxlong" => Ok(V::Integer(i64::MAX)),
        "apoc.math.minlong" => Ok(V::Integer(i64::MIN)),
        "apoc.math.maxint" => Ok(V::Integer(i32::MAX as i64)),
        "apoc.math.minint" => Ok(V::Integer(i32::MIN as i64)),
        "apoc.math.maxbyte" => Ok(V::Integer(127)),
        "apoc.math.minbyte" => Ok(V::Integer(-128)),
        "apoc.math.maxdouble" => Ok(V::Float(f64::MAX)),
        "apoc.math.mindouble" => Ok(V::Float(f64::MIN_POSITIVE)),
        "apoc.bitwise.op" => bitwise_op(args),
        "apoc.number.arabictoroman" => number_to_roman(args),
        "apoc.number.romantoarabic" => number_from_roman(args),
        "apoc.number.exact.add" => exact_binop(args, "add"),
        "apoc.number.exact.sub" => exact_binop(args, "sub"),
        "apoc.number.exact.mul" => exact_binop(args, "mul"),
        "apoc.number.exact.div" => exact_binop(args, "div"),
        "apoc.number.exact.toexact" => Ok(args.first().cloned().unwrap_or(V::Null)),
        "apoc.number.exact.tofloat" => match args.first() {
            Some(V::String(s)) => Ok(s.trim().parse::<f64>().map(V::Float).unwrap_or(V::Null)),
            _ => Ok(V::Null),
        },
        "apoc.number.exact.tointeger" => match args.first() {
            Some(V::String(s)) => Ok(s
                .trim()
                .parse::<rust_decimal::Decimal>()
                .ok()
                .and_then(|d| {
                    use rust_decimal::prelude::ToPrimitive;
                    d.round().to_i64()
                })
                .map(V::Integer)
                .unwrap_or(V::Null)),
            _ => Ok(V::Null),
        },
        "apoc.scoring.existence" => {
            let score = int_arg(args, 0).unwrap_or(0) as f64;
            let exists = matches!(args.get(1), Some(V::Bool(true)));
            Ok(V::Float(if exists { score } else { 0.0 }))
        }
        "apoc.scoring.pareto" => scoring_pareto(args),
        _ => return None,
    };
    Some(r)
}

fn number_parse_int(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let radix = int_arg(args, 1).unwrap_or(10).clamp(2, 36) as u32;
    with_str(args, |s| {
        i64::from_str_radix(s.trim(), radix)
            .map(CypherValue::Integer)
            .unwrap_or(CypherValue::Null)
    })
}

fn number_parse_float(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_str(args, |s| {
        s.trim()
            .parse::<f64>()
            .map(CypherValue::Float)
            .unwrap_or(CypherValue::Null)
    })
}

fn number_format(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    // Java DecimalFormat "#,##0.00"-style subset: thousands separators + fixed decimals from the
    // pattern
    let Some(n) = args.first().and_then(num_of) else {
        return Ok(CypherValue::Null);
    };
    let pattern = str_arg(args, 1).unwrap_or_else(|| "#,##0".to_string());
    let decimals = pattern.split('.').nth(1).map(|d| d.len()).unwrap_or(0);
    let grouped = pattern.contains(',');
    let s = format!("{:.*}", decimals, n);
    if !grouped {
        return Ok(CypherValue::String(s));
    }
    let (int_part, frac) = match s.split_once('.') {
        Some((i, f)) => (i.to_string(), Some(f.to_string())),
        None => (s, None),
    };
    let neg = int_part.starts_with('-');
    let digits: Vec<char> = int_part.trim_start_matches('-').chars().collect();
    let mut out = String::new();
    for (i, c) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*c);
    }
    let mut full = if neg { format!("-{out}") } else { out };
    if let Some(f) = frac {
        full.push('.');
        full.push_str(&f);
    }
    Ok(CypherValue::String(full))
}

fn math1(args: &[CypherValue], f: impl Fn(f64) -> f64) -> Result<CypherValue, CypherError> {
    match args.first().and_then(num_of) {
        Some(x) => Ok(CypherValue::Float(f(x))),
        None => Ok(CypherValue::Null),
    }
}

fn bitwise_op(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(a), Some(op), Some(b)) = (int_arg(args, 0), str_arg(args, 1), int_arg(args, 2))
    else {
        return Ok(CypherValue::Null);
    };
    let r = match op.as_str() {
        "&" | "AND" | "and" => a & b,
        "|" | "OR" | "or" => a | b,
        "^" | "XOR" | "xor" => a ^ b,
        "~" | "NOT" | "not" => !a,
        ">>" | "right shift" => a >> b,
        ">>>" | "right shift unsigned" => ((a as u64) >> b) as i64,
        "<<" | "left shift" => a << b,
        _ => {
            return Err(CypherError::semantic(format!(
                "invalid bitwise operator `{op}`"
            )));
        }
    };
    Ok(CypherValue::Integer(r))
}

fn number_to_roman(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(mut n) = args.first().and_then(num_of).map(|f| f as i64) else {
        return Ok(CypherValue::Null);
    };
    if !(1..=3999).contains(&n) {
        return Ok(CypherValue::Null);
    }
    const TABLE: &[(i64, &str)] = &[
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for (v, sym) in TABLE {
        while n >= *v {
            out.push_str(sym);
            n -= v;
        }
    }
    Ok(CypherValue::String(out))
}

fn number_from_roman(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(s) = str_arg(args, 0) else {
        return Ok(CypherValue::Null);
    };
    let val = |c: char| -> i64 {
        match c.to_ascii_uppercase() {
            'I' => 1,
            'V' => 5,
            'X' => 10,
            'L' => 50,
            'C' => 100,
            'D' => 500,
            'M' => 1000,
            _ => 0,
        }
    };
    let cs: Vec<i64> = s.chars().map(val).collect();
    let mut total = 0i64;
    for i in 0..cs.len() {
        if cs[i] == 0 {
            return Ok(CypherValue::Null);
        }
        if i + 1 < cs.len() && cs[i] < cs[i + 1] {
            total -= cs[i];
        } else {
            total += cs[i];
        }
    }
    Ok(CypherValue::Integer(total))
}

/// BigDecimal-style exact string arithmetic (apoc.number.exact.*).
fn exact_binop(args: &[CypherValue], op: &str) -> Result<CypherValue, CypherError> {
    use rust_decimal::Decimal;
    use std::str::FromStr;
    let (Some(a), Some(b)) = (str_arg(args, 0), str_arg(args, 1)) else {
        return Ok(CypherValue::Null);
    };
    let (Ok(da), Ok(db)) = (Decimal::from_str(a.trim()), Decimal::from_str(b.trim())) else {
        return Err(CypherError::semantic("invalid decimal string".to_string()));
    };
    let precision = int_arg(args, 2).unwrap_or(0).max(0) as u32;
    let r = match op {
        "add" => da.checked_add(db),
        "sub" => da.checked_sub(db),
        "mul" => da.checked_mul(db),
        _ => da.checked_div(db),
    };
    let Some(mut r) = r else {
        return Err(
            CypherError::semantic("exact arithmetic overflow".to_string())
                .with_status(crate::status::GqlStatus::NumericOutOfRange),
        );
    };
    if precision > 0 {
        r = r.round_dp(precision);
    }
    Ok(CypherValue::String(r.normalize().to_string()))
}

fn scoring_pareto(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(min_t), Some(eighty), Some(max_v), Some(score)) = (
        int_arg(args, 0),
        int_arg(args, 1),
        int_arg(args, 2),
        int_arg(args, 3),
    ) else {
        return Ok(CypherValue::Null);
    };
    if score < min_t {
        return Ok(CypherValue::Float(0.0));
    }
    // APOC: exponential pareto scaled so `eightyPercentValue` scores 0.8*max
    let alpha = (0.2f64).ln() / eighty as f64;
    Ok(CypherValue::Float(
        max_v as f64 * (1.0 - (alpha * score as f64).exp()),
    ))
}
