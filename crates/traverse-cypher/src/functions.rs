//! Built-in Cypher scalar functions.

use std::collections::BTreeMap;

use unicode_normalization::UnicodeNormalization;

use crate::error::CypherError;
use crate::temporal;
use crate::types::CypherValue;

/// Dispatches a built-in function call.
pub fn call_function(name: &str, args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    match name {
        // -- Identity & type functions ----------------------------------
        "id" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Node { id, .. } => Ok(CypherValue::Integer(id.0 as i64)),
                CypherValue::Edge { id, .. } => Ok(CypherValue::Integer(id.0 as i64)),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "id() requires a node or relationship, got {}",
                    args[0].type_name()
                ))),
            }
        }
        // `elementId()`: Neo4j's opaque String element id (replacing the deprecated `id()`).
        // There's no separate element-id space, so it's the internal id as a decimal string:
        // stable, unique, opaque to drivers. Lazy NodeRef/EdgeRef are handled in the executor first
        // (like `id`)
        "elementid" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Node { id, .. } => Ok(CypherValue::String(id.0.to_string())),
                CypherValue::Edge { id, .. } => Ok(CypherValue::String(id.0.to_string())),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "elementId() requires a node or relationship, got {}",
                    args[0].type_name()
                ))),
            }
        }
        "type" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Edge { edge_type, .. } => Ok(CypherValue::String(edge_type.clone())),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "type() requires a relationship, got {}",
                    args[0].type_name()
                ))),
            }
        }
        "labels" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Node { labels, .. } => Ok(CypherValue::List(
                    labels
                        .iter()
                        .map(|l| CypherValue::String(l.clone()))
                        .collect(),
                )),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "labels() requires a node, got {}",
                    args[0].type_name()
                ))),
            }
        }
        "keys" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Node { properties, .. } => Ok(CypherValue::List(
                    properties
                        .keys()
                        .map(|k| CypherValue::String(k.clone()))
                        .collect(),
                )),
                CypherValue::Edge { properties, .. } => Ok(CypherValue::List(
                    properties
                        .keys()
                        .map(|k| CypherValue::String(k.clone()))
                        .collect(),
                )),
                CypherValue::Map(m) => Ok(CypherValue::List(
                    m.keys().map(|k| CypherValue::String(k.clone())).collect(),
                )),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "keys() requires a node, relationship, or map, got {}",
                    args[0].type_name()
                ))),
            }
        }
        "properties" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Node { properties, .. } => Ok(CypherValue::Map(
                    properties
                        .iter()
                        .map(|(k, v)| (k.clone(), CypherValue::from_property(v)))
                        .collect(),
                )),
                CypherValue::Edge { properties, .. } => Ok(CypherValue::Map(
                    properties
                        .iter()
                        .map(|(k, v)| (k.clone(), CypherValue::from_property(v)))
                        .collect(),
                )),
                CypherValue::Map(m) => Ok(CypherValue::Map(m.clone())),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "properties() requires a node, relationship, or map, got {}",
                    args[0].type_name()
                ))),
            }
        }

        // -- Conversion functions --------------------------------------
        "tostring" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Null => Ok(CypherValue::Null),
                CypherValue::String(s) => Ok(CypherValue::String(s.clone())),
                CypherValue::Integer(i) => Ok(CypherValue::String(i.to_string())),
                CypherValue::Float(f) => Ok(CypherValue::String(f.to_string())),
                CypherValue::Bool(b) => Ok(CypherValue::String(b.to_string())),
                CypherValue::Date(_)
                | CypherValue::LocalTime(_)
                | CypherValue::Time(_)
                | CypherValue::LocalDateTime(_)
                | CypherValue::DateTime(_)
                | CypherValue::Duration(_) => {
                    Ok(CypherValue::String(temporal::temporal_to_string(&args[0])))
                }
                other => Err(CypherError::type_error(format!(
                    "cannot convert {} to STRING",
                    other.type_name()
                ))),
            }
        }
        "tointeger" | "toint" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Null => Ok(CypherValue::Null),
                CypherValue::Integer(i) => Ok(CypherValue::Integer(*i)),
                CypherValue::Float(f) => Ok(CypherValue::Integer(*f as i64)),
                CypherValue::String(s) => {
                    // try float first ("1.5" -> 1), then int
                    if let Ok(v) = s.parse::<i64>() {
                        Ok(CypherValue::Integer(v))
                    } else if let Ok(f) = s.parse::<f64>() {
                        Ok(CypherValue::Integer(f as i64))
                    } else {
                        Ok(CypherValue::Null)
                    }
                }
                CypherValue::Bool(b) => Ok(CypherValue::Integer(if *b { 1 } else { 0 })),
                other => Err(CypherError::type_error(format!(
                    "cannot convert {} to INTEGER",
                    other.type_name()
                ))),
            }
        }
        "tofloat" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Null => Ok(CypherValue::Null),
                CypherValue::Integer(i) => Ok(CypherValue::Float(*i as f64)),
                CypherValue::Float(f) => Ok(CypherValue::Float(*f)),
                CypherValue::String(s) => s
                    .parse::<f64>()
                    .map(CypherValue::Float)
                    .ok()
                    .ok_or(CypherValue::Null)
                    .or(Ok(CypherValue::Null)),
                other => Err(CypherError::type_error(format!(
                    "cannot convert {} to FLOAT",
                    other.type_name()
                ))),
            }
        }
        "toboolean" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Null => Ok(CypherValue::Null),
                CypherValue::Bool(b) => Ok(CypherValue::Bool(*b)),
                CypherValue::String(s) => match s.to_lowercase().as_str() {
                    "true" => Ok(CypherValue::Bool(true)),
                    "false" => Ok(CypherValue::Bool(false)),
                    _ => Ok(CypherValue::Null),
                },
                CypherValue::Integer(i) => Ok(CypherValue::Bool(*i != 0)),
                other => Err(CypherError::type_error(format!(
                    "cannot convert {} to BOOLEAN",
                    other.type_name()
                ))),
            }
        }

        // elementwise list conversions: Neo4j requires LIST<ANY>; other non-null
        // arguments are type errors. A failing element becomes NULL, length and order
        // are kept. All four share `convert_list_elementwise` with the singular forms'
        // per-element logic.
        "tobooleanlist" => {
            check_arity(name, args, 1)?;
            convert_list_elementwise(name, &args[0], |v| match v {
                CypherValue::Null => CypherValue::Null,
                CypherValue::Bool(b) => CypherValue::Bool(*b),
                CypherValue::String(s) => match s.to_lowercase().as_str() {
                    "true" => CypherValue::Bool(true),
                    "false" => CypherValue::Bool(false),
                    _ => CypherValue::Null,
                },
                CypherValue::Integer(i) => CypherValue::Bool(*i != 0),
                _ => CypherValue::Null,
            })
        }
        "tofloatlist" => {
            check_arity(name, args, 1)?;
            convert_list_elementwise(name, &args[0], |v| match v {
                CypherValue::Null => CypherValue::Null,
                CypherValue::Integer(i) => CypherValue::Float(*i as f64),
                CypherValue::Float(f) => CypherValue::Float(*f),
                CypherValue::String(s) => s
                    .parse::<f64>()
                    .map(CypherValue::Float)
                    .unwrap_or(CypherValue::Null),
                _ => CypherValue::Null,
            })
        }
        "tointegerlist" => {
            check_arity(name, args, 1)?;
            convert_list_elementwise(name, &args[0], |v| match v {
                CypherValue::Null => CypherValue::Null,
                CypherValue::Integer(i) => CypherValue::Integer(*i),
                CypherValue::Float(f) => CypherValue::Integer(*f as i64),
                CypherValue::String(s) => {
                    if let Ok(n) = s.parse::<i64>() {
                        CypherValue::Integer(n)
                    } else if let Ok(f) = s.parse::<f64>() {
                        CypherValue::Integer(f as i64)
                    } else {
                        CypherValue::Null
                    }
                }
                CypherValue::Bool(b) => CypherValue::Integer(if *b { 1 } else { 0 }),
                _ => CypherValue::Null,
            })
        }
        "tostringlist" => {
            check_arity(name, args, 1)?;
            convert_list_elementwise(name, &args[0], |v| match v {
                CypherValue::Null => CypherValue::Null,
                CypherValue::String(s) => CypherValue::String(s.clone()),
                CypherValue::Integer(i) => CypherValue::String(i.to_string()),
                CypherValue::Float(f) => CypherValue::String(f.to_string()),
                CypherValue::Bool(b) => CypherValue::String(b.to_string()),
                CypherValue::Date(_)
                | CypherValue::LocalTime(_)
                | CypherValue::Time(_)
                | CypherValue::LocalDateTime(_)
                | CypherValue::DateTime(_)
                | CypherValue::Duration(_) => CypherValue::String(temporal::temporal_to_string(v)),
                _ => CypherValue::Null,
            })
        }

        // -- Spatial: point() family -----------------------------------
        //
        // Points are Maps `{ x: f64, y: f64, z?: f64, srid: i64, crs: String }` (no
        // dedicated Point CypherValue). Neo4j SRIDs:
        //   7203 = cartesian (2D),   9157 = cartesian-3d
        //   4326 = wgs-84 (2D),      4979 = wgs-84-3d
        // `.x`, `.y`, `.z`, `.srid`, `.crs` go through Map property access, so the
        // visible shape matches Neo4j.
        "point" => {
            check_arity(name, args, 1)?;
            point_from_map(&args[0])
        }
        "point.distance" => {
            check_arity(name, args, 2)?;
            point_distance(&args[0], &args[1])
        }
        "point.withinbbox" => {
            check_arity(name, args, 3)?;
            point_within_bbox(&args[0], &args[1], &args[2])
        }

        // -- String functions ------------------------------------------
        //
        // GQL (ISO/IEC 39075) notes:
        // * `upper` / `lower` (GQL) and `toUpper` / `toLower` (Cypher) both resolve here.
        // * `trim` / `btrim` / `ltrim` / `rtrim` strip whitespace by default, or any of the
        //   characters in an optional second argument (Neo4j's multi-character trim). GQL
        //   section 20.24 defaults to U+0020 only: the GQL dialect passes `' '` explicitly, and its
        //   `TRIM(... FROM ...)` lowers to `gql_trim`.
        "toupper" | "touppercase" | "upper" => {
            check_arity(name, args, 1)?;
            string_fn(&args[0], |s| s.to_uppercase())
        }
        "tolower" | "tolowercase" | "lower" => {
            check_arity(name, args, 1)?;
            string_fn(&args[0], |s| s.to_lowercase())
        }
        "trim" | "btrim" => trim_fn(name, args, Trim::Both),
        "ltrim" => trim_fn(name, args, Trim::Leading),
        "rtrim" => trim_fn(name, args, Trim::Trailing),
        "gql_trim" => gql_trim(args),
        "normalize" => {
            // GQL section 20.24 NORMALIZE(string [, form]): NFC/NFD/NFKC/NFKD, any case, default NFC
            if args.is_empty() || args.len() > 2 {
                return Err(CypherError::type_error(
                    "normalize() expects 1 or 2 arguments".to_string(),
                ));
            }
            let input = match &args[0] {
                CypherValue::String(s) => s,
                CypherValue::Null => return Ok(CypherValue::Null),
                _ => {
                    return Err(CypherError::type_error(format!(
                        "normalize() requires a string, got {}",
                        args[0].type_name()
                    )));
                }
            };
            let form = if args.len() == 2 {
                match &args[1] {
                    CypherValue::String(s) => s.clone(),
                    CypherValue::Null => return Ok(CypherValue::Null),
                    _ => {
                        return Err(CypherError::type_error(format!(
                            "normalize() form must be a string, got {}",
                            args[1].type_name()
                        )));
                    }
                }
            } else {
                "NFC".to_string()
            };
            Ok(CypherValue::String(normalize_string(input, &form)?))
        }
        "gql_is_normalized" => {
            // ISO section 19.7 IS [NOT] NORMALIZED: true iff already in the form. Null input -> false (type
            // predicates don't propagate null)
            if args.is_empty() || args.len() > 2 {
                return Err(CypherError::type_error(
                    "gql_is_normalized() expects 1 or 2 arguments".to_string(),
                ));
            }
            let input = match &args[0] {
                CypherValue::String(s) => s,
                CypherValue::Null => return Ok(CypherValue::Bool(false)),
                _ => return Ok(CypherValue::Bool(false)),
            };
            let form = if args.len() == 2 {
                match &args[1] {
                    CypherValue::String(s) => s.clone(),
                    _ => "NFC".to_string(),
                }
            } else {
                "NFC".to_string()
            };
            let normalised = normalize_string(input, &form)?;
            Ok(CypherValue::Bool(normalised.as_str() == input.as_str()))
        }
        "char_length" | "character_length" => {
            // GQL section 20.22 character_length: code points, not bytes (`size`/`length` give bytes for
            // strings, as Cypher always did)
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::String(s) => Ok(CypherValue::Integer(s.chars().count() as i64)),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "{name}() requires a string, got {}",
                    args[0].type_name()
                ))),
            }
        }
        "replace" => {
            // Cypher 25 (Neo4j 2025.06) optional 4th `limit` arg caps replacements
            if args.len() != 3 && args.len() != 4 {
                return Err(CypherError::type_error(
                    "replace() requires 3 or 4 arguments".to_string(),
                ));
            }
            match (&args[0], &args[1], &args[2]) {
                (CypherValue::String(s), CypherValue::String(from), CypherValue::String(to)) => {
                    if args.len() == 4 {
                        match &args[3] {
                            CypherValue::Integer(n) if *n >= 0 => Ok(CypherValue::String(
                                s.replacen(from.as_str(), to.as_str(), *n as usize),
                            )),
                            CypherValue::Null => Ok(CypherValue::Null),
                            _ => Err(CypherError::type_error(
                                "replace() limit must be a non-negative integer".to_string(),
                            )
                            .with_status(crate::status::GqlStatus::NegativeValue)),
                        }
                    } else {
                        Ok(CypherValue::String(s.replace(from.as_str(), to.as_str())))
                    }
                }
                _ if args.iter().any(|a| a.is_null()) => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "replace() requires string arguments".to_string(),
                )),
            }
        }

        // -- Cypher 25 list library (`coll.*`, Neo4j 2025.11) --------------
        "coll.distinct" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Null => Ok(CypherValue::Null),
                CypherValue::List(items) => {
                    let mut out: Vec<CypherValue> = Vec::new();
                    for it in items {
                        if !out.iter().any(|x| x == it) {
                            out.push(it.clone());
                        }
                    }
                    Ok(CypherValue::List(out))
                }
                other => Err(coll_type_err("coll.distinct", other)),
            }
        }
        "coll.flatten" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Null => Ok(CypherValue::Null),
                CypherValue::List(items) => {
                    let mut out: Vec<CypherValue> = Vec::new();
                    for it in items {
                        match it {
                            CypherValue::List(inner) => out.extend(inner.iter().cloned()),
                            other => out.push(other.clone()),
                        }
                    }
                    Ok(CypherValue::List(out))
                }
                other => Err(coll_type_err("coll.flatten", other)),
            }
        }
        "coll.indexof" => {
            check_arity(name, args, 2)?;
            match &args[0] {
                CypherValue::Null => Ok(CypherValue::Null),
                CypherValue::List(items) => {
                    let idx = items.iter().position(|x| x == &args[1]);
                    Ok(CypherValue::Integer(idx.map(|i| i as i64).unwrap_or(-1)))
                }
                other => Err(coll_type_err("coll.indexOf", other)),
            }
        }
        "coll.min" | "coll.max" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Null => Ok(CypherValue::Null),
                CypherValue::List(items) => {
                    let non_null: Vec<&CypherValue> =
                        items.iter().filter(|v| !v.is_null()).collect();
                    if non_null.is_empty() {
                        return Ok(CypherValue::Null);
                    }
                    let want_max = name == "coll.max";
                    let mut best = non_null[0];
                    for v in &non_null[1..] {
                        let ord = crate::executor::compare_values(v, best);
                        if (want_max && ord == std::cmp::Ordering::Greater)
                            || (!want_max && ord == std::cmp::Ordering::Less)
                        {
                            best = v;
                        }
                    }
                    Ok(best.clone())
                }
                other => Err(coll_type_err(name, other)),
            }
        }
        "coll.sort" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Null => Ok(CypherValue::Null),
                CypherValue::List(items) => {
                    let mut out = items.clone();
                    out.sort_by(crate::executor::compare_values);
                    Ok(CypherValue::List(out))
                }
                other => Err(coll_type_err("coll.sort", other)),
            }
        }
        "coll.insert" => {
            check_arity(name, args, 3)?;
            match (&args[0], &args[1]) {
                (CypherValue::Null, _) => Ok(CypherValue::Null),
                (CypherValue::List(items), CypherValue::Integer(idx)) => {
                    let mut out = items.clone();
                    let pos = (*idx).clamp(0, out.len() as i64) as usize;
                    out.insert(pos, args[2].clone());
                    Ok(CypherValue::List(out))
                }
                _ => Err(CypherError::type_error(
                    "coll.insert() requires (list, integer, value)".to_string(),
                )),
            }
        }
        "coll.remove" => {
            if args.len() != 2 && args.len() != 3 {
                return Err(CypherError::type_error(
                    "coll.remove() requires (list, index [, count])".to_string(),
                ));
            }
            match (&args[0], &args[1]) {
                (CypherValue::Null, _) => Ok(CypherValue::Null),
                (CypherValue::List(items), CypherValue::Integer(idx)) => {
                    let mut out = items.clone();
                    let count = match args.get(2) {
                        Some(CypherValue::Integer(c)) => (*c).max(0) as usize,
                        _ => 1,
                    };
                    if *idx >= 0 && (*idx as usize) < out.len() {
                        let start = *idx as usize;
                        let end = (start + count).min(out.len());
                        out.drain(start..end);
                    }
                    Ok(CypherValue::List(out))
                }
                _ => Err(CypherError::type_error(
                    "coll.remove() requires (list, integer [, integer])".to_string(),
                )),
            }
        }

        // -- Cypher 25 string library (`string.*`, Neo4j 2026.05) ----------
        "string.join" => {
            if args.len() != 1 && args.len() != 2 {
                return Err(CypherError::type_error(
                    "string.join() requires (list [, delimiter])".to_string(),
                ));
            }
            match &args[0] {
                CypherValue::Null => Ok(CypherValue::Null),
                CypherValue::List(items) => {
                    let delim = match args.get(1) {
                        Some(CypherValue::String(d)) => d.as_str(),
                        Some(CypherValue::Null) | None => "",
                        Some(_) => {
                            return Err(CypherError::type_error(
                                "string.join() delimiter must be a string".to_string(),
                            ));
                        }
                    };
                    let mut parts: Vec<String> = Vec::with_capacity(items.len());
                    for it in items {
                        match it {
                            CypherValue::String(s) => parts.push(s.clone()),
                            CypherValue::Null => {} // Neo4j skips nulls
                            other => {
                                return Err(CypherError::type_error(format!(
                                    "string.join() requires a list of strings, got {}",
                                    other.type_name()
                                )));
                            }
                        }
                    }
                    Ok(CypherValue::String(parts.join(delim)))
                }
                other => Err(CypherError::type_error(format!(
                    "string.join() requires a list, got {}",
                    other.type_name()
                ))),
            }
        }
        "string.indexof" => {
            if args.len() != 2 && args.len() != 3 {
                return Err(CypherError::type_error(
                    "string.indexOf() requires (string, substring [, from])".to_string(),
                ));
            }
            match (&args[0], &args[1]) {
                (CypherValue::Null, _) | (_, CypherValue::Null) => Ok(CypherValue::Null),
                (CypherValue::String(s), CypherValue::String(sub)) => {
                    // character (not byte) positions, as in Neo4j
                    let from = match args.get(2) {
                        Some(CypherValue::Integer(i)) => (*i).max(0) as usize,
                        _ => 0,
                    };
                    let hay: Vec<char> = s.chars().collect();
                    let needle: Vec<char> = sub.chars().collect();
                    let start = from.min(hay.len());
                    let mut found: i64 = -1;
                    if needle.is_empty() {
                        found = start as i64;
                    } else if needle.len() <= hay.len() {
                        for i in start..=(hay.len() - needle.len()) {
                            if hay[i..i + needle.len()] == needle[..] {
                                found = i as i64;
                                break;
                            }
                        }
                    }
                    Ok(CypherValue::Integer(found))
                }
                _ => Err(CypherError::type_error(
                    "string.indexOf() requires string arguments".to_string(),
                )),
            }
        }
        "string.regexreplace" => {
            check_arity(name, args, 3)?;
            match (&args[0], &args[1], &args[2]) {
                (CypherValue::String(s), CypherValue::String(re), CypherValue::String(rep)) => {
                    match regex::Regex::new(re) {
                        Ok(compiled) => Ok(CypherValue::String(
                            compiled.replace_all(s, rep.as_str()).into_owned(),
                        )),
                        Err(e) => Err(CypherError::type_error(format!(
                            "string.regexReplace() invalid regex: {e}"
                        ))),
                    }
                }
                _ if args.iter().any(|a| a.is_null()) => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "string.regexReplace() requires string arguments".to_string(),
                )),
            }
        }

        // -- `*OrNull` coercions: null instead of an error for unconvertible values --
        "tobooleanornull" => {
            check_arity(name, args, 1)?;
            Ok(call_function("toboolean", &args[..1]).unwrap_or(CypherValue::Null))
        }
        "tofloatornull" => {
            check_arity(name, args, 1)?;
            Ok(call_function("tofloat", &args[..1]).unwrap_or(CypherValue::Null))
        }
        "tointegerornull" => {
            check_arity(name, args, 1)?;
            Ok(call_function("tointeger", &args[..1]).unwrap_or(CypherValue::Null))
        }
        "tostringornull" => {
            check_arity(name, args, 1)?;
            Ok(call_function("tostring", &args[..1]).unwrap_or(CypherValue::Null))
        }
        "substring" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(CypherError::type_error(
                    "substring() requires 2 or 3 arguments".to_string(),
                ));
            }
            match &args[0] {
                CypherValue::String(s) => {
                    let start = match &args[1] {
                        CypherValue::Integer(i) => *i as usize,
                        _ => {
                            return Err(CypherError::type_error(
                                "substring() requires integer start".to_string(),
                            ));
                        }
                    };
                    let len = if args.len() == 3 {
                        match &args[2] {
                            CypherValue::Integer(i) => Some(*i as usize),
                            _ => {
                                return Err(CypherError::type_error(
                                    "substring() requires integer length".to_string(),
                                ));
                            }
                        }
                    } else {
                        None
                    };
                    let chars: Vec<char> = s.chars().collect();
                    let start = start.min(chars.len());
                    let end = match len {
                        Some(l) => (start + l).min(chars.len()),
                        None => chars.len(),
                    };
                    Ok(CypherValue::String(chars[start..end].iter().collect()))
                }
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "substring() requires a string".to_string(),
                )),
            }
        }
        "left" => {
            check_arity(name, args, 2)?;
            match (&args[0], &args[1]) {
                (CypherValue::String(s), CypherValue::Integer(n)) => {
                    if *n < 0 {
                        return Err(CypherError::type_error(
                            "left() length cannot be negative".to_string(),
                        ));
                    }
                    Ok(CypherValue::String(s.chars().take(*n as usize).collect()))
                }
                _ if args.iter().any(|a| a.is_null()) => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "left() requires string and integer".to_string(),
                )),
            }
        }
        "right" => {
            check_arity(name, args, 2)?;
            match (&args[0], &args[1]) {
                (CypherValue::String(s), CypherValue::Integer(n)) => {
                    if *n < 0 {
                        return Err(CypherError::type_error(
                            "right() length cannot be negative".to_string(),
                        ));
                    }
                    let chars: Vec<char> = s.chars().collect();
                    let start = chars.len().saturating_sub(*n as usize);
                    Ok(CypherValue::String(chars[start..].iter().collect()))
                }
                _ if args.iter().any(|a| a.is_null()) => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "right() requires string and integer".to_string(),
                )),
            }
        }
        "split" => {
            check_arity(name, args, 2)?;
            match (&args[0], &args[1]) {
                (CypherValue::String(s), CypherValue::String(delim)) => {
                    let parts: Vec<CypherValue> = s
                        .split(delim.as_str())
                        .map(|p| CypherValue::String(p.to_string()))
                        .collect();
                    Ok(CypherValue::List(parts))
                }
                _ if args.iter().any(|a| a.is_null()) => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "split() requires string arguments".to_string(),
                )),
            }
        }
        "reverse" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::String(s) => Ok(CypherValue::String(s.chars().rev().collect())),
                CypherValue::List(l) => Ok(CypherValue::List(l.iter().rev().cloned().collect())),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "reverse() requires string or list".to_string(),
                )),
            }
        }

        // -- Numeric functions -----------------------------------------
        "abs" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Integer(i) => Ok(CypherValue::Integer(i.abs())),
                CypherValue::Float(f) => Ok(CypherValue::Float(f.abs())),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "abs() requires a numeric value".to_string(),
                )),
            }
        }
        // GQL section GF01 spells `ceil` as `ceiling`; both resolve here
        "ceil" | "ceiling" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Integer(i) => Ok(CypherValue::Integer(*i)),
                CypherValue::Float(f) => Ok(CypherValue::Float(f.ceil())),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "ceil() requires a numeric value".to_string(),
                )),
            }
        }
        "mod" => {
            // GQL section GF01 MOD(a, b): numeric remainder, like `%`
            check_arity(name, args, 2)?;
            match (&args[0], &args[1]) {
                (CypherValue::Integer(a), CypherValue::Integer(b)) => {
                    if *b == 0 {
                        return Err(CypherError::type_error(
                            "mod() divisor cannot be zero".to_string(),
                        ));
                    }
                    Ok(CypherValue::Integer(a % b))
                }
                (CypherValue::Float(a), CypherValue::Float(b)) => Ok(CypherValue::Float(a % b)),
                (CypherValue::Integer(a), CypherValue::Float(b)) => {
                    Ok(CypherValue::Float(*a as f64 % b))
                }
                (CypherValue::Float(a), CypherValue::Integer(b)) => {
                    Ok(CypherValue::Float(a % *b as f64))
                }
                _ if args.iter().any(|a| a.is_null()) => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "mod() requires numeric arguments".to_string(),
                )),
            }
        }
        "power" => {
            // GQL section 20.22 POWER(base, exponent), the function form of Cypher's `^`; Float result
            check_arity(name, args, 2)?;
            let base = match &args[0] {
                CypherValue::Integer(i) => *i as f64,
                CypherValue::Float(f) => *f,
                CypherValue::Null => return Ok(CypherValue::Null),
                _ => {
                    return Err(CypherError::type_error(
                        "power() requires numeric base".to_string(),
                    ));
                }
            };
            let exp = match &args[1] {
                CypherValue::Integer(i) => *i as f64,
                CypherValue::Float(f) => *f,
                CypherValue::Null => return Ok(CypherValue::Null),
                _ => {
                    return Err(CypherError::type_error(
                        "power() requires numeric exponent".to_string(),
                    ));
                }
            };
            Ok(CypherValue::Float(base.powf(exp)))
        }
        "floor" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Integer(i) => Ok(CypherValue::Integer(*i)),
                CypherValue::Float(f) => Ok(CypherValue::Float(f.floor())),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "floor() requires a numeric value".to_string(),
                )),
            }
        }
        "round" => {
            // Neo4j round(x), round(x, precision), round(x, precision, mode); HALF_UP default
            if args.is_empty() || args.len() > 3 {
                return Err(CypherError::type_error(format!(
                    "round() expected 1 to 3 arguments, got {}",
                    args.len()
                )));
            }
            if args.len() >= 2 {
                let x = match &args[0] {
                    CypherValue::Integer(i) => *i as f64,
                    CypherValue::Float(f) => *f,
                    CypherValue::Null => return Ok(CypherValue::Null),
                    _ => {
                        return Err(CypherError::type_error(
                            "round() requires a numeric value".to_string(),
                        ));
                    }
                };
                let prec = match &args[1] {
                    CypherValue::Integer(i) => *i,
                    CypherValue::Float(f) => *f as i64,
                    CypherValue::Null => return Ok(CypherValue::Null),
                    _ => {
                        return Err(CypherError::type_error(
                            "round() precision must be numeric".to_string(),
                        ));
                    }
                };
                let factor = 10f64.powi(prec as i32);
                let mode = match &args[2..] {
                    [CypherValue::String(s)] => {
                        let m = s.to_ascii_uppercase();
                        if !matches!(
                            m.as_str(),
                            "CEILING"
                                | "FLOOR"
                                | "UP"
                                | "DOWN"
                                | "HALF_UP"
                                | "HALF_DOWN"
                                | "HALF_EVEN"
                        ) {
                            return Err(CypherError::type_error(format!(
                                "Unknown rounding mode '{s}'. Valid values are: CEILING, \
                                 FLOOR, UP, DOWN, HALF_UP, HALF_DOWN, HALF_EVEN"
                            )));
                        }
                        m
                    }
                    [CypherValue::Null] => return Ok(CypherValue::Null),
                    [other] => {
                        return Err(CypherError::type_error(format!(
                            "round() mode must be a string, got {}",
                            other.type_name()
                        )));
                    }
                    _ => "HALF_UP".to_string(),
                };
                let scaled = x * factor;
                let rounded = match mode.as_str() {
                    "CEILING" => scaled.ceil(),
                    "FLOOR" => scaled.floor(),
                    "UP" => scaled.abs().ceil().copysign(scaled),
                    "DOWN" => scaled.trunc(),
                    "HALF_DOWN" => {
                        let t = scaled.trunc();
                        if (scaled - t).abs() > 0.5 {
                            t + (scaled - t).signum()
                        } else {
                            t
                        }
                    }
                    "HALF_EVEN" => {
                        // Rust's round_ties_even
                        let r = scaled.round();
                        if (scaled - scaled.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
                            r - (scaled).signum()
                        } else {
                            r
                        }
                    }
                    _ => scaled.abs().round().copysign(scaled), // HALF_UP
                };
                return Ok(CypherValue::Float(rounded / factor));
            }
            match &args[0] {
                CypherValue::Integer(i) => Ok(CypherValue::Integer(*i)),
                CypherValue::Float(f) => Ok(CypherValue::Float(f.round())),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "round() requires a numeric value".to_string(),
                )),
            }
        }
        "sign" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Integer(i) => Ok(CypherValue::Integer(i.signum())),
                CypherValue::Float(f) => Ok(CypherValue::Integer(if *f > 0.0 {
                    1
                } else if *f < 0.0 {
                    -1
                } else {
                    0
                })),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "sign() requires a numeric value".to_string(),
                )),
            }
        }
        "rand" => {
            check_arity(name, args, 0)?;
            // deterministic LCG seeded per call counter; not cryptographic
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEED: AtomicU64 = AtomicU64::new(42);
            let s = SEED.fetch_add(1, Ordering::Relaxed);
            // LCG step
            let val = ((s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407))
                >> 33) as f64
                / (1u64 << 31) as f64;
            Ok(CypherValue::Float(val))
        }
        // `vector.similarity.cosine(a, b)`: cosine of equal-length vectors normalised to [0, 1] as
        // `(1 + cos) / 2`. Like Neo4j, rejects length mismatches and non-numeric elements; NULL
        // propagates
        "vector.similarity.cosine" => {
            check_arity(name, args, 2)?;
            vector_similarity(name, &args[0], &args[1], SimilarityKind::Cosine)
        }
        // `vector.similarity.euclidean(a, b)`: `1 / (1 + sum(a_i - b_i)^2)`, 1.0 for identical vectors,
        // decaying with distance. Same NULL / type rules
        "vector.similarity.euclidean" => {
            check_arity(name, args, 2)?;
            vector_similarity(name, &args[0], &args[1], SimilarityKind::Euclidean)
        }

        // -- Cypher 25 VECTOR functions (Neo4j 2025.10) --------------------
        // A VECTOR is a numeric LIST here (as stored in `PropertyValue::Vector`, the
        // HNSW index and `vector.similarity.*`), so these work on number lists.
        "vector" => {
            if args.is_empty() || args.len() > 3 {
                return Err(CypherError::type_error(
                    "vector() expects (list [, dimensions [, type]])".to_string(),
                ));
            }
            match &args[0] {
                CypherValue::Null => Ok(CypherValue::Null),
                CypherValue::List(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for it in items {
                        match it {
                            CypherValue::Integer(i) => out.push(CypherValue::Float(*i as f64)),
                            CypherValue::Float(f) => out.push(CypherValue::Float(*f)),
                            other => {
                                return Err(CypherError::type_error(format!(
                                    "vector() elements must be numbers, got {}",
                                    other.type_name()
                                )));
                            }
                        }
                    }
                    if let Some(CypherValue::Integer(dim)) = args.get(1)
                        && *dim >= 0
                        && (*dim as usize) != out.len()
                    {
                        return Err(CypherError::type_error(format!(
                            "vector(): declared dimension {dim} != list length {}",
                            out.len()
                        )));
                    }
                    // args[2] (coordinate type, e.g. FLOAT32) is accepted and ignored; coordinates
                    // are stored uniformly
                    Ok(CypherValue::List(out))
                }
                other => Err(CypherError::type_error(format!(
                    "vector() requires a list, got {}",
                    other.type_name()
                ))),
            }
        }
        "vector_dimension_count" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Null => Ok(CypherValue::Null),
                CypherValue::List(items) => Ok(CypherValue::Integer(items.len() as i64)),
                other => Err(CypherError::type_error(format!(
                    "vector_dimension_count() requires a vector, got {}",
                    other.type_name()
                ))),
            }
        }
        "vector_norm" => {
            check_arity(name, args, 1)?;
            if args[0].is_null() {
                return Ok(CypherValue::Null);
            }
            let v = vector_to_f64(name, &args[0])?;
            Ok(CypherValue::Float(
                v.iter().map(|x| x * x).sum::<f64>().sqrt(),
            ))
        }
        "vector_distance" => {
            check_arity(name, args, 3)?;
            if args[0].is_null() || args[1].is_null() {
                return Ok(CypherValue::Null);
            }
            let a = vector_to_f64(name, &args[0])?;
            let b = vector_to_f64(name, &args[1])?;
            if a.len() != b.len() {
                return Err(CypherError::type_error(format!(
                    "vector_distance(): dimension mismatch ({} vs {})",
                    a.len(),
                    b.len()
                )));
            }
            let metric = match &args[2] {
                CypherValue::String(s) => s.to_uppercase(),
                _ => {
                    return Err(CypherError::type_error(
                        "vector_distance() metric must be a string".to_string(),
                    ));
                }
            };
            let dist = match metric.as_str() {
                "EUCLIDEAN" | "L2" => a
                    .iter()
                    .zip(&b)
                    .map(|(x, y)| (x - y) * (x - y))
                    .sum::<f64>()
                    .sqrt(),
                "COSINE" => {
                    let dot: f64 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
                    let na = a.iter().map(|x| x * x).sum::<f64>().sqrt();
                    let nb = b.iter().map(|x| x * x).sum::<f64>().sqrt();
                    if na == 0.0 || nb == 0.0 {
                        return Ok(CypherValue::Null);
                    }
                    1.0 - dot / (na * nb)
                }
                "DOT" | "DOT_PRODUCT" => -a.iter().zip(&b).map(|(x, y)| x * y).sum::<f64>(),
                other => {
                    return Err(CypherError::type_error(format!(
                        "vector_distance(): unknown metric '{other}' \
                         (expected EUCLIDEAN, COSINE, or DOT_PRODUCT)"
                    )));
                }
            };
            Ok(CypherValue::Float(dist))
        }
        // `file()` / `linenumber()` are LOAD CSV helpers that `eval_expr` intercepts first (they
        // need the frame's hidden CSV-context slot). Reaching here means a direct call_function
        // call without that context (e.g. a bytecode compile-time stub): NULL
        "file" | "linenumber" => {
            check_arity(name, args, 0)?;
            Ok(CypherValue::Null)
        }
        // `isNaN(x)`: only FLOAT can be NaN; INTEGER -> false, NULL -> NULL
        "isnan" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Null => Ok(CypherValue::Null),
                CypherValue::Float(f) => Ok(CypherValue::Bool(f.is_nan())),
                CypherValue::Integer(_) => Ok(CypherValue::Bool(false)),
                other => Err(CypherError::type_error(format!(
                    "isNaN() requires INTEGER or FLOAT, got {}",
                    other.type_name()
                ))),
            }
        }
        // `isEmpty(x)`: true for empty STRING / LIST / MAP; NULL passes through; other types
        // rejected
        "isempty" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Null => Ok(CypherValue::Null),
                CypherValue::String(s) => Ok(CypherValue::Bool(s.is_empty())),
                CypherValue::List(l) => Ok(CypherValue::Bool(l.is_empty())),
                CypherValue::Map(m) => Ok(CypherValue::Bool(m.is_empty())),
                other => Err(CypherError::type_error(format!(
                    "isEmpty() requires STRING / LIST / MAP, got {}",
                    other.type_name()
                ))),
            }
        }
        // `randomUUID()`: RFC 4122 v4 string (`xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx`, `y in
        // {8,9,a,b}`) from the same LCG as `rand()`, no extra dependency; 122 random bits make
        // collisions vanishingly unlikely. Function names arrive lowercased
        "randomuuid" => {
            check_arity(name, args, 0)?;
            use std::sync::atomic::{AtomicU64, Ordering};
            static UUID_SEED: AtomicU64 = AtomicU64::new(0xCAFEF00DBABE0042);
            // two u64s of noise from stirring the LCG twice, so consecutive calls share no
            // structure despite the monotonic seed
            let mix = |s: u64| {
                s.wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407)
                    ^ (s.rotate_left(17))
            };
            let s1 = UUID_SEED.fetch_add(1, Ordering::Relaxed);
            let s2 = UUID_SEED.fetch_add(1, Ordering::Relaxed);
            let mut hi = mix(s1);
            let mut lo = mix(s2.wrapping_add(0x9E37_79B9_7F4A_7C15));
            // version 4 in the top nibble of byte 6, variant 10xx in the top bits of byte 8
            // (MSB-first 8/4/4/4/12 hex)
            hi = (hi & 0xFFFF_FFFF_FFFF_0FFF) | 0x0000_0000_0000_4000;
            lo = (lo & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000;
            let bytes = [
                (hi >> 56) as u8,
                (hi >> 48) as u8,
                (hi >> 40) as u8,
                (hi >> 32) as u8,
                (hi >> 24) as u8,
                (hi >> 16) as u8,
                (hi >> 8) as u8,
                (hi) as u8,
                (lo >> 56) as u8,
                (lo >> 48) as u8,
                (lo >> 40) as u8,
                (lo >> 32) as u8,
                (lo >> 24) as u8,
                (lo >> 16) as u8,
                (lo >> 8) as u8,
                (lo) as u8,
            ];
            let s = format!(
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                bytes[0],
                bytes[1],
                bytes[2],
                bytes[3],
                bytes[4],
                bytes[5],
                bytes[6],
                bytes[7],
                bytes[8],
                bytes[9],
                bytes[10],
                bytes[11],
                bytes[12],
                bytes[13],
                bytes[14],
                bytes[15],
            );
            Ok(CypherValue::String(s))
        }

        "sqrt" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Integer(i) => Ok(CypherValue::Float((*i as f64).sqrt())),
                CypherValue::Float(f) => Ok(CypherValue::Float(f.sqrt())),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "sqrt() requires a numeric argument, got {}",
                    args[0].type_name()
                ))),
            }
        }
        // GQL section GF03 spells natural log `ln`; Cypher's `log` is natural log too, so they're synonyms
        "log" | "ln" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Integer(i) => Ok(CypherValue::Float((*i as f64).ln())),
                CypherValue::Float(f) => Ok(CypherValue::Float(f.ln())),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "log()/ln() requires a numeric argument, got {}",
                    args[0].type_name()
                ))),
            }
        }
        "log10" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Integer(i) => Ok(CypherValue::Float((*i as f64).log10())),
                CypherValue::Float(f) => Ok(CypherValue::Float(f.log10())),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "log10() requires a numeric argument, got {}",
                    args[0].type_name()
                ))),
            }
        }
        "exp" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Integer(i) => Ok(CypherValue::Float((*i as f64).exp())),
                CypherValue::Float(f) => Ok(CypherValue::Float(f.exp())),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "exp() requires a numeric argument, got {}",
                    args[0].type_name()
                ))),
            }
        }
        "e" => {
            check_arity(name, args, 0)?;
            Ok(CypherValue::Float(std::f64::consts::E))
        }
        "pi" => {
            check_arity(name, args, 0)?;
            Ok(CypherValue::Float(std::f64::consts::PI))
        }

        // -- Trigonometric functions (GQL section GF02) -----------------------
        // One numeric argument in radians (`atan2(y, x)` takes two; `degrees`/`radians`
        // convert units). Integers coerce to f64; non-numerics are type errors.
        "sin" => unary_trig(name, args, f64::sin),
        "cos" => unary_trig(name, args, f64::cos),
        "tan" => unary_trig(name, args, f64::tan),
        "asin" => unary_trig(name, args, f64::asin),
        "acos" => unary_trig(name, args, f64::acos),
        "atan" => unary_trig(name, args, f64::atan),
        "sinh" => unary_trig(name, args, f64::sinh),
        "cosh" => unary_trig(name, args, f64::cosh),
        "tanh" => unary_trig(name, args, f64::tanh),
        "cot" => unary_trig(name, args, |x| 1.0 / x.tan()),
        "degrees" => unary_trig(name, args, f64::to_degrees),
        "radians" => unary_trig(name, args, f64::to_radians),
        // haversin(x) = (1 - cos(x)) / 2, half the versine (haversine formula); result in [0, 1]
        "haversin" => unary_trig(name, args, |x| (1.0 - x.cos()) * 0.5),
        "atan2" => {
            check_arity(name, args, 2)?;
            let y = match &args[0] {
                CypherValue::Integer(i) => *i as f64,
                CypherValue::Float(f) => *f,
                CypherValue::Null => return Ok(CypherValue::Null),
                _ => {
                    return Err(CypherError::type_error(format!(
                        "atan2() requires numeric arguments, got {}",
                        args[0].type_name()
                    )));
                }
            };
            let x = match &args[1] {
                CypherValue::Integer(i) => *i as f64,
                CypherValue::Float(f) => *f,
                CypherValue::Null => return Ok(CypherValue::Null),
                _ => {
                    return Err(CypherError::type_error(format!(
                        "atan2() requires numeric arguments, got {}",
                        args[1].type_name()
                    )));
                }
            };
            Ok(CypherValue::Float(y.atan2(x)))
        }

        // -- Graph entity functions ------------------------------------
        "startnode" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Edge { source, .. } => Ok(CypherValue::Node {
                    id: *source,
                    labels: Vec::new(),
                    properties: BTreeMap::new(),
                }),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "startNode() requires a relationship, got {}",
                    args[0].type_name()
                ))),
            }
        }
        "endnode" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Edge { target, .. } => Ok(CypherValue::Node {
                    id: *target,
                    labels: Vec::new(),
                    properties: BTreeMap::new(),
                }),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "endNode() requires a relationship, got {}",
                    args[0].type_name()
                ))),
            }
        }

        // -- List functions --------------------------------------------
        // `path_length` is the Cypher 25 GQL alias (Neo4j 2026.02) of `length` on a path
        // (relationship count).
        "size" | "length" | "path_length" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::String(s) => Ok(CypherValue::Integer(s.len() as i64)),
                CypherValue::List(l) => Ok(CypherValue::Integer(l.len() as i64)),
                CypherValue::Bytes(b) => Ok(CypherValue::Integer(b.len() as i64)),
                CypherValue::Path { edges, .. } => Ok(CypherValue::Integer(edges.len() as i64)),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(format!(
                    "size()/length() requires a string, list, or path, got {}",
                    args[0].type_name()
                ))),
            }
        }
        "head" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::List(l) => Ok(l.first().cloned().unwrap_or(CypherValue::Null)),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "head() requires a list".to_string(),
                )),
            }
        }
        "tail" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::List(l) => {
                    if l.is_empty() {
                        Ok(CypherValue::List(Vec::new()))
                    } else {
                        Ok(CypherValue::List(l[1..].to_vec()))
                    }
                }
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "tail() requires a list".to_string(),
                )),
            }
        }
        "last" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::List(l) => Ok(l.last().cloned().unwrap_or(CypherValue::Null)),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "last() requires a list".to_string(),
                )),
            }
        }
        "range" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(CypherError::type_error(
                    "range() requires 2 or 3 arguments".to_string(),
                ));
            }
            let start = match &args[0] {
                CypherValue::Integer(i) => *i,
                _ => {
                    return Err(CypherError::type_error(
                        "range() requires integer arguments".to_string(),
                    ));
                }
            };
            let end = match &args[1] {
                CypherValue::Integer(i) => *i,
                _ => {
                    return Err(CypherError::type_error(
                        "range() requires integer arguments".to_string(),
                    ));
                }
            };
            let step = if args.len() == 3 {
                match &args[2] {
                    CypherValue::Integer(i) => *i,
                    _ => {
                        return Err(CypherError::type_error(
                            "range() requires integer arguments".to_string(),
                        ));
                    }
                }
            } else {
                1
            };
            if step == 0 {
                return Err(CypherError::type_error(
                    "range() step cannot be zero".to_string(),
                ));
            }
            // materialized list: charge the memory budget before allocating, so `range(1,
            // 10000000000)` fails with the memory-limit error instead of exhausting the heap
            let count = if (step > 0 && start <= end) || (step < 0 && start >= end) {
                ((end as i128 - start as i128) / step as i128) as u64 + 1
            } else {
                0
            };
            let bytes = (count as usize).saturating_mul(std::mem::size_of::<CypherValue>());
            crate::memory::charge(bytes)?;
            let mut result = Vec::with_capacity(count.min(1 << 20) as usize);
            let mut i = start;
            if step > 0 {
                while i <= end {
                    result.push(CypherValue::Integer(i));
                    i += step;
                }
            } else {
                while i >= end {
                    result.push(CypherValue::Integer(i));
                    i += step;
                }
            }
            Ok(CypherValue::List(result))
        }

        // -- Null functions --------------------------------------------
        "coalesce" => {
            if args.is_empty() {
                return Err(CypherError::type_error(
                    "coalesce() requires at least one argument".to_string(),
                ));
            }
            for a in args {
                if !a.is_null() {
                    return Ok(a.clone());
                }
            }
            Ok(CypherValue::Null)
        }
        "nullif" => {
            // ISO/IEC 39075 section 20.7 `NULLIF(a, b)`: NULL if a equals b, else a. Comparisons with NULL
            // are NULL, treated as not equal, so `nullif(a, NULL)` = a and `nullif(NULL, b)` = NULL
            // (the spec's truth table)
            check_arity(name, args, 2)?;
            if args[0].is_null() {
                return Ok(CypherValue::Null);
            }
            if args[1].is_null() {
                return Ok(args[0].clone());
            }
            if crate::executor::proc_values_equal(&args[0], &args[1]) {
                Ok(CypherValue::Null)
            } else {
                Ok(args[0].clone())
            }
        }

        // -- Path functions --------------------------------------------
        "nodes" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Path { nodes, .. } => Ok(CypherValue::List(nodes.clone())),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "nodes() requires a path".to_string(),
                )),
            }
        }
        "relationships" | "rels" => {
            check_arity(name, args, 1)?;
            match &args[0] {
                CypherValue::Path { edges, .. } => Ok(CypherValue::List(edges.clone())),
                CypherValue::Null => Ok(CypherValue::Null),
                _ => Err(CypherError::type_error(
                    "relationships() requires a path".to_string(),
                )),
            }
        }

        // -- Predicate functions ---------------------------------------
        "exists" => {
            check_arity(name, args, 1)?;
            Ok(CypherValue::Bool(!args[0].is_null()))
        }
        "property_exists" => {
            // GQL section G115 `PROPERTY_EXISTS(entity, propName)`: entity has `propName` bound to a
            // non-null value (same as `entity.prop IS NOT NULL`)
            check_arity(name, args, 2)?;
            let prop_name = match &args[1] {
                CypherValue::String(s) => s.clone(),
                CypherValue::Null => return Ok(CypherValue::Null),
                _ => {
                    return Err(CypherError::type_error(format!(
                        "property_exists() requires a string property name, got {}",
                        args[1].type_name()
                    )));
                }
            };
            let exists = match &args[0] {
                CypherValue::Node { properties, .. } | CypherValue::Edge { properties, .. } => {
                    properties
                        .get(&prop_name)
                        .map(|v| !matches!(v, traverse_core::graph::property::PropertyValue::Null))
                        .unwrap_or(false)
                }
                CypherValue::Map(m) => m.get(&prop_name).map(|v| !v.is_null()).unwrap_or(false),
                CypherValue::Null => return Ok(CypherValue::Null),
                _ => {
                    return Err(CypherError::type_error(format!(
                        "property_exists() requires a node, edge, or map, got {}",
                        args[0].type_name()
                    )));
                }
            };
            Ok(CypherValue::Bool(exists))
        }

        // -- Type checking functions -----------------------------------
        // `valueType`: the most precise ISO 39075 value type (BOOLEAN, INTEGER, FLOAT,
        // STRING, LIST<inner>, MAP, NODE, RELATIONSHIP, PATH, DATE, LOCAL TIME, ZONED
        // TIME, LOCAL DATETIME, ZONED DATETIME, DURATION, POINT), with " NOT NULL" for
        // any non-null value; `null` gives "NULL". `typename` is the older Cypher form
        // (no NOT NULL, no element type), kept as is for existing callers.
        "valuetype" => {
            check_arity(name, args, 1)?;
            Ok(CypherValue::String(format_value_type(&args[0])))
        }
        "typename" => {
            check_arity(name, args, 1)?;
            Ok(CypherValue::String(args[0].type_name().to_string()))
        }

        // -- Temporal constructors -----------------------------------------
        // GQL spells them `local_time`/`local_datetime`/`zoned_time`/`zoned_datetime`,
        // Cypher `localtime`/`localdatetime`/`time`/`datetime`; per Neo4j's GV39 / GV40
        // mapping `ZONED_TIME` == `time` and `ZONED_DATETIME` == `datetime`. Both resolve
        // to one impl. Cypher 25 `format(temporal, pattern)` (Neo4j 2025.09).
        "format" => temporal::func_format(args),
        "date" => temporal::func_date(args),
        "localtime" | "local_time" => temporal::func_localtime(args),
        "time" | "zoned_time" => temporal::func_time(args),
        "localdatetime" | "local_datetime" => temporal::func_localdatetime(args),
        "datetime" | "zoned_datetime" => temporal::func_datetime(args),
        "duration" => temporal::func_duration(args),
        // GQL section GV39 / section GV40 parameterless keywords: lexed as keywords (`RETURN CURRENT_DATE`
        // without parens) and also accepted here as zero-arg calls (`CURRENT_DATE()`)
        "current_date" => temporal::func_date(&[]),
        "current_time" => temporal::func_time(&[]),
        "current_timestamp" => temporal::func_datetime(&[]),
        "local_timestamp" => temporal::func_localdatetime(&[]),
        // `timestamp()`: epoch milliseconds (INTEGER) from the statement clock
        "timestamp" => temporal::func_timestamp(args),
        "session_user" => {
            // ISO/IEC 39075 section 20.3 `SESSION_USER`: who opened the session. The executor has no
            // session; the HTTP layer installs the API-key holder as a thread-local
            // (traverse-server's session module). Outside a session -> null, not an error, as Neo4j
            // does for driverless calls
            check_arity(name, args, 0)?;
            Ok(match crate::session::current_user() {
                Some(s) => CypherValue::String(s),
                None => CypherValue::Null,
            })
        }

        // -- Temporal namespaced functions ---------------------------------
        "date.truncate"
        | "localtime.truncate"
        | "time.truncate"
        | "localdatetime.truncate"
        | "datetime.truncate" => temporal::func_truncate(name, args),
        // `duration_between` is the Cypher 25 GQL alias (Neo4j 2026.02) of `duration.between`
        "duration.between" | "duration_between" => temporal::func_duration_between(args),
        "duration.inmonths" => temporal::func_duration_in_months(args),
        "duration.indays" => temporal::func_duration_in_days(args),
        "duration.inseconds" => temporal::func_duration_in_seconds(args),
        "datetime.fromepoch" => temporal::func_datetime_from_epoch(args),
        "datetime.fromepochmillis" => temporal::func_datetime_from_epoch_millis(args),
        // clock variants: `statement` and `transaction` both use the cached query clock (no
        // transaction scope in the executor yet); `realtime` samples wall-clock per call, as
        // Neo4j's `*.realtime()`
        "date.transaction" | "date.statement" => {
            temporal::func_date_now_with_clock(args, temporal::ClockMode::Statement)
        }
        "date.realtime" => temporal::func_date_now_with_clock(args, temporal::ClockMode::Realtime),
        "localtime.transaction" | "localtime.statement" => {
            temporal::func_localtime_now_with_clock(args, temporal::ClockMode::Statement)
        }
        "localtime.realtime" => {
            temporal::func_localtime_now_with_clock(args, temporal::ClockMode::Realtime)
        }
        "time.transaction" | "time.statement" => {
            temporal::func_time_now_with_clock(args, temporal::ClockMode::Statement)
        }
        "time.realtime" => temporal::func_time_now_with_clock(args, temporal::ClockMode::Realtime),
        "localdatetime.transaction" | "localdatetime.statement" => {
            temporal::func_localdatetime_now_with_clock(args, temporal::ClockMode::Statement)
        }
        "localdatetime.realtime" => {
            temporal::func_localdatetime_now_with_clock(args, temporal::ClockMode::Realtime)
        }
        "datetime.transaction" | "datetime.statement" => {
            temporal::func_datetime_now_with_clock(args, temporal::ClockMode::Statement)
        }
        "datetime.realtime" => {
            temporal::func_datetime_now_with_clock(args, temporal::ClockMode::Realtime)
        }

        _ => Err(CypherError::semantic(format!("unknown function: {name}"))
            .with_status(crate::status::GqlStatus::UnknownFunction)),
    }
}

