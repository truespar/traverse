//! Writes. `CREATE`, `SET`, `REMOVE` and `MERGE` stream chunk by chunk: the rows of one input
//! chunk are applied in order and emitted before the next is pulled, so a row's effect is visible
//! to the next row's reads (`MERGE` depends on that) and imports run in memory proportional to a
//! chunk. The optimizer's `Eager` barriers keep later clauses from reading ahead of a write.
//!
//! `DELETE` stays a barrier: relationships go before nodes across all rows.

use std::collections::BTreeMap;

use rustc_hash::FxHashSet;
use traverse_core::graph::property::{PropertyMap, PropertyValue};
use traverse_core::graph::types::{Direction, EdgeId, LabelId, NodeId, PropertyKeyId};
use traverse_core::tx::view::GraphView;

use crate::ast::{Expr, Literal, NodePattern, Pattern, RelDirection, RelPattern};
use crate::error::CypherError;
use crate::executor::{
    check_all_edge_constraints, check_all_node_constraints, check_node_property_constraint,
    inc_stat,
};
use crate::types::CypherValue;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::expr::compile::compile_expr;
use super::super::expr::{CompiledExpr, EvalScratch};
use super::super::layout::Layout;
use super::super::plan::{RemoveAction, SeedRow, SetAction};
use super::super::values::{
    cypher_value_to_property_map, index_key, is_storable_property, resolve_ref,
};
use super::super::vector::{ValueRef, Vector};
use super::{PhysOperator, Staged, install, move_cols};

/// The node or relationship a `SET` / `REMOVE` item names.
enum Entity {
    Node(NodeId),
    Edge(EdgeId),
    Null,
}

fn entity_of(v: ValueRef<'_>, what: &str) -> Result<Entity, CypherError> {
    Ok(match v {
        ValueRef::Null => Entity::Null,
        ValueRef::Node(id) => Entity::Node(id),
        ValueRef::Edge { id, .. } => Entity::Edge(id),
        ValueRef::Any(CypherValue::Null) => Entity::Null,
        ValueRef::Any(CypherValue::Node { id, .. }) => Entity::Node(*id),
        ValueRef::Any(CypherValue::Edge { id, .. }) => Entity::Edge(*id),
        other => {
            return Err(CypherError::type_error(format!(
                "{what} expected a node or relationship, got {}",
                other.type_name()
            )));
        }
    })
}

/// The fresh reference to `e`, written back over a materialized copy in the row.
fn refreshed(e: &Entity, graph: &dyn GraphView) -> Option<CypherValue> {
    match e {
        Entity::Node(id) => Some(CypherValue::NodeRef(*id)),
        Entity::Edge(id) => graph.get_edge(*id).ok().map(|edge| CypherValue::EdgeRef {
            id: *id,
            edge_type_id: edge.edge_type,
            source: edge.source,
            target: edge.target,
        }),
        Entity::Null => None,
    }
}

/// Sets one property, bracketed by index maintenance; null removes it.
fn set_property(
    graph: &mut dyn GraphView,
    e: &Entity,
    key: &str,
    val: CypherValue,
) -> Result<(), CypherError> {
    if !is_storable_property(&val) {
        return Err(CypherError::type_error("InvalidPropertyType".to_string()));
    }
    let pval = val.to_property().unwrap_or(PropertyValue::Null);
    match e {
        Entity::Node(nid) => {
            let nid = *nid;
            graph.unindex_node_prop(nid, key);
            if matches!(pval, PropertyValue::Null) {
                if let Some(pid) = graph.strings().get(key).map(PropertyKeyId) {
                    let node = graph.get_node_mut(nid).map_err(CypherError::from)?;
                    node.properties.remove(pid);
                }
            } else {
                let pid = graph.intern_property_key(key);
                let node = graph.get_node_mut(nid).map_err(CypherError::from)?;
                node.properties.insert(pid, pval);
            }
            graph.index_node_prop(nid, key);
            inc_stat(|s| s.properties_set += 1);
            check_node_property_constraint(graph, nid, key)?;
        }
        Entity::Edge(eid) => {
            let eid = *eid;
            graph.unindex_edge_prop(eid, key);
            if matches!(pval, PropertyValue::Null) {
                if let Some(pid) = graph.strings().get(key).map(PropertyKeyId) {
                    let edge = graph.get_edge_mut(eid).map_err(CypherError::from)?;
                    edge.properties.remove(pid);
                }
            } else {
                let pid = graph.intern_property_key(key);
                let edge = graph.get_edge_mut(eid).map_err(CypherError::from)?;
                edge.properties.insert(pid, pval);
            }
            graph.index_edge_prop(eid, key);
            inc_stat(|s| s.properties_set += 1);
            check_all_edge_constraints(graph, eid)?;
        }
        Entity::Null => {}
    }
    Ok(())
}

/// Removes one property.
fn remove_property(graph: &mut dyn GraphView, e: &Entity, key: &str) -> Result<(), CypherError> {
    match e {
        Entity::Node(nid) => {
            let nid = *nid;
            graph.unindex_node_prop(nid, key);
            if let Some(pid) = graph.strings().get(key).map(PropertyKeyId) {
                let node = graph.get_node_mut(nid).map_err(CypherError::from)?;
                node.properties.remove(pid);
            }
            graph.index_node_prop(nid, key);
            inc_stat(|s| s.properties_set += 1);
            check_node_property_constraint(graph, nid, key)?;
        }
        Entity::Edge(eid) => {
            let eid = *eid;
            graph.unindex_edge_prop(eid, key);
            if let Some(pid) = graph.strings().get(key).map(PropertyKeyId) {
                let edge = graph.get_edge_mut(eid).map_err(CypherError::from)?;
                edge.properties.remove(pid);
            }
            graph.index_edge_prop(eid, key);
            inc_stat(|s| s.properties_set += 1);
            check_all_edge_constraints(graph, eid)?;
        }
        Entity::Null => {}
    }
    Ok(())
}

