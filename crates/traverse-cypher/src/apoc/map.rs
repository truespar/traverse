//! `apoc.map.*`: map functions.

use std::collections::BTreeMap;

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::{as_map, key_string, str_arg, with_list};

/// The pure functions of this module; `None` for other names.
pub(super) fn eval(name: &str, args: &[CypherValue]) -> Option<Result<CypherValue, CypherError>> {
    use CypherValue as V;
    let r = match name {
        "apoc.map.frompairs" => map_from_pairs(args),
        "apoc.map.fromlists" => map_from_lists(args),
        "apoc.map.fromvalues" => map_from_values(args),
        "apoc.map.merge" => map_merge(args),
        "apoc.map.mergelist" => map_merge_list(args),
        "apoc.map.setkey" | "apoc.map.setentry" => map_set_key(args),
        "apoc.map.removekey" => map_remove_key(args),
        "apoc.map.removekeys" => map_remove_keys(args),
        "apoc.map.clean" => map_clean(args),
        "apoc.map.flatten" => map_flatten(args),
        "apoc.map.submap" | "apoc.map.mget" => map_submap(args, name == "apoc.map.mget"),
        "apoc.map.get" => map_get(args),
        "apoc.map.groupby" => map_group_by(args),
        "apoc.map.groupbymulti" => map_group_by_multi(args),
        "apoc.map.sortedproperties" => map_sorted_properties(args),
        "apoc.map.values" => map_values(args),
        "apoc.map.setlists" => {
            let Some(m) = as_map(args.first()) else {
                return Some(Ok(V::Null));
            };
            let keys = match args.get(1) {
                Some(V::List(l)) => l.clone(),
                _ => Vec::new(),
            };
            let vals = match args.get(2) {
                Some(V::List(l)) => l.clone(),
                _ => Vec::new(),
            };
            let mut out = m.clone();
            for (i, k) in keys.iter().enumerate() {
                out.insert(key_string(k), vals.get(i).cloned().unwrap_or(V::Null));
            }
            Ok(V::Map(out))
        }
        "apoc.map.setpairs" => {
            let Some(m) = as_map(args.first()) else {
                return Some(Ok(V::Null));
            };
            let mut out = m.clone();
            if let Some(V::List(pairs)) = args.get(1) {
                for p in pairs {
                    if let V::List(kv) = p
                        && let Some(k) = kv.first()
                    {
                        out.insert(key_string(k), kv.get(1).cloned().unwrap_or(V::Null));
                    }
                }
            }
            Ok(V::Map(out))
        }
        "apoc.map.setvalues" => {
            let Some(m) = as_map(args.first()) else {
                return Some(Ok(V::Null));
            };
            let mut out = m.clone();
            if let Some(V::List(flat)) = args.get(1) {
                for kv in flat.chunks(2) {
                    if let Some(k) = kv.first() {
                        out.insert(key_string(k), kv.get(1).cloned().unwrap_or(V::Null));
                    }
                }
            }
            Ok(V::Map(out))
        }
        "apoc.map.unflatten" => {
            let Some(m) = as_map(args.first()) else {
                return Some(Ok(V::Null));
            };
            let delim = str_arg(args, 1).unwrap_or_else(|| ".".to_string());
            Ok(V::Map(map_unflatten(m, &delim)))
        }
        "apoc.map.updatetree" => map_update_tree(args),
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
        "apoc.map.fromnodes" => {
            let (Some(label), Some(prop)) = (str_arg(args, 0), str_arg(args, 1)) else {
                return Some(Ok(V::Null));
            };
            let mut m = BTreeMap::new();
            for nid in graph.nodes_by_label(&label) {
                if let Ok(Some(pv)) = graph.get_node_property(nid, &prop) {
                    m.insert(key_string(&V::from_property(&pv)), V::NodeRef(nid));
                }
            }
            Ok(V::Map(m))
        }
        _ => return None,
    };
    Some(r)
}

fn map_from_pairs(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        let mut m = BTreeMap::new();
        for pair in l {
            if let CypherValue::List(p) = pair
                && let Some(k) = p.first()
            {
                m.insert(
                    key_string(k),
                    p.get(1).cloned().unwrap_or(CypherValue::Null),
                );
            }
        }
        CypherValue::Map(m)
    })
}

fn map_from_lists(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::List(keys)), Some(CypherValue::List(vals))) =
        (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let mut m = BTreeMap::new();
    for (i, k) in keys.iter().enumerate() {
        m.insert(
            key_string(k),
            vals.get(i).cloned().unwrap_or(CypherValue::Null),
        );
    }
    Ok(CypherValue::Map(m))
}

