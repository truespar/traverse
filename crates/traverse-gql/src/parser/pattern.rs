//! Path patterns: nodes, edges, quantifiers and label expressions.

use crate::ast::{
    EdgeDirection, EdgePattern, EdgeQuantifier, LabelExpression, NodePattern, PathPattern,
    PropertyMap,
};
use crate::error::ParseError;
use crate::lexer::{Span, TokenKind};

use super::{Parser, anon_node_pattern, describe_token, u32_from_i64};

impl<'t> Parser<'t> {
    pub(super) fn parse_path_pattern(&mut self) -> Result<PathPattern, ParseError> {
        let start = self.peek().span.start;
        // Optional path-variable binding: `name = pattern` (ISO/IEC
        // 39075 section 16.9). Disambiguate by 2-token lookahead - the
        // identifier must be followed by `=` and then either `(` or
        // a path-grouping `(`. Reserved words used as identifiers
        // aren't legal here (Cypher behaviour) so we only accept
        // plain idents and delimited idents.
        let variable = if matches!(
            self.peek().kind,
            TokenKind::Ident(_) | TokenKind::DelimitedIdent(_)
        ) && matches!(
            self.tokens.get(self.cursor + 1).map(|t| &t.kind),
            Some(TokenKind::Eq)
        ) {
            let name = self.expect_name()?;
            self.expect_kind(TokenKind::Eq, "expected `=` after path variable name")?;
            Some(name)
        } else {
            None
        };

        // ISO section 16.7 parenthesised path pattern (optional features
        // G050 / G051): `( <pattern> [WHERE <pred>] )`. Disambiguate
        // from a node pattern by 2-token lookahead - node patterns
        // open with `(` then an identifier *or* a `:` / `{` / `)`;
        // a parenthesised path group opens with `(` then another `(`.
        let opens_path_group = matches!(self.peek().kind, TokenKind::LParen)
            && matches!(
                self.tokens.get(self.cursor + 1).map(|t| &t.kind),
                Some(TokenKind::LParen),
            );
        if opens_path_group {
            self.advance(); // consume outer `(`
            let first = self.parse_node_pattern()?;
            let mut chain: Vec<crate::ast::PathElement> = Vec::new();
            while matches!(self.peek().kind, TokenKind::Minus | TokenKind::LeftArrow) {
                let edge = self.parse_edge_pattern()?;
                let next = self.parse_node_pattern()?;
                chain.push(crate::ast::PathElement::Step { edge, node: next });
            }
            let inner_where = if self.peek_keyword("where") {
                self.advance();
                Some(self.parse_expression()?)
            } else {
                None
            };
            self.expect_kind(
                TokenKind::RParen,
                "expected `)` to close parenthesised path pattern",
            )?;
            // a trailing quantifier on the outer parenthesized group makes it a quantified path
            // pattern (ISO section 16.11 / G035); the inner pattern is re-emitted as a single-element
            // chain wrapped in a `Group`
            if let Some(quantifier) = self.try_parse_path_quantifier() {
                let end = self.previous_end();
                let inner = PathPattern {
                    variable: None,
                    start: first,
                    chain,
                    inner_where,
                    span: Span::new(start, end),
                };
                // the quantified group hangs off an implicit empty start; chain-level groups are
                // normally stitched in by `parse_path_pattern`'s loop below
                let group = crate::ast::PathElement::Group {
                    sub: Box::new(inner),
                    quantifier,
                    span: Span::new(start, end),
                };
                // anonymous start node so the PathPattern's chain has an anchor; lowering treats it
                // as "any node" for an unbound group
                return Ok(PathPattern {
                    variable,
                    start: anon_node_pattern(Span::new(start, start)),
                    chain: vec![group],
                    inner_where: None,
                    span: Span::new(start, end),
                });
            }
            let end = self.previous_end();
            return Ok(PathPattern {
                variable,
                start: first,
                chain,
                inner_where,
                span: Span::new(start, end),
            });
        }

        let first = self.parse_node_pattern()?;
        let mut chain: Vec<crate::ast::PathElement> = Vec::new();
        loop {
            // Edge step continuation.
            if matches!(self.peek().kind, TokenKind::Minus | TokenKind::LeftArrow) {
                let edge = self.parse_edge_pattern()?;
                let next = self.parse_node_pattern()?;
                chain.push(crate::ast::PathElement::Step { edge, node: next });
                continue;
            }
            // mid-chain quantified group `((sub)){m,n}`, detected by the same `((` opener as the
            // leading G050 form; without a trailing quantifier the `(` is left for the outer parser
            // to reject (bare mid-chain parenthesized patterns aren't legal ISO)
            let opens_group = matches!(self.peek().kind, TokenKind::LParen)
                && matches!(
                    self.tokens.get(self.cursor + 1).map(|t| &t.kind),
                    Some(TokenKind::LParen),
                );
            if opens_group {
                let group_start = self.peek().span.start;
                let save = self.cursor;
                self.advance(); // outer `(`
                let inner_first = match self.parse_node_pattern() {
                    Ok(n) => n,
                    Err(_) => {
                        self.cursor = save;
                        break;
                    }
                };
                let mut inner_chain: Vec<crate::ast::PathElement> = Vec::new();
                while matches!(self.peek().kind, TokenKind::Minus | TokenKind::LeftArrow) {
                    let edge = self.parse_edge_pattern()?;
                    let next = self.parse_node_pattern()?;
                    inner_chain.push(crate::ast::PathElement::Step { edge, node: next });
                }
                if !matches!(self.peek().kind, TokenKind::RParen) {
                    self.cursor = save;
                    break;
                }
                self.advance(); // close `)`
                let q = match self.try_parse_path_quantifier() {
                    Some(q) => q,
                    None => {
                        // mid-chain `((...))` without a quantifier isn't valid GQL here: restore, let
                        // the outer parser handle the next clause
                        self.cursor = save;
                        break;
                    }
                };
                let inner_end = self.previous_end();
                chain.push(crate::ast::PathElement::Group {
                    sub: Box::new(PathPattern {
                        variable: None,
                        start: inner_first,
                        chain: inner_chain,
                        inner_where: None,
                        span: Span::new(group_start, inner_end),
                    }),
                    quantifier: q,
                    span: Span::new(group_start, inner_end),
                });
                continue;
            }
            break;
        }
        let end = self.previous_end();
        Ok(PathPattern {
            variable,
            start: first,
            chain,
            inner_where: None,
            span: Span::new(start, end),
        })
    }

