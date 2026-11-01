use traverse_cypher::types::CypherValue;

/// Split input on semicolons, respecting string literals.
///
/// Re-exported from `traverse-cypher` so the CLI, the server's import
/// endpoint and the WASM worker all cut scripts the same way. It lived here
/// first; the other two grew their own answers, one of which was wrong.
pub use traverse_cypher::statements::split_statements;

/// Parse a parameter value string into a CypherValue.
/// Supports: integers, floats, strings ('...' or "..."), true, false, null.
pub fn parse_param_value(s: &str) -> Result<CypherValue, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty value".into());
    }
    // null
    if s.eq_ignore_ascii_case("null") {
        return Ok(CypherValue::Null);
    }
    // booleans
    if s.eq_ignore_ascii_case("true") {
        return Ok(CypherValue::Bool(true));
    }
    if s.eq_ignore_ascii_case("false") {
        return Ok(CypherValue::Bool(false));
    }
    // string literal
    if (s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"')) {
        let inner = &s[1..s.len() - 1];
        return Ok(CypherValue::String(inner.to_string()));
    }
    // integer
    if let Ok(n) = s.parse::<i64>() {
        return Ok(CypherValue::Integer(n));
    }
    // float
    if let Ok(f) = s.parse::<f64>() {
        return Ok(CypherValue::Float(f));
    }
    // bare string
    Ok(CypherValue::String(s.to_string()))
}