/// Numeric vector (list of numbers) as `Vec<f64>`, for the Cypher 25 `vector_*` functions.
fn vector_to_f64(name: &str, v: &CypherValue) -> Result<Vec<f64>, CypherError> {
    match v {
        CypherValue::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    CypherValue::Integer(i) => out.push(*i as f64),
                    CypherValue::Float(f) => out.push(*f),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "{name}() requires a numeric vector, got element {}",
                            other.type_name()
                        )));
                    }
                }
            }
            Ok(out)
        }
        other => Err(CypherError::type_error(format!(
            "{name}() requires a vector, got {}",
            other.type_name()
        ))),
    }
}

/// "requires a list" type error for `coll.*`.
fn coll_type_err(name: &str, got: &CypherValue) -> CypherError {
    CypherError::type_error(format!("{name}() requires a list, got {}", got.type_name()))
}

fn check_arity(name: &str, args: &[CypherValue], expected: usize) -> Result<(), CypherError> {
    if args.len() != expected {
        Err(CypherError::type_error(format!(
            "{name}() expected {expected} arguments, got {}",
            args.len()
        )))
    } else {
        Ok(())
    }
}

// -- Vector similarity helpers ---------------------------------------

#[derive(Clone, Copy)]
enum SimilarityKind {
    Cosine,
    Euclidean,
}

