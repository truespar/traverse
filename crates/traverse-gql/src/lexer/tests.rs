use super::*;
use crate::error::ParseErrorKind;

/// Drop the trailing EOF token so test assertions stay focused on
/// content tokens. Panics if the stream doesn't end in EOF.
fn kinds(input: &str) -> Vec<TokenKind> {
    let toks = tokenize(input).expect(input);
    let last = toks.last().expect("at least EOF");
    assert!(matches!(last.kind, TokenKind::Eof), "stream missing EOF");
    toks.into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Eof))
        .map(|t| t.kind)
        .collect()
}

fn lex_err(input: &str) -> Vec<ParseError> {
    tokenize(input).expect_err("expected at least one error")
}

// -- Trivia ----------------------------------------------------

#[test]
fn empty_input_yields_just_eof() {
    let toks = tokenize("").expect("empty lex");
    assert_eq!(toks.len(), 1);
    assert!(matches!(toks[0].kind, TokenKind::Eof));
    assert_eq!(toks[0].span, Span::at(0));
}

#[test]
fn whitespace_only_yields_just_eof() {
    assert!(kinds("   \n\t  ").is_empty());
}

#[test]
fn line_comment_is_skipped() {
    assert!(kinds("-- comment\n").is_empty());
}

#[test]
fn double_solidus_line_comment_is_skipped() {
    // GQL optional feature GB03: `//` to end-of-line.
    assert!(kinds("// comment\n").is_empty());
    // The trailing newline isn't required if the source ends.
    assert!(kinds("// comment").is_empty());
    // Whitespace + double-solidus + content is dropped entirely.
    assert_eq!(
        kinds("MATCH // outer\n(n)"),
        vec![
            TokenKind::Keyword("match"),
            TokenKind::LParen,
            TokenKind::Ident("n".to_string()),
            TokenKind::RParen,
        ]
    );
}

#[test]
fn block_comment_is_skipped() {
    assert!(kinds("/* block\n spanning */ ").is_empty());
}

#[test]
fn unterminated_block_comment_is_an_error() {
    let errs = lex_err("/* still open");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unterminated block"))
    );
}

#[test]
fn trivia_between_tokens() {
    assert_eq!(
        kinds("(  /* gap */ )"),
        vec![TokenKind::LParen, TokenKind::RParen]
    );
}

// -- Keywords / identifiers ------------------------------------

#[test]
fn keyword_match() {
    assert_eq!(kinds("MATCH"), vec![TokenKind::Keyword("match")]);
}

#[test]
fn keyword_case_insensitive() {
    assert_eq!(
        kinds("match Return UNION OptiOnal"),
        vec![
            TokenKind::Keyword("match"),
            TokenKind::Keyword("return"),
            TokenKind::Keyword("union"),
            TokenKind::Keyword("optional"),
        ]
    );
}

#[test]
fn keyword_canonical_spelling_is_lowercase() {
    // `Keyword` holds a slice from RESERVED_WORDS - always lower
    // case - regardless of how the user typed it.
    if let TokenKind::Keyword(s) = &kinds("MATCH")[0] {
        assert_eq!(*s, "match");
    } else {
        panic!("expected Keyword");
    }
}

#[test]
fn ident_not_a_keyword() {
    assert_eq!(kinds("myVar"), vec![TokenKind::Ident("myVar".into())]);
}

#[test]
fn ident_with_underscore_and_digits() {
    assert_eq!(
        kinds("_foo_bar2"),
        vec![TokenKind::Ident("_foo_bar2".into())]
    );
}

#[test]
fn ident_starting_with_underscore() {
    assert_eq!(kinds("_"), vec![TokenKind::Ident("_".into())]);
}

#[test]
fn unicode_identifier() {
    // ISO 39075 section 6.4 admits Unicode letters in regular identifiers.
    assert_eq!(kinds("café"), vec![TokenKind::Ident("café".into())]);
    assert_eq!(kinds("åland"), vec![TokenKind::Ident("åland".into())]);
}

