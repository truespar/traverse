//! Expression evaluation.

use std::collections::BTreeMap;

use traverse_core::tx::view::GraphView;

use crate::ast::{BinOp, Expr, Literal, QuantifierKind, StringOp, UnaryOp};
use crate::bytecode;
use crate::error::CypherError;
use crate::frame::{self, Frame, SymbolTable};
use crate::functions;
use crate::temporal;
use crate::types::CypherValue;

use super::compare::{cypher_eq, eval_comparison};
use super::comprehension::{
    eval_pattern_comprehension, pattern_comprehension_refs, try_slot_native_comprehension,
};
use super::context::{get_query_param, regex_full_match};
use super::subquery::{
    eval_full_subquery_collect, eval_full_subquery_exists, eval_subquery_count,
    eval_subquery_exists, try_chain_pattern_exists,
};
use super::values::{edge_to_cypher_value, node_to_cypher_value, resolve_ref};
use super::{Record, VIRTUAL_ID_BASE};

/// Evaluates an expression against a frame.
pub fn eval_expr(
    expr: &Expr,
    frame: &Frame,
    graph: &dyn GraphView,
    symtab: &SymbolTable,
) -> Result<CypherValue, CypherError> {
    match expr {
        Expr::Literal(lit) => Ok(match lit {
            Literal::Null => CypherValue::Null,
            Literal::Bool(b) => CypherValue::Bool(*b),
            Literal::Integer(i) => CypherValue::Integer(*i),
            Literal::Float(f) => CypherValue::Float(*f),
            Literal::String(s) => CypherValue::String(s.clone()),
        }),

        Expr::Variable(name) => Ok(match symtab.slot(name) {
            Some(s) => frame[s].to_cypher_value(),
            None => CypherValue::Null,
        }),

        Expr::Parameter(name) => get_query_param(name).ok_or_else(|| {
            // Neo4j: ParameterMissing, not NULL; a mistyped parameter must not silently change
            // results
            CypherError::semantic(format!("Expected parameter(s): {name}"))
                .with_status(crate::status::GqlStatus::InvalidParameter)
        }),

        Expr::Property(base_expr, key) => {
            let base = eval_expr(base_expr, frame, graph, symtab)?;
            match &base {
                CypherValue::NodeRef(id) => match graph.get_node_property(*id, key) {
                    Ok(Some(pv)) => Ok(CypherValue::from_property(&pv)),
                    Ok(None) => Ok(CypherValue::Null),
                    Err(_) => Err(CypherError::semantic("deleted entity access".to_string())),
                },
                CypherValue::EdgeRef { id, .. } => match graph.get_edge_property(*id, key) {
                    Ok(Some(pv)) => Ok(CypherValue::from_property(&pv)),
                    Ok(None) => Ok(CypherValue::Null),
                    Err(_) => Err(CypherError::semantic("deleted entity access".to_string())),
                },
                // virtual entities (APOC vNode/vRelationship, ids in the top range) are pure
                // values: read their embedded map
                CypherValue::Node { id, .. }
                    if id.0 < VIRTUAL_ID_BASE && graph.get_node(*id).is_err() =>
                {
                    Err(CypherError::semantic("deleted entity access".to_string()))
                }
                CypherValue::Edge { id, .. }
                    if id.0 < VIRTUAL_ID_BASE && graph.get_edge(*id).is_err() =>
                {
                    Err(CypherError::semantic("deleted entity access".to_string()))
                }
                _ => get_property(&base, key),
            }
        }

        Expr::BinaryOp(left, op, right) => {
            let lval = eval_expr(left, frame, graph, symtab)?;
            // three-valued AND/OR
            match op {
                BinOp::And => {
                    // false AND x -> false (short-circuit)
                    if matches!(&lval, CypherValue::Bool(false)) {
                        return Ok(CypherValue::Bool(false));
                    }
                    let rval = eval_expr(right, frame, graph, symtab)?;
                    // x AND false -> false
                    if matches!(&rval, CypherValue::Bool(false)) {
                        return Ok(CypherValue::Bool(false));
                    }
                    // null AND non-false -> null
                    if lval.is_null() || rval.is_null() {
                        return Ok(CypherValue::Null);
                    }
                    return Ok(CypherValue::Bool(is_truthy(&lval) && is_truthy(&rval)));
                }
                BinOp::Or => {
                    // true OR x -> true (short-circuit)
                    if is_truthy(&lval) {
                        return Ok(CypherValue::Bool(true));
                    }
                    let rval = eval_expr(right, frame, graph, symtab)?;
                    // x OR true -> true
                    if is_truthy(&rval) {
                        return Ok(CypherValue::Bool(true));
                    }
                    // null OR non-true -> null
                    if lval.is_null() || rval.is_null() {
                        return Ok(CypherValue::Null);
                    }
                    return Ok(CypherValue::Bool(false));
                }
                _ => {}
            }
            let rval = eval_expr(right, frame, graph, symtab)?;
            eval_binary_op(&lval, *op, &rval)
        }

        Expr::UnaryOp(op, inner) => {
            let val = eval_expr(inner, frame, graph, symtab)?;
            match op {
                UnaryOp::Not => {
                    if val.is_null() {
                        Ok(CypherValue::Null)
                    } else {
                        Ok(CypherValue::Bool(!is_truthy(&val)))
                    }
                }
                UnaryOp::Neg => match val {
                    CypherValue::Integer(i) => {
                        i.checked_neg().map(CypherValue::Integer).ok_or_else(|| {
                            CypherError::type_error("long overflow".to_string())
                                .with_status(crate::status::GqlStatus::NumericOutOfRange)
                        })
                    }
                    CypherValue::Float(f) => Ok(CypherValue::Float(-f)),
                    CypherValue::Null => Ok(CypherValue::Null),
                    _ => Err(CypherError::type_error(format!(
                        "cannot negate {}",
                        val.type_name()
                    ))),
                },
            }
        }

        Expr::Comparison(left, cmps) => {
            let mut current = eval_expr(left, frame, graph, symtab)?;
            for (op, right_expr) in cmps {
                let right = eval_expr(right_expr, frame, graph, symtab)?;
                let result = eval_comparison(&current, *op, &right);
                if result.is_null() {
                    return Ok(CypherValue::Null);
                }
                if !is_truthy(&result) {
                    return Ok(CypherValue::Bool(false));
                }
                current = right;
            }
            Ok(CypherValue::Bool(true))
        }

        Expr::FunctionCall {
            name,
            distinct: _,
            args,
        } => {
            // Handle special internal functions
            if name == "__has_label" {
                return eval_has_label(&args[0], &args[1], frame, graph, symtab);
            }
            // composite-index seek key: encode components EXACTLY like the index side
            // (IndexKey::composite_encode) so the b-tree lookup matches
            if name == "__composite_key" {
                use traverse_core::graph::property::IndexKey;
                let mut parts = Vec::with_capacity(args.len());
                for a in args {
                    let v = eval_expr(a, frame, graph, symtab)?;
                    let ik = match v {
                        CypherValue::Bool(b) => IndexKey::Bool(b),
                        CypherValue::Integer(i) => IndexKey::Int(i),
                        CypherValue::Float(f) => IndexKey::Float(f),
                        CypherValue::String(s) => IndexKey::String(s),
                        // null / non-scalar component: no tuple can match
                        _ => return Ok(CypherValue::Null),
                    };
                    parts.push(ik);
                }
                return Ok(match IndexKey::composite_encode(&parts) {
                    IndexKey::String(s) => CypherValue::String(s),
                    _ => CypherValue::Null,
                });
            }
            // LOAD CSV per-row helpers: read the hidden context slot LoadCsvOp sets, index
            // into the thread-local stack, return the attribute. Outside LOAD CSV the slot
            // is absent -> NULL, like Neo4j.
            if name == "file" || name == "linenumber" {
                if !args.is_empty() {
                    return Err(CypherError::type_error(format!(
                        "{name}() expected 0 arguments, got {}",
                        args.len()
                    )));
                }
                let idx_opt = symtab
                    .slot(crate::operators::load_csv::CSV_CTX_SLOT)
                    .and_then(|slot| match &frame[slot] {
                        crate::frame::SlotValue::Integer(i) if *i >= 0 => Some(*i as usize),
                        _ => None,
                    });
                let ctx = idx_opt.and_then(crate::operators::load_csv::get_csv_context);
                return Ok(match (name.as_str(), ctx) {
                    ("file", Some(c)) => CypherValue::String(c.file),
                    ("linenumber", Some(c)) => CypherValue::Integer(c.line),
                    _ => CypherValue::Null,
                });
            }
            let arg_vals: Vec<CypherValue> = args
                .iter()
                .map(|a| eval_expr(a, frame, graph, symtab))
                .collect::<Result<_, _>>()?;
            // APOC compatibility layer (apoc.coll/text/map/convert/date/...)
            if name.starts_with("apoc.") {
                if let Some(r) = crate::apoc::eval(name, &arg_vals) {
                    return r;
                }
                if let Some(r) = crate::apoc::eval_graph(name, &arg_vals, graph) {
                    return r;
                }
                return Err(CypherError::semantic(format!(
                    "Unknown function: {name} (not in Traverse's APOC compatibility subset)"
                ))
                .with_status(crate::status::GqlStatus::UnknownFunction));
            }
            // GDS compatibility functions (gds.util.*, gds.similarity.*)
            if name.starts_with("gds.") {
                if let Some(r) = crate::gds_catalog::eval_fn(name, &arg_vals, graph) {
                    return r;
                }
                return Err(CypherError::semantic(format!(
                    "Unknown function: {name} (not in Traverse's GDS compatibility subset)"
                ))
                .with_status(crate::status::GqlStatus::UnknownFunction));
            }
            // startNode/endNode: NodeRef, resolved lazily
            if name == "startnode" || name == "endnode" {
                match arg_vals.first() {
                    Some(CypherValue::Edge { source, target, .. })
                    | Some(CypherValue::EdgeRef { source, target, .. }) => {
                        let nid = if name == "startnode" {
                            *source
                        } else {
                            *target
                        };
                        return Ok(CypherValue::NodeRef(nid));
                    }
                    _ => {}
                }
            }
            // id() on NodeRef/EdgeRef without full resolution
            if name == "id" {
                match arg_vals.first() {
                    Some(CypherValue::NodeRef(id)) => {
                        return Ok(CypherValue::Integer(id.0 as i64));
                    }
                    Some(CypherValue::EdgeRef { id, .. }) => {
                        return Ok(CypherValue::Integer(id.0 as i64));
                    }
                    _ => {}
                }
            }
            // elementId() on NodeRef/EdgeRef: opaque String from the internal id, no
            // materialization
            if name == "elementid" {
                match arg_vals.first() {
                    Some(CypherValue::NodeRef(id)) => {
                        return Ok(CypherValue::String(id.0.to_string()));
                    }
                    Some(CypherValue::EdgeRef { id, .. }) => {
                        return Ok(CypherValue::String(id.0.to_string()));
                    }
                    _ => {}
                }
            }
            // type() on EdgeRef: type name from the interned ID, no full resolution
            if name == "type"
                && let Some(CypherValue::EdgeRef { edge_type_id, .. }) = arg_vals.first()
            {
                let type_name = graph.strings().resolve(edge_type_id.0).to_string();
                return Ok(CypherValue::String(type_name));
            }
            // deleted-entity check for labels()
            if name == "labels"
                && let Some(val) = arg_vals.first()
            {
                match val {
                    // virtual entities read their embedded data
                    CypherValue::Node { id, .. } | CypherValue::NodeRef(id)
                        if id.0 < VIRTUAL_ID_BASE && graph.get_node(*id).is_err() =>
                    {
                        return Err(CypherError::semantic("deleted entity access".to_string()));
                    }
                    CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. }
                        if id.0 < VIRTUAL_ID_BASE && graph.get_edge(*id).is_err() =>
                    {
                        return Err(CypherError::semantic("deleted entity access".to_string()));
                    }
                    _ => {}
                }
            }
            // resolve refs first; call_function has no graph access
            let resolved_args: Vec<CypherValue> = arg_vals
                .into_iter()
                .map(|v| resolve_ref(v, graph))
                .collect();
            functions::call_function(name, &resolved_args)
        }

        Expr::ListLiteral(items) => {
            let vals: Vec<CypherValue> = items
                .iter()
                .map(|e| eval_expr(e, frame, graph, symtab))
                .collect::<Result<_, _>>()?;
            Ok(CypherValue::List(vals))
        }

        Expr::MapLiteral(entries) => {
            let mut map = BTreeMap::new();
            for (key, val_expr) in entries {
                let val = eval_expr(val_expr, frame, graph, symtab)?;
                map.insert(key.clone(), val);
            }
            Ok(CypherValue::Map(map))
        }

        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(op) = operand {
                // simple CASE uses Cypher `=` (three-valued): a null operand or comparand NEVER
                // matches, even `WHEN null` falls to ELSE (Neo4j/GQL). Use `CASE WHEN x IS NULL` to
                // test for null.
                let op_val = eval_expr(op, frame, graph, symtab)?;
                for (when_expr, then_expr) in whens {
                    let when_val = eval_expr(when_expr, frame, graph, symtab)?;
                    if matches!(cypher_eq(&op_val, &when_val), CypherValue::Bool(true)) {
                        return eval_expr(then_expr, frame, graph, symtab);
                    }
                }
            } else {
                // searched CASE
                for (when_expr, then_expr) in whens {
                    let when_val = eval_expr(when_expr, frame, graph, symtab)?;
                    if is_truthy(&when_val) {
                        return eval_expr(then_expr, frame, graph, symtab);
                    }
                }
            }
            if let Some(else_e) = else_expr {
                eval_expr(else_e, frame, graph, symtab)
            } else {
                Ok(CypherValue::Null)
            }
        }

        Expr::In(left, right) => {
            let lval = eval_expr(left, frame, graph, symtab)?;
            let rval = eval_expr(right, frame, graph, symtab)?;
            match rval {
                CypherValue::List(items) => {
                    if items.is_empty() {
                        return Ok(CypherValue::Bool(false));
                    }
                    if lval.is_null() {
                        return Ok(CypherValue::Null);
                    }
                    let mut has_null = false;
                    for item in &items {
                        match cypher_eq(&lval, item) {
                            CypherValue::Bool(true) => return Ok(CypherValue::Bool(true)),
                            CypherValue::Null => has_null = true,
                            _ => {}
                        }
                    }
                    if has_null {
                        Ok(CypherValue::Null)
                    } else {
                        Ok(CypherValue::Bool(false))
                    }
                }
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "IN requires a list on the right side, got {}",
                    rval.type_name()
                ))),
            }
        }

        Expr::IsNull(inner) => {
            let val = eval_expr(inner, frame, graph, symtab)?;
            Ok(CypherValue::Bool(val.is_null()))
        }

        Expr::IsNotNull(inner) => {
            let val = eval_expr(inner, frame, graph, symtab)?;
            Ok(CypherValue::Bool(!val.is_null()))
        }

        Expr::IsTyped {
            expr,
            type_name,
            negated,
            not_null,
        } => {
            let val = eval_expr(expr, frame, graph, symtab)?;
            let matches = type_predicate_matches(&val, type_name, *not_null);
            Ok(CypherValue::Bool(matches != *negated))
        }

        Expr::StringOp(left, op, right) => {
            let lval = eval_expr(left, frame, graph, symtab)?;
            let rval = eval_expr(right, frame, graph, symtab)?;
            match (&lval, &rval) {
                (CypherValue::String(a), CypherValue::String(b)) => {
                    let result = match op {
                        StringOp::StartsWith => a.starts_with(b.as_str()),
                        StringOp::EndsWith => a.ends_with(b.as_str()),
                        StringOp::Contains => a.contains(b.as_str()),
                        StringOp::Regex => regex_full_match(b, a)?,
                    };
                    Ok(CypherValue::Bool(result))
                }
                _ => Ok(CypherValue::Null),
            }
        }

        Expr::ListIndex(list_expr, idx_expr) => {
            let list = eval_expr(list_expr, frame, graph, symtab)?;
            let idx = eval_expr(idx_expr, frame, graph, symtab)?;
            match (&list, &idx) {
                (CypherValue::List(items), CypherValue::Integer(i)) => {
                    let index = if *i < 0 { items.len() as i64 + *i } else { *i };
                    Ok(items
                        .get(index as usize)
                        .cloned()
                        .unwrap_or(CypherValue::Null))
                }
                // list with a string index: type error
                (CypherValue::List(_), CypherValue::String(_)) => Err(CypherError::type_error(
                    "list indexing requires an integer".to_string(),
                )),
                // bracket property access: node['key'], relationship['key'], map['key']
                (CypherValue::NodeRef(id), CypherValue::String(key)) => {
                    match graph.get_node_property(*id, key) {
                        Ok(Some(pv)) => Ok(CypherValue::from_property(&pv)),
                        Ok(None) => Ok(CypherValue::Null),
                        Err(_) => Err(CypherError::semantic("deleted entity access".to_string())),
                    }
                }
                (CypherValue::EdgeRef { id, .. }, CypherValue::String(key)) => {
                    match graph.get_edge_property(*id, key) {
                        Ok(Some(pv)) => Ok(CypherValue::from_property(&pv)),
                        Ok(None) => Ok(CypherValue::Null),
                        Err(_) => Err(CypherError::semantic("deleted entity access".to_string())),
                    }
                }
                (_, CypherValue::String(key)) => get_property(&list, key),
                (CypherValue::Null, _) | (_, CypherValue::Null) => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "list indexing type error".to_string(),
                )),
            }
        }

        Expr::ListSlice(list_expr, start_expr, end_expr) => {
            let list = eval_expr(list_expr, frame, graph, symtab)?;
            match list {
                CypherValue::List(items) => {
                    let len = items.len() as i64;
                    let start = if let Some(s) = start_expr {
                        match eval_expr(s, frame, graph, symtab)? {
                            CypherValue::Integer(i) => {
                                if i < 0 {
                                    (len + i).max(0) as usize
                                } else {
                                    i as usize
                                }
                            }
                            CypherValue::Null => return Ok(CypherValue::Null),
                            _ => 0,
                        }
                    } else {
                        0
                    };
                    let end = if let Some(e) = end_expr {
                        match eval_expr(e, frame, graph, symtab)? {
                            CypherValue::Integer(i) => {
                                if i < 0 {
                                    (len + i).max(0) as usize
                                } else {
                                    i.min(len) as usize
                                }
                            }
                            CypherValue::Null => return Ok(CypherValue::Null),
                            _ => items.len(),
                        }
                    } else {
                        items.len()
                    };
                    let end = end.min(items.len());
                    let start = start.min(end);
                    Ok(CypherValue::List(items[start..end].to_vec()))
                }
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error("slice requires a list".to_string())),
            }
        }

        Expr::ListComprehension {
            variable,
            list_expr,
            filter,
            map_expr,
        } => {
            let list = eval_expr(list_expr, frame, graph, symtab)?;
            match list {
                CypherValue::List(items) => {
                    let mut result = Vec::new();
                    let var_slot = symtab.slot(variable).unwrap();
                    // try compiling the filter to bytecode
                    let compiled_filter = filter
                        .as_ref()
                        .and_then(|f| bytecode::try_compile(f, symtab));
                    // clone the frame once and mutate the variable slot in place (avoids O(n^2) with
                    // big collected lists)
                    let mut inner = frame.clone();
                    for item in items {
                        inner[var_slot] = item.into();
                        if let Some(f) = filter {
                            let pass = if let Some(ref bc) = compiled_filter {
                                bytecode::eval_bytecode(bc, &inner, graph)?
                            } else {
                                let fval = eval_expr(f, &inner, graph, symtab)?;
                                is_truthy(&fval)
                            };
                            if !pass {
                                continue;
                            }
                        }
                        let value = if let Some(m) = map_expr {
                            eval_expr(m, &inner, graph, symtab)?
                        } else {
                            inner[var_slot].to_cypher_value()
                        };
                        result.push(value);
                    }
                    Ok(CypherValue::List(result))
                }
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "list comprehension requires a list, got {}",
                    list.type_name()
                ))),
            }
        }

        Expr::ExistentialSubquery(match_clause) | Expr::PatternPredicate(match_clause) => {
            // fast path: simple chain patterns get a frame-native DFS with early exit; the generic
            // path materializes EVERY match through the Record interpreter just to test emptiness
            if let Some(ans) = try_chain_pattern_exists(match_clause, frame, symtab, graph)? {
                return Ok(CypherValue::Bool(ans));
            }
            let rec = frame::frame_to_record(frame, symtab);
            eval_subquery_exists(match_clause, &rec, graph)
        }

        Expr::FullExistentialSubquery(subquery) => {
            let rec = frame::frame_to_record(frame, symtab);
            eval_full_subquery_exists(subquery, &rec, graph)
        }

        Expr::CountSubquery(match_clause) => {
            let rec = frame::frame_to_record(frame, symtab);
            eval_subquery_count(match_clause, &rec, graph)
        }

        Expr::CollectSubquery(subquery) => {
            let rec = frame::frame_to_record(frame, symtab);
            eval_full_subquery_collect(subquery, &rec, graph)
        }

        Expr::PatternComprehension {
            variable,
            match_clause,
            projection,
        } => {
            // slot-native fast path: run the comprehension through the operator tree with the outer
            // row seeded via ContextSeed, no Record interpreter
            if let Some(v) = try_slot_native_comprehension(
                variable.as_deref(),
                match_clause,
                projection,
                frame,
                symtab,
                graph,
            )? {
                return Ok(v);
            }
            // fallback (bound pattern vars / unsupported shapes): convert only the slots the
            // comprehension uses; a full frame_to_record clones every heap value (paths,
            // collected lists) per evaluation. Values left out can't enter bound_edge_ids,
            // so they need no uniqueness exemption.
            let refs = pattern_comprehension_refs(match_clause, projection);
            let rec = frame::frame_to_record_filtered(frame, symtab, &refs);
            eval_pattern_comprehension(variable.as_deref(), match_clause, projection, &rec, graph)
        }

        Expr::QuantifierExpr {
            kind,
            variable,
            list_expr,
            predicate,
        } => {
            let list = eval_expr(list_expr, frame, graph, symtab)?;
            let items = match list {
                CypherValue::List(l) => l,
                CypherValue::Null => return Ok(CypherValue::Null),
                _ => {
                    return Err(CypherError::type_error(format!(
                        "quantifier expects a list, got {}",
                        list.type_name()
                    )));
                }
            };
            let mut true_count = 0usize;
            let mut has_null = false;
            let mut has_false = false;
            let var_slot = symtab.slot(variable).unwrap();
            // try compiling the predicate to bytecode
            let compiled_pred = bytecode::try_compile(predicate, symtab);
            // clone once, mutate the variable slot in place (avoids O(n^2))
            let mut inner = frame.clone();
            for item in &items {
                inner[var_slot] = item.clone().into();
                if let Some(ref bc) = compiled_pred {
                    // bytecode fast path, no tree-walking eval_expr per item
                    match bytecode::eval_bytecode_3val(bc, &inner, graph)? {
                        None => has_null = true,
                        Some(true) => {
                            true_count += 1;
                            if matches!(kind, QuantifierKind::Any | QuantifierKind::None) {
                                break;
                            }
                            if matches!(kind, QuantifierKind::Single) && true_count > 1 {
                                break;
                            }
                        }
                        Some(false) => {
                            has_false = true;
                            if matches!(kind, QuantifierKind::All) {
                                break;
                            }
                        }
                    }
                } else {
                    let val = eval_expr(predicate, &inner, graph, symtab)?;
                    match val {
                        CypherValue::Null => has_null = true,
                        _ if is_truthy(&val) => {
                            true_count += 1;
                            if matches!(kind, QuantifierKind::Any | QuantifierKind::None) {
                                break;
                            }
                            if matches!(kind, QuantifierKind::Single) && true_count > 1 {
                                break;
                            }
                        }
                        _ => {
                            has_false = true;
                            if matches!(kind, QuantifierKind::All) {
                                break;
                            }
                        }
                    }
                }
            }
            // three-valued quantifiers:
            // ALL: true if all true; null if any null and no false; false if any false
            // ANY: true if any true; null if any null and no true; false if all false
            // NONE: true if no true and no null; null if any null and no true; false if any true
            // SINGLE: true if exactly one true and no null; null if any null; false if 0 or >1 true
            match kind {
                QuantifierKind::All => {
                    if has_false {
                        Ok(CypherValue::Bool(false))
                    } else if has_null {
                        Ok(CypherValue::Null)
                    } else {
                        Ok(CypherValue::Bool(true))
                    }
                }
                QuantifierKind::Any => {
                    if true_count > 0 {
                        Ok(CypherValue::Bool(true))
                    } else if has_null {
                        Ok(CypherValue::Null)
                    } else {
                        Ok(CypherValue::Bool(false))
                    }
                }
                QuantifierKind::None => {
                    if true_count > 0 {
                        Ok(CypherValue::Bool(false))
                    } else if has_null {
                        Ok(CypherValue::Null)
                    } else {
                        Ok(CypherValue::Bool(true))
                    }
                }
                QuantifierKind::Single => {
                    if true_count > 1 {
                        Ok(CypherValue::Bool(false))
                    } else if has_null {
                        Ok(CypherValue::Null)
                    } else {
                        Ok(CypherValue::Bool(true_count == 1))
                    }
                }
            }
        }

        Expr::Reduce {
            accumulator,
            initial,
            variable,
            list_expr,
            update_expr,
        } => {
            let list = eval_expr(list_expr, frame, graph, symtab)?;
            match list {
                CypherValue::List(items) => {
                    // fast path for numeric accumulation (skips ~6 eval_expr calls per item)
                    let init_val = eval_expr(initial, frame, graph, symtab)?;
                    if let Some(result) = try_reduce_numeric_fast(
                        accumulator,
                        &init_val,
                        variable,
                        update_expr,
                        &items,
                    ) {
                        return Ok(result);
                    }
                    // general path
                    let acc_slot = symtab.slot(accumulator).unwrap();
                    let var_slot = symtab.slot(variable).unwrap();
                    let mut inner = frame.clone();
                    inner[acc_slot] = init_val.into();
                    for item in items {
                        inner[var_slot] = item.into();
                        let new_acc = eval_expr(update_expr, &inner, graph, symtab)?;
                        inner[acc_slot] = new_acc.into();
                    }
                    Ok(inner[acc_slot].to_cypher_value())
                }
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "reduce requires a list, got {}",
                    list.type_name()
                ))),
            }
        }

        Expr::MapProjection {
            expr: base_expr,
            properties,
            all_properties,
            literal_entries,
        } => {
            let base = eval_expr(base_expr, frame, graph, symtab)?;
            let mut map = BTreeMap::new();
            // `.*`: every property of the base entity/map first; explicit `.prop` entries below may
            // override
            if *all_properties {
                let materialized = match &base {
                    CypherValue::NodeRef(id) => graph
                        .get_node(*id)
                        .ok()
                        .map(|n| node_to_cypher_value(&n, graph)),
                    CypherValue::EdgeRef { id, .. } => graph
                        .get_edge(*id)
                        .ok()
                        .map(|e| edge_to_cypher_value(&e, graph)),
                    other => Some(other.clone()),
                };
                match materialized {
                    Some(CypherValue::Node {
                        properties: props, ..
                    })
                    | Some(CypherValue::Edge {
                        properties: props, ..
                    }) => {
                        for (k, pv) in props.iter() {
                            map.insert(k.clone(), CypherValue::from_property(pv));
                        }
                    }
                    Some(CypherValue::Map(m)) => {
                        for (k, v) in m.iter() {
                            map.insert(k.clone(), v.clone());
                        }
                    }
                    _ => {}
                }
            }
            for key in properties {
                let val = match &base {
                    CypherValue::NodeRef(id) => graph
                        .get_node_property(*id, key)
                        .ok()
                        .flatten()
                        .map(|pv| CypherValue::from_property(&pv))
                        .unwrap_or(CypherValue::Null),
                    CypherValue::EdgeRef { id, .. } => graph
                        .get_edge_property(*id, key)
                        .ok()
                        .flatten()
                        .map(|pv| CypherValue::from_property(&pv))
                        .unwrap_or(CypherValue::Null),
                    CypherValue::Node {
                        properties: props, ..
                    } => props
                        .get(key.as_str())
                        .map(CypherValue::from_property)
                        .unwrap_or(CypherValue::Null),
                    CypherValue::Edge {
                        properties: props, ..
                    } => props
                        .get(key.as_str())
                        .map(CypherValue::from_property)
                        .unwrap_or(CypherValue::Null),
                    CypherValue::Map(m) => m.get(key).cloned().unwrap_or(CypherValue::Null),
                    CypherValue::Null => CypherValue::Null,
                    _ => CypherValue::Null,
                };
                map.insert(key.clone(), val);
            }
            // literal entries (`key: expr` / bare variable selectors) apply LAST, overriding `.*`
            // and `.prop` of the same key
            for (key, entry) in literal_entries {
                let val = eval_expr(entry, frame, graph, symtab)?;
                map.insert(key.clone(), val);
            }
            Ok(CypherValue::Map(map))
        }
    }
}