fn map_from_values(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        let mut m = BTreeMap::new();
        for chunk in l.chunks(2) {
            if let Some(k) = chunk.first() {
                m.insert(
                    key_string(k),
                    chunk.get(1).cloned().unwrap_or(CypherValue::Null),
                );
            }
        }
        CypherValue::Map(m)
    })
}

fn map_merge(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(a), Some(b)) = (as_map(args.first()), as_map(args.get(1))) else {
        return Ok(CypherValue::Null);
    };
    let mut m = a.clone();
    for (k, v) in b {
        m.insert(k.clone(), v.clone());
    }
    Ok(CypherValue::Map(m))
}

fn map_merge_list(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_list(args, |l| {
        let mut m = BTreeMap::new();
        for v in l {
            if let CypherValue::Map(inner) = v {
                for (k, val) in inner {
                    m.insert(k.clone(), val.clone());
                }
            }
        }
        CypherValue::Map(m)
    })
}

fn map_set_key(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(m) = as_map(args.first()) else {
        return Ok(CypherValue::Null);
    };
    let Some(key) = str_arg(args, 1) else {
        return Ok(CypherValue::Map(m.clone()));
    };
    let mut out = m.clone();
    out.insert(key, args.get(2).cloned().unwrap_or(CypherValue::Null));
    Ok(CypherValue::Map(out))
}

fn map_remove_key(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(m) = as_map(args.first()) else {
        return Ok(CypherValue::Null);
    };
    let mut out = m.clone();
    if let Some(key) = str_arg(args, 1) {
        out.remove(&key);
    }
    Ok(CypherValue::Map(out))
}

fn map_remove_keys(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(m) = as_map(args.first()) else {
        return Ok(CypherValue::Null);
    };
    let mut out = m.clone();
    if let Some(CypherValue::List(keys)) = args.get(1) {
        for k in keys {
            out.remove(&key_string(k));
        }
    }
    Ok(CypherValue::Map(out))
}

