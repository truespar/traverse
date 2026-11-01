//! Query statements and bodies: composition, subqueries, UNION parts and MATCH with its path
//! prefixes.

use crate::ast::{
    Clause, MatchClause, PathPattern, QueryBody, QueryStatement, UnionKind, UnionPart,
};
use crate::error::ParseError;
use crate::lexer::{Span, TokenKind};

use super::{Parser, body_has_write_clause, describe_token};

impl<'t> Parser<'t> {
    pub(super) fn parse_query_statement(&mut self) -> Result<QueryStatement, ParseError> {
        let start = self.peek().span.start;
        let head = self.parse_query_body()?;
        let mut unions = Vec::new();
        while self.peek_keyword("union") {
            unions.push(self.parse_union_part()?);
        }
        let end = self.previous_end();
        Ok(QueryStatement {
            head,
            unions,
            span: Span::new(start, end),
        })
    }

    /// At a keyword that closes the enclosing statement rather than starting a clause: `NEXT`, a
    /// transaction block's `COMMIT` / `ROLLBACK`, a conditional's `ELSE` / `END`.
    fn at_statement_end(&self) -> bool {
        ["next", "commit", "rollback", "else", "end"]
            .iter()
            .any(|kw| self.peek_keyword(kw))
    }

    fn parse_query_body(&mut self) -> Result<QueryBody, ParseError> {
        let start = self.peek().span.start;
        let mut clauses = Vec::new();
        loop {
            if self.is_optional_match() || self.peek_keyword("match") {
                clauses.push(Clause::Match(self.parse_match_clause()?));
            } else if self.peek_keyword("with") {
                clauses.push(Clause::With(self.parse_with_clause()?));
            } else if self.peek_keyword("unwind") {
                clauses.push(Clause::Unwind(self.parse_unwind_clause()?));
            } else if self.peek_keyword("for") {
                clauses.push(Clause::For(self.parse_for_clause()?));
            } else if self.peek_keyword("filter") {
                clauses.push(Clause::Filter(self.parse_filter_clause()?));
            } else if self.peek_keyword("let") {
                clauses.push(Clause::Let(self.parse_let_clause()?));
            } else if self.peek_keyword("insert") || self.peek_keyword("create") {
                // `CREATE` is a Cypher-compat alias for `INSERT`; same Insert clause AST
                clauses.push(Clause::Insert(self.parse_insert_clause()?));
            } else if self.peek_keyword("set") {
                clauses.push(Clause::Set(self.parse_set_clause()?));
            } else if self.peek_keyword("remove") {
                clauses.push(Clause::Remove(self.parse_remove_clause()?));
            } else if self.peek_keyword("delete") || self.is_detach_delete() {
                clauses.push(Clause::Delete(self.parse_delete_clause()?));
            } else if self.peek_keyword("call") {
                // `CALL { ... }` and `CALL (imports) { ... }` are the
                // inline-subquery forms (GP01 / GP03); `CALL name(args)`
                // is the named-procedure form. Disambiguate by
                // peeking past the optional import list.
                if self.is_call_subquery_form() {
                    clauses.push(Clause::CallSubquery(self.parse_call_subquery_clause()?));
                } else {
                    clauses.push(Clause::Call(self.parse_call_clause()?));
                }
            } else if self.peek_keyword("return") {
                clauses.push(Clause::Return(self.parse_return_clause()?));
                break;
            } else if self.peek_keyword("finish") {
                let span = self.peek().span;
                self.advance();
                clauses.push(Clause::Finish(span));
                break;
            } else if self.is_eof()
                || matches!(self.peek().kind, TokenKind::Semicolon)
                || self.peek_keyword("union")
                || self.at_statement_end()
            {
                // A write-only body may end without RETURN/FINISH.
                // Reject only if we never parsed anything.
                if clauses.is_empty() {
                    return Err(ParseError::unexpected_eof(
                        self.peek().span,
                        "expected a clause to begin a query body",
                    ));
                }
                if !body_has_write_clause(&clauses) {
                    return Err(ParseError::unexpected_token(
                        self.peek().span,
                        "read-only query is missing a `RETURN` or `FINISH` clause",
                    ));
                }
                break;
            } else {
                return Err(ParseError::unexpected_token(
                    self.peek().span,
                    format!(
                        "expected `MATCH`, `WITH`, `UNWIND`, `FOR`, `FILTER`, \
                         `INSERT`/`CREATE`, `SET`, `REMOVE`, `DELETE`, `CALL`, \
                         `RETURN`, or `FINISH`, found {}",
                        describe_token(&self.peek().kind),
                    ),
                ));
            }
        }
        let end = self.previous_end();
        Ok(QueryBody {
            clauses,
            span: Span::new(start, end),
        })
    }