    /// Parse `{m,n}` / `{m}` / `+` / `*` / `?` into a
    /// [`crate::ast::PathQuantifier`]. Returns `None` if no
    /// quantifier follows.
    fn try_parse_path_quantifier(&mut self) -> Option<crate::ast::PathQuantifier> {
        match self.peek().kind {
            TokenKind::Plus => {
                self.advance();
                Some(crate::ast::PathQuantifier { min: 1, max: None })
            }
            TokenKind::Star => {
                self.advance();
                Some(crate::ast::PathQuantifier { min: 0, max: None })
            }
            TokenKind::Question => {
                self.advance();
                Some(crate::ast::PathQuantifier {
                    min: 0,
                    max: Some(1),
                })
            }
            TokenKind::LBrace => {
                // `{m}` or `{m,n}` - both bounds inclusive. Empty
                // bounds (`{,n}` / `{m,}`) are accepted with min=0
                // / max=None respectively.
                let save = self.cursor;
                self.advance();
                let min = if matches!(self.peek().kind, TokenKind::Integer(_)) {
                    let n = if let TokenKind::Integer(v) = self.peek().kind {
                        v
                    } else {
                        0
                    };
                    self.advance();
                    n as usize
                } else {
                    0
                };
                let max = if self.consume_if(&TokenKind::Comma) {
                    if matches!(self.peek().kind, TokenKind::Integer(_)) {
                        let n = if let TokenKind::Integer(v) = self.peek().kind {
                            v
                        } else {
                            0
                        };
                        self.advance();
                        Some(n as usize)
                    } else {
                        None
                    }
                } else {
                    Some(min)
                };
                if !self.consume_if(&TokenKind::RBrace) {
                    self.cursor = save;
                    return None;
                }
                Some(crate::ast::PathQuantifier { min, max })
            }
            _ => None,
        }
    }

