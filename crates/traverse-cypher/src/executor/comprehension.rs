//! Pattern comprehensions: `[(a)-->(b) WHERE ... | expr]`, on the operator path when the shape
//! allows, else on the record interpreter.

use std::cell::RefCell;
use std::collections::HashMap;

use traverse_core::tx::view::GraphView;

use crate::ast::{BinOp, CmpOp, Expr, MatchClause};
use crate::error::CypherError;
use crate::frame::{self, Frame, SymbolTable};
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

use super::Record;
use super::eval::eval_expr;
use super::interp::{PC_EXEMPT_EDGES, exec_subquery};
use super::plan_cache::{cached_plan, subquery_fingerprint};

thread_local! {
    /// Outer-row frame emitted by a slot-native comprehension plan's `ContextSeed` leaf. Set right
    /// before pulling the operator tree, cleared after.
    static SUBQUERY_SEED: RefCell<Option<Frame>> = const { RefCell::new(None) };
}

/// Current subquery seed frame (cloned) for `ContextSeedOp`.
pub(crate) fn current_subquery_seed() -> Option<Frame> {
    SUBQUERY_SEED.with(|s| s.borrow().clone())
}

pub(super) fn eval_pattern_comprehension(
    path_var: Option<&str>,
    match_clause: &MatchClause,
    projection: &Expr,
    outer_rec: &Record,
    graph: &dyn GraphView,
) -> Result<CypherValue, CypherError> {
    // building the plan (pattern clone, anon vars, planning, access-path rewrite) dominated
    // per-row cost before it was cached
    let plan = cached_plan(
        match_clause as *const MatchClause as usize,
        || subquery_fingerprint(&format!("{match_clause:?}|{path_var:?}"), 3),
        || build_pattern_comprehension_plan(path_var, match_clause, graph),
    )?;
    let refs = pattern_comprehension_refs(match_clause, projection);
    exec_pattern_comprehension_plan(&plan, projection, outer_rec, &refs, graph)
}

/// Cached slot-native comprehension: operator-path plan (leaves seeded by
/// [`LogicalPlan::ContextSeed`]), its symbol table, and seeding/eligibility data.
struct SlotNativeComprehension {
    plan: LogicalPlan,
    symtab: SymbolTable,
    /// Outer variables the comprehension uses, copied into the seed frame (resolved by name in the
    /// CALLER's symtab).
    seed_vars: Vec<String>,
    /// Pattern variable names: if any is BOUND in the outer frame the frame-native plan would
    /// rebind it, so fall back.
    pattern_vars: Vec<String>,
}

thread_local! {
    /// match_clause address -> slot-native executable (None = unsupported shape, fall back). Cleared
    /// with the pointer caches.
    static PC_SLOT_NATIVE: RefCell<HashMap<usize, std::rc::Rc<Option<SlotNativeComprehension>>>> =
        RefCell::new(HashMap::new());
}

