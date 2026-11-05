//! `apoc.nodes.*` and `apoc.neighbors.*`: node sets, grouping, linking and neighborhoods.

use traverse_core::tx::view::GraphView;

use crate::apoc::{key_string, next_virtual_id};
use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::procedures::ProcCall;
use crate::types::CypherValue;

use super::{apoc_expand, apoc_step, node_ref_of, parse_rel_specs};

/// `apoc.nodes.*` and `apoc.neighbors.*`.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall {
        args,
        frames,
        symtab,
        ..
    } = *cx;
    let name_lower = &cx.name_lower;
    match name_lower.as_str() {
        "apoc.nodes.get" | "apoc.nodes.rels" => {
            let is_nodes = name_lower.ends_with("get");
            cx.emit_rows(&|frame| {
                let v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let items = match v {
                    CypherValue::List(l) => l,
                    other => vec![other],
                };
                Ok(items
                    .into_iter()
                    .filter(|it| !matches!(it, CypherValue::Null))
                    .map(|it| vec![(if is_nodes { "node" } else { "rel" }, it)])
                    .collect())
            })
        }
        "apoc.nodes.delete" => cx.emit_rows(&|frame| {
            let v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
            let ids: Vec<_> = match v {
                CypherValue::List(l) => l.iter().filter_map(node_ref_of).collect(),
                other => node_ref_of(&other).into_iter().collect(),
            };
            Ok(vec![vec![(
                "value",
                CypherValue::Integer(ids.len() as i64),
            )]])
        }),
        "apoc.neighbors.athop"
        | "apoc.neighbors.tohop"
        | "apoc.neighbors.byhop"
        | "apoc.neighbors.athop.count"
        | "apoc.neighbors.tohop.count"
        | "apoc.neighbors.byhop.count" => {
            cx.emit_rows(&|frame| {
                let Some(start) = eval_expr(cx.arg(0)?, frame, &*graph, symtab)
                    .ok()
                    .as_ref()
                    .and_then(node_ref_of)
                else {
                    return Ok(Vec::new());
                };
                let spec = args
                    .and_then(|a| a.get(1))
                    .map(|e| eval_expr(e, frame, &*graph, symtab))
                    .transpose()?
                    .and_then(|v| match v {
                        CypherValue::String(s) => Some(s),
                        _ => None,
                    })
                    .unwrap_or_default();
                let distance = args
                    .and_then(|a| a.get(2))
                    .map(|e| eval_expr(e, frame, &*graph, symtab))
                    .transpose()?
                    .and_then(|v| match v {
                        CypherValue::Integer(n) => Some(n.max(1)),
                        _ => None,
                    })
                    .unwrap_or(1);
                let specs = parse_rel_specs(&spec);
                // BFS levels 1..=distance, node-global uniqueness
                let mut visited: std::collections::HashSet<u64> = [start.0].into();
                let mut level: Vec<_> = vec![start];
                let mut by_hop: Vec<Vec<CypherValue>> = Vec::new();
                for _ in 0..distance {
                    let mut next = Vec::new();
                    for nid in &level {
                        for (_e, other) in apoc_step(&*graph, *nid, &specs) {
                            if visited.insert(other.0) {
                                next.push(other);
                            }
                        }
                    }
                    by_hop.push(next.iter().map(|n| CypherValue::NodeRef(*n)).collect());
                    level = next;
                    if level.is_empty() {
                        break;
                    }
                }
                let rows: Vec<Vec<(&'static str, CypherValue)>> =
                    if name_lower.starts_with("apoc.neighbors.athop") {
                        let last = by_hop
                            .get(distance as usize - 1)
                            .cloned()
                            .unwrap_or_default();
                        if name_lower.ends_with("count") {
                            vec![vec![("value", CypherValue::Integer(last.len() as i64))]]
                        } else {
                            last.into_iter().map(|n| vec![("node", n)]).collect()
                        }
                    } else if name_lower.starts_with("apoc.neighbors.tohop") {
                        let all: Vec<CypherValue> = by_hop.iter().flatten().cloned().collect();
                        if name_lower.ends_with("count") {
                            vec![vec![("value", CypherValue::Integer(all.len() as i64))]]
                        } else {
                            all.into_iter().map(|n| vec![("node", n)]).collect()
                        }
                    } else if name_lower.ends_with("count") {
                        vec![vec![(
                            "value",
                            CypherValue::List(
                                by_hop
                                    .iter()
                                    .map(|l| CypherValue::Integer(l.len() as i64))
                                    .collect(),
                            ),
                        )]]
                    } else {
                        by_hop
                            .into_iter()
                            .map(|l| vec![("nodes", CypherValue::List(l))])
                            .collect()
                    };
                Ok(rows)
            })
        }
        "apoc.nodes.link" => {
            for frame in frames {
                let CypherValue::List(nodes) = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?
                else {
                    continue;
                };
                let rtype = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let ids: Vec<_> = nodes.iter().filter_map(node_ref_of).collect();
                for w in ids.windows(2) {
                    graph
                        .create_edge(w[0], w[1], &rtype, Default::default())
                        .map_err(CypherError::from)?;
                    crate::executor::inc_stat(|s| s.relationships_created += 1);
                }
            }
            Ok(frames.to_vec())
        }
        "apoc.nodes.collapse" => {
            // Virtual grouping: one virtual node per input set + virtual
            // rels to external neighbors (single-row summary form).
            let mut out = Vec::new();
            for frame in frames {
                let CypherValue::List(nodes) = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?
                else {
                    continue;
                };
                let ids: Vec<_> = nodes.iter().filter_map(node_ref_of).collect();
                if ids.is_empty() {
                    continue;
                }
                let vnode = CypherValue::Node {
                    id: next_virtual_id(),
                    labels: vec!["Collapsed".to_string()],
                    properties: Default::default(),
                };
                use traverse_core::graph::types::Direction;
                let idset: std::collections::HashSet<u64> = ids.iter().map(|n| n.0).collect();
                for nid in &ids {
                    for (eid, other) in apoc_step(&*graph, *nid, &[]) {
                        if idset.contains(&other.0) {
                            continue;
                        }
                        let Ok(e) = graph.get_edge(eid) else { continue };
                        let outgoing = e.source == *nid;
                        let t = graph.strings().resolve(e.edge_type.0).to_string();
                        let vid = match &vnode {
                            CypherValue::Node { id, .. } => *id,
                            _ => unreachable!(),
                        };
                        let rel = CypherValue::Edge {
                            id: traverse_core::graph::types::EdgeId(next_virtual_id().0),
                            edge_type: t,
                            source: if outgoing { vid } else { other },
                            target: if outgoing { other } else { vid },
                            properties: Default::default(),
                        };
                        let mut nf = frame.clone();
                        if let Some(sl) = cx.slot_for("from") {
                            nf[sl] = if outgoing {
                                vnode.clone().into()
                            } else {
                                CypherValue::NodeRef(other).into()
                            };
                        }
                        if let Some(sl) = cx.slot_for("rel") {
                            nf[sl] = rel.into();
                        }
                        if let Some(sl) = cx.slot_for("to") {
                            nf[sl] = if outgoing {
                                CypherValue::NodeRef(other).into()
                            } else {
                                vnode.clone().into()
                            };
                        }
                        out.push(nf);
                    }
                    let _ = Direction::Outgoing;
                }
            }
            Ok(out)
        }
        "apoc.nodes.cycles" => {
            // Cycles through the given nodes (edge-unique DFS, bounded).
            let mut out = Vec::new();
            for frame in frames {
                let CypherValue::List(nodes) = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?
                else {
                    continue;
                };
                for start_v in &nodes {
                    let Some(start) = node_ref_of(start_v) else {
                        continue;
                    };
                    let paths = apoc_expand(&*graph, start, "", "", 1, 6, "expand")?;
                    for (pnodes, pedges) in paths {
                        let last = pnodes.last().and_then(node_ref_of);
                        if last == Some(start) && !pedges.is_empty() {
                            let mut nf = frame.clone();
                            if let Some(sl) = cx.slot_for("path") {
                                nf[sl] = CypherValue::Path {
                                    nodes: pnodes,
                                    edges: pedges,
                                }
                                .into();
                            }
                            out.push(nf);
                        }
                    }
                }
            }
            Ok(out)
        }
        // -- apoc.nodes.group - virtual-graph aggregation -----
        "apoc.nodes.group" => {
            let mut out = Vec::new();
            for frame in frames {
                let labels: Vec<String> = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::List(l) => l.iter().map(key_string).collect(),
                    _ => Vec::new(),
                };
                let group_props: Vec<String> = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)?
                {
                    CypherValue::List(l) => l.iter().map(key_string).collect(),
                    _ => Vec::new(),
                };
                let aggs = match cx.opt_arg(2, frame, &*graph) {
                    Some(CypherValue::List(l)) => l,
                    _ => Vec::new(),
                };
                let agg_map = |i: usize| -> Vec<(String, String)> {
                    match aggs.get(i) {
                        Some(CypherValue::Map(m)) => m
                            .iter()
                            .map(|(k, v)| (k.clone(), key_string(v).to_lowercase()))
                            .collect(),
                        _ => vec![("*".to_string(), "count".to_string())],
                    }
                };
                let (node_aggs, rel_aggs) = (agg_map(0), agg_map(1));
                let config = match cx.opt_arg(3, frame, &*graph) {
                    Some(CypherValue::Map(m)) => m,
                    _ => Default::default(),
                };
                let self_rels = !matches!(config.get("selfRels"), Some(CypherValue::Bool(false)));
                let orphans = !matches!(config.get("orphans"), Some(CypherValue::Bool(false)));

                // Collect member nodes.
                let members: Vec<traverse_core::graph::types::NodeId> =
                    if labels.iter().any(|l| l == "*") {
                        graph.nodes_vec()
                    } else {
                        let mut v: Vec<_> = labels
                            .iter()
                            .flat_map(|l| graph.nodes_by_label(l))
                            .collect();
                        v.sort_unstable_by_key(|n| n.0);
                        v.dedup();
                        v
                    };
                // Group key: (sorted labels, groupBy value tuple).
                let mut groups: std::collections::BTreeMap<
                    String,
                    (Vec<String>, Vec<(String, CypherValue)>, Vec<u64>),
                > = Default::default();
                let mut member_group: std::collections::HashMap<u64, String> = Default::default();
                for nid in &members {
                    let Ok(n) = graph.get_node(*nid) else {
                        continue;
                    };
                    let mut nlabels: Vec<String> = n
                        .labels
                        .iter()
                        .map(|l| graph.strings().resolve(l.0).to_string())
                        .collect();
                    nlabels.sort();
                    let mut vals = Vec::new();
                    for gp in &group_props {
                        let v = graph
                            .get_node_property(*nid, gp)
                            .ok()
                            .flatten()
                            .map(|pv| CypherValue::from_property(&pv))
                            .unwrap_or(CypherValue::Null);
                        vals.push((gp.clone(), v));
                    }
                    let key = format!(
                        "{}|{}",
                        nlabels.join(":"),
                        vals.iter()
                            .map(|(_, v)| key_string(v))
                            .collect::<Vec<_>>()
                            .join("\u{1F}")
                    );
                    member_group.insert(nid.0, key.clone());
                    groups
                        .entry(key)
                        .or_insert_with(|| (nlabels, vals, Vec::new()))
                        .2
                        .push(nid.0);
                }

                // Aggregate helper over a set of entity property values.
                let aggregate = |vals: &[CypherValue], how: &str| -> CypherValue {
                    let nums: Vec<f64> = vals
                        .iter()
                        .filter_map(|v| match v {
                            CypherValue::Integer(i) => Some(*i as f64),
                            CypherValue::Float(f) => Some(*f),
                            _ => None,
                        })
                        .collect();
                    let all_int = vals
                        .iter()
                        .all(|v| matches!(v, CypherValue::Integer(_) | CypherValue::Null));
                    let wrap = |f: f64| {
                        if all_int && f.fract() == 0.0 {
                            CypherValue::Integer(f as i64)
                        } else {
                            CypherValue::Float(f)
                        }
                    };
                    match how {
                        "count" => CypherValue::Integer(
                            vals.iter()
                                .filter(|v| !matches!(v, CypherValue::Null))
                                .count() as i64,
                        ),
                        "sum" => wrap(nums.iter().sum()),
                        "min" => nums
                            .iter()
                            .cloned()
                            .fold(None::<f64>, |a, b| Some(a.map_or(b, |x| x.min(b))))
                            .map(wrap)
                            .unwrap_or(CypherValue::Null),
                        "max" => nums
                            .iter()
                            .cloned()
                            .fold(None::<f64>, |a, b| Some(a.map_or(b, |x| x.max(b))))
                            .map(wrap)
                            .unwrap_or(CypherValue::Null),
                        "avg" => {
                            if nums.is_empty() {
                                CypherValue::Null
                            } else {
                                CypherValue::Float(nums.iter().sum::<f64>() / nums.len() as f64)
                            }
                        }
                        "collect" => CypherValue::List(
                            vals.iter()
                                .filter(|v| !matches!(v, CypherValue::Null))
                                .cloned()
                                .collect(),
                        ),
                        _ => CypherValue::Null,
                    }
                };

                // Build virtual nodes.
                let mut vnodes: std::collections::BTreeMap<String, CypherValue> =
                    Default::default();
                for (key, (nlabels, vals, ids)) in &groups {
                    let mut props = traverse_core::graph::property::PropertyMap::new();
                    for (k, v) in vals {
                        if let Some(pv) = v.to_property() {
                            props.insert(k.clone(), pv);
                        }
                    }
                    for (prop, how) in &node_aggs {
                        let (name, value) = if prop == "*" {
                            (
                                "count_*".to_string(),
                                CypherValue::Integer(ids.len() as i64),
                            )
                        } else {
                            let vals: Vec<CypherValue> = ids
                                .iter()
                                .map(|id| {
                                    graph
                                        .get_node_property(
                                            traverse_core::graph::types::NodeId(*id),
                                            prop,
                                        )
                                        .ok()
                                        .flatten()
                                        .map(|pv| CypherValue::from_property(&pv))
                                        .unwrap_or(CypherValue::Null)
                                })
                                .collect();
                            (format!("{how}_{prop}"), aggregate(&vals, how))
                        };
                        if let Some(pv) = value.to_property() {
                            props.insert(name, pv);
                        }
                    }
                    vnodes.insert(
                        key.clone(),
                        CypherValue::Node {
                            id: next_virtual_id(),
                            labels: nlabels.clone(),
                            properties: props,
                        },
                    );
                }

                // Aggregate rels between groups: (src key, dst key, type).
                let mut vrels: std::collections::BTreeMap<
                    (String, String, String),
                    Vec<traverse_core::graph::types::EdgeId>,
                > = Default::default();
                use traverse_core::graph::types::Direction;
                for nid in &members {
                    for eid in graph
                        .edges_of(*nid, Direction::Outgoing, None)
                        .unwrap_or_default()
                    {
                        let Ok(e) = graph.get_edge(eid) else { continue };
                        let (Some(sk), Some(dk)) =
                            (member_group.get(&e.source.0), member_group.get(&e.target.0))
                        else {
                            continue;
                        };
                        if !self_rels && sk == dk {
                            continue;
                        }
                        let t = graph.strings().resolve(e.edge_type.0).to_string();
                        vrels
                            .entry((sk.clone(), dk.clone(), t))
                            .or_default()
                            .push(eid);
                    }
                }
                let mut rels_by_src: std::collections::BTreeMap<String, Vec<CypherValue>> =
                    Default::default();
                let mut all_rels = Vec::new();
                for ((sk, dk, t), eids) in &vrels {
                    let mut props = traverse_core::graph::property::PropertyMap::new();
                    for (prop, how) in &rel_aggs {
                        let (name, value) = if prop == "*" {
                            (
                                "count_*".to_string(),
                                CypherValue::Integer(eids.len() as i64),
                            )
                        } else {
                            let vals: Vec<CypherValue> = eids
                                .iter()
                                .map(|eid| {
                                    graph
                                        .get_edge_property(*eid, prop)
                                        .ok()
                                        .flatten()
                                        .map(|pv| CypherValue::from_property(&pv))
                                        .unwrap_or(CypherValue::Null)
                                })
                                .collect();
                            (format!("{how}_{prop}"), aggregate(&vals, how))
                        };
                        if let Some(pv) = value.to_property() {
                            props.insert(name, pv);
                        }
                    }
                    let (src_id, dst_id) = match (vnodes.get(sk), vnodes.get(dk)) {
                        (
                            Some(CypherValue::Node { id: s, .. }),
                            Some(CypherValue::Node { id: d, .. }),
                        ) => (*s, *d),
                        _ => continue,
                    };
                    let rel = CypherValue::Edge {
                        id: traverse_core::graph::types::EdgeId(next_virtual_id().0),
                        edge_type: t.clone(),
                        source: src_id,
                        target: dst_id,
                        properties: props,
                    };
                    rels_by_src.entry(sk.clone()).or_default().push(rel.clone());
                    all_rels.push(rel);
                }

                let all_nodes: Vec<CypherValue> = vnodes.values().cloned().collect();
                // One row per (virtual node, outgoing virtual rel);
                // orphan groups get a null-relationship row.
                for (key, vnode) in &vnodes {
                    let rels = rels_by_src.get(key);
                    let has_rels = rels.map(|r| !r.is_empty()).unwrap_or(false);
                    if !has_rels && !orphans {
                        continue;
                    }
                    let rel_rows: Vec<CypherValue> = if has_rels {
                        rels.cloned().unwrap_or_default()
                    } else {
                        vec![CypherValue::Null]
                    };
                    for rel in rel_rows {
                        let mut nf = frame.clone();
                        for (col, val) in [
                            ("nodes", CypherValue::List(all_nodes.clone())),
                            ("relationships", CypherValue::List(all_rels.clone())),
                            ("node", vnode.clone()),
                            ("relationship", rel),
                        ] {
                            if let Some(sl) = cx.slot_for(col) {
                                nf[sl] = val.into();
                            }
                        }
                        out.push(nf);
                    }
                }
            }
            Ok(out)
        }
        other => Err(super::unknown(other)),
    }
}
