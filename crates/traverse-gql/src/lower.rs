//! Lowers a GQL [`crate::ast::Program`] into the
//! [`traverse_cypher::ast::Statement`] the shared executor runs.
//!
//! Two parsers (`traverse-cypher`, `traverse-gql`), one executor: this module
//! walks the GQL AST and builds the equivalent Cypher AST, so the engine needs
//! no dialect awareness. Spec coverage is tracked in `CONFORMANCE.md`.

use traverse_cypher::ast as cy;

use crate::ast::{
    self, BinaryOp, ConstraintKind, EdgeDirection, Expression, LabelExpression, QueryBody,
    SortDirection, TypeKind, UnaryOp,
};
use crate::error::{ParseError, ParseErrorKind};
use crate::lexer::Span;

/// Lowering result: one [`traverse_cypher::ast::Statement`] for the executor.
pub type Lowered = cy::Statement;

/// Lowers a whole GQL program: a single statement, or a `NEXT` chain fused into one.
pub fn lower(program: &ast::Program) -> Result<Lowered, Vec<ParseError>> {
    // multi-step quantified groups with a range (`((sub)){m,n}`, plus capped `+` / `*` / `{m,}`)
    // become UNION ALL of exact-count unrollings before lowering
    let program_owned;
    let program: &ast::Program = if needs_range_expansion(program) {
        program_owned = expand_range_quantifiers(program.clone());
        &program_owned
    } else {
        program
    };

    // ISO section 9.3 `NEXT` chains fuse into one QueryStatement (inner RETURNs become WITHs, filtered by
    // the separator's YIELD list) so lowering runs unchanged; `;`-separated and mixed DDL/query
    // programs need script-level dispatch and error here
    if program.statements.len() > 1
        && let Some(fused) = try_fuse_next_chain(program)
    {
        return lower_statement(&fused).map_err(|e| vec![e]);
    }
    match program.statements.as_slice() {
        [] => Err(vec![ParseError {
            span: program.span,
            kind: ParseErrorKind::NotImplemented,
            message: "cannot lower an empty program".into(),
        }]),
        [one] => lower_statement(one).map_err(|e| vec![e]),
        many => Err(vec![ParseError {
            span: many[1].span(),
            kind: ParseErrorKind::NotImplemented,
            message: "multi-statement scripts mixing catalog DDL with \
             queries are not yet supported by the executor - split \
             into separate programs or use `NEXT` between query \
             statements"
                .into(),
        }]),
    }
}

/// Does `program` have an [`ast::PathElement::Group`] with a multi-step inner pattern and a
/// non-exact quantifier? Those get rewritten into UNION ALL of exact-count unrollings before
/// lowering.
fn needs_range_expansion(program: &ast::Program) -> bool {
    program.statements.iter().any(|s| match s {
        ast::Statement::Query(q) => {
            body_has_expandable_range(&q.head)
                || q.unions.iter().any(|u| body_has_expandable_range(&u.body))
        }
        _ => false,
    })
}

fn body_has_expandable_range(body: &ast::QueryBody) -> bool {
    body.clauses.iter().any(clause_has_expandable_range)
}

fn clause_has_expandable_range(c: &ast::Clause) -> bool {
    if let ast::Clause::Match(m) = c {
        m.patterns.iter().any(pattern_has_expandable_range)
    } else {
        false
    }
}

fn pattern_has_expandable_range(p: &ast::PathPattern) -> bool {
    p.chain.iter().any(element_is_expandable_range)
}

/// Iteration cap for unbounded multi-step quantifiers (`+`, `*`, `{m,}`), like the implicit max
/// depth Neo4j applies to var-length edges. Deeper paths need an explicit `{m,n}`.
const UNBOUNDED_ITERATION_CAP: usize = 10;

fn element_is_expandable_range(e: &ast::PathElement) -> bool {
    match e {
        ast::PathElement::Step { .. } => false,
        ast::PathElement::Group {
            sub, quantifier, ..
        } => {
            // expand any multi-step group whose effective range spans more than one count:
            // * bounded `{m,n}` with min >= 1 and max > min
            // * unbounded `+`/`*`/`{m,}`, capped at `UNBOUNDED_ITERATION_CAP` to keep the
            //   UNION finite
            // * min = 0 with max > 0: the k=0 arm drops the group from the chain
            if sub.chain.len() < 2 {
                return false;
            }
            match (quantifier.min, quantifier.max) {
                (m, Some(mx)) if m >= 1 && mx > m => true, // bounded range
                (_, None) => true,                         // unbounded
                (0, Some(mx)) if mx >= 1 => true,          // zero-iteration
                _ => false,
            }
        }
    }
}

/// Effective `(min, max)` of an expandable quantifier, unbounded forms capped.
fn effective_range(quantifier: &ast::PathQuantifier) -> (usize, usize) {
    match quantifier.max {
        Some(mx) => (quantifier.min, mx),
        None => (quantifier.min, UNBOUNDED_ITERATION_CAP),
    }
}

/// Rewrites every non-exact multi-step quantified group into UNION ALL of exact-count unrollings:
/// find the first expandable range, emit one body per count, join with `UnionKind::All` parts.
/// Repeated to a fixed point so several ranges cascade.
fn expand_range_quantifiers(mut program: ast::Program) -> ast::Program {
    // bound the cascade so degenerate input (`{1,1000}` on several groups) can't blow up the plan:
    // 1024 UnionParts in total
    let mut budget: usize = 1024;
    loop {
        let mut changed = false;
        for stmt in program.statements.iter_mut() {
            if let ast::Statement::Query(q) = stmt
                && expand_one_range_in_query(q, &mut budget)
            {
                changed = true;
            }
        }
        if !changed || budget == 0 {
            break;
        }
    }
    program
}

/// Expands the first non-exact multi-step group in `q.head` or its union arms; `true` if it did.
/// One range per call, outer loops cascade.
fn expand_one_range_in_query(q: &mut ast::QueryStatement, budget: &mut usize) -> bool {
    // (arm body, is it the head?) list, to find a range in any arm
    enum Arm {
        Head,
        Union(usize),
    }
    let mut target_arm: Option<Arm> = None;
    let mut target_range: Option<(usize, usize)> = None;
    if let Some(r) = find_first_expandable_range(&q.head) {
        target_arm = Some(Arm::Head);
        target_range = Some(r);
    } else {
        for (i, u) in q.unions.iter().enumerate() {
            if let Some(r) = find_first_expandable_range(&u.body) {
                target_arm = Some(Arm::Union(i));
                target_range = Some(r);
                break;
            }
        }
    }
    let (arm, (min, max)) = match (target_arm, target_range) {
        (Some(a), Some(r)) => (a, r),
        _ => return false,
    };
    let count = max - min + 1;
    if *budget < count {
        return false;
    }
    *budget = budget.saturating_sub(count);
    // one body per iteration count
    let source_body: ast::QueryBody = match &arm {
        Arm::Head => q.head.clone(),
        Arm::Union(i) => q.unions[*i].body.clone(),
    };
    let mut variants: Vec<ast::QueryBody> = Vec::with_capacity(count);
    for k in min..=max {
        variants.push(substitute_first_range_in_body(source_body.clone(), k));
    }
    // the first variant replaces the source arm, the rest become UnionParts right after it, keeping
    // the source's relative order
    let mut new_arms = variants.into_iter();
    let first = new_arms.next().unwrap();
    let new_unions: Vec<ast::UnionPart> = new_arms
        .map(|body| ast::UnionPart {
            kind: ast::UnionKind::All,
            body,
            span: Span::default(),
        })
        .collect();
    match arm {
        Arm::Head => {
            // new UnionParts go in front of the existing unions, next to the (new) head
            let mut combined: Vec<ast::UnionPart> =
                Vec::with_capacity(new_unions.len() + q.unions.len());
            combined.extend(new_unions);
            combined.append(&mut q.unions);
            q.head = first;
            q.unions = combined;
        }
        Arm::Union(i) => {
            q.unions[i].body = first;
            for (off, u) in new_unions.into_iter().enumerate() {
                q.unions.insert(i + 1 + off, u);
            }
        }
    }
    true
}

/// `(min, max)` of the first non-exact multi-step quantified group in `body`, across every MATCH
/// clause's chain.
fn find_first_expandable_range(body: &ast::QueryBody) -> Option<(usize, usize)> {
    for c in &body.clauses {
        if let ast::Clause::Match(m) = c {
            for p in &m.patterns {
                if let Some(r) = find_first_range_in_chain(&p.chain) {
                    return Some(r);
                }
            }
        }
    }
    None
}