/// `var += map`.
fn merge_properties(
    graph: &mut dyn GraphView,
    e: &Entity,
    val: &CypherValue,
) -> Result<(), CypherError> {
    let map: BTreeMap<String, CypherValue> = match val {
        CypherValue::Map(m) => m.clone(),
        CypherValue::Node { properties, .. } | CypherValue::Edge { properties, .. } => properties
            .iter()
            .map(|(k, v)| (k.clone(), CypherValue::from_property(v)))
            .collect(),
        _ => BTreeMap::new(),
    };
    let count = map.len() as i64;
    let ops: Vec<(PropertyKeyId, Option<PropertyValue>)> = map
        .iter()
        .map(|(k, v)| {
            let pid = graph.intern_property_key(k);
            if matches!(v, CypherValue::Null) {
                (pid, None)
            } else {
                (pid, Some(v.to_property().unwrap_or(PropertyValue::Null)))
            }
        })
        .collect();
    match e {
        Entity::Node(nid) => {
            let nid = *nid;
            graph.unindex_node(nid);
            let node = graph.get_node_mut(nid).map_err(CypherError::from)?;
            for (pid, pv) in ops {
                match pv {
                    Some(pv) => {
                        node.properties.insert(pid, pv);
                    }
                    None => {
                        node.properties.remove(pid);
                    }
                }
            }
            graph.index_node(nid);
            inc_stat(|s| s.properties_set += count);
            check_all_node_constraints(graph, nid)?;
        }
        Entity::Edge(eid) => {
            let eid = *eid;
            graph.unindex_edge(eid);
            let edge = graph.get_edge_mut(eid).map_err(CypherError::from)?;
            for (pid, pv) in ops {
                match pv {
                    Some(pv) => {
                        edge.properties.insert(pid, pv);
                    }
                    None => {
                        edge.properties.remove(pid);
                    }
                }
            }
            graph.index_edge(eid);
            inc_stat(|s| s.properties_set += count);
        }
        Entity::Null => {}
    }
    Ok(())
}

/// Labels from static names and evaluated dynamic expressions; a null dynamic label is skipped.
fn label_names<'p>(
    labels: &[String],
    dynamic: &'p [CompiledExpr],
    rs: &ResultSet,
    graph: &dyn GraphView,
    lanes: &[usize],
    sc: &mut EvalScratch<'p>,
    what: &str,
) -> Result<Vec<String>, CypherError> {
    let mut out: Vec<String> = labels.to_vec();
    for e in dynamic {
        match e.eval_row(rs, graph, lanes, sc)? {
            CypherValue::String(s) => out.push(s),
            CypherValue::Null => {}
            other => {
                return Err(CypherError::type_error(format!(
                    "{what} dynamic label must evaluate to STRING, got {}",
                    other.type_name()
                )));
            }
        }
    }
    Ok(out)
}

/// Applies one `SET` item to the row at `lanes`; the entity's fresh reference comes back with
/// its position when the row should carry it on.
pub(crate) fn apply_set<'p>(
    action: &'p SetAction,
    rs: &ResultSet,
    lanes: &[usize],
    cx: &mut ExecCtx<'_>,
    sc: &mut EvalScratch<'p>,
) -> Result<Option<(DataPos, CypherValue)>, CypherError> {
    Ok(match action {
        SetAction::Unbound => None,
        SetAction::Property { entity, key, value } => {
            let val = value.eval_row(rs, cx.view(), lanes, sc)?;
            let e = entity_of(rs.value_at(*entity, lanes), "SET")?;
            set_property(cx.graph, &e, key, val)?;
            refreshed(&e, cx.view()).map(|v| (*entity, v))
        }
        SetAction::DynamicProperty { entity, key, value } => {
            let key = match key.eval_row(rs, cx.view(), lanes, sc)? {
                CypherValue::String(s) => s,
                CypherValue::Null => return Ok(None),
                other => {
                    return Err(CypherError::type_error(format!(
                        "SET n[key] = ...: key must evaluate to STRING, got {}",
                        other.type_name()
                    )));
                }
            };
            let val = value.eval_row(rs, cx.view(), lanes, sc)?;
            let e = entity_of(rs.value_at(*entity, lanes), "SET")?;
            set_property(cx.graph, &e, &key, val)?;
            refreshed(&e, cx.view()).map(|v| (*entity, v))
        }
        SetAction::AllProperties { entity, value } => {
            // `n = m` copies another entity's properties: resolve a reference first
            let val = resolve_ref(value.eval_row(rs, cx.view(), lanes, sc)?, cx.view());
            let props = cypher_value_to_property_map(&val)?;
            let count = props.len() as i64;
            let e = entity_of(rs.value_at(*entity, lanes), "SET")?;
            match &e {
                Entity::Node(nid) => {
                    cx.graph
                        .update_node_properties(*nid, props)
                        .map_err(CypherError::from)?;
                    inc_stat(|s| s.properties_set += count);
                    check_all_node_constraints(cx.graph, *nid)?;
                }
                Entity::Edge(eid) => {
                    cx.graph
                        .update_edge_properties(*eid, props)
                        .map_err(CypherError::from)?;
                    inc_stat(|s| s.properties_set += count);
                }
                Entity::Null => {}
            }
            refreshed(&e, cx.view()).map(|v| (*entity, v))
        }
        SetAction::MergeProperties { entity, value } => {
            let val = resolve_ref(value.eval_row(rs, cx.view(), lanes, sc)?, cx.view());
            let e = entity_of(rs.value_at(*entity, lanes), "SET")?;
            merge_properties(cx.graph, &e, &val)?;
            refreshed(&e, cx.view()).map(|v| (*entity, v))
        }
        SetAction::Labels {
            entity,
            labels,
            dynamic,
        } => {
            let nid = match rs.value_at(*entity, lanes) {
                ValueRef::Null | ValueRef::Any(CypherValue::Null) => return Ok(None),
                v => match v.node_id() {
                    Some(id) => id,
                    None => {
                        return Err(CypherError::type_error(format!(
                            "SET label expected a node, got {}",
                            v.type_name()
                        )));
                    }
                },
            };
            let names = label_names(labels, dynamic, rs, cx.view(), lanes, sc, "SET")?;
            for l in &names {
                let _ = cx.graph.add_label(nid, l);
                inc_stat(|s| s.labels_added += 1);
            }
            check_all_node_constraints(cx.graph, nid)?;
            Some((*entity, CypherValue::NodeRef(nid)))
        }
    })
}

