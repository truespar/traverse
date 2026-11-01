//! Expressions: a Pratt parser over the full precedence ladder, OR down to property access.

use crate::ast::{BinaryOp, CaseArm, Expression, TypeKind, UnaryOp};
use crate::error::ParseError;
use crate::lexer::{Span, TokenKind};

use super::{Parser, binary, describe_token};

impl<'t> Parser<'t> {
    //
    // Precedence ladder (lowest to highest):
    //   1. OR
    //   2. XOR
    //   3. AND
    //   4. NOT
    //   5. comparison: =, <>, !=, <, <=, >, >=
    //   6. additive: +, -
    //   7. multiplicative: *, /, %
    //   8. unary: -, +
    //   9. postfix: . (property access)
    //  10. primary: literal, variable, parameter, (expr)

    #[recursive::recursive]
    pub(super) fn parse_expression(&mut self) -> Result<Expression, ParseError> {
        self.enter_nesting()?;
        let result = self.parse_expression_inner();
        self.leave_nesting();
        result
    }

    fn parse_expression_inner(&mut self) -> Result<Expression, ParseError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expression, ParseError> {
        let mut left = self.parse_xor()?;
        let mut terms = 1usize;
        while self.peek_keyword("or") {
            terms = self.count_chain_term(terms)?;
            self.advance();
            let right = self.parse_xor()?;
            left = binary(BinaryOp::Or, left, right);
        }
        Ok(left)
    }