/// Shared `vector.similarity.cosine` / `euclidean`: `LIST<Number>` on both sides; non-lists and
/// length mismatches rejected; NULL propagates.
fn vector_similarity(
    name: &str,
    a: &CypherValue,
    b: &CypherValue,
    kind: SimilarityKind,
) -> Result<CypherValue, CypherError> {
    let to_f64 = |v: &CypherValue| -> Option<f64> {
        match v {
            CypherValue::Integer(i) => Some(*i as f64),
            CypherValue::Float(f) => Some(*f),
            _ => None,
        }
    };
    let to_vec = |v: &CypherValue| -> Result<Option<Vec<f64>>, CypherError> {
        match v {
            CypherValue::Null => Ok(None),
            CypherValue::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match to_f64(item) {
                        Some(x) => out.push(x),
                        None => {
                            return Err(CypherError::type_error(format!(
                                "{name}() requires a LIST of numbers, got {}",
                                item.type_name()
                            )));
                        }
                    }
                }
                Ok(Some(out))
            }
            other => Err(CypherError::type_error(format!(
                "{name}() requires a LIST argument, got {}",
                other.type_name()
            ))),
        }
    };
    let va = match to_vec(a)? {
        None => return Ok(CypherValue::Null),
        Some(v) => v,
    };
    let vb = match to_vec(b)? {
        None => return Ok(CypherValue::Null),
        Some(v) => v,
    };
    if va.len() != vb.len() {
        return Err(CypherError::type_error(format!(
            "{name}() requires equal-length vectors; got {} and {}",
            va.len(),
            vb.len()
        )));
    }
    let result = match kind {
        SimilarityKind::Cosine => {
            let mut dot = 0.0_f64;
            let mut na = 0.0_f64;
            let mut nb = 0.0_f64;
            for (x, y) in va.iter().zip(vb.iter()) {
                dot += x * y;
                na += x * x;
                nb += y * y;
            }
            let denom = (na * nb).sqrt();
            if denom == 0.0 {
                // zero vector on either side: Neo4j returns 0.5 (midpoint of cos in [-1,1]
                // normalised to [0,1])
                0.5
            } else {
                (1.0 + dot / denom) / 2.0
            }
        }
        SimilarityKind::Euclidean => {
            // Neo4j: `1 / (1 + sum(a_i-b_i)^2)`, the squared distance: identical -> 1.0, unit step -> 0.5,
            // decays quadratically
            let mut sq = 0.0_f64;
            for (x, y) in va.iter().zip(vb.iter()) {
                let d = x - y;
                sq += d * d;
            }
            1.0 / (1.0 + sq)
        }
    };
    Ok(CypherValue::Float(result))
}

