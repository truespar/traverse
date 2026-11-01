//! Recursive-descent openCypher parser: lexer `Vec<Token>` -> [`Statement`].

mod clause;
mod ddl;
mod expr;
mod pattern;
mod statement;

use crate::ast::{Expr, Statement};
use crate::error::CypherError;
use crate::lexer::{Token, TokenKind};

/// Recursive-descent parser: token stream -> Cypher AST ([`Statement`]).
pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Source text, for original-cased identifiers.
    source: String,
    /// Counter for parse-time synthesized variables (e.g. an anonymous node with a complex label
    /// expression, whose predicate needs a referent).
    fresh: u32,
    /// Current syntactic nesting (expressions, subqueries, label expressions, quantified path
    /// patterns), bounded by `stack::MAX_NESTING_DEPTH`.
    depth: u32,
    /// Deepest nesting and longest operator chain seen; `stack::with_stack_for_ast` sizes the
    /// execution stack from it.
    max_depth: u32,
    max_chain: usize,
    /// Pending SHOW pre-filter (typed `SHOW <TYPE> INDEXES`), set by the SHOW dispatcher, consumed
    /// by `parse_show_tail`.
    show_pre_filter: Option<Expr>,
}

impl Parser {
    /// Parser over a token stream.
    pub fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            pos: 0,
            source: String::new(),
            fresh: 0,
            depth: 0,
            max_depth: 0,
            max_chain: 0,
            show_pre_filter: None,
        }
    }

    /// Parser over a token stream with its source text.
    pub fn new_with_source(tokens: Vec<Token>, source: &str) -> Self {
        Self {
            tokens,
            pos: 0,
            source: source.to_string(),
            fresh: 0,
            depth: 0,
            max_depth: 0,
            max_chain: 0,
            show_pre_filter: None,
        }
    }

    // -- Helpers -------------------------------------------------------

    fn peek(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn peek_offset(&self, offset: usize) -> &TokenKind {
        let idx = self.pos + offset;
        if idx < self.tokens.len() {
            &self.tokens[idx].kind
        } else {
            &TokenKind::Eof
        }
    }

    /// Enters one nesting level (expression, subquery, label expression, quantified path pattern).
    /// Bounded by `stack::MAX_NESTING_DEPTH` so pathological input fails cleanly instead of growing
    /// the stack without limit.
    fn enter_nesting(&mut self) -> Result<(), CypherError> {
        if self.depth >= crate::stack::MAX_NESTING_DEPTH {
            return Err(CypherError::syntax(
                format!(
                    "query nesting too deep: more than {} nested expressions, subqueries or patterns",
                    crate::stack::MAX_NESTING_DEPTH
                ),
                self.span_pos(),
            ));
        }
        self.depth += 1;
        self.max_depth = self.max_depth.max(self.depth);
        Ok(())
    }

    fn leave_nesting(&mut self) {
        self.depth -= 1;
    }

    /// One more operand of a left-associative chain (`a OR b OR ...`). Chains build left-deep trees,
    /// so their length is the depth of every later walk (validation, planning, evaluation, drop);
    /// bounded by `stack::MAX_CHAIN_LENGTH`.
    fn count_chain_term(&mut self, terms: usize) -> Result<usize, CypherError> {
        if terms >= crate::stack::MAX_CHAIN_LENGTH {
            return Err(CypherError::syntax(
                format!(
                    "expression too long: more than {} chained operands",
                    crate::stack::MAX_CHAIN_LENGTH
                ),
                self.span_pos(),
            ));
        }
        self.max_chain = self.max_chain.max(terms + 1);
        Ok(terms + 1)
    }

    fn span_pos(&self) -> usize {
        self.tokens[self.pos].span.start
    }

    fn advance(&mut self) -> &Token {
        let t = &self.tokens[self.pos];
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, expected: &TokenKind) -> Result<&Token, CypherError> {
        if std::mem::discriminant(self.peek()) == std::mem::discriminant(expected) {
            Ok(self.advance())
        } else {
            Err(CypherError::syntax(
                format!("expected {:?}, found {:?}", expected, self.peek()),
                self.span_pos(),
            ))
        }
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if std::mem::discriminant(self.peek()) == std::mem::discriminant(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn eat_keyword(&mut self, kw: &TokenKind) -> bool {
        if self.peek() == kw {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Symbolic name: identifier or keyword in variable position.
    fn parse_symbolic_name(&mut self) -> Result<String, CypherError> {
        match self.peek().clone() {
            TokenKind::Ident(name) => {
                self.advance();
                Ok(name)
            }
            // many keywords are valid variable names in Cypher
            ref kw if is_keyword_as_ident(kw) => {
                // source text keeps the casing
                let name = if !self.source.is_empty() {
                    let span = &self.tokens[self.pos].span;
                    self.source[span.start..span.end].to_string()
                } else {
                    keyword_to_string(kw)
                };
                self.advance();
                Ok(name)
            }
            _ => Err(CypherError::syntax(
                format!("expected name, found {:?}", self.peek()),
                self.span_pos(),
            )),
        }
    }
}

/// Cypher 25 GQL aggregate aliases -> canonical lowercase names; others unchanged.
fn normalize_agg_alias(name: String) -> String {
    match name.as_str() {
        "collect_list" => "collect".to_string(),
        "percentile_cont" => "percentilecont".to_string(),
        "percentile_disc" => "percentiledisc".to_string(),
        "stdev_samp" => "stdev".to_string(),
        "stdev_pop" => "stdevp".to_string(),
        _ => name,
    }
}

/// Can this keyword be a variable name?
fn is_keyword_as_ident(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Count
            | TokenKind::Exists
            | TokenKind::All
            | TokenKind::Asc
            | TokenKind::Ascending
            | TokenKind::Desc
            | TokenKind::Descending
            | TokenKind::Null
            | TokenKind::True
            | TokenKind::False
            | TokenKind::And
            | TokenKind::Or
            | TokenKind::Xor
            | TokenKind::Not
            | TokenKind::In
            | TokenKind::Is
            | TokenKind::Contains
            | TokenKind::StartsWith
            | TokenKind::EndsWith
            | TokenKind::End
            | TokenKind::Case
            | TokenKind::When
            | TokenKind::Then
            | TokenKind::Else
            | TokenKind::On
            | TokenKind::By
            | TokenKind::Order
            | TokenKind::Skip
            | TokenKind::Limit
            | TokenKind::Distinct
            | TokenKind::Optional
            | TokenKind::Detach
            | TokenKind::Call
            | TokenKind::Yield
            | TokenKind::Index
            | TokenKind::Drop
            | TokenKind::For
            | TokenKind::Analyze
            | TokenKind::Graph
            | TokenKind::Load
            | TokenKind::Csv
    )
}

/// Keyword token -> identifier string.
fn keyword_to_string(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Count => "count".to_string(),
        TokenKind::Exists => "exists".to_string(),
        TokenKind::All => "all".to_string(),
        TokenKind::Asc => "asc".to_string(),
        TokenKind::Ascending => "ascending".to_string(),
        TokenKind::Desc => "desc".to_string(),
        TokenKind::Descending => "descending".to_string(),
        TokenKind::Null => "null".to_string(),
        TokenKind::True => "true".to_string(),
        TokenKind::False => "false".to_string(),
        TokenKind::And => "and".to_string(),
        TokenKind::Or => "or".to_string(),
        TokenKind::Xor => "xor".to_string(),
        TokenKind::Not => "not".to_string(),
        TokenKind::In => "in".to_string(),
        TokenKind::Is => "is".to_string(),
        TokenKind::Contains => "contains".to_string(),
        TokenKind::StartsWith => "starts".to_string(),
        TokenKind::EndsWith => "ends".to_string(),
        TokenKind::End => "end".to_string(),
        TokenKind::Case => "case".to_string(),
        TokenKind::When => "when".to_string(),
        TokenKind::Then => "then".to_string(),
        TokenKind::Else => "else".to_string(),
        TokenKind::On => "on".to_string(),
        TokenKind::By => "by".to_string(),
        TokenKind::Order => "order".to_string(),
        TokenKind::Skip => "skip".to_string(),
        TokenKind::Limit => "limit".to_string(),
        TokenKind::Distinct => "distinct".to_string(),
        TokenKind::Optional => "optional".to_string(),
        TokenKind::Detach => "detach".to_string(),
        TokenKind::Call => "call".to_string(),
        TokenKind::Yield => "yield".to_string(),
        TokenKind::Index => "index".to_string(),
        TokenKind::Drop => "drop".to_string(),
        TokenKind::For => "for".to_string(),
        TokenKind::Analyze => "analyze".to_string(),
        TokenKind::Graph => "graph".to_string(),
        TokenKind::Load => "load".to_string(),
        TokenKind::Csv => "csv".to_string(),
        _ => unreachable!(),
    }
}

/// Parses a Cypher query into its AST.
pub fn parse(input: &str) -> Result<Statement, CypherError> {
    parse_with_stats(input).map(|(stmt, _)| stmt)
}

/// Parses `input` and also returns the deepest nesting and longest operator chain seen. Later AST
/// walks (validation, planning, evaluation, derived `Clone`/`Drop`) recurse at most that deep, so
/// callers size their stack from it (`stack::with_stack_for_ast`).
pub fn parse_with_stats(input: &str) -> Result<(Statement, crate::stack::ParseDepth), CypherError> {
    let tokens = crate::lexer::tokenize(input)?;
    let mut parser = Parser::new_with_source(tokens, input);
    let stmt = parser.parse_statement()?;
    let depth = crate::stack::ParseDepth {
        nesting: parser.max_depth,
        chain: parser.max_chain,
    };
    Ok((stmt, depth))
}

#[cfg(test)]
mod tests;