    fn is_detach_delete(&self) -> bool {
        if !self.peek_keyword("detach") {
            return false;
        }
        matches!(
            self.tokens.get(self.cursor + 1).map(|t| &t.kind),
            Some(TokenKind::Keyword(k)) if *k == "delete"
        )
    }

    /// Body parser for `EXISTS { ... }` subqueries (ISO section 19.4). Same
    /// clauses as `parse_query_body` but terminates on `}`, allows the
    /// body to omit `RETURN`/`FINISH`, and supports the pattern-only
    /// shortcut `EXISTS { pattern }` by promoting a bare pattern to an
    /// implicit `MATCH pattern`.
    #[recursive::recursive]
    pub(super) fn parse_subquery_body(&mut self) -> Result<QueryBody, ParseError> {
        self.enter_nesting()?;
        let result = self.parse_subquery_body_inner();
        self.leave_nesting();
        result
    }

    fn parse_subquery_body_inner(&mut self) -> Result<QueryBody, ParseError> {
        let start = self.peek().span.start;
        let mut clauses = Vec::new();
        // Pattern-only shortcut: if the first token is `(` (the start
        // of a node pattern), wrap it in an implicit MATCH.
        if matches!(self.peek().kind, TokenKind::LParen) {
            let patterns = self.parse_path_pattern_list()?;
            let mc_end = self.previous_end();
            clauses.push(Clause::Match(crate::ast::MatchClause {
                optional: false,
                path_mode: crate::ast::PathMode::Default,
                path_selector: crate::ast::PathSelector::All,
                patterns,
                where_clause: None,
                span: Span::new(start, mc_end),
            }));
        }
        loop {
            if matches!(self.peek().kind, TokenKind::RBrace) {
                break;
            }
            if self.is_optional_match() || self.peek_keyword("match") {
                clauses.push(Clause::Match(self.parse_match_clause()?));
            } else if self.peek_keyword("with") {
                clauses.push(Clause::With(self.parse_with_clause()?));
            } else if self.peek_keyword("unwind") {
                clauses.push(Clause::Unwind(self.parse_unwind_clause()?));
            } else if self.peek_keyword("for") {
                clauses.push(Clause::For(self.parse_for_clause()?));
            } else if self.peek_keyword("filter") {
                clauses.push(Clause::Filter(self.parse_filter_clause()?));
            } else if self.peek_keyword("let") {
                clauses.push(Clause::Let(self.parse_let_clause()?));
            } else if self.peek_keyword("call") {
                if self.is_call_subquery_form() {
                    clauses.push(Clause::CallSubquery(self.parse_call_subquery_clause()?));
                } else {
                    clauses.push(Clause::Call(self.parse_call_clause()?));
                }
            } else if self.peek_keyword("return") {
                clauses.push(Clause::Return(self.parse_return_clause()?));
                break;
            } else if self.is_eof() {
                return Err(ParseError::unexpected_eof(
                    self.peek().span,
                    "unexpected end of input inside `EXISTS { ... }` subquery",
                ));
            } else {
                return Err(ParseError::unexpected_token(
                    self.peek().span,
                    format!(
                        "expected `MATCH`, `WITH`, `UNWIND`, `FOR`, `FILTER`, \
                         `CALL`, `RETURN`, or `}}`, found {}",
                        describe_token(&self.peek().kind),
                    ),
                ));
            }
        }
        if clauses.is_empty() {
            return Err(ParseError::unexpected_token(
                self.peek().span,
                "`EXISTS { }` requires at least one clause or a pattern".to_string(),
            ));
        }
        let end = self.previous_end();
        Ok(QueryBody {
            clauses,
            span: Span::new(start, end),
        })
    }

