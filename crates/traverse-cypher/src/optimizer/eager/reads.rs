//! The reads and writes of expressions, patterns and clauses.

use rustc_hash::FxHashSet;

use crate::ast::{
    Clause, Expr, MatchClause, NodePattern, Pattern, ProjectionItem, ProjectionItems, RelPattern,
    RemoveItem, SetClause, SetItem, SingleQuery,
};

use super::analyze::bind_projection;
use super::{Kind, PropKind, Reads, VarKinds, Writes};

/// Functions that never return a node or relationship, so an alias bound to one is a plain value.
fn is_scalar_function(name: &str) -> bool {
    matches!(
        name,
        "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "stdev"
            | "stdevp"
            | "percentilecont"
            | "percentiledisc"
            | "tostring"
            | "tointeger"
            | "tofloat"
            | "toboolean"
            | "tolower"
            | "toupper"
            | "trim"
            | "ltrim"
            | "rtrim"
            | "size"
            | "length"
            | "abs"
            | "round"
            | "floor"
            | "ceil"
            | "sqrt"
            | "sign"
            | "rand"
            | "randomuuid"
            | "timestamp"
            | "id"
            | "elementid"
            | "type"
            | "labels"
            | "keys"
            | "properties"
            | "split"
            | "replace"
            | "substring"
            | "left"
            | "right"
            | "reverse"
            | "range"
            | "coalesce"
            | "date"
            | "datetime"
            | "localdatetime"
            | "time"
            | "localtime"
            | "duration"
            | "tochar"
            | "char_length"
            | "character_length"
            | "exp"
            | "log"
            | "log10"
            | "sin"
            | "cos"
            | "tan"
            | "asin"
            | "acos"
            | "atan"
            | "atan2"
            | "pi"
            | "e"
            | "degrees"
            | "radians"
            | "haversin"
            | "isempty"
            | "tostringornull"
            | "tointegerornull"
            | "tofloatornull"
            | "tobooleanornull"
            | "point"
            | "distance"
            | "__has_label"
    )
}

/// What an expression evaluates to, for binding projection aliases.
pub(super) fn expr_kind(expr: &Expr, ctx: &VarKinds) -> Kind {
    match expr {
        Expr::Variable(v) => ctx.kind(v),
        Expr::Literal(_)
        | Expr::Parameter(_)
        | Expr::Property(..)
        | Expr::BinaryOp(..)
        | Expr::UnaryOp(..)
        | Expr::Comparison(..)
        | Expr::In(..)
        | Expr::IsNull(_)
        | Expr::IsNotNull(_)
        | Expr::IsTyped { .. }
        | Expr::StringOp(..)
        | Expr::MapLiteral(_)
        | Expr::MapProjection { .. }
        | Expr::CountSubquery(_)
        | Expr::ExistentialSubquery(_)
        | Expr::FullExistentialSubquery(_)
        | Expr::PatternPredicate(_)
        | Expr::QuantifierExpr { .. } => Kind::Value,
        Expr::FunctionCall { name, .. } if is_scalar_function(name) => Kind::Value,
        // lists may carry entities (`head(list)`, `collect(n)`, CASE over nodes, subscripts...)
        _ => Kind::Unknown,
    }
}