#[recursive::recursive]
fn find_first_range_in_chain(chain: &[ast::PathElement]) -> Option<(usize, usize)> {
    for e in chain {
        if let ast::PathElement::Group {
            sub, quantifier, ..
        } = e
        {
            if element_is_expandable_range(e) {
                return Some(effective_range(quantifier));
            }
            // nested groups: recurse into the sub-pattern's chain
            if let Some(r) = find_first_range_in_chain(&sub.chain) {
                return Some(r);
            }
        }
    }
    None
}

/// Clones `body` with the first expandable group rewritten for count `k`:
/// * `k = 0`: group removed from the chain (zero-iteration arm)
/// * `k >= 1`: quantifier becomes `{k, k}`, i.e. the exact-count unroll
fn substitute_first_range_in_body(mut body: ast::QueryBody, k: usize) -> ast::QueryBody {
    for c in body.clauses.iter_mut() {
        if let ast::Clause::Match(m) = c {
            for p in m.patterns.iter_mut() {
                if substitute_first_range_in_chain(&mut p.chain, k) {
                    return body;
                }
            }
        }
    }
    body
}

#[recursive::recursive]
fn substitute_first_range_in_chain(chain: &mut Vec<ast::PathElement>, k: usize) -> bool {
    // first expandable group at this level
    let mut target_idx: Option<usize> = None;
    for (idx, e) in chain.iter().enumerate() {
        if let ast::PathElement::Group {
            sub, quantifier, ..
        } = e
        {
            let is_expandable = sub.chain.len() >= 2
                && match (quantifier.min, quantifier.max) {
                    (m, Some(mx)) if m >= 1 && mx > m => true,
                    (_, None) => true,
                    (0, Some(mx)) if mx >= 1 => true,
                    _ => false,
                };
            if is_expandable {
                target_idx = Some(idx);
                break;
            }
        }
    }
    if let Some(idx) = target_idx {
        if k == 0 {
            // zero-iteration arm: drop the group
            chain.remove(idx);
        } else if let ast::PathElement::Group { quantifier, .. } = &mut chain[idx] {
            quantifier.min = k;
            quantifier.max = Some(k);
        }
        return true;
    }
    // recurse into nested groups
    for e in chain.iter_mut() {
        if let ast::PathElement::Group { sub, .. } = e
            && substitute_first_range_in_chain(&mut sub.chain, k)
        {
            return true;
        }
    }
    false
}

