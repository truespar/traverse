use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use rustyline::Context;
use rustyline::Helper;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};

use crate::highlight;

/// Cypher keywords for tab completion.
const COMPLETION_KEYWORDS: &[&str] = &[
    "MATCH",
    "OPTIONAL",
    "WHERE",
    "RETURN",
    "WITH",
    "UNWIND",
    "UNION",
    "ALL",
    "CREATE",
    "DELETE",
    "DETACH",
    "SET",
    "REMOVE",
    "MERGE",
    "ON",
    "ORDER",
    "BY",
    "ASC",
    "DESC",
    "CALL",
    "YIELD",
    "SKIP",
    "LIMIT",
    "AS",
    "DISTINCT",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "AND",
    "OR",
    "XOR",
    "NOT",
    "IN",
    "IS",
    "CONTAINS",
    "STARTS",
    "ENDS",
    "EXISTS",
    "COUNT",
    "NULL",
    "TRUE",
    "FALSE",
    "INDEX",
    "DROP",
    "FOR",
    "EXPLAIN",
    "PROFILE",
    "CONSTRAINT",
    "UNIQUE",
    // User management
    "USER",
    "USERS",
    "PASSWORD",
    "ROLE",
    "ALTER",
    "SHOW",
    "ADMIN",
    "EDITOR",
    "READER",
    // Database management
    "DATABASE",
    "DATABASES",
    // Data import
    "LOAD",
    "CSV",
    "HEADERS",
    "FROM",
    "FIELDTERMINATOR",
];

/// Shared schema words (labels, relationship types, property keys) for completion.
pub type SchemaWords = Arc<Mutex<Vec<String>>>;

/// Create a new empty schema words container.
pub fn new_schema_words() -> SchemaWords {
    Arc::new(Mutex::new(Vec::new()))
}

/// Rustyline helper combining syntax highlighting, tab completion, and multi-line support.
pub struct CypherHelper {
    pub highlighting_enabled: bool,
    pub schema_words: SchemaWords,
}

impl CypherHelper {
    pub fn new(highlighting_enabled: bool, schema_words: SchemaWords) -> Self {
        Self {
            highlighting_enabled,
            schema_words,
        }
    }
}

impl Helper for CypherHelper {}

impl Highlighter for CypherHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if self.highlighting_enabled {
            highlight::highlight_cypher(line)
        } else {
            Cow::Borrowed(line)
        }
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        self.highlighting_enabled
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        default: bool,
    ) -> Cow<'b, str> {
        if default {
            Cow::Owned(format!("\x1b[1;32m{prompt}\x1b[0m"))
        } else {
            Cow::Borrowed(prompt)
        }
    }
}

impl Completer for CypherHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        // Find the start of the current word
        let before = &line[..pos];
        let word_start = before
            .rfind(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|p| p + 1)
            .unwrap_or(0);
        let prefix = &before[word_start..];

        // Check if we're right after a dot (property access context)
        let after_dot = word_start > 0 && before.as_bytes()[word_start - 1] == b'.';

        if prefix.is_empty() && !after_dot {
            return Ok((pos, vec![]));
        }

        let upper_prefix = prefix.to_ascii_uppercase();
        let lower_prefix = prefix.to_ascii_lowercase();

        let mut candidates: Vec<Pair> = Vec::new();

        // Keyword completions (only when not in property access context)
        if !after_dot {
            for kw in COMPLETION_KEYWORDS {
                if kw.starts_with(&upper_prefix) {
                    candidates.push(Pair {
                        display: kw.to_string(),
                        replacement: kw.to_string(),
                    });
                }
            }
        }

        // Schema word completions (labels, types, properties)
        if let Ok(words) = self.schema_words.lock() {
            for word in words.iter() {
                if prefix.is_empty() || word.to_ascii_lowercase().starts_with(&lower_prefix) {
                    candidates.push(Pair {
                        display: word.clone(),
                        replacement: word.clone(),
                    });
                }
            }
        }

        Ok((word_start, candidates))
    }
}

impl Hinter for CypherHelper {
    type Hint = String;

    fn hint(&self, _line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<String> {
        None
    }
}

impl Validator for CypherHelper {
    fn validate(&self, ctx: &mut ValidationContext<'_>) -> rustyline::Result<ValidationResult> {
        let input = ctx.input().trim();

        // Empty input is valid (just press enter)
        if input.is_empty() {
            return Ok(ValidationResult::Valid(None));
        }

        // Meta-commands are always complete
        if input.starts_with(':') {
            return Ok(ValidationResult::Valid(None));
        }

        // If the input ends with a semicolon, it's complete
        if input.ends_with(';') {
            return Ok(ValidationResult::Valid(None));
        }

        // Otherwise, request more input (multi-line)
        Ok(ValidationResult::Incomplete)
    }
}
