//! `apoc.coll.*`: list functions.

use std::collections::BTreeMap;

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::{
    cheap_rand, dedup_keep_order, int_arg, key_string, node_id_of, num_of, str_arg, with_list,
};

/// The pure functions of this module; `None` for other names.
pub(super) fn eval(name: &str, args: &[CypherValue]) -> Option<Result<CypherValue, CypherError>> {
    use CypherValue as V;
    let r = match name {
        "apoc.coll.sum" => coll_sum(args, false),
        "apoc.coll.sumlongs" => coll_sum(args, true),
        "apoc.coll.avg" => coll_avg(args),
        "apoc.coll.min" => coll_min_max(args, true),
        "apoc.coll.max" => coll_min_max(args, false),
        "apoc.coll.stdev" => coll_stdev(args),
        "apoc.coll.sort" => coll_sort(args),
        "apoc.coll.reverse" => with_list(args, |l| V::List(l.iter().rev().cloned().collect())),
        "apoc.coll.contains" => with_list(args, |l| {
            V::Bool(l.contains(args.get(1).unwrap_or(&V::Null)))
        }),
        "apoc.coll.containsall" => coll_contains_all(args),
        "apoc.coll.containsany" => coll_contains_any(args),
        "apoc.coll.toset" => with_list(args, |l| V::List(dedup_keep_order(l))),
        "apoc.coll.flatten" => coll_flatten(args),
        "apoc.coll.pairs" => coll_pairs(args, false),
        "apoc.coll.pairsmin" => coll_pairs(args, true),
        "apoc.coll.zip" => coll_zip(args),
        "apoc.coll.union" => coll_union(args, true),
        "apoc.coll.unionall" => coll_union(args, false),
        "apoc.coll.intersection" => coll_intersection(args),
        "apoc.coll.subtract" => coll_subtract(args, true),
        "apoc.coll.removeall" => coll_subtract(args, false),
        "apoc.coll.disjunction" => coll_disjunction(args),
        "apoc.coll.indexof" => coll_index_of(args),
        "apoc.coll.split" => coll_split(args),
        "apoc.coll.partition" => coll_partition(args),
        "apoc.coll.frequencies" => coll_frequencies(args),
        "apoc.coll.duplicates" => coll_duplicates(args),
        "apoc.coll.occurrences" => coll_occurrences(args),
        "apoc.coll.shuffle" => coll_shuffle(args),
        "apoc.coll.randomitem" => coll_random_item(args),
        "apoc.coll.insert" => coll_insert(args),
        "apoc.coll.remove" => coll_remove(args),
        "apoc.coll.different" => with_list(args, |l| V::Bool(dedup_keep_order(l).len() == l.len())),
        "apoc.coll.dropduplicateneighbors" => coll_drop_dup_neighbors(args),
        "apoc.coll.isequalcollection" => coll_is_equal_collection(args),
        "apoc.coll.combinations" => coll_combinations(args),
        "apoc.coll.containsallsorted" => {
            let (Some(V::List(a)), Some(V::List(b))) = (args.first(), args.get(1)) else {
                return Some(Ok(V::Null));
            };
            Ok(V::Bool(
                b.iter()
                    .all(|v| a.binary_search_by(|x| cmp_vals(x, v)).is_ok()),
            ))
        }
        "apoc.coll.containssorted" => with_list(args, |l| {
            let needle = args.get(1).cloned().unwrap_or(V::Null);
            V::Bool(l.binary_search_by(|x| cmp_vals(x, &needle)).is_ok())
        }),
        "apoc.coll.containsduplicates" => {
            with_list(args, |l| V::Bool(dedup_keep_order(l).len() != l.len()))
        }
        "apoc.coll.duplicateswithcount" => with_list(args, |l| {
            let uniq = dedup_keep_order(l);
            V::List(
                uniq.into_iter()
                    .filter_map(|item| {
                        let count = l.iter().filter(|v| **v == item).count() as i64;
                        (count > 1).then(|| {
                            let mut m = BTreeMap::new();
                            m.insert("count".to_string(), V::Integer(count));
                            m.insert("item".to_string(), item);
                            V::Map(m)
                        })
                    })
                    .collect(),
            )
        }),
        "apoc.coll.fill" => {
            let item = args.first().cloned().unwrap_or(V::Null);
            let count = int_arg(args, 1).unwrap_or(0).max(0) as usize;
            Ok(V::List(vec![item; count]))
        }
        "apoc.coll.frequenciesasmap" => with_list(args, |l| {
            let mut m = BTreeMap::new();
            for item in dedup_keep_order(l) {
                let count = l.iter().filter(|v| **v == item).count() as i64;
                m.insert(key_string(&item), V::Integer(count));
            }
            V::Map(m)
        }),
        "apoc.coll.insertall" => {
            let idx = int_arg(args, 1).unwrap_or(0).max(0) as usize;
            with_list(args, |l| {
                let values = match args.get(2) {
                    Some(V::List(v)) => v.clone(),
                    Some(other) => vec![other.clone()],
                    None => Vec::new(),
                };
                let mut v = l.to_vec();
                let at = idx.min(v.len());
                v.splice(at..at, values);
                V::List(v)
            })
        }
        "apoc.coll.pairwithoffset" => {
            let off = int_arg(args, 1).unwrap_or(1);
            with_list(args, |l| {
                V::List(
                    (0..l.len())
                        .map(|i| {
                            let j = i as i64 + off;
                            let second = if j >= 0 && (j as usize) < l.len() {
                                l[j as usize].clone()
                            } else {
                                V::Null
                            };
                            V::List(vec![l[i].clone(), second])
                        })
                        .collect(),
                )
            })
        }
        "apoc.coll.randomitems" => {
            let count = int_arg(args, 1).unwrap_or(0).max(0) as usize;
            let repick = matches!(args.get(2), Some(V::Bool(true)));
            with_list(args, |l| {
                if l.is_empty() {
                    return V::List(Vec::new());
                }
                let mut out = Vec::with_capacity(count);
                if repick {
                    for _ in 0..count {
                        out.push(l[cheap_rand(l.len())].clone());
                    }
                } else {
                    let mut pool = l.to_vec();
                    for _ in 0..count.min(pool.len()) {
                        out.push(pool.swap_remove(cheap_rand(pool.len())));
                    }
                }
                V::List(out)
            })
        }
        "apoc.coll.runningtotal" => with_list(args, |l| {
            let mut acc = 0.0f64;
            let mut all_int = true;
            let out: Vec<V> = l
                .iter()
                .map(|v| {
                    match v {
                        V::Integer(i) => acc += *i as f64,
                        V::Float(f) => {
                            all_int = false;
                            acc += f;
                        }
                        _ => {}
                    }
                    if all_int {
                        V::Integer(acc as i64)
                    } else {
                        V::Float(acc)
                    }
                })
                .collect();
            V::List(out)
        }),
        "apoc.coll.set" => {
            let idx = int_arg(args, 1).unwrap_or(-1);
            with_list(args, |l| {
                let mut v = l.to_vec();
                if idx >= 0 && (idx as usize) < v.len() {
                    v[idx as usize] = args.get(2).cloned().unwrap_or(V::Null);
                }
                V::List(v)
            })
        }
        "apoc.coll.sortmaps" => {
            let prop = str_arg(args, 1).unwrap_or_default();
            with_list(args, |l| {
                let mut v = l.to_vec();
                // APOC sortMaps sorts DESCENDING by the property
                v.sort_by(|a, b| {
                    let ka = map_key(a, &prop);
                    let kb = map_key(b, &prop);
                    cmp_vals(&kb, &ka)
                });
                V::List(v)
            })
        }
        "apoc.coll.sortmulti" => coll_sort_multi(args),
        "apoc.coll.sorttext" => with_list(args, |l| {
            let mut v = l.to_vec();
            v.sort_by_key(key_string);
            V::List(v)
        }),
        _ => return None,
    };
    Some(r)
}