#[test]
fn ident_then_keyword() {
    assert_eq!(
        kinds("foo MATCH bar"),
        vec![
            TokenKind::Ident("foo".into()),
            TokenKind::Keyword("match"),
            TokenKind::Ident("bar".into()),
        ]
    );
}

#[test]
fn delimited_ident_basic() {
    assert_eq!(
        kinds("`hello world`"),
        vec![TokenKind::DelimitedIdent("hello world".into())]
    );
}

#[test]
fn delimited_ident_can_be_a_reserved_spelling() {
    // The delimited form preserves the spelling exactly, even if
    // it would otherwise tokenise as a keyword.
    assert_eq!(
        kinds("`MATCH`"),
        vec![TokenKind::DelimitedIdent("MATCH".into())]
    );
}

#[test]
fn delimited_ident_double_backtick_escapes() {
    assert_eq!(
        kinds("`a``b`"),
        vec![TokenKind::DelimitedIdent("a`b".into())]
    );
}

#[test]
fn unterminated_delimited_ident_is_an_error() {
    let errs = lex_err("`open");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unterminated backtick"))
    );
}

#[test]
fn reserved_word_lookup_smoke() {
    assert_eq!(lookup_reserved_word("MATCH"), Some("match"));
    assert_eq!(lookup_reserved_word("match"), Some("match"));
    assert_eq!(lookup_reserved_word("Return"), Some("return"));
    assert_eq!(lookup_reserved_word("myvar"), None);
    assert_eq!(lookup_reserved_word(""), None);
}

// -- Parameters -----------------------------------------------

#[test]
fn parameter_simple() {
    assert_eq!(kinds("$name"), vec![TokenKind::Parameter("name".into())]);
}

#[test]
fn parameter_with_underscore() {
    assert_eq!(
        kinds("$_my_param2"),
        vec![TokenKind::Parameter("_my_param2".into())]
    );
}

#[test]
fn parameter_delimited() {
    assert_eq!(
        kinds("$\"weird name\""),
        vec![TokenKind::DelimitedParameter("weird name".into())]
    );
}

#[test]
fn parameter_missing_name_is_an_error() {
    let errs = lex_err("$ ");
    assert!(errs.iter().any(|e| e.message.contains("parameter name")));
}

#[test]
fn parameter_unterminated_delimited_is_an_error() {
    let errs = lex_err("$\"open");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unterminated delimited parameter"))
    );
}

// -- Integer literals -----------------------------------------

#[test]
fn integer_decimal() {
    assert_eq!(kinds("42"), vec![TokenKind::Integer(42)]);
    assert_eq!(kinds("0"), vec![TokenKind::Integer(0)]);
    assert_eq!(kinds("1234567890"), vec![TokenKind::Integer(1234567890)]);
}

#[test]
fn integer_hex() {
    assert_eq!(kinds("0x1A"), vec![TokenKind::Integer(26)]);
    assert_eq!(kinds("0XFF"), vec![TokenKind::Integer(255)]);
}

#[test]
fn integer_octal() {
    assert_eq!(kinds("0o17"), vec![TokenKind::Integer(15)]);
    assert_eq!(kinds("0O7"), vec![TokenKind::Integer(7)]);
}

#[test]
fn integer_binary() {
    assert_eq!(kinds("0b1010"), vec![TokenKind::Integer(10)]);
    assert_eq!(kinds("0B1111"), vec![TokenKind::Integer(15)]);
}

#[test]
fn integer_overflow_falls_back_to_string() {
    let toks = kinds("99999999999999999999");
    assert!(matches!(toks[0], TokenKind::OverflowInt(_)));
}

#[test]
fn integer_radix_without_digits_is_an_error() {
    let errs = lex_err("0x");
    assert!(errs.iter().any(|e| e.message.contains("hex digits")));
    let errs = lex_err("0o");
    assert!(errs.iter().any(|e| e.message.contains("octal digits")));
    let errs = lex_err("0b");
    assert!(errs.iter().any(|e| e.message.contains("binary digits")));
}

// -- Float literals -------------------------------------------

#[test]
fn float_simple() {
    assert_eq!(kinds("1.25"), vec![TokenKind::Float(1.25)]);
}