    /// Parse one or more `,`-separated path patterns. Shared between
    /// MATCH / INSERT clauses and the pattern-only shortcut inside
    /// `EXISTS { ... }`.
    fn parse_path_pattern_list(&mut self) -> Result<Vec<PathPattern>, ParseError> {
        let mut patterns = vec![self.parse_path_pattern()?];
        while self.consume_if(&TokenKind::Comma) {
            patterns.push(self.parse_path_pattern()?);
        }
        Ok(patterns)
    }

    fn parse_union_part(&mut self) -> Result<UnionPart, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("union")?;
        let kind = if self.peek_keyword("all") {
            self.advance();
            UnionKind::All
        } else if self.peek_keyword("distinct") {
            self.advance();
            UnionKind::Distinct
        } else {
            // Bare `UNION` is `UNION DISTINCT` per section 9.6.
            UnionKind::Distinct
        };
        let body = self.parse_query_body()?;
        let end = self.previous_end();
        Ok(UnionPart {
            kind,
            body,
            span: Span::new(start, end),
        })
    }

    // -- MATCH ----------------------------------------------------

    fn parse_match_clause(&mut self) -> Result<MatchClause, ParseError> {
        let start = self.peek().span.start;
        let optional = if self.peek_keyword("optional") {
            self.advance();
            true
        } else {
            false
        };
        self.expect_keyword("match")?;

        // optional path-mode / path-selector prefixes (ISO section 14.5-6), in either order (spec
        // convention: mode first); stored in canonical AST order
        let (path_mode, path_selector) = self.parse_match_path_prefixes()?;

        let mut patterns = vec![self.parse_path_pattern()?];
        while self.consume_if(&TokenKind::Comma) {
            patterns.push(self.parse_path_pattern()?);
        }

        let where_clause = if self.peek_keyword("where") {
            self.advance();
            Some(self.parse_expression()?)
        } else {
            None
        };

        let end = self.previous_end();
        Ok(MatchClause {
            optional,
            path_mode,
            path_selector,
            patterns,
            where_clause,
            span: Span::new(start, end),
        })
    }

    /// Optional `<path_mode>` and `<path_selector>` prefixes between `MATCH` and the first pattern
    /// (ISO/IEC 39075 section 14.5-6). Either may be absent; defaults `PathMode::Default` and
    /// `PathSelector::All`.
    fn parse_match_path_prefixes(
        &mut self,
    ) -> Result<(crate::ast::PathMode, crate::ast::PathSelector), ParseError> {
        use crate::ast::{PathMode, PathSelector};
        let mut mode = PathMode::Default;
        let mut selector = PathSelector::All;
        // either order, at most one of each
        loop {
            if matches!(mode, PathMode::Default) {
                if self.peek_keyword("walk") {
                    self.advance();
                    mode = PathMode::Walk;
                    continue;
                }
                if self.peek_keyword("trail") {
                    self.advance();
                    mode = PathMode::Trail;
                    continue;
                }
                if self.peek_keyword("acyclic") {
                    self.advance();
                    mode = PathMode::Acyclic;
                    continue;
                }
                // ISO/IEC 39075 section G002 / section G003: `DIFFERENT RELATIONSHIPS` and `REPEATABLE ELEMENTS`
                // are explicit match modes. DIFFERENT RELATIONSHIPS is the default (no edge revisit
                // within a path), i.e. `PathMode::Trail`
                if self.peek_keyword("different") {
                    let next = self.tokens.get(self.cursor + 1).map(|t| &t.kind);
                    if matches!(
                        next,
                        Some(TokenKind::Keyword(k)) if *k == "relationships"
                    ) {
                        self.advance(); // DIFFERENT
                        self.advance(); // RELATIONSHIPS
                        mode = PathMode::Trail;
                        continue;
                    }
                }
                if self.peek_keyword("repeatable") {
                    let next = self.tokens.get(self.cursor + 1).map(|t| &t.kind);
                    if matches!(
                        next,
                        Some(TokenKind::Keyword(k)) if *k == "elements"
                    ) {
                        self.advance(); // REPEATABLE
                        self.advance(); // ELEMENTS
                        // REPEATABLE ELEMENTS = WALK semantics
                        mode = PathMode::Walk;
                        continue;
                    }
                }
            }
            if matches!(selector, PathSelector::All) {
                if self.peek_keyword("shortest") {
                    selector = self.parse_shortest_tail()?;
                    continue;
                }
                // `ANY` opens several forms - `ANY`, `ANY k`, `ANY
                // SHORTEST`. Only consume `ANY` when followed by a
                // shortest-related continuation or an integer; bare
                // `ANY` is reserved for the `(a:ANY)` type predicate
                // form so we need 1-token disambiguation.
                if self.peek_keyword("any") {
                    let next = self.tokens.get(self.cursor + 1).map(|t| &t.kind);
                    let is_path_search = matches!(
                        next,
                        Some(TokenKind::Keyword(k))
                            if *k == "shortest"
                    ) || matches!(next, Some(TokenKind::Integer(_)));
                    // Plain `ANY` followed by `(pattern)` is the
                    // selector form per the spec.
                    let is_plain_any = matches!(next, Some(TokenKind::LParen));
                    if !(is_path_search || is_plain_any) {
                        break;
                    }
                    self.advance();
                    selector = self.parse_any_tail()?;
                    continue;
                }
                if self.peek_keyword("all") {
                    // `ALL SHORTEST` is the selector form; bare `ALL`
                    // is a quantifier elsewhere. Require the SHORTEST
                    // continuation here.
                    let next = self.tokens.get(self.cursor + 1).map(|t| &t.kind);
                    if matches!(
                        next,
                        Some(TokenKind::Keyword(k)) if *k == "shortest"
                    ) {
                        self.advance(); // ALL
                        self.expect_keyword("shortest")?;
                        // Optional `PATHS` keyword sugar - accept it
                        // if present.
                        if matches!(
                            self.peek().kind,
                            TokenKind::Keyword(k) if k == "paths" || k == "path"
                        ) {
                            self.advance();
                        }
                        selector = PathSelector::AllShortest;
                        continue;
                    }
                }
            }
            break;
        }
        Ok((mode, selector))
    }

    /// Tail of a selector that begins with `SHORTEST`. Consumes any of
    /// `SHORTEST`, `SHORTEST k`, `SHORTEST k GROUPS`, `SHORTEST PATH`,
    /// `SHORTEST PATHS`, plus the `GROUP` / `GROUPS` synonyms.
    fn parse_shortest_tail(&mut self) -> Result<crate::ast::PathSelector, ParseError> {
        use crate::ast::PathSelector;
        self.expect_keyword("shortest")?;
        // Optional integer count.
        let count: Option<u32> = if let TokenKind::Integer(n) = self.peek().kind {
            self.advance();
            Some(u32::try_from(n).map_err(|_| {
                ParseError::unexpected_token(
                    self.previous_span(),
                    "shortest-path count must fit in u32".to_string(),
                )
            })?)
        } else {
            None
        };
        // Optional `GROUPS` / `GROUP` keyword after the count.
        let groups = if self.peek_keyword("groups") || self.peek_keyword("group") {
            self.advance();
            true
        } else {
            false
        };
        // Optional `PATH` / `PATHS` sugar.
        if matches!(
            self.peek().kind,
            TokenKind::Keyword(k) if k == "paths" || k == "path"
        ) {
            self.advance();
        }
        match (count, groups) {
            (None, false) => Ok(PathSelector::Shortest),
            (Some(k), false) => Ok(PathSelector::ShortestN(k)),
            (Some(k), true) => Ok(PathSelector::ShortestGroups(k)),
            (None, true) => Err(ParseError::unexpected_token(
                self.previous_span(),
                "`SHORTEST GROUPS` requires an integer count".to_string(),
            )),
        }
    }

    /// Tail of a selector that begins with `ANY`. Consumes `SHORTEST`
    /// (-> `AnyShortest`), an integer (-> `AnyN`), or nothing (-> `Any`).
    /// The leading `ANY` has already been consumed.
    fn parse_any_tail(&mut self) -> Result<crate::ast::PathSelector, ParseError> {
        use crate::ast::PathSelector;
        if self.peek_keyword("shortest") {
            self.advance();
            if matches!(
                self.peek().kind,
                TokenKind::Keyword(k) if k == "paths" || k == "path"
            ) {
                self.advance();
            }
            return Ok(PathSelector::AnyShortest);
        }
        if let TokenKind::Integer(n) = self.peek().kind {
            self.advance();
            let n = u32::try_from(n).map_err(|_| {
                ParseError::unexpected_token(
                    self.previous_span(),
                    "ANY count must fit in u32".to_string(),
                )
            })?;
            return Ok(PathSelector::AnyN(n));
        }
        Ok(PathSelector::Any)
    }
}
