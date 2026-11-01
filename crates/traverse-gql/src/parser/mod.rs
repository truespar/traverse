//! GQL recursive-descent parser for the productions in `DESIGN.md` section 8.
//! Expressions use a Pratt parser over the full precedence ladder, `OR` down to
//! property access.
//!
//! Fails fast on the first error in a clause (a `Vec<ParseError>` with that one
//! diagnostic); no cross-clause recovery. Legal input round-trips, illegal
//! input gets an error at the right span.

mod catalog;
mod clause;
mod expr;
mod graph_type;
mod pattern;
mod query;

use crate::ast::{BinaryOp, Clause, Expression, NodePattern, Program, Statement};
use crate::error::ParseError;
use crate::lexer::{Span, Token, TokenKind, tokenize};

/// Parses `input` into a [`Program`]; on failure, every diagnostic found.
pub fn parse(input: &str) -> Result<Program, Vec<ParseError>> {
    parse_with_stats(input).map(|(program, _)| program)
}

/// Parses `input`, also returning the deepest nesting and longest operator chain seen: the AST
/// depth hint the executor sizes its stack from (see `traverse_cypher::stack::with_stack_for_ast`).
pub fn parse_with_stats(
    input: &str,
) -> Result<(Program, traverse_cypher::stack::ParseDepth), Vec<ParseError>> {
    let tokens = tokenize(input)?;
    let mut p = Parser::new(&tokens, input);
    let program = p.parse_program().map_err(|e| vec![e])?;
    let depth = traverse_cypher::stack::ParseDepth {
        nesting: p.max_depth,
        chain: p.max_chain,
    };
    Ok((program, depth))
}

struct Parser<'t> {
    tokens: &'t [Token],
    cursor: usize,
    /// Current syntactic nesting (expressions, subqueries, label
    /// expressions); bounded by `traverse_cypher::stack::MAX_NESTING_DEPTH`.
    depth: u32,
    /// Deepest nesting and longest operator chain seen (AST depth hint).
    max_depth: u32,
    max_chain: usize,
    /// Original query text. Keyword tokens carry only their lowercased canonical spelling, so names
    /// that happen to be keywords (`[:NEXT]`, `(:Value)`) are recovered from the source by span;
    /// otherwise they'd silently change case (edge types and labels are case-sensitive).
    source: &'t str,
}

impl<'t> Parser<'t> {
    fn new(tokens: &'t [Token], source: &'t str) -> Self {
        Self {
            tokens,
            cursor: 0,
            source,
            depth: 0,
            max_depth: 0,
            max_chain: 0,
        }
    }

    /// Enters one nesting level; bounded by `traverse_cypher::stack::MAX_NESTING_DEPTH` (shared
    /// with the Cypher parser, so both dialects fail the same way on pathological input).
    fn enter_nesting(&mut self) -> Result<(), ParseError> {
        if self.depth >= traverse_cypher::stack::MAX_NESTING_DEPTH {
            return Err(ParseError::unexpected_token(
                self.peek().span,
                format!(
                    "query nesting too deep: more than {} nested expressions, subqueries or patterns",
                    traverse_cypher::stack::MAX_NESTING_DEPTH
                ),
            ));
        }
        self.depth += 1;
        self.max_depth = self.max_depth.max(self.depth);
        Ok(())
    }

    fn leave_nesting(&mut self) {
        self.depth -= 1;
    }

    /// One more operand in a left-associative operator chain; bounded by
    /// `traverse_cypher::stack::MAX_CHAIN_LENGTH` (a chain is tree depth for every later walk).
    fn count_chain_term(&mut self, terms: usize) -> Result<usize, ParseError> {
        if terms >= traverse_cypher::stack::MAX_CHAIN_LENGTH {
            return Err(ParseError::unexpected_token(
                self.peek().span,
                format!(
                    "expression too long: more than {} chained operands",
                    traverse_cypher::stack::MAX_CHAIN_LENGTH
                ),
            ));
        }
        self.max_chain = self.max_chain.max(terms + 1);
        Ok(terms + 1)
    }

    // -- Top level -------------------------------------------------

