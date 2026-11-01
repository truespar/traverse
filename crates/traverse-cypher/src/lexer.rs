//! Cypher lexer: source text -> tokens.

use crate::error::CypherError;

/// Source span (byte offsets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Start byte offset (inclusive).
    pub start: usize,
    /// End byte offset (exclusive).
    pub end: usize,
}

/// Lexer token.
#[derive(Debug, Clone)]
pub struct Token {
    /// Token kind.
    pub kind: TokenKind,
    /// Source span.
    pub span: Span,
}

/// Every Cypher token kind.
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)]
pub enum TokenKind {
    // -- Literals ------------------------------------------------------
    /// Integer literal.
    Integer(i64),
    /// Integer literal overflowing i64, kept as text so the parser can handle negation.
    OverflowInt(String),
    /// Float literal.
    Float(f64),
    /// String literal (contents, unescaped).
    StringLit(String),

    // -- Identifiers & parameters --------------------------------------
    /// Identifier (variable, label, ...).
    Ident(String),
    /// Parameter: `$name`.
    Parameter(String),
    /// `$(`: Neo4j dynamic-expression escape in dynamic labels / types / property keys
    /// (`(n:$($label))`, `[r:$($type)]`, `SET n:$($x.kind)`). The closing `)` is a plain `RParen`;
    /// parsers take the whole `$(<expr>)` where dynamic values are allowed.
    DollarLParen,

    // -- Keywords (case-insensitive) -----------------------------------
    Match,
    Optional,
    Where,
    Return,
    With,
    Unwind,
    Foreach,
    Union,
    All,
    Create,
    Delete,
    Detach,
    Set,
    Remove,
    Merge,
    On,
    Order,
    By,
    Asc,
    Desc,
    Skip,
    Limit,
    As,
    Distinct,
    Case,
    When,
    Then,
    Else,
    End,
    And,
    Or,
    Xor,
    Not,
    In,
    Is,
    Contains,
    StartsWith,
    EndsWith,
    Exists,
    Count,
    Null,
    True,
    False,
    Ascending,
    Descending,
    Call,
    Yield,
    Index,
    Using,
    Scan,
    Drop,
    For,
    Analyze,
    Graph,
    Explain,
    Profile,
    Constraint,
    Require,
    Unique,
    Show,
    Load,
    Csv,

    // -- Punctuation & operators ---------------------------------------
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
    /// `..`
    DotDot,
    /// `,`
    Comma,
    /// `:`
    Colon,
    DoubleColon,
    /// `;`
    Semicolon,
    /// `|`
    Pipe,
    /// `||`: string / list concatenation (Cypher 25, ISO section 20.23).
    ConcatOp,
    /// `&`: label-expression conjunction (Cypher 5+).
    Amp,
    /// `!`: label-expression negation (Cypher 5+).
    Bang,
    /// `=`
    Eq,
    /// `<>`
    Neq,
    /// `<`
    Lt,
    /// `<=`
    Lte,
    /// `>`
    Gt,
    /// `>=`
    Gte,
    /// `+`
    Plus,
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
    /// `+=`
    PlusEq,
    /// `=~`
    RegexMatch,
    /// End of input.
    Eof,
}