#[test]
fn float_leading_dot() {
    assert_eq!(kinds(".5"), vec![TokenKind::Float(0.5)]);
}

#[test]
fn float_exponent_positive() {
    assert_eq!(kinds("1e10"), vec![TokenKind::Float(1e10)]);
}

#[test]
fn float_exponent_signed() {
    assert_eq!(kinds("2.5E-3"), vec![TokenKind::Float(2.5e-3)]);
    assert_eq!(kinds("1.0e+5"), vec![TokenKind::Float(1.0e5)]);
}

#[test]
fn float_exponent_missing_digits_is_an_error() {
    assert!(lex_err("1e").iter().any(|e| e.message.contains("exponent")));
    assert!(
        lex_err(".5e+")
            .iter()
            .any(|e| e.message.contains("exponent"))
    );
}

#[test]
fn float_infinite_is_an_error() {
    // 1e500 overflows f64.
    assert!(
        lex_err("1e500")
            .iter()
            .any(|e| e.message.contains("not finite"))
    );
}

#[test]
fn integer_dot_dot_is_range_not_float() {
    assert_eq!(
        kinds("1..3"),
        vec![
            TokenKind::Integer(1),
            TokenKind::DotDot,
            TokenKind::Integer(3)
        ]
    );
}

#[test]
fn integer_dot_dot_in_quantifier() {
    // *1..3 is a quantifier on a path.
    assert_eq!(
        kinds("*1..3"),
        vec![
            TokenKind::Star,
            TokenKind::Integer(1),
            TokenKind::DotDot,
            TokenKind::Integer(3),
        ]
    );
}

// -- String literals ------------------------------------------

#[test]
fn string_single_quoted() {
    assert_eq!(kinds("'hello'"), vec![TokenKind::StringLit("hello".into())]);
}

#[test]
fn double_quoted_is_delimited_identifier() {
    // ISO 39075 section 21.1 - double-quoted text is a delimited
    // identifier, NOT a string literal. String literals use
    // single quotes only.
    assert_eq!(
        kinds("\"world\""),
        vec![TokenKind::DelimitedIdent("world".into())]
    );
}

#[test]
fn double_quoted_identifier_doubled_quote_escape() {
    // `"a""b"` - doubled `""` is an embedded literal `"`.
    assert_eq!(
        kinds("\"a\"\"b\""),
        vec![TokenKind::DelimitedIdent("a\"b".into())]
    );
}

#[test]
fn string_doubled_quote_escapes() {
    // String literals (single-quote only per ISO 39075 section 20.2)
    // accept the doubled-quote escape `''`.
    assert_eq!(kinds("'it''s'"), vec![TokenKind::StringLit("it's".into())]);
    // the double-quoted form is a delimited identifier, doubled `""` escaping a quote (section 21.1)
    assert_eq!(
        kinds("\"say \"\"hi\"\"\""),
        vec![TokenKind::DelimitedIdent("say \"hi\"".into())]
    );
}

#[test]
fn string_backslash_escapes() {
    assert_eq!(
        kinds(r"'a\nb\tc'"),
        vec![TokenKind::StringLit("a\nb\tc".into())]
    );
    assert_eq!(kinds(r"'\\\''"), vec![TokenKind::StringLit("\\'".into())]);
}

#[test]
fn string_unicode_escape_brace_form() {
    assert_eq!(
        kinds(r"'\u{2603}'"),
        vec![TokenKind::StringLit("\u{2603}".into())]
    );
    assert_eq!(
        kinds(r"'\u{1F600}'"),
        vec![TokenKind::StringLit("\u{1F600}".into())]
    );
}

#[test]
fn string_unicode_escape_missing_braces_is_an_error() {
    // `\u` + bare 4-digit hex isn't GQL, which needs the brace form `\u{XXXX}`
    let errs = lex_err("'\\u2603'");
    assert!(errs.iter().any(|e| e.message.contains("`{`")));
}

#[test]
fn string_unicode_escape_too_many_hex_is_an_error() {
    assert!(
        lex_err(r"'\u{1234567}'")
            .iter()
            .any(|e| e.message.contains("1-6 hex digits"))
    );
}