    fn parse_program(&mut self) -> Result<Program, ParseError> {
        use crate::ast::{NextYieldItem, StatementSeparator};
        let start = self.peek().span.start;
        let mut statements = Vec::new();
        let mut separators: Vec<StatementSeparator> = Vec::new();
        while !self.is_eof() {
            statements.push(self.parse_statement()?);
            // Statements may be separated by `;` (terminator - no row
            // piping) or by `NEXT` (ISO 39075 section 9.3 linear composition
            // - pipes prior rows into the next statement). After NEXT
            // an optional `YIELD name [AS alias], ...` list restricts
            // which columns propagate; absent YIELD is implicit
            // YIELD * (every column passes through).
            //
            // When multiple separators appear back-to-back (e.g.
            // `S1 ; NEXT S2`), the strongest one wins - Next > Semi.
            // This matches the principle that an explicit NEXT
            // commits to piping regardless of stray semicolons.
            let mut sep_for_this: Option<StatementSeparator> = None;
            loop {
                if self.consume_if(&TokenKind::Semicolon) {
                    if sep_for_this.is_none() {
                        sep_for_this = Some(StatementSeparator::Semicolon);
                    }
                    continue;
                }
                if self.peek_keyword("next") {
                    self.advance();
                    let yields = if self.peek_keyword("yield") {
                        self.advance();
                        let mut items: Vec<NextYieldItem> = Vec::new();
                        loop {
                            if !matches!(
                                self.peek().kind,
                                TokenKind::Ident(_)
                                    | TokenKind::DelimitedIdent(_)
                                    | TokenKind::Keyword(_)
                            ) {
                                break;
                            }
                            let name = self.expect_name()?;
                            let alias = if self.peek_keyword("as") {
                                self.advance();
                                Some(self.expect_name()?)
                            } else {
                                None
                            };
                            items.push(NextYieldItem { name, alias });
                            if !self.consume_if(&TokenKind::Comma) {
                                break;
                            }
                        }
                        Some(items)
                    } else {
                        None
                    };
                    sep_for_this = Some(StatementSeparator::Next { yields });
                    continue;
                }
                break;
            }
            // Push the separator descriptor only when another
            // statement follows - the trailing one is implicit.
            if !self.is_eof() {
                separators.push(sep_for_this.unwrap_or(StatementSeparator::Semicolon));
            }
        }
        let end = self.previous_end();
        Ok(Program {
            statements,
            separators,
            span: Span::new(start, end),
        })
    }

