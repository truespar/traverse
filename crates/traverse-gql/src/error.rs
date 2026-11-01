//! Parser diagnostics. The parser returns a `Program` on success, else a `Vec<ParseError>`: several
//! per call, so editors get every issue in one round trip.

use crate::lexer::Span;

/// One parser diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// Byte span in the source; `0..0` when there's no specific span (e.g. EOF before any token).
    pub span: Span,
    /// Machine-readable category.
    pub kind: ParseErrorKind,
    /// Human-readable message: what was expected vs. what was found.
    pub message: String,
}

/// Kind of parse error; the usual failure points of a recursive-descent parser.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseErrorKind {
    /// Text the lexer can't tokenize.
    LexError,
    /// Token the parser didn't expect here.
    UnexpectedToken,
    /// Input ended mid-production.
    UnexpectedEof,
    /// A recognized construct that isn't supported (mostly raised by lowering).
    NotImplemented,
}

impl ParseError {
    /// [`ParseErrorKind::LexError`].
    pub fn lex(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            kind: ParseErrorKind::LexError,
            message: message.into(),
        }
    }

    /// [`ParseErrorKind::UnexpectedToken`].
    pub fn unexpected_token(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            kind: ParseErrorKind::UnexpectedToken,
            message: message.into(),
        }
    }

    /// [`ParseErrorKind::UnexpectedEof`].
    pub fn unexpected_eof(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            kind: ParseErrorKind::UnexpectedEof,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}..{}: {}",
            self.span.start, self.span.end, self.message
        )
    }
}

impl std::error::Error for ParseError {}
