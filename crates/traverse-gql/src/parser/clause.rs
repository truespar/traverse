//! Clauses: INSERT, SET, REMOVE, DELETE, CALL, WITH, UNWIND, FOR, FILTER, LET and RETURN.

use crate::ast::{
    CallClause, DeleteClause, FilterClause, ForClause, InsertClause, OrderItem, RemoveClause,
    RemoveItem, ReturnClause, ReturnItem, SetClause, SetItem, SortDirection, UnwindClause,
    WithClause, YieldItem,
};
use crate::error::ParseError;
use crate::lexer::{Span, TokenKind};

use super::{Parser, describe_token};

impl<'t> Parser<'t> {
    pub(super) fn parse_insert_clause(&mut self) -> Result<InsertClause, ParseError> {
        let start = self.peek().span.start;
        // ISO 39075 spells the data-write keyword `INSERT`;
        // openCypher uses `CREATE`. Accept either so TCK preambles
        // (inherited from the openCypher suite) parse cleanly. The
        // caller (parse_statement / parse_query_body) has already
        // disambiguated `CREATE GRAPH` etc. from the data path, so
        // by the time we're here `CREATE` always means insert.
        if self.peek_keyword("insert") || self.peek_keyword("create") {
            self.advance();
        } else {
            return Err(ParseError::unexpected_token(
                self.peek().span,
                format!(
                    "expected `INSERT` or `CREATE` to open data-write clause, found {}",
                    describe_token(&self.peek().kind),
                ),
            ));
        }
        let mut patterns = vec![self.parse_path_pattern()?];
        while self.consume_if(&TokenKind::Comma) {
            patterns.push(self.parse_path_pattern()?);
        }
        let end = self.previous_end();
        Ok(InsertClause {
            patterns,
            span: Span::new(start, end),
        })
    }