    #[recursive::recursive]
    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        // Transaction blocks wrap inner statements; recognised before
        // the catalog dispatch so `START` doesn't fall into the query
        // path. The inner block ends at `COMMIT` or `ROLLBACK`.
        if self.peek_keyword("start") {
            return self.parse_transaction_statement();
        }
        // ISO section 15.4 statement-level conditional. The minimal form
        // `WHEN <expr> THEN <stmt> [ELSE <stmt>] [END WHEN]` -
        // recognised at statement position so the leading `WHEN`
        // doesn't get swept up by a query body's `CASE WHEN`
        // expression (which is always lexically nested under
        // `CASE`, so the disambiguation is straightforward).
        if self.peek_keyword("when") {
            return self.parse_when_statement();
        }
        // Catalog statements dispatch on the leading keyword; anything
        // else is a query statement.
        if self.peek_keyword("create") {
            // `CREATE GRAPH/SCHEMA/INDEX/CONSTRAINT ...` is the catalog
            // path; everything else - `CREATE (pattern)`,
            // `CREATE (a)-[:R]->(b)` - is openCypher-style data
            // creation, which we accept as a Cypher-compat alias for
            // `INSERT (pattern)` (the ISO 39075 form). The opengql/tck
            // suite is partly inherited from the openCypher TCK and
            // still uses `CREATE` in its data-write preambles, so
            // accepting it nearly doubles the executable subset.
            if self.peek_next_is_catalog_keyword() {
                return self.parse_create_statement();
            }
            return Ok(Statement::Query(self.parse_query_statement()?));
        }
        if self.peek_keyword("drop") {
            return self.parse_drop_statement();
        }
        if self.peek_keyword("use") {
            return self.parse_use_statement();
        }
        Ok(Statement::Query(self.parse_query_statement()?))
    }

    /// `true` if the token after `CREATE` is one of the catalog
    /// keywords (`GRAPH`, `SCHEMA`, `INDEX`, `CONSTRAINT`). Used to
    /// disambiguate `CREATE GRAPH foo` (catalog) from `CREATE (n)`
    /// (data-write Cypher-compat).
    fn peek_next_is_catalog_keyword(&self) -> bool {
        matches!(
            self.tokens.get(self.cursor + 1).map(|t| &t.kind),
            Some(TokenKind::Keyword(k))
                if *k == "graph" || *k == "schema" || *k == "index" || *k == "constraint"
        )
    }

    /// Span of the most recently consumed token (or a zero span if
    /// none).
    fn previous_span(&self) -> Span {
        if self.cursor == 0 {
            Span::new(0, 0)
        } else {
            self.tokens[self.cursor - 1].span
        }
    }

    // -- Token helpers --------------------------------------------

    fn peek(&self) -> &Token {
        // Safe: `tokenize` always emits an `Eof` token last.
        &self.tokens[self.cursor]
    }

    fn is_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    fn advance(&mut self) {
        if !self.is_eof() {
            self.cursor += 1;
        }
    }

    /// End-byte of the most recently consumed token, or 0 if none.
    fn previous_end(&self) -> usize {
        if self.cursor == 0 {
            0
        } else {
            self.tokens[self.cursor - 1].span.end
        }
    }

    fn peek_keyword(&self, kw: &str) -> bool {
        matches!(&self.peek().kind, TokenKind::Keyword(k) if *k == kw)
    }

    fn is_optional_match(&self) -> bool {
        if !self.peek_keyword("optional") {
            return false;
        }
        matches!(
            self.tokens.get(self.cursor + 1).map(|t| &t.kind),
            Some(TokenKind::Keyword(k)) if *k == "match"
        )
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), ParseError> {
        if self.peek_keyword(kw) {
            self.advance();
            Ok(())
        } else {
            Err(ParseError::unexpected_token(
                self.peek().span,
                format!(
                    "expected keyword `{}`, found {}",
                    kw.to_uppercase(),
                    describe_token(&self.peek().kind),
                ),
            ))
        }
    }

    fn expect_kind(&mut self, kind: TokenKind, message: &str) -> Result<(), ParseError> {
        if same_kind(&self.peek().kind, &kind) {
            self.advance();
            Ok(())
        } else {
            Err(ParseError::unexpected_token(
                self.peek().span,
                message.to_string(),
            ))
        }
    }

    fn consume_if(&mut self, kind: &TokenKind) -> bool {
        if same_kind(&self.peek().kind, kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Consume a name-position token: a regular identifier, a
    /// delimited identifier, or a reserved keyword used as a name
    /// (labels, property keys, aliases).
    fn expect_name(&mut self) -> Result<String, ParseError> {
        match &self.peek().kind {
            TokenKind::Ident(n) => {
                let s = n.clone();
                self.advance();
                Ok(s)
            }
            TokenKind::DelimitedIdent(n) => {
                let s = n.clone();
                self.advance();
                Ok(s)
            }
            TokenKind::Keyword(_) => {
                // Use the source spelling, not the lowercased canonical
                // keyword: `MATCH ()-[:NEXT]->()` names the edge type
                // NEXT, and names are case-sensitive.
                let span = self.peek().span;
                let s = self
                    .source
                    .get(span.start..span.end)
                    .map(str::to_string)
                    .unwrap_or_else(|| match &self.peek().kind {
                        TokenKind::Keyword(k) => (*k).to_string(),
                        _ => unreachable!(),
                    });
                self.advance();
                Ok(s)
            }
            _ => Err(ParseError::unexpected_token(
                self.peek().span,
                format!(
                    "expected a name, found {}",
                    describe_token(&self.peek().kind)
                ),
            )),
        }
    }

    fn peek_comparison_op(&self) -> Option<BinaryOp> {
        match self.peek().kind {
            TokenKind::Eq => Some(BinaryOp::Eq),
            TokenKind::Neq | TokenKind::NeqBang => Some(BinaryOp::Neq),
            TokenKind::Lt => Some(BinaryOp::Lt),
            TokenKind::Lte => Some(BinaryOp::Lte),
            TokenKind::Gt => Some(BinaryOp::Gt),
            TokenKind::Gte => Some(BinaryOp::Gte),
            _ => None,
        }
    }
}

fn body_has_write_clause(clauses: &[Clause]) -> bool {
    // CALL is included here because a procedure may be the only
    // result-producing clause in a query (e.g. `CALL db.labels() YIELD
    // label` - no RETURN needed).
    clauses.iter().any(|c| {
        matches!(
            c,
            Clause::Insert(_)
                | Clause::Set(_)
                | Clause::Remove(_)
                | Clause::Delete(_)
                | Clause::Call(_)
        )
    })
}

fn binary(op: BinaryOp, lhs: Expression, rhs: Expression) -> Expression {
    let span = Span::new(lhs.span().start, rhs.span().end);
    Expression::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
        span,
    }
}