// -- valueType formatter ---------------------------------------------

/// GQL/Cypher base type name of a value (`INTEGER`, `POINT`, `LIST`, ...), shared by `valueType()`
/// and the `IS :: TYPE` predicate.
pub(crate) fn value_base_type(v: &CypherValue) -> &'static str {
    match v {
        CypherValue::Bytes(_) => "BYTEARRAY",
        CypherValue::Bool(_) => "BOOLEAN",
        CypherValue::Integer(_) => "INTEGER",
        CypherValue::Float(_) => "FLOAT",
        CypherValue::String(_) => "STRING",
        CypherValue::List(_) => "LIST",
        CypherValue::Map(m) if is_point_map(m) => "POINT",
        CypherValue::Map(_) => "MAP",
        CypherValue::Node { .. } | CypherValue::NodeRef(_) => "NODE",
        CypherValue::Edge { .. } | CypherValue::EdgeRef { .. } => "RELATIONSHIP",
        CypherValue::Path { .. } => "PATH",
        CypherValue::Date(_) => "DATE",
        CypherValue::LocalTime(_) => "LOCAL TIME",
        CypherValue::Time(_) => "ZONED TIME",
        CypherValue::LocalDateTime(_) => "LOCAL DATETIME",
        CypherValue::DateTime(_) => "ZONED DATETIME",
        CypherValue::Duration(_) => "DURATION",
        CypherValue::Null => "NULL",
    }
}

