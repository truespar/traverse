//! Catalog and session statements: CREATE / DROP (graphs, graph types, schemas, indexes,
//! constraints), USE, transactions and WHEN.

use crate::ast::{ConstraintKind, SchemaPath, Statement, UseReference};
use crate::error::ParseError;
use crate::lexer::{Span, TokenKind};

use super::{Parser, describe_token};

impl<'t> Parser<'t> {
    pub(super) fn parse_create_statement(&mut self) -> Result<Statement, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("create")?;
        if self.peek_keyword("graph") {
            self.advance();
            if self.peek_keyword("type") {
                // `CREATE GRAPH TYPE path { body }`
                self.advance();
                let path = self.parse_schema_path()?;
                let body = self.parse_graph_type_body()?;
                let end = self.previous_end();
                return Ok(Statement::CreateGraphType {
                    path,
                    body,
                    span: Span::new(start, end),
                });
            }
            // Three surface forms diverge here:
            //   * Cypher-style: `CREATE GRAPH [IF NOT EXISTS] name`
            //     - no body, body field stays None.
            //   * GQL-native:   `CREATE GRAPH path body`
            //     - IF NOT EXISTS is not allowed here per ISO 39075;
            //       a body must follow.
            //   * Headless GQL: `CREATE GRAPH body` (no path) - used
            //     for anonymous-graph creation, e.g. `CREATE GRAPH
            //     ANY AS COPY OF other`.
            let if_not_exists = self.consume_if_not_exists()?;
            // If the next token starts a graph-type body keyword
            // (ANY, LIKE, `{`), this is the headless form: no name
            // segments, body is required.
            let (path, body) = if !if_not_exists && self.peek_starts_graph_body_keyword() {
                let pos = self.peek().span;
                let body = self.parse_graph_type_body()?;
                let empty_path = SchemaPath {
                    absolute: false,
                    segments: Vec::new(),
                    span: Span {
                        start: pos.start,
                        end: pos.start,
                    },
                };
                (empty_path, Some(body))
            } else {
                let path = self.parse_schema_path()?;
                let body = if if_not_exists || !self.peek_can_start_graph_body() {
                    None
                } else {
                    Some(self.parse_graph_type_body()?)
                };
                (path, body)
            };
            let end = self.previous_end();
            Ok(Statement::CreateGraph {
                path,
                if_not_exists,
                body,
                span: Span::new(start, end),
            })
        } else if self.peek_keyword("schema") {
            self.advance();
            let if_not_exists = self.consume_if_not_exists()?;
            let path = self.parse_schema_path()?;
            let end = self.previous_end();
            Ok(Statement::CreateSchema {
                path,
                if_not_exists,
                span: Span::new(start, end),
            })
        } else if self.peek_keyword("index") {
            self.advance();
            // Optional index name: present iff the next token is not
            // the `FOR` keyword.
            let name = if !self.peek_keyword("for") {
                Some(self.expect_name()?)
            } else {
                None
            };
            self.expect_keyword("for")?;
            let (_var, label) = self.parse_for_label_pattern()?;
            self.expect_keyword("on")?;
            let property = self.parse_on_property_pattern()?;
            let end = self.previous_end();
            Ok(Statement::CreateIndex {
                name,
                label,
                property,
                span: Span::new(start, end),
            })
        } else if self.peek_keyword("constraint") {
            self.advance();
            let name = if !self.peek_keyword("for") {
                Some(self.expect_name()?)
            } else {
                None
            };
            self.expect_keyword("for")?;
            let (_var, label) = self.parse_for_label_pattern()?;
            self.expect_keyword("require")?;
            let property = self.parse_postfix_property_path()?;
            self.expect_keyword("is")?;
            let kind = if self.peek_keyword("unique") {
                self.advance();
                ConstraintKind::Unique
            } else if self.peek_keyword("not") {
                self.advance();
                self.expect_keyword("null")?;
                ConstraintKind::NotNull
            } else {
                return Err(ParseError::unexpected_token(
                    self.peek().span,
                    format!(
                        "expected `UNIQUE` or `NOT NULL`, found {}",
                        describe_token(&self.peek().kind),
                    ),
                ));
            };
            let end = self.previous_end();
            Ok(Statement::CreateConstraint {
                name,
                label,
                property,
                kind,
                span: Span::new(start, end),
            })
        } else {
            Err(ParseError::unexpected_token(
                self.peek().span,
                format!(
                    "expected `GRAPH`, `INDEX`, or `CONSTRAINT` after `CREATE`, found {}",
                    describe_token(&self.peek().kind),
                ),
            ))
        }
    }

    pub(super) fn parse_drop_statement(&mut self) -> Result<Statement, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("drop")?;
        if self.peek_keyword("graph") {
            self.advance();
            if self.peek_keyword("type") {
                self.advance();
                let if_exists = self.consume_if_exists()?;
                let path = self.parse_schema_path()?;
                let end = self.previous_end();
                return Ok(Statement::DropGraphType {
                    path,
                    if_exists,
                    span: Span::new(start, end),
                });
            }
            let if_exists = self.consume_if_exists()?;
            let path = self.parse_schema_path()?;
            let end = self.previous_end();
            Ok(Statement::DropGraph {
                path,
                if_exists,
                span: Span::new(start, end),
            })
        } else if self.peek_keyword("schema") {
            self.advance();
            let if_exists = self.consume_if_exists()?;
            let path = self.parse_schema_path()?;
            let end = self.previous_end();
            Ok(Statement::DropSchema {
                path,
                if_exists,
                span: Span::new(start, end),
            })
        } else if self.peek_keyword("index") {
            self.advance();
            let name = self.expect_name()?;
            let end = self.previous_end();
            Ok(Statement::DropIndex {
                name,
                span: Span::new(start, end),
            })
        } else if self.peek_keyword("constraint") {
            self.advance();
            let name = self.expect_name()?;
            let end = self.previous_end();
            Ok(Statement::DropConstraint {
                name,
                span: Span::new(start, end),
            })
        } else {
            Err(ParseError::unexpected_token(
                self.peek().span,
                format!(
                    "expected `GRAPH`, `INDEX`, or `CONSTRAINT` after `DROP`, found {}",
                    describe_token(&self.peek().kind),
                ),
            ))
        }
    }

    pub(super) fn parse_use_statement(&mut self) -> Result<Statement, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("use")?;
        // First identifier - always required.
        let first = self.expect_name()?;
        // Collect any further `.segment` chain; later disambiguated
        // into either a Composite reference or a Dynamic reference
        // by the trailing `(args)` test.
        let mut segments = vec![first.clone()];
        while matches!(self.peek().kind, TokenKind::Dot) {
            self.advance();
            // The trailer of a dotted chain is just another name
            // segment; reserved keywords are not valid here.
            let seg = self.expect_name()?;
            segments.push(seg);
        }
        // If a `(` follows, this is a dynamic reference. Capture the
        // raw argument spans without lowering.
        let reference = if matches!(self.peek().kind, TokenKind::LParen) {
            self.advance();
            let mut arg_spans = Vec::new();
            if !matches!(self.peek().kind, TokenKind::RParen) {
                loop {
                    let arg_start = self.peek().span.start;
                    // Skip-the-argument: balanced parens + commas at
                    // depth 0 separate arguments. We avoid the full
                    // expression parser here because dynamic graph
                    // resolvers haven't been written.
                    let mut depth: u32 = 0;
                    loop {
                        if self.is_eof() {
                            return Err(ParseError::unexpected_eof(
                                self.peek().span,
                                "unterminated USE function argument list".to_string(),
                            ));
                        }
                        match self.peek().kind {
                            TokenKind::LParen => {
                                depth += 1;
                                self.advance();
                            }
                            TokenKind::RParen => {
                                if depth == 0 {
                                    let arg_end = self.peek().span.start;
                                    arg_spans.push(Span::new(arg_start, arg_end));
                                    break;
                                }
                                depth -= 1;
                                self.advance();
                            }
                            TokenKind::Comma if depth == 0 => {
                                let arg_end = self.peek().span.start;
                                arg_spans.push(Span::new(arg_start, arg_end));
                                break;
                            }
                            _ => self.advance(),
                        }
                    }
                    if matches!(self.peek().kind, TokenKind::Comma) {
                        self.advance();
                        continue;
                    }
                    break;
                }
            }
            self.expect_kind(
                TokenKind::RParen,
                "expected `)` to close USE function arguments",
            )?;
            UseReference::Dynamic {
                function: segments.clone(),
                arg_spans,
            }
        } else if segments.len() > 1 {
            UseReference::Composite {
                segments: segments.clone(),
            }
        } else {
            UseReference::Simple
        };
        let end = self.previous_end();
        Ok(Statement::Use {
            graph: first,
            reference,
            span: Span::new(start, end),
        })
    }

    /// Parse `(var:Label)` - returns `(variable, label)`. Used by
    /// `CREATE INDEX ... FOR (...)` and `CREATE CONSTRAINT ... FOR (...)`.
    fn parse_for_label_pattern(&mut self) -> Result<(String, String), ParseError> {
        self.expect_kind(TokenKind::LParen, "expected `(` after `FOR`")?;
        let variable = self.expect_name()?;
        self.expect_kind(TokenKind::Colon, "expected `:` before label")?;
        let label = self.expect_name()?;
        self.expect_kind(TokenKind::RParen, "expected `)` to close FOR pattern")?;
        Ok((variable, label))
    }

    /// Parse `(var.property)` - returns just the property key. Used by
    /// `CREATE INDEX ... ON (...)`.
    fn parse_on_property_pattern(&mut self) -> Result<String, ParseError> {
        self.expect_kind(TokenKind::LParen, "expected `(` after `ON`")?;
        let _var = self.expect_name()?;
        self.expect_kind(TokenKind::Dot, "expected `.` before property")?;
        let property = self.expect_name()?;
        self.expect_kind(TokenKind::RParen, "expected `)` to close ON pattern")?;
        Ok(property)
    }

    /// Parse `var.property` (without parentheses) - used by
    /// `CREATE CONSTRAINT ... REQUIRE var.prop ...`. Returns the property key.
    fn parse_postfix_property_path(&mut self) -> Result<String, ParseError> {
        let _var = self.expect_name()?;
        self.expect_kind(TokenKind::Dot, "expected `.` before property")?;
        self.expect_name()
    }

    pub(super) fn parse_transaction_statement(&mut self) -> Result<Statement, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("start")?;
        self.expect_keyword("transaction")?;
        // Optional access mode: `READ ONLY` | `READ WRITE`. Anything
        // else is treated as the default (read-write).
        let read_only = if self.peek_keyword("read") {
            self.advance();
            if self.peek_keyword("only") {
                self.advance();
                true
            } else if self.peek_keyword("write") {
                self.advance();
                false
            } else {
                false
            }
        } else {
            false
        };
        // Allow optional `;` / `NEXT` separators between the access-
        // mode line and the first inner statement.
        loop {
            if self.consume_if(&TokenKind::Semicolon) {
                continue;
            }
            if self.peek_keyword("next") {
                self.advance();
                continue;
            }
            break;
        }
        let mut statements = Vec::new();
        while !self.peek_keyword("commit") && !self.peek_keyword("rollback") && !self.is_eof() {
            statements.push(self.parse_statement()?);
            loop {
                if self.consume_if(&TokenKind::Semicolon) {
                    continue;
                }
                if self.peek_keyword("next") {
                    self.advance();
                    continue;
                }
                break;
            }
        }
        let rollback = if self.peek_keyword("commit") {
            false
        } else if self.peek_keyword("rollback") {
            true
        } else {
            return Err(ParseError::unexpected_token(
                self.peek().span,
                "expected `COMMIT` or `ROLLBACK` to close transaction".to_string(),
            ));
        };
        self.advance();
        let end = self.previous_end();
        Ok(Statement::Transaction {
            read_only,
            statements,
            rollback,
            span: Span::new(start, end),
        })
    }

    /// Parse ISO section 15.4 conditional statement:
    /// `WHEN <expr> THEN <stmt> [ELSE <stmt>] [END WHEN]`.
    /// The condition is a regular GQL expression; the branches are
    /// any single Statement. Optional `END WHEN` terminator per
    /// the spec grammar.
    pub(super) fn parse_when_statement(&mut self) -> Result<Statement, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("when")?;
        let condition = self.parse_expression()?;
        self.expect_keyword("then")?;
        let then_stmt = Box::new(self.parse_statement()?);
        let else_stmt = if self.peek_keyword("else") {
            self.advance();
            Some(Box::new(self.parse_statement()?))
        } else {
            None
        };
        // Optional `END WHEN` terminator. Both keywords must appear
        // together; a bare `END` is left for the outer parser
        // (it might be the end of an enclosing construct).
        if self.peek_keyword("end") {
            let save = self.cursor;
            self.advance();
            if self.peek_keyword("when") {
                self.advance();
            } else {
                self.cursor = save;
            }
        }
        let end = self.previous_end();
        Ok(Statement::When {
            condition,
            then_stmt,
            else_stmt,
            span: Span::new(start, end),
        })
    }

    fn consume_if_not_exists(&mut self) -> Result<bool, ParseError> {
        if self.peek_keyword("if") {
            self.advance();
            self.expect_keyword("not")?;
            self.expect_keyword("exists")?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn consume_if_exists(&mut self) -> Result<bool, ParseError> {
        if self.peek_keyword("if") {
            self.advance();
            self.expect_keyword("exists")?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}