/// Narrow an `i64` literal to `u32` for edge-quantifier bounds. The
/// span argument is the location of the *next* token (where the error
/// would surface) - passed by callers because the literal has already
/// been consumed by the time we'd want to report it.
fn u32_from_i64(value: i64, next_span: Span) -> Result<u32, ParseError> {
    u32::try_from(value).map_err(|_| {
        ParseError::unexpected_token(
            next_span,
            format!(
                "edge quantifier bound {value} out of range (0..={})",
                u32::MAX
            ),
        )
    })
}

/// Variant-only equality. Two `TokenKind`s match if they share a
/// discriminant - payload values are ignored, so callers can build a
/// sentinel like `TokenKind::Ident(String::new())` to mean "any Ident".
fn same_kind(a: &TokenKind, b: &TokenKind) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

/// Anonymous, unconstrained node pattern at `span`; anchors the implicit start of a quantified path
/// group written as `((sub)){m,n}` with no named start node.
fn anon_node_pattern(span: Span) -> NodePattern {
    NodePattern {
        variable: None,
        label_expr: None,
        properties: None,
        span,
    }
}

fn describe_token(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Eof => "end of input".to_string(),
        TokenKind::Keyword(k) => format!("keyword `{}`", k.to_uppercase()),
        TokenKind::Ident(n) => format!("identifier `{n}`"),
        TokenKind::DelimitedIdent(n) => format!("identifier `` `{n}` ``"),
        TokenKind::Integer(v) => format!("integer `{v}`"),
        TokenKind::OverflowInt(s) => format!("integer literal `{s}`"),
        TokenKind::Float(v) => format!("float `{v}`"),
        TokenKind::StringLit(s) => format!("string `{s:?}`"),
        TokenKind::Parameter(n) => format!("parameter `${n}`"),
        TokenKind::DelimitedParameter(n) => format!("parameter `$\"{n}\"`"),
        TokenKind::LParen => "`(`".into(),
        TokenKind::RParen => "`)`".into(),
        TokenKind::LBracket => "`[`".into(),
        TokenKind::RBracket => "`]`".into(),
        TokenKind::LBrace => "`{`".into(),
        TokenKind::RBrace => "`}`".into(),
        TokenKind::Dot => "`.`".into(),
        TokenKind::DotDot => "`..`".into(),
        TokenKind::Comma => "`,`".into(),
        TokenKind::Colon => "`:`".into(),
        TokenKind::ColonColon => "`::`".into(),
        TokenKind::Semicolon => "`;`".into(),
        TokenKind::Pipe => "`|`".into(),
        TokenKind::PipePipe => "`||`".into(),
        TokenKind::Ampersand => "`&`".into(),
        TokenKind::Bang => "`!`".into(),
        TokenKind::Plus => "`+`".into(),
        TokenKind::PlusEq => "`+=`".into(),
        TokenKind::Minus => "`-`".into(),
        TokenKind::Star => "`*`".into(),
        TokenKind::Slash => "`/`".into(),
        TokenKind::Percent => "`%`".into(),
        TokenKind::Caret => "`^`".into(),
        TokenKind::Eq => "`=`".into(),
        TokenKind::Neq => "`<>`".into(),
        TokenKind::NeqBang => "`!=`".into(),
        TokenKind::Lt => "`<`".into(),
        TokenKind::Lte => "`<=`".into(),
        TokenKind::Gt => "`>`".into(),
        TokenKind::Gte => "`>=`".into(),
        TokenKind::Arrow => "`->`".into(),
        TokenKind::LeftArrow => "`<-`".into(),
        TokenKind::Tilde => "`~`".into(),
        TokenKind::Question => "`?`".into(),
    }
}

// -- Tests ----------------------------------------------------------

#[cfg(test)]
mod tests;
