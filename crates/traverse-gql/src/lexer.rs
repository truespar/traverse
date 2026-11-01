//! GQL lexer: source text -> [`Token`]s per ISO/IEC 39075 section 6 ("Language
//! elements"). Hand-written, byte-offset based, tolerant of malformed input: it
//! emits a [`ParseError`] per problem and keeps going, so editors get every
//! diagnostic in one pass.
//!
//! Token categories:
//!
//! * Literals: [`TokenKind::Integer`], [`TokenKind::Float`],
//!   [`TokenKind::StringLit`].
//! * Identifiers: [`TokenKind::Ident`] (regular),
//!   [`TokenKind::DelimitedIdent`] (backtick).
//! * Parameters: [`TokenKind::Parameter`] (`$name`),
//!   [`TokenKind::DelimitedParameter`] (`$"name"`).
//! * Reserved keywords: [`TokenKind::Keyword`] holding a `&'static str` from
//!   [`RESERVED_WORDS`], no allocation.
//! * Punctuation & operators: one variant per token.
//!
//! Trivia (whitespace, line and block comments) is dropped, as in
//! `traverse-cypher` and openCypher.

use crate::error::ParseError;

/// Byte-offset range into the source; same convention as `traverse-cypher`, so errors read the same
/// in both dialects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    /// Start byte offset (inclusive).
    pub start: usize,
    /// End byte offset (exclusive).
    pub end: usize,
}

impl Span {
    /// Span from inclusive start and exclusive end byte offsets.
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Zero-width span at `pos`, for diagnostics pointing at a position, not a range ("expected
    /// `RETURN` here").
    pub fn at(pos: usize) -> Self {
        Self {
            start: pos,
            end: pos,
        }
    }
}

/// One lexical token.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// The kind of token.
    pub kind: TokenKind,
    /// Source span (byte offsets).
    pub span: Span,
}

/// All GQL token kinds. `Keyword` carries a `&'static str` into [`RESERVED_WORDS`], so the parser
/// matches lowercased keyword text without a per-token allocation.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum TokenKind {
    // -- Literals (section 6.6, section 6.7) --------------------------------------
    /// Integer literal in `i64` range.
    Integer(i64),
    /// Integer literal outside `i64`, kept as raw digits (no sign / radix prefix) for the parser to
    /// handle at semantic analysis.
    OverflowInt(String),
    /// Floating-point literal. Never `NaN`; an infinite result is a
    /// lex error so `PartialEq` stays meaningful.
    Float(f64),
    /// String literal contents (with escape sequences resolved).
    StringLit(String),

    // -- Identifiers & parameters (section 6.4, section 6.5) ----------------------
    /// Regular identifier. Guaranteed not to match any entry in
    /// [`RESERVED_WORDS`] (case-insensitively) - reserved spellings
    /// land in [`TokenKind::Keyword`] instead.
    Ident(String),
    /// Backtick-delimited identifier. Always carries its exact spelling
    /// (case-sensitive), even if that spelling is a reserved word -
    /// the delimited form is GQL's escape hatch for reserved spellings.
    DelimitedIdent(String),
    /// `$name` parameter reference.
    Parameter(String),
    /// `$"name"` delimited parameter reference (allows spaces and
    /// reserved spellings in the parameter name).
    DelimitedParameter(String),

    // -- Reserved keywords (section 6.2) -----------------------------------
    /// A reserved word. The static slice points into [`RESERVED_WORDS`]
    /// so the parser can match on it directly: `Keyword("match") => ...`.
    Keyword(&'static str),

    // -- Punctuation ------------------------------------------------
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `.`
    Dot,
    /// `..` - range, used in path-quantifier syntax (`*1..3`).
    DotDot,
    /// `,`
    Comma,
    /// `:`
    Colon,
    /// `::` - type cast operator.
    ColonColon,
    /// `;`
    Semicolon,
    /// `|` - pattern alternation / label disjunction.
    Pipe,
    /// `||` - string concatenation operator (ISO/IEC 39075 section 20.23).
    PipePipe,
    /// `&` - label conjunction (`:Movie&Award`).
    Ampersand,
    /// `!` - label negation prefix (`:!Person`).
    Bang,

    // -- Arithmetic / comparison operators --------------------------
    /// `+`
    Plus,
    /// `+=` - additive-merge assignment in `SET n += {...}`.
    PlusEq,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `%`
    Percent,
    /// `^`
    Caret,
    /// `=`
    Eq,
    /// `<>` - SQL-style not-equal.
    Neq,
    /// `!=` - C-style not-equal (accepted as an alias of `<>`).
    NeqBang,
    /// `<`
    Lt,
    /// `<=`
    Lte,
    /// `>`
    Gt,
    /// `>=`
    Gte,
    /// `->` - directed edge.
    Arrow,
    /// `<-` - reverse-directed edge.
    LeftArrow,
    /// `~` - path concatenation / "any edge" filler in some
    /// pattern-quantifier forms.
    Tilde,
    /// `?` - optional quantifier.
    Question,

    // -- End of input -----------------------------------------------
    /// End of input. Always emitted last so the parser can match
    /// "expected `RETURN`, got end of input" uniformly.
    Eof,
}

