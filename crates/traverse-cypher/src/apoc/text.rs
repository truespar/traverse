//! `apoc.text.*`: string functions: similarity, phonetics, regex, case and formatting.

use std::collections::BTreeMap;

use base64::Engine;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::{cheap_rand, int_arg, num_of, str_arg, with_list, with_str};

/// The pure functions of this module; `None` for other names.
pub(super) fn eval(name: &str, args: &[CypherValue]) -> Option<Result<CypherValue, CypherError>> {
    use CypherValue as V;
    let r = match name {
        "apoc.text.join" => text_join(args),
        "apoc.text.clean" => with_str(args, |s| V::String(clean_text(s))),
        "apoc.text.comparecleaned" => text_compare_cleaned(args),
        "apoc.text.distance" | "apoc.text.levenshteindistance" => text_levenshtein(args),
        "apoc.text.levenshteinsimilarity" => text_levenshtein_similarity(args),
        "apoc.text.hammingdistance" => text_hamming(args),
        "apoc.text.jarowinklerdistance" => text_jaro_winkler(args),
        "apoc.text.sorensendicesimilarity" => text_sorensen_dice(args),
        "apoc.text.fuzzymatch" => text_fuzzy_match(args),
        "apoc.text.indexof" => text_index_of(args),
        "apoc.text.indexesof" => text_indexes_of(args),
        "apoc.text.split" => text_split(args),
        "apoc.text.replace" | "apoc.text.regreplace" => text_regreplace(args),
        "apoc.text.regexgroups" => text_regex_groups(args),
        "apoc.text.capitalize" => with_str(args, |s| V::String(capitalize(s))),
        "apoc.text.capitalizeall" => with_str(args, |s| {
            V::String(s.split(' ').map(capitalize).collect::<Vec<_>>().join(" "))
        }),
        "apoc.text.decapitalize" => with_str(args, |s| V::String(decapitalize(s))),
        "apoc.text.decapitalizeall" => with_str(args, |s| {
            V::String(s.split(' ').map(decapitalize).collect::<Vec<_>>().join(" "))
        }),
        "apoc.text.swapcase" => with_str(args, |s| {
            V::String(
                s.chars()
                    .map(|c| {
                        if c.is_uppercase() {
                            c.to_lowercase().next().unwrap_or(c)
                        } else {
                            c.to_uppercase().next().unwrap_or(c)
                        }
                    })
                    .collect(),
            )
        }),
        "apoc.text.camelcase" => with_str(args, |s| V::String(camel_case(s, false))),
        "apoc.text.uppercamelcase" => with_str(args, |s| V::String(camel_case(s, true))),
        "apoc.text.snakecase" => with_str(args, |s| V::String(snake_case(s))),
        "apoc.text.lpad" => text_pad(args, true),
        "apoc.text.rpad" => text_pad(args, false),
        "apoc.text.charat" => text_char_at(args),
        "apoc.text.code" => text_code(args),
        "apoc.text.hexvalue" => text_hex_value(args),
        "apoc.text.slug" => text_slug(args),
        "apoc.text.repeat" => text_repeat(args),
        "apoc.text.format" => text_format(args),
        "apoc.text.base64encode" => with_str(args, |s| {
            V::String(base64::engine::general_purpose::STANDARD.encode(s.as_bytes()))
        }),
        "apoc.text.base64decode" => text_base64_decode(args),
        "apoc.text.urlencode" => with_str(args, |s| V::String(url_encode(s))),
        "apoc.text.urldecode" => text_url_decode(args),
        // APOC's toUpperCase is UPPER_SNAKE_CASE (word split + underscores).
        "apoc.text.touppercase" => with_str(args, |s| {
            V::String(snake_case(s).replace('-', "_").to_uppercase())
        }),
        "apoc.text.base64urlencode" => with_str(args, |s| {
            V::String(base64::engine::general_purpose::URL_SAFE.encode(s.as_bytes()))
        }),
        "apoc.text.base64urldecode" => match args.first() {
            Some(V::String(s)) => {
                match base64::engine::general_purpose::URL_SAFE.decode(s.as_bytes()) {
                    Ok(b) => Ok(V::String(String::from_utf8_lossy(&b).to_string())),
                    Err(e) => Err(CypherError::semantic(format!("invalid base64: {e}"))),
                }
            }
            _ => Ok(V::Null),
        },
        "apoc.text.bytecount" => with_str(args, |s| V::Integer(s.len() as i64)),
        "apoc.text.bytes" => with_str(args, |s| {
            V::List(s.bytes().map(|b| V::Integer(b as i64)).collect())
        }),
        "apoc.text.hexcharat" => {
            let idx = int_arg(args, 1).unwrap_or(0).max(0) as usize;
            with_str(args, |s| match s.chars().nth(idx) {
                Some(c) => V::String(format!("{:04X}", c as u32)),
                None => V::Null,
            })
        }
        "apoc.text.random" => {
            let len = int_arg(args, 0).unwrap_or(0).max(0) as usize;
            let valid = str_arg(args, 1).unwrap_or_else(|| "A-Za-z0-9".to_string());
            Ok(V::String(text_random(len, &valid)))
        }
        "apoc.text.regexgroupsbyname" => text_regex_groups_by_name(args),
        "apoc.text.tocypher" => Ok(V::String(to_cypher_literal(
            args.first().unwrap_or(&V::Null),
        ))),
        "apoc.text.phonetic" => with_str(args, |s| V::String(metaphone(s, 12))),
        "apoc.text.doublemetaphone" => with_str(args, |s| V::String(metaphone(s, 12))),
        _ => return None,
    };
    Some(r)
}