/// Applies one `REMOVE` item.
fn apply_remove<'p>(
    action: &'p RemoveAction,
    rs: &ResultSet,
    lanes: &[usize],
    cx: &mut ExecCtx<'_>,
    sc: &mut EvalScratch<'p>,
) -> Result<Option<(DataPos, CypherValue)>, CypherError> {
    Ok(match action {
        RemoveAction::Unbound => None,
        RemoveAction::Property { entity, key } => {
            let e = match entity_of(rs.value_at(*entity, lanes), "REMOVE") {
                Ok(e) => e,
                Err(_) => return Ok(None),
            };
            remove_property(cx.graph, &e, key)?;
            refreshed(&e, cx.view()).map(|v| (*entity, v))
        }
        RemoveAction::Labels {
            entity,
            labels,
            dynamic,
        } => {
            let Some(nid) = rs.value_at(*entity, lanes).node_id() else {
                return Ok(None);
            };
            let names = label_names(labels, dynamic, rs, cx.view(), lanes, sc, "REMOVE")?;
            for l in &names {
                let _ = cx.graph.remove_label(nid, l);
                inc_stat(|s| s.labels_removed += 1);
            }
            Some((*entity, CypherValue::NodeRef(nid)))
        }
    })
}

/// A streaming row consumer: the rows of one input chunk, each mapped through `moves` plus its
/// own outputs, staged and emitted.
struct Consumer<'p> {
    in_groups: &'p [GroupId],
    out_group: GroupId,
    moves: Vec<(DataPos, u16)>,
    staged: Option<Staged>,
    done: bool,
}

impl<'p> Consumer<'p> {
    fn new(in_groups: &'p [GroupId], out_group: GroupId, moves: &'p [(DataPos, DataPos)]) -> Self {
        Self {
            in_groups,
            out_group,
            moves: move_cols(moves),
            staged: None,
            done: false,
        }
    }

    /// Stages the row at `lanes`: the moves, `overrides` replacing moved entity values, and
    /// `extra` columns.
    fn push(
        &self,
        st: &mut Staged,
        rs: &ResultSet,
        lanes: &[usize],
        overrides: &[(DataPos, CypherValue)],
        extra: &[(u16, CypherValue)],
    ) -> Result<(), CypherError> {
        st.push_row(|cols| {
            for (src, c) in &self.moves {
                match overrides.iter().find(|(p, _)| p == src) {
                    Some((_, v)) => cols[*c as usize].push_owned(v.clone()),
                    None => cols[*c as usize].push(rs.value_at(*src, lanes)),
                }
            }
            for (c, v) in extra {
                cols[*c as usize].push_owned(v.clone());
            }
        })
    }

    /// Emits a staged chunk if one is pending; `Some(true)` emitted, `Some(false)` nothing staged
    /// and the input is exhausted, `None` pull the next input chunk.
    fn emit_or_pull(&mut self, rs: &mut ResultSet) -> Result<Option<bool>, CypherError> {
        if let Some(st) = &mut self.staged {
            if st.emit(rs, self.out_group)? {
                return Ok(Some(true));
            }
            self.staged = None;
        }
        if self.done {
            return Ok(Some(false));
        }
        Ok(None)
    }

    fn reset(&mut self) {
        self.staged = None;
        self.done = false;
    }
}

/// Evaluates an optional property-map expression for the row.
fn eval_props<'p>(
    expr: Option<&'p CompiledExpr>,
    rs: &ResultSet,
    graph: &dyn GraphView,
    lanes: &[usize],
    sc: &mut EvalScratch<'p>,
) -> Result<PropertyMap, CypherError> {
    match expr {
        Some(e) => {
            let v = resolve_ref(e.eval_row(rs, graph, lanes, sc)?, graph);
            cypher_value_to_property_map(&v)
        }
        None => Ok(PropertyMap::new()),
    }
}

/// `CREATE (n:Label {props})`.
pub struct CreateNodeOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    out: Option<DataPos>,
    labels: &'p [String],
    dynamic_labels: &'p [CompiledExpr],
    properties: Option<&'p CompiledExpr>,
    consumer: Consumer<'p>,
    scratch: EvalScratch<'p>,
}

impl<'p> CreateNodeOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        out: Option<DataPos>,
        labels: &'p [String],
        dynamic_labels: &'p [CompiledExpr],
        properties: Option<&'p CompiledExpr>,
        in_groups: &'p [GroupId],
        out_group: GroupId,
        moves: &'p [(DataPos, DataPos)],
    ) -> Self {
        Self {
            input,
            out,
            labels,
            dynamic_labels,
            properties,
            consumer: Consumer::new(in_groups, out_group, moves),
            scratch: EvalScratch::new(),
        }
    }
}