/// Tokenizes a Cypher query.
pub fn tokenize(input: &str) -> Result<Vec<Token>, CypherError> {
    let mut tokens = Vec::new();
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut pos = 0;

    while pos < len {
        // whitespace
        if bytes[pos].is_ascii_whitespace() {
            pos += 1;
            continue;
        }

        // line comments //
        if pos + 1 < len && bytes[pos] == b'/' && bytes[pos + 1] == b'/' {
            pos += 2;
            while pos < len && bytes[pos] != b'\n' {
                pos += 1;
            }
            continue;
        }

        // block comments /* ... */
        if pos + 1 < len && bytes[pos] == b'/' && bytes[pos + 1] == b'*' {
            let start = pos;
            pos += 2;
            let mut found_end = false;
            while pos + 1 < len {
                if bytes[pos] == b'*' && bytes[pos + 1] == b'/' {
                    pos += 2;
                    found_end = true;
                    break;
                }
                pos += 1;
            }
            if !found_end {
                return Err(CypherError::syntax("unterminated block comment", start));
            }
            continue;
        }

        let start = pos;

        match bytes[pos] {
            b'(' => {
                tokens.push(tok(TokenKind::LParen, start, start + 1));
                pos += 1;
            }
            b')' => {
                tokens.push(tok(TokenKind::RParen, start, start + 1));
                pos += 1;
            }
            b'[' => {
                tokens.push(tok(TokenKind::LBracket, start, start + 1));
                pos += 1;
            }
            b']' => {
                tokens.push(tok(TokenKind::RBracket, start, start + 1));
                pos += 1;
            }
            b'{' => {
                tokens.push(tok(TokenKind::LBrace, start, start + 1));
                pos += 1;
            }
            b'}' => {
                tokens.push(tok(TokenKind::RBrace, start, start + 1));
                pos += 1;
            }
            b',' => {
                tokens.push(tok(TokenKind::Comma, start, start + 1));
                pos += 1;
            }
            b';' => {
                tokens.push(tok(TokenKind::Semicolon, start, start + 1));
                pos += 1;
            }
            b':' => {
                if pos + 1 < bytes.len() && bytes[pos + 1] == b':' {
                    tokens.push(tok(TokenKind::DoubleColon, start, start + 2));
                    pos += 2;
                } else {
                    tokens.push(tok(TokenKind::Colon, start, start + 1));
                    pos += 1;
                }
            }
            b'|' => {
                // `||`: Cypher 25 / ISO section 20.23 concatenation
                if bytes.get(pos + 1) == Some(&b'|') {
                    tokens.push(tok(TokenKind::ConcatOp, start, start + 2));
                    pos += 2;
                } else {
                    tokens.push(tok(TokenKind::Pipe, start, start + 1));
                    pos += 1;
                }
            }
            b'^' => {
                tokens.push(tok(TokenKind::Caret, start, start + 1));
                pos += 1;
            }
            b'&' => {
                // label/type-expression conjunction (Cypher 5+)
                tokens.push(tok(TokenKind::Amp, start, start + 1));
                pos += 1;
            }
            b'!' => {
                // label/type-expression negation (Cypher 5+)
                tokens.push(tok(TokenKind::Bang, start, start + 1));
                pos += 1;
            }
            b'%' => {
                tokens.push(tok(TokenKind::Percent, start, start + 1));
                pos += 1;
            }
            b'*' => {
                tokens.push(tok(TokenKind::Star, start, start + 1));
                pos += 1;
            }
            b'.' => {
                if pos + 1 < len && bytes[pos + 1] == b'.' {
                    tokens.push(tok(TokenKind::DotDot, start, start + 2));
                    pos += 2;
                } else if pos + 1 < len && bytes[pos + 1].is_ascii_digit() {
                    // float starting with a dot: .123
                    let (kind, end) = lex_number(input, start)?;
                    tokens.push(tok(kind, start, end));
                    pos = end;
                } else {
                    tokens.push(tok(TokenKind::Dot, start, start + 1));
                    pos += 1;
                }
            }
            b'+' => {
                if pos + 1 < len && bytes[pos + 1] == b'=' {
                    tokens.push(tok(TokenKind::PlusEq, start, start + 2));
                    pos += 2;
                } else {
                    tokens.push(tok(TokenKind::Plus, start, start + 1));
                    pos += 1;
                }
            }
            b'-' => {
                tokens.push(tok(TokenKind::Minus, start, start + 1));
                pos += 1;
            }
            b'/' => {
                tokens.push(tok(TokenKind::Slash, start, start + 1));
                pos += 1;
            }
            b'=' => {
                if pos + 1 < len && bytes[pos + 1] == b'~' {
                    tokens.push(tok(TokenKind::RegexMatch, start, start + 2));
                    pos += 2;
                } else {
                    tokens.push(tok(TokenKind::Eq, start, start + 1));
                    pos += 1;
                }
            }
            b'<' => {
                if pos + 1 < len && bytes[pos + 1] == b'>' {
                    tokens.push(tok(TokenKind::Neq, start, start + 2));
                    pos += 2;
                } else if pos + 1 < len && bytes[pos + 1] == b'=' {
                    tokens.push(tok(TokenKind::Lte, start, start + 2));
                    pos += 2;
                } else {
                    tokens.push(tok(TokenKind::Lt, start, start + 1));
                    pos += 1;
                }
            }
            b'>' => {
                if pos + 1 < len && bytes[pos + 1] == b'=' {
                    tokens.push(tok(TokenKind::Gte, start, start + 2));
                    pos += 2;
                } else {
                    tokens.push(tok(TokenKind::Gt, start, start + 1));
                    pos += 1;
                }
            }

            // string literals
            b'\'' | b'"' => {
                let (s, end) = lex_string(input, pos)?;
                tokens.push(tok(TokenKind::StringLit(s), start, end));
                pos = end;
            }

            // parameter, or `$(expr)` escape
            b'$' => {
                pos += 1;
                // `$(...)` dynamic-expression escape (`MATCH (n:$($label)) ...`, `SET n:$($x.kind)`):
                // one `DollarLParen` token so the parser recognises it unambiguously
                if pos < len && bytes[pos] == b'(' {
                    pos += 1;
                    tokens.push(tok(TokenKind::DollarLParen, start, pos));
                    continue;
                }
                let ident_start = pos;
                pos = scan_ident(input, pos);
                if pos == ident_start {
                    return Err(CypherError::syntax(
                        "expected parameter name after '$'",
                        start,
                    ));
                }
                let name = input[ident_start..pos].to_string();
                tokens.push(tok(TokenKind::Parameter(name), start, pos));
            }

            // backtick-quoted identifier
            b'`' => {
                pos += 1;
                let ident_start = pos;
                while pos < len && bytes[pos] != b'`' {
                    pos += 1;
                }
                if pos >= len {
                    return Err(CypherError::syntax(
                        "unterminated backtick-quoted identifier",
                        start,
                    ));
                }
                let name = input[ident_start..pos].to_string();
                pos += 1; // closing backtick
                tokens.push(tok(TokenKind::Ident(name), start, pos));
            }

            // numbers
            b'0'..=b'9' => {
                let (kind, end) = lex_number(input, pos)?;
                tokens.push(tok(kind, start, end));
                pos = end;
            }

            // identifiers and keywords
            b if is_ident_start(b) => {
                pos = scan_ident(input, pos);
                let word = &input[start..pos];
                let kind = keyword_or_ident(word);
                tokens.push(tok(kind, start, pos));
            }

            // non-ASCII identifier start: openCypher identifiers allow Unicode letters (Neo4j
            // parity, `förkortning`, `TILLHÖR`). Keywords are ASCII, so this is always an Ident;
            // punctuation and symbols still hit the error arm
            b if b >= 0x80 => {
                let ch = input[pos..].chars().next().unwrap_or('\u{FFFD}');
                if !ch.is_alphabetic() {
                    return Err(CypherError::syntax(
                        format!("unexpected character '{ch}'"),
                        pos,
                    ));
                }
                pos = scan_ident(input, pos);
                let word = &input[start..pos];
                tokens.push(tok(TokenKind::Ident(word.to_string()), start, pos));
            }

            _ => {
                let ch = input[pos..].chars().next().unwrap_or('\u{FFFD}');
                return Err(CypherError::syntax(
                    format!("unexpected character '{ch}'"),
                    pos,
                ));
            }
        }
    }

    tokens.push(tok(TokenKind::Eof, len, len));
    Ok(tokens)
}

