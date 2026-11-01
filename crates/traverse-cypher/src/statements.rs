//! Splitting a script into statements.
//!
//! The parser takes exactly one statement plus an optional trailing semicolon,
//! so anything that accepts pasted scripts (CLI, server, WASM worker) splits
//! them here first.
//!
//! A bare `split(';')` fails quietly: `CREATE (a:Person {name: 'Smith; Jones'})`
//! splits inside the string into two invalid fragments, and a legitimate import
//! fails with a syntax error pointing at nothing the user wrote wrong.

/// Splits a script on semicolons, ignoring those inside string literals and line
/// comments. Empty statements are dropped, results trimmed, a trailing statement
/// without a semicolon kept.
///
/// Single and double quotes both delimit strings; a doubled quote inside one is
/// an escaped quote, not its end.
pub fn split_statements(input: &str) -> Vec<&str> {
    let mut statements = Vec::new();
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut start = 0;

    while i < len {
        let b = bytes[i];
        // skip string literals
        if b == b'\'' || b == b'"' {
            let quote = b;
            i += 1;
            while i < len {
                if bytes[i] == quote {
                    i += 1;
                    // escaped (doubled) quote?
                    if i < len && bytes[i] == quote {
                        i += 1;
                    } else {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        // skip line comments
        if b == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b';' {
            let stmt = input[start..i].trim();
            if !stmt.is_empty() {
                statements.push(stmt);
            }
            start = i + 1;
        }
        i += 1;
    }

    // trailing statement without semicolon
    let tail = input[start..].trim();
    if !tail.is_empty() {
        statements.push(tail);
    }

    statements
}

#[cfg(test)]
mod tests {
    use super::split_statements;

    /// the shape the Studio's import placeholder suggests: three statements, each ending in a
    /// semicolon
    #[test]
    fn splits_the_suggested_import_shape() {
        let script = "CREATE (a:Person {name: 'Alice'});\n\
                      CREATE (b:Person {name: 'Bob'});\n\
                      CREATE (a)-[:KNOWS]->(b);";
        let stmts = split_statements(script);
        assert_eq!(stmts.len(), 3);
        assert_eq!(stmts[0], "CREATE (a:Person {name: 'Alice'})");
        assert_eq!(stmts[2], "CREATE (a)-[:KNOWS]->(b)");
    }

    /// why a bare `split(';')` won't do; both quote styles, since Cypher accepts both
    #[test]
    fn a_semicolon_inside_a_string_is_not_a_separator() {
        let stmts = split_statements("CREATE (a:Person {name: 'Smith; Jones'})");
        assert_eq!(stmts, vec!["CREATE (a:Person {name: 'Smith; Jones'})"]);

        let stmts = split_statements("CREATE (a:P {n: \"a;b\"});CREATE (b:P)");
        assert_eq!(stmts, vec!["CREATE (a:P {n: \"a;b\"})", "CREATE (b:P)"]);
    }

    #[test]
    fn a_doubled_quote_escapes_rather_than_ends_the_string() {
        let stmts = split_statements("CREATE (a:P {n: 'it''s; fine'});CREATE (b:P)");
        assert_eq!(
            stmts,
            vec!["CREATE (a:P {n: 'it''s; fine'})", "CREATE (b:P)"]
        );
    }

    #[test]
    fn a_semicolon_in_a_line_comment_is_not_a_separator() {
        let stmts = split_statements("MATCH (n) RETURN n // count them; carefully\n");
        assert_eq!(stmts, vec!["MATCH (n) RETURN n // count them; carefully"]);
    }

    #[test]
    fn trailing_statement_without_a_semicolon_is_kept() {
        let stmts = split_statements("CREATE (a:P);\nCREATE (b:P)");
        assert_eq!(stmts, vec!["CREATE (a:P)", "CREATE (b:P)"]);
    }

    #[test]
    fn empty_and_whitespace_only_statements_are_dropped() {
        assert!(split_statements("").is_empty());
        assert!(split_statements("   \n  ").is_empty());
        assert!(split_statements(";;;").is_empty());
        assert_eq!(split_statements("CREATE (a:P);;;"), vec!["CREATE (a:P)"]);
    }

    /// a lone statement without a semicolon (the ordinary query) comes back unchanged, not as zero
    /// statements
    #[test]
    fn a_lone_statement_survives_intact() {
        assert_eq!(
            split_statements("MATCH (n) RETURN n"),
            vec!["MATCH (n) RETURN n"]
        );
    }
}