impl PhysOperator for CreateNodeOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if let Some(r) = self.consumer.emit_or_pull(rs)? {
                return Ok(r);
            }
            if !self.input.next(rs, cx)? {
                self.consumer.done = true;
                continue;
            }
            let width = rs.group(self.consumer.out_group).cols.len();
            let mut st = Staged::new(width);
            let total = rs.row_count(self.consumer.in_groups);
            let mut lanes = Vec::new();
            for row in 0..total {
                rs.row_lanes(self.consumer.in_groups, row, &mut lanes);
                if row & 0xFFF == 0xFFF {
                    cx.check()?;
                }
                let props = eval_props(self.properties, rs, cx.view(), &lanes, &mut self.scratch)?;
                let names = label_names(
                    self.labels,
                    self.dynamic_labels,
                    rs,
                    cx.view(),
                    &lanes,
                    &mut self.scratch,
                    "CREATE",
                )?;
                let label_strs: Vec<&str> = names.iter().map(String::as_str).collect();
                let prop_count = props.len() as i64;
                let nid = cx
                    .graph
                    .create_node(&label_strs, props)
                    .map_err(CypherError::from)?;
                inc_stat(|s| s.nodes_created += 1);
                inc_stat(|s| s.labels_added += label_strs.len() as i64);
                inc_stat(|s| s.properties_set += prop_count);
                check_all_node_constraints(cx.graph, nid)?;
                let extra: Vec<(u16, CypherValue)> = match self.out {
                    Some(p) => vec![(p.col, CypherValue::NodeRef(nid))],
                    None => Vec::new(),
                };
                self.consumer.push(&mut st, rs, &lanes, &[], &extra)?;
                if row & 0xFFF == 0xFFF {
                    cx.check()?;
                }
            }
            cx.tick()?;
            self.consumer.staged = Some(st);
        }
    }

    fn reset(&mut self) {
        self.consumer.reset();
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// `CREATE (a)-[r:T {props}]->(b)`.
pub struct CreateRelOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    out: Option<DataPos>,
    from: DataPos,
    to: DataPos,
    edge_type: &'p str,
    dynamic_edge_type: Option<&'p CompiledExpr>,
    properties: Option<&'p CompiledExpr>,
    consumer: Consumer<'p>,
    scratch: EvalScratch<'p>,
}

impl<'p> CreateRelOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        out: Option<DataPos>,
        from: DataPos,
        to: DataPos,
        edge_type: &'p str,
        dynamic_edge_type: Option<&'p CompiledExpr>,
        properties: Option<&'p CompiledExpr>,
        in_groups: &'p [GroupId],
        out_group: GroupId,
        moves: &'p [(DataPos, DataPos)],
    ) -> Self {
        Self {
            input,
            out,
            from,
            to,
            edge_type,
            dynamic_edge_type,
            properties,
            consumer: Consumer::new(in_groups, out_group, moves),
            scratch: EvalScratch::new(),
        }
    }
}

fn node_id_or_err(v: ValueRef<'_>, what: &str) -> Result<NodeId, CypherError> {
    v.node_id().ok_or_else(|| {
        CypherError::type_error(format!("expected node for {what}, got {}", v.type_name()))
    })
}

impl PhysOperator for CreateRelOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if let Some(r) = self.consumer.emit_or_pull(rs)? {
                return Ok(r);
            }
            if !self.input.next(rs, cx)? {
                self.consumer.done = true;
                continue;
            }
            let width = rs.group(self.consumer.out_group).cols.len();
            let mut st = Staged::new(width);
            let total = rs.row_count(self.consumer.in_groups);
            let mut lanes = Vec::new();
            for row in 0..total {
                rs.row_lanes(self.consumer.in_groups, row, &mut lanes);
                if row & 0xFFF == 0xFFF {
                    cx.check()?;
                }
                let from_id =
                    node_id_or_err(rs.value_at(self.from, &lanes), "the start of CREATE")?;
                let to_id = node_id_or_err(rs.value_at(self.to, &lanes), "the end of CREATE")?;
                let props = eval_props(self.properties, rs, cx.view(), &lanes, &mut self.scratch)?;
                let dynamic: Option<String> = match self.dynamic_edge_type {
                    Some(e) => match e.eval_row(rs, cx.view(), &lanes, &mut self.scratch)? {
                        CypherValue::String(s) => Some(s),
                        other => {
                            return Err(CypherError::type_error(format!(
                                "dynamic edge type must evaluate to STRING, got {}",
                                other.type_name()
                            )));
                        }
                    },
                    None => None,
                };
                let etype = dynamic.as_deref().unwrap_or(self.edge_type);
                let prop_count = props.len() as i64;
                let eid = cx
                    .graph
                    .create_edge(from_id, to_id, etype, props)
                    .map_err(CypherError::from)?;
                check_all_edge_constraints(cx.view(), eid)?;
                inc_stat(|s| s.relationships_created += 1);
                inc_stat(|s| s.properties_set += prop_count);
                let extra: Vec<(u16, CypherValue)> = match self.out {
                    Some(p) => {
                        let edge = cx.view().get_edge(eid).map_err(CypherError::from)?;
                        vec![(
                            p.col,
                            CypherValue::EdgeRef {
                                id: eid,
                                edge_type_id: edge.edge_type,
                                source: edge.source,
                                target: edge.target,
                            },
                        )]
                    }
                    None => Vec::new(),
                };
                self.consumer.push(&mut st, rs, &lanes, &[], &extra)?;
                if row & 0xFFF == 0xFFF {
                    cx.check()?;
                }
            }
            cx.tick()?;
            self.consumer.staged = Some(st);
        }
    }

    fn reset(&mut self) {
        self.consumer.reset();
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// `SET`.
pub struct SetPropsOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    items: &'p [SetAction],
    consumer: Consumer<'p>,
    scratch: EvalScratch<'p>,
}

impl<'p> SetPropsOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        items: &'p [SetAction],
        in_groups: &'p [GroupId],
        out_group: GroupId,
        moves: &'p [(DataPos, DataPos)],
    ) -> Self {
        Self {
            input,
            items,
            consumer: Consumer::new(in_groups, out_group, moves),
            scratch: EvalScratch::new(),
        }
    }
}