    pub(super) fn parse_set_clause(&mut self) -> Result<SetClause, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("set")?;
        let mut items = vec![self.parse_set_item()?];
        while self.consume_if(&TokenKind::Comma) {
            items.push(self.parse_set_item()?);
        }
        let end = self.previous_end();
        Ok(SetClause {
            items,
            span: Span::new(start, end),
        })
    }

    /// One SET item. Three forms:
    /// * `target.prop = expr` or `target = expr` - property write
    /// * `target += expr` - merge map into properties
    /// * `target:Label[:Label ...]` - add labels
    fn parse_set_item(&mut self) -> Result<SetItem, ParseError> {
        let start = self.peek().span.start;
        let target = self.parse_postfix()?;
        // If the next token is `:`, this is a label-add item.
        if matches!(self.peek().kind, TokenKind::Colon) {
            let mut labels = Vec::new();
            while self.consume_if(&TokenKind::Colon) {
                labels.push(self.expect_name()?);
            }
            let span = Span::new(start, self.previous_end());
            return Ok(SetItem::Labels {
                target,
                labels,
                span,
            });
        }
        // Otherwise expect `=` or `+=` followed by an expression.
        match self.peek().kind {
            TokenKind::Eq => {
                self.advance();
                let value = self.parse_expression()?;
                let span = Span::new(start, self.previous_end());
                Ok(SetItem::Property {
                    target,
                    value,
                    span,
                })
            }
            TokenKind::PlusEq => {
                self.advance();
                let value = self.parse_expression()?;
                let span = Span::new(start, self.previous_end());
                Ok(SetItem::Merge {
                    target,
                    value,
                    span,
                })
            }
            _ => Err(ParseError::unexpected_token(
                self.peek().span,
                format!(
                    "expected `=`, `+=`, or `:` after SET target, found {}",
                    describe_token(&self.peek().kind),
                ),
            )),
        }
    }

    pub(super) fn parse_remove_clause(&mut self) -> Result<RemoveClause, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("remove")?;
        let mut items = vec![self.parse_remove_item()?];
        while self.consume_if(&TokenKind::Comma) {
            items.push(self.parse_remove_item()?);
        }
        let end = self.previous_end();
        Ok(RemoveClause {
            items,
            span: Span::new(start, end),
        })
    }

    fn parse_remove_item(&mut self) -> Result<RemoveItem, ParseError> {
        let start = self.peek().span.start;
        let target = self.parse_postfix()?;
        if matches!(self.peek().kind, TokenKind::Colon) {
            let mut labels = Vec::new();
            while self.consume_if(&TokenKind::Colon) {
                labels.push(self.expect_name()?);
            }
            let span = Span::new(start, self.previous_end());
            return Ok(RemoveItem::Labels {
                target,
                labels,
                span,
            });
        }
        // No `:` -> must be a bare property access (the target is
        // already a postfix expression, so the property part was
        // already consumed).
        let span = Span::new(start, self.previous_end());
        Ok(RemoveItem::Property { target, span })
    }

    /// `true` if the next clause-leading `CALL` is the inline-subquery
    /// form (`CALL { ... }` or `CALL (imports) { ... }`) rather than the
    /// named-procedure form (`CALL ns.proc(args)`). The leading `CALL`
    /// has not been consumed yet.
    pub(super) fn is_call_subquery_form(&self) -> bool {
        // 1 token past CALL: `{` => definitely subquery.
        if matches!(
            self.tokens.get(self.cursor + 1).map(|t| &t.kind),
            Some(TokenKind::LBrace)
        ) {
            return true;
        }
        // 1 token past CALL: `(` could be either:
        //   * named-call argument list `proc(args)` - but then the
        //     preceding token would be a name, not `CALL`;
        //   * import list `CALL (a, b) { ... }` for GP03.
        // Walk through the parenthesised list to see if it closes
        // with `) {` - that's the import-list form. Anything else
        // means the named-procedure path.
        if !matches!(
            self.tokens.get(self.cursor + 1).map(|t| &t.kind),
            Some(TokenKind::LParen)
        ) {
            return false;
        }
        let mut depth = 0usize;
        let mut i = self.cursor + 1;
        while i < self.tokens.len() {
            match &self.tokens[i].kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(
                            self.tokens.get(i + 1).map(|t| &t.kind),
                            Some(TokenKind::LBrace)
                        );
                    }
                }
                TokenKind::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// Parse the inline-subquery `CALL [(imports)] { body }` form.
    pub(super) fn parse_call_subquery_clause(
        &mut self,
    ) -> Result<crate::ast::CallSubqueryClause, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("call")?;
        let imports = if self.consume_if(&TokenKind::LParen) {
            let mut names = Vec::new();
            if !matches!(self.peek().kind, TokenKind::RParen) {
                names.push(self.expect_name()?);
                while self.consume_if(&TokenKind::Comma) {
                    names.push(self.expect_name()?);
                }
            }
            self.expect_kind(TokenKind::RParen, "expected `)` to close CALL import list")?;
            Some(names)
        } else {
            None
        };
        self.expect_kind(TokenKind::LBrace, "expected `{` to open CALL subquery body")?;
        let body = self.parse_subquery_body()?;
        self.expect_kind(
            TokenKind::RBrace,
            "expected `}` to close CALL subquery body",
        )?;
        let end = self.previous_end();
        Ok(crate::ast::CallSubqueryClause {
            imports,
            body: Box::new(body),
            span: Span::new(start, end),
        })
    }

    pub(super) fn parse_call_clause(&mut self) -> Result<CallClause, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("call")?;

        // Qualified name: `a.b.c`.
        let mut name = vec![self.expect_name()?];
        while self.consume_if(&TokenKind::Dot) {
            name.push(self.expect_name()?);
        }

        // Argument list. Parentheses are required by the spec but we
        // accept the elided form `CALL proc YIELD ...` for ergonomics
        // (matches Cypher behaviour).
        let mut args = Vec::new();
        if self.consume_if(&TokenKind::LParen) {
            if !matches!(self.peek().kind, TokenKind::RParen) {
                args.push(self.parse_expression()?);
                while self.consume_if(&TokenKind::Comma) {
                    args.push(self.parse_expression()?);
                }
            }
            self.expect_kind(
                TokenKind::RParen,
                "expected `)` to close CALL argument list",
            )?;
        }

        // Optional YIELD projection.
        let yields = if self.peek_keyword("yield") {
            self.advance();
            let mut items = vec![self.parse_yield_item()?];
            while self.consume_if(&TokenKind::Comma) {
                items.push(self.parse_yield_item()?);
            }
            items
        } else {
            Vec::new()
        };

        let end = self.previous_end();
        Ok(CallClause {
            name,
            args,
            yields,
            span: Span::new(start, end),
        })
    }

    fn parse_yield_item(&mut self) -> Result<YieldItem, ParseError> {
        let start = self.peek().span.start;
        let field = self.expect_name()?;
        let alias = if self.peek_keyword("as") {
            self.advance();
            Some(self.expect_name()?)
        } else {
            None
        };
        let end = self.previous_end();
        Ok(YieldItem {
            field,
            alias,
            span: Span::new(start, end),
        })
    }

    pub(super) fn parse_delete_clause(&mut self) -> Result<DeleteClause, ParseError> {
        let start = self.peek().span.start;
        let detach = if self.peek_keyword("detach") {
            self.advance();
            true
        } else {
            false
        };
        self.expect_keyword("delete")?;
        let mut targets = vec![self.parse_expression()?];
        while self.consume_if(&TokenKind::Comma) {
            targets.push(self.parse_expression()?);
        }
        let end = self.previous_end();
        Ok(DeleteClause {
            detach,
            targets,
            span: Span::new(start, end),
        })
    }

    // -- WITH / UNWIND --------------------------------------------

    pub(super) fn parse_with_clause(&mut self) -> Result<WithClause, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("with")?;

        let distinct = if self.peek_keyword("distinct") {
            self.advance();
            true
        } else {
            false
        };

        let mut items = vec![self.parse_return_item()?];
        while self.consume_if(&TokenKind::Comma) {
            items.push(self.parse_return_item()?);
        }

        // WITH's WHERE comes *after* the items (filters the projected
        // rows). It's optional.
        let where_clause = if self.peek_keyword("where") {
            self.advance();
            Some(self.parse_expression()?)
        } else {
            None
        };

        let order_by = if self.peek_keyword("order") {
            self.advance();
            self.expect_keyword("by")?;
            let mut keys = vec![self.parse_order_item()?];
            while self.consume_if(&TokenKind::Comma) {
                keys.push(self.parse_order_item()?);
            }
            keys
        } else {
            Vec::new()
        };

        let skip = if self.peek_keyword("skip") || self.peek_keyword("offset") {
            self.advance();
            Some(self.parse_expression()?)
        } else {
            None
        };

        let limit = if self.peek_keyword("limit") {
            self.advance();
            Some(self.parse_expression()?)
        } else {
            None
        };

        let end = self.previous_end();
        Ok(WithClause {
            distinct,
            items,
            where_clause,
            order_by,
            skip,
            limit,
            span: Span::new(start, end),
        })
    }

    pub(super) fn parse_unwind_clause(&mut self) -> Result<UnwindClause, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("unwind")?;
        let expr = self.parse_expression()?;
        self.expect_keyword("as")?;
        let alias = self.expect_name()?;
        let end = self.previous_end();
        Ok(UnwindClause {
            expr,
            alias,
            span: Span::new(start, end),
        })
    }

    pub(super) fn parse_for_clause(&mut self) -> Result<ForClause, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("for")?;
        let variable = self.expect_name()?;
        self.expect_keyword("in")?;
        let list = self.parse_expression()?;
        let end = self.previous_end();
        Ok(ForClause {
            variable,
            list,
            span: Span::new(start, end),
        })
    }

    pub(super) fn parse_filter_clause(&mut self) -> Result<FilterClause, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("filter")?;
        // `FILTER WHERE expr` and `FILTER expr` are both legal - the
        // optional `WHERE` is syntactic sugar that the AST collapses.
        if self.peek_keyword("where") {
            self.advance();
        }
        let predicate = self.parse_expression()?;
        let end = self.previous_end();
        Ok(FilterClause {
            predicate,
            span: Span::new(start, end),
        })
    }

    pub(super) fn parse_let_clause(&mut self) -> Result<crate::ast::LetClause, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("let")?;
        let mut bindings = Vec::new();
        loop {
            let name = self.expect_name()?;
            self.expect_kind(TokenKind::Eq, "expected `=` after LET binding name")?;
            let value = self.parse_expression()?;
            bindings.push((name, value));
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
        }
        let end = self.previous_end();
        Ok(crate::ast::LetClause {
            bindings,
            span: Span::new(start, end),
        })
    }

    // -- RETURN / ORDER BY / SKIP / LIMIT -------------------------

    pub(super) fn parse_return_clause(&mut self) -> Result<ReturnClause, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("return")?;

        let distinct = if self.peek_keyword("distinct") {
            self.advance();
            true
        } else {
            false
        };

        let mut items = vec![self.parse_return_item()?];
        while self.consume_if(&TokenKind::Comma) {
            items.push(self.parse_return_item()?);
        }

        let order_by = if self.peek_keyword("order") {
            self.advance();
            self.expect_keyword("by")?;
            let mut keys = vec![self.parse_order_item()?];
            while self.consume_if(&TokenKind::Comma) {
                keys.push(self.parse_order_item()?);
            }
            keys
        } else {
            Vec::new()
        };

        let skip = if self.peek_keyword("skip") || self.peek_keyword("offset") {
            self.advance();
            Some(self.parse_expression()?)
        } else {
            None
        };

        let limit = if self.peek_keyword("limit") {
            self.advance();
            Some(self.parse_expression()?)
        } else {
            None
        };

        let end = self.previous_end();
        Ok(ReturnClause {
            distinct,
            items,
            order_by,
            skip,
            limit,
            span: Span::new(start, end),
        })
    }

    fn parse_return_item(&mut self) -> Result<ReturnItem, ParseError> {
        let start = self.peek().span.start;
        let expr = self.parse_expression()?;
        let alias = if self.peek_keyword("as") {
            self.advance();
            Some(self.expect_name()?)
        } else {
            None
        };
        let end = self.previous_end();
        Ok(ReturnItem {
            expr,
            alias,
            span: Span::new(start, end),
        })
    }

    fn parse_order_item(&mut self) -> Result<OrderItem, ParseError> {
        let start = self.peek().span.start;
        let expr = self.parse_expression()?;
        let direction = if self.peek_keyword("asc") || self.peek_keyword("ascending") {
            self.advance();
            SortDirection::Asc
        } else if self.peek_keyword("desc") || self.peek_keyword("descending") {
            self.advance();
            SortDirection::Desc
        } else {
            SortDirection::Asc
        };
        let end = self.previous_end();
        Ok(OrderItem {
            expr,
            direction,
            span: Span::new(start, end),
        })
    }
}
