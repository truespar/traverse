//! Clauses: MATCH, SEARCH, CREATE, MERGE, DELETE, SET, REMOVE, UNWIND, FOREACH, LOAD CSV,
//! CALL, RETURN and WITH.

use crate::ast::{
    CallClause, CallSubqueryClause, Clause, CreateClause, DeleteClause, Expr, ForEachClause,
    InTransactions, LoadCsvClause, MatchClause, MergeClause, OnErrorBehavior, ProjectionItem,
    ProjectionItems, PropertyAccess, RegularQuery, RemoveClause, RemoveItem, ReturnClause,
    SetClause, SetItem, SortItem, Union, UnwindClause, WithClause, YieldItem, YieldItems,
};
use crate::error::CypherError;
use crate::lexer::TokenKind;

use super::{Parser, is_keyword_as_ident, keyword_to_string};

impl Parser {
    pub(super) fn parse_match_clause(&mut self) -> Result<Clause, CypherError> {
        let optional = self.eat_keyword(&TokenKind::Optional);
        self.expect(&TokenKind::Match)?;
        // ISO section 14.4 match modes: `REPEATABLE ELEMENTS` lifts uniqueness (paths become WALKs);
        // `DIFFERENT RELATIONSHIPS` is the default spelled out
        let kw =
            |t: &TokenKind, w: &str| matches!(t, TokenKind::Ident(s) if s.eq_ignore_ascii_case(w));
        let repeatable = if kw(self.peek(), "REPEATABLE") && kw(self.peek_offset(1), "ELEMENTS") {
            self.advance();
            self.advance();
            true
        } else if kw(self.peek(), "DIFFERENT") && kw(self.peek_offset(1), "RELATIONSHIPS") {
            self.advance();
            self.advance();
            false
        } else {
            false
        };
        let mut pattern = self.parse_pattern()?;
        if repeatable {
            for part in &mut pattern.parts {
                // explicit per-part modes (`ACYCLIC (...)`) still apply under REPEATABLE ELEMENTS;
                // only default parts relax
                if part.path_mode == crate::ast::PathMode::Trail {
                    part.path_mode = crate::ast::PathMode::Walk;
                }
            }
        }
        let pattern = pattern;
        // `USING INDEX var:Label(prop)` / `USING SCAN var:Label`: any number, any order, before
        // WHERE
        let mut hints = Vec::new();
        while self.peek() == &TokenKind::Using {
            if let Some(h) = self.parse_match_hint()? {
                hints.push(h);
            }
        }
        // `WHERE` and `SEARCH` in either order (Neo4j allows the post-filter on either side; both
        // post-filter). A WHERE on each side is ANDed
        let mut where_clause: Option<Expr> = None;
        let mut search: Option<crate::ast::SearchClause> = None;
        // two WHEREs need a SEARCH between them; back-to-back WHEREs are a syntax error, not an
        // implicit AND
        let mut where_this_side = false;
        loop {
            if self.peek() == &TokenKind::Where {
                if where_this_side {
                    return Err(CypherError::syntax(
                        "multiple WHERE clauses on a single MATCH",
                        self.span_pos(),
                    ));
                }
                self.advance();
                where_this_side = true;
                let pred = self.parse_expr()?;
                where_clause = Some(match where_clause.take() {
                    None => pred,
                    Some(prev) => {
                        Expr::BinaryOp(Box::new(prev), crate::ast::BinOp::And, Box::new(pred))
                    }
                });
            } else if search.is_none()
                && matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("SEARCH"))
            {
                self.advance(); // SEARCH
                search = Some(self.parse_search_clause()?);
                where_this_side = false;
            } else {
                break;
            }
        }
        let mc = MatchClause {
            pattern,
            where_clause,
            hints,
            search,
        };
        if optional {
            Ok(Clause::OptionalMatch(mc))
        } else {
            Ok(Clause::Match(mc))
        }
    }

    /// Body of a `SEARCH` subclause (Cypher 25), `SEARCH` already consumed:
    /// `var IN ( VECTOR INDEX name FOR query_vector [WHERE pred] LIMIT k )
    ///  [SCORE AS alias]`.
    fn parse_search_clause(&mut self) -> Result<crate::ast::SearchClause, CypherError> {
        let binding = self.parse_symbolic_name()?;
        self.expect(&TokenKind::In)?;
        self.expect(&TokenKind::LParen)?;
        if !matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("VECTOR")) {
            return Err(CypherError::syntax(
                "expected VECTOR INDEX in SEARCH clause",
                self.span_pos(),
            ));
        }
        self.advance(); // VECTOR
        self.expect(&TokenKind::Index)?;
        let index_name = self.parse_symbolic_name()?;
        self.expect(&TokenKind::For)?;
        let query_vector = self.parse_expr()?;
        let index_where = if self.eat_keyword(&TokenKind::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect(&TokenKind::Limit)?;
        let limit = self.parse_expr()?;
        self.expect(&TokenKind::RParen)?;
        // optional `SCORE AS alias`
        let score_alias = if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("SCORE"))
        {
            self.advance(); // SCORE
            self.expect(&TokenKind::As)?;
            Some(self.parse_symbolic_name()?)
        } else {
            None
        };
        Ok(crate::ast::SearchClause {
            binding,
            index_name,
            query_vector,
            index_where,
            limit,
            score_alias,
        })
    }

    /// `USING ...` planner hint. `None` for hints accepted but not acted on (`JOIN`, typed-index
    /// qualifiers, `SEEK`): the optimizer is cost-based.
    fn parse_match_hint(&mut self) -> Result<Option<crate::ast::MatchHint>, CypherError> {
        self.expect(&TokenKind::Using)?;
        // optional index type: `USING RANGE|TEXT|POINT INDEX ...`
        if matches!(self.peek(), TokenKind::Ident(s)
            if matches!(s.to_uppercase().as_str(), "RANGE" | "TEXT" | "POINT"))
            && *self.peek_offset(1) == TokenKind::Index
        {
            self.advance(); // type qualifier
        }
        if self.eat(&TokenKind::Index) {
            // optional `SEEK`: `USING INDEX SEEK var:Label(prop)`
            if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("SEEK")) {
                self.advance();
            }
            let variable = self.parse_symbolic_name()?;
            self.expect(&TokenKind::Colon)?;
            let label = self.parse_symbolic_name()?;
            self.expect(&TokenKind::LParen)?;
            let property = self.parse_symbolic_name()?;
            // composite-index hints list more properties; keep the first
            while self.eat(&TokenKind::Comma) {
                let _ = self.parse_symbolic_name()?;
            }
            self.expect(&TokenKind::RParen)?;
            Ok(Some(crate::ast::MatchHint::Index {
                variable,
                label,
                property,
            }))
        } else if self.eat(&TokenKind::Scan) {
            let variable = self.parse_symbolic_name()?;
            self.expect(&TokenKind::Colon)?;
            let label = self.parse_symbolic_name()?;
            Ok(Some(crate::ast::MatchHint::Scan { variable, label }))
        } else if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("JOIN")) {
            // `USING JOIN ON var[, var ...]`: advisory, ignored
            self.advance(); // JOIN
            self.expect(&TokenKind::On)?;
            let _ = self.parse_symbolic_name()?;
            while self.eat(&TokenKind::Comma) {
                let _ = self.parse_symbolic_name()?;
            }
            Ok(None)
        } else {
            Err(CypherError::syntax(
                format!(
                    "USING must be followed by INDEX, SCAN, or JOIN, got {:?}",
                    self.peek()
                ),
                self.span_pos(),
            ))
        }
    }

    pub(super) fn parse_create_clause(&mut self) -> Result<CreateClause, CypherError> {
        self.expect(&TokenKind::Create)?;
        let pattern = self.parse_pattern()?;
        Ok(CreateClause { pattern })
    }

    pub(super) fn parse_merge_clause(&mut self) -> Result<MergeClause, CypherError> {
        self.expect(&TokenKind::Merge)?;
        let pattern = self.parse_pattern()?;
        let mut on_create = None;
        let mut on_match = None;
        while self.eat_keyword(&TokenKind::On) {
            match self.peek().clone() {
                TokenKind::Create => {
                    self.advance();
                    on_create = Some(self.parse_set_clause()?);
                }
                TokenKind::Match => {
                    self.advance();
                    on_match = Some(self.parse_set_clause()?);
                }
                _ => {
                    return Err(CypherError::syntax(
                        format!("expected CREATE or MATCH after ON, found {:?}", self.peek()),
                        self.span_pos(),
                    ));
                }
            }
        }
        Ok(MergeClause {
            pattern,
            on_create,
            on_match,
        })
    }

    pub(super) fn parse_delete_clause(
        &mut self,
        detach: bool,
    ) -> Result<DeleteClause, CypherError> {
        self.expect(&TokenKind::Delete)?;
        let mut clause = self.parse_delete_inner()?;
        clause.detach = detach;
        Ok(clause)
    }

    pub(super) fn parse_delete_inner(&mut self) -> Result<DeleteClause, CypherError> {
        let mut exprs = vec![self.parse_expr()?];
        while self.eat(&TokenKind::Comma) {
            exprs.push(self.parse_expr()?);
        }
        Ok(DeleteClause {
            detach: false,
            exprs,
        })
    }

    pub(super) fn parse_set_clause(&mut self) -> Result<SetClause, CypherError> {
        self.expect(&TokenKind::Set)?;
        let mut items = vec![self.parse_set_item()?];
        while self.eat(&TokenKind::Comma) {
            items.push(self.parse_set_item()?);
        }
        Ok(SetClause { items })
    }

    fn parse_set_item(&mut self) -> Result<SetItem, CypherError> {
        // parenthesized variable: SET (n).prop = expr
        let paren = self.eat(&TokenKind::LParen);
        let name = self.parse_symbolic_name()?;
        if paren {
            self.expect(&TokenKind::RParen)?;
        }

        match self.peek() {
            TokenKind::Dot => {
                // n.prop = expr
                self.advance();
                let key = self.parse_symbolic_name()?;
                self.expect(&TokenKind::Eq)?;
                let expr = self.parse_expr()?;
                Ok(SetItem::Property(
                    PropertyAccess {
                        expr_or_var: name,
                        key,
                    },
                    expr,
                ))
            }
            TokenKind::LBracket => {
                // n[$key] = expr: dynamic property, key evaluates per row to a String
                self.advance(); // `[`
                let key = self.parse_expr()?;
                self.expect(&TokenKind::RBracket)?;
                self.expect(&TokenKind::Eq)?;
                let value = self.parse_expr()?;
                Ok(SetItem::DynamicProperty {
                    variable: name,
                    key,
                    value,
                })
            }
            TokenKind::Eq => {
                // n = expr (replace all properties)
                self.advance();
                let expr = self.parse_expr()?;
                Ok(SetItem::AllProperties(name, expr))
            }
            TokenKind::PlusEq => {
                // n += expr (merge properties)
                self.advance();
                let expr = self.parse_expr()?;
                Ok(SetItem::MergeProperties(name, expr))
            }
            TokenKind::Colon => {
                // n:Label1:Label2:$($dyn)
                let mut labels = Vec::new();
                let mut dyn_labels: Vec<Expr> = Vec::new();
                while self.eat(&TokenKind::Colon) {
                    match self.peek() {
                        TokenKind::Parameter(_) => {
                            if let TokenKind::Parameter(p) = self.advance().kind.clone() {
                                dyn_labels.push(Expr::Parameter(p));
                            }
                        }
                        TokenKind::DollarLParen => {
                            self.advance();
                            let e = self.parse_expr()?;
                            self.expect(&TokenKind::RParen)?;
                            dyn_labels.push(e);
                        }
                        _ => labels.push(self.parse_symbolic_name()?),
                    }
                }
                Ok(SetItem::Label(name, labels, dyn_labels))
            }
            _ => Err(CypherError::syntax(
                format!(
                    "expected '.', '[', '=', '+=', or ':' in SET item, found {:?}",
                    self.peek()
                ),
                self.span_pos(),
            )),
        }
    }

    pub(super) fn parse_remove_clause(&mut self) -> Result<RemoveClause, CypherError> {
        self.expect(&TokenKind::Remove)?;
        let mut items = vec![self.parse_remove_item()?];
        while self.eat(&TokenKind::Comma) {
            items.push(self.parse_remove_item()?);
        }
        Ok(RemoveClause { items })
    }

    fn parse_remove_item(&mut self) -> Result<RemoveItem, CypherError> {
        let name = self.parse_symbolic_name()?;
        match self.peek() {
            TokenKind::Dot => {
                self.advance();
                let key = self.parse_symbolic_name()?;
                Ok(RemoveItem::Property(PropertyAccess {
                    expr_or_var: name,
                    key,
                }))
            }
            TokenKind::Colon => {
                let mut labels = Vec::new();
                let mut dyn_labels: Vec<Expr> = Vec::new();
                while self.eat(&TokenKind::Colon) {
                    match self.peek() {
                        TokenKind::Parameter(_) => {
                            if let TokenKind::Parameter(p) = self.advance().kind.clone() {
                                dyn_labels.push(Expr::Parameter(p));
                            }
                        }
                        TokenKind::DollarLParen => {
                            self.advance();
                            let e = self.parse_expr()?;
                            self.expect(&TokenKind::RParen)?;
                            dyn_labels.push(e);
                        }
                        _ => labels.push(self.parse_symbolic_name()?),
                    }
                }
                Ok(RemoveItem::Label(name, labels, dyn_labels))
            }
            _ => Err(CypherError::syntax(
                format!(
                    "expected '.' or ':' in REMOVE item, found {:?}",
                    self.peek()
                ),
                self.span_pos(),
            )),
        }
    }

    pub(super) fn parse_unwind_clause(&mut self) -> Result<UnwindClause, CypherError> {
        self.expect(&TokenKind::Unwind)?;
        let expr = self.parse_expr()?;
        self.expect(&TokenKind::As)?;
        let alias = self.parse_symbolic_name()?;
        Ok(UnwindClause { expr, alias })
    }

    /// `FOREACH (var IN list | update_clauses)` (openCypher). Body holds only updates
    /// (CREATE/SET/MERGE/REMOVE/DELETE/nested FOREACH), run once per element with `var` bound.
    #[recursive::recursive]
    pub(super) fn parse_foreach_clause(&mut self) -> Result<ForEachClause, CypherError> {
        self.expect(&TokenKind::Foreach)?;
        self.expect(&TokenKind::LParen)?;
        let variable = self.parse_symbolic_name()?;
        self.expect(&TokenKind::In)?;
        // the list runs up to `|`; `|` isn't an expression operator here, so parse_expr stops
        // before it (a `|` inside reduce()/comprehensions belongs to that parser)
        let list = self.parse_expr()?;
        self.expect(&TokenKind::Pipe)?;
        let mut updates = Vec::new();
        while *self.peek() != TokenKind::RParen {
            if *self.peek() == TokenKind::Eof {
                return Err(CypherError::syntax(
                    "unterminated FOREACH: expected ')'",
                    self.span_pos(),
                ));
            }
            updates.push(self.parse_foreach_body_clause()?);
        }
        self.expect(&TokenKind::RParen)?;
        Ok(ForEachClause {
            variable,
            list,
            updates,
        })
    }

    /// One updating clause of a FOREACH body; reading clauses (MATCH/WITH/RETURN/UNWIND/CALL) are
    /// rejected, as in Neo4j.
    fn parse_foreach_body_clause(&mut self) -> Result<Clause, CypherError> {
        match self.peek() {
            TokenKind::Create => Ok(Clause::Create(self.parse_create_clause()?)),
            TokenKind::Merge => Ok(Clause::Merge(self.parse_merge_clause()?)),
            TokenKind::Set => Ok(Clause::Set(self.parse_set_clause()?)),
            TokenKind::Remove => Ok(Clause::Remove(self.parse_remove_clause()?)),
            TokenKind::Delete => Ok(Clause::Delete(self.parse_delete_clause(false)?)),
            TokenKind::Detach => {
                self.advance();
                self.expect(&TokenKind::Delete)?;
                let mut clause = self.parse_delete_inner()?;
                clause.detach = true;
                Ok(Clause::Delete(clause))
            }
            TokenKind::Foreach => Ok(Clause::ForEach(self.parse_foreach_clause()?)),
            other => Err(CypherError::syntax(
                format!(
                    "FOREACH body allows only updating clauses \
                     (CREATE/SET/MERGE/REMOVE/DELETE/FOREACH), got {other:?}"
                ),
                self.span_pos(),
            )),
        }
    }

    pub(super) fn parse_load_csv_clause(&mut self) -> Result<LoadCsvClause, CypherError> {
        self.expect(&TokenKind::Load)?;
        self.expect(&TokenKind::Csv)?;

        // WITH HEADERS vs a WITH projection: consume both only when the token after WITH is
        // "HEADERS"
        let with_headers = if *self.peek() == TokenKind::With {
            match self.peek_offset(1) {
                TokenKind::Ident(s) if s.eq_ignore_ascii_case("HEADERS") => {
                    self.advance(); // WITH
                    self.advance(); // HEADERS
                    true
                }
                _ => false,
            }
        } else {
            false
        };

        // FROM: contextual keyword (Ident or keyword-as-ident)
        match self.peek() {
            TokenKind::Ident(s) if s.eq_ignore_ascii_case("FROM") => {
                self.advance();
            }
            TokenKind::For => {
                // FROM is never a keyword
                return Err(CypherError::syntax(
                    "expected FROM after LOAD CSV".to_string(),
                    self.span_pos(),
                ));
            }
            _ => {
                return Err(CypherError::syntax(
                    "expected FROM after LOAD CSV".to_string(),
                    self.span_pos(),
                ));
            }
        }

        let url = self.parse_expr()?;
        self.expect(&TokenKind::As)?;
        let row_alias = self.parse_symbolic_name()?;

        // optional FIELDTERMINATOR
        let field_terminator = match self.peek() {
            TokenKind::Ident(s) if s.eq_ignore_ascii_case("FIELDTERMINATOR") => {
                self.advance();
                match self.peek().clone() {
                    TokenKind::StringLit(s) => {
                        self.advance();
                        let mut chars = s.chars();
                        let ch = chars.next().ok_or_else(|| {
                            CypherError::syntax(
                                "FIELDTERMINATOR must be a single character".to_string(),
                                self.span_pos(),
                            )
                        })?;
                        if chars.next().is_some() {
                            return Err(CypherError::syntax(
                                "FIELDTERMINATOR must be a single character".to_string(),
                                self.span_pos(),
                            ));
                        }
                        Some(ch)
                    }
                    _ => {
                        return Err(CypherError::syntax(
                            "expected string literal after FIELDTERMINATOR".to_string(),
                            self.span_pos(),
                        ));
                    }
                }
            }
            _ => None,
        };

        Ok(LoadCsvClause {
            with_headers,
            url,
            row_alias,
            field_terminator,
        })
    }

    /// One dotted segment of a procedure name: any `Ident` or keyword (Cypher allows `create`,
    /// `drop`, `match`, `return` ... after a dot). Broader than `parse_symbolic_name`; procedure-name
    /// positions are unambiguous.
    pub(super) fn parse_procedure_name_segment(&mut self) -> Result<String, CypherError> {
        // fast path: plain identifier
        if let TokenKind::Ident(name) = self.peek().clone() {
            self.advance();
            return Ok(name);
        }
        // slow path: keyword token; recover source text for casing (`Create` / `create` / `CREATE`)
        let kw = self.peek().clone();
        let is_kw_like = matches!(
            kw,
            // the keyword-as-ident list...
            ref k if is_keyword_as_ident(k))
            // ...plus the CRUD keywords GDS-style procedure names use (create / drop / match / return
            // / ...), listed explicitly
            || matches!(
                kw,
                TokenKind::Create
                    | TokenKind::Match
                    | TokenKind::Return
                    | TokenKind::With
                    | TokenKind::Where
                    | TokenKind::Set
                    | TokenKind::Remove
                    | TokenKind::Delete
                    | TokenKind::Merge
                    | TokenKind::Unwind
                    | TokenKind::Union
                    // `traverse.ml.explain.*`, `....profile.*`
                    | TokenKind::Explain
                    | TokenKind::Profile
                    // YIELD columns named like keywords (apoc.schema.assert yields `unique`, SHOW
                    // yields `type`, ...)
                    | TokenKind::Unique
                    | TokenKind::Constraint
                    | TokenKind::Index
                    | TokenKind::All
            );
        if !is_kw_like {
            return self.parse_symbolic_name();
        }
        let name = if !self.source.is_empty() {
            let span = &self.tokens[self.pos].span;
            self.source[span.start..span.end].to_string()
        } else {
            keyword_to_string(&kw)
        };
        self.advance();
        Ok(name)
    }

    /// Inline procedure subquery `CALL [(imports)] { body }` (ISO 39075 section 9.4 /
    /// openCypher 5). The body is a full `SingleQuery` (reads *and* writes), planned
    /// and run per input row with the imported variables seeded; its rows cross-join
    /// back onto the outer record. `parse_single_query` routes here when `CALL` is
    /// followed by `{` or `(`.
    pub(super) fn parse_call_subquery_clause(&mut self) -> Result<CallSubqueryClause, CypherError> {
        self.expect(&TokenKind::Call)?;
        // optional import list `CALL (a, b) { ... }`; `CALL () { ... }` imports nothing, `CALL { ... }`
        // without parens is transparent scope
        let imports = if *self.peek() == TokenKind::LParen {
            self.advance(); // '('
            // `CALL (*) { ... }`: import ALL outer variables (Cypher 25 spelling of transparent
            // scope)
            if *self.peek() == TokenKind::Star {
                self.advance();
                self.expect(&TokenKind::RParen)?;
                None // transparent scope, like `CALL { ... }`
            } else {
                let mut names = Vec::new();
                if *self.peek() != TokenKind::RParen {
                    loop {
                        names.push(self.parse_symbolic_name()?);
                        if *self.peek() == TokenKind::Comma {
                            self.advance();
                            continue;
                        }
                        break;
                    }
                }
                self.expect(&TokenKind::RParen)?;
                Some(names)
            }
        } else {
            None
        };
        self.expect(&TokenKind::LBrace)?;
        let single = self.parse_single_query()?;
        let mut unions = Vec::new();
        while self.eat_keyword(&TokenKind::Union) {
            let all = self.eat_keyword(&TokenKind::All);
            let query = self.parse_single_query()?;
            unions.push(Union { all, query });
        }
        let body = RegularQuery { single, unions };
        self.expect(&TokenKind::RBrace)?;
        // `CALL { ... } IN TRANSACTIONS [OF n ROWS] [ON ERROR ...] [REPORT STATUS AS v]`
        let in_transactions = if *self.peek() == TokenKind::In {
            Some(self.parse_in_transactions()?)
        } else {
            None
        };
        Ok(CallSubqueryClause {
            imports,
            body: Box::new(body),
            in_transactions,
        })
    }

    /// `IN TRANSACTIONS [OF <expr> ROWS] [ON ERROR {CONTINUE|BREAK|FAIL}] [REPORT STATUS AS var]`
    /// tail; `IN` is the current token.
    fn parse_in_transactions(&mut self) -> Result<InTransactions, CypherError> {
        self.expect(&TokenKind::In)?;
        // TRANSACTION[S] (contextual identifier)
        match self.peek() {
            TokenKind::Ident(s)
                if s.eq_ignore_ascii_case("TRANSACTIONS")
                    || s.eq_ignore_ascii_case("TRANSACTION") =>
            {
                self.advance();
            }
            _ => {
                return Err(CypherError::syntax(
                    "expected TRANSACTIONS after IN",
                    self.span_pos(),
                ));
            }
        }
        let is_kw = |p: &TokenKind, kw: &str| matches!(p, TokenKind::Ident(s) if s.eq_ignore_ascii_case(kw));

        // OF <expr> ROWS
        let batch_size = if is_kw(self.peek(), "OF") {
            self.advance();
            let e = self.parse_expr()?;
            // optional ROW / ROWS
            if is_kw(self.peek(), "ROWS") || is_kw(self.peek(), "ROW") {
                self.advance();
            }
            Some(e)
        } else {
            None
        };

        // ON ERROR {CONTINUE | BREAK | FAIL}
        let on_error = if *self.peek() == TokenKind::On {
            self.advance();
            if !is_kw(self.peek(), "ERROR") {
                return Err(CypherError::syntax(
                    "expected ERROR after ON",
                    self.span_pos(),
                ));
            }
            self.advance();
            // each arm consumes its own tokens (RETRY has a multi-token tail)
            match self.peek().clone() {
                t if is_kw(&t, "CONTINUE") => {
                    self.advance();
                    OnErrorBehavior::Continue
                }
                t if is_kw(&t, "BREAK") => {
                    self.advance();
                    OnErrorBehavior::Break
                }
                t if is_kw(&t, "FAIL") => {
                    self.advance();
                    OnErrorBehavior::Fail
                }
                // `RETRY [FOR <duration> [SECOND[S]]] [THEN CONTINUE|BREAK|FAIL]` (Cypher 2025.06).
                // Nothing transient to retry here, so the THEN behavior applies directly (default
                // FAIL, like Neo4j)
                t if is_kw(&t, "RETRY") => {
                    self.advance(); // RETRY
                    if *self.peek() == TokenKind::For {
                        self.advance();
                        let _duration = self.parse_expr()?;
                        if is_kw(self.peek(), "SECONDS") || is_kw(self.peek(), "SECOND") {
                            self.advance();
                        }
                    }
                    if is_kw(self.peek(), "THEN") || *self.peek() == TokenKind::Then {
                        self.advance();
                        let b = match self.peek() {
                            t if is_kw(t, "CONTINUE") => OnErrorBehavior::Continue,
                            t if is_kw(t, "BREAK") => OnErrorBehavior::Break,
                            t if is_kw(t, "FAIL") => OnErrorBehavior::Fail,
                            _ => {
                                return Err(CypherError::syntax(
                                    "expected CONTINUE, BREAK, or FAIL after ON ERROR RETRY THEN",
                                    self.span_pos(),
                                ));
                            }
                        };
                        self.advance();
                        b
                    } else {
                        OnErrorBehavior::Fail
                    }
                }
                _ => {
                    return Err(CypherError::syntax(
                        "expected CONTINUE, BREAK, RETRY, or FAIL after ON ERROR",
                        self.span_pos(),
                    ));
                }
            }
        } else {
            OnErrorBehavior::Fail
        };

        // REPORT STATUS AS var
        let report_status = if is_kw(self.peek(), "REPORT") {
            self.advance();
            if !is_kw(self.peek(), "STATUS") {
                return Err(CypherError::syntax(
                    "expected STATUS after REPORT",
                    self.span_pos(),
                ));
            }
            self.advance();
            self.expect(&TokenKind::As)?;
            Some(self.parse_symbolic_name()?)
        } else {
            None
        };

        Ok(InTransactions {
            batch_size,
            on_error,
            report_status,
        })
    }

    pub(super) fn parse_call_clause(&mut self) -> Result<CallClause, CypherError> {
        self.expect(&TokenKind::Call)?;
        // dotted procedure name: segments may be keywords (Neo4j:
        // `gds.beta.pipeline.nodeClassification.create`), so `parse_procedure_name_segment` is more
        // permissive than `parse_symbolic_name`
        let mut name = self.parse_procedure_name_segment()?;
        while self.eat(&TokenKind::Dot) {
            let part = self.parse_procedure_name_segment()?;
            name = format!("{name}.{part}");
        }
        // optional explicit arguments
        let args = if self.eat(&TokenKind::LParen) {
            let mut arg_list = Vec::new();
            if *self.peek() != TokenKind::RParen {
                arg_list.push(self.parse_expr()?);
                while self.eat(&TokenKind::Comma) {
                    arg_list.push(self.parse_expr()?);
                }
            }
            self.expect(&TokenKind::RParen)?;
            Some(arg_list)
        } else {
            None
        };
        // optional YIELD
        let yield_items = if self.eat_keyword(&TokenKind::Yield) {
            if self.eat(&TokenKind::Star) {
                Some(YieldItems::All)
            } else {
                let mut items = vec![self.parse_yield_item()?];
                while self.eat(&TokenKind::Comma) {
                    items.push(self.parse_yield_item()?);
                }
                Some(YieldItems::Explicit(items))
            }
        } else {
            None
        };
        Ok(CallClause {
            procedure_name: name,
            args,
            yield_items,
        })
    }

    fn parse_yield_item(&mut self) -> Result<YieldItem, CypherError> {
        // keyword-permissive: procedures yield columns named like keywords (`unique`, `type`,
        // `index`, ...)
        let name = self.parse_procedure_name_segment()?;
        let alias = if self.eat_keyword(&TokenKind::As) {
            Some(self.parse_symbolic_name()?)
        } else {
            None
        };
        Ok(YieldItem { name, alias })
    }

    pub(super) fn parse_return_clause(&mut self) -> Result<ReturnClause, CypherError> {
        self.expect(&TokenKind::Return)?;
        let distinct = self.eat_keyword(&TokenKind::Distinct);
        let items = self.parse_projection_items()?;
        let order_by = self.parse_order_by()?;
        let skip = if self.eat_keyword(&TokenKind::Skip) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let limit = if self.eat_keyword(&TokenKind::Limit) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(ReturnClause {
            distinct,
            items,
            order_by,
            skip,
            limit,
        })
    }

    pub(super) fn parse_with_clause(&mut self) -> Result<WithClause, CypherError> {
        self.expect(&TokenKind::With)?;
        let distinct = self.eat_keyword(&TokenKind::Distinct);
        let items = self.parse_projection_items()?;
        let order_by = self.parse_order_by()?;
        let skip = if self.eat_keyword(&TokenKind::Skip) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let limit = if self.eat_keyword(&TokenKind::Limit) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let where_clause = if self.eat_keyword(&TokenKind::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(WithClause {
            distinct,
            items,
            order_by,
            skip,
            limit,
            where_clause,
        })
    }

    fn parse_projection_items(&mut self) -> Result<ProjectionItems, CypherError> {
        if self.eat(&TokenKind::Star) {
            return Ok(ProjectionItems::All);
        }
        let mut items = vec![self.parse_projection_item()?];
        while self.eat(&TokenKind::Comma) {
            items.push(self.parse_projection_item()?);
        }
        Ok(ProjectionItems::Explicit(items))
    }

    fn parse_projection_item(&mut self) -> Result<ProjectionItem, CypherError> {
        let start_span = self.tokens[self.pos].span.start;
        let expr = self.parse_expr()?;
        // original expression text, for column naming
        let source_text = if !self.source.is_empty() {
            let end_span = self.tokens[self.pos.saturating_sub(1)].span.end;
            let text = self.source[start_span..end_span].trim();
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        } else {
            None
        };
        let alias = if self.eat_keyword(&TokenKind::As) {
            Some(self.parse_symbolic_name()?)
        } else {
            None
        };
        Ok(ProjectionItem {
            expr,
            alias,
            source_text,
        })
    }

    pub(super) fn parse_order_by(&mut self) -> Result<Option<Vec<SortItem>>, CypherError> {
        if !self.eat_keyword(&TokenKind::Order) {
            return Ok(None);
        }
        self.expect(&TokenKind::By)?;
        let mut items = vec![self.parse_sort_item()?];
        while self.eat(&TokenKind::Comma) {
            items.push(self.parse_sort_item()?);
        }
        Ok(Some(items))
    }

    fn parse_sort_item(&mut self) -> Result<SortItem, CypherError> {
        let expr = self.parse_expr()?;
        let ascending = match self.peek() {
            TokenKind::Asc | TokenKind::Ascending => {
                self.advance();
                true
            }
            TokenKind::Desc | TokenKind::Descending => {
                self.advance();
                false
            }
            _ => true,
        };
        Ok(SortItem { expr, ascending })
    }
}