/// The functions of this module that read the graph; `None` for other names.
pub(super) fn eval_graph(
    name: &str,
    args: &[CypherValue],
    graph: &dyn GraphView,
) -> Option<Result<CypherValue, CypherError>> {
    use CypherValue as V;
    let r = match name {
        "apoc.coll.sortnodes" => {
            let prop = str_arg(args, 1).unwrap_or_default();
            match args.first() {
                Some(V::List(l)) => {
                    let mut v = l.clone();
                    // DESCENDING by property, like sortMaps
                    v.sort_by(|a, b| {
                        let ka = node_prop(graph, a, &prop);
                        let kb = node_prop(graph, b, &prop);
                        crate::executor::compare_values(&kb, &ka)
                    });
                    Ok(V::List(v))
                }
                _ => Ok(V::Null),
            }
        }
        _ => return None,
    };
    Some(r)
}

fn node_prop(graph: &dyn GraphView, v: &CypherValue, prop: &str) -> CypherValue {
    match node_id_of(v) {
        Some(id) => graph
            .get_node_property(id, prop)
            .ok()
            .flatten()
            .map(|pv| CypherValue::from_property(&pv))
            .unwrap_or(CypherValue::Null),
        None => map_key(v, prop),
    }
}

fn cmp_vals(a: &CypherValue, b: &CypherValue) -> std::cmp::Ordering {
    crate::executor::compare_values(a, b)
}