    fn parse_node_pattern(&mut self) -> Result<NodePattern, ParseError> {
        let start = self.peek().span.start;
        self.expect_kind(TokenKind::LParen, "expected `(` to open node pattern")?;

        let variable = if matches!(
            self.peek().kind,
            TokenKind::Ident(_) | TokenKind::DelimitedIdent(_)
        ) {
            Some(self.expect_name()?)
        } else {
            None
        };

        let label_expr = if self.consume_if(&TokenKind::Colon) {
            Some(self.parse_label_expression()?)
        } else {
            None
        };

        let properties = if matches!(self.peek().kind, TokenKind::LBrace) {
            Some(self.parse_property_map()?)
        } else {
            None
        };

        self.expect_kind(TokenKind::RParen, "expected `)` to close node pattern")?;
        let end = self.previous_end();
        Ok(NodePattern {
            variable,
            label_expr,
            properties,
            span: Span::new(start, end),
        })
    }

    fn parse_edge_pattern(&mut self) -> Result<EdgePattern, ParseError> {
        let start = self.peek().span.start;
        // Leading token: `-` or `<-`.
        let leading_left = match self.peek().kind {
            TokenKind::Minus => {
                self.advance();
                false
            }
            TokenKind::LeftArrow => {
                self.advance();
                true
            }
            _ => {
                return Err(ParseError::unexpected_token(
                    self.peek().span,
                    "expected `-` or `<-` starting an edge pattern",
                ));
            }
        };

        // Optional `[...]` body: variable, type-expression, quantifier,
        // properties. All four are themselves optional.
        let (variable, label_expr, quantifier, properties) =
            if self.consume_if(&TokenKind::LBracket) {
                let variable = if matches!(
                    self.peek().kind,
                    TokenKind::Ident(_) | TokenKind::DelimitedIdent(_)
                ) {
                    Some(self.expect_name()?)
                } else {
                    None
                };
                let label_expr = if self.consume_if(&TokenKind::Colon) {
                    Some(self.parse_label_expression()?)
                } else {
                    None
                };
                let quantifier = if matches!(self.peek().kind, TokenKind::Star) {
                    Some(self.parse_edge_quantifier()?)
                } else {
                    None
                };
                let properties = if matches!(self.peek().kind, TokenKind::LBrace) {
                    Some(self.parse_property_map()?)
                } else {
                    None
                };
                self.expect_kind(TokenKind::RBracket, "expected `]` to close edge body")?;
                (variable, label_expr, quantifier, properties)
            } else {
                (None, None, None, None)
            };

        // Trailing token: combine with the leading one to determine
        // direction.
        let direction = match (leading_left, &self.peek().kind) {
            // `<-[...]-` -> incoming
            (true, TokenKind::Minus) => {
                self.advance();
                EdgeDirection::Incoming
            }
            // `-[...]->` -> outgoing
            (false, TokenKind::Arrow) => {
                self.advance();
                EdgeDirection::Outgoing
            }
            // `-[...]-` -> undirected
            (false, TokenKind::Minus) => {
                self.advance();
                EdgeDirection::Undirected
            }
            (true, _) => {
                return Err(ParseError::unexpected_token(
                    self.peek().span,
                    "expected `-` to close incoming edge pattern",
                ));
            }
            (false, _) => {
                return Err(ParseError::unexpected_token(
                    self.peek().span,
                    "expected `->` or `-` to close edge pattern",
                ));
            }
        };

        let end = self.previous_end();
        Ok(EdgePattern {
            variable,
            label_expr,
            direction,
            quantifier,
            properties,
            span: Span::new(start, end),
        })
    }

    // -- Label expressions & edge quantifiers ---------------------

    /// `expr ::= or_expr`
    /// `or_expr ::= and_expr ( '|' and_expr )*`
    /// `and_expr ::= unary ( '&' unary )*`
    /// `unary ::= '!' unary | primary`
    /// `primary ::= '(' expr ')' | '%' | NAME`
    #[recursive::recursive]
    pub(super) fn parse_label_expression(&mut self) -> Result<LabelExpression, ParseError> {
        self.enter_nesting()?;
        let result = self.parse_label_expression_inner();
        self.leave_nesting();
        result
    }

    fn parse_label_expression_inner(&mut self) -> Result<LabelExpression, ParseError> {
        self.parse_label_or()
    }

