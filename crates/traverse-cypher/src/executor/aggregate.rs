//! Aggregation over frames: grouping keys and the aggregate functions.

use rustc_hash::{FxHashMap, FxHashSet};
use traverse_core::tx::view::GraphView;

use crate::ast::{Expr, ProjectionItem};
use crate::error::CypherError;
use crate::frame::{self, Frame, SymbolTable, new_frame};
use crate::planner::is_aggregate_function;
use crate::types::CypherValue;

use super::Record;
use super::compare::compare_values;
use super::describe::projection_name;
use super::eval::{eval_expr, eval_expr_rec};
use super::values::{JoinKey, cypher_to_join_key};

pub(crate) fn exec_aggregate(
    frames: &[Frame],
    keys: &[ProjectionItem],
    aggs: &[ProjectionItem],
    graph: &dyn GraphView,
    symtab: &SymbolTable,
) -> Result<Vec<Frame>, CypherError> {
    // group by key expressions, O(1) group lookup
    let mut group_map: FxHashMap<Vec<JoinKey>, usize> = FxHashMap::default();
    let mut groups: Vec<(Vec<CypherValue>, Vec<&Frame>)> = Vec::new();

    for frame in frames {
        let key_vals: Vec<CypherValue> = keys
            .iter()
            .map(|k| eval_expr(&k.expr, frame, graph, symtab))
            .collect::<Result<_, _>>()?;

        let hash_key: Vec<JoinKey> = key_vals
            .iter()
            .map(cypher_to_join_key)
            .collect::<Result<_, _>>()?;

        if let Some(&idx) = group_map.get(&hash_key) {
            groups[idx].1.push(frame);
        } else {
            let idx = groups.len();
            group_map.insert(hash_key, idx);
            groups.push((key_vals, vec![frame]));
        }
    }

    // empty input with aggregation and no keys: one row
    if groups.is_empty() && keys.is_empty() {
        groups.push((Vec::new(), Vec::new()));
    }

    let mut result = Vec::new();
    for (key_vals, group_frames) in &groups {
        let mut out = new_frame(symtab);

        for (i, key_item) in keys.iter().enumerate() {
            let name = projection_name(key_item);
            if let Some(slot) = symtab.slot(&name) {
                out[slot] = key_vals[i].clone().into();
            }
        }

        for agg_item in aggs {
            let val = eval_aggregate(&agg_item.expr, group_frames, graph, symtab)?;
            let name = projection_name(agg_item);
            if let Some(slot) = symtab.slot(&name) {
                out[slot] = val.into();
            }
        }

        result.push(out);
    }

    Ok(result)
}

fn eval_aggregate(
    expr: &Expr,
    frames: &[&Frame],
    graph: &dyn GraphView,
    symtab: &SymbolTable,
) -> Result<CypherValue, CypherError> {
    match expr {
        Expr::FunctionCall {
            name,
            distinct,
            args,
        } if is_aggregate_function(name) => {
            eval_direct_aggregate(name, *distinct, args, frames, graph, symtab)
        }
        // fast path collect(expr)[0..N]: collect at most N items instead of everything then slicing
        Expr::ListSlice(base, from_opt, to_opt) => {
            if let Expr::FunctionCall {
                name,
                distinct,
                args,
            } = base.as_ref()
                && name == "collect"
                && !distinct
                && args.len() == 1
            {
                let empty = new_frame(symtab);
                let first_frame = if frames.is_empty() { &empty } else { frames[0] };
                let from = match from_opt {
                    Some(e) => match eval_expr(e, first_frame, graph, symtab)? {
                        CypherValue::Integer(i) => i.max(0) as usize,
                        _ => 0,
                    },
                    None => 0,
                };
                let to = match to_opt {
                    Some(e) => match eval_expr(e, first_frame, graph, symtab)? {
                        CypherValue::Integer(i) => Some(i.max(0) as usize),
                        _ => None,
                    },
                    None => None,
                };
                // collect at most `to` items, skipping the first `from`
                let limit = to.unwrap_or(usize::MAX);
                let mut collected = Vec::with_capacity(limit.min(frames.len()).min(4096));
                let mut count = 0usize;
                for f in frames {
                    let val = eval_expr(&args[0], f, graph, symtab)?;
                    if !val.is_null() {
                        if count >= from && collected.len() < limit.saturating_sub(from) {
                            collected.push(val);
                        }
                        count += 1;
                        if collected.len() >= limit.saturating_sub(from) {
                            break;
                        }
                    }
                }
                // slice already applied: skipped `from`, capped at `to - from`
                return Ok(CypherValue::List(collected));
            }
            // not a collect slice: generic path
            eval_aggregate_generic(expr, frames, graph, symtab)
        }
        _ => eval_aggregate_generic(expr, frames, graph, symtab),
    }
}