/// Rewrites a Record-interpreter comprehension plan into operator form: leaves get a
/// [`LogicalPlan::ContextSeed`] driving side so outer bindings reach lookup-join keys and filters.
/// `None` for shapes the frame path can't run faithfully.
#[recursive::recursive]
fn slot_native_transform(plan: &LogicalPlan) -> Option<LogicalPlan> {
    match plan {
        LogicalPlan::Scan { variable, label } => Some(LogicalPlan::CrossProduct {
            left: Box::new(LogicalPlan::ContextSeed),
            right: Box::new(LogicalPlan::Scan {
                variable: variable.clone(),
                label: label.clone(),
            }),
        }),
        LogicalPlan::IndexScan {
            variable,
            label,
            property,
            value,
        } => Some(LogicalPlan::IndexLookupJoin {
            input: Box::new(LogicalPlan::ContextSeed),
            variable: variable.clone(),
            label: label.clone(),
            property: property.clone(),
            key: value.clone(),
        }),
        LogicalPlan::Union { left, right, all } => Some(LogicalPlan::Union {
            left: Box::new(slot_native_transform(left)?),
            right: Box::new(slot_native_transform(right)?),
            all: *all,
        }),
        LogicalPlan::Filter { input, predicate } => Some(LogicalPlan::Filter {
            input: Box::new(slot_native_transform(input)?),
            predicate: predicate.clone(),
        }),
        LogicalPlan::ExpandOut {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => Some(LogicalPlan::ExpandOut {
            input: Box::new(slot_native_transform(input)?),
            from: from.clone(),
            rel_var: rel_var.clone(),
            to: to.clone(),
            edge_types: edge_types.clone(),
            range: *range,
            prior_edge_vars: prior_edge_vars.clone(),
        }),
        LogicalPlan::ExpandIn {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => Some(LogicalPlan::ExpandIn {
            input: Box::new(slot_native_transform(input)?),
            from: from.clone(),
            rel_var: rel_var.clone(),
            to: to.clone(),
            edge_types: edge_types.clone(),
            range: *range,
            prior_edge_vars: prior_edge_vars.clone(),
        }),
        LogicalPlan::ExpandBoth {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => Some(LogicalPlan::ExpandBoth {
            input: Box::new(slot_native_transform(input)?),
            from: from.clone(),
            rel_var: rel_var.clone(),
            to: to.clone(),
            edge_types: edge_types.clone(),
            range: *range,
            prior_edge_vars: prior_edge_vars.clone(),
        }),
        LogicalPlan::BindPath {
            input,
            variable,
            node_vars,
            edge_vars,
        } => Some(LogicalPlan::BindPath {
            input: Box::new(slot_native_transform(input)?),
            variable: variable.clone(),
            node_vars: node_vars.clone(),
            edge_vars: edge_vars.clone(),
        }),
        _ => None,
    }
}

/// Builds (or fetches) the slot-native executable for a comprehension and runs it on the caller's
/// frame, skipping the Record interpreter. `None` if ineligible.
pub(super) fn try_slot_native_comprehension(
    path_var: Option<&str>,
    match_clause: &MatchClause,
    projection: &Expr,
    frame: &Frame,
    symtab: &SymbolTable,
    graph: &dyn GraphView,
) -> Result<Option<CypherValue>, CypherError> {
    let key = match_clause as *const MatchClause as usize;
    let entry = PC_SLOT_NATIVE.with(|c| c.borrow().get(&key).cloned());
    let entry = match entry {
        Some(e) => e,
        None => {
            let built: Option<SlotNativeComprehension> = (|| {
                let record_plan =
                    build_pattern_comprehension_plan(path_var, match_clause, graph).ok()?;
                let plan = slot_native_transform(&record_plan)?;
                let mut sub_symtab = SymbolTable::new();
                frame::collect_plan_variables(&plan, &mut sub_symtab);
                frame::collect_expr_variables(projection, &mut sub_symtab);
                let refs = pattern_comprehension_refs(match_clause, projection);
                let mut pattern_vars = Vec::new();
                for part in &match_clause.pattern.parts {
                    let mut nv = |v: &Option<String>| {
                        if let Some(v) = v {
                            pattern_vars.push(v.clone());
                        }
                    };
                    nv(&part.element.node.variable);
                    for chain in &part.element.chain {
                        nv(&chain.rel.variable);
                        nv(&chain.node.variable);
                    }
                }
                let mut seed_vars = Vec::new();
                for v in refs.iter() {
                    // outer refs the pattern doesn't produce must be seeded (outer-bound pattern
                    // vars too, but those trigger the fallback anyway)
                    if !pattern_vars.contains(v) {
                        sub_symtab.add(v.clone());
                        seed_vars.push(v.clone());
                    }
                }
                Some(SlotNativeComprehension {
                    plan,
                    symtab: sub_symtab,
                    seed_vars,
                    pattern_vars,
                })
            })();
            let rc = std::rc::Rc::new(built);
            PC_SLOT_NATIVE.with(|c| {
                c.borrow_mut().insert(key, rc.clone());
            });
            rc
        }
    };
    let Some(sn) = entry.as_ref() else {
        return Ok(None);
    };
    // bound pattern variables: the frame path would rebind, fall back
    for pv in &sn.pattern_vars {
        if let Some(slot) = symtab.slot(pv)
            && !frame[slot].is_null()
        {
            return Ok(None);
        }
    }
    // seed frame: copy the referenced outer slots, slot to slot
    let mut seed = frame::new_frame(&sn.symtab);
    for name in &sn.seed_vars {
        if let (Some(outer), Some(inner)) = (symtab.slot(name), sn.symtab.slot(name)) {
            seed[inner] = frame[outer].clone();
        }
    }
    // Operator::next needs &mut dyn GraphView; comprehensions only read, so wrap the &(dyn + Sync)
    // view; views without one use the Record path
    let Some(sync_graph) = graph.as_sync_ref() else {
        return Ok(None);
    };
    SUBQUERY_SEED.with(|s| *s.borrow_mut() = Some(seed));
    let result = (|| -> Result<CypherValue, CypherError> {
        let mut op = crate::operators::build_operator_tree(&sn.plan);
        let mut results = Vec::new();
        let mut shim = crate::operators::pipeline_par::ReadOnlyGraphView(sync_graph);
        while let Some(f) = op.next(&mut shim, &sn.symtab)? {
            results.push(eval_expr(projection, &f, graph, &sn.symtab)?);
        }
        Ok(CypherValue::List(results))
    })();
    SUBQUERY_SEED.with(|s| *s.borrow_mut() = None);
    result.map(Some)
}

thread_local! {
    /// Address-keyed cache of the OUTER variables a comprehension uses (pattern vars + WHERE +
    /// projection vars). Seeding only these keeps per-row clones from dragging unrelated outer
    /// values (paths, collected lists) along.
    static PC_REFS: RefCell<HashMap<usize, std::rc::Rc<std::collections::HashSet<String>>>> =
        RefCell::new(HashMap::new());
}

/// Drops the comprehension caches, which are keyed by AST address.
pub(super) fn clear_comprehension_caches() {
    PC_REFS.with(|c| c.borrow_mut().clear());
    PC_SLOT_NATIVE.with(|c| c.borrow_mut().clear());
}

pub(super) fn pattern_comprehension_refs(
    match_clause: &MatchClause,
    projection: &Expr,
) -> std::rc::Rc<std::collections::HashSet<String>> {
    let key = match_clause as *const MatchClause as usize;
    if let Some(r) = PC_REFS.with(|c| c.borrow().get(&key).cloned()) {
        return r;
    }
    let mut vars = std::collections::HashSet::new();
    for part in &match_clause.pattern.parts {
        if let Some(v) = &part.variable {
            vars.insert(v.clone());
        }
        fn node(n: &crate::ast::NodePattern, vars: &mut std::collections::HashSet<String>) {
            if let Some(v) = &n.variable {
                vars.insert(v.clone());
            }
            if let Some(props) = &n.properties {
                crate::planner::collect_expr_variables(props, vars);
            }
        }
        node(&part.element.node, &mut vars);
        for chain in &part.element.chain {
            node(&chain.node, &mut vars);
            if let Some(v) = &chain.rel.variable {
                vars.insert(v.clone());
            }
            if let Some(props) = &chain.rel.properties {
                crate::planner::collect_expr_variables(props, &mut vars);
            }
        }
    }
    if let Some(w) = &match_clause.where_clause {
        crate::planner::collect_expr_variables(w, &mut vars);
    }
    crate::planner::collect_expr_variables(projection, &mut vars);
    let rc = std::rc::Rc::new(vars);
    PC_REFS.with(|c| {
        c.borrow_mut().insert(key, rc.clone());
    });
    rc
}

/// Builds the cacheable plan of a pattern comprehension: anon-var naming, pattern planning,
/// optional BindPath, WHERE access-path rewrite + Filter.
fn build_pattern_comprehension_plan(
    path_var: Option<&str>,
    match_clause: &MatchClause,
    graph: &dyn GraphView,
) -> Result<LogicalPlan, CypherError> {
    // deterministic names for anonymous elements so BindPath can reference them
    let mut pattern = match_clause.pattern.clone();
    let mut node_vars = Vec::new();
    let mut edge_vars = Vec::new();
    fn next_pc_id() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }
    for part in &mut pattern.parts {
        if part.element.node.variable.is_none() {
            part.element.node.variable = Some(format!("__pc_{}", next_pc_id()));
        }
        node_vars.push(part.element.node.variable.clone().unwrap());
        for chain in &mut part.element.chain {
            if chain.rel.variable.is_none() {
                chain.rel.variable = Some(format!("__pc_{}", next_pc_id()));
            }
            edge_vars.push(chain.rel.variable.clone().unwrap());
            if chain.node.variable.is_none() {
                chain.node.variable = Some(format!("__pc_{}", next_pc_id()));
            }
            node_vars.push(chain.node.variable.clone().unwrap());
        }
    }

    let mut comp_local_vars: std::collections::HashSet<String> =
        node_vars.iter().cloned().collect();
    comp_local_vars.extend(edge_vars.iter().cloned());
    if let Some(pv) = path_var {
        comp_local_vars.insert(pv.to_string());
    }

    let mut plan = crate::planner::plan_match_pattern_pub(&pattern)?;

    // path variable requested: add BindPath
    if let Some(pv) = path_var {
        plan = LogicalPlan::BindPath {
            input: Box::new(plan),
            variable: pv.to_string(),
            node_vars,
            edge_vars,
        };
    }

    if let Some(ref where_expr) = match_clause.where_clause {
        // access path: when WHERE pins the leaf variable's indexed property to outer-only
        // expressions (`x.id = startNode(r).id OR x.id = endNode(r).id`), replace the leaf label
        // Scan with index seeks; the full WHERE Filter stays on top
        plan = comprehension_access_path(plan, where_expr, &comp_local_vars, graph);
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: where_expr.clone(),
        };
    }

    Ok(plan)
}

fn exec_pattern_comprehension_plan(
    plan: &LogicalPlan,
    projection: &Expr,
    outer_rec: &Record,
    refs: &std::collections::HashSet<String>,
    graph: &dyn GraphView,
) -> Result<CypherValue, CypherError> {
    // the comprehension has its own relationship-uniqueness scope: outer edges stay
    // BOUND (its WHERE/projection may use them, e.g. startNode(r) of an enclosing
    // variable) but must NOT block its own matches, so their ids go under the
    // exemption key bound_edge_ids subtracts. Seed ONLY the outer variables it uses
    // (unused paths / collected lists would be cloned into every row); exemptions
    // still cover ALL outer edges.
    let mut seeded_rec = Record::new();
    let mut exempt: Vec<CypherValue> = Vec::new();
    for (k, v) in outer_rec {
        if refs.contains(k) {
            seeded_rec.insert(k.clone(), v.clone());
        }
        match v {
            CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } => {
                exempt.push(CypherValue::Integer(id.0 as i64));
            }
            CypherValue::List(items) => {
                for item in items {
                    if let CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } = item {
                        exempt.push(CypherValue::Integer(id.0 as i64));
                    }
                }
            }
            _ => {}
        }
    }
    if !exempt.is_empty() {
        seeded_rec.insert(PC_EXEMPT_EDGES.to_string(), CypherValue::List(exempt));
    }
    let records = exec_subquery(plan, &seeded_rec, graph)?;
    if records.is_empty() {
        return Ok(CypherValue::List(Vec::new()));
    }
    // one symtab for the batch; eval_expr_rec would rebuild and rehash it per record
    let mut symtab = frame::records_to_symtab(&records);
    frame::collect_expr_variables(projection, &mut symtab);
    let mut results = Vec::new();
    for inner_rec in &records {
        let frame = frame::record_to_frame(inner_rec, &symtab);
        results.push(eval_expr(projection, &frame, graph, &symtab)?);
    }
    Ok(CypherValue::List(results))
}