    fn parse_label_or(&mut self) -> Result<LabelExpression, ParseError> {
        let first = self.parse_label_and()?;
        let mut operands = vec![first];
        while self.consume_if(&TokenKind::Pipe) {
            operands.push(self.parse_label_and()?);
        }
        if operands.len() == 1 {
            Ok(operands.pop().unwrap())
        } else {
            let span = Span::new(
                operands[0].span().start,
                operands.last().unwrap().span().end,
            );
            Ok(LabelExpression::Disjunction { operands, span })
        }
    }

    fn parse_label_and(&mut self) -> Result<LabelExpression, ParseError> {
        let first = self.parse_label_unary()?;
        let mut operands = vec![first];
        while self.consume_if(&TokenKind::Ampersand) {
            operands.push(self.parse_label_unary()?);
        }
        if operands.len() == 1 {
            Ok(operands.pop().unwrap())
        } else {
            let span = Span::new(
                operands[0].span().start,
                operands.last().unwrap().span().end,
            );
            Ok(LabelExpression::Conjunction { operands, span })
        }
    }

    fn parse_label_unary(&mut self) -> Result<LabelExpression, ParseError> {
        // `!!!A`: count the bangs iteratively, wrap from the inside out.
        let mut starts = Vec::new();
        while matches!(self.peek().kind, TokenKind::Bang) {
            self.count_chain_term(starts.len())?;
            starts.push(self.peek().span.start);
            self.advance();
        }
        let mut operand = self.parse_label_primary()?;
        while let Some(start) = starts.pop() {
            let span = Span::new(start, operand.span().end);
            operand = LabelExpression::Negation {
                operand: Box::new(operand),
                span,
            };
        }
        Ok(operand)
    }

    fn parse_label_primary(&mut self) -> Result<LabelExpression, ParseError> {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::LParen => {
                self.advance();
                let inner = self.parse_label_expression()?;
                self.expect_kind(TokenKind::RParen, "expected `)` to close label expression")?;
                Ok(inner)
            }
            TokenKind::Percent => {
                self.advance();
                Ok(LabelExpression::Wildcard { span: tok.span })
            }
            TokenKind::Ident(_) | TokenKind::DelimitedIdent(_) | TokenKind::Keyword(_) => {
                let start = tok.span.start;
                let name = self.expect_name()?;
                let end = self.previous_end();
                Ok(LabelExpression::Label {
                    name,
                    span: Span::new(start, end),
                })
            }
            _ => Err(ParseError::unexpected_token(
                tok.span,
                format!(
                    "expected a label name, `%`, `!`, or `(`, found {}",
                    describe_token(&tok.kind)
                ),
            )),
        }
    }

    /// `*` | `*N` | `*N..` | `*..M` | `*N..M`
    fn parse_edge_quantifier(&mut self) -> Result<EdgeQuantifier, ParseError> {
        let start = self.peek().span.start;
        self.expect_kind(TokenKind::Star, "expected `*` to open edge quantifier")?;

        // Parse optional lower bound.
        let min = match self.peek().kind {
            TokenKind::Integer(v) => {
                self.advance();
                Some(u32_from_i64(v, self.peek().span)?)
            }
            _ => None,
        };

        // If we see `..`, parse the optional upper bound after it.
        // Otherwise the quantifier is exact (`*N` -> min=max=N) or
        // open (`*` -> both None).
        let (min, max) = if self.consume_if(&TokenKind::DotDot) {
            let max = match self.peek().kind {
                TokenKind::Integer(v) => {
                    self.advance();
                    Some(u32_from_i64(v, self.peek().span)?)
                }
                _ => None,
            };
            (min, max)
        } else if let Some(n) = min {
            // `*N` is exactly N.
            (Some(n), Some(n))
        } else {
            (None, None)
        };

        let end = self.previous_end();
        Ok(EdgeQuantifier {
            min,
            max,
            span: Span::new(start, end),
        })
    }

    fn parse_property_map(&mut self) -> Result<PropertyMap, ParseError> {
        let start = self.peek().span.start;
        self.expect_kind(TokenKind::LBrace, "expected `{` to open property map")?;
        let mut entries = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RBrace) {
            loop {
                let key = self.expect_name()?;
                self.expect_kind(TokenKind::Colon, "expected `:` after property key")?;
                let value = self.parse_expression()?;
                entries.push((key, value));
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect_kind(TokenKind::RBrace, "expected `}` to close property map")?;
        let end = self.previous_end();
        Ok(PropertyMap {
            entries,
            span: Span::new(start, end),
        })
    }
}