/// `IS :: TYPE` with Neo4j semantics: null matches any type unless `NOT NULL`; non-null must match
/// structurally; `LIST<T>` checks elements.
fn type_predicate_matches(v: &CypherValue, type_name: &str, not_null: bool) -> bool {
    if matches!(v, CypherValue::Null) {
        return !not_null;
    }
    let norm: String = type_name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase();
    let (base, inner) = match norm.split_once('<') {
        Some((b, rest)) => (b.trim(), Some(rest.trim_end_matches('>').trim())),
        None => (norm.as_str(), None),
    };
    let actual = crate::functions::value_base_type(v);
    match base {
        "ANY" => true,
        "NOTHING" | "NULL" => false, // a non-null value is neither
        "LIST" | "ARRAY" => {
            let CypherValue::List(items) = v else {
                return false;
            };
            match inner {
                Some(inner_ty) if !inner_ty.is_empty() => {
                    let inner_ty = inner_ty.trim_end_matches("NOT NULL").trim();
                    items
                        .iter()
                        .all(|e| type_predicate_matches(e, inner_ty, false))
                }
                _ => true,
            }
        }
        "BOOLEAN" | "BOOL" => actual == "BOOLEAN",
        "INTEGER" | "INT" | "SIGNED INTEGER" => actual == "INTEGER",
        "FLOAT" => actual == "FLOAT",
        "STRING" | "VARCHAR" => actual == "STRING",
        "POINT" => actual == "POINT",
        "MAP" => actual == "MAP",
        "NODE" | "VERTEX" => actual == "NODE",
        "RELATIONSHIP" | "EDGE" => actual == "RELATIONSHIP",
        "PATH" => actual == "PATH",
        "DATE" => actual == "DATE",
        "LOCAL TIME" => actual == "LOCAL TIME",
        "ZONED TIME" | "TIME" => actual == "ZONED TIME",
        "LOCAL DATETIME" => actual == "LOCAL DATETIME",
        "ZONED DATETIME" | "DATETIME" => actual == "ZONED DATETIME",
        "DURATION" => actual == "DURATION",
        _ => false, // unknown type name
    }
}