fn eval_aggregate_generic(
    expr: &Expr,
    frames: &[&Frame],
    graph: &dyn GraphView,
    symtab: &SymbolTable,
) -> Result<CypherValue, CypherError> {
    // nested aggregate calls in the expression?
    let mut subs = Record::new();
    let mut counter = 0usize;
    let substituted = replace_agg_calls(expr, frames, graph, symtab, &mut counter, &mut subs)?;
    if counter > 0 {
        // nested aggregation: merge subs into the first record, cold path
        let recs: Vec<Record> = frames
            .iter()
            .map(|f| frame::frame_to_record(f, symtab))
            .collect();
        let mut rec = if recs.is_empty() {
            Record::new()
        } else {
            recs[0].clone()
        };
        rec.extend(subs);
        eval_expr_rec(&substituted, &rec, graph)
    } else {
        // no aggregation: evaluate directly
        if frames.is_empty() {
            Ok(CypherValue::Null)
        } else {
            eval_expr(expr, frames[0], graph, symtab)
        }
    }
}

/// Evaluates a direct aggregate call (count, sum, avg, ...).
fn eval_direct_aggregate(
    name: &str,
    distinct: bool,
    args: &[Expr],
    frames: &[&Frame],
    graph: &dyn GraphView,
    symtab: &SymbolTable,
) -> Result<CypherValue, CypherError> {
    // fast path count(DISTINCT expr): one pass, count only, no intermediate Vec
    if name == "count" && distinct && !args.is_empty() {
        let mut seen: FxHashSet<JoinKey> = FxHashSet::default();
        for f in frames {
            let val = eval_expr(&args[0], f, graph, symtab)?;
            if !val.is_null() {
                seen.insert(cypher_to_join_key(&val).unwrap_or(JoinKey::Null));
            }
        }
        return Ok(CypherValue::Integer(seen.len() as i64));
    }

    let mut values: Vec<CypherValue> = if args.is_empty() {
        // count(*)
        return Ok(CypherValue::Integer(frames.len() as i64));
    } else {
        frames
            .iter()
            .map(|f| eval_expr(&args[0], f, graph, symtab))
            .collect::<Result<_, _>>()?
    };

    if distinct {
        values = dedup_values(values);
    }

    // non-count aggregates skip nulls
    let non_null: Vec<&CypherValue> = values.iter().filter(|v| !v.is_null()).collect();

    match name {
        "count" => Ok(CypherValue::Integer(non_null.len() as i64)),
        "sum" => {
            let mut sum = 0.0_f64;
            let mut all_int = true;
            let mut int_sum = 0_i64;
            for v in &non_null {
                match v {
                    CypherValue::Integer(i) => {
                        int_sum += i;
                        sum += *i as f64;
                    }
                    CypherValue::Float(f) => {
                        all_int = false;
                        sum += f;
                    }
                    _ => {
                        return Err(CypherError::type_error(format!(
                            "sum() requires numeric values, got {}",
                            v.type_name()
                        )));
                    }
                }
            }
            if non_null.is_empty() {
                Ok(CypherValue::Integer(0))
            } else if all_int {
                Ok(CypherValue::Integer(int_sum))
            } else {
                Ok(CypherValue::Float(sum))
            }
        }
        "avg" => {
            if non_null.is_empty() {
                return Ok(CypherValue::Null);
            }
            let mut sum = 0.0_f64;
            for v in &non_null {
                match v.to_f64() {
                    Some(f) => sum += f,
                    None => {
                        return Err(CypherError::type_error(format!(
                            "avg() requires numeric values, got {}",
                            v.type_name()
                        )));
                    }
                }
            }
            Ok(CypherValue::Float(sum / non_null.len() as f64))
        }
        "min" => {
            if non_null.is_empty() {
                return Ok(CypherValue::Null);
            }
            let mut best = non_null[0].clone();
            for v in &non_null[1..] {
                if compare_values(v, &best) == std::cmp::Ordering::Less {
                    best = (*v).clone();
                }
            }
            Ok(best)
        }
        "max" => {
            if non_null.is_empty() {
                return Ok(CypherValue::Null);
            }
            let mut best = non_null[0].clone();
            for v in &non_null[1..] {
                if compare_values(v, &best) == std::cmp::Ordering::Greater {
                    best = (*v).clone();
                }
            }
            Ok(best)
        }
        "collect" => {
            let list: Vec<CypherValue> = non_null.into_iter().cloned().collect();
            Ok(CypherValue::List(list))
        }
        "percentiledisc" => {
            if non_null.is_empty() {
                return Ok(CypherValue::Null);
            }
            let pct = match args.get(1) {
                Some(pct_expr) => match eval_expr(pct_expr, frames[0], graph, symtab)? {
                    CypherValue::Float(f) => f,
                    CypherValue::Integer(i) => i as f64,
                    _ => {
                        return Err(CypherError::type_error(
                            "percentileDisc() requires a numeric percentile".to_string(),
                        ));
                    }
                },
                None => {
                    return Err(CypherError::type_error(
                        "percentileDisc() requires two arguments".to_string(),
                    ));
                }
            };
            if !(0.0..=1.0).contains(&pct) {
                return Err(CypherError::semantic(
                    "percentileDisc() percentile must be between 0.0 and 1.0".to_string(),
                ));
            }
            let mut nums: Vec<f64> = Vec::new();
            let mut all_int = true;
            for v in &non_null {
                match v {
                    CypherValue::Integer(i) => nums.push(*i as f64),
                    CypherValue::Float(f) => {
                        all_int = false;
                        nums.push(*f);
                    }
                    _ => {
                        return Err(CypherError::type_error(
                            "percentileDisc() requires numeric values".to_string(),
                        ));
                    }
                }
            }
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let idx = if pct == 0.0 {
                0
            } else {
                ((pct * nums.len() as f64).ceil() as usize).saturating_sub(1)
            };
            let idx = idx.min(nums.len() - 1);
            if all_int {
                Ok(CypherValue::Integer(nums[idx] as i64))
            } else {
                Ok(CypherValue::Float(nums[idx]))
            }
        }
        "percentilecont" => {
            if non_null.is_empty() {
                return Ok(CypherValue::Null);
            }
            let pct = match args.get(1) {
                Some(pct_expr) => match eval_expr(pct_expr, frames[0], graph, symtab)? {
                    CypherValue::Float(f) => f,
                    CypherValue::Integer(i) => i as f64,
                    _ => {
                        return Err(CypherError::type_error(
                            "percentileCont() requires a numeric percentile".to_string(),
                        ));
                    }
                },
                None => {
                    return Err(CypherError::type_error(
                        "percentileCont() requires two arguments".to_string(),
                    ));
                }
            };
            if !(0.0..=1.0).contains(&pct) {
                return Err(CypherError::semantic(
                    "percentileCont() percentile must be between 0.0 and 1.0".to_string(),
                ));
            }
            let mut nums: Vec<f64> = Vec::new();
            for v in &non_null {
                match v {
                    CypherValue::Integer(i) => nums.push(*i as f64),
                    CypherValue::Float(f) => nums.push(*f),
                    _ => {
                        return Err(CypherError::type_error(
                            "percentileCont() requires numeric values".to_string(),
                        ));
                    }
                }
            }
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            if nums.len() == 1 || pct == 0.0 {
                return Ok(CypherValue::Float(nums[0]));
            }
            if pct == 1.0 {
                return Ok(CypherValue::Float(nums[nums.len() - 1]));
            }
            let rank = pct * (nums.len() - 1) as f64;
            let lower = rank.floor() as usize;
            let upper = rank.ceil() as usize;
            let fraction = rank - lower as f64;
            let result = nums[lower] + fraction * (nums[upper] - nums[lower]);
            Ok(CypherValue::Float(result))
        }
        "stdev" => {
            if non_null.len() < 2 {
                return Ok(CypherValue::Float(0.0));
            }
            let mut nums: Vec<f64> = Vec::new();
            for v in &non_null {
                match v.to_f64() {
                    Some(f) => nums.push(f),
                    None => {
                        return Err(CypherError::type_error(format!(
                            "stDev() requires numeric values, got {}",
                            v.type_name()
                        )));
                    }
                }
            }
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            let variance =
                nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (nums.len() - 1) as f64;
            Ok(CypherValue::Float(variance.sqrt()))
        }
        "stdevp" => {
            if non_null.is_empty() {
                return Ok(CypherValue::Float(0.0));
            }
            let mut nums: Vec<f64> = Vec::new();
            for v in &non_null {
                match v.to_f64() {
                    Some(f) => nums.push(f),
                    None => {
                        return Err(CypherError::type_error(format!(
                            "stDevP() requires numeric values, got {}",
                            v.type_name()
                        )));
                    }
                }
            }
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            let variance = nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / nums.len() as f64;
            Ok(CypherValue::Float(variance.sqrt()))
        }
        // -- APOC aggregates (semantics match APOC core 2026.05) --
        "apoc.agg.first" => Ok(non_null
            .first()
            .map(|v| (*v).clone())
            .unwrap_or(CypherValue::Null)),
        "apoc.agg.last" => Ok(non_null
            .last()
            .map(|v| (*v).clone())
            .unwrap_or(CypherValue::Null)),
        "apoc.agg.median" => {
            let mut nums: Vec<f64> = non_null.iter().filter_map(|v| v.to_f64()).collect();
            if nums.is_empty() {
                return Ok(CypherValue::Null);
            }
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let n = nums.len();
            Ok(CypherValue::Float(if n % 2 == 1 {
                nums[n / 2]
            } else {
                (nums[n / 2 - 1] + nums[n / 2]) / 2.0
            }))
        }
        "apoc.agg.product" => {
            // APOC quirk: empty product is Float 0.0; Integer only when every input was an Integer
            if non_null.is_empty() {
                return Ok(CypherValue::Float(0.0));
            }
            let all_int = non_null
                .iter()
                .all(|v| matches!(**v, CypherValue::Integer(_)));
            let prod: f64 = non_null.iter().filter_map(|v| v.to_f64()).product();
            Ok(if all_int {
                CypherValue::Integer(prod as i64)
            } else {
                CypherValue::Float(prod)
            })
        }
        "apoc.agg.statistics" => {
            let mut nums: Vec<f64> = non_null.iter().filter_map(|v| v.to_f64()).collect();
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let all_int = non_null
                .iter()
                .all(|v| matches!(**v, CypherValue::Integer(_)));
            let as_val = |f: f64| {
                if all_int {
                    CypherValue::Integer(f as i64)
                } else {
                    CypherValue::Float(f)
                }
            };
            let mut m = std::collections::BTreeMap::new();
            m.insert("total".to_string(), CypherValue::Integer(nums.len() as i64));
            if nums.is_empty() {
                return Ok(CypherValue::Map(m));
            }
            let disc = |p: f64| {
                let idx = if p == 0.0 {
                    0
                } else {
                    ((p * nums.len() as f64).ceil() as usize).saturating_sub(1)
                };
                nums[idx.min(nums.len() - 1)]
            };
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            let var = nums.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / nums.len() as f64;
            m.insert("min".to_string(), as_val(nums[0]));
            m.insert("max".to_string(), as_val(nums[nums.len() - 1]));
            m.insert(
                "minNonZero".to_string(),
                CypherValue::Float(nums.iter().copied().find(|x| *x != 0.0).unwrap_or(f64::MAX)),
            );
            m.insert("mean".to_string(), CypherValue::Float(mean));
            m.insert("stdev".to_string(), CypherValue::Float(var.sqrt()));
            for p in [0.5, 0.75, 0.9, 0.95, 0.99] {
                m.insert(p.to_string(), as_val(disc(p)));
            }
            Ok(CypherValue::Map(m))
        }
        "apoc.agg.percentiles" => {
            let mut nums: Vec<f64> = non_null.iter().filter_map(|v| v.to_f64()).collect();
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let all_int = non_null
                .iter()
                .all(|v| matches!(**v, CypherValue::Integer(_)));
            let pcts: Vec<f64> = match args.get(1) {
                Some(e) => match eval_expr(e, frames[0], graph, symtab)? {
                    CypherValue::List(l) => l.iter().filter_map(|v| v.to_f64()).collect(),
                    _ => vec![0.5, 0.75, 0.9, 0.95, 0.99],
                },
                None => vec![0.5, 0.75, 0.9, 0.95, 0.99],
            };
            let out = pcts
                .iter()
                .map(|p| {
                    if nums.is_empty() {
                        return CypherValue::Null;
                    }
                    let idx = if *p == 0.0 {
                        0
                    } else {
                        ((p * nums.len() as f64).ceil() as usize).saturating_sub(1)
                    };
                    let v = nums[idx.min(nums.len() - 1)];
                    if all_int {
                        CypherValue::Integer(v as i64)
                    } else {
                        CypherValue::Float(v)
                    }
                })
                .collect();
            Ok(CypherValue::List(out))
        }
        "apoc.agg.nth" => {
            let n = match args.get(1) {
                Some(e) => match eval_expr(e, frames[0], graph, symtab)? {
                    CypherValue::Integer(i) => i,
                    _ => 0,
                },
                None => 0,
            };
            Ok(if n >= 0 && (n as usize) < non_null.len() {
                non_null[n as usize].clone()
            } else {
                CypherValue::Null
            })
        }
        "apoc.agg.slice" => {
            let from = match args.get(1) {
                Some(e) => match eval_expr(e, frames[0], graph, symtab)? {
                    CypherValue::Integer(i) => i.max(0) as usize,
                    _ => 0,
                },
                None => 0,
            };
            let to = match args.get(2) {
                Some(e) => match eval_expr(e, frames[0], graph, symtab)? {
                    CypherValue::Integer(i) => i,
                    _ => -1,
                },
                None => -1,
            };
            // third argument is a LENGTH (APOC), not an end index; negative = to the end
            let end = if to < 0 {
                non_null.len()
            } else {
                (from + to as usize).min(non_null.len())
            };
            Ok(CypherValue::List(
                non_null
                    .get(from.min(non_null.len())..end)
                    .unwrap_or(&[])
                    .iter()
                    .map(|v| (*v).clone())
                    .collect(),
            ))
        }
        "apoc.agg.maxitems" | "apoc.agg.minitems" => {
            // the generic collector keeps only the FIRST argument's values, so (item, value)
            // pairs are approximated from the collected items when the value expression
            // equals the item; full pair support is the row-collect below
            agg_min_max_items(name, args, frames, graph, symtab)
        }
        "apoc.agg.graph" => {
            let mut nodes: Vec<CypherValue> = Vec::new();
            let mut rels: Vec<CypherValue> = Vec::new();
            for v in &non_null {
                collect_graph_parts(v, &mut nodes, &mut rels);
            }
            let mut m = std::collections::BTreeMap::new();
            m.insert("nodes".to_string(), CypherValue::List(dedup_vals(nodes)));
            m.insert(
                "relationships".to_string(),
                CypherValue::List(dedup_vals(rels)),
            );
            Ok(CypherValue::Map(m))
        }
        _ => Err(CypherError::semantic(format!(
            "unknown aggregate function: {name}"
        ))),
    }
}