/// Graph reads an expression performs.
#[recursive::recursive]
pub(super) fn expr_reads(expr: &Expr, ctx: &VarKinds, out: &mut Reads) {
    match expr {
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Variable(_) => {}
        Expr::Property(base, key) => match base.as_ref() {
            Expr::Variable(v) => out.read_prop(key, &ctx.kind(v)),
            other => {
                expr_reads(other, ctx, out);
                out.read_prop(key, &expr_kind(other, ctx));
            }
        },
        Expr::BinaryOp(a, _, b)
        | Expr::In(a, b)
        | Expr::StringOp(a, _, b)
        | Expr::ListIndex(a, b) => {
            expr_reads(a, ctx, out);
            expr_reads(b, ctx, out);
        }
        Expr::UnaryOp(_, a) | Expr::IsNull(a) | Expr::IsNotNull(a) => expr_reads(a, ctx, out),
        Expr::IsTyped { expr, .. } => expr_reads(expr, ctx, out),
        Expr::Comparison(first, rest) => {
            expr_reads(first, ctx, out);
            for (_, e) in rest {
                expr_reads(e, ctx, out);
            }
        }
        Expr::FunctionCall { name, args, .. } => {
            for a in args {
                expr_reads(a, ctx, out);
            }
            match name.as_str() {
                "__has_label" => {
                    if let Some(Expr::Literal(crate::ast::Literal::String(label))) = args.get(1) {
                        out.node_labels.insert(label.clone());
                    }
                }
                "labels" => out.label_values = true,
                "properties" | "keys" => {
                    let kind = args
                        .first()
                        .map(|a| expr_kind(a, ctx))
                        .unwrap_or(Kind::Unknown);
                    out.read_all_props(&kind);
                }
                // `type(r)` reads an immutable attribute; `id()`, `startNode()`, `endNode()`,
                // `nodes()`, `relationships()` read structure only DELETE (a barrier) changes
                _ => {}
            }
        }
        Expr::ListLiteral(items) => {
            for e in items {
                expr_reads(e, ctx, out);
            }
        }
        Expr::MapLiteral(pairs) => {
            for (_, e) in pairs {
                expr_reads(e, ctx, out);
            }
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(o) = operand {
                expr_reads(o, ctx, out);
            }
            for (w, t) in whens {
                expr_reads(w, ctx, out);
                expr_reads(t, ctx, out);
            }
            if let Some(e) = else_expr {
                expr_reads(e, ctx, out);
            }
        }
        Expr::ListSlice(a, b, c) => {
            expr_reads(a, ctx, out);
            if let Some(b) = b {
                expr_reads(b, ctx, out);
            }
            if let Some(c) = c {
                expr_reads(c, ctx, out);
            }
        }
        Expr::ListComprehension {
            variable,
            list_expr,
            filter,
            map_expr,
        } => {
            expr_reads(list_expr, ctx, out);
            let mut inner = ctx.clone();
            inner.bind(variable, Kind::Unknown);
            if let Some(f) = filter {
                expr_reads(f, &inner, out);
            }
            if let Some(m) = map_expr {
                expr_reads(m, &inner, out);
            }
        }
        Expr::QuantifierExpr {
            variable,
            list_expr,
            predicate,
            ..
        } => {
            expr_reads(list_expr, ctx, out);
            let mut inner = ctx.clone();
            inner.bind(variable, Kind::Unknown);
            expr_reads(predicate, &inner, out);
        }
        Expr::Reduce {
            accumulator,
            initial,
            variable,
            list_expr,
            update_expr,
        } => {
            expr_reads(initial, ctx, out);
            expr_reads(list_expr, ctx, out);
            let mut inner = ctx.clone();
            inner.bind(accumulator, Kind::Unknown);
            inner.bind(variable, Kind::Unknown);
            expr_reads(update_expr, &inner, out);
        }
        Expr::MapProjection {
            expr,
            properties,
            all_properties,
            literal_entries,
        } => {
            expr_reads(expr, ctx, out);
            let kind = expr_kind(expr, ctx);
            for p in properties {
                out.read_prop(p, &kind);
            }
            if *all_properties {
                out.read_all_props(&kind);
            }
            for (_, e) in literal_entries {
                expr_reads(e, ctx, out);
            }
        }
        Expr::ExistentialSubquery(mc) | Expr::PatternPredicate(mc) | Expr::CountSubquery(mc) => {
            let mut inner = ctx.clone();
            match_clause_reads(mc, &mut inner, out);
        }
        Expr::PatternComprehension {
            match_clause,
            projection,
            ..
        } => {
            let mut inner = ctx.clone();
            match_clause_reads(match_clause, &mut inner, out);
            expr_reads(projection, &inner, out);
        }
        Expr::FullExistentialSubquery(q) | Expr::CollectSubquery(q) => {
            let mut inner = ctx.clone();
            single_query_reads(q, &mut inner, out);
        }
    }
}

/// Reads of matching a pattern (labels, relationship types, inline property maps, predicates);
/// binds its variables.
fn pattern_reads(pattern: &Pattern, ctx: &mut VarKinds, out: &mut Reads) {
    for part in &pattern.parts {
        let connected = !part.element.chain.is_empty();
        node_pattern_reads(&part.element.node, connected, ctx, out);
        for chain in &part.element.chain {
            rel_pattern_reads(&chain.rel, ctx, out);
            node_pattern_reads(&chain.node, true, ctx, out);
        }
    }
}