fn eval_has_label(
    var_expr: &Expr,
    label_expr: &Expr,
    frame: &Frame,
    graph: &dyn GraphView,
    symtab: &SymbolTable,
) -> Result<CypherValue, CypherError> {
    let node_val = eval_expr(var_expr, frame, graph, symtab)?;
    // label as a String: the static planner path gives `Expr::Literal(String(...))`, dynamic labels
    // (`MATCH (n:$param)` / `(n:$($expr))`) any expression, evaluated per call. NULL -> false (no
    // node has a NULL label)
    let label = match label_expr {
        Expr::Literal(Literal::String(s)) => s.clone(),
        other => match eval_expr(other, frame, graph, symtab)? {
            CypherValue::String(s) => s,
            CypherValue::Null => return Ok(CypherValue::Bool(false)),
            v => {
                return Err(CypherError::type_error(format!(
                    "dynamic label expression must evaluate to STRING, got {}",
                    v.type_name()
                )));
            }
        },
    };
    match node_val {
        CypherValue::Node { labels, .. } => {
            Ok(CypherValue::Bool(labels.iter().any(|l| l == &label)))
        }
        CypherValue::NodeRef(id) => {
            if let Ok(node) = graph.get_node(id) {
                let has = node
                    .labels
                    .iter()
                    .any(|lid| graph.strings().resolve(lid.0) == label);
                Ok(CypherValue::Bool(has))
            } else {
                Ok(CypherValue::Null)
            }
        }
        CypherValue::Edge { edge_type, .. } => Ok(CypherValue::Bool(edge_type == label)),
        CypherValue::EdgeRef { edge_type_id, .. } => {
            let etype = graph.strings().resolve(edge_type_id.0);
            Ok(CypherValue::Bool(etype == label))
        }
        CypherValue::Null => Ok(CypherValue::Null),
        _ => Ok(CypherValue::Bool(false)),
    }
}