/// Neo4j 5 value-type string for `valueType()`:
///   42            -> "INTEGER NOT NULL"
///   null          -> "NULL"
///   [1, 2]        -> "LIST<INTEGER NOT NULL> NOT NULL"
///   datetime(...) -> "ZONED DATETIME NOT NULL"
///   point({x,y})  -> "POINT NOT NULL"
///
/// Temporal types use ISO spelling with spaces ("LOCAL DATETIME"). Point maps
/// (`{x, y, srid, crs}`) report `POINT`, not `MAP`. `LIST<...>` looks at the
/// first non-null element; empty or all-null lists give
/// `LIST<NOTHING> NOT NULL`, as in Neo4j.
fn format_value_type(v: &CypherValue) -> String {
    if matches!(v, CypherValue::Null) {
        return "NULL".to_string();
    }
    let base_str = value_base_type(v);
    if let CypherValue::List(items) = v {
        let inner = items
            .iter()
            .find(|e| !matches!(e, CypherValue::Null))
            .map(format_value_type)
            .unwrap_or_else(|| "NOTHING".to_string());
        return format!("LIST<{inner}> NOT NULL");
    }
    format!("{base_str} NOT NULL")
}

/// Has the `{x, y, srid, crs}` shape `point()` builds (so `valueType()` says `POINT NOT NULL`).
fn is_point_map(m: &std::collections::BTreeMap<String, CypherValue>) -> bool {
    m.contains_key("x") && m.contains_key("y") && m.contains_key("srid") && m.contains_key("crs")
}