/// ISO 39075 section 9.3 linear composition: if every separator is `NEXT`, fuses the chain into one
/// [`ast::Statement::Query`], turning each non-final `RETURN` into a `WITH` (YIELD list as a column
/// filter) and concatenating clauses. `None` if any separator is `Semicolon`, any statement isn't a
/// query, or a non-final statement has a `UNION` (it needs a terminating RETURN that doesn't
/// survive RETURN->WITH).
fn try_fuse_next_chain(program: &ast::Program) -> Option<ast::Statement> {
    use crate::ast::{Clause, ReturnClause, Statement, WithClause};
    // all separators `Next`, all statements `Query`, no UNION before the last statement (the last
    // one's UNION is kept)
    for s in &program.separators {
        if !matches!(s, ast::StatementSeparator::Next { .. }) {
            return None;
        }
    }
    let queries: Vec<&ast::QueryStatement> = program
        .statements
        .iter()
        .map(|s| match s {
            Statement::Query(q) => Some(q),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    if queries.len() < 2 {
        return None;
    }
    // non-final statements single-body (no UNIONs) so RETURN->WITH is well defined
    if queries[..queries.len() - 1]
        .iter()
        .any(|q| !q.unions.is_empty())
    {
        return None;
    }
    let mut clauses: Vec<Clause> = Vec::new();
    let span_start = program.statements[0].span().start;
    for (i, q) in queries.iter().enumerate() {
        let is_last = i == queries.len() - 1;
        if is_last {
            // final statement: clauses + its RETURN as-is
            clauses.extend(q.head.clauses.iter().cloned());
            break;
        }
        // non-final statement: RETURN -> WITH, filtered by the next separator's YIELD list
        let yields = match &program.separators[i] {
            ast::StatementSeparator::Next { yields } => yields.clone(),
            ast::StatementSeparator::Semicolon => return None, // unreachable (guarded above)
        };
        // copy non-RETURN clauses, turn the tail RETURN into a WITH
        let mut ret: Option<ReturnClause> = None;
        for c in &q.head.clauses {
            match c {
                Clause::Return(r) => ret = Some(r.clone()),
                other => clauses.push(other.clone()),
            }
        }
        let r = ret?;
        let items = match yields {
            Some(ys) if !ys.is_empty() => {
                // project the YIELD list, renaming aliased items
                let mut filtered = Vec::with_capacity(ys.len());
                for y in &ys {
                    // match by projected alias / source name
                    let item = r.items.iter().find(|it| match &it.alias {
                        Some(a) => a == &y.name,
                        None => matches!(&it.expr, ast::Expression::Variable { name, .. } if name == &y.name),
                    })?;
                    let mut new_item = item.clone();
                    if let Some(a) = &y.alias {
                        new_item.alias = Some(a.clone());
                    }
                    filtered.push(new_item);
                }
                filtered
            }
            // YIELD * (none given): every column
            _ => r.items.clone(),
        };
        clauses.push(Clause::With(WithClause {
            distinct: r.distinct,
            items,
            where_clause: None,
            order_by: r.order_by.clone(),
            skip: r.skip.clone(),
            limit: r.limit.clone(),
            span: r.span,
        }));
    }
    // wrap as a synthetic QueryStatement keeping the final statement's UNIONs
    let last = queries.last().unwrap();
    let span_end = last.span.end;
    Some(Statement::Query(ast::QueryStatement {
        head: ast::QueryBody {
            clauses,
            span: Span::new(span_start, span_end),
        },
        unions: last.unions.clone(),
        span: Span::new(span_start, span_end),
    }))
}

/// ISO section 14.5 path mode governing the whole program; the caller installs a thread-local guard with
/// it before running the lowered statement, so the matcher applies the right edge / node
/// uniqueness. The first non-default mode of any `MATCH` wins, exact for single-mode programs;
/// per-MATCH scoping for mixed modes is a known limitation (see `CONFORMANCE.md`).
pub fn dominant_path_mode(program: &ast::Program) -> cy::PathMode {
    let mut mode = cy::PathMode::Trail;
    for stmt in &program.statements {
        if let ast::Statement::Query(q) = stmt {
            visit_query_path_modes(q, &mut mode);
        }
    }
    mode
}

/// Per-binding selector limit (e.g. `MATCH ANY 3 (...)`). `Some(n)`: append a top-level `LIMIT n`
/// to the lowered statement so each outer binding yields at most `n` rows. `None` without an `ANY`
/// / `ANY k` selector.
pub fn dominant_path_selector_limit(program: &ast::Program) -> Option<usize> {
    let mut limit: Option<usize> = None;
    for stmt in &program.statements {
        if let ast::Statement::Query(q) = stmt {
            visit_query_selector_limits(q, &mut limit);
        }
    }
    limit
}

fn visit_query_path_modes(q: &ast::QueryStatement, mode: &mut cy::PathMode) {
    visit_body_path_modes(&q.head, mode);
    for u in &q.unions {
        visit_body_path_modes(&u.body, mode);
    }
}

fn visit_body_path_modes(body: &QueryBody, mode: &mut cy::PathMode) {
    // first non-default mode wins; exact for single-mode programs, mixed modes would need per-MATCH
    // plan scoping (known limitation in CONFORMANCE.md, rare in practice)
    for c in &body.clauses {
        if let ast::Clause::Match(m) = c {
            let m_mode = match m.path_mode {
                ast::PathMode::Walk => cy::PathMode::Walk,
                ast::PathMode::Acyclic => cy::PathMode::Acyclic,
                ast::PathMode::Default | ast::PathMode::Trail => cy::PathMode::Trail,
            };
            if matches!(*mode, cy::PathMode::Trail) && !matches!(m_mode, cy::PathMode::Trail) {
                *mode = m_mode;
            }
        }
    }
}

fn visit_query_selector_limits(q: &ast::QueryStatement, limit: &mut Option<usize>) {
    visit_body_selector_limits(&q.head, limit);
    for u in &q.unions {
        visit_body_selector_limits(&u.body, limit);
    }
}

fn visit_body_selector_limits(body: &QueryBody, limit: &mut Option<usize>) {
    for c in &body.clauses {
        if let ast::Clause::Match(m) = c {
            // `SHORTEST k` is per endpoint pair (up to k paths per (source, target)), so the cap
            // belongs on the path operator: the pattern lowerer passes `k` as `Counted(k)` and it's
            // skipped here. `ANY` / `ANY k` pick any k *result* paths regardless of endpoints:
            // exactly a top-level LIMIT k.
            let sel = match &m.path_selector {
                ast::PathSelector::Any => Some(1usize),
                ast::PathSelector::AnyN(n) => Some((*n).max(1) as usize),
                _ => None,
            };
            if let Some(n) = sel {
                *limit = Some(match *limit {
                    Some(prev) => prev.min(n),
                    None => n,
                });
            }
        }
    }
}

/// Attaches a top-level `LIMIT n` to every terminating `RETURN` (head and unions). A user-written
/// `LIMIT` wins: only empty slots are filled, so a user projection is never over-restricted.
pub fn apply_path_selector_limit(stmt: &mut cy::Statement, n: usize) {
    if let cy::Query::Regular(ref mut rq) = stmt.query {
        apply_limit_to_single(&mut rq.single, n);
        for u in &mut rq.unions {
            apply_limit_to_single(&mut u.query, n);
        }
    }
}

fn apply_limit_to_single(single: &mut cy::SingleQuery, n: usize) {
    if let Some(ref mut rc) = single.final_part.return_clause
        && rc.limit.is_none()
    {
        rc.limit = Some(cy::Expr::Literal(cy::Literal::Integer(n as i64)));
    }
}

fn lower_statement(stmt: &ast::Statement) -> Result<cy::Statement, ParseError> {
    match stmt {
        ast::Statement::Query(q) => {
            let query = lower_query_statement(q)?;
            Ok(cy::Statement {
                mode: cy::QueryMode::Normal,
                query,
            })
        }
        ast::Statement::CreateIndex {
            name,
            label,
            property,
            ..
        } => {
            Ok(cy::Statement {
                mode: cy::QueryMode::Normal,
                query: cy::Query::CreateIndex {
                    extra_properties: Vec::new(),
                    // the executor needs an index name; derive one from label+property if none was
                    // given
                    name: name
                        .clone()
                        .unwrap_or_else(|| format!("idx_{label}_{property}")),
                    label: label.clone(),
                    property: property.clone(),
                },
            })
        }
        ast::Statement::DropIndex { name, .. } => Ok(cy::Statement {
            mode: cy::QueryMode::Normal,
            query: cy::Query::DropIndex { name: name.clone() },
        }),
        ast::Statement::CreateConstraint {
            name,
            label,
            property,
            kind,
            ..
        } => {
            Ok(cy::Statement {
                mode: cy::QueryMode::Normal,
                query: cy::Query::CreateConstraint {
                    name: name
                        .clone()
                        .unwrap_or_else(|| format!("cons_{label}_{property}")),
                    label: label.clone(),
                    property: property.clone(),
                    extra_properties: Vec::new(),
                    constraint_type: match kind {
                        ConstraintKind::Unique => "UNIQUE".into(),
                        ConstraintKind::NotNull => "NOT NULL".into(),
                    },
                    // GQL constraint DDL is node-only here
                    is_relationship: false,
                    if_not_exists: false,
                },
            })
        }
        ast::Statement::DropConstraint { name, .. } => Ok(cy::Statement {
            mode: cy::QueryMode::Normal,
            query: cy::Query::DropConstraint { name: name.clone() },
        }),
        ast::Statement::CreateGraph { span, .. }
        | ast::Statement::DropGraph { span, .. }
        | ast::Statement::CreateGraphType { span, .. }
        | ast::Statement::DropGraphType { span, .. }
        | ast::Statement::CreateSchema { span, .. }
        | ast::Statement::DropSchema { span, .. }
        | ast::Statement::Use { span, .. } => Err(not_implemented(
            *span,
            "graph catalog statements (CREATE/DROP GRAPH, GRAPH TYPE, SCHEMA, USE) \
             are not yet wired through to the executor",
        )),
        ast::Statement::Transaction { span, .. } => Err(not_implemented(
            *span,
            "a transaction block (START TRANSACTION ... COMMIT) has no Cypher form: it runs as a \
             statement of its own, not inside a NEXT chain or a multi-statement script",
        )),
        ast::Statement::When { span, .. } => Err(not_implemented(
            *span,
            "a `WHEN ... THEN ...` statement has no Cypher form: it runs as a statement of its own, \
             not inside a NEXT chain or a multi-statement script",
        )),
    }
}

fn lower_query_statement(q: &ast::QueryStatement) -> Result<cy::Query, ParseError> {
    let single = lower_query_body(&q.head)?;
    let unions = q
        .unions
        .iter()
        .map(lower_union_part)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(cy::Query::Regular(cy::RegularQuery { single, unions }))
}

fn lower_union_part(u: &ast::UnionPart) -> Result<cy::Union, ParseError> {
    Ok(cy::Union {
        all: matches!(u.kind, ast::UnionKind::All),
        query: lower_query_body(&u.body)?,
    })
}

/// Walks a GQL query body, splitting on `WITH` into Cypher [`cy::QueryPart`]s and a terminating
/// [`cy::FinalPart`].
#[recursive::recursive]
fn lower_query_body(body: &QueryBody) -> Result<cy::SingleQuery, ParseError> {
    let mut parts = Vec::new();
    let mut current: Vec<cy::Clause> = Vec::new();
    let mut return_clause = None;
    // variables in scope; LET lowering builds an explicit pass-through (`WITH a, b, ..., expr AS
    // name`) from them, since the executor has no `WITH *, expr AS x` shape
    let mut in_scope: Vec<String> = Vec::new();

    for clause in &body.clauses {
        match clause {
            ast::Clause::Match(m) => {
                for name in collect_match_variables(m) {
                    if !in_scope.contains(&name) {
                        in_scope.push(name);
                    }
                }
                current.push(lower_clause(clause)?);
            }
            ast::Clause::With(w) => {
                let with = lower_with(w)?;
                // WITH resets scope to its aliases
                in_scope = collect_with_aliases(w);
                parts.push(cy::QueryPart {
                    clauses: std::mem::take(&mut current),
                    with,
                });
            }
            ast::Clause::Return(r) => {
                return_clause = Some(lower_return(r)?);
                // RETURN ends the body
                break;
            }
            ast::Clause::Finish(_) => {
                // FINISH ends a write-only query with no rows: stop collecting clauses
                break;
            }
            ast::Clause::Call(c) => {
                // YIELD items bring their (maybe aliased) names into scope
                for y in &c.yields {
                    let name = y.alias.clone().unwrap_or_else(|| y.field.clone());
                    if !in_scope.contains(&name) {
                        in_scope.push(name);
                    }
                }
                current.push(cy::Clause::Call(lower_call_clause(c)?));
            }
            ast::Clause::CallSubquery(c) => {
                // lower the body recursively into a Cypher `CallSubquery`; the planner re-plans it
                // and `CallSubqueryOp` runs it once per outer row via `exec_subquery`, then
                // cross-joins
                let body = lower_query_body(&c.body)?;
                // the body's RETURN aliases extend the outer record (ISO section 9.4)
                if let Some(ret) = &body.final_part.return_clause
                    && let cy::ProjectionItems::Explicit(items) = &ret.items
                {
                    for it in items {
                        let name = if let Some(a) = &it.alias {
                            a.clone()
                        } else if let cy::Expr::Variable(v) = &it.expr {
                            v.clone()
                        } else {
                            continue;
                        };
                        if !in_scope.contains(&name) {
                            in_scope.push(name);
                        }
                    }
                }
                current.push(cy::Clause::CallSubquery(cy::CallSubqueryClause {
                    imports: c.imports.clone(),
                    body: Box::new(cy::RegularQuery {
                        single: body,
                        unions: Vec::new(),
                    }),
                    in_transactions: None,
                }));
            }
            ast::Clause::For(f) => {
                // `FOR x IN list` == `UNWIND list AS x` (one row per element), so no dedicated FOR
                // operator
                current.push(cy::Clause::Unwind(cy::UnwindClause {
                    expr: lower_expr(&f.list)?,
                    alias: f.variable.clone(),
                }));
                if !in_scope.contains(&f.variable) {
                    in_scope.push(f.variable.clone());
                }
            }
            ast::Clause::Unwind(u) => {
                current.push(lower_clause(clause)?);
                if !in_scope.contains(&u.alias) {
                    in_scope.push(u.alias.clone());
                }
            }
            ast::Clause::Filter(f) => {
                // `FILTER [WHERE] pred` has no Cypher clause; `WITH * WHERE pred` is equivalent (a
                // part split passing everything through, with the predicate), which the planner
                // handles well
                parts.push(cy::QueryPart {
                    clauses: std::mem::take(&mut current),
                    with: cy::WithClause {
                        distinct: false,
                        items: cy::ProjectionItems::All,
                        order_by: None,
                        skip: None,
                        limit: None,
                        where_clause: Some(lower_expr(&f.predicate)?),
                    },
                });
            }
            ast::Clause::Let(l) => {
                // GQL `LET a = e1, b = e2, ...` binds **sequentially** (`LET a = 1, b = a + 1` gives
                // `b = 2`), while one Cypher `WITH ... AS a, ... AS b` evaluates against the *prior*
                // scope and would leave `b` null; so one pass-through-and-extend part split per
                // binding
                let mut clauses_for_first = std::mem::take(&mut current);
                for (name, expr) in &l.bindings {
                    let mut items: Vec<cy::ProjectionItem> = in_scope
                        .iter()
                        .map(|v| cy::ProjectionItem {
                            expr: cy::Expr::Variable(v.clone()),
                            alias: None,
                            source_text: None,
                        })
                        .collect();
                    items.push(cy::ProjectionItem {
                        expr: lower_expr(expr)?,
                        alias: Some(name.clone()),
                        source_text: None,
                    });
                    parts.push(cy::QueryPart {
                        clauses: std::mem::take(&mut clauses_for_first),
                        with: cy::WithClause {
                            distinct: false,
                            items: cy::ProjectionItems::Explicit(items),
                            order_by: None,
                            skip: None,
                            limit: None,
                            where_clause: None,
                        },
                    });
                    if !in_scope.contains(name) {
                        in_scope.push(name.clone());
                    }
                }
            }
            other => current.push(lower_clause(other)?),
        }
    }

    Ok(cy::SingleQuery {
        parts,
        final_part: cy::FinalPart {
            clauses: current,
            return_clause,
        },
    })
}

/// Every variable a MATCH clause's patterns bind (path, node, edge), for LET's pass-through WITH.
#[recursive::recursive]
fn collect_inner_path_variables(p: &ast::PathPattern, out: &mut Vec<String>) {
    if let Some(v) = &p.start.variable {
        out.push(v.clone());
    }
    for elem in &p.chain {
        match elem {
            ast::PathElement::Step { edge, node } => {
                if let Some(v) = &edge.variable {
                    out.push(v.clone());
                }
                if let Some(v) = &node.variable {
                    out.push(v.clone());
                }
            }
            ast::PathElement::Group { sub, .. } => {
                collect_inner_path_variables(sub, out);
            }
        }
    }
}

fn collect_match_variables(m: &ast::MatchClause) -> Vec<String> {
    let mut out = Vec::new();
    for p in &m.patterns {
        if let Some(v) = &p.variable {
            out.push(v.clone());
        }
        if let Some(v) = &p.start.variable {
            out.push(v.clone());
        }
        for elem in &p.chain {
            match elem {
                ast::PathElement::Step { edge, node } => {
                    if let Some(v) = &edge.variable {
                        out.push(v.clone());
                    }
                    if let Some(v) = &node.variable {
                        out.push(v.clone());
                    }
                }
                ast::PathElement::Group { sub, .. } => {
                    // variables inside the quantified inner pattern are in scope after the match
                    collect_inner_path_variables(sub, &mut out);
                }
            }
        }
    }
    out
}

/// Alias names a WITH clause projects (the variable name when there's no alias).
fn collect_with_aliases(w: &ast::WithClause) -> Vec<String> {
    w.items
        .iter()
        .filter_map(|item| match &item.alias {
            Some(a) => Some(a.clone()),
            None => match &item.expr {
                Expression::Variable { name, .. } => Some(name.clone()),
                _ => None,
            },
        })
        .collect()
}

fn lower_clause(c: &ast::Clause) -> Result<cy::Clause, ParseError> {
    match c {
        ast::Clause::Match(m) => {
            let mc = lower_match_body(m)?;
            Ok(if m.optional {
                cy::Clause::OptionalMatch(mc)
            } else {
                cy::Clause::Match(mc)
            })
        }
        ast::Clause::Unwind(u) => Ok(cy::Clause::Unwind(cy::UnwindClause {
            expr: lower_expr(&u.expr)?,
            alias: u.alias.clone(),
        })),
        ast::Clause::Insert(i) => Ok(cy::Clause::Create(cy::CreateClause {
            pattern: lower_pattern(&i.patterns)?,
        })),
        ast::Clause::Set(s) => Ok(cy::Clause::Set(cy::SetClause {
            items: s
                .items
                .iter()
                .map(lower_set_item)
                .collect::<Result<_, _>>()?,
        })),
        ast::Clause::Remove(r) => Ok(cy::Clause::Remove(cy::RemoveClause {
            items: r
                .items
                .iter()
                .map(lower_remove_item)
                .collect::<Result<_, _>>()?,
        })),
        ast::Clause::Delete(d) => Ok(cy::Clause::Delete(cy::DeleteClause {
            detach: d.detach,
            exprs: d.targets.iter().map(lower_expr).collect::<Result<_, _>>()?,
        })),
        ast::Clause::Call(c) => Ok(cy::Clause::Call(lower_call_clause(c)?)),
        ast::Clause::CallSubquery(c) => {
            // lowered subquery body for the planner to re-plan; the body's RETURN alias scoping
            // lives in `lower_query_body`'s splitting walk, this arm only builds the clause when
            // `lower_clause` is called directly (unit tests)
            Ok(cy::Clause::CallSubquery(cy::CallSubqueryClause {
                imports: c.imports.clone(),
                body: Box::new(cy::RegularQuery {
                    single: lower_query_body(&c.body)?,
                    unions: Vec::new(),
                }),
                in_transactions: None,
            }))
        }
        ast::Clause::With(_)
        | ast::Clause::Return(_)
        | ast::Clause::Finish(_)
        | ast::Clause::For(_)
        | ast::Clause::Filter(_)
        | ast::Clause::Let(_) => {
            // handled by the body-splitting walk, never reach `lower_clause`
            unreachable!("body splitter handles {c:?} before reaching lower_clause");
        }
    }
}

fn lower_call_clause(c: &ast::CallClause) -> Result<cy::CallClause, ParseError> {
    // the GQL parser always produces an explicit (maybe empty) argument list, i.e. Cypher's
    // `Some(vec![])`; `None` ("implicit args from query parameters") doesn't exist in GQL
    let args = Some(c.args.iter().map(lower_expr).collect::<Result<_, _>>()?);
    let yield_items = if c.yields.is_empty() {
        None
    } else {
        Some(cy::YieldItems::Explicit(
            c.yields
                .iter()
                .map(|y| cy::YieldItem {
                    name: y.field.clone(),
                    alias: y.alias.clone(),
                })
                .collect(),
        ))
    };
    Ok(cy::CallClause {
        procedure_name: c.name.join("."),
        args,
        yield_items,
    })
}

fn lower_match_body(m: &ast::MatchClause) -> Result<cy::MatchClause, ParseError> {
    // ISO section 14.5 path modes: TRAIL keeps edge uniqueness, WALK relaxes it (edges may repeat),
    // ACYCLIC tightens it (nodes may not repeat); REPEATABLE ELEMENTS is WALK and lexes to the same
    // `PathMode::Walk`
    let path_mode = match m.path_mode {
        ast::PathMode::Default | ast::PathMode::Trail => cy::PathMode::Trail,
        ast::PathMode::Walk => cy::PathMode::Walk,
        ast::PathMode::Acyclic => cy::PathMode::Acyclic,
    };
    // ISO section 14.6 path selectors: the ones that map onto `shortestPath` / `allShortestPaths` lower to
    // that; ANY / ANY k / SHORTEST k / SHORTEST k GROUPS lower to `path_selector_limit` on each
    // pattern part so the executor caps rows
    let mut shortest = cy::ShortestPathMode::None;
    let mut selector_limit: Option<usize> = None;
    match &m.path_selector {
        ast::PathSelector::All => {}
        ast::PathSelector::Shortest | ast::PathSelector::AnyShortest => {
            shortest = cy::ShortestPathMode::Single;
        }
        ast::PathSelector::AllShortest => {
            shortest = cy::ShortestPathMode::All;
        }
        ast::PathSelector::Any => {
            selector_limit = Some(1);
        }
        ast::PathSelector::AnyN(n) => {
            selector_limit = Some((*n).max(1) as usize);
        }
        ast::PathSelector::ShortestN(n) => {
            // `Counted(k)`: Yen-style BFS in `bfs_k_shortest`, at most k paths per (source, target)
            // by ascending length, filling from the next length tier when the shortest has fewer
            // than k; no top-level LIMIT, SHORTEST k is per pair
            shortest = cy::ShortestPathMode::Counted((*n).max(1) as usize);
        }
        ast::PathSelector::ShortestGroups(n) => {
            // `CountedGroups(k)`: length-tier DFS enumeration in `bfs_k_groups`, every loopless
            // path whose length is among the k smallest distinct lengths between (source, target);
            // `k = 1` is ALL SHORTEST through the same operator
            shortest = cy::ShortestPathMode::CountedGroups((*n).max(1) as usize);
        }
    }
    let mut pattern = lower_pattern(&m.patterns)?;
    for part in &mut pattern.parts {
        part.path_mode = path_mode;
        if !matches!(shortest, cy::ShortestPathMode::None) {
            part.shortest_path = shortest.clone();
        }
        part.path_selector_limit = selector_limit;
    }
    // ISO section 16.7 parenthesized path patterns (G050 / G051): inner WHERE predicates are AND-ed with
    // the MATCH-level WHERE, one filter pass
    let mut combined_where = m.where_clause.as_ref().map(lower_expr).transpose()?;
    for pp in &m.patterns {
        if let Some(inner) = &pp.inner_where {
            let inner_cy = lower_expr(inner)?;
            combined_where = Some(match combined_where {
                None => inner_cy,
                Some(prev) => {
                    cy::Expr::BinaryOp(Box::new(prev), cy::BinOp::And, Box::new(inner_cy))
                }
            });
        }
    }
    Ok(cy::MatchClause {
        pattern,
        where_clause: combined_where,
        hints: Vec::new(),
        search: None,
    })
}

fn lower_with(w: &ast::WithClause) -> Result<cy::WithClause, ParseError> {
    Ok(cy::WithClause {
        distinct: w.distinct,
        items: cy::ProjectionItems::Explicit(
            w.items
                .iter()
                .map(lower_return_item)
                .collect::<Result<_, _>>()?,
        ),
        order_by: lower_order_by(&w.order_by)?,
        skip: w.skip.as_ref().map(lower_expr).transpose()?,
        limit: w.limit.as_ref().map(lower_expr).transpose()?,
        where_clause: w.where_clause.as_ref().map(lower_expr).transpose()?,
    })
}

fn lower_return(r: &ast::ReturnClause) -> Result<cy::ReturnClause, ParseError> {
    Ok(cy::ReturnClause {
        distinct: r.distinct,
        items: cy::ProjectionItems::Explicit(
            r.items
                .iter()
                .map(lower_return_item)
                .collect::<Result<_, _>>()?,
        ),
        order_by: lower_order_by(&r.order_by)?,
        skip: r.skip.as_ref().map(lower_expr).transpose()?,
        limit: r.limit.as_ref().map(lower_expr).transpose()?,
    })
}

fn lower_return_item(item: &ast::ReturnItem) -> Result<cy::ProjectionItem, ParseError> {
    Ok(cy::ProjectionItem {
        expr: lower_expr(&item.expr)?,
        alias: item.alias.clone(),
        source_text: None,
    })
}

fn lower_order_by(items: &[ast::OrderItem]) -> Result<Option<Vec<cy::SortItem>>, ParseError> {
    if items.is_empty() {
        return Ok(None);
    }
    items
        .iter()
        .map(|o| {
            Ok(cy::SortItem {
                expr: lower_expr(&o.expr)?,
                ascending: matches!(o.direction, SortDirection::Asc),
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn lower_set_item(item: &ast::SetItem) -> Result<cy::SetItem, ParseError> {
    match item {
        ast::SetItem::Property {
            target,
            value,
            span,
        } => {
            // `n.prop` target -> PropertyAccess; bare-variable `n = map` -> AllProperties
            match target {
                Expression::Property { base, name, .. } => {
                    let var = expr_to_variable_name(base, *span)?;
                    Ok(cy::SetItem::Property(
                        cy::PropertyAccess {
                            expr_or_var: var,
                            key: name.clone(),
                        },
                        lower_expr(value)?,
                    ))
                }
                Expression::Variable { name, .. } => {
                    Ok(cy::SetItem::AllProperties(name.clone(), lower_expr(value)?))
                }
                _ => Err(not_implemented(
                    *span,
                    "complex SET targets are not yet lowered (only `n.prop = ...` and `n = ...`)",
                )),
            }
        }
        ast::SetItem::Merge {
            target,
            value,
            span,
        } => {
            let var = expr_to_variable_name(target, *span)?;
            Ok(cy::SetItem::MergeProperties(var, lower_expr(value)?))
        }
        ast::SetItem::Labels {
            target,
            labels,
            span,
        } => {
            let var = expr_to_variable_name(target, *span)?;
            Ok(cy::SetItem::Label(var, labels.clone(), Vec::new()))
        }
    }
}

fn lower_remove_item(item: &ast::RemoveItem) -> Result<cy::RemoveItem, ParseError> {
    match item {
        ast::RemoveItem::Property { target, span } => match target {
            Expression::Property { base, name, .. } => {
                let var = expr_to_variable_name(base, *span)?;
                Ok(cy::RemoveItem::Property(cy::PropertyAccess {
                    expr_or_var: var,
                    key: name.clone(),
                }))
            }
            _ => Err(not_implemented(
                *span,
                "REMOVE only supports `n.prop` and `n:Label` forms",
            )),
        },
        ast::RemoveItem::Labels {
            target,
            labels,
            span,
        } => {
            let var = expr_to_variable_name(target, *span)?;
            Ok(cy::RemoveItem::Label(var, labels.clone(), Vec::new()))
        }
    }
}

fn expr_to_variable_name(e: &Expression, span: Span) -> Result<String, ParseError> {
    match e {
        Expression::Variable { name, .. } => Ok(name.clone()),
        _ => Err(not_implemented(
            span,
            "expected a bare variable name in this position",
        )),
    }
}

// -- patterns -------------------------------------------------------

fn lower_pattern(patterns: &[ast::PathPattern]) -> Result<cy::Pattern, ParseError> {
    let parts = patterns
        .iter()
        .map(lower_pattern_part)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(cy::Pattern { parts })
}

/// One ISO section 16.11 group-variable list binding produced by unrolling a
/// multi-step quantified group: `var` binds to the LIST of `members`
/// (synthesized per-iteration slot names). `prefix_anchor` marks the inner
/// START variable, whose iteration-0 value is the node PRECEDING the group -
/// [`lower_pattern_part`] prepends that anchor's name (synthesizing one when
/// the anchor is anonymous).
struct QppListGroup {
    var: String,
    members: Vec<String>,
    prefix_anchor: bool,
}

/// Fresh id for synthesized QPP unroll names - process-wide so unrolled
/// groups in different clauses never collide.
fn qpp_fresh_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

fn lower_pattern_part(p: &ast::PathPattern) -> Result<cy::PatternPart, ParseError> {
    let mut node = lower_node_pattern(&p.start)?;
    let mut chain: Vec<cy::PatternChain> = Vec::new();
    let mut qpp_list_groups: Vec<(String, Vec<String>)> = Vec::new();
    for elem in &p.chain {
        let (elem_chains, groups) = lower_path_element(elem)?;
        for g in groups {
            let mut members = g.members;
            if g.prefix_anchor {
                // The group's iteration-0 start is the current chain tail -
                // name it if anonymous so the list can reference it.
                let tail_var: &mut Option<String> = match chain.last_mut() {
                    Some(last) => &mut last.node.variable,
                    None => &mut node.variable,
                };
                let name = tail_var
                    .get_or_insert_with(|| format!("__qppa_{}", qpp_fresh_id()))
                    .clone();
                members.insert(0, name);
            }
            qpp_list_groups.push((g.var, members));
        }
        chain.extend(elem_chains);
    }
    Ok(cy::PatternPart {
        // GQL section 16.9 path variable: `p = (a)-[r]->(b)` binds `p` to the whole path, carried on
        // PatternPart.variable; the executor materializes a PATH value
        variable: p.variable.clone(),
        element: cy::PatternElement { node, chain },
        shortest_path: cy::ShortestPathMode::None,
        weight_property: None,
        total_cost_var: None,
        path_mode: cy::PathMode::Trail,
        qpp_list_groups,
        path_selector_limit: None,
    })
}

/// Lowers one [`ast::PathElement`] into the Cypher AST's flat chain. A `Step` is one
/// `PatternChain`. A `Group` with a single inner step becomes an edge-level quantifier (`*m..n`); a
/// multi-step group with an exact count `{n}` is unrolled n times. Forms the executor can't express
/// (inner WHERE, constrained inner start) get a clear error at the source span.
fn lower_path_element(
    elem: &ast::PathElement,
) -> Result<(Vec<cy::PatternChain>, Vec<QppListGroup>), ParseError> {
    match elem {
        ast::PathElement::Step { edge, node } => Ok((
            vec![cy::PatternChain {
                rel: lower_edge_pattern(edge)?,
                node: lower_node_pattern(node)?,
            }],
            Vec::new(),
        )),
        ast::PathElement::Group {
            sub,
            quantifier,
            span,
        } => {
            // no inner WHERE on a quantified group
            if sub.inner_where.is_some() {
                return Err(not_implemented(
                    *span,
                    "quantified path patterns with an inner `WHERE` are \
                     not yet supported by the executor - split the \
                     predicate out into the surrounding `MATCH ... WHERE`",
                ));
            }
            // multi-step inner bodies: exact count `((sub)){n}` (min == max) unrolls `sub.chain` n
            // times with anonymized intermediates; single-step inner uses the edge-quantifier
            // desugaring
            if sub.chain.len() != 1 {
                let n = match (quantifier.min, quantifier.max) {
                    (m, Some(mx)) if m == mx && m >= 1 => m,
                    _ => {
                        // `expand_range_quantifiers` turns every non-exact multi-step quantifier
                        // into exact-count UNION ALL arms beforehand, so getting here is an
                        // invariant violation, not a user limitation
                        return Err(not_implemented(
                            *span,
                            "internal: a non-exact multi-step quantified \
                             group reached `lower_path_element` after range \
                             expansion - please file a bug.",
                        ));
                    }
                };
                // inner-start labels / property maps would have to hold at every iteration
                // boundary, which unrolling doesn't model: rejected (a bare inner-start variable
                // becomes a group list below)
                if sub.start.label_expr.is_some() || sub.start.properties.is_some() {
                    return Err(not_implemented(
                        *span,
                        "the inner start node of a quantified multi-step \
                         group can't carry a label or property constraint \
                         - anchor those on the outer chain's preceding \
                         node instead.",
                    ));
                }
                // ISO section 16.11 group variables: every user-named interior
                // variable becomes a LIST with one entry per iteration. The
                // unroll keeps synthesized per-iteration slot names and the
                // Cypher planner rebuilds the lists (BindVarLists) from the
                // groupings recorded on the pattern part. The inner START
                // variable's iteration-0 entry is the node preceding the
                // group - marked `prefix_anchor` for `lower_pattern_part`.
                let gid = qpp_fresh_id();
                let steps = sub.chain.len();
                let mut chains: Vec<cy::PatternChain> = Vec::with_capacity(n * steps);
                let mut groups: Vec<QppListGroup> = Vec::new();
                let push_member =
                    |groups: &mut Vec<QppListGroup>, var: &str, member: String| match groups
                        .iter_mut()
                        .find(|g| g.var == var)
                    {
                        Some(g) => g.members.push(member),
                        None => groups.push(QppListGroup {
                            var: var.to_string(),
                            members: vec![member],
                            prefix_anchor: false,
                        }),
                    };
                let start_var = sub.start.variable.clone();
                for iter in 0..n {
                    for (step_idx, elem) in sub.chain.iter().enumerate() {
                        let (inner_edge, inner_node) = match elem {
                            ast::PathElement::Step { edge, node } => (edge, node),
                            ast::PathElement::Group { .. } => {
                                return Err(not_implemented(
                                    *span,
                                    "nested quantified path patterns aren't supported.",
                                ));
                            }
                        };
                        if inner_edge.quantifier.is_some() {
                            return Err(not_implemented(
                                *span,
                                "inner edges of a quantified multi-step \
                                 group can't carry their own `*m..n` \
                                 quantifier - drop one of the two.",
                            ));
                        }
                        let mut lowered_edge = lower_edge_pattern(inner_edge)?;
                        let mut lowered_node = lower_node_pattern(inner_node)?;
                        let is_last_step = step_idx == steps - 1;

                        // Edge variable -> per-iteration synthesized slot.
                        if let Some(ev) = lowered_edge.variable.take() {
                            let synth = format!("__qppg{gid}_{ev}_{iter}");
                            push_member(&mut groups, &ev, synth.clone());
                            lowered_edge.variable = Some(synth);
                        }
                        // Node variable -> per-iteration synthesized slot.
                        // The LAST node of iteration i doubles as the START
                        // of iteration i+1, so it may join two lists under
                        // one slot name.
                        let node_user_var = lowered_node.variable.take();
                        let feeds_next_start = is_last_step && iter + 1 < n && start_var.is_some();
                        if node_user_var.is_some() || feeds_next_start {
                            let base = node_user_var
                                .clone()
                                .or_else(|| start_var.clone())
                                .expect("named by one of the branches");
                            let synth = format!("__qppg{gid}_{base}_{iter}");
                            if let Some(ref v) = node_user_var {
                                push_member(&mut groups, v, synth.clone());
                            }
                            if feeds_next_start {
                                push_member(
                                    &mut groups,
                                    start_var.as_deref().expect("checked"),
                                    synth.clone(),
                                );
                            }
                            lowered_node.variable = Some(synth);
                        }
                        chains.push(cy::PatternChain {
                            rel: lowered_edge,
                            node: lowered_node,
                        });
                    }
                }
                // The inner-start list needs the anchor node prepended as
                // its iteration-0 entry.
                if let Some(ref sv) = start_var {
                    match groups.iter_mut().find(|g| g.var == *sv) {
                        Some(g) => g.prefix_anchor = true,
                        None => groups.push(QppListGroup {
                            var: sv.clone(),
                            members: Vec::new(),
                            prefix_anchor: true,
                        }),
                    }
                }
                return Ok((chains, groups));
            }
            let inner_step = match &sub.chain[0] {
                ast::PathElement::Step { edge, node } => (edge, node),
                ast::PathElement::Group { .. } => {
                    return Err(not_implemented(
                        *span,
                        "nested quantified path patterns aren't supported",
                    ));
                }
            };
            let (inner_edge, inner_target) = inner_step;
            if inner_edge.quantifier.is_some() {
                return Err(not_implemented(
                    *span,
                    "a quantified path group whose inner edge *also* \
                     carries a `*m..n` quantifier is ambiguous - drop \
                     one of the quantifiers",
                ));
            }
            // desugar `((x)-[r]->(y)){m,n}` into the single-edge chain `-[r*m..n]->(boundary)` from
            // the outer start. Per ISO section 16.11 `x` and `y` are GROUP variables (per-iteration node
            // lists) whose label/property constraints hold at every iteration: the inner node
            // patterns ride along in `qpp_group` and the Cypher planner emits the list bindings +
            // all() filters. The chain's own endpoint is an anonymous boundary node (the group's
            // exit).
            let mut lowered_edge = lower_edge_pattern(inner_edge)?;
            // RelPattern `range: Option<(Option<u32>, Option<u32>)>` for `*m..n`: outer `None` =
            // fixed length 1; `(Some(m), Some(n))` bounded; `(Some(m), None)` unbounded above
            let cap = quantifier.max.map(|n| n as u32);
            lowered_edge.range = Some((Some(quantifier.min as u32), cap));
            let inner_start_cy = lower_node_pattern(&sub.start)?;
            let inner_end_cy = lower_node_pattern(inner_target)?;
            if inner_start_cy.variable.is_some()
                || inner_end_cy.variable.is_some()
                || !inner_start_cy.labels.is_empty()
                || !inner_end_cy.labels.is_empty()
                || inner_start_cy.properties.is_some()
                || inner_end_cy.properties.is_some()
            {
                lowered_edge.qpp_group = Some(Box::new(cy::QppGroupInner {
                    start: inner_start_cy,
                    end: inner_end_cy,
                }));
            }
            Ok((
                vec![cy::PatternChain {
                    rel: lowered_edge,
                    node: cy::NodePattern {
                        variable: None,
                        labels: Vec::new(),
                        dynamic_labels: Vec::new(),
                        properties: None,
                        where_pred: None,
                    },
                }],
                Vec::new(),
            ))
        }
    }
}

fn lower_node_pattern(n: &ast::NodePattern) -> Result<cy::NodePattern, ParseError> {
    let labels = match &n.label_expr {
        None => Vec::new(),
        Some(le) => flatten_conjunctive_labels(le)?,
    };
    let properties = n.properties.as_ref().map(lower_property_map).transpose()?;
    Ok(cy::NodePattern {
        variable: n.variable.clone(),
        labels,
        dynamic_labels: Vec::new(),
        properties,
        where_pred: None,
    })
}

fn lower_edge_pattern(e: &ast::EdgePattern) -> Result<cy::RelPattern, ParseError> {
    let types = match &e.label_expr {
        None => Vec::new(),
        // Cypher edge `types` is a flat list the planner treats as a disjunction (`:KNOWS|:LIKES`),
        // so a single Label or a top-level Disjunction of labels lowers directly
        Some(LabelExpression::Label { name, .. }) => vec![name.clone()],
        Some(LabelExpression::Disjunction { operands, span }) => {
            let mut names = Vec::with_capacity(operands.len());
            for op in operands {
                match op {
                    LabelExpression::Label { name, .. } => names.push(name.clone()),
                    _ => {
                        return Err(not_implemented(
                            *span,
                            "edge-type disjunctions must contain plain type names only",
                        ));
                    }
                }
            }
            names
        }
        Some(other) => {
            return Err(not_implemented(
                other.span(),
                "edge label expressions beyond a single type or flat disjunction are not yet lowered",
            ));
        }
    };
    let direction = match e.direction {
        EdgeDirection::Outgoing => cy::RelDirection::Outgoing,
        EdgeDirection::Incoming => cy::RelDirection::Incoming,
        EdgeDirection::Undirected => cy::RelDirection::Both,
    };
    let range = e.quantifier.map(|q| (q.min, q.max));
    let properties = e.properties.as_ref().map(lower_property_map).transpose()?;
    Ok(cy::RelPattern {
        variable: e.variable.clone(),
        direction,
        types,
        dynamic_types: Vec::new(),
        range,
        properties,
        wshortest_weight: None,
        wshortest_cost_var: None,
        where_pred: None,
        qpp_group: None,
    })
}

/// Flattens a single label or a conjunction of single labels (Cypher's `labels: Vec<String>`
/// already conjoins); disjunction, negation and wildcard return `NotImplemented`.
fn flatten_conjunctive_labels(le: &LabelExpression) -> Result<Vec<String>, ParseError> {
    match le {
        LabelExpression::Label { name, .. } => Ok(vec![name.clone()]),
        LabelExpression::Conjunction { operands, span } => {
            let mut out = Vec::with_capacity(operands.len());
            for op in operands {
                match op {
                    LabelExpression::Label { name, .. } => out.push(name.clone()),
                    _ => {
                        return Err(not_implemented(
                            *span,
                            "nested label expressions are not yet lowered - \
                         keep conjunctions flat for now (e.g. `:A&B` is OK, \
                         `:A&(B|C)` is not)",
                        ));
                    }
                }
            }
            Ok(out)
        }
        LabelExpression::Disjunction { span, .. } => Err(not_implemented(
            *span,
            "node-label disjunctions are not yet lowered - Cypher's NodePattern \
             conjoins labels, so `A|B` has no direct translation",
        )),
        LabelExpression::Negation { span, .. } => Err(not_implemented(
            *span,
            "label negation (`!Foo`) is not yet lowered",
        )),
        LabelExpression::Wildcard { span } => Err(not_implemented(
            *span,
            "label wildcard (`%`) is not yet lowered",
        )),
    }
}

fn lower_property_map(p: &ast::PropertyMap) -> Result<cy::Expr, ParseError> {
    let entries = p
        .entries
        .iter()
        .map(|(k, v)| Ok((k.clone(), lower_expr(v)?)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(cy::Expr::MapLiteral(entries))
}

// -- expressions ---------------------------------------------------

/// GQL-source function name -> executor name. Most names pass through (the function table takes
/// Cypher and GQL spellings via `|` aliases); only where the GQL spec's semantics differ from the
/// same-named Cypher built-in is the GQL spelling routed to a dedicated impl.
fn gql_canonical_function_name(name: &str) -> String {
    match name.to_lowercase().as_str() {
        // GQL section 20.24 TRIM removes only U+0020 SPACE, Cypher's trim() any whitespace: route to the
        // spec-strict impl
        "trim" => "gql_trim".to_string(),
        other => other.to_string(),
    }
}

#[recursive::recursive]
pub(crate) fn lower_expr(e: &Expression) -> Result<cy::Expr, ParseError> {
    match e {
        Expression::IntegerLit { value, .. } => Ok(cy::Expr::Literal(cy::Literal::Integer(*value))),
        Expression::FloatLit { value, .. } => Ok(cy::Expr::Literal(cy::Literal::Float(*value))),
        Expression::StringLit { value, .. } => {
            Ok(cy::Expr::Literal(cy::Literal::String(value.clone())))
        }
        Expression::BoolLit { value, .. } => Ok(cy::Expr::Literal(cy::Literal::Bool(*value))),
        Expression::NullLit { .. } => Ok(cy::Expr::Literal(cy::Literal::Null)),
        Expression::Variable { name, .. } => Ok(cy::Expr::Variable(name.clone())),
        Expression::Parameter { name, .. } => Ok(cy::Expr::Parameter(name.clone())),
        Expression::Property { base, name, .. } => Ok(cy::Expr::Property(
            Box::new(lower_expr(base)?),
            name.clone(),
        )),
        Expression::Paren { inner, .. } => lower_expr(inner),
        Expression::Unary { op, operand, .. } => {
            let inner = Box::new(lower_expr(operand)?);
            Ok(cy::Expr::UnaryOp(
                match op {
                    UnaryOp::Neg => cy::UnaryOp::Neg,
                    UnaryOp::Not => cy::UnaryOp::Not,
                },
                inner,
            ))
        }
        Expression::Binary { op, lhs, rhs, .. } => {
            let l = lower_expr(lhs)?;
            let r = lower_expr(rhs)?;
            match op {
                BinaryOp::Add => Ok(bin(l, cy::BinOp::Add, r)),
                BinaryOp::Sub => Ok(bin(l, cy::BinOp::Sub, r)),
                BinaryOp::Mul => Ok(bin(l, cy::BinOp::Mul, r)),
                BinaryOp::Div => Ok(bin(l, cy::BinOp::Div, r)),
                BinaryOp::Mod => Ok(bin(l, cy::BinOp::Mod, r)),
                BinaryOp::Concat => Ok(bin(l, cy::BinOp::Concat, r)),
                BinaryOp::And => Ok(bin(l, cy::BinOp::And, r)),
                BinaryOp::Or => Ok(bin(l, cy::BinOp::Or, r)),
                BinaryOp::Xor => Ok(bin(l, cy::BinOp::Xor, r)),
                // comparisons use Cypher's `Comparison` form, which handles chains
                BinaryOp::Eq => Ok(cmp(l, cy::CmpOp::Eq, r)),
                BinaryOp::Neq => Ok(cmp(l, cy::CmpOp::Neq, r)),
                BinaryOp::Lt => Ok(cmp(l, cy::CmpOp::Lt, r)),
                BinaryOp::Lte => Ok(cmp(l, cy::CmpOp::Lte, r)),
                BinaryOp::Gt => Ok(cmp(l, cy::CmpOp::Gt, r)),
                BinaryOp::Gte => Ok(cmp(l, cy::CmpOp::Gte, r)),
            }
        }
        Expression::FunctionCall {
            name,
            distinct,
            args,
            ..
        } => {
            // where Cypher and GQL semantics diverge the GQL spelling goes to a GQL-strict impl
            // (`trim` -> `gql_trim`); everything else passes through, the function table handles
            // aliases (`upper`/`toupper`, `ceiling`/`ceil`, `local_time`/`localtime`, ...)
            let canonical = gql_canonical_function_name(name);
            let mut args = args.iter().map(lower_expr).collect::<Result<Vec<_>, _>>()?;
            // GQL section 20.24 trims strip U+0020 by default, Cypher's any whitespace: pass the space
            if args.len() == 1 && matches!(canonical.as_str(), "btrim" | "ltrim" | "rtrim") {
                args.push(str_lit(" "));
            }
            Ok(cy::Expr::FunctionCall {
                name: canonical,
                distinct: *distinct,
                args,
            })
        }
        Expression::List { items, .. } => Ok(cy::Expr::ListLiteral(
            items.iter().map(lower_expr).collect::<Result<_, _>>()?,
        )),
        Expression::Map { entries, .. } => {
            let lowered = entries
                .iter()
                .map(|(k, v)| lower_expr(v).map(|val| (k.clone(), val)))
                .collect::<Result<_, _>>()?;
            Ok(cy::Expr::MapLiteral(lowered))
        }
        Expression::Case {
            operand,
            arms,
            else_,
            ..
        } => {
            let cy_operand = operand
                .as_ref()
                .map(|o| lower_expr(o).map(Box::new))
                .transpose()?;
            let whens = arms
                .iter()
                .map(|arm| {
                    let w = lower_expr(&arm.when)?;
                    let t = lower_expr(&arm.then)?;
                    Ok((w, t))
                })
                .collect::<Result<Vec<_>, ParseError>>()?;
            let cy_else = else_
                .as_ref()
                .map(|e| lower_expr(e).map(Box::new))
                .transpose()?;
            Ok(cy::Expr::Case {
                operand: cy_operand,
                whens,
                else_expr: cy_else,
            })
        }
        Expression::Index { base, index, .. } => Ok(cy::Expr::ListIndex(
            Box::new(lower_expr(base)?),
            Box::new(lower_expr(index)?),
        )),
        Expression::TypePredicate {
            operand,
            kind,
            negated,
            span,
        } => lower_type_predicate(operand, kind, *negated, *span),
        Expression::ExistsSubquery { body, .. } => {
            // the executor has `ExistentialSubquery(MatchClause)` for a single MATCH and
            // `FullExistentialSubquery` for everything else; always emit the full form, it covers
            // both
            let lowered = lower_query_body(body)?;
            Ok(cy::Expr::FullExistentialSubquery(Box::new(lowered)))
        }
        Expression::IsNull {
            operand, negated, ..
        } => {
            let inner = Box::new(lower_expr(operand)?);
            Ok(if *negated {
                cy::Expr::IsNotNull(inner)
            } else {
                cy::Expr::IsNull(inner)
            })
        }
    }
}

fn bin(l: cy::Expr, op: cy::BinOp, r: cy::Expr) -> cy::Expr {
    cy::Expr::BinaryOp(Box::new(l), op, Box::new(r))
}

/// Lowers `IS [NOT] <type>` / `IS [NOT] LABELED ...` / `IS [NOT] NORMALIZED ...` by desugaring into
/// Cypher primitives (`typename()`, `labels()`, `IN`, `gql_is_normalized()`). Type predicates are
/// always BOOLEAN (ISO section 19.6): null gives `false`, not `null`. Negation wraps a final `NOT`.
fn lower_type_predicate(
    operand: &Expression,
    kind: &TypeKind,
    negated: bool,
    span: Span,
) -> Result<cy::Expr, ParseError> {
    // the Cypher AST has no per-expression spans; `span` stays in the signature for diagnostics
    let _ = span;
    let lowered_operand = lower_expr(operand)?;
    let inner = match kind {
        // scalar / structural types compare against `typename()`, the bare
        // `CypherValue::type_name()` spelling
        TypeKind::Boolean => typename_eq(&lowered_operand, "BOOLEAN"),
        TypeKind::String => typename_eq(&lowered_operand, "STRING"),
        TypeKind::Integer => typename_eq(&lowered_operand, "INTEGER"),
        TypeKind::Float => typename_eq(&lowered_operand, "FLOAT"),
        TypeKind::Number => {
            // BOOLEAN-valued: type_name == 'INTEGER' OR type_name == 'FLOAT'.
            let vt = typename_call(&lowered_operand);
            let int_eq = cmp(vt.clone(), cy::CmpOp::Eq, str_lit("INTEGER"));
            let float_eq = cmp(vt, cy::CmpOp::Eq, str_lit("FLOAT"));
            bin(int_eq, cy::BinOp::Or, float_eq)
        }
        TypeKind::Node => typename_eq(&lowered_operand, "NODE"),
        TypeKind::Edge => typename_eq(&lowered_operand, "RELATIONSHIP"),
        TypeKind::Path => typename_eq(&lowered_operand, "PATH"),
        TypeKind::List => typename_eq(&lowered_operand, "LIST"),
        TypeKind::Map => typename_eq(&lowered_operand, "MAP"),
        TypeKind::Date => typename_eq(&lowered_operand, "DATE"),
        TypeKind::LocalTime => typename_eq(&lowered_operand, "LOCAL_TIME"),
        TypeKind::ZonedTime => typename_eq(&lowered_operand, "TIME"),
        TypeKind::LocalDatetime => typename_eq(&lowered_operand, "LOCAL_DATE_TIME"),
        TypeKind::ZonedDatetime => typename_eq(&lowered_operand, "DATE_TIME"),
        TypeKind::Duration => typename_eq(&lowered_operand, "DURATION"),
        // `IS ANY` => NOT NULL (ISO section 19.6: ANY excludes only null)
        TypeKind::Any => cy::Expr::IsNotNull(Box::new(lowered_operand)),
        // `IS NOTHING` => NULL, the empty type
        TypeKind::Nothing => cy::Expr::IsNull(Box::new(lowered_operand)),
        // `IS LABELED <label_expr>`: a boolean tree over `labels(operand)`; for an edge
        // `type(edge)` is one string, so the type name is checked via `type()` as well
        TypeKind::Labeled(le) => {
            let labels_call = function_call("labels", vec![lowered_operand.clone()]);
            let labeled_tree = lower_label_expression(&labels_call, le);
            // `labels(null)` is null: coerce to boolean (ISO section 19.6)
            coalesce_to_false(labeled_tree)
        }
        // `IS NORMALIZED [<form>]`: the executor's `gql_is_normalized` already returns a boolean
        // (`false` for null or non-strings)
        TypeKind::Normalized(form) => {
            let mut args = vec![lowered_operand];
            if let Some(f) = form {
                args.push(cy::Expr::Literal(cy::Literal::String(f.clone())));
            }
            function_call("gql_is_normalized", args)
        }
        // `IS T1 | T2 | ...`: OR of per-component predicates, each lowered un-negated against the
        // same operand; the outer negation applies once below, so `IS NOT (T1|T2)` is `NOT (is T1
        // OR is T2)`
        TypeKind::Union(alts) => {
            let mut iter = alts.iter();
            // never empty per the parser, but a malformed AST yields `false`
            let first = match iter.next() {
                Some(k) => lower_type_predicate(operand, k, false, span)?,
                None => cy::Expr::Literal(cy::Literal::Bool(false)),
            };
            iter.try_fold(first, |acc, k| {
                let rhs = lower_type_predicate(operand, k, false, span)?;
                Ok::<cy::Expr, ParseError>(bin(acc, cy::BinOp::Or, rhs))
            })?
        }
    };
    if negated {
        Ok(cy::Expr::UnaryOp(cy::UnaryOp::Not, Box::new(inner)))
    } else {
        Ok(inner)
    }
}

/// Boolean expression for "node's labels match `label_expr`" (section 14.7: conjunction, disjunction,
/// negation, wildcard, single labels); each `Label("X")` becomes `'X' IN labels_expr`.
#[recursive::recursive]
fn lower_label_expression(labels_expr: &cy::Expr, le: &LabelExpression) -> cy::Expr {
    match le {
        LabelExpression::Label { name, .. } => {
            cy::Expr::In(Box::new(str_lit(name)), Box::new(labels_expr.clone()))
        }
        LabelExpression::Wildcard { .. } => {
            // `%` matches any label: `size(labels(n)) > 0`
            let size = function_call("size", vec![labels_expr.clone()]);
            cy::Expr::Comparison(
                Box::new(size),
                vec![(cy::CmpOp::Gt, cy::Expr::Literal(cy::Literal::Integer(0)))],
            )
        }
        LabelExpression::Conjunction { operands, .. } => operands
            .iter()
            .map(|op| lower_label_expression(labels_expr, op))
            .reduce(|l, r| bin(l, cy::BinOp::And, r))
            .unwrap_or(cy::Expr::Literal(cy::Literal::Bool(true))),
        LabelExpression::Disjunction { operands, .. } => operands
            .iter()
            .map(|op| lower_label_expression(labels_expr, op))
            .reduce(|l, r| bin(l, cy::BinOp::Or, r))
            .unwrap_or(cy::Expr::Literal(cy::Literal::Bool(false))),
        LabelExpression::Negation { operand, .. } => cy::Expr::UnaryOp(
            cy::UnaryOp::Not,
            Box::new(lower_label_expression(labels_expr, operand)),
        ),
    }
}

/// `typename(operand)`: the bare `CypherValue::type_name()` spelling ("BOOLEAN", "LOCAL_TIME", ...)
/// the type predicates compare against. Not `valuetype()`, which returns ISO descriptor strings ("
/// NOT NULL" suffix, `LIST<inner>` element types) that never equal a bare type name.
fn typename_call(operand: &cy::Expr) -> cy::Expr {
    function_call("typename", vec![operand.clone()])
}

fn typename_eq(operand: &cy::Expr, type_name: &str) -> cy::Expr {
    cmp(typename_call(operand), cy::CmpOp::Eq, str_lit(type_name))
}

fn coalesce_to_false(expr: cy::Expr) -> cy::Expr {
    function_call(
        "coalesce",
        vec![expr, cy::Expr::Literal(cy::Literal::Bool(false))],
    )
}

fn function_call(name: &str, args: Vec<cy::Expr>) -> cy::Expr {
    cy::Expr::FunctionCall {
        name: name.into(),
        distinct: false,
        args,
    }
}

fn str_lit(s: &str) -> cy::Expr {
    cy::Expr::Literal(cy::Literal::String(s.to_string()))
}

fn cmp(l: cy::Expr, op: cy::CmpOp, r: cy::Expr) -> cy::Expr {
    cy::Expr::Comparison(Box::new(l), vec![(op, r)])
}

fn not_implemented(span: Span, msg: &str) -> ParseError {
    ParseError {
        span,
        kind: ParseErrorKind::NotImplemented,
        message: msg.into(),
    }
}

// -- tests ----------------------------------------------------------

#[cfg(test)]
mod tests;