#[test]
fn string_unknown_escape_is_an_error() {
    let errs = lex_err(r"'\q'");
    assert!(errs.iter().any(|e| e.message.contains("unknown escape")));
}

#[test]
fn string_unterminated_is_an_error() {
    assert!(
        lex_err("'open")
            .iter()
            .any(|e| e.message.contains("unterminated string"))
    );
}

#[test]
fn string_utf8_body() {
    assert_eq!(
        kinds("'café 🌍'"),
        vec![TokenKind::StringLit("café 🌍".into())]
    );
}

// -- Punctuation ----------------------------------------------

#[test]
fn parens_and_brackets() {
    assert_eq!(
        kinds("()[]{}"),
        vec![
            TokenKind::LParen,
            TokenKind::RParen,
            TokenKind::LBracket,
            TokenKind::RBracket,
            TokenKind::LBrace,
            TokenKind::RBrace,
        ]
    );
}

#[test]
fn dot_and_dotdot() {
    assert_eq!(
        kinds(". .. ."),
        vec![TokenKind::Dot, TokenKind::DotDot, TokenKind::Dot]
    );
}

#[test]
fn colon_and_double_colon() {
    assert_eq!(
        kinds(": :: :"),
        vec![TokenKind::Colon, TokenKind::ColonColon, TokenKind::Colon]
    );
}

#[test]
fn pipe_amp_caret_tilde_question_bang() {
    assert_eq!(
        kinds("| & ^ ~ ? !"),
        vec![
            TokenKind::Pipe,
            TokenKind::Ampersand,
            TokenKind::Caret,
            TokenKind::Tilde,
            TokenKind::Question,
            TokenKind::Bang,
        ]
    );
}

#[test]
fn pipe_pipe_lexes_as_single_token() {
    // GQL section 20.23 `||` is one token (string concat), separate
    // from `|` (label disjunction / alternation).
    assert_eq!(kinds("||"), vec![TokenKind::PipePipe]);
    // Three pipes in a row: `||` then a single `|`.
    assert_eq!(kinds("|||"), vec![TokenKind::PipePipe, TokenKind::Pipe]);
    // Pipes separated by whitespace stay as two `|` tokens.
    assert_eq!(kinds("| |"), vec![TokenKind::Pipe, TokenKind::Pipe]);
}

#[test]
fn comma_and_semicolon() {
    assert_eq!(kinds(", ;"), vec![TokenKind::Comma, TokenKind::Semicolon]);
}

// -- Operators ------------------------------------------------

#[test]
fn arithmetic_operators() {
    assert_eq!(
        kinds("+ - * / %"),
        vec![
            TokenKind::Plus,
            TokenKind::Minus,
            TokenKind::Star,
            TokenKind::Slash,
            TokenKind::Percent,
        ]
    );
}

#[test]
fn comparison_operators() {
    assert_eq!(
        kinds("= <> != < <= > >="),
        vec![
            TokenKind::Eq,
            TokenKind::Neq,
            TokenKind::NeqBang,
            TokenKind::Lt,
            TokenKind::Lte,
            TokenKind::Gt,
            TokenKind::Gte,
        ]
    );
}

#[test]
fn arrow_tokens() {
    assert_eq!(kinds("-> <-"), vec![TokenKind::Arrow, TokenKind::LeftArrow]);
}

#[test]
fn lt_alone_then_dash() {
    // `< -` (with a space) should not collapse to `<-`.
    assert_eq!(kinds("< -"), vec![TokenKind::Lt, TokenKind::Minus]);
}

#[test]
fn minus_alone() {
    assert_eq!(
        kinds("a - b"),
        vec![
            TokenKind::Ident("a".into()),
            TokenKind::Minus,
            TokenKind::Ident("b".into()),
        ]
    );
}

// -- Composite token streams ----------------------------------