fn coll_sum(args: &[CypherValue], longs: bool) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        if longs {
            CypherValue::Integer(l.iter().filter_map(num_of).map(|f| f as i64).sum())
        } else {
            CypherValue::Float(l.iter().filter_map(num_of).sum())
        }
    })
}

fn coll_avg(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        let nums: Vec<f64> = l.iter().filter_map(num_of).collect();
        if nums.is_empty() {
            CypherValue::Null
        } else {
            CypherValue::Float(nums.iter().sum::<f64>() / nums.len() as f64)
        }
    })
}

fn coll_min_max(args: &[CypherValue], min: bool) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        let mut it = l.iter().filter(|v| !matches!(v, CypherValue::Null));
        let Some(first) = it.next() else {
            return CypherValue::Null;
        };
        let mut best = first;
        for v in it {
            let ord = cmp_vals(v, best);
            if (min && ord == std::cmp::Ordering::Less)
                || (!min && ord == std::cmp::Ordering::Greater)
            {
                best = v;
            }
        }
        best.clone()
    })
}

fn coll_stdev(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    // isBiased=false gives the SAMPLE standard deviation (n-1); the default true gives population
    let biased = !matches!(args.get(1), Some(CypherValue::Bool(false)));
    with_list(args, |l| {
        let nums: Vec<f64> = l.iter().filter_map(num_of).collect();
        let n = nums.len();
        if n < 2 {
            return CypherValue::Float(0.0);
        }
        let mean = nums.iter().sum::<f64>() / n as f64;
        let ss: f64 = nums.iter().map(|x| (x - mean) * (x - mean)).sum();
        let denom = if biased { n as f64 } else { (n - 1) as f64 };
        CypherValue::Float((ss / denom).sqrt())
    })
}

fn coll_sort(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        let mut v = l.to_vec();
        v.sort_by(cmp_vals);
        CypherValue::List(v)
    })
}

fn coll_contains_all(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::List(a)), Some(CypherValue::List(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    Ok(CypherValue::Bool(b.iter().all(|v| a.contains(v))))
}

fn coll_contains_any(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::List(a)), Some(CypherValue::List(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    Ok(CypherValue::Bool(b.iter().any(|v| a.contains(v))))
}

fn coll_flatten(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        let mut out = Vec::new();
        for v in l {
            match v {
                CypherValue::List(inner) => out.extend(inner.iter().cloned()),
                other => out.push(other.clone()),
            }
        }
        CypherValue::List(out)
    })
}

fn coll_pairs(args: &[CypherValue], min: bool) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        if l.is_empty() {
            return CypherValue::List(Vec::new());
        }
        let end = if min { l.len() - 1 } else { l.len() };
        let mut out = Vec::with_capacity(end);
        for i in 0..end {
            let second = l.get(i + 1).cloned().unwrap_or(CypherValue::Null);
            out.push(CypherValue::List(vec![l[i].clone(), second]));
        }
        CypherValue::List(out)
    })
}