// -- Spatial helpers -------------------------------------------------

const SRID_CARTESIAN: i64 = 7203;
const SRID_CARTESIAN_3D: i64 = 9157;
const SRID_WGS84: i64 = 4326;
const SRID_WGS84_3D: i64 = 4979;

/// Coordinate from a map entry as f64 (integers or floats), for `point()` arguments.
fn coord(m: &std::collections::BTreeMap<String, CypherValue>, k: &str) -> Option<f64> {
    match m.get(k)? {
        CypherValue::Integer(i) => Some(*i as f64),
        CypherValue::Float(f) => Some(*f),
        _ => None,
    }
}

/// 2D / 3D point Map from a constructor map: `{x, y, [z], crs}`, `{longitude, latitude, [height],
/// crs}`, or coordinates alone with `crs` inferred (longitude/latitude -> WGS-84).
fn point_from_map(arg: &CypherValue) -> Result<CypherValue, CypherError> {
    match arg {
        CypherValue::Null => Ok(CypherValue::Null),
        CypherValue::Map(m) => {
            // coordinates: x/y/z or longitude/latitude/height
            let (x, y, z, is_geo);
            if let (Some(lon), Some(lat)) = (coord(m, "longitude"), coord(m, "latitude")) {
                x = lon;
                y = lat;
                z = coord(m, "height");
                is_geo = true;
            } else if let (Some(xv), Some(yv)) = (coord(m, "x"), coord(m, "y")) {
                x = xv;
                y = yv;
                z = coord(m, "z");
                // CRS hint from the keys, else cartesian
                is_geo = matches!(
                    m.get("crs"),
                    Some(CypherValue::String(s))
                        if s.eq_ignore_ascii_case("wgs-84") || s.eq_ignore_ascii_case("wgs-84-3d")
                );
            } else {
                return Err(CypherError::type_error(
                    "point() requires a map with x/y or longitude/latitude".to_string(),
                ));
            }

            let (srid, crs) = match (is_geo, z.is_some()) {
                (true, false) => (SRID_WGS84, "wgs-84"),
                (true, true) => (SRID_WGS84_3D, "wgs-84-3d"),
                (false, false) => (SRID_CARTESIAN, "cartesian"),
                (false, true) => (SRID_CARTESIAN_3D, "cartesian-3d"),
            };

            let mut out = std::collections::BTreeMap::new();
            out.insert("x".into(), CypherValue::Float(x));
            out.insert("y".into(), CypherValue::Float(y));
            if let Some(zv) = z {
                out.insert("z".into(), CypherValue::Float(zv));
            }
            out.insert("srid".into(), CypherValue::Integer(srid));
            out.insert("crs".into(), CypherValue::String(crs.into()));
            Ok(CypherValue::Map(out))
        }
        other => Err(CypherError::type_error(format!(
            "point() requires a MAP argument, got {}",
            other.type_name()
        ))),
    }
}