impl PhysOperator for SetPropsOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if let Some(r) = self.consumer.emit_or_pull(rs)? {
                return Ok(r);
            }
            if !self.input.next(rs, cx)? {
                self.consumer.done = true;
                continue;
            }
            let width = rs.group(self.consumer.out_group).cols.len();
            let mut st = Staged::new(width);
            let total = rs.row_count(self.consumer.in_groups);
            let mut lanes = Vec::new();
            for row in 0..total {
                rs.row_lanes(self.consumer.in_groups, row, &mut lanes);
                let mut overrides: Vec<(DataPos, CypherValue)> = Vec::new();
                for item in self.items {
                    if let Some(o) = apply_set(item, rs, &lanes, cx, &mut self.scratch)? {
                        overrides.retain(|(p, _)| *p != o.0);
                        overrides.push(o);
                    }
                }
                self.consumer.push(&mut st, rs, &lanes, &overrides, &[])?;
                if row & 0xFFF == 0xFFF {
                    cx.check()?;
                }
            }
            cx.tick()?;
            self.consumer.staged = Some(st);
        }
    }

    fn reset(&mut self) {
        self.consumer.reset();
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// `REMOVE`.
pub struct RemoveOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    items: &'p [RemoveAction],
    consumer: Consumer<'p>,
    scratch: EvalScratch<'p>,
}

impl<'p> RemoveOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        items: &'p [RemoveAction],
        in_groups: &'p [GroupId],
        out_group: GroupId,
        moves: &'p [(DataPos, DataPos)],
    ) -> Self {
        Self {
            input,
            items,
            consumer: Consumer::new(in_groups, out_group, moves),
            scratch: EvalScratch::new(),
        }
    }
}

impl PhysOperator for RemoveOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if let Some(r) = self.consumer.emit_or_pull(rs)? {
                return Ok(r);
            }
            if !self.input.next(rs, cx)? {
                self.consumer.done = true;
                continue;
            }
            let width = rs.group(self.consumer.out_group).cols.len();
            let mut st = Staged::new(width);
            let total = rs.row_count(self.consumer.in_groups);
            let mut lanes = Vec::new();
            for row in 0..total {
                rs.row_lanes(self.consumer.in_groups, row, &mut lanes);
                let mut overrides: Vec<(DataPos, CypherValue)> = Vec::new();
                for item in self.items {
                    if let Some(o) = apply_remove(item, rs, &lanes, cx, &mut self.scratch)? {
                        overrides.retain(|(p, _)| *p != o.0);
                        overrides.push(o);
                    }
                }
                self.consumer.push(&mut st, rs, &lanes, &overrides, &[])?;
                if row & 0xFFF == 0xFFF {
                    cx.check()?;
                }
            }
            cx.tick()?;
            self.consumer.staged = Some(st);
        }
    }

    fn reset(&mut self) {
        self.consumer.reset();
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// `DELETE` / `DETACH DELETE`: a barrier, relationships before nodes across every row.
pub struct DeleteOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    exprs: &'p [CompiledExpr],
    detach: bool,
    consumer: Consumer<'p>,
    scratch: EvalScratch<'p>,
}

impl<'p> DeleteOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        exprs: &'p [CompiledExpr],
        detach: bool,
        in_groups: &'p [GroupId],
        out_group: GroupId,
        moves: &'p [(DataPos, DataPos)],
    ) -> Self {
        Self {
            input,
            exprs,
            detach,
            consumer: Consumer::new(in_groups, out_group, moves),
            scratch: EvalScratch::new(),
        }
    }
}

/// Collects the ids a `DELETE` value names.
fn collect_delete(
    val: CypherValue,
    nodes: &mut Vec<NodeId>,
    edges: &mut Vec<EdgeId>,
) -> Result<(), CypherError> {
    match val {
        CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => nodes.push(id),
        CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } => edges.push(id),
        CypherValue::Path {
            nodes: ns,
            edges: es,
        } => {
            for n in ns {
                if let CypherValue::Node { id, .. } | CypherValue::NodeRef(id) = n {
                    nodes.push(id);
                }
            }
            for e in es {
                if let CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } = e {
                    edges.push(id);
                }
            }
        }
        CypherValue::List(items) => {
            for item in items {
                match item {
                    CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => nodes.push(id),
                    CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } => {
                        edges.push(id)
                    }
                    CypherValue::Null => {}
                    _ => {
                        return Err(CypherError::type_error(
                            "DELETE requires node or relationship".to_string(),
                        ));
                    }
                }
            }
        }
        CypherValue::Null => {}
        _ => {
            return Err(CypherError::type_error(
                "DELETE requires node or relationship".to_string(),
            ));
        }
    }
    Ok(())
}

impl PhysOperator for DeleteOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.consumer.staged.is_none() && !self.consumer.done {
            let width = rs.group(self.consumer.out_group).cols.len();
            let mut st = Staged::new(width);
            let mut node_ids: Vec<NodeId> = Vec::new();
            let mut edge_ids: Vec<EdgeId> = Vec::new();
            while self.input.next(rs, cx)? {
                let total = rs.row_count(self.consumer.in_groups);
                let mut lanes = Vec::new();
                for row in 0..total {
                    rs.row_lanes(self.consumer.in_groups, row, &mut lanes);
                    if row & 0xFFF == 0xFFF {
                        cx.check()?;
                    }
                    for e in self.exprs {
                        let v = e.eval_row(rs, cx.view(), &lanes, &mut self.scratch)?;
                        collect_delete(v, &mut node_ids, &mut edge_ids)?;
                    }
                    self.consumer.push(&mut st, rs, &lanes, &[], &[])?;
                }
                cx.tick()?;
            }
            let edge_set: FxHashSet<EdgeId> = edge_ids.into_iter().collect();
            for eid in &edge_set {
                if cx.graph.delete_edge(*eid).is_ok() {
                    inc_stat(|s| s.relationships_deleted += 1);
                }
            }
            let node_set: FxHashSet<NodeId> = node_ids.into_iter().collect();
            for nid in &node_set {
                if self.detach {
                    if let Ok(out) = cx.view().edges_of(*nid, Direction::Both, None) {
                        let cascade = out.iter().filter(|e| !edge_set.contains(e)).count();
                        inc_stat(|s| s.relationships_deleted += cascade as i64);
                    }
                    if cx.graph.delete_node_cascading(*nid).is_ok() {
                        inc_stat(|s| s.nodes_deleted += 1);
                    }
                } else {
                    cx.graph.delete_node(*nid).map_err(CypherError::from)?;
                    inc_stat(|s| s.nodes_deleted += 1);
                }
            }
            self.consumer.staged = Some(st);
            self.consumer.done = true;
        }
        match self.consumer.emit_or_pull(rs)? {
            Some(r) => Ok(r),
            None => Ok(false),
        }
    }

    fn reset(&mut self) {
        self.consumer.reset();
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

// -- MERGE -------------------------------------------------------------

/// A node pattern of the merge with its property map compiled against the output row.
struct MergeNode<'p> {
    pat: &'p NodePattern,
    props: Option<CompiledExpr>,
    /// Output position of its variable, when it has one.
    bind: Option<DataPos>,
}