fn node_pattern_reads(np: &NodePattern, connected: bool, ctx: &mut VarKinds, out: &mut Reads) {
    let already_bound = np
        .variable
        .as_ref()
        .is_some_and(|v| matches!(ctx.kind(v), Kind::Node(_)));
    if !np.dynamic_labels.is_empty() {
        out.all_nodes = true;
    }
    for l in &np.labels {
        out.node_labels.insert(l.clone());
    }
    // an unlabeled, unconnected, unbound node is found by scanning every node
    if np.labels.is_empty() && !connected && !already_bound {
        out.all_nodes = true;
    }
    if let Some(v) = &np.variable {
        if !already_bound {
            ctx.bind(v, Kind::Node(np.labels.clone()));
        } else {
            for l in &np.labels {
                ctx.add_label(v, l);
            }
        }
    }
    let kind = np
        .variable
        .as_ref()
        .map(|v| ctx.kind(v))
        .unwrap_or_else(|| Kind::Node(np.labels.clone()));
    if let Some(Expr::MapLiteral(pairs)) = &np.properties {
        for (k, e) in pairs {
            out.read_prop(k, &kind);
            expr_reads(e, ctx, out);
        }
    } else if let Some(p) = &np.properties {
        out.read_all_props(&kind);
        expr_reads(p, ctx, out);
    }
    if let Some(w) = &np.where_pred {
        expr_reads(w, ctx, out);
    }
}

fn rel_pattern_reads(rp: &RelPattern, ctx: &mut VarKinds, out: &mut Reads) {
    if !rp.dynamic_types.is_empty() {
        out.all_rels = true;
    } else {
        out.read_rel_types(&rp.types);
    }
    let kind = Kind::Rel(rp.types.clone());
    if let Some(v) = &rp.variable {
        ctx.bind(v, kind.clone());
    }
    if let Some(Expr::MapLiteral(pairs)) = &rp.properties {
        for (k, e) in pairs {
            out.read_prop(k, &kind);
            expr_reads(e, ctx, out);
        }
    } else if let Some(p) = &rp.properties {
        out.read_all_props(&kind);
        expr_reads(p, ctx, out);
    }
    if let Some(w) = &rp.where_pred {
        expr_reads(w, ctx, out);
    }
    if let Some(g) = &rp.qpp_group {
        node_pattern_reads(&g.start, true, ctx, out);
        node_pattern_reads(&g.end, true, ctx, out);
    }
}

fn match_clause_reads(mc: &MatchClause, ctx: &mut VarKinds, out: &mut Reads) {
    pattern_reads(&mc.pattern, ctx, out);
    if let Some(w) = &mc.where_clause {
        expr_reads(w, ctx, out);
    }
    if mc.search.is_some() {
        out.everything = true;
    }
}

pub(super) fn projection_items_reads(items: &[ProjectionItem], ctx: &VarKinds, out: &mut Reads) {
    for item in items {
        expr_reads(&item.expr, ctx, out);
    }
}

/// Reads of a nested query (EXISTS { ... } / COLLECT { ... }); conservatively every clause's patterns
/// and expressions.
#[recursive::recursive]
fn single_query_reads(q: &SingleQuery, ctx: &mut VarKinds, out: &mut Reads) {
    for part in &q.parts {
        for c in &part.clauses {
            clause_reads(c, ctx, out);
        }
        if let ProjectionItems::Explicit(items) = &part.with.items {
            projection_items_reads(items, ctx, out);
            bind_projection(items, ctx);
        }
        if let Some(w) = &part.with.where_clause {
            expr_reads(w, ctx, out);
        }
    }
    for c in &q.final_part.clauses {
        clause_reads(c, ctx, out);
    }
    if let Some(r) = &q.final_part.return_clause
        && let ProjectionItems::Explicit(items) = &r.items
    {
        projection_items_reads(items, ctx, out);
    }
}

fn clause_reads(c: &Clause, ctx: &mut VarKinds, out: &mut Reads) {
    match c {
        Clause::Match(mc) | Clause::OptionalMatch(mc) => match_clause_reads(mc, ctx, out),
        Clause::Unwind(u) => {
            expr_reads(&u.expr, ctx, out);
            ctx.bind(&u.alias, Kind::Unknown);
        }
        Clause::LoadCsv(l) => {
            expr_reads(&l.url, ctx, out);
            ctx.bind(&l.row_alias, Kind::Value);
        }
        // validation rejects writes in nested read queries; procedure calls are opaque
        Clause::Call(_) | Clause::CallSubquery(_) => out.everything = true,
        Clause::Create(_)
        | Clause::Merge(_)
        | Clause::Delete(_)
        | Clause::Set(_)
        | Clause::Remove(_)
        | Clause::ForEach(_) => out.everything = true,
    }
}