    fn parse_xor(&mut self) -> Result<Expression, ParseError> {
        let mut left = self.parse_and()?;
        let mut terms = 1usize;
        while self.peek_keyword("xor") {
            terms = self.count_chain_term(terms)?;
            self.advance();
            let right = self.parse_and()?;
            left = binary(BinaryOp::Xor, left, right);
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expression, ParseError> {
        let mut left = self.parse_not()?;
        let mut terms = 1usize;
        while self.peek_keyword("and") {
            terms = self.count_chain_term(terms)?;
            self.advance();
            let right = self.parse_not()?;
            left = binary(BinaryOp::And, left, right);
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expression, ParseError> {
        // `NOT NOT ... x`: consume the prefix iteratively (a long chain costs
        // no stack), then wrap from the inside out - right-associative.
        let mut starts = Vec::new();
        while self.peek_keyword("not") {
            self.count_chain_term(starts.len())?;
            starts.push(self.peek().span.start);
            self.advance();
        }
        let mut operand = self.parse_comparison()?;
        while let Some(start) = starts.pop() {
            let span = Span::new(start, operand.span().end);
            operand = Expression::Unary {
                op: UnaryOp::Not,
                operand: Box::new(operand),
                span,
            };
        }
        Ok(operand)
    }

    fn parse_comparison(&mut self) -> Result<Expression, ParseError> {
        // `IS [NOT] NULL` attaches to each comparison operand
        // independently - so `a IS NULL = b IS NULL` parses as
        // `(a IS NULL) = (b IS NULL)`, not as the equality
        // re-wrapped in another `IS NULL`. We delegate to a helper
        // that pulls in any trailing `IS NULL` on a single operand,
        // then chain comparison ops at this level.
        let mut left = self.parse_additive_with_is_null()?;
        while let Some(op) = self.peek_comparison_op() {
            self.advance();
            let right = self.parse_additive_with_is_null()?;
            left = binary(op, left, right);
        }
        Ok(left)
    }

    /// `parse_additive` followed by any number of postfix
    /// `IS [NOT] NULL` predicates (section 11.4). Returning here means the
    /// `IS NULL` binds to the additive expression, not to a
    /// comparison wrapping it. The same loop also handles the
    /// broader type-predicate forms `IS [NOT] <typename>`,
    /// `IS [NOT] LABELED <label_expr>`, and `IS [NOT] NORMALIZED
    /// [<form>]` (ISO/IEC 39075 section 19.5, section 19.6, section 19.7, G111).
    fn parse_additive_with_is_null(&mut self) -> Result<Expression, ParseError> {
        let mut e = self.parse_additive()?;
        while self.peek_keyword("is") {
            let start = e.span().start;
            self.advance(); // `IS`
            let negated = if self.peek_keyword("not") {
                self.advance();
                true
            } else {
                false
            };
            // After `IS [NOT]`, branch on the type-test name. `NULL`
            // is its own AST node; the rest fold into TypePredicate.
            if self.peek_keyword("null") {
                self.advance();
                let span = Span::new(start, self.previous_end());
                e = Expression::IsNull {
                    operand: Box::new(e),
                    negated,
                    span,
                };
                continue;
            }
            if self.peek_keyword("labeled") {
                self.advance();
                let label_expr = self.parse_label_expression()?;
                let span = Span::new(start, self.previous_end());
                e = Expression::TypePredicate {
                    operand: Box::new(e),
                    kind: TypeKind::Labeled(label_expr),
                    negated,
                    span,
                };
                continue;
            }
            if self.peek_keyword("normalized") {
                self.advance();
                // Optional form name. Per ISO section 19.7 the form keyword
                // sits between NORMALIZED and the operand chain; we
                // accept any of NFC / NFD / NFKC / NFKD (case-
                // insensitive) plus the explicit spellings the
                // grammar gives.
                let form = self.try_consume_normalization_form();
                let span = Span::new(start, self.previous_end());
                e = Expression::TypePredicate {
                    operand: Box::new(e),
                    kind: TypeKind::Normalized(form),
                    negated,
                    span,
                };
                continue;
            }
            let first = self.parse_type_kind_keyword()?;
            // GV66 / GV67 - dynamic union of value types. A trailing
            // `|` after a non-LABELED type name starts another
            // alternative; we accumulate until no more `|` follows.
            // The label-expression `|` inside `LABELED A|B` is
            // consumed by `parse_label_expression` above, so an outer
            // `|` only ever signals a type union here.
            let kind = if matches!(self.peek().kind, TokenKind::Pipe) {
                let mut alts = vec![first];
                while self.consume_if(&TokenKind::Pipe) {
                    let next = self.parse_type_kind_keyword()?;
                    alts.push(next);
                }
                TypeKind::Union(alts)
            } else {
                first
            };
            let span = Span::new(start, self.previous_end());
            e = Expression::TypePredicate {
                operand: Box::new(e),
                kind,
                negated,
                span,
            };
        }
        Ok(e)
    }

    /// Consume the type-name keywords for `IS [NOT] <type>`. Handles
    /// the multi-token spellings (`ANY NODE`, `ANY VERTEX`, `SIGNED
    /// INTEGER`, `LOCAL TIME`, `LOCAL DATETIME`, `ZONED TIME`,
    /// `ZONED DATETIME`) by lookahead.
    fn parse_type_kind_keyword(&mut self) -> Result<TypeKind, ParseError> {
        let tok = self.peek().clone();
        let kw = match &tok.kind {
            TokenKind::Keyword(k) => *k,
            _ => {
                return Err(ParseError::unexpected_token(
                    tok.span,
                    format!(
                        "expected a GQL type name after `IS [NOT]`, found {}",
                        describe_token(&tok.kind),
                    ),
                ));
            }
        };
        let kind = match kw {
            "boolean" | "bool" => {
                self.advance();
                TypeKind::Boolean
            }
            "string" | "varchar" => {
                self.advance();
                TypeKind::String
            }
            "int" | "integer" => {
                self.advance();
                TypeKind::Integer
            }
            "signed" => {
                self.advance();
                self.expect_keyword("integer")?;
                TypeKind::Integer
            }
            "float" => {
                self.advance();
                TypeKind::Float
            }
            "number" => {
                self.advance();
                TypeKind::Number
            }
            "node" | "vertex" => {
                self.advance();
                TypeKind::Node
            }
            "any" => {
                self.advance();
                // Disambiguate `ANY` from `ANY NODE` / `ANY VERTEX` /
                // `ANY EDGE` / `ANY RELATIONSHIP`.
                if self.peek_keyword("node") || self.peek_keyword("vertex") {
                    self.advance();
                    TypeKind::Node
                } else if self.peek_keyword("edge") || self.peek_keyword("relationship") {
                    self.advance();
                    TypeKind::Edge
                } else {
                    TypeKind::Any
                }
            }
            "edge" | "relationship" => {
                self.advance();
                TypeKind::Edge
            }
            "path" => {
                self.advance();
                TypeKind::Path
            }
            "list" | "array" => {
                self.advance();
                TypeKind::List
            }
            "map" => {
                self.advance();
                TypeKind::Map
            }
            "date" => {
                self.advance();
                TypeKind::Date
            }
            "duration" => {
                self.advance();
                TypeKind::Duration
            }
            "local" => {
                self.advance();
                if self.peek_keyword("time") {
                    self.advance();
                    TypeKind::LocalTime
                } else if self.peek_keyword("datetime") {
                    self.advance();
                    TypeKind::LocalDatetime
                } else {
                    return Err(ParseError::unexpected_token(
                        self.peek().span,
                        format!(
                            "expected `TIME` or `DATETIME` after `LOCAL`, found {}",
                            describe_token(&self.peek().kind),
                        ),
                    ));
                }
            }
            "local_time" => {
                self.advance();
                TypeKind::LocalTime
            }
            "local_datetime" => {
                self.advance();
                TypeKind::LocalDatetime
            }
            "zoned" => {
                self.advance();
                if self.peek_keyword("time") {
                    self.advance();
                    TypeKind::ZonedTime
                } else if self.peek_keyword("datetime") {
                    self.advance();
                    TypeKind::ZonedDatetime
                } else {
                    return Err(ParseError::unexpected_token(
                        self.peek().span,
                        format!(
                            "expected `TIME` or `DATETIME` after `ZONED`, found {}",
                            describe_token(&self.peek().kind),
                        ),
                    ));
                }
            }
            "zoned_time" => {
                self.advance();
                TypeKind::ZonedTime
            }
            "zoned_datetime" => {
                self.advance();
                TypeKind::ZonedDatetime
            }
            "time" => {
                // Bare `TIME` resolves to ZONED TIME per GQL - Cypher
                // / ISO `TIME` is zoned by default.
                self.advance();
                TypeKind::ZonedTime
            }
            "datetime" => {
                // Bare `DATETIME` likewise resolves to ZONED DATETIME.
                self.advance();
                TypeKind::ZonedDatetime
            }
            "nothing" => {
                self.advance();
                TypeKind::Nothing
            }
            other => {
                return Err(ParseError::unexpected_token(
                    tok.span,
                    format!("`{other}` is not a recognised GQL type name in `IS [NOT] <type>`",),
                ));
            }
        };
        Ok(kind)
    }

    /// Optional normalisation form for `IS NORMALIZED [<form>]`. The
    /// caller has already consumed `NORMALIZED`. Returns `None` if
    /// no form keyword follows.
    fn try_consume_normalization_form(&mut self) -> Option<String> {
        // The spec spells the forms as NFC / NFD / NFKC / NFKD; the
        // lexer treats them as plain identifiers (not keywords). So
        // we accept either an `Ident` or a `Keyword` whose text
        // matches one of the four. The compare is case-insensitive.
        let s = match &self.peek().kind {
            TokenKind::Ident(s) => Some(s.clone()),
            TokenKind::Keyword(k) => Some((*k).to_string()),
            _ => None,
        }?;
        let upper = s.to_ascii_uppercase();
        if matches!(upper.as_str(), "NFC" | "NFD" | "NFKC" | "NFKD") {
            self.advance();
            Some(upper)
        } else {
            None
        }
    }

    fn parse_additive(&mut self) -> Result<Expression, ParseError> {
        // String concatenation (`||`, ISO/IEC 39075 section 20.23) sits at
        // additive precedence - same level as `+` and `-`. ISO has
        // it left-associative, which falls out of the loop shape.
        let mut left = self.parse_multiplicative()?;
        let mut terms = 1usize;
        loop {
            let op = match self.peek().kind {
                TokenKind::Plus => BinaryOp::Add,
                TokenKind::Minus => BinaryOp::Sub,
                TokenKind::PipePipe => BinaryOp::Concat,
                _ => break,
            };
            self.advance();
            terms = self.count_chain_term(terms)?;
            let right = self.parse_multiplicative()?;
            left = binary(op, left, right);
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expression, ParseError> {
        let mut left = self.parse_unary()?;
        let mut terms = 1usize;
        loop {
            let op = match self.peek().kind {
                TokenKind::Star => BinaryOp::Mul,
                TokenKind::Slash => BinaryOp::Div,
                TokenKind::Percent => BinaryOp::Mod,
                _ => break,
            };
            self.advance();
            terms = self.count_chain_term(terms)?;
            let right = self.parse_unary()?;
            left = binary(op, left, right);
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expression, ParseError> {
        // `- - x` / `+ + x`: consume the prefix iteratively (a long chain
        // costs no stack). Unary `+` is a no-op; each `-` wraps a negation
        // from the inside out.
        let mut neg_starts = Vec::new();
        loop {
            match self.peek().kind {
                TokenKind::Minus => {
                    self.count_chain_term(neg_starts.len())?;
                    neg_starts.push(self.peek().span.start);
                    self.advance();
                }
                TokenKind::Plus => {
                    self.advance();
                }
                _ => break,
            }
        }
        let mut operand = self.parse_postfix()?;
        while let Some(start) = neg_starts.pop() {
            let span = Span::new(start, operand.span().end);
            operand = Expression::Unary {
                op: UnaryOp::Neg,
                operand: Box::new(operand),
                span,
            };
        }
        Ok(operand)
    }

    pub(super) fn parse_postfix(&mut self) -> Result<Expression, ParseError> {
        let mut e = self.parse_primary()?;
        loop {
            if self.consume_if(&TokenKind::Dot) {
                let name = self.expect_name()?;
                let span = Span::new(e.span().start, self.previous_end());
                e = Expression::Property {
                    base: Box::new(e),
                    name,
                    span,
                };
            } else if matches!(self.peek().kind, TokenKind::LBracket) {
                // `base[index]` - element access. (List literals are
                // handled in `parse_primary`; this branch only fires
                // when `[` follows an already-parsed expression.)
                self.advance();
                let index = self.parse_expression()?;
                self.expect_kind(
                    TokenKind::RBracket,
                    "expected `]` to close index expression",
                )?;
                let span = Span::new(e.span().start, self.previous_end());
                e = Expression::Index {
                    base: Box::new(e),
                    index: Box::new(index),
                    span,
                };
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn parse_primary(&mut self) -> Result<Expression, ParseError> {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::Integer(v) => {
                self.advance();
                Ok(Expression::IntegerLit {
                    value: v,
                    span: tok.span,
                })
            }
            TokenKind::Float(v) => {
                self.advance();
                Ok(Expression::FloatLit {
                    value: v,
                    span: tok.span,
                })
            }
            TokenKind::StringLit(s) => {
                self.advance();
                Ok(Expression::StringLit {
                    value: s,
                    span: tok.span,
                })
            }
            TokenKind::Keyword("true") => {
                self.advance();
                Ok(Expression::BoolLit {
                    value: true,
                    span: tok.span,
                })
            }
            TokenKind::Keyword("false") => {
                self.advance();
                Ok(Expression::BoolLit {
                    value: false,
                    span: tok.span,
                })
            }
            TokenKind::Keyword("null") => {
                self.advance();
                Ok(Expression::NullLit { span: tok.span })
            }
            TokenKind::Ident(n) => {
                self.advance();
                // `name(args)` is a function call; otherwise the
                // identifier is a variable reference.
                if matches!(self.peek().kind, TokenKind::LParen) {
                    self.parse_function_call_tail(n, tok.span.start)
                } else {
                    Ok(Expression::Variable {
                        name: n,
                        span: tok.span,
                    })
                }
            }
            TokenKind::DelimitedIdent(n) => {
                self.advance();
                if matches!(self.peek().kind, TokenKind::LParen) {
                    self.parse_function_call_tail(n, tok.span.start)
                } else {
                    Ok(Expression::Variable {
                        name: n,
                        span: tok.span,
                    })
                }
            }
            TokenKind::Parameter(n) | TokenKind::DelimitedParameter(n) => {
                self.advance();
                Ok(Expression::Parameter {
                    name: n,
                    span: tok.span,
                })
            }
            TokenKind::LParen => {
                let start = tok.span.start;
                self.advance();
                let inner = self.parse_expression()?;
                self.expect_kind(TokenKind::RParen, "expected `)` to close expression")?;
                let span = Span::new(start, self.previous_end());
                Ok(Expression::Paren {
                    inner: Box::new(inner),
                    span,
                })
            }
            TokenKind::LBracket => {
                // List literal: `[item, item, ...]`.
                let start = tok.span.start;
                self.advance();
                let mut items = Vec::new();
                if !matches!(self.peek().kind, TokenKind::RBracket) {
                    items.push(self.parse_expression()?);
                    while self.consume_if(&TokenKind::Comma) {
                        items.push(self.parse_expression()?);
                    }
                }
                self.expect_kind(TokenKind::RBracket, "expected `]` to close list literal")?;
                let span = Span::new(start, self.previous_end());
                Ok(Expression::List { items, span })
            }
            TokenKind::Keyword("case") => self.parse_case_expression(),
            // GQL parameterless noun-style expressions: temporal
            // keywords (section GV39 / section GV40) and `SESSION_USER` (section 20.3).
            // These are nouns at the surface (no parentheses) but
            // map to zero-arg function calls under the hood. Skip
            // this arm when the keyword is followed by `(` - the
            // next arm handles the explicit `name(args)` form which
            // the function table accepts for the same names.
            TokenKind::Keyword(k)
                if (k == "current_date"
                    || k == "current_time"
                    || k == "current_timestamp"
                    || k == "local_timestamp"
                    || k == "session_user")
                    && !matches!(
                        self.tokens.get(self.cursor + 1).map(|t| &t.kind),
                        Some(TokenKind::LParen)
                    ) =>
            {
                let name = (*k).to_string();
                let span = tok.span;
                self.advance();
                Ok(Expression::FunctionCall {
                    name,
                    distinct: false,
                    args: Vec::new(),
                    span,
                })
            }
            // `EXISTS { ... }` is the subquery form, `exists(value)` the single-arg function: `{` =>
            // subquery, `(` => call
            TokenKind::Keyword("exists") => {
                if matches!(
                    self.tokens.get(self.cursor + 1).map(|t| &t.kind),
                    Some(TokenKind::LBrace)
                ) {
                    let start = tok.span.start;
                    self.advance(); // EXISTS
                    self.expect_kind(
                        TokenKind::LBrace,
                        "expected `{` to open EXISTS subquery body",
                    )?;
                    let body = self.parse_subquery_body()?;
                    self.expect_kind(
                        TokenKind::RBrace,
                        "expected `}` to close EXISTS subquery body",
                    )?;
                    let end = self.previous_end();
                    Ok(Expression::ExistsSubquery {
                        body: Box::new(body),
                        span: Span::new(start, end),
                    })
                } else if matches!(
                    self.tokens.get(self.cursor + 1).map(|t| &t.kind),
                    Some(TokenKind::LParen)
                ) {
                    self.advance();
                    self.parse_function_call_tail("exists".to_string(), tok.span.start)
                } else {
                    // Bare `EXISTS` is not legal in expression position
                    // - fall through to the catch-all error.
                    Err(ParseError::unexpected_token(
                        tok.span,
                        "expected `{` or `(` after `EXISTS`".to_string(),
                    ))
                }
            }
            // ISO 39075 section 20.2 typed temporal literals: a temporal
            // type keyword followed by a single-quoted string is
            // shorthand for the corresponding constructor function
            // (`DATE '2024-01-01'` == `date('2024-01-01')`). Optional
            // feature GV39. Recognised before the function-call arm
            // because the matched pattern has a string literal where
            // a `(` would otherwise be required.
            TokenKind::Keyword(k)
                if matches!(
                    k,
                    "date"
                        | "time"
                        | "datetime"
                        | "local_time"
                        | "local_datetime"
                        | "zoned_time"
                        | "zoned_datetime"
                        | "duration"
                ) && matches!(
                    self.tokens.get(self.cursor + 1).map(|t| &t.kind),
                    Some(TokenKind::StringLit(_)),
                ) =>
            {
                let start = tok.span.start;
                let fn_name = (*k).to_string();
                self.advance(); // consume type keyword
                let lit_tok = self.peek().clone();
                let lit_value = match &lit_tok.kind {
                    TokenKind::StringLit(s) => s.clone(),
                    _ => unreachable!(),
                };
                self.advance(); // consume string literal
                let end = self.previous_end();
                // Lower as `<typename>('literal')` - the existing
                // constructor functions handle parsing.
                Ok(Expression::FunctionCall {
                    name: fn_name,
                    args: vec![Expression::StringLit {
                        value: lit_value,
                        span: lit_tok.span,
                    }],
                    distinct: false,
                    span: Span::new(start, end),
                })
            }
            // Reserved words that double as function names (`date`,
            // `time`, `local_time`, `zoned_datetime`, `normalize`, ...)
            // need to round-trip through expression position when
            // followed by `(`. ISO 39075 permits this - type/category
            // names act as constructor functions when called.
            TokenKind::Keyword(k)
                if self
                    .tokens
                    .get(self.cursor + 1)
                    .map(|t| matches!(t.kind, TokenKind::LParen))
                    .unwrap_or(false) =>
            {
                self.advance();
                self.parse_function_call_tail((*k).to_string(), tok.span.start)
            }
            TokenKind::LBrace => {
                // Map literal: `{}`, `{key: expr, ...}`. Keys are
                // names - plain idents, delimited idents, or keywords
                // used as names (see `expect_name`).
                let start = tok.span.start;
                self.advance();
                let mut entries = Vec::new();
                if !matches!(self.peek().kind, TokenKind::RBrace) {
                    loop {
                        let key = self.expect_name()?;
                        self.expect_kind(TokenKind::Colon, "expected `:` after map key")?;
                        let value = self.parse_expression()?;
                        entries.push((key, value));
                        if !self.consume_if(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect_kind(TokenKind::RBrace, "expected `}` to close map literal")?;
                let span = Span::new(start, self.previous_end());
                Ok(Expression::Map { entries, span })
            }
            _ => Err(ParseError::unexpected_token(
                tok.span,
                format!(
                    "expected an expression, found {}",
                    describe_token(&tok.kind)
                ),
            )),
        }
    }

    /// Parse a `CASE` expression (ISO/IEC 39075 section 20.7).
    ///
    /// Disambiguation between the simple and searched forms is by
    /// 1-token lookahead after `CASE`: if the next token is `WHEN`,
    /// it's a searched CASE; otherwise we parse an operand expression
    /// (simple CASE).
    fn parse_case_expression(&mut self) -> Result<Expression, ParseError> {
        let start = self.peek().span.start;
        self.expect_keyword("case")?;
        let operand = if self.peek_keyword("when") {
            None
        } else {
            Some(Box::new(self.parse_expression()?))
        };
        // At least one WHEN arm is required by the spec.
        let mut arms = Vec::new();
        while self.peek_keyword("when") {
            let arm_start = self.peek().span.start;
            self.advance(); // WHEN
            let when = self.parse_expression()?;
            self.expect_keyword("then")?;
            let then = self.parse_expression()?;
            let arm_end = self.previous_end();
            arms.push(CaseArm {
                when,
                then,
                span: Span::new(arm_start, arm_end),
            });
        }
        if arms.is_empty() {
            return Err(ParseError::unexpected_token(
                self.peek().span,
                format!(
                    "expected at least one `WHEN` arm in CASE expression, found {}",
                    describe_token(&self.peek().kind),
                ),
            ));
        }
        let else_ = if self.peek_keyword("else") {
            self.advance();
            Some(Box::new(self.parse_expression()?))
        } else {
            None
        };
        self.expect_keyword("end")?;
        let end = self.previous_end();
        Ok(Expression::Case {
            operand,
            arms,
            else_,
            span: Span::new(start, end),
        })
    }

    /// Called after a function-name token has been consumed and the
    /// next token is the opening `(`. `start` is the byte offset of
    /// the name, so the resulting span covers `name(args)`.
    fn parse_function_call_tail(
        &mut self,
        name: String,
        start: usize,
    ) -> Result<Expression, ParseError> {
        self.expect_kind(TokenKind::LParen, "expected `(` to open argument list")?;
        if name.eq_ignore_ascii_case("trim") {
            return self.parse_trim_args(name, start);
        }
        let distinct = if self.peek_keyword("distinct") {
            self.advance();
            true
        } else {
            false
        };
        // `count(*)` special case (ISO/IEC 39075 section 20.9). The wildcard
        // argument counts all rows including those with NULL values
        // - distinct from `count(x)` which excludes NULL `x`. We
        // represent it as `count` with an empty argument list, which
        // is what the executor's aggregate dispatcher already expects.
        if name.eq_ignore_ascii_case("count") && matches!(self.peek().kind, TokenKind::Star) {
            self.advance(); // consume `*`
            self.expect_kind(TokenKind::RParen, "expected `)` to close `count(*)`")?;
            let span = Span::new(start, self.previous_end());
            return Ok(Expression::FunctionCall {
                name,
                distinct,
                args: Vec::new(),
                span,
            });
        }
        let mut args = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RParen) {
            args.push(self.parse_expression()?);
            while self.consume_if(&TokenKind::Comma) {
                args.push(self.parse_expression()?);
            }
        }
        self.expect_kind(TokenKind::RParen, "expected `)` to close argument list")?;
        let span = Span::new(start, self.previous_end());
        Ok(Expression::FunctionCall {
            name,
            distinct,
            args,
            span,
        })
    }

    /// `TRIM` arguments after the `(`: ISO section 20.24 `[LEADING | TRAILING | BOTH] [<char>] FROM
    /// <source>`, which becomes `gql_trim(source, char, end)` (char defaults to a space, end to
    /// both), or the plain `trim(source [, characters])` call.
    ///
    /// `LEADING` / `TRAILING` / `BOTH` aren't reserved here, so they read as the trim end only
    /// when `FROM` or a trim character follows; elsewhere they stay ordinary names.
    fn parse_trim_args(&mut self, name: String, start: usize) -> Result<Expression, ParseError> {
        let end = match &self.peek().kind {
            TokenKind::Ident(word) => ["leading", "trailing", "both"]
                .into_iter()
                .find(|kw| word.eq_ignore_ascii_case(kw))
                .filter(|_| {
                    matches!(
                        self.tokens.get(self.cursor + 1).map(|t| &t.kind),
                        Some(
                            TokenKind::Keyword("from")
                                | TokenKind::StringLit(_)
                                | TokenKind::Parameter(_)
                                | TokenKind::DelimitedParameter(_)
                                | TokenKind::Ident(_)
                                | TokenKind::DelimitedIdent(_)
                        )
                    )
                }),
            _ => None,
        };
        if end.is_some() {
            self.advance();
        }
        let first = if self.peek_keyword("from") || matches!(self.peek().kind, TokenKind::RParen) {
            None
        } else {
            Some(self.parse_expression()?)
        };
        if end.is_none() && !self.peek_keyword("from") {
            // plain call
            let mut args: Vec<Expression> = first.into_iter().collect();
            while self.consume_if(&TokenKind::Comma) {
                args.push(self.parse_expression()?);
            }
            self.expect_kind(TokenKind::RParen, "expected `)` to close argument list")?;
            return Ok(Expression::FunctionCall {
                name,
                distinct: false,
                args,
                span: Span::new(start, self.previous_end()),
            });
        }
        if !self.peek_keyword("from") {
            return Err(ParseError::unexpected_token(
                self.peek().span,
                format!(
                    "expected `FROM` in `TRIM(...)`, found {}",
                    describe_token(&self.peek().kind)
                ),
            ));
        }
        self.advance();
        let source = self.parse_expression()?;
        self.expect_kind(TokenKind::RParen, "expected `)` to close `TRIM(...)`")?;
        let span = Span::new(start, self.previous_end());
        let trim_char = first.unwrap_or(Expression::StringLit {
            value: " ".into(),
            span,
        });
        Ok(Expression::FunctionCall {
            name: "gql_trim".into(),
            distinct: false,
            args: vec![
                source,
                trim_char,
                Expression::StringLit {
                    value: end.unwrap_or("both").into(),
                    span,
                },
            ],
            span,
        })
    }
}