fn coll_zip(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::List(a)), Some(CypherValue::List(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let out = a
        .iter()
        .enumerate()
        .map(|(i, x)| {
            CypherValue::List(vec![
                x.clone(),
                b.get(i).cloned().unwrap_or(CypherValue::Null),
            ])
        })
        .collect();
    Ok(CypherValue::List(out))
}

fn coll_union(args: &[CypherValue], dedup: bool) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::List(a)), Some(CypherValue::List(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let mut all = a.clone();
    all.extend(b.iter().cloned());
    Ok(CypherValue::List(if dedup {
        dedup_keep_order(&all)
    } else {
        all
    }))
}

fn coll_intersection(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::List(a)), Some(CypherValue::List(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    Ok(CypherValue::List(
        dedup_keep_order(a)
            .into_iter()
            .filter(|v| b.contains(v))
            .collect(),
    ))
}

fn coll_subtract(args: &[CypherValue], dedup: bool) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::List(a)), Some(CypherValue::List(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let base: Vec<CypherValue> = if dedup {
        dedup_keep_order(a)
    } else {
        a.clone()
    };
    Ok(CypherValue::List(
        base.into_iter().filter(|v| !b.contains(v)).collect(),
    ))
}

fn coll_disjunction(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::List(a)), Some(CypherValue::List(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let mut out: Vec<CypherValue> = dedup_keep_order(a)
        .into_iter()
        .filter(|v| !b.contains(v))
        .collect();
    out.extend(dedup_keep_order(b).into_iter().filter(|v| !a.contains(v)));
    Ok(CypherValue::List(out))
}

fn coll_index_of(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        let needle = args.get(1).cloned().unwrap_or(CypherValue::Null);
        CypherValue::Integer(
            l.iter()
                .position(|v| *v == needle)
                .map(|i| i as i64)
                .unwrap_or(-1),
        )
    })
}

fn coll_split(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        let sep = args.get(1).cloned().unwrap_or(CypherValue::Null);
        let mut out = Vec::new();
        let mut cur = Vec::new();
        for v in l {
            if *v == sep {
                if !cur.is_empty() {
                    out.push(CypherValue::List(std::mem::take(&mut cur)));
                }
            } else {
                cur.push(v.clone());
            }
        }
        if !cur.is_empty() {
            out.push(CypherValue::List(cur));
        }
        CypherValue::List(out)
    })
}

fn coll_partition(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let size = int_arg(args, 1).unwrap_or(1).max(1) as usize;
    with_list(args, |l| {
        CypherValue::List(
            l.chunks(size)
                .map(|c| CypherValue::List(c.to_vec()))
                .collect(),
        )
    })
}

fn coll_frequencies(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        let uniq = dedup_keep_order(l);
        CypherValue::List(
            uniq.into_iter()
                .map(|item| {
                    let count = l.iter().filter(|v| **v == item).count() as i64;
                    let mut m = BTreeMap::new();
                    m.insert("item".to_string(), item);
                    m.insert("count".to_string(), CypherValue::Integer(count));
                    CypherValue::Map(m)
                })
                .collect(),
        )
    })
}

fn coll_duplicates(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        let uniq = dedup_keep_order(l);
        CypherValue::List(
            uniq.into_iter()
                .filter(|item| l.iter().filter(|v| *v == item).count() > 1)
                .collect(),
        )
    })
}

fn coll_occurrences(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        let needle = args.get(1).cloned().unwrap_or(CypherValue::Null);
        CypherValue::Integer(l.iter().filter(|v| **v == needle).count() as i64)
    })
}

fn coll_shuffle(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        let mut v = l.to_vec();
        // Fisher-Yates with the cheap time-seeded generator (APOC's shuffle is nondeterministic
        // too)
        let mut seed = cheap_rand(usize::MAX - 1) as u64 | 1;
        for i in (1..v.len()).rev() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let j = ((seed >> 33) as usize) % (i + 1);
            v.swap(i, j);
        }
        CypherValue::List(v)
    })
}