/// `(x, y, z?, srid)` of a point value (from `point()` or storage).
fn unpack_point(v: &CypherValue) -> Option<(f64, f64, Option<f64>, i64)> {
    let m = match v {
        CypherValue::Map(m) => m,
        _ => return None,
    };
    let x = coord(m, "x")?;
    let y = coord(m, "y")?;
    let z = coord(m, "z");
    let srid = match m.get("srid") {
        Some(CypherValue::Integer(s)) => *s,
        _ => {
            // no srid: infer from the keys (a raw {x:1,y:2} map that skipped point())
            if z.is_some() {
                SRID_CARTESIAN_3D
            } else {
                SRID_CARTESIAN
            }
        }
    };
    Some((x, y, z, srid))
}

/// Distance between points: haversine metres for WGS-84 pairs, Euclidean otherwise. Mismatched CRS
/// is a type error (Neo4j).
fn point_distance(a: &CypherValue, b: &CypherValue) -> Result<CypherValue, CypherError> {
    if matches!(a, CypherValue::Null) || matches!(b, CypherValue::Null) {
        return Ok(CypherValue::Null);
    }
    let (ax, ay, az, asrid) = unpack_point(a).ok_or_else(|| {
        CypherError::type_error("point.distance() requires point arguments".to_string())
    })?;
    let (bx, by, bz, bsrid) = unpack_point(b).ok_or_else(|| {
        CypherError::type_error("point.distance() requires point arguments".to_string())
    })?;
    if asrid != bsrid {
        return Err(CypherError::type_error(format!(
            "point.distance() requires both points to use the same CRS (got SRIDs {asrid} and {bsrid})"
        )));
    }
    let d = match asrid {
        SRID_WGS84 | SRID_WGS84_3D => {
            // haversine on Neo4j's sphere, the WGS-84 equatorial radius 6_378_140 m (~111320 m per
            // degree of longitude at the equator, not the 6_371_000 mean radius); 3D adds the
            // height difference in quadrature
            let r = 6_378_140.0_f64;
            let to_rad = std::f64::consts::PI / 180.0;
            let (lat1, lat2) = (ay * to_rad, by * to_rad);
            let (dlat, dlon) = ((by - ay) * to_rad, (bx - ax) * to_rad);
            let h =
                (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
            let great_circle = 2.0 * r * h.sqrt().asin();
            if asrid == SRID_WGS84_3D {
                let dz = bz.unwrap_or(0.0) - az.unwrap_or(0.0);
                (great_circle * great_circle + dz * dz).sqrt()
            } else {
                great_circle
            }
        }
        _ => {
            // cartesian (unknown SRIDs too)
            let dx = bx - ax;
            let dy = by - ay;
            let dz = bz.unwrap_or(0.0) - az.unwrap_or(0.0);
            (dx * dx + dy * dy + dz * dz).sqrt()
        }
    };
    Ok(CypherValue::Float(d))
}

/// Bounding-box test. As in Neo4j all three arguments are points of one CRS; component-wise `ll[i]
/// <= p[i] <= ur[i]` for x, y and z (if present).
fn point_within_bbox(
    p: &CypherValue,
    ll: &CypherValue,
    ur: &CypherValue,
) -> Result<CypherValue, CypherError> {
    if matches!(p, CypherValue::Null)
        || matches!(ll, CypherValue::Null)
        || matches!(ur, CypherValue::Null)
    {
        return Ok(CypherValue::Null);
    }
    let (px, py, pz, ps) = unpack_point(p).ok_or_else(|| {
        CypherError::type_error("point.withinBBox() requires point arguments".to_string())
    })?;
    let (lx, ly, lz, ls) = unpack_point(ll).ok_or_else(|| {
        CypherError::type_error("point.withinBBox() requires point arguments".to_string())
    })?;
    let (ux, uy, uz, us) = unpack_point(ur).ok_or_else(|| {
        CypherError::type_error("point.withinBBox() requires point arguments".to_string())
    })?;
    if ps != ls || ls != us {
        return Err(CypherError::type_error(
            "point.withinBBox() requires all points to share a CRS".to_string(),
        ));
    }
    let xy_ok = px >= lx && px <= ux && py >= ly && py <= uy;
    let z_ok = match (pz, lz, uz) {
        (Some(pz), Some(lz), Some(uz)) => pz >= lz && pz <= uz,
        (None, None, None) => true,
        _ => false, // mixed dimensionality
    };
    Ok(CypherValue::Bool(xy_ok && z_ok))
}

/// For the `to{Boolean,Float,Integer,String}List` family: maps `convert` over a `LIST<ANY>` (length
/// and order kept, failures become `NULL`), passes `NULL` through, rejects non-lists.
fn convert_list_elementwise(
    name: &str,
    arg: &CypherValue,
    convert: impl Fn(&CypherValue) -> CypherValue,
) -> Result<CypherValue, CypherError> {
    match arg {
        CypherValue::Null => Ok(CypherValue::Null),
        CypherValue::List(items) => {
            let out: Vec<CypherValue> = items.iter().map(&convert).collect();
            Ok(CypherValue::List(out))
        }
        other => Err(CypherError::type_error(format!(
            "{name}() requires a LIST, got {}",
            other.type_name()
        ))),
    }
}

/// Which end(s) a trim strips.
#[derive(Clone, Copy)]
enum Trim {
    Leading,
    Trailing,
    Both,
}

fn trim_with(s: &str, end: Trim, strip: impl Fn(char) -> bool) -> String {
    match end {
        Trim::Leading => s.trim_start_matches(strip),
        Trim::Trailing => s.trim_end_matches(strip),
        Trim::Both => s.trim_matches(strip),
    }
    .to_string()
}

/// `trim` / `btrim` / `ltrim` / `rtrim(original [, characters])`: strips whitespace, or every
/// character that occurs in `characters`. A null argument gives null.
fn trim_fn(name: &str, args: &[CypherValue], end: Trim) -> Result<CypherValue, CypherError> {
    match args {
        [s] => string_fn(s, |s| trim_with(s, end, char::is_whitespace)),
        [_, CypherValue::Null] => Ok(CypherValue::Null),
        [s, CypherValue::String(chars)] => {
            string_fn(s, |s| trim_with(s, end, |c| chars.contains(c)))
        }
        [_, other] => Err(CypherError::type_error(format!(
            "{name}() characters must be a string, got {}",
            other.type_name()
        ))),
        _ => Err(CypherError::type_error(format!(
            "{name}() expected 1 or 2 arguments, got {}",
            args.len()
        ))),
    }
}

/// GQL section 20.24 `TRIM([LEADING | TRAILING | BOTH] [c] FROM s)`, lowered to `gql_trim(s [, c [,
/// end]])`: strips the single character `c` (U+0020 by default) from the given end(s). A trim
/// character that isn't exactly one character is a data exception (22027).
fn gql_trim(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.is_empty() || args.len() > 3 {
        return Err(CypherError::type_error(format!(
            "gql_trim() expected 1 to 3 arguments, got {}",
            args.len()
        )));
    }
    if args.iter().any(|a| matches!(a, CypherValue::Null)) {
        return Ok(CypherValue::Null);
    }
    let c = match args.get(1) {
        None => ' ',
        Some(CypherValue::String(c)) => {
            let mut chars = c.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => c,
                _ => {
                    return Err(CypherError::runtime(format!(
                        "trim error (22027): the trim character must be exactly one character, \
                         got '{c}'"
                    )));
                }
            }
        }
        Some(other) => {
            return Err(CypherError::type_error(format!(
                "TRIM character must be a string, got {}",
                other.type_name()
            )));
        }
    };
    let end = match args.get(2) {
        None => Trim::Both,
        Some(CypherValue::String(e)) => match e.as_str() {
            "leading" => Trim::Leading,
            "trailing" => Trim::Trailing,
            _ => Trim::Both,
        },
        Some(other) => {
            return Err(CypherError::type_error(format!(
                "TRIM end must be a string, got {}",
                other.type_name()
            )));
        }
    };
    string_fn(&args[0], |s| trim_with(s, end, |x| x == c))
}