/// Replaces aggregate calls in an expression with placeholder variables, storing their values in
/// `subs`.
#[recursive::recursive]
fn replace_agg_calls(
    expr: &Expr,
    frames: &[&Frame],
    graph: &dyn GraphView,
    symtab: &SymbolTable,
    counter: &mut usize,
    subs: &mut Record,
) -> Result<Expr, CypherError> {
    match expr {
        Expr::FunctionCall {
            name,
            distinct,
            args,
        } if is_aggregate_function(name) => {
            let var_name = format!("__agg_{counter}");
            *counter += 1;
            let val = eval_direct_aggregate(name, *distinct, args, frames, graph, symtab)?;
            subs.insert(var_name.clone(), val);
            Ok(Expr::Variable(var_name))
        }
        Expr::BinaryOp(l, op, r) => {
            let l2 = replace_agg_calls(l, frames, graph, symtab, counter, subs)?;
            let r2 = replace_agg_calls(r, frames, graph, symtab, counter, subs)?;
            Ok(Expr::BinaryOp(Box::new(l2), *op, Box::new(r2)))
        }
        Expr::UnaryOp(op, e) => {
            let e2 = replace_agg_calls(e, frames, graph, symtab, counter, subs)?;
            Ok(Expr::UnaryOp(*op, Box::new(e2)))
        }
        Expr::FunctionCall {
            name,
            distinct,
            args,
        } => {
            let mut new_args = Vec::new();
            for arg in args {
                new_args.push(replace_agg_calls(
                    arg, frames, graph, symtab, counter, subs,
                )?);
            }
            Ok(Expr::FunctionCall {
                name: name.clone(),
                distinct: *distinct,
                args: new_args,
            })
        }
        Expr::Property(base, key) => {
            let base2 = replace_agg_calls(base, frames, graph, symtab, counter, subs)?;
            Ok(Expr::Property(Box::new(base2), key.clone()))
        }
        Expr::MapLiteral(pairs) => {
            let mut new_pairs = Vec::new();
            for (k, v) in pairs {
                let v2 = replace_agg_calls(v, frames, graph, symtab, counter, subs)?;
                new_pairs.push((k.clone(), v2));
            }
            Ok(Expr::MapLiteral(new_pairs))
        }
        Expr::ListLiteral(items) => {
            let mut new_items = Vec::new();
            for item in items {
                new_items.push(replace_agg_calls(
                    item, frames, graph, symtab, counter, subs,
                )?);
            }
            Ok(Expr::ListLiteral(new_items))
        }
        Expr::Comparison(base, pairs) => {
            let base2 = replace_agg_calls(base, frames, graph, symtab, counter, subs)?;
            let mut new_pairs = Vec::new();
            for (op, e) in pairs {
                let e2 = replace_agg_calls(e, frames, graph, symtab, counter, subs)?;
                new_pairs.push((*op, e2));
            }
            Ok(Expr::Comparison(Box::new(base2), new_pairs))
        }
        Expr::In(l, r) => {
            let l2 = replace_agg_calls(l, frames, graph, symtab, counter, subs)?;
            let r2 = replace_agg_calls(r, frames, graph, symtab, counter, subs)?;
            Ok(Expr::In(Box::new(l2), Box::new(r2)))
        }
        Expr::IsNull(e) => {
            let e2 = replace_agg_calls(e, frames, graph, symtab, counter, subs)?;
            Ok(Expr::IsNull(Box::new(e2)))
        }
        Expr::IsNotNull(e) => {
            let e2 = replace_agg_calls(e, frames, graph, symtab, counter, subs)?;
            Ok(Expr::IsNotNull(Box::new(e2)))
        }
        Expr::StringOp(l, op, r) => {
            let l2 = replace_agg_calls(l, frames, graph, symtab, counter, subs)?;
            let r2 = replace_agg_calls(r, frames, graph, symtab, counter, subs)?;
            Ok(Expr::StringOp(Box::new(l2), *op, Box::new(r2)))
        }
        Expr::ListIndex(base, idx) => {
            let base2 = replace_agg_calls(base, frames, graph, symtab, counter, subs)?;
            let idx2 = replace_agg_calls(idx, frames, graph, symtab, counter, subs)?;
            Ok(Expr::ListIndex(Box::new(base2), Box::new(idx2)))
        }
        Expr::ListSlice(base, from, to) => {
            let base2 = replace_agg_calls(base, frames, graph, symtab, counter, subs)?;
            let from2 = match from {
                Some(e) => Some(Box::new(replace_agg_calls(
                    e, frames, graph, symtab, counter, subs,
                )?)),
                None => None,
            };
            let to2 = match to {
                Some(e) => Some(Box::new(replace_agg_calls(
                    e, frames, graph, symtab, counter, subs,
                )?)),
                None => None,
            };
            Ok(Expr::ListSlice(Box::new(base2), from2, to2))
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            let operand2 = match operand {
                Some(e) => Some(Box::new(replace_agg_calls(
                    e, frames, graph, symtab, counter, subs,
                )?)),
                None => None,
            };
            let mut new_whens = Vec::new();
            for (w, t) in whens {
                let w2 = replace_agg_calls(w, frames, graph, symtab, counter, subs)?;
                let t2 = replace_agg_calls(t, frames, graph, symtab, counter, subs)?;
                new_whens.push((w2, t2));
            }
            let else2 = match else_expr {
                Some(e) => Some(Box::new(replace_agg_calls(
                    e, frames, graph, symtab, counter, subs,
                )?)),
                None => None,
            };
            Ok(Expr::Case {
                operand: operand2,
                whens: new_whens,
                else_expr: else2,
            })
        }
        Expr::ListComprehension {
            variable,
            list_expr,
            filter,
            map_expr,
        } => {
            let list2 = replace_agg_calls(list_expr, frames, graph, symtab, counter, subs)?;
            let filter2 = match filter {
                Some(f) => Some(Box::new(replace_agg_calls(
                    f, frames, graph, symtab, counter, subs,
                )?)),
                None => None,
            };
            let map2 = match map_expr {
                Some(m) => Some(Box::new(replace_agg_calls(
                    m, frames, graph, symtab, counter, subs,
                )?)),
                None => None,
            };
            Ok(Expr::ListComprehension {
                variable: variable.clone(),
                list_expr: Box::new(list2),
                filter: filter2,
                map_expr: map2,
            })
        }
        Expr::QuantifierExpr {
            kind,
            variable,
            list_expr,
            predicate,
        } => {
            let list2 = replace_agg_calls(list_expr, frames, graph, symtab, counter, subs)?;
            let pred2 = replace_agg_calls(predicate, frames, graph, symtab, counter, subs)?;
            Ok(Expr::QuantifierExpr {
                kind: *kind,
                variable: variable.clone(),
                list_expr: Box::new(list2),
                predicate: Box::new(pred2),
            })
        }
        _ => Ok(expr.clone()),
    }
}