#[test]
fn match_node_return_property() {
    // MATCH (n:Person) RETURN n.name (`name` is deliberately non-reserved, see RESERVED_WORDS)
    assert_eq!(
        kinds("MATCH (n:Person) RETURN n.name"),
        vec![
            TokenKind::Keyword("match"),
            TokenKind::LParen,
            TokenKind::Ident("n".into()),
            TokenKind::Colon,
            TokenKind::Ident("Person".into()),
            TokenKind::RParen,
            TokenKind::Keyword("return"),
            TokenKind::Ident("n".into()),
            TokenKind::Dot,
            TokenKind::Ident("name".into()),
        ]
    );
}

#[test]
fn match_directed_edge() {
    // MATCH (a)-[r:KNOWS]->(b)
    assert_eq!(
        kinds("MATCH (a)-[r:KNOWS]->(b)"),
        vec![
            TokenKind::Keyword("match"),
            TokenKind::LParen,
            TokenKind::Ident("a".into()),
            TokenKind::RParen,
            TokenKind::Minus,
            TokenKind::LBracket,
            TokenKind::Ident("r".into()),
            TokenKind::Colon,
            TokenKind::Ident("KNOWS".into()),
            TokenKind::RBracket,
            TokenKind::Arrow,
            TokenKind::LParen,
            TokenKind::Ident("b".into()),
            TokenKind::RParen,
        ]
    );
}

#[test]
fn label_conjunction_with_ampersand() {
    // MATCH (n:Movie&Award)
    assert_eq!(
        kinds("MATCH (n:Movie&Award)"),
        vec![
            TokenKind::Keyword("match"),
            TokenKind::LParen,
            TokenKind::Ident("n".into()),
            TokenKind::Colon,
            TokenKind::Ident("Movie".into()),
            TokenKind::Ampersand,
            TokenKind::Ident("Award".into()),
            TokenKind::RParen,
        ]
    );
}

#[test]
fn parameter_in_predicate() {
    // WHERE n.age > $minAge
    assert_eq!(
        kinds("WHERE n.age > $minAge"),
        vec![
            TokenKind::Keyword("where"),
            TokenKind::Ident("n".into()),
            TokenKind::Dot,
            TokenKind::Ident("age".into()),
            TokenKind::Gt,
            TokenKind::Parameter("minAge".into()),
        ]
    );
}

#[test]
fn cast_expression() {
    // CAST(x AS INTEGER)
    assert_eq!(
        kinds("CAST(x AS INTEGER)"),
        vec![
            TokenKind::Keyword("cast"),
            TokenKind::LParen,
            TokenKind::Ident("x".into()),
            TokenKind::Keyword("as"),
            TokenKind::Keyword("integer"),
            TokenKind::RParen,
        ]
    );
}

#[test]
fn type_cast_with_double_colon() {
    // x::INTEGER
    assert_eq!(
        kinds("x::INTEGER"),
        vec![
            TokenKind::Ident("x".into()),
            TokenKind::ColonColon,
            TokenKind::Keyword("integer"),
        ]
    );
}

// -- Spans ----------------------------------------------------

#[test]
fn span_of_first_token_starts_at_zero() {
    let toks = tokenize("MATCH").expect("ok");
    assert_eq!(toks[0].span, Span::new(0, 5));
}

#[test]
fn span_skips_leading_whitespace() {
    let toks = tokenize("   MATCH").expect("ok");
    assert_eq!(toks[0].span, Span::new(3, 8));
}

#[test]
fn span_of_string_includes_quotes() {
    let toks = tokenize("'hi'").expect("ok");
    assert_eq!(toks[0].span, Span::new(0, 4));
}

#[test]
fn span_of_two_byte_token() {
    let toks = tokenize("->").expect("ok");
    assert_eq!(toks[0].span, Span::new(0, 2));
}

#[test]
fn span_of_eof_is_at_end() {
    let toks = tokenize("x").expect("ok");
    assert_eq!(toks.last().expect("eof").span, Span::at(1));
}

#[test]
fn spans_advance_across_tokens() {
    let toks = tokenize("ab cd").expect("ok");
    assert_eq!(toks[0].span, Span::new(0, 2));
    assert_eq!(toks[1].span, Span::new(3, 5));
}

#[test]
fn span_after_block_comment() {
    let toks = tokenize("/* skip */ x").expect("ok");
    assert_eq!(toks[0].span, Span::new(11, 12));
}