fn string_fn(val: &CypherValue, f: impl Fn(&str) -> String) -> Result<CypherValue, CypherError> {
    match val {
        CypherValue::String(s) => Ok(CypherValue::String(f(s))),
        CypherValue::Null => Ok(CypherValue::Null),
        _ => Err(CypherError::type_error(format!(
            "expected string, got {}",
            val.type_name()
        ))),
    }
}

/// Normalises `s` to an ISO/IEC 39075 Unicode form (`NFC` / `NFD` / `NFKC` / `NFKD`, any case);
/// unknown forms error.
fn normalize_string(s: &str, form: &str) -> Result<String, CypherError> {
    match form.to_ascii_uppercase().as_str() {
        "NFC" => Ok(s.nfc().collect()),
        "NFD" => Ok(s.nfd().collect()),
        "NFKC" => Ok(s.nfkc().collect()),
        "NFKD" => Ok(s.nfkd().collect()),
        other => Err(CypherError::type_error(format!(
            "unknown normalization form `{other}`; expected NFC, NFD, NFKC, or NFKD"
        ))),
    }
}

/// Single-argument trig body: integers coerce to f64, non-numerics are type errors, null
/// propagates; `f` is an [`f64`] trig method (or a closure on one, for `cot`).
fn unary_trig(
    name: &str,
    args: &[CypherValue],
    f: impl Fn(f64) -> f64,
) -> Result<CypherValue, CypherError> {
    check_arity(name, args, 1)?;
    match &args[0] {
        CypherValue::Integer(i) => Ok(CypherValue::Float(f(*i as f64))),
        CypherValue::Float(x) => Ok(CypherValue::Float(f(*x))),
        CypherValue::Null => Ok(CypherValue::Null),
        _ => Err(CypherError::type_error(format!(
            "{name}() requires a numeric argument, got {}",
            args[0].type_name()
        ))),
    }
}

#[cfg(test)]
mod gql_function_tests {
    //! Tests for the ISO/IEC 39075 function additions: GQL spellings and semantics (Cypher-only
    //! functions are covered elsewhere).

    use super::*;

    fn s(v: &str) -> CypherValue {
        CypherValue::String(v.to_string())
    }
    fn i(v: i64) -> CypherValue {
        CypherValue::Integer(v)
    }
    fn f(v: f64) -> CypherValue {
        CypherValue::Float(v)
    }

    #[test]
    fn upper_lower_aliases_resolve() {
        assert_eq!(call_function("upper", &[s("abc")]).unwrap(), s("ABC"));
        assert_eq!(call_function("lower", &[s("ABC")]).unwrap(), s("abc"));
        // Cypher spellings still work
        assert_eq!(call_function("toupper", &[s("abc")]).unwrap(), s("ABC"));
        assert_eq!(call_function("tolower", &[s("ABC")]).unwrap(), s("abc"));
    }

    #[test]
    fn gql_trim_strips_only_ascii_space() {
        // GQL section 20.24: trim() removes only U+0020, NOT tabs / newlines / NBSP
        assert_eq!(call_function("gql_trim", &[s("  hi  ")]).unwrap(), s("hi"));
        // tabs and newlines survive on the GQL path
        assert_eq!(
            call_function("gql_trim", &[s("\t hi \t")]).unwrap(),
            s("\t hi \t"),
        );
        // Cypher `trim` strips any whitespace
        assert_eq!(call_function("trim", &[s("\t hi \t")]).unwrap(), s("hi"));
    }

    #[test]
    fn trims_take_a_set_of_characters() {
        assert_eq!(
            call_function("btrim", &[s("xyhixy"), s("yx")]).unwrap(),
            s("hi")
        );
        assert_eq!(
            call_function("ltrim", &[s("xyhixy"), s("yx")]).unwrap(),
            s("hixy")
        );
        assert_eq!(
            call_function("rtrim", &[s("xyhixy"), s("yx")]).unwrap(),
            s("xyhi")
        );
        // no set: whitespace
        assert_eq!(call_function("ltrim", &[s("\t hi ")]).unwrap(), s("hi "));
        assert_eq!(
            call_function("btrim", &[s("hi"), CypherValue::Null]).unwrap(),
            CypherValue::Null
        );
        assert!(call_function("btrim", &[s("hi"), CypherValue::Integer(1)]).is_err());
    }

    #[test]
    fn gql_trim_takes_one_character_and_an_end() {
        let t = |args: &[CypherValue]| call_function("gql_trim", args);
        assert_eq!(t(&[s("xxhixx"), s("x")]).unwrap(), s("hi"));
        assert_eq!(t(&[s("xxhixx"), s("x"), s("leading")]).unwrap(), s("hixx"));
        assert_eq!(t(&[s("xxhixx"), s("x"), s("trailing")]).unwrap(), s("xxhi"));
        assert_eq!(t(&[s("  hi  "), s(" "), s("both")]).unwrap(), s("hi"));
        // ISO: exactly one trim character, else 22027
        let err = t(&[s("xxhixx"), s("xy")]).unwrap_err();
        assert!(err.to_string().contains("22027"), "{err}");
        assert!(t(&[s("hi"), s("")]).is_err());
        assert_eq!(t(&[CypherValue::Null, s("x")]).unwrap(), CypherValue::Null);
    }

    #[test]
    fn char_length_counts_codepoints_not_bytes() {
        // `size()` is byte length for strings (Cypher); `char_length()` / `character_length()`
        // count code points (GQL section 20.22)
        let multibyte = s("café"); // 5 bytes, 4 codepoints
        assert_eq!(
            call_function("char_length", std::slice::from_ref(&multibyte)).unwrap(),
            i(4),
        );
        assert_eq!(
            call_function("character_length", std::slice::from_ref(&multibyte)).unwrap(),
            i(4),
        );
        assert_eq!(call_function("size", &[multibyte]).unwrap(), i(5));
    }

    #[test]
    fn ceiling_and_ln_are_aliases() {
        assert_eq!(call_function("ceiling", &[f(1.2)]).unwrap(), f(2.0));
        assert_eq!(call_function("ceil", &[f(1.2)]).unwrap(), f(2.0));
        // `ln(e)` == 1.
        let e_val = call_function("e", &[]).unwrap();
        match call_function("ln", &[e_val]).unwrap() {
            CypherValue::Float(v) => assert!((v - 1.0).abs() < 1e-12),
            other => panic!("expected float, got {other:?}"),
        }
    }

    #[test]
    fn mod_function_matches_percent_operator() {
        assert_eq!(call_function("mod", &[i(7), i(3)]).unwrap(), i(1));
        assert_eq!(call_function("mod", &[i(-7), i(3)]).unwrap(), i(-1));
        assert!(call_function("mod", &[i(1), i(0)]).is_err());
    }

    #[test]
    fn power_function_returns_float() {
        match call_function("power", &[i(2), i(10)]).unwrap() {
            CypherValue::Float(v) => assert!((v - 1024.0).abs() < 1e-9),
            other => panic!("expected float, got {other:?}"),
        }
        match call_function("power", &[f(2.0), f(0.5)]).unwrap() {
            CypherValue::Float(v) => {
                assert!((v - std::f64::consts::SQRT_2).abs() < 1e-12)
            }
            other => panic!("expected float, got {other:?}"),
        }
    }

    #[test]
    fn trig_functions_register() {
        for (name, x) in [
            ("sin", 0.0_f64),
            ("cos", 0.0),
            ("tan", 0.0),
            ("asin", 0.0),
            ("acos", 1.0),
            ("atan", 0.0),
            ("sinh", 0.0),
            ("cosh", 0.0),
            ("tanh", 0.0),
            ("cot", std::f64::consts::FRAC_PI_4),
            ("degrees", std::f64::consts::PI),
            ("radians", 180.0),
        ] {
            let r = call_function(name, &[f(x)]).unwrap();
            assert!(
                matches!(r, CypherValue::Float(_)),
                "{name} must return Float"
            );
        }
        // atan2 takes two args.
        match call_function("atan2", &[f(0.0), f(1.0)]).unwrap() {
            CypherValue::Float(v) => assert!((v - 0.0).abs() < 1e-12),
            other => panic!("expected float, got {other:?}"),
        }
    }

    #[test]
    fn temporal_aliases_resolve() {
        // smoke: zero-arg GQL spellings reach the same impl as Cypher's now-functions
        assert!(call_function("local_time", &[]).is_ok());
        assert!(call_function("local_datetime", &[]).is_ok());
        assert!(call_function("zoned_time", &[]).is_ok());
        assert!(call_function("zoned_datetime", &[]).is_ok());
    }

    #[test]
    fn property_exists_on_map() {
        use std::collections::BTreeMap;
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), i(1));
        m.insert("b".to_string(), CypherValue::Null);
        let map = CypherValue::Map(m);

        assert_eq!(
            call_function("property_exists", &[map.clone(), s("a")]).unwrap(),
            CypherValue::Bool(true),
        );
        // null property -> doesn't exist (like `IS NOT NULL`)
        assert_eq!(
            call_function("property_exists", &[map.clone(), s("b")]).unwrap(),
            CypherValue::Bool(false),
        );
        // missing key -> doesn't exist
        assert_eq!(
            call_function("property_exists", &[map, s("c")]).unwrap(),
            CypherValue::Bool(false),
        );
    }
}