/// apoc.agg.maxItems/minItems: (item, value) per row -> {items: [...], value: best} (APOC shape).
fn agg_min_max_items(
    name: &str,
    args: &[Expr],
    frames: &[&Frame],
    graph: &dyn GraphView,
    symtab: &SymbolTable,
) -> Result<CypherValue, CypherError> {
    let want_max = name.ends_with("maxitems");
    let mut best: Option<CypherValue> = None;
    let mut items: Vec<CypherValue> = Vec::new();
    for frame in frames {
        let item = args
            .first()
            .map(|e| eval_expr(e, frame, graph, symtab))
            .transpose()?
            .unwrap_or(CypherValue::Null);
        let value = args
            .get(1)
            .map(|e| eval_expr(e, frame, graph, symtab))
            .transpose()?
            .unwrap_or(CypherValue::Null);
        if matches!(value, CypherValue::Null) {
            continue;
        }
        match &best {
            None => {
                best = Some(value);
                items = vec![item];
            }
            Some(b) => {
                let ord = compare_values(&value, b);
                let better = if want_max {
                    ord == std::cmp::Ordering::Greater
                } else {
                    ord == std::cmp::Ordering::Less
                };
                if better {
                    best = Some(value);
                    items = vec![item];
                } else if ord == std::cmp::Ordering::Equal {
                    items.push(item);
                }
            }
        }
    }
    let mut m = std::collections::BTreeMap::new();
    m.insert("items".to_string(), CypherValue::List(items));
    m.insert("value".to_string(), best.unwrap_or(CypherValue::Null));
    Ok(CypherValue::Map(m))
}

