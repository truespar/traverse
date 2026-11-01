use std::borrow::Cow;

/// Cypher keywords to highlight (from traverse-cypher lexer).
const CYPHER_KEYWORDS: &[&str] = &[
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
    "ASCENDING",
    "DESC",
    "DESCENDING",
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
    "ANALYZE",
    "GRAPH",
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

/// Check if a word is a Cypher keyword (case-insensitive).
pub fn is_keyword(word: &str) -> bool {
    let upper = word.to_ascii_uppercase();
    CYPHER_KEYWORDS.iter().any(|&kw| kw == upper)
}

/// Apply syntax highlighting to a Cypher input line.
/// Returns an ANSI-colored string.
pub fn highlight_cypher(line: &str) -> Cow<'_, str> {
    let mut result = String::with_capacity(line.len() + 64);
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        let ch = chars[i];

        // String literal (single or double quoted)
        if ch == '\'' || ch == '"' {
            let quote = ch;
            result.push_str("\x1b[32m"); // green
            result.push(ch);
            i += 1;
            while i < len {
                let c = chars[i];
                result.push(c);
                i += 1;
                if c == quote {
                    // Check for escaped quote
                    if i < len && chars[i] == quote {
                        result.push(chars[i]);
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            result.push_str("\x1b[0m");
            continue;
        }

        // Line comment
        if ch == '/' && i + 1 < len && chars[i + 1] == '/' {
            result.push_str("\x1b[90m"); // dim gray
            while i < len && chars[i] != '\n' {
                result.push(chars[i]);
                i += 1;
            }
            result.push_str("\x1b[0m");
            continue;
        }

        // Number
        if ch.is_ascii_digit() || (ch == '-' && i + 1 < len && chars[i + 1].is_ascii_digit()) {
            // Only color negative numbers if preceded by a non-alphanumeric
            let is_neg = ch == '-';
            if is_neg && i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_') {
                result.push(ch);
                i += 1;
                continue;
            }
            result.push_str("\x1b[33m"); // yellow
            result.push(ch);
            i += 1;
            while i < len && (chars[i].is_ascii_digit() || chars[i] == '.') {
                result.push(chars[i]);
                i += 1;
            }
            result.push_str("\x1b[0m");
            continue;
        }

        // Identifier / keyword
        if ch.is_alphabetic() || ch == '_' {
            let start = i;
            while i < len && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if is_keyword(&word) {
                result.push_str("\x1b[1;34m"); // bold blue
                result.push_str(&word);
                result.push_str("\x1b[0m");
            } else {
                result.push_str(&word);
            }
            continue;
        }

        // Everything else
        result.push(ch);
        i += 1;
    }

    Cow::Owned(result)
}