/// Replaces a comprehension plan's leaf label `Scan` with index seeks when
/// `where_expr` pins the leaf variable's indexed property to OUTER-only
/// expressions: one equality conjunct (one `IndexScan`), or an OR with such an
/// equality in every branch (distinct `Union` of seeks).
fn comprehension_access_path(
    plan: LogicalPlan,
    where_expr: &Expr,
    local_vars: &std::collections::HashSet<String>,
    graph: &dyn GraphView,
) -> LogicalPlan {
    // leaf Scan's variable + label
    #[recursive::recursive]
    fn find_leaf<'a>(p: &'a LogicalPlan) -> Option<(&'a str, &'a str)> {
        match p {
            LogicalPlan::Scan {
                variable,
                label: Some(l),
            } => Some((variable, l)),
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::ExpandOut { input, .. }
            | LogicalPlan::ExpandIn { input, .. }
            | LogicalPlan::ExpandBoth { input, .. }
            | LogicalPlan::DistinctExpand { input, .. }
            | LogicalPlan::BindPath { input, .. } => find_leaf(input),
            _ => None,
        }
    }
    let Some((leaf_var, leaf_label)) = find_leaf(&plan) else {
        return plan;
    };
    let leaf_var = leaf_var.to_string();
    let leaf_label = leaf_label.to_string();
    let hints = crate::build_index_hints_for(graph);

    // outer-only equality keys on the leaf var: `leaf.prop = e`
    let usable = |e: &Expr| -> Option<(String, Expr)> {
        let Expr::Comparison(l, pairs) = e else {
            return None;
        };
        if pairs.len() != 1 || pairs[0].0 != CmpOp::Eq {
            return None;
        }
        let r = &pairs[0].1;
        for (prop_side, key_side) in [(&**l, r), (r, &**l)] {
            if let Expr::Property(base, key) = prop_side
                && matches!(base.as_ref(), Expr::Variable(v) if *v == leaf_var)
                && hints.contains(&(leaf_label.clone(), key.clone()))
            {
                let mut vars = std::collections::HashSet::new();
                crate::planner::collect_expr_variables(key_side, &mut vars);
                if vars.is_disjoint(local_vars) {
                    return Some((key.clone(), key_side.clone()));
                }
            }
        }
        None
    };
    #[recursive::recursive]
    fn conjuncts(e: &Expr, out: &mut Vec<Expr>) {
        if let Expr::BinaryOp(l, BinOp::And, r) = e {
            conjuncts(l, out);
            conjuncts(r, out);
        } else {
            out.push(e.clone());
        }
    }
    #[recursive::recursive]
    fn or_branches(e: &Expr, out: &mut Vec<Expr>) {
        if let Expr::BinaryOp(l, BinOp::Or, r) = e {
            or_branches(l, out);
            or_branches(r, out);
        } else {
            out.push(e.clone());
        }
    }

    // seek keys: one conjunct, or one per OR branch
    let mut keys: Vec<(String, Expr)> = Vec::new();
    let mut top = Vec::new();
    conjuncts(where_expr, &mut top);
    if let Some(k) = top.iter().find_map(&usable) {
        keys.push(k);
    } else {
        let mut branches = Vec::new();
        or_branches(where_expr, &mut branches);
        if branches.len() >= 2 {
            let per_branch: Option<Vec<(String, Expr)>> = branches
                .iter()
                .map(|b| {
                    let mut cs = Vec::new();
                    conjuncts(b, &mut cs);
                    cs.iter().find_map(&usable)
                })
                .collect();
            match per_branch {
                Some(ks) if !ks.is_empty() && ks.iter().all(|(p, _)| *p == ks[0].0) => {
                    keys = ks;
                }
                _ => {}
            }
        }
    }
    if keys.is_empty() {
        return plan;
    }

    // replacement access path
    let prop = keys[0].0.clone();
    let mut seek: Option<LogicalPlan> = None;
    for (_, key) in keys {
        let scan = LogicalPlan::IndexScan {
            variable: leaf_var.clone(),
            label: leaf_label.clone(),
            property: prop.clone(),
            value: key,
        };
        seek = Some(match seek {
            None => scan,
            Some(prev) => LogicalPlan::Union {
                left: Box::new(prev),
                right: Box::new(scan),
                all: false,
            },
        });
    }
    let seek = seek.unwrap();

    #[recursive::recursive]
    fn swap_leaf(p: LogicalPlan, seek: &LogicalPlan) -> LogicalPlan {
        match p {
            LogicalPlan::Scan { label: Some(_), .. } => seek.clone(),
            LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
                input: Box::new(swap_leaf(*input, seek)),
                predicate,
            },
            LogicalPlan::ExpandOut {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => LogicalPlan::ExpandOut {
                input: Box::new(swap_leaf(*input, seek)),
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            },
            LogicalPlan::ExpandIn {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => LogicalPlan::ExpandIn {
                input: Box::new(swap_leaf(*input, seek)),
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            },
            LogicalPlan::ExpandBoth {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => LogicalPlan::ExpandBoth {
                input: Box::new(swap_leaf(*input, seek)),
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            },
            LogicalPlan::BindPath {
                input,
                variable,
                node_vars,
                edge_vars,
            } => LogicalPlan::BindPath {
                input: Box::new(swap_leaf(*input, seek)),
                variable,
                node_vars,
                edge_vars,
            },
            other => other,
        }
    }
    swap_leaf(plan, &seek)
}