struct MergeRel<'p> {
    pat: &'p RelPattern,
    props: Option<CompiledExpr>,
    bind: Option<DataPos>,
}

/// `MERGE`: per row, match the pattern or create it, then `ON CREATE` / `ON MATCH`.
pub struct MergeOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    node: MergeNode<'p>,
    rel: Option<(MergeRel<'p>, MergeNode<'p>)>,
    on_create: Option<&'p [SetAction]>,
    on_match: Option<&'p [SetAction]>,
    consumer: Consumer<'p>,
    width: usize,
    scratch: EvalScratch<'p>,
}

impl<'p> MergeOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        pattern: &'p Pattern,
        on_create: Option<&'p [SetAction]>,
        on_match: Option<&'p [SetAction]>,
        binds: &'p [(String, DataPos)],
        out_layout: &'p Layout,
        in_groups: &'p [GroupId],
        out_group: GroupId,
        moves: &'p [(DataPos, DataPos)],
    ) -> Result<Self, CypherError> {
        // null properties in a MERGE pattern are rejected up front
        for part in &pattern.parts {
            let mut maps: Vec<&Expr> = part.element.node.properties.iter().collect();
            maps.extend(
                part.element
                    .chain
                    .iter()
                    .filter_map(|c| c.rel.properties.as_ref()),
            );
            for m in maps {
                if let Expr::MapLiteral(pairs) = m
                    && pairs
                        .iter()
                        .any(|(_, v)| matches!(v, Expr::Literal(Literal::Null)))
                {
                    return Err(CypherError::semantic(
                        "MERGE does not support null property values".to_string(),
                    ));
                }
            }
        }
        let bind_of = |v: &Option<String>| {
            v.as_ref()
                .and_then(|v| binds.iter().find(|(n, _)| n == v).map(|(_, p)| *p))
        };
        let compile = |e: &Option<Expr>| -> Result<Option<CompiledExpr>, CypherError> {
            match e {
                Some(e) => Ok(Some(compile_expr(e, out_layout)?)),
                None => Ok(None),
            }
        };
        let node_of = |pat: &'p NodePattern| -> Result<MergeNode<'p>, CypherError> {
            Ok(MergeNode {
                pat,
                props: compile(&pat.properties)?,
                bind: bind_of(&pat.variable),
            })
        };
        let part = &pattern.parts[0];
        let node = node_of(&part.element.node)?;
        let rel = match part.element.chain.first() {
            Some(chain) => Some((
                MergeRel {
                    pat: &chain.rel,
                    props: compile(&chain.rel.properties)?,
                    bind: bind_of(&chain.rel.variable),
                },
                node_of(&chain.node)?,
            )),
            None => None,
        };
        Ok(Self {
            input,
            node,
            rel,
            on_create,
            on_match,
            consumer: Consumer::new(in_groups, out_group, moves),
            width: 0,
            scratch: EvalScratch::new(),
        })
    }

    /// Installs `row` as the single lane of the output group and returns the lane assignment.
    fn install_row(&self, rs: &mut ResultSet, row: &[CypherValue], lanes: &mut Vec<usize>) {
        let cols: Vec<(u16, Vector)> = row
            .iter()
            .enumerate()
            .map(|(c, v)| (c as u16, Vector::from_values(vec![v.clone()])))
            .collect();
        install(rs, self.consumer.out_group, 1, cols);
        rs.row_lanes(&[self.consumer.out_group], 0, lanes);
    }
}

/// The expected properties of a merge pattern element for the current row.
fn expected_props<'p>(
    props: &'p Option<CompiledExpr>,
    rs: &ResultSet,
    graph: &dyn GraphView,
    lanes: &[usize],
    sc: &mut EvalScratch<'p>,
) -> Result<BTreeMap<String, CypherValue>, CypherError> {
    match props {
        Some(e) => match e.eval_row(rs, graph, lanes, sc)? {
            CypherValue::Map(m) => Ok(m),
            CypherValue::Null => Ok(BTreeMap::new()),
            other => Err(CypherError::type_error(format!(
                "MERGE properties must be a MAP, got {}",
                other.type_name()
            ))),
        },
        None => Ok(BTreeMap::new()),
    }
}

/// Property keys of the first label with a single-property index.
fn indexed_keys(
    graph: &dyn GraphView,
    label: Option<&str>,
    keys: &BTreeMap<String, CypherValue>,
) -> Vec<String> {
    let Some(label) = label else {
        return Vec::new();
    };
    let st = graph.strings();
    let Some(lid) = st.get(label) else {
        return Vec::new();
    };
    graph
        .index_catalog()
        .descriptors()
        .iter()
        .filter(|d| d.label_id.0 == lid)
        .filter_map(|d| st.try_resolve(d.property_key.0))
        .filter(|k| !k.contains('\u{1F}') && keys.contains_key(*k))
        .map(|k| k.to_string())
        .collect()
}