fn coll_random_item(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        if l.is_empty() {
            CypherValue::Null
        } else {
            l[cheap_rand(l.len())].clone()
        }
    })
}

fn coll_insert(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let idx = int_arg(args, 1).unwrap_or(0).max(0) as usize;
    with_list(args, |l| {
        let mut v = l.to_vec();
        let value = args.get(2).cloned().unwrap_or(CypherValue::Null);
        v.insert(idx.min(v.len()), value);
        CypherValue::List(v)
    })
}

fn coll_remove(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let idx = int_arg(args, 1).unwrap_or(-1);
    let count = int_arg(args, 2).unwrap_or(1).max(0) as usize;
    with_list(args, |l| {
        if idx < 0 || idx as usize >= l.len() {
            return CypherValue::List(l.to_vec());
        }
        let mut v = l.to_vec();
        let start = idx as usize;
        let end = (start + count).min(v.len());
        v.drain(start..end);
        CypherValue::List(v)
    })
}

fn coll_drop_dup_neighbors(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        let mut out: Vec<CypherValue> = Vec::with_capacity(l.len());
        for v in l {
            if out.last() != Some(v) {
                out.push(v.clone());
            }
        }
        CypherValue::List(out)
    })
}

fn coll_is_equal_collection(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::List(a)), Some(CypherValue::List(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let eq = a.len() == b.len()
        && dedup_keep_order(a)
            .iter()
            .all(|v| a.iter().filter(|x| *x == v).count() == b.iter().filter(|x| *x == v).count());
    Ok(CypherValue::Bool(eq))
}

fn coll_combinations(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let min = int_arg(args, 1).unwrap_or(1).max(0) as usize;
    with_list(args, |l| {
        let max_raw = int_arg(args, 2).unwrap_or(-1);
        let max = if max_raw < 0 { min } else { max_raw as usize };
        let mut out: Vec<CypherValue> = Vec::new();
        let n = l.len();
        for k in min..=max.min(n) {
            if k == 0 {
                continue;
            }
            // iterative k-combination index walk
            let mut idx: Vec<usize> = (0..k).collect();
            loop {
                out.push(CypherValue::List(
                    idx.iter().map(|&i| l[i].clone()).collect(),
                ));
                // advance
                let mut i = k;
                loop {
                    if i == 0 {
                        break;
                    }
                    i -= 1;
                    if idx[i] != i + n - k {
                        idx[i] += 1;
                        for j in i + 1..k {
                            idx[j] = idx[j - 1] + 1;
                        }
                        break;
                    }
                    if i == 0 {
                        return CypherValue::List(out);
                    }
                }
                if idx[0] > n - k {
                    break;
                }
            }
        }
        CypherValue::List(out)
    })
}

fn map_key(v: &CypherValue, prop: &str) -> CypherValue {
    match v {
        CypherValue::Map(m) => m.get(prop).cloned().unwrap_or(CypherValue::Null),
        _ => CypherValue::Null,
    }
}

fn coll_sort_multi(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let fields: Vec<String> = match args.get(1) {
        Some(CypherValue::List(l)) => l.iter().map(key_string).collect(),
        _ => Vec::new(),
    };
    let limit = int_arg(args, 2).unwrap_or(-1);
    let skip = int_arg(args, 3).unwrap_or(0).max(0) as usize;
    with_list(args, |l| {
        let mut v = l.to_vec();
        v.sort_by(|a, b| {
            for f in &fields {
                // `^field` = ascending; default DESCENDING (APOC)
                let (name, asc) = match f.strip_prefix('^') {
                    Some(n) => (n, true),
                    None => (f.as_str(), false),
                };
                let ka = map_key(a, name);
                let kb = map_key(b, name);
                let ord = if asc {
                    cmp_vals(&ka, &kb)
                } else {
                    cmp_vals(&kb, &ka)
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
        let iter = v.into_iter().skip(skip);
        CypherValue::List(if limit >= 0 {
            iter.take(limit as usize).collect()
        } else {
            iter.collect()
        })
    })
}