fn eval_binary_op(
    left: &CypherValue,
    op: BinOp,
    right: &CypherValue,
) -> Result<CypherValue, CypherError> {
    // null propagation
    if left.is_null() || right.is_null() {
        return match op {
            BinOp::And | BinOp::Or => {
                // handled above by short-circuit
                Ok(CypherValue::Null)
            }
            _ => Ok(CypherValue::Null),
        };
    }

    match op {
        BinOp::Add => match (left, right) {
            (CypherValue::Integer(a), CypherValue::Integer(b)) => {
                a.checked_add(*b).map(CypherValue::Integer).ok_or_else(|| {
                    CypherError::type_error("long overflow".to_string())
                        .with_status(crate::status::GqlStatus::NumericOutOfRange)
                })
            }
            (CypherValue::Float(a), CypherValue::Float(b)) => Ok(CypherValue::Float(a + b)),
            (CypherValue::Integer(a), CypherValue::Float(b)) => {
                Ok(CypherValue::Float(*a as f64 + b))
            }
            (CypherValue::Float(a), CypherValue::Integer(b)) => {
                Ok(CypherValue::Float(a + *b as f64))
            }
            (CypherValue::String(a), CypherValue::String(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            // Neo4j `+` concatenates strings with numbers/booleans ('epoch: ' + 123 -> 'epoch: 123')
            (CypherValue::String(a), CypherValue::Integer(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            (CypherValue::String(a), CypherValue::Float(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            (CypherValue::String(a), CypherValue::Bool(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            (CypherValue::Integer(a), CypherValue::String(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            (CypherValue::Float(a), CypherValue::String(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            (CypherValue::Bool(a), CypherValue::String(b)) => {
                Ok(CypherValue::String(format!("{a}{b}")))
            }
            (CypherValue::List(a), CypherValue::List(b)) => {
                let mut result = a.clone();
                result.extend(b.iter().cloned());
                Ok(CypherValue::List(result))
            }
            // list + scalar -> append
            (CypherValue::List(a), other) if !matches!(other, CypherValue::Duration(_)) => {
                let mut result = a.clone();
                result.push(other.clone());
                Ok(CypherValue::List(result))
            }
            // scalar + list -> prepend
            (other, CypherValue::List(b)) if !matches!(other, CypherValue::Duration(_)) => {
                let mut result = vec![other.clone()];
                result.extend(b.iter().cloned());
                Ok(CypherValue::List(result))
            }
            // temporal + duration / duration + temporal / duration + duration
            (_, CypherValue::Duration(d)) => temporal::temporal_add(left, d),
            (CypherValue::Duration(d), _) => temporal::temporal_add(right, d),
            _ => Err(CypherError::type_error(format!(
                "cannot add {} and {}",
                left.type_name(),
                right.type_name()
            ))),
        },
        BinOp::Sub => match (left, right) {
            // temporal - duration (incl. duration - duration)
            (_, CypherValue::Duration(d)) => temporal::temporal_sub(left, d),
            _ => numeric_op(left, right, i64::checked_sub, |a, b| a - b),
        },
        BinOp::Mul => match (left, right) {
            (CypherValue::Duration(d), CypherValue::Integer(n)) => {
                Ok(temporal::duration_mul(d, *n as f64))
            }
            (CypherValue::Duration(d), CypherValue::Float(n)) => Ok(temporal::duration_mul(d, *n)),
            (CypherValue::Integer(n), CypherValue::Duration(d)) => {
                Ok(temporal::duration_mul(d, *n as f64))
            }
            (CypherValue::Float(n), CypherValue::Duration(d)) => Ok(temporal::duration_mul(d, *n)),
            _ => numeric_op(left, right, i64::checked_mul, |a, b| a * b),
        },
        BinOp::Div => {
            match (left, right) {
                (CypherValue::Duration(d), CypherValue::Integer(n)) => {
                    if *n == 0 {
                        Err(CypherError::type_error("division by zero".to_string())
                            .with_status(crate::status::GqlStatus::DivisionByZero))
                    } else {
                        temporal::duration_div(d, *n as f64)
                    }
                }
                (CypherValue::Duration(d), CypherValue::Float(n)) => temporal::duration_div(d, *n),
                // division by zero
                (CypherValue::Integer(_), CypherValue::Integer(0)) => {
                    Err(CypherError::type_error("division by zero".to_string())
                        .with_status(crate::status::GqlStatus::DivisionByZero))
                }
                (CypherValue::Integer(a), CypherValue::Integer(b)) => {
                    // checked_div guards i64::MIN / -1, which traps even in release (b == 0 is
                    // handled above)
                    a.checked_div(*b).map(CypherValue::Integer).ok_or_else(|| {
                        CypherError::type_error("integer division overflow".to_string())
                            .with_status(crate::status::GqlStatus::NumericOutOfRange)
                    })
                }
                _ => numeric_op(left, right, i64::checked_div, |a, b| a / b),
            }
        }
        BinOp::Mod => match (left, right) {
            (CypherValue::Integer(a), CypherValue::Integer(b)) => {
                if *b == 0 {
                    Err(CypherError::type_error("modulo by zero".to_string())
                        .with_status(crate::status::GqlStatus::DivisionByZero))
                } else {
                    // checked_rem guards i64::MIN % -1 (same trap)
                    a.checked_rem(*b).map(CypherValue::Integer).ok_or_else(|| {
                        CypherError::type_error("integer modulo overflow".to_string())
                            .with_status(crate::status::GqlStatus::NumericOutOfRange)
                    })
                }
            }
            _ => numeric_op(left, right, i64::checked_rem, |a, b| a % b),
        },
        BinOp::Pow => {
            let a = left.to_f64().ok_or_else(|| {
                CypherError::type_error(format!("cannot exponentiate {}", left.type_name()))
            })?;
            let b = right.to_f64().ok_or_else(|| {
                CypherError::type_error(format!("cannot exponentiate by {}", right.type_name()))
            })?;
            Ok(CypherValue::Float(a.powf(b)))
        }
        BinOp::Concat => {
            // ISO/IEC 39075 section 20.23 / Cypher 25 `||`: concatenates STRINGs or LISTs (Neo4j 2026.x
            // extends it to lists; elements are not coerced or flattened, NULL elements kept). No
            // cross-kind coercion (`'a' || [1]`, `'a' || 1` are type errors); NULL operand
            // propagates
            match (left, right) {
                (CypherValue::Null, _) | (_, CypherValue::Null) => Ok(CypherValue::Null),
                (CypherValue::String(a), CypherValue::String(b)) => {
                    Ok(CypherValue::String(format!("{a}{b}")))
                }
                (CypherValue::List(a), CypherValue::List(b)) => {
                    let mut out = Vec::with_capacity(a.len() + b.len());
                    out.extend(a.iter().cloned());
                    out.extend(b.iter().cloned());
                    Ok(CypherValue::List(out))
                }
                _ => Err(CypherError::type_error(format!(
                    "`||` requires two STRING or two LIST operands; got {} and {}",
                    left.type_name(),
                    right.type_name()
                ))),
            }
        }
        BinOp::Xor => {
            if left.is_null() || right.is_null() {
                Ok(CypherValue::Null)
            } else {
                Ok(CypherValue::Bool(is_truthy(left) ^ is_truthy(right)))
            }
        }
        BinOp::And | BinOp::Or => {
            // handled in eval_expr by short-circuit
            unreachable!()
        }
    }
}

fn numeric_op(
    left: &CypherValue,
    right: &CypherValue,
    int_op: fn(i64, i64) -> Option<i64>,
    float_op: fn(f64, f64) -> f64,
) -> Result<CypherValue, CypherError> {
    match (left, right) {
        (CypherValue::Integer(a), CypherValue::Integer(b)) => {
            int_op(*a, *b).map(CypherValue::Integer).ok_or_else(|| {
                CypherError::type_error("long overflow".to_string())
                    .with_status(crate::status::GqlStatus::NumericOutOfRange)
            })
        }
        (CypherValue::Float(a), CypherValue::Float(b)) => Ok(CypherValue::Float(float_op(*a, *b))),
        (CypherValue::Integer(a), CypherValue::Float(b)) => {
            Ok(CypherValue::Float(float_op(*a as f64, *b)))
        }
        (CypherValue::Float(a), CypherValue::Integer(b)) => {
            Ok(CypherValue::Float(float_op(*a, *b as f64)))
        }
        _ => Err(CypherError::type_error(format!(
            "numeric operation on {} and {}",
            left.type_name(),
            right.type_name()
        ))),
    }
}

/// Fast path for reduce() over numeric lists: `reduce(total = 0, s IN list | total + s)`
/// and `... | total + coalesce(s, 0)` run as a tight int/float loop instead of
/// ~6 eval_expr calls per item.
fn try_reduce_numeric_fast(
    acc_name: &str,
    init: &CypherValue,
    var_name: &str,
    update_expr: &Expr,
    items: &[CypherValue],
) -> Option<CypherValue> {
    // acc + expr or expr + acc
    let (is_add, item_expr) = match update_expr {
        Expr::BinaryOp(left, BinOp::Add, right) => {
            if matches!(left.as_ref(), Expr::Variable(v) if v == acc_name) {
                (true, right.as_ref())
            } else if matches!(right.as_ref(), Expr::Variable(v) if v == acc_name) {
                (true, left.as_ref())
            } else {
                return None;
            }
        }
        _ => return None,
    };
    if !is_add {
        return None;
    }

    // item_expr is `Variable(v)` or `coalesce(Variable(v), Literal(default))`
    let (direct_var, null_default) = match item_expr {
        Expr::Variable(v) if v == var_name => (true, None),
        Expr::FunctionCall { name, args, .. } if name == "coalesce" && args.len() == 2 => {
            if matches!(&args[0], Expr::Variable(v) if v == var_name) {
                match &args[1] {
                    Expr::Literal(Literal::Integer(n)) => (false, Some(*n)),
                    _ => return None,
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };

    // integer accumulation
    let mut acc = match init {
        CypherValue::Integer(n) => *n,
        _ => return None,
    };
    let default = null_default.unwrap_or(0);

    for item in items {
        match item {
            CypherValue::Integer(n) => acc += n,
            CypherValue::Float(f) => {
                // mixed int/float: switch to float
                let mut facc = acc as f64 + f;
                for rest in &items[items.iter().position(|x| std::ptr::eq(x, item)).unwrap() + 1..]
                {
                    match rest {
                        CypherValue::Integer(n) => facc += *n as f64,
                        CypherValue::Float(f) => facc += f,
                        CypherValue::Null if !direct_var => facc += default as f64,
                        CypherValue::Null if direct_var => return Some(CypherValue::Null),
                        _ => return None,
                    }
                }
                return Some(CypherValue::Float(facc));
            }
            CypherValue::Null if !direct_var => acc += default,
            CypherValue::Null if direct_var => return Some(CypherValue::Null),
            _ => return None,
        }
    }
    Some(CypherValue::Integer(acc))
}

pub(crate) fn get_property(val: &CypherValue, key: &str) -> Result<CypherValue, CypherError> {
    Ok(match val {
        CypherValue::Node { properties, .. } => properties
            .get(key)
            .map(CypherValue::from_property)
            .unwrap_or(CypherValue::Null),
        CypherValue::Edge { properties, .. } => properties
            .get(key)
            .map(CypherValue::from_property)
            .unwrap_or(CypherValue::Null),
        CypherValue::Map(m) => m.get(key).cloned().unwrap_or(CypherValue::Null),
        CypherValue::Date(_)
        | CypherValue::LocalTime(_)
        | CypherValue::Time(_)
        | CypherValue::LocalDateTime(_)
        | CypherValue::DateTime(_)
        | CypherValue::Duration(_) => {
            temporal::get_temporal_property(val, key).unwrap_or(CypherValue::Null)
        }
        CypherValue::Null => CypherValue::Null,
        // property access on a scalar is a type error (Neo4j parity); NULL would hide typos like
        // `WITH 1 AS x RETURN x.prop`
        other => {
            return Err(CypherError::type_error(format!(
                "Type mismatch: expected a map, node, relationship, point or temporal \
                 value for property access, got {}",
                other.type_name()
            )));
        }
    })
}

pub(crate) fn eval_integer_expr(expr: &Expr, graph: &dyn GraphView) -> Result<i64, CypherError> {
    let rec = Record::new();
    let val = eval_expr_rec(expr, &rec, graph)?;
    match val {
        CypherValue::Integer(n) => Ok(n),
        _ => Err(CypherError::type_error(format!(
            "expected integer, got {}",
            val.type_name()
        ))),
    }
}

/// Truthiness: null -> false, bool -> itself, else true.
pub(crate) fn is_truthy(val: &CypherValue) -> bool {
    match val {
        CypherValue::Null => false,
        CypherValue::Bool(b) => *b,
        _ => true,
    }
}

/// Evaluates an expression against a Record (cold-path wrapper around eval_expr).
pub(super) fn eval_expr_rec(
    expr: &Expr,
    rec: &Record,
    graph: &dyn GraphView,
) -> Result<CypherValue, CypherError> {
    let mut symtab = frame::record_to_symtab(rec);
    frame::collect_expr_variables(expr, &mut symtab);
    let frame = frame::record_to_frame(rec, &symtab);
    eval_expr(expr, &frame, graph, &symtab)
}