fn map_clean(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(m) = as_map(args.first()) else {
        return Ok(CypherValue::Null);
    };
    let empty = Vec::new();
    let skip_keys = match args.get(1) {
        Some(CypherValue::List(l)) => l,
        _ => &empty,
    };
    let skip_vals = match args.get(2) {
        Some(CypherValue::List(l)) => l,
        _ => &empty,
    };
    let mut out = BTreeMap::new();
    for (k, v) in m {
        if matches!(v, CypherValue::Null) {
            continue;
        }
        if skip_keys.iter().any(|sk| key_string(sk) == *k) {
            continue;
        }
        if skip_vals.contains(v) {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    Ok(CypherValue::Map(out))
}

fn map_flatten(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(m) = as_map(args.first()) else {
        return Ok(CypherValue::Null);
    };
    let delim = str_arg(args, 1).unwrap_or_else(|| ".".to_string());
    #[recursive::recursive]
    fn walk(
        prefix: &str,
        m: &BTreeMap<String, CypherValue>,
        delim: &str,
        out: &mut BTreeMap<String, CypherValue>,
    ) {
        for (k, v) in m {
            let key = if prefix.is_empty() {
                k.clone()
            } else {
                format!("{prefix}{delim}{k}")
            };
            match v {
                CypherValue::Map(inner) => walk(&key, inner, delim, out),
                other => {
                    out.insert(key, other.clone());
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    walk("", m, &delim, &mut out);
    Ok(CypherValue::Map(out))
}

fn map_submap(args: &[CypherValue], mget: bool) -> Result<CypherValue, CypherError> {
    let Some(m) = as_map(args.first()) else {
        return Ok(CypherValue::Null);
    };
    let keys = match args.get(1) {
        Some(CypherValue::List(l)) => l.clone(),
        _ => Vec::new(),
    };
    let defaults = match args.get(2) {
        Some(CypherValue::List(l)) => l.clone(),
        _ => Vec::new(),
    };
    let fail = match args.get(3) {
        Some(CypherValue::Bool(b)) => *b,
        _ => defaults.is_empty(),
    };
    let mut sub = BTreeMap::new();
    let mut vals = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        let ks = key_string(k);
        match m.get(&ks) {
            Some(v) => {
                sub.insert(ks, v.clone());
                vals.push(v.clone());
            }
            None => {
                if let Some(d) = defaults.get(i) {
                    sub.insert(ks, d.clone());
                    vals.push(d.clone());
                } else if fail {
                    return Err(CypherError::semantic(format!(
                        "key `{ks}` is not present in the map"
                    )));
                } else {
                    sub.insert(ks, CypherValue::Null);
                    vals.push(CypherValue::Null);
                }
            }
        }
    }
    Ok(if mget {
        CypherValue::List(vals)
    } else {
        CypherValue::Map(sub)
    })
}

fn map_get(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(m) = as_map(args.first()) else {
        return Ok(CypherValue::Null);
    };
    let Some(key) = str_arg(args, 1) else {
        return Ok(CypherValue::Null);
    };
    match m.get(&key) {
        Some(v) => Ok(v.clone()),
        None => match args.get(2) {
            Some(d) => Ok(d.clone()),
            None => Err(CypherError::semantic(format!(
                "key `{key}` is not present in the map"
            ))),
        },
    }
}

fn map_group_by(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(key) = str_arg(args, 1) else {
        return Ok(CypherValue::Null);
    };
    with_list(args, |l| {
        let mut out = BTreeMap::new();
        for v in l {
            if let CypherValue::Map(m) = v
                && let Some(kv) = m.get(&key)
            {
                out.insert(key_string(kv), v.clone());
            }
        }
        CypherValue::Map(out)
    })
}

fn map_group_by_multi(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(key) = str_arg(args, 1) else {
        return Ok(CypherValue::Null);
    };
    with_list(args, |l| {
        let mut out: BTreeMap<String, Vec<CypherValue>> = BTreeMap::new();
        for v in l {
            if let CypherValue::Map(m) = v
                && let Some(kv) = m.get(&key)
            {
                out.entry(key_string(kv)).or_default().push(v.clone());
            }
        }
        CypherValue::Map(
            out.into_iter()
                .map(|(k, vs)| (k, CypherValue::List(vs)))
                .collect(),
        )
    })
}

fn map_sorted_properties(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(m) = as_map(args.first()) else {
        return Ok(CypherValue::Null);
    };
    // BTreeMap is key-sorted already
    Ok(CypherValue::List(
        m.iter()
            .map(|(k, v)| CypherValue::List(vec![CypherValue::String(k.clone()), v.clone()]))
            .collect(),
    ))
}

fn map_values(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(m) = as_map(args.first()) else {
        return Ok(CypherValue::Null);
    };
    let keys = match args.get(1) {
        Some(CypherValue::List(l)) => l.clone(),
        _ => m.keys().map(|k| CypherValue::String(k.clone())).collect(),
    };
    let add_null = matches!(args.get(2), Some(CypherValue::Bool(true)));
    let mut out = Vec::new();
    for k in keys {
        match m.get(&key_string(&k)) {
            Some(v) => out.push(v.clone()),
            None if add_null => out.push(CypherValue::Null),
            None => {}
        }
    }
    Ok(CypherValue::List(out))
}

fn map_unflatten(m: &BTreeMap<String, CypherValue>, delim: &str) -> BTreeMap<String, CypherValue> {
    let mut out: BTreeMap<String, CypherValue> = BTreeMap::new();
    for (k, v) in m {
        let parts: Vec<&str> = k.split(delim).collect();
        insert_path(&mut out, &parts, v.clone());
    }
    out
}

#[recursive::recursive]
fn insert_path(m: &mut BTreeMap<String, CypherValue>, path: &[&str], v: CypherValue) {
    if path.len() == 1 {
        m.insert(path[0].to_string(), v);
        return;
    }
    let entry = m
        .entry(path[0].to_string())
        .or_insert_with(|| CypherValue::Map(BTreeMap::new()));
    if let CypherValue::Map(inner) = entry {
        insert_path(inner, &path[1..], v);
    }
}

fn map_update_tree(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    // updateTree(tree, key, [[value, {data}], ...]): merge data into every nested map whose `key`
    // equals a listed value
    let Some(tree) = as_map(args.first()) else {
        return Ok(CypherValue::Null);
    };
    let Some(key) = str_arg(args, 1) else {
        return Ok(CypherValue::Map(tree.clone()));
    };
    let updates: Vec<(CypherValue, BTreeMap<String, CypherValue>)> = match args.get(2) {
        Some(CypherValue::List(l)) => l
            .iter()
            .filter_map(|p| match p {
                CypherValue::List(kv) if kv.len() == 2 => match &kv[1] {
                    CypherValue::Map(d) => Some((kv[0].clone(), d.clone())),
                    _ => None,
                },
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    #[recursive::recursive]
    fn walk(
        m: &mut BTreeMap<String, CypherValue>,
        key: &str,
        updates: &[(CypherValue, BTreeMap<String, CypherValue>)],
    ) {
        if let Some(id) = m.get(key).cloned() {
            for (want, data) in updates {
                if *want == id {
                    for (k, v) in data {
                        m.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        for v in m.values_mut() {
            match v {
                CypherValue::Map(inner) => walk(inner, key, updates),
                CypherValue::List(items) => {
                    for it in items {
                        if let CypherValue::Map(inner) = it {
                            walk(inner, key, updates);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = tree.clone();
    walk(&mut out, &key, &updates);
    Ok(CypherValue::Map(out))
}