/// Nodes and relationships of `v` (an entity, a path, or lists of them, nested), appended in order.
#[recursive::recursive]
pub(crate) fn collect_graph_parts(
    v: &CypherValue,
    nodes: &mut Vec<CypherValue>,
    rels: &mut Vec<CypherValue>,
) {
    match v {
        CypherValue::Path { nodes: n, edges: e } => {
            nodes.extend(n.iter().cloned());
            rels.extend(e.iter().cloned());
        }
        CypherValue::Node { .. } | CypherValue::NodeRef(_) => nodes.push(v.clone()),
        CypherValue::Edge { .. } | CypherValue::EdgeRef { .. } => rels.push(v.clone()),
        CypherValue::List(l) => {
            for x in l {
                collect_graph_parts(x, nodes, rels);
            }
        }
        _ => {}
    }
}

fn dedup_vals(v: Vec<CypherValue>) -> Vec<CypherValue> {
    let mut out: Vec<CypherValue> = Vec::new();
    for x in v {
        if !out.contains(&x) {
            out.push(x);
        }
    }
    out
}

fn dedup_values(values: Vec<CypherValue>) -> Vec<CypherValue> {
    let mut seen: FxHashSet<JoinKey> = FxHashSet::default();
    let mut result = Vec::new();
    for v in values {
        let key = cypher_to_join_key(&v).unwrap_or(JoinKey::Null);
        if seen.insert(key) {
            result.push(v);
        }
    }
    result
}