fn tok(kind: TokenKind, start: usize, end: usize) -> Token {
    Token {
        kind,
        span: Span { start, end },
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

/// Scans identifier bytes from `pos`, char-aware past ASCII: Unicode letters and digits continue it
/// (`förkortning` is one token); other characters (a pasted em-dash, punctuation) end it instead of
/// being absorbed. ASCII takes the byte fast path; stepping by `len_utf8` keeps slices on char
/// boundaries.
fn scan_ident(input: &str, mut pos: usize) -> usize {
    let bytes = input.as_bytes();
    while pos < bytes.len() {
        let b = bytes[pos];
        if b.is_ascii_alphanumeric() || b == b'_' {
            pos += 1;
            continue;
        }
        if b >= 0x80
            && let Some(ch) = input[pos..].chars().next()
            && ch.is_alphanumeric()
        {
            pos += ch.len_utf8();
            continue;
        }
        break;
    }
    pos
}

/// String literal (single or double quoted) with escapes.
fn lex_string(input: &str, start: usize) -> Result<(String, usize), CypherError> {
    let bytes = input.as_bytes();
    let quote = bytes[start];
    let mut pos = start + 1;
    let mut s = String::new();

    while pos < bytes.len() {
        let b = bytes[pos];
        if b == quote {
            // escaped (doubled) quote
            if pos + 1 < bytes.len() && bytes[pos + 1] == quote {
                s.push(quote as char);
                pos += 2;
                continue;
            }
            return Ok((s, pos + 1));
        }
        if b == b'\\' {
            pos += 1;
            if pos >= bytes.len() {
                return Err(CypherError::syntax("unterminated string escape", pos));
            }
            match bytes[pos] {
                b'\\' => s.push('\\'),
                b'\'' => s.push('\''),
                b'"' => s.push('"'),
                b'n' => s.push('\n'),
                b'r' => s.push('\r'),
                b't' => s.push('\t'),
                b'b' => s.push('\u{0008}'),
                b'f' => s.push('\u{000C}'),
                b'u' => {
                    pos += 1;
                    if pos + 4 > bytes.len() {
                        return Err(CypherError::syntax("incomplete unicode escape", pos));
                    }
                    let hex = &input[pos..pos + 4];
                    let code = u32::from_str_radix(hex, 16).map_err(|_| {
                        CypherError::syntax(format!("invalid unicode escape: \\u{hex}"), pos)
                    })?;
                    let ch = char::from_u32(code).ok_or_else(|| {
                        CypherError::syntax(format!("invalid unicode codepoint: \\u{hex}"), pos)
                    })?;
                    s.push(ch);
                    pos += 3; // +1 more at the end of the loop
                }
                other => {
                    return Err(CypherError::syntax(
                        format!("invalid escape sequence: \\{}", other as char),
                        pos,
                    ));
                }
            }
            pos += 1;
        } else if b < 0x80 {
            s.push(b as char);
            pos += 1;
        } else {
            // multi-byte UTF-8: decode the whole character
            let rest = &input[pos..];
            if let Some(ch) = rest.chars().next() {
                s.push(ch);
                pos += ch.len_utf8();
            } else {
                pos += 1;
            }
        }
    }
    Err(CypherError::syntax("unterminated string literal", start))
}

/// Number (integer or float): hex, octal, exponents.
fn lex_number(input: &str, start: usize) -> Result<(TokenKind, usize), CypherError> {
    let bytes = input.as_bytes();
    let mut pos = start;

    // hex 0x...
    if pos + 1 < bytes.len()
        && bytes[pos] == b'0'
        && (bytes[pos + 1] == b'x' || bytes[pos + 1] == b'X')
    {
        pos += 2;
        let hex_start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_hexdigit() {
            pos += 1;
        }
        if pos == hex_start {
            return Err(CypherError::syntax("expected hex digits after 0x", start));
        }
        let hex_text = &input[hex_start..pos];
        match i64::from_str_radix(hex_text, 16) {
            Ok(val) => return Ok((TokenKind::Integer(val), pos)),
            Err(_) => return Ok((TokenKind::OverflowInt(format!("0x{hex_text}")), pos)),
        }
    }

    // octal 0o...
    if pos + 1 < bytes.len()
        && bytes[pos] == b'0'
        && (bytes[pos + 1] == b'o' || bytes[pos + 1] == b'O')
    {
        pos += 2;
        let oct_start = pos;
        while pos < bytes.len() && (b'0'..=b'7').contains(&bytes[pos]) {
            pos += 1;
        }
        if pos == oct_start {
            return Err(CypherError::syntax("expected octal digits after 0o", start));
        }
        let oct_text = &input[oct_start..pos];
        match i64::from_str_radix(oct_text, 8) {
            Ok(val) => return Ok((TokenKind::Integer(val), pos)),
            Err(_) => return Ok((TokenKind::OverflowInt(format!("0o{oct_text}")), pos)),
        }
    }

    // decimal digits
    while pos < bytes.len() && bytes[pos].is_ascii_digit() {
        pos += 1;
    }

    let mut is_float = false;

    // decimal point (not `..`, which is a range)
    if pos < bytes.len() && bytes[pos] == b'.' && !(pos + 1 < bytes.len() && bytes[pos + 1] == b'.')
    {
        // float only if a digit follows
        if pos + 1 < bytes.len() && bytes[pos + 1].is_ascii_digit() {
            is_float = true;
            pos += 1;
            while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                pos += 1;
            }
        }
    }

    // exponent
    if pos < bytes.len() && (bytes[pos] == b'e' || bytes[pos] == b'E') {
        is_float = true;
        pos += 1;
        if pos < bytes.len() && (bytes[pos] == b'+' || bytes[pos] == b'-') {
            pos += 1;
        }
        let exp_start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos == exp_start {
            return Err(CypherError::syntax("expected exponent digits", start));
        }
    }

    let text = &input[start..pos];
    if is_float {
        let val: f64 = text
            .parse()
            .map_err(|e| CypherError::syntax(format!("invalid float literal: {e}"), start))?;
        if val.is_infinite() {
            return Err(CypherError::syntax(
                "floating point number is too large",
                start,
            ));
        }
        Ok((TokenKind::Float(val), pos))
    } else {
        match text.parse::<i64>() {
            Ok(val) => Ok((TokenKind::Integer(val), pos)),
            Err(_) => {
                // overflows i64: OverflowInt for the parser to negate
                Ok((TokenKind::OverflowInt(text.to_string()), pos))
            }
        }
    }
}

/// Word -> keyword token or identifier.
fn keyword_or_ident(word: &str) -> TokenKind {
    // case-insensitive keywords
    let upper = word.to_ascii_uppercase();
    match upper.as_str() {
        "MATCH" => TokenKind::Match,
        "OPTIONAL" => TokenKind::Optional,
        "WHERE" => TokenKind::Where,
        "RETURN" => TokenKind::Return,
        "WITH" => TokenKind::With,
        "UNWIND" => TokenKind::Unwind,
        "FOREACH" => TokenKind::Foreach,
        "UNION" => TokenKind::Union,
        "ALL" => TokenKind::All,
        "CREATE" => TokenKind::Create,
        "DELETE" => TokenKind::Delete,
        "DETACH" => TokenKind::Detach,
        "SET" => TokenKind::Set,
        "REMOVE" => TokenKind::Remove,
        "MERGE" => TokenKind::Merge,
        "ON" => TokenKind::On,
        "ORDER" => TokenKind::Order,
        "BY" => TokenKind::By,
        "ASC" => TokenKind::Asc,
        "ASCENDING" => TokenKind::Ascending,
        "DESC" => TokenKind::Desc,
        "DESCENDING" => TokenKind::Descending,
        "CALL" => TokenKind::Call,
        "YIELD" => TokenKind::Yield,
        "SKIP" => TokenKind::Skip,
        "LIMIT" => TokenKind::Limit,
        "AS" => TokenKind::As,
        "DISTINCT" => TokenKind::Distinct,
        "CASE" => TokenKind::Case,
        "WHEN" => TokenKind::When,
        "THEN" => TokenKind::Then,
        "ELSE" => TokenKind::Else,
        "END" => TokenKind::End,
        "AND" => TokenKind::And,
        "OR" => TokenKind::Or,
        "XOR" => TokenKind::Xor,
        "NOT" => TokenKind::Not,
        "IN" => TokenKind::In,
        "IS" => TokenKind::Is,
        "CONTAINS" => TokenKind::Contains,
        "STARTS" => TokenKind::StartsWith,
        "ENDS" => TokenKind::EndsWith,
        "EXISTS" => TokenKind::Exists,
        "COUNT" => TokenKind::Count,
        "NULL" => TokenKind::Null,
        "TRUE" => TokenKind::True,
        "FALSE" => TokenKind::False,
        "SHOW" => TokenKind::Show,
        "INDEX" => TokenKind::Index,
        "USING" => TokenKind::Using,
        "SCAN" => TokenKind::Scan,
        "DROP" => TokenKind::Drop,
        "FOR" => TokenKind::For,
        "ANALYZE" => TokenKind::Analyze,
        "GRAPH" => TokenKind::Graph,
        "EXPLAIN" => TokenKind::Explain,
        "PROFILE" => TokenKind::Profile,
        "CONSTRAINT" => TokenKind::Constraint,
        "REQUIRE" => TokenKind::Require,
        "UNIQUE" => TokenKind::Unique,
        "LOAD" => TokenKind::Load,
        "CSV" => TokenKind::Csv,
        _ => TokenKind::Ident(word.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(input: &str) -> Vec<TokenKind> {
        tokenize(input)
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| !matches!(k, TokenKind::Eof))
            .collect()
    }

    #[test]
    fn simple_match_return() {
        let tokens = kinds("MATCH (n) RETURN n");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Match,
                TokenKind::LParen,
                TokenKind::Ident("n".into()),
                TokenKind::RParen,
                TokenKind::Return,
                TokenKind::Ident("n".into()),
            ]
        );
    }

    #[test]
    fn case_insensitive_keywords() {
        let tokens = kinds("match RETURN Return");
        assert_eq!(
            tokens,
            vec![TokenKind::Match, TokenKind::Return, TokenKind::Return]
        );
    }

    #[test]
    fn integer_literals() {
        assert_eq!(kinds("42"), vec![TokenKind::Integer(42)]);
        assert_eq!(kinds("0x1A"), vec![TokenKind::Integer(26)]);
        assert_eq!(kinds("0o17"), vec![TokenKind::Integer(15)]);
    }

    #[test]
    fn float_literals() {
        assert_eq!(kinds("1.25"), vec![TokenKind::Float(1.25)]);
        assert_eq!(kinds("1e10"), vec![TokenKind::Float(1e10)]);
        assert_eq!(kinds("2.5E-3"), vec![TokenKind::Float(2.5e-3)]);
    }

    #[test]
    fn string_literals() {
        assert_eq!(kinds("'hello'"), vec![TokenKind::StringLit("hello".into())]);
        assert_eq!(
            kinds("\"world\""),
            vec![TokenKind::StringLit("world".into())]
        );
        // escaped quotes
        assert_eq!(kinds("'it''s'"), vec![TokenKind::StringLit("it's".into())]);
        // escape sequences
        assert_eq!(kinds("'a\\nb'"), vec![TokenKind::StringLit("a\nb".into())]);
    }

    #[test]
    fn operators() {
        let tokens = kinds("<> <= >= += =~ ..");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Neq,
                TokenKind::Lte,
                TokenKind::Gte,
                TokenKind::PlusEq,
                TokenKind::RegexMatch,
                TokenKind::DotDot,
            ]
        );
    }

    #[test]
    fn line_comments() {
        let tokens = kinds("MATCH // this is a comment\n(n)");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Match,
                TokenKind::LParen,
                TokenKind::Ident("n".into()),
                TokenKind::RParen,
            ]
        );
    }

    #[test]
    fn block_comments() {
        let tokens = kinds("MATCH /* block */ (n)");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Match,
                TokenKind::LParen,
                TokenKind::Ident("n".into()),
                TokenKind::RParen,
            ]
        );
    }

    #[test]
    fn parameters() {
        assert_eq!(kinds("$name"), vec![TokenKind::Parameter("name".into())]);
    }

    #[test]
    fn backtick_quoted_ident() {
        assert_eq!(
            kinds("`my variable`"),
            vec![TokenKind::Ident("my variable".into())]
        );
    }

    #[test]
    fn pattern_tokens() {
        let tokens = kinds("(a:Person)-[:KNOWS]->(b)");
        assert_eq!(
            tokens,
            vec![
                TokenKind::LParen,
                TokenKind::Ident("a".into()),
                TokenKind::Colon,
                TokenKind::Ident("Person".into()),
                TokenKind::RParen,
                TokenKind::Minus,
                TokenKind::LBracket,
                TokenKind::Colon,
                TokenKind::Ident("KNOWS".into()),
                TokenKind::RBracket,
                TokenKind::Minus,
                TokenKind::Gt,
                TokenKind::LParen,
                TokenKind::Ident("b".into()),
                TokenKind::RParen,
            ]
        );
    }

    #[test]
    fn number_range_not_float() {
        // `1..3` is Integer(1), DotDot, Integer(3), not Float
        let tokens = kinds("1..3");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Integer(1),
                TokenKind::DotDot,
                TokenKind::Integer(3)
            ]
        );
    }
}