/// Nodes matching a merge node pattern: through a property index when one covers a key, else a
/// label scan with the properties and labels checked.
fn match_nodes(
    graph: &dyn GraphView,
    pat: &NodePattern,
    expected: &BTreeMap<String, CypherValue>,
) -> Result<Vec<NodeId>, CypherError> {
    let label = pat.labels.first().map(String::as_str);
    let via_index = indexed_keys(graph, label, expected);
    let candidates: Vec<NodeId> = match via_index.first() {
        Some(key) => match index_key(&expected[key]) {
            Some(k) => graph
                .index_lookup(label.unwrap(), key, &k)
                .unwrap_or_default(),
            None => Vec::new(),
        },
        None => match label {
            Some(l) => graph.nodes_by_label(l),
            None => graph.nodes_vec(),
        },
    };
    let st = graph.strings();
    let label_ids: Option<Vec<LabelId>> =
        pat.labels.iter().map(|l| st.get(l).map(LabelId)).collect();
    let Some(label_ids) = label_ids else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for nid in candidates {
        let Ok(node) = graph.get_node(nid) else {
            continue;
        };
        if !label_ids.iter().all(|l| node.has_label(*l)) {
            continue;
        }
        let all_match = expected.iter().all(|(k, v)| {
            let actual = st
                .get(k)
                .map(PropertyKeyId)
                .and_then(|pid| node.properties.get(pid))
                .map(|pv| CypherValue::from_property(&pv))
                .unwrap_or(CypherValue::Null);
            actual == *v
        });
        if all_match {
            out.push(nid);
        }
    }
    Ok(out)
}

/// Creates the node of a merge pattern.
fn create_node(
    graph: &mut dyn GraphView,
    pat: &NodePattern,
    expected: &BTreeMap<String, CypherValue>,
) -> Result<NodeId, CypherError> {
    let props = cypher_value_to_property_map(&CypherValue::Map(expected.clone()))?;
    let label_strs: Vec<&str> = pat.labels.iter().map(String::as_str).collect();
    let prop_count = props.len() as i64;
    let nid = graph
        .create_node(&label_strs, props)
        .map_err(CypherError::from)?;
    inc_stat(|s| s.nodes_created += 1);
    inc_stat(|s| s.labels_added += label_strs.len() as i64);
    inc_stat(|s| s.properties_set += prop_count);
    check_all_node_constraints(graph, nid)?;
    Ok(nid)
}

/// Edges from `from` to `to` matching the relationship pattern.
fn find_edges(
    graph: &dyn GraphView,
    from: NodeId,
    to: NodeId,
    edge_type: &str,
    expected: &BTreeMap<String, CypherValue>,
    undirected: bool,
) -> Vec<EdgeId> {
    let tf = if edge_type.is_empty() {
        None
    } else {
        Some(edge_type)
    };
    let direction = if undirected {
        Direction::Both
    } else {
        Direction::Outgoing
    };
    let Ok(ids) = graph.edges_of(from, direction, tf) else {
        return Vec::new();
    };
    let st = graph.strings();
    let mut out = Vec::new();
    for eid in ids {
        let Ok(e) = graph.get_edge(eid) else { continue };
        let ok = if undirected {
            (e.source == from && e.target == to) || (e.source == to && e.target == from)
        } else {
            e.target == to
        };
        if !ok {
            continue;
        }
        let props_ok = expected.iter().all(|(k, v)| {
            let actual = st
                .get(k)
                .map(PropertyKeyId)
                .and_then(|pid| e.properties.get(pid));
            match (actual, v.to_property()) {
                (Some(a), Some(exp)) => a == exp,
                _ => false,
            }
        });
        if props_ok {
            out.push(eid);
        }
    }
    out
}

fn edge_ref(graph: &dyn GraphView, eid: EdgeId) -> Result<CypherValue, CypherError> {
    let e = graph.get_edge(eid).map_err(CypherError::from)?;
    Ok(CypherValue::EdgeRef {
        id: eid,
        edge_type_id: e.edge_type,
        source: e.source,
        target: e.target,
    })
}