pub(super) fn set_clause_effects(
    sc: &SetClause,
    ctx: &VarKinds,
    reads: &mut Reads,
    writes: &mut Writes,
) {
    for item in &sc.items {
        set_item_effects(item, ctx, reads, writes);
    }
}

pub(super) fn set_item_effects(
    item: &SetItem,
    ctx: &VarKinds,
    reads: &mut Reads,
    writes: &mut Writes,
) {
    match item {
        SetItem::Property(access, value) => {
            expr_reads(value, ctx, reads);
            let kind = PropKind::of(&ctx.kind(&access.expr_or_var)).unwrap_or(PropKind::Any);
            writes.write_prop(&access.key, kind, Vec::new());
        }
        SetItem::AllProperties(var, value) | SetItem::MergeProperties(var, value) => {
            expr_reads(value, ctx, reads);
            writes.write_all_props(PropKind::of(&ctx.kind(var)).unwrap_or(PropKind::Any));
        }
        SetItem::Label(_, labels, dynamic) => {
            writes.changed_labels.extend(labels.iter().cloned());
            if !dynamic.is_empty() {
                writes.changed_any_label = true;
                for e in dynamic {
                    expr_reads(e, ctx, reads);
                }
            }
        }
        SetItem::DynamicProperty {
            variable,
            key,
            value,
        } => {
            expr_reads(key, ctx, reads);
            expr_reads(value, ctx, reads);
            writes.write_all_props(PropKind::of(&ctx.kind(variable)).unwrap_or(PropKind::Any));
        }
    }
}

pub(super) fn remove_item_effects(
    item: &RemoveItem,
    ctx: &VarKinds,
    reads: &mut Reads,
    writes: &mut Writes,
) {
    match item {
        RemoveItem::Property(access) => {
            let kind = PropKind::of(&ctx.kind(&access.expr_or_var)).unwrap_or(PropKind::Any);
            writes.write_prop(&access.key, kind, Vec::new());
        }
        RemoveItem::Label(_, labels, dynamic) => {
            writes.changed_labels.extend(labels.iter().cloned());
            if !dynamic.is_empty() {
                writes.changed_any_label = true;
                for e in dynamic {
                    expr_reads(e, ctx, reads);
                }
            }
        }
    }
}

/// MERGE effects: reads what it matches, writes what it may create.
pub(super) fn merge_pattern_effects(
    pattern: &Pattern,
    ctx: &mut VarKinds,
    reads: &mut Reads,
    writes: &mut Writes,
) {
    // node variables bound before MERGE are matched, never created; decide before `pattern_reads`
    // binds the pattern's variables
    let prebound: FxHashSet<String> = pattern
        .parts
        .iter()
        .flat_map(|part| {
            std::iter::once(&part.element.node).chain(part.element.chain.iter().map(|c| &c.node))
        })
        .filter_map(|np| np.variable.clone())
        .filter(|v| matches!(ctx.kind(v), Kind::Node(_)))
        .collect();
    // reads: same as matching the pattern
    pattern_reads(pattern, ctx, reads);
    // writes: each unbound node may be created with exactly its labels and inline properties, each
    // relationship with its type
    for part in &pattern.parts {
        merge_node_writes(&part.element.node, &prebound, writes);
        for chain in &part.element.chain {
            let rel = &chain.rel;
            if !rel.dynamic_types.is_empty() {
                writes.created_any_rel_type = true;
            } else {
                writes.created_rel_types.extend(rel.types.iter().cloned());
            }
            match &rel.properties {
                Some(Expr::MapLiteral(pairs)) => {
                    for (k, _) in pairs {
                        writes.write_prop(k, PropKind::Rel, Vec::new());
                    }
                }
                Some(_) => writes.write_all_props(PropKind::Rel),
                None => {}
            }
            merge_node_writes(&chain.node, &prebound, writes);
        }
    }
}

fn merge_node_writes(np: &NodePattern, prebound: &FxHashSet<String>, writes: &mut Writes) {
    if np.variable.as_ref().is_some_and(|v| prebound.contains(v)) {
        return;
    }
    if !np.dynamic_labels.is_empty() {
        writes.created_any_label = true;
    } else {
        writes.created_nodes.push(np.labels.clone());
    }
    match &np.properties {
        Some(Expr::MapLiteral(pairs)) => {
            for (k, _) in pairs {
                writes.write_prop(k, PropKind::Node, np.labels.clone());
            }
        }
        Some(_) => writes.write_all_props(PropKind::Node),
        None => {}
    }
}