fn text_join(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let sep = str_arg(args, 1).unwrap_or_default();
    with_list(args, |l| {
        CypherValue::String(
            l.iter()
                .map(|v| match v {
                    CypherValue::String(s) => s.clone(),
                    CypherValue::Null => "null".to_string(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(&sep),
        )
    })
}

fn clean_text(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn text_compare_cleaned(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::String(a)), Some(CypherValue::String(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    Ok(CypherValue::Bool(clean_text(a) == clean_text(b)))
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn text_levenshtein(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::String(a)), Some(CypherValue::String(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    Ok(CypherValue::Integer(levenshtein(a, b) as i64))
}

fn text_levenshtein_similarity(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::String(a)), Some(CypherValue::String(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let max_len = a.chars().count().max(b.chars().count());
    if max_len == 0 {
        return Ok(CypherValue::Float(1.0));
    }
    Ok(CypherValue::Float(
        1.0 - levenshtein(a, b) as f64 / max_len as f64,
    ))
}

fn text_hamming(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::String(a)), Some(CypherValue::String(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let common = ac.len().min(bc.len());
    let mut d = ac.len().abs_diff(bc.len());
    for i in 0..common {
        if ac[i] != bc[i] {
            d += 1;
        }
    }
    Ok(CypherValue::Integer(d as i64))
}

fn jaro(a: &[char], b: &[char]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut b_used = vec![false; b.len()];
    let mut matches = 0usize;
    let mut a_matched = Vec::new();
    for (i, ca) in a.iter().enumerate() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(b.len());
        for j in lo..hi {
            if !b_used[j] && b[j] == *ca {
                b_used[j] = true;
                a_matched.push((i, j));
                matches += 1;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let mut transpositions = 0usize;
    let b_seq: Vec<char> = b_used
        .iter()
        .enumerate()
        .filter(|(_, u)| **u)
        .map(|(j, _)| b[j])
        .collect();
    for (k, (i, _)) in a_matched.iter().enumerate() {
        if a[*i] != b_seq[k] {
            transpositions += 1;
        }
    }
    let m = matches as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64 / 2.0) / m) / 3.0
}

fn text_jaro_winkler(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::String(a)), Some(CypherValue::String(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let j = jaro(&ac, &bc);
    let prefix = ac
        .iter()
        .zip(bc.iter())
        .take(4)
        .take_while(|(x, y)| x == y)
        .count();
    // APOC's jaroWinklerDistance is a DISTANCE: 1 - similarity
    Ok(CypherValue::Float(
        1.0 - (j + prefix as f64 * 0.1 * (1.0 - j)),
    ))
}

fn text_sorensen_dice(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::String(a)), Some(CypherValue::String(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let bigrams = |s: &str| -> Vec<(char, char)> {
        let cs: Vec<char> = s.to_lowercase().chars().collect();
        cs.windows(2).map(|w| (w[0], w[1])).collect()
    };
    let ba = bigrams(a);
    let bb = bigrams(b);
    if ba.is_empty() && bb.is_empty() {
        return Ok(CypherValue::Float(1.0));
    }
    let mut used = vec![false; bb.len()];
    let mut inter = 0usize;
    for g in &ba {
        if let Some(j) = bb.iter().enumerate().position(|(j, h)| !used[j] && h == g) {
            used[j] = true;
            inter += 1;
        }
    }
    Ok(CypherValue::Float(
        2.0 * inter as f64 / (ba.len() + bb.len()) as f64,
    ))
}

fn text_fuzzy_match(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::String(a)), Some(CypherValue::String(b))) = (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let len = a.chars().count();
    let max_dist = if len < 3 {
        0
    } else if len < 5 {
        1
    } else {
        2
    };
    Ok(CypherValue::Bool(levenshtein(a, b) <= max_dist))
}

fn text_index_of(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::String(s)), Some(CypherValue::String(needle))) =
        (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let from = int_arg(args, 2).unwrap_or(0).max(0) as usize;
    let chars: Vec<char> = s.chars().collect();
    let ncs: Vec<char> = needle.chars().collect();
    if ncs.is_empty() {
        return Ok(CypherValue::Integer(from.min(chars.len()) as i64));
    }
    let mut i = from;
    while i + ncs.len() <= chars.len() {
        if chars[i..i + ncs.len()] == ncs[..] {
            return Ok(CypherValue::Integer(i as i64));
        }
        i += 1;
    }
    Ok(CypherValue::Integer(-1))
}

fn text_indexes_of(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::String(s)), Some(CypherValue::String(needle))) =
        (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let chars: Vec<char> = s.chars().collect();
    let ncs: Vec<char> = needle.chars().collect();
    let mut out = Vec::new();
    if !ncs.is_empty() {
        let mut i = 0usize;
        while i + ncs.len() <= chars.len() {
            if chars[i..i + ncs.len()] == ncs[..] {
                out.push(CypherValue::Integer(i as i64));
            }
            i += 1;
        }
    }
    Ok(CypherValue::List(out))
}

fn text_split(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::String(s)), Some(CypherValue::String(pat))) =
        (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let re = crate::executor::cached_regex(pat)?;
    Ok(CypherValue::List(
        re.split(s)
            .map(|p| CypherValue::String(p.to_string()))
            .collect(),
    ))
}

fn text_regreplace(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::String(s)), Some(CypherValue::String(pat))) =
        (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let replacement = str_arg(args, 2).unwrap_or_default();
    let re = crate::executor::cached_regex(pat)?;
    // Java-style $1 group references -> regex-crate ${1}
    let converted = crate::executor::cached_regex(r"\$(\d+)")?
        .replace_all(&replacement, "$${$1}")
        .to_string();
    Ok(CypherValue::String(
        re.replace_all(s, converted.as_str()).to_string(),
    ))
}

fn text_regex_groups(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::String(s)), Some(CypherValue::String(pat))) =
        (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let re = crate::executor::cached_regex(pat)?;
    let out = re
        .captures_iter(s)
        .map(|caps| {
            CypherValue::List(
                (0..caps.len())
                    .map(|i| match caps.get(i) {
                        Some(m) => CypherValue::String(m.as_str().to_string()),
                        None => CypherValue::Null,
                    })
                    .collect(),
            )
        })
        .collect();
    Ok(CypherValue::List(out))
}

fn capitalize(s: &str) -> String {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) => c.to_uppercase().collect::<String>() + cs.as_str(),
        None => String::new(),
    }
}

fn decapitalize(s: &str) -> String {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) => c.to_lowercase().collect::<String>() + cs.as_str(),
        None => String::new(),
    }
}

fn camel_case(s: &str, upper_first: bool) -> String {
    let words: Vec<String> = s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect();
    let mut out = String::new();
    for (i, w) in words.iter().enumerate() {
        if i == 0 && !upper_first {
            out.push_str(w);
        } else {
            out.push_str(&capitalize(w));
        }
    }
    out
}

fn snake_case(s: &str) -> String {
    // APOC snakeCase: split camel/word boundaries, lowercase, join with '-'
    let mut words: Vec<String> = Vec::new();
    for chunk in s.split(|c: char| !c.is_alphanumeric()) {
        if chunk.is_empty() {
            continue;
        }
        let mut cur = String::new();
        let chars: Vec<char> = chunk.chars().collect();
        for (i, c) in chars.iter().enumerate() {
            if c.is_uppercase() && i > 0 && chars[i - 1].is_lowercase() {
                words.push(std::mem::take(&mut cur));
            }
            cur.push(c.to_lowercase().next().unwrap_or(*c));
        }
        if !cur.is_empty() {
            words.push(cur);
        }
    }
    words.join("-")
}

fn text_pad(args: &[CypherValue], left: bool) -> Result<CypherValue, CypherError> {
    let count = int_arg(args, 1).unwrap_or(0).max(0) as usize;
    let delim = str_arg(args, 2).unwrap_or_else(|| " ".to_string());
    let pad_char = delim.chars().next().unwrap_or(' ');
    with_str(args, |s| {
        let len = s.chars().count();
        if len >= count {
            return CypherValue::String(s.to_string());
        }
        let pad: String = std::iter::repeat_n(pad_char, count - len).collect();
        CypherValue::String(if left {
            format!("{pad}{s}")
        } else {
            format!("{s}{pad}")
        })
    })
}

fn text_char_at(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let idx = int_arg(args, 1).unwrap_or(0);
    with_str(args, |s| {
        if idx < 0 {
            return CypherValue::Null;
        }
        match s.chars().nth(idx as usize) {
            Some(c) => CypherValue::Integer(c as i64),
            None => CypherValue::Null,
        }
    })
}

fn text_code(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    match args.first() {
        Some(CypherValue::Integer(n)) => Ok(char::from_u32(*n as u32)
            .map(|c| CypherValue::String(c.to_string()))
            .unwrap_or(CypherValue::Null)),
        _ => Ok(CypherValue::Null),
    }
}

fn text_hex_value(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    match args.first() {
        Some(CypherValue::Integer(n)) => Ok(CypherValue::String(format!("{n:04X}"))),
        _ => Ok(CypherValue::Null),
    }
}

fn text_slug(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    // APOC replaces every RUN of non-word characters with the delimiter WITHOUT trimming the edges:
    // 'Hello, World!' -> 'Hello-World-'
    let delim = str_arg(args, 1).unwrap_or_else(|| "-".to_string());
    with_str(args, |s| {
        let mut out = String::new();
        let mut in_run = false;
        for c in s.chars() {
            if c.is_alphanumeric() || c == '_' {
                in_run = false;
                out.push(c);
            } else if !in_run {
                in_run = true;
                out.push_str(&delim);
            }
        }
        CypherValue::String(out)
    })
}

fn text_repeat(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let count = int_arg(args, 1).unwrap_or(0).max(0) as usize;
    with_str(args, |s| CypherValue::String(s.repeat(count)))
}

fn text_format(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    // Java String.format subset: %s, %d, %f (and %%), enough for typical migrations
    let Some(CypherValue::String(fmt)) = args.first() else {
        return Ok(CypherValue::Null);
    };
    let params: &[CypherValue] = match args.get(1) {
        Some(CypherValue::List(l)) => l,
        _ => &[],
    };
    let mut out = String::new();
    let mut pi = 0usize;
    let mut cs = fmt.chars().peekable();
    while let Some(c) = cs.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match cs.next() {
            Some('%') => out.push('%'),
            Some('s') => {
                let v = params.get(pi).cloned().unwrap_or(CypherValue::Null);
                pi += 1;
                out.push_str(&match v {
                    CypherValue::String(s) => s,
                    other => other.to_string(),
                });
            }
            Some('d') => {
                let v = params.get(pi).and_then(num_of).unwrap_or(0.0);
                pi += 1;
                out.push_str(&(v as i64).to_string());
            }
            Some('f') => {
                let v = params.get(pi).and_then(num_of).unwrap_or(0.0);
                pi += 1;
                out.push_str(&format!("{v:.6}"));
            }
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    Ok(CypherValue::String(out))
}

fn text_base64_decode(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    match args.first() {
        Some(CypherValue::String(s)) => {
            match base64::engine::general_purpose::STANDARD.decode(s.as_bytes()) {
                Ok(bytes) => Ok(CypherValue::String(
                    String::from_utf8_lossy(&bytes).to_string(),
                )),
                Err(e) => Err(CypherError::semantic(format!("invalid base64: {e}"))),
            }
        }
        _ => Ok(CypherValue::Null),
    }
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'*' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn text_url_decode(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_str(args, |s| {
        let mut out = Vec::new();
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                b'%' if i + 2 < bytes.len() + 1 && i + 2 <= bytes.len() => {
                    if let Ok(v) = u8::from_str_radix(&s[i + 1..(i + 3).min(s.len())], 16) {
                        out.push(v);
                        i += 3;
                    } else {
                        out.push(b'%');
                        i += 1;
                    }
                }
                b => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        CypherValue::String(String::from_utf8_lossy(&out).to_string())
    })
}

fn text_random(len: usize, valid: &str) -> String {
    // expand A-Z a-z 0-9 style ranges in the validator spec
    let mut alphabet: Vec<char> = Vec::new();
    let cs: Vec<char> = valid.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        if i + 2 < cs.len() && cs[i + 1] == '-' {
            let (a, b) = (cs[i] as u32, cs[i + 2] as u32);
            for c in a..=b {
                if let Some(ch) = char::from_u32(c) {
                    alphabet.push(ch);
                }
            }
            i += 3;
        } else {
            alphabet.push(cs[i]);
            i += 1;
        }
    }
    if alphabet.is_empty() {
        return String::new();
    }
    (0..len)
        .map(|_| alphabet[cheap_rand(alphabet.len())])
        .collect()
}

fn text_regex_groups_by_name(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(CypherValue::String(s)), Some(CypherValue::String(pat))) =
        (args.first(), args.get(1))
    else {
        return Ok(CypherValue::Null);
    };
    let re = crate::executor::cached_regex(pat)?;
    let names: Vec<&str> = re.capture_names().flatten().collect();
    let out = re
        .captures_iter(s)
        .map(|caps| {
            let mut m = BTreeMap::new();
            m.insert(
                "group".to_string(),
                CypherValue::String(caps.get(0).map(|g| g.as_str()).unwrap_or("").to_string()),
            );
            let mut named = BTreeMap::new();
            for n in &names {
                if let Some(g) = caps.name(n) {
                    named.insert(n.to_string(), CypherValue::String(g.as_str().to_string()));
                }
            }
            m.insert("matches".to_string(), CypherValue::Map(named));
            CypherValue::Map(m)
        })
        .collect();
    Ok(CypherValue::List(out))
}

/// Value as a Cypher literal (apoc.text.toCypher).
#[recursive::recursive]
fn to_cypher_literal(v: &CypherValue) -> String {
    match v {
        CypherValue::Null => "null".to_string(),
        CypherValue::Bool(b) => b.to_string(),
        CypherValue::Integer(i) => i.to_string(),
        CypherValue::Float(f) => f.to_string(),
        CypherValue::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        CypherValue::List(l) => format!(
            "[{}]",
            l.iter()
                .map(to_cypher_literal)
                .collect::<Vec<_>>()
                .join(",")
        ),
        CypherValue::Map(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, x)| format!("{k}:{}", to_cypher_literal(x)))
                .collect::<Vec<_>>()
                .join(",")
        ),
        other => format!("'{}'", other),
    }
}