impl MergeOp<'_> {
    /// Runs the merge for one input row (`lanes` over the input groups), staging its output rows.
    fn merge_row(
        &mut self,
        rs: &mut ResultSet,
        in_lanes: &[usize],
        cx: &mut ExecCtx<'_>,
        st: &mut Staged,
    ) -> Result<(), CypherError> {
        // the output row: the input columns moved, the pattern's variables as they already are
        let mut row: Vec<CypherValue> = vec![CypherValue::Null; self.width];
        for (src, c) in &self.consumer.moves {
            row[*c as usize] = rs.value_at(*src, in_lanes).to_owned();
        }
        let mut lanes = Vec::new();
        self.install_row(rs, &row, &mut lanes);
        let bound_node = |row: &[CypherValue], bind: Option<DataPos>| -> Option<NodeId> {
            bind.and_then(|p| match &row[p.col as usize] {
                CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => Some(*id),
                _ => None,
            })
        };
        // the pattern's property maps belong to the operator, so they evaluate with a scratch
        // of their own
        let mut local = EvalScratch::new();
        let node_expected = expected_props(&self.node.props, rs, cx.view(), &lanes, &mut local)?;
        let (target_expected, rel_expected) = match &self.rel {
            Some((rel, target)) => (
                expected_props(&target.props, rs, cx.view(), &lanes, &mut local)?,
                expected_props(&rel.props, rs, cx.view(), &lanes, &mut local)?,
            ),
            None => (BTreeMap::new(), BTreeMap::new()),
        };
        drop(local);
        // (row with binds, created?) per output row
        let mut outputs: Vec<(Vec<CypherValue>, bool)> = Vec::new();
        match &self.rel {
            None => {
                if let Some(nid) = bound_node(&row, self.node.bind) {
                    let mut r = row.clone();
                    if let Some(p) = self.node.bind {
                        r[p.col as usize] = CypherValue::NodeRef(nid);
                    }
                    outputs.push((r, false));
                } else {
                    let matched = match_nodes(cx.view(), self.node.pat, &node_expected)?;
                    if matched.is_empty() {
                        let nid = create_node(cx.graph, self.node.pat, &node_expected)?;
                        let mut r = row.clone();
                        if let Some(p) = self.node.bind {
                            r[p.col as usize] = CypherValue::NodeRef(nid);
                        }
                        outputs.push((r, true));
                    } else {
                        for nid in matched {
                            let mut r = row.clone();
                            if let Some(p) = self.node.bind {
                                r[p.col as usize] = CypherValue::NodeRef(nid);
                            }
                            outputs.push((r, false));
                        }
                    }
                }
            }
            Some((rel, target)) => {
                let edge_type = rel.pat.types.first().cloned().unwrap_or_default();
                let resolve = |graph: &dyn GraphView,
                               node: &MergeNode<'_>,
                               expected: &BTreeMap<String, CypherValue>,
                               row: &[CypherValue]|
                 -> Result<Option<NodeId>, CypherError> {
                    if let Some(id) = bound_node(row, node.bind) {
                        return Ok(Some(id));
                    }
                    Ok(match_nodes(graph, node.pat, expected)?.into_iter().next())
                };
                let from_id = resolve(cx.view(), &self.node, &node_expected, &row)?;
                let to_id = resolve(cx.view(), target, &target_expected, &row)?;
                let oriented = |fid: NodeId, tid: NodeId| match rel.pat.direction {
                    RelDirection::Incoming => (tid, fid),
                    _ => (fid, tid),
                };
                let bind_all =
                    |r: &mut Vec<CypherValue>, fid: NodeId, tid: NodeId, e: CypherValue| {
                        if let Some(p) = self.node.bind {
                            r[p.col as usize] = CypherValue::NodeRef(fid);
                        }
                        if let Some(p) = target.bind {
                            r[p.col as usize] = CypherValue::NodeRef(tid);
                        }
                        if let Some(p) = rel.bind {
                            r[p.col as usize] = e;
                        }
                    };
                let create_rel = |graph: &mut dyn GraphView,
                                  fid: NodeId,
                                  tid: NodeId|
                 -> Result<EdgeId, CypherError> {
                    let (af, at) = oriented(fid, tid);
                    let props =
                        cypher_value_to_property_map(&CypherValue::Map(rel_expected.clone()))?;
                    let prop_count = props.len() as i64;
                    let eid = graph
                        .create_edge(af, at, &edge_type, props)
                        .map_err(CypherError::from)?;
                    check_all_edge_constraints(&*graph, eid)?;
                    inc_stat(|s| s.relationships_created += 1);
                    inc_stat(|s| s.properties_set += prop_count);
                    Ok(eid)
                };
                match (from_id, to_id) {
                    (Some(fid), Some(tid)) => {
                        let undirected = matches!(rel.pat.direction, RelDirection::Both);
                        let (af, at) = oriented(fid, tid);
                        let matched =
                            find_edges(cx.view(), af, at, &edge_type, &rel_expected, undirected);
                        if matched.is_empty() {
                            let eid = create_rel(cx.graph, fid, tid)?;
                            let mut r = row.clone();
                            bind_all(&mut r, fid, tid, edge_ref(cx.view(), eid)?);
                            outputs.push((r, true));
                        } else {
                            for eid in matched {
                                let mut r = row.clone();
                                bind_all(&mut r, fid, tid, edge_ref(cx.view(), eid)?);
                                outputs.push((r, false));
                            }
                        }
                    }
                    _ => {
                        let fid = match from_id {
                            Some(id) => id,
                            None => create_node(cx.graph, self.node.pat, &node_expected)?,
                        };
                        let tid = match to_id {
                            Some(id) => id,
                            None => create_node(cx.graph, target.pat, &target_expected)?,
                        };
                        let eid = create_rel(cx.graph, fid, tid)?;
                        let mut r = row.clone();
                        bind_all(&mut r, fid, tid, edge_ref(cx.view(), eid)?);
                        outputs.push((r, true));
                    }
                }
            }
        }
        // ON CREATE / ON MATCH over each output row, evaluated against the row itself
        for (mut r, created) in outputs {
            let actions = if created {
                self.on_create
            } else {
                self.on_match
            };
            if let Some(actions) = actions
                && !actions.is_empty()
            {
                self.install_row(rs, &r, &mut lanes);
                for a in actions {
                    if let Some((p, v)) = apply_set(a, rs, &lanes, cx, &mut self.scratch)? {
                        r[p.col as usize] = v;
                    }
                }
            }
            st.push_row(|cols| {
                for (c, v) in r.iter().enumerate() {
                    cols[c].push_owned(v.clone());
                }
            })?;
        }
        Ok(())
    }
}

impl PhysOperator for MergeOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if let Some(r) = self.consumer.emit_or_pull(rs)? {
                return Ok(r);
            }
            if !self.input.next(rs, cx)? {
                self.consumer.done = true;
                continue;
            }
            self.width = rs.group(self.consumer.out_group).cols.len();
            let mut st = Staged::new(self.width);
            // the input rows' lanes first: the output group doubles as the row scratch
            let total = rs.row_count(self.consumer.in_groups);
            let mut all_lanes: Vec<Vec<usize>> = Vec::with_capacity(total);
            for row in 0..total {
                let mut lanes = Vec::new();
                rs.row_lanes(self.consumer.in_groups, row, &mut lanes);
                all_lanes.push(lanes);
            }
            for (i, lanes) in all_lanes.iter().enumerate() {
                self.merge_row(rs, lanes, cx, &mut st)?;
                if i & 0xFF == 0xFF {
                    cx.check()?;
                }
            }
            cx.tick()?;
            self.consumer.staged = Some(st);
        }
    }

    fn reset(&mut self) {
        self.consumer.reset();
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}