// -- Error recovery -------------------------------------------

#[test]
fn unknown_character_recovers_and_keeps_lexing() {
    // a section sign, which GQL has no token for
    let errs = lex_err("\u{a7} MATCH");
    // its lex error doesn't hide the MATCH: one error recorded, the rest of the stream well-formed
    assert!(
        errs.iter()
            .all(|e| matches!(e.kind, ParseErrorKind::LexError))
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unexpected character"))
    );
}

#[test]
fn multiple_errors_in_one_pass() {
    // Two malformed escapes - both should surface.
    let errs = lex_err(r"'\q' '\z'");
    assert!(errs.len() >= 2);
    assert!(
        errs.iter()
            .all(|e| matches!(e.kind, ParseErrorKind::LexError))
    );
}

#[test]
fn parameter_error_doesnt_eat_following_tokens() {
    let errs = lex_err("$ name");
    assert!(errs.iter().any(|e| e.message.contains("parameter name")));
    // whatever the parser does next, the lexer must report the issue, not panic
}

// -- Boolean / null keywords ----------------------------------

#[test]
fn boolean_keywords() {
    assert_eq!(
        kinds("TRUE FALSE true false TrUe"),
        vec![
            TokenKind::Keyword("true"),
            TokenKind::Keyword("false"),
            TokenKind::Keyword("true"),
            TokenKind::Keyword("false"),
            TokenKind::Keyword("true"),
        ]
    );
}

#[test]
fn null_keyword() {
    assert_eq!(kinds("NULL"), vec![TokenKind::Keyword("null")]);
    assert_eq!(kinds("null"), vec![TokenKind::Keyword("null")]);
}

// -- Misc edge cases ------------------------------------------

#[test]
fn many_tokens_no_panic_on_long_input() {
    // 250 reps x 4 tokens (`MATCH ( n )`) + EOF = 1001 tokens.
    let mut s = String::new();
    for _ in 0..250 {
        s.push_str("MATCH (n) ");
    }
    let toks = tokenize(&s).expect("long input lexes");
    assert!(toks.len() >= 1000, "got {} tokens", toks.len());
}

#[test]
fn keyword_name_is_static_pointer_into_table() {
    // identity check: the keyword slice must come from RESERVED_WORDS via
    // `lookup_reserved_word`, not a fresh allocation (string-literal interning across modules
    // is best-effort, so the canonical pointer goes through the lexer's own lookup)
    let canonical = lookup_reserved_word("match").expect("table has match");
    let toks = tokenize("MATCH").expect("ok");
    if let TokenKind::Keyword(s) = &toks[0].kind {
        assert!(std::ptr::eq(*s as *const str, canonical as *const str));
    } else {
        panic!("expected keyword");
    }
}

#[test]
fn block_comment_does_not_swallow_following_token() {
    assert_eq!(kinds("/* c */MATCH"), vec![TokenKind::Keyword("match")]);
}

#[test]
fn line_comment_ends_at_newline() {
    assert_eq!(
        kinds("-- skip me\nMATCH"),
        vec![TokenKind::Keyword("match")]
    );
}

#[test]
fn delimited_ident_preserves_case() {
    assert_eq!(
        kinds("`MyVar`"),
        vec![TokenKind::DelimitedIdent("MyVar".into())]
    );
}

#[test]
fn empty_delimited_ident() {
    assert_eq!(kinds("``"), vec![TokenKind::DelimitedIdent(String::new())]);
}

#[test]
fn integer_zero() {
    assert_eq!(kinds("0"), vec![TokenKind::Integer(0)]);
}

#[test]
fn hex_max_i64() {
    assert_eq!(
        kinds("0x7FFFFFFFFFFFFFFF"),
        vec![TokenKind::Integer(i64::MAX)]
    );
}

#[test]
fn float_zero_dot_zero() {
    assert_eq!(kinds("0.0"), vec![TokenKind::Float(0.0)]);
}

#[test]
fn float_starts_with_dot_then_exponent() {
    assert_eq!(kinds(".5e2"), vec![TokenKind::Float(50.0)]);
}