/// Reserved keywords in GQL (ISO/IEC 39075:2024 section 6.2 "Reserved
/// words"), lower-cased for case-insensitive comparison. Grouped by
/// the spec's lexical categories for readability - lookups go through
/// [`lookup_reserved_word`] so order is
/// irrelevant.
///
/// **Invariant**: all entries are lower-case ASCII. Adding non-ASCII
/// or mixed-case entries will break [`lookup_reserved_word`] which
/// returns a slice from this table (so the parser can compare to
/// lower-cased input cheaply).
pub const RESERVED_WORDS: &[&str] = &[
    // Catalog / DDL
    "alter",
    "create",
    "drop",
    "graph",
    "index",
    "rename",
    "schema",
    "table",
    "tabular",
    "use",
    // DML
    "delete",
    "detach",
    "insert",
    "merge",
    "remove",
    "set",
    // Query / linear chain
    "all",
    "any",
    "as",
    "by",
    "exists",
    "filter",
    "for",
    "from",
    "group",
    "having",
    "in",
    "limit",
    "match",
    "offset",
    "optional",
    "order",
    "skip",
    "union",
    "unwind",
    "where",
    "with",
    "yield",
    // Pattern matching
    "acyclic",
    "any_shortest",
    "different",
    "elements",
    "group",
    "groups",
    "relationships",
    "repeatable",
    "shortest",
    "trail",
    "walk",
    "edge",
    "edges",
    "node",
    "nodes",
    "path",
    "paths",
    "relationship",
    // Quantifiers and modifiers
    "asc",
    "ascending",
    "desc",
    "descending",
    "distinct",
    // Logical / null
    "and",
    "is",
    "not",
    "null",
    "or",
    "unknown",
    "xor",
    // Type system
    "array",
    "bool",
    "boolean",
    "cast",
    "current_date",
    "current_time",
    "current_timestamp",
    "date",
    "datetime",
    "duration",
    "float",
    "int",
    "integer",
    "labeled",
    "list",
    "local",
    "local_datetime",
    "local_timestamp",
    "local_time",
    "map",
    "normalized",
    "number",
    "of",
    "record",
    "signed",
    "string",
    "time",
    "type",
    "typed",
    "varchar",
    "vertex",
    "zoned",
    "zoned_datetime",
    "zoned_time",
    // Sessions / transactions
    "abort",
    "begin",
    "close",
    "commit",
    "current",
    "only",
    "open",
    "read",
    "rollback",
    "session",
    "start",
    "transaction",
    "write",
    // Catalog references. ISO 39075 lists `name`, `home`, `global` as
    // *non*-reserved - they can be used as identifiers - so they're
    // intentionally absent. Only the truly reserved spellings live here.
    "current_graph",
    "current_schema",
    "session_user",
    // Procedure language
    "case",
    "do",
    "else",
    "elseif",
    "end",
    "if",
    "let",
    "next",
    "nothing",
    "raise",
    "return",
    "then",
    "throw",
    "to",
    "until",
    "when",
    "while",
    // Schema constraints / types
    "constraint",
    "key",
    "require",
    "required",
    "unique",
    // Boolean literals + misc
    "between",
    "call",
    "copy",
    "default",
    "false",
    "finish",
    "function",
    "like",
    "no",
    "on",
    "primary",
    "true",
    "value",
    "values",
    "void",
];