/// Compact Metaphone phonetic key, close to Lawrence Philips' classic algorithm for matching;
/// approximate, not byte-identical to commons-codec.
fn metaphone(input: &str, max_len: usize) -> String {
    let s: Vec<char> = input
        .to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .collect();
    if s.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let n = s.len();
    let mut i = 0usize;
    // initial-letter exceptions
    if n >= 2 {
        match (s[0], s[1]) {
            ('A', 'E') | ('G', 'N') | ('K', 'N') | ('P', 'N') | ('W', 'R') => i = 1,
            ('X', _) => {
                out.push('S');
                i = 1;
            }
            ('W', 'H') => {
                out.push('W');
                i = 2;
            }
            _ => {}
        }
    }
    let at = |k: isize| -> char {
        if k < 0 || k as usize >= n {
            '\0'
        } else {
            s[k as usize]
        }
    };
    let is_vowel = |c: char| matches!(c, 'A' | 'E' | 'I' | 'O' | 'U');
    while i < n && out.len() < max_len {
        let c = s[i];
        let prev = at(i as isize - 1);
        let next = at(i as isize + 1);
        // skip doubled letters except C
        if c == prev && c != 'C' {
            i += 1;
            continue;
        }
        match c {
            'A' | 'E' | 'I' | 'O' | 'U' => {
                if i == 0 {
                    out.push(c);
                }
            }
            'B' => {
                if !(i == n - 1 && prev == 'M') {
                    out.push('B');
                }
            }
            'C' => {
                if next == 'I' && at(i as isize + 2) == 'A' {
                    out.push('X');
                } else if next == 'H' {
                    out.push(if prev == 'S' { 'K' } else { 'X' });
                    i += 1;
                } else if matches!(next, 'I' | 'E' | 'Y') {
                    out.push('S');
                } else {
                    out.push('K');
                }
            }
            'D' => {
                if next == 'G' && matches!(at(i as isize + 2), 'E' | 'Y' | 'I') {
                    out.push('J');
                    i += 1;
                } else {
                    out.push('T');
                }
            }
            'G' => {
                if next == 'H' {
                    if i + 2 < n && !is_vowel(at(i as isize + 2)) {
                        i += 1;
                    } else {
                        out.push('K');
                        i += 1;
                    }
                } else if next == 'N' {
                    // silent GN
                } else if matches!(next, 'I' | 'E' | 'Y') {
                    out.push('J');
                } else {
                    out.push('K');
                }
            }
            'H' => {
                if is_vowel(prev) && !is_vowel(next) {
                    // silent
                } else if matches!(prev, 'C' | 'S' | 'P' | 'T' | 'G') {
                    // handled by digraphs
                } else {
                    out.push('H');
                }
            }
            'K' => {
                if prev != 'C' {
                    out.push('K');
                }
            }
            'P' => {
                if next == 'H' {
                    out.push('F');
                    i += 1;
                } else {
                    out.push('P');
                }
            }
            'Q' => out.push('K'),
            'S' => {
                if next == 'H' {
                    out.push('X');
                    i += 1;
                } else if next == 'I' && matches!(at(i as isize + 2), 'O' | 'A') {
                    out.push('X');
                } else {
                    out.push('S');
                }
            }
            'T' => {
                if next == 'H' {
                    out.push('0');
                    i += 1;
                } else if next == 'I' && matches!(at(i as isize + 2), 'O' | 'A') {
                    out.push('X');
                } else {
                    out.push('T');
                }
            }
            'V' => out.push('F'),
            'W' | 'Y' => {
                if is_vowel(next) {
                    out.push(c);
                }
            }
            'X' => out.push_str("KS"),
            'Z' => out.push('S'),
            'F' | 'J' | 'L' | 'M' | 'N' | 'R' => out.push(c),
            _ => {}
        }
        i += 1;
    }
    out.truncate(max_len);
    out
}