/// Case-insensitive lookup that returns the `&'static str` entry from
/// [`RESERVED_WORDS`] (always lower-case) when `word` matches one.
/// Used by the lexer to embed the canonical spelling in
/// [`TokenKind::Keyword`].
pub fn lookup_reserved_word(word: &str) -> Option<&'static str> {
    if word.is_empty() {
        return None;
    }
    RESERVED_WORDS
        .iter()
        .find(|kw| kw.eq_ignore_ascii_case(word))
        .copied()
}

/// Tokenize the input.
///
/// Returns the full token stream (always terminated by
/// [`TokenKind::Eof`]) on success. On failure, returns every
/// diagnostic encountered - the lexer recovers past errors so editors
/// get every issue from a single round-trip.
pub fn tokenize(input: &str) -> Result<Vec<Token>, Vec<ParseError>> {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    let mut c = SourceCursor::new(input);

    loop {
        c.skip_trivia(&mut errors);
        let start = c.pos();
        let Some(ch) = c.peek() else { break };

        match ch {
            // -- Punctuation ----------------------------------------
            '(' => {
                c.advance();
                push(&mut out, TokenKind::LParen, start, c.pos());
            }
            ')' => {
                c.advance();
                push(&mut out, TokenKind::RParen, start, c.pos());
            }
            '[' => {
                c.advance();
                push(&mut out, TokenKind::LBracket, start, c.pos());
            }
            ']' => {
                c.advance();
                push(&mut out, TokenKind::RBracket, start, c.pos());
            }
            '{' => {
                c.advance();
                push(&mut out, TokenKind::LBrace, start, c.pos());
            }
            '}' => {
                c.advance();
                push(&mut out, TokenKind::RBrace, start, c.pos());
            }
            ',' => {
                c.advance();
                push(&mut out, TokenKind::Comma, start, c.pos());
            }
            ';' => {
                c.advance();
                push(&mut out, TokenKind::Semicolon, start, c.pos());
            }
            '|' => {
                c.advance();
                if c.peek() == Some('|') {
                    c.advance();
                    push(&mut out, TokenKind::PipePipe, start, c.pos());
                } else {
                    push(&mut out, TokenKind::Pipe, start, c.pos());
                }
            }
            '&' => {
                c.advance();
                push(&mut out, TokenKind::Ampersand, start, c.pos());
            }
            '^' => {
                c.advance();
                push(&mut out, TokenKind::Caret, start, c.pos());
            }
            '~' => {
                c.advance();
                push(&mut out, TokenKind::Tilde, start, c.pos());
            }
            '?' => {
                c.advance();
                push(&mut out, TokenKind::Question, start, c.pos());
            }
            '%' => {
                c.advance();
                push(&mut out, TokenKind::Percent, start, c.pos());
            }
            '*' => {
                c.advance();
                push(&mut out, TokenKind::Star, start, c.pos());
            }
            '+' => {
                c.advance();
                if c.peek() == Some('=') {
                    c.advance();
                    push(&mut out, TokenKind::PlusEq, start, c.pos());
                } else {
                    push(&mut out, TokenKind::Plus, start, c.pos());
                }
            }
            '/' => {
                c.advance();
                push(&mut out, TokenKind::Slash, start, c.pos());
            }

            // -- `:` or `::` ----------------------------------------
            ':' => {
                c.advance();
                if c.peek() == Some(':') {
                    c.advance();
                    push(&mut out, TokenKind::ColonColon, start, c.pos());
                } else {
                    push(&mut out, TokenKind::Colon, start, c.pos());
                }
            }

            // -- `!` or `!=` ----------------------------------------
            '!' => {
                c.advance();
                if c.peek() == Some('=') {
                    c.advance();
                    push(&mut out, TokenKind::NeqBang, start, c.pos());
                } else {
                    push(&mut out, TokenKind::Bang, start, c.pos());
                }
            }

            // -- `=` ------------------------------------------------
            '=' => {
                c.advance();
                push(&mut out, TokenKind::Eq, start, c.pos());
            }

            // -- `<`, `<=`, `<>`, `<-` -----------------------------
            '<' => {
                c.advance();
                match c.peek() {
                    Some('=') => {
                        c.advance();
                        push(&mut out, TokenKind::Lte, start, c.pos());
                    }
                    Some('>') => {
                        c.advance();
                        push(&mut out, TokenKind::Neq, start, c.pos());
                    }
                    Some('-') => {
                        c.advance();
                        push(&mut out, TokenKind::LeftArrow, start, c.pos());
                    }
                    _ => push(&mut out, TokenKind::Lt, start, c.pos()),
                }
            }

            // -- `>`, `>=` -----------------------------------------
            '>' => {
                c.advance();
                if c.peek() == Some('=') {
                    c.advance();
                    push(&mut out, TokenKind::Gte, start, c.pos());
                } else {
                    push(&mut out, TokenKind::Gt, start, c.pos());
                }
            }

            // -- `-`, `->` -----------------------------------------
            // `--` is already consumed as a line comment by
            // `skip_trivia`, so here we only see single `-` or `->`.
            '-' => {
                c.advance();
                if c.peek() == Some('>') {
                    c.advance();
                    push(&mut out, TokenKind::Arrow, start, c.pos());
                } else {
                    push(&mut out, TokenKind::Minus, start, c.pos());
                }
            }

            // -- `.`, `..`, or `.5`-style float --------------------
            '.' => {
                c.advance();
                match c.peek() {
                    Some('.') => {
                        c.advance();
                        push(&mut out, TokenKind::DotDot, start, c.pos());
                    }
                    Some(d) if d.is_ascii_digit() => {
                        match lex_float_starting_with_dot(input, &mut c, start) {
                            Ok(tok) => out.push(tok),
                            Err(e) => errors.push(e),
                        }
                    }
                    _ => push(&mut out, TokenKind::Dot, start, c.pos()),
                }
            }

            // -- String literals -----------------------------------
            // ISO/IEC 39075 section 21.1: `"foo"` is a *delimited
            // identifier*, NOT a string literal - string literals
            // are single-quoted only. This is one of the few places
            // GQL deliberately diverges from openCypher (which
            // accepts both quote styles for strings). We honour the
            // ISO spelling and route `"..."` to the same path as
            // backtick-delimited identifiers.
            '\'' => match lex_string(input, &mut c, start) {
                Ok(tok) => out.push(tok),
                Err(e) => errors.push(e),
            },

            // -- Backtick or double-quoted delimited identifier ----
            '`' => match lex_delimited_ident(input, &mut c, start) {
                Ok(tok) => out.push(tok),
                Err(e) => errors.push(e),
            },
            '"' => match lex_double_quoted_ident(input, &mut c, start) {
                Ok(tok) => out.push(tok),
                Err(e) => errors.push(e),
            },

            // -- Parameter: `$name` or `$"name"` -------------------
            '$' => match lex_parameter(input, &mut c, start) {
                Ok(tok) => out.push(tok),
                Err(e) => errors.push(e),
            },

            // -- Numeric literal -----------------------------------
            '0'..='9' => match lex_number(input, &mut c, start) {
                Ok(tok) => out.push(tok),
                Err(e) => errors.push(e),
            },

            // -- Identifier or keyword -----------------------------
            ch if is_ident_start(ch) => {
                lex_ident_or_keyword(input, &mut c, start, &mut out);
            }

            // -- Unknown character ---------------------------------
            other => {
                c.advance();
                errors.push(ParseError::lex(
                    Span::new(start, c.pos()),
                    format!("unexpected character {other:?}"),
                ));
            }
        }
    }

    out.push(Token {
        kind: TokenKind::Eof,
        span: Span::at(c.pos()),
    });

    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

// -- Helpers --------------------------------------------------------

fn push(out: &mut Vec<Token>, kind: TokenKind, start: usize, end: usize) {
    out.push(Token {
        kind,
        span: Span::new(start, end),
    });
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

fn lex_ident_or_keyword(input: &str, c: &mut SourceCursor<'_>, start: usize, out: &mut Vec<Token>) {
    while let Some(ch) = c.peek() {
        if is_ident_continue(ch) {
            c.advance();
        } else {
            break;
        }
    }
    let word = &input[start..c.pos()];
    let kind = if let Some(kw) = lookup_reserved_word(word) {
        TokenKind::Keyword(kw)
    } else {
        TokenKind::Ident(word.to_string())
    };
    push(out, kind, start, c.pos());
}

fn lex_delimited_ident(
    input: &str,
    c: &mut SourceCursor<'_>,
    start: usize,
) -> Result<Token, ParseError> {
    debug_assert_eq!(c.peek(), Some('`'));
    c.advance(); // opening backtick
    let body_start = c.pos();
    let mut body = String::new();
    loop {
        match c.peek() {
            Some('`') => {
                c.advance();
                // Doubled backtick is an embedded literal backtick.
                if c.peek() == Some('`') {
                    body.push('`');
                    c.advance();
                    continue;
                }
                if body.is_empty() {
                    // Fall back to the input slice - no escapes seen.
                    body = input[body_start..c.pos() - 1].to_string();
                }
                return Ok(Token {
                    kind: TokenKind::DelimitedIdent(body),
                    span: Span::new(start, c.pos()),
                });
            }
            Some(ch) => {
                body.push(ch);
                c.advance();
            }
            None => {
                return Err(ParseError::lex(
                    Span::new(start, c.pos()),
                    "unterminated backtick-delimited identifier",
                ));
            }
        }
    }
}

/// Lex a double-quoted delimited identifier per ISO/IEC 39075
/// section 21.1. Distinct from `lex_delimited_ident` (backtick) because
/// the embedded-quote escape uses a doubled `"` instead of a
/// doubled backtick. Strings literals (`'...'`) take the dedicated
/// `lex_string` path.
fn lex_double_quoted_ident(
    input: &str,
    c: &mut SourceCursor<'_>,
    start: usize,
) -> Result<Token, ParseError> {
    debug_assert_eq!(c.peek(), Some('"'));
    c.advance(); // opening `"`
    let body_start = c.pos();
    let mut body = String::new();
    loop {
        match c.peek() {
            Some('"') => {
                c.advance();
                // Doubled `""` is an embedded literal `"`.
                if c.peek() == Some('"') {
                    body.push('"');
                    c.advance();
                    continue;
                }
                if body.is_empty() {
                    body = input[body_start..c.pos() - 1].to_string();
                }
                return Ok(Token {
                    kind: TokenKind::DelimitedIdent(body),
                    span: Span::new(start, c.pos()),
                });
            }
            Some(ch) => {
                body.push(ch);
                c.advance();
            }
            None => {
                return Err(ParseError::lex(
                    Span::new(start, c.pos()),
                    "unterminated double-quoted identifier",
                ));
            }
        }
    }
}

fn lex_parameter(input: &str, c: &mut SourceCursor<'_>, start: usize) -> Result<Token, ParseError> {
    debug_assert_eq!(c.peek(), Some('$'));
    c.advance(); // `$`
    match c.peek() {
        Some('"') => {
            // Delimited parameter: `$"..."`.
            c.advance();
            let body_start = c.pos();
            while let Some(ch) = c.peek() {
                if ch == '"' {
                    let name = input[body_start..c.pos()].to_string();
                    c.advance();
                    return Ok(Token {
                        kind: TokenKind::DelimitedParameter(name),
                        span: Span::new(start, c.pos()),
                    });
                }
                c.advance();
            }
            Err(ParseError::lex(
                Span::new(start, c.pos()),
                "unterminated delimited parameter name",
            ))
        }
        Some(ch) if is_ident_start(ch) => {
            let name_start = c.pos();
            while let Some(ch) = c.peek() {
                if is_ident_continue(ch) {
                    c.advance();
                } else {
                    break;
                }
            }
            Ok(Token {
                kind: TokenKind::Parameter(input[name_start..c.pos()].to_string()),
                span: Span::new(start, c.pos()),
            })
        }
        _ => Err(ParseError::lex(
            Span::new(start, c.pos()),
            "expected parameter name after `$`",
        )),
    }
}

fn lex_string(input: &str, c: &mut SourceCursor<'_>, start: usize) -> Result<Token, ParseError> {
    let quote = c.peek().expect("caller ensured a quote char");
    // Per ISO 39075 section 20.2 single quote is the only string-literal
    // delimiter; double quote routes to `lex_double_quoted_ident`.
    // The `'\"'` branch is retained for callers that may still pass
    // a `"` (e.g. the Cypher-compat string-literal path that
    // dynamically switches behaviour) - defence-in-depth.
    debug_assert!(quote == '\'' || quote == '"');
    c.advance(); // opening quote
    let mut s = String::new();
    loop {
        match c.peek() {
            Some(ch) if ch == quote => {
                c.advance();
                // Doubled quote = embedded literal quote.
                if c.peek() == Some(quote) {
                    s.push(quote);
                    c.advance();
                    continue;
                }
                return Ok(Token {
                    kind: TokenKind::StringLit(s),
                    span: Span::new(start, c.pos()),
                });
            }
            Some('\\') => {
                let escape_start = c.pos();
                c.advance();
                match c.peek() {
                    Some('\\') => {
                        s.push('\\');
                        c.advance();
                    }
                    Some('\'') => {
                        s.push('\'');
                        c.advance();
                    }
                    Some('"') => {
                        s.push('"');
                        c.advance();
                    }
                    Some('n') => {
                        s.push('\n');
                        c.advance();
                    }
                    Some('r') => {
                        s.push('\r');
                        c.advance();
                    }
                    Some('t') => {
                        s.push('\t');
                        c.advance();
                    }
                    Some('b') => {
                        s.push('\u{0008}');
                        c.advance();
                    }
                    Some('f') => {
                        s.push('\u{000C}');
                        c.advance();
                    }
                    Some('0') => {
                        s.push('\0');
                        c.advance();
                    }
                    Some('u') => {
                        c.advance();
                        let ch = lex_unicode_escape(input, c, escape_start)?;
                        s.push(ch);
                    }
                    Some(other) => {
                        let span = Span::new(escape_start, c.pos() + other.len_utf8());
                        c.advance();
                        return Err(ParseError::lex(
                            span,
                            format!("unknown escape sequence `\\{other}`"),
                        ));
                    }
                    None => {
                        return Err(ParseError::lex(
                            Span::new(escape_start, c.pos()),
                            "unterminated escape sequence",
                        ));
                    }
                }
            }
            Some(ch) => {
                s.push(ch);
                c.advance();
            }
            None => {
                return Err(ParseError::lex(
                    Span::new(start, c.pos()),
                    "unterminated string literal",
                ));
            }
        }
    }
}

/// Parse `\u{XXXX}` (ISO 39075 brace form) after the `\u` has been
/// consumed. `escape_start` is the offset of the leading backslash so
/// diagnostics point at the whole escape.
fn lex_unicode_escape(
    input: &str,
    c: &mut SourceCursor<'_>,
    escape_start: usize,
) -> Result<char, ParseError> {
    if c.peek() != Some('{') {
        return Err(ParseError::lex(
            Span::new(escape_start, c.pos()),
            "expected `{` after `\\u`",
        ));
    }
    c.advance(); // `{`
    let hex_start = c.pos();
    while let Some(ch) = c.peek() {
        if ch.is_ascii_hexdigit() {
            c.advance();
        } else {
            break;
        }
    }
    let hex = &input[hex_start..c.pos()];
    if hex.is_empty() || hex.len() > 6 {
        return Err(ParseError::lex(
            Span::new(escape_start, c.pos()),
            "unicode escape needs 1-6 hex digits",
        ));
    }
    if c.peek() != Some('}') {
        return Err(ParseError::lex(
            Span::new(escape_start, c.pos()),
            "expected `}` to close unicode escape",
        ));
    }
    c.advance(); // `}`
    let code = u32::from_str_radix(hex, 16).map_err(|_| {
        ParseError::lex(
            Span::new(escape_start, c.pos()),
            "invalid hex in unicode escape",
        )
    })?;
    char::from_u32(code).ok_or_else(|| {
        ParseError::lex(
            Span::new(escape_start, c.pos()),
            format!("invalid unicode codepoint U+{code:04X}"),
        )
    })
}

fn lex_number(input: &str, c: &mut SourceCursor<'_>, start: usize) -> Result<Token, ParseError> {
    // Radix prefixes: `0x`, `0o`, `0b`.
    if c.peek() == Some('0') {
        let after_zero = c.pos() + 1;
        let radix_marker = input[after_zero..].chars().next();
        match radix_marker {
            Some('x') | Some('X') => return lex_radix_int(input, c, start, 16, "hex"),
            Some('o') | Some('O') => return lex_radix_int(input, c, start, 8, "octal"),
            Some('b') | Some('B') => return lex_radix_int(input, c, start, 2, "binary"),
            _ => {}
        }
    }

    // Decimal digits.
    while let Some(ch) = c.peek() {
        if ch.is_ascii_digit() {
            c.advance();
        } else {
            break;
        }
    }

    let mut is_float = false;

    // Fractional part: `.<digit>+`, but not `..` (range).
    if c.peek() == Some('.') {
        let after_dot = input[c.pos() + 1..].chars().next();
        if matches!(after_dot, Some(d) if d.is_ascii_digit()) {
            is_float = true;
            c.advance(); // `.`
            while let Some(ch) = c.peek() {
                if ch.is_ascii_digit() {
                    c.advance();
                } else {
                    break;
                }
            }
        }
    }

    // Exponent.
    if matches!(c.peek(), Some('e') | Some('E')) {
        is_float = true;
        c.advance();
        if matches!(c.peek(), Some('+') | Some('-')) {
            c.advance();
        }
        let exp_start = c.pos();
        while let Some(ch) = c.peek() {
            if ch.is_ascii_digit() {
                c.advance();
            } else {
                break;
            }
        }
        if c.pos() == exp_start {
            return Err(ParseError::lex(
                Span::new(start, c.pos()),
                "exponent has no digits",
            ));
        }
    }

    let text = &input[start..c.pos()];
    finalize_number(text, is_float, start, c.pos())
}

fn lex_float_starting_with_dot(
    input: &str,
    c: &mut SourceCursor<'_>,
    start: usize,
) -> Result<Token, ParseError> {
    // The leading `.` is already consumed; the next char is a digit.
    while let Some(ch) = c.peek() {
        if ch.is_ascii_digit() {
            c.advance();
        } else {
            break;
        }
    }
    if matches!(c.peek(), Some('e') | Some('E')) {
        c.advance();
        if matches!(c.peek(), Some('+') | Some('-')) {
            c.advance();
        }
        let exp_start = c.pos();
        while let Some(ch) = c.peek() {
            if ch.is_ascii_digit() {
                c.advance();
            } else {
                break;
            }
        }
        if c.pos() == exp_start {
            return Err(ParseError::lex(
                Span::new(start, c.pos()),
                "exponent has no digits",
            ));
        }
    }
    let text = &input[start..c.pos()];
    finalize_number(text, true, start, c.pos())
}

fn lex_radix_int(
    input: &str,
    c: &mut SourceCursor<'_>,
    start: usize,
    radix: u32,
    label: &str,
) -> Result<Token, ParseError> {
    c.advance(); // `0`
    c.advance(); // radix marker (`x`/`X`, `o`/`O`, `b`/`B`)
    let digits_start = c.pos();
    while let Some(ch) = c.peek() {
        if ch.is_digit(radix) {
            c.advance();
        } else {
            break;
        }
    }
    if c.pos() == digits_start {
        return Err(ParseError::lex(
            Span::new(start, c.pos()),
            format!("expected {label} digits"),
        ));
    }
    let digits = &input[digits_start..c.pos()];
    let kind = match i64::from_str_radix(digits, radix) {
        Ok(v) => TokenKind::Integer(v),
        Err(_) => TokenKind::OverflowInt(input[start..c.pos()].to_string()),
    };
    Ok(Token {
        kind,
        span: Span::new(start, c.pos()),
    })
}

fn finalize_number(
    text: &str,
    is_float: bool,
    start: usize,
    end: usize,
) -> Result<Token, ParseError> {
    if is_float {
        let val: f64 = text.parse().map_err(|e| {
            ParseError::lex(Span::new(start, end), format!("invalid float literal: {e}"))
        })?;
        if !val.is_finite() {
            return Err(ParseError::lex(
                Span::new(start, end),
                "floating-point literal is not finite",
            ));
        }
        Ok(Token {
            kind: TokenKind::Float(val),
            span: Span::new(start, end),
        })
    } else {
        let kind = match text.parse::<i64>() {
            Ok(v) => TokenKind::Integer(v),
            Err(_) => TokenKind::OverflowInt(text.to_string()),
        };
        Ok(Token {
            kind,
            span: Span::new(start, end),
        })
    }
}

/// Tiny byte-offset cursor over the source. Decodes one UTF-8 code
/// point at a time. Crate-private.
struct SourceCursor<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> SourceCursor<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn pos(&self) -> usize {
        self.pos
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn starts_with(&self, needle: &str) -> bool {
        self.src[self.pos..].starts_with(needle)
    }

    /// Consume whitespace and comments. Errors (e.g. unterminated
    /// block comment) are pushed onto `errors` so the lexer can keep
    /// going.
    fn skip_trivia(&mut self, errors: &mut Vec<ParseError>) {
        loop {
            match self.peek() {
                Some(ch) if ch.is_whitespace() => {
                    self.advance();
                }
                // Line comment: `--` to end-of-line. Per ISO 39075
                // section 6.3.2 <simple comment>.
                //
                // Carve-outs: `-->` is an edge pattern (outgoing
                // anonymous edge) and `---` / `<--` are likewise edge
                // forms in section 14. So `--` only starts a comment if the
                // *third* character is neither `>` nor `-` - anything
                // else is consumed as text-until-EOL per the spec.
                Some('-')
                    if self.starts_with("--")
                        && !self.starts_with("-->")
                        && !self.starts_with("---") =>
                {
                    while let Some(ch) = self.peek() {
                        self.advance();
                        if ch == '\n' {
                            break;
                        }
                    }
                }
                // Double-solidus line comment (GQL optional feature
                // GB03): `//` to end-of-line. Same semantics as `--`.
                Some('/') if self.starts_with("//") => {
                    while let Some(ch) = self.peek() {
                        self.advance();
                        if ch == '\n' {
                            break;
                        }
                    }
                }
                // Block comment: `/* ... */` per section 6.3.2
                // <bracketed comment>. Non-nesting (consistent with
                // SQL / Cypher).
                Some('/') if self.starts_with("/*") => {
                    let start = self.pos;
                    self.advance();
                    self.advance();
                    let mut closed = false;
                    while let Some(ch) = self.advance() {
                        if ch == '*' && self.peek() == Some('/') {
                            self.advance();
                            closed = true;
                            break;
                        }
                    }
                    if !closed {
                        errors.push(ParseError::lex(
                            Span::new(start, self.pos),
                            "unterminated block comment",
                        ));
                    }
                }
                _ => break,
            }
        }
    }
}

// -- Tests ----------------------------------------------------------

#[cfg(test)]
mod tests;
