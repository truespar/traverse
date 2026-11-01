use crate::error::{Result, ServerError};

use super::value::Value;

/// Max container nesting. Bolt messages are shallow (a RUN is a struct around a string + a map of
/// scalars/lists); 64 is far above any legitimate message and stops a hostile deeply nested packet
/// from overflowing the stack (a SIGSEGV for the whole process).
const MAX_DEPTH: usize = 64;

/// Decodes one PackStream value from `buf` at `pos`.
pub fn decode_value(buf: &[u8], pos: &mut usize) -> Result<Value> {
    decode_value_depth(buf, pos, 0)
}

fn decode_value_depth(buf: &[u8], pos: &mut usize, depth: usize) -> Result<Value> {
    if depth > MAX_DEPTH {
        return Err(ServerError::PackStream(format!(
            "message nesting exceeds {MAX_DEPTH} levels"
        )));
    }
    if *pos >= buf.len() {
        return Err(ServerError::PackStream("unexpected end of data".into()));
    }

    let marker = buf[*pos];
    *pos += 1;

    match marker {
        // Null
        0xC0 => Ok(Value::Null),

        // Bool
        0xC2 => Ok(Value::Bool(false)),
        0xC3 => Ok(Value::Bool(true)),

        // Float64
        0xC1 => {
            let bytes = read_bytes(buf, pos, 8)?;
            let bits = u64::from_be_bytes(bytes.try_into().unwrap());
            Ok(Value::Float(f64::from_bits(bits)))
        }

        // INT_8
        0xC8 => {
            let b = read_byte(buf, pos)?;
            Ok(Value::Integer(b as i8 as i64))
        }

        // INT_16
        0xC9 => {
            let bytes = read_bytes(buf, pos, 2)?;
            Ok(Value::Integer(
                i16::from_be_bytes(bytes.try_into().unwrap()) as i64,
            ))
        }

        // INT_32
        0xCA => {
            let bytes = read_bytes(buf, pos, 4)?;
            Ok(Value::Integer(
                i32::from_be_bytes(bytes.try_into().unwrap()) as i64,
            ))
        }

        // INT_64
        0xCB => {
            let bytes = read_bytes(buf, pos, 8)?;
            Ok(Value::Integer(i64::from_be_bytes(
                bytes.try_into().unwrap(),
            )))
        }

        // Bytes
        0xCC => {
            let len = read_byte(buf, pos)? as usize;
            let data = read_bytes(buf, pos, len)?;
            Ok(Value::Bytes(data.to_vec()))
        }
        0xCD => {
            let len = read_u16(buf, pos)? as usize;
            let data = read_bytes(buf, pos, len)?;
            Ok(Value::Bytes(data.to_vec()))
        }
        0xCE => {
            let len = read_u32(buf, pos)? as usize;
            let data = read_bytes(buf, pos, len)?;
            Ok(Value::Bytes(data.to_vec()))
        }

        // TINY_STRING (0x80..=0x8F)
        m if (0x80..=0x8F).contains(&m) => {
            let len = (m & 0x0F) as usize;
            let data = read_bytes(buf, pos, len)?;
            let s = String::from_utf8(data.to_vec())
                .map_err(|e| ServerError::PackStream(format!("invalid UTF-8: {e}")))?;
            Ok(Value::String(s))
        }
        // STRING_8
        0xD0 => {
            let len = read_byte(buf, pos)? as usize;
            read_string(buf, pos, len)
        }
        // STRING_16
        0xD1 => {
            let len = read_u16(buf, pos)? as usize;
            read_string(buf, pos, len)
        }
        // STRING_32
        0xD2 => {
            let len = read_u32(buf, pos)? as usize;
            read_string(buf, pos, len)
        }

        // TINY_LIST (0x90..=0x9F)
        m if (0x90..=0x9F).contains(&m) => {
            let len = (m & 0x0F) as usize;
            read_list(buf, pos, len, depth)
        }
        // LIST_8
        0xD4 => {
            let len = read_byte(buf, pos)? as usize;
            read_list(buf, pos, len, depth)
        }
        // LIST_16
        0xD5 => {
            let len = read_u16(buf, pos)? as usize;
            read_list(buf, pos, len, depth)
        }
        // LIST_32
        0xD6 => {
            let len = read_u32(buf, pos)? as usize;
            read_list(buf, pos, len, depth)
        }

        // TINY_MAP (0xA0..=0xAF)
        m if (0xA0..=0xAF).contains(&m) => {
            let len = (m & 0x0F) as usize;
            read_map(buf, pos, len, depth)
        }
        // MAP_8
        0xD8 => {
            let len = read_byte(buf, pos)? as usize;
            read_map(buf, pos, len, depth)
        }
        // MAP_16
        0xD9 => {
            let len = read_u16(buf, pos)? as usize;
            read_map(buf, pos, len, depth)
        }
        // MAP_32
        0xDA => {
            let len = read_u32(buf, pos)? as usize;
            read_map(buf, pos, len, depth)
        }

        // TINY_STRUCT (0xB0..=0xBF)
        m if (0xB0..=0xBF).contains(&m) => {
            let num_fields = (m & 0x0F) as usize;
            read_struct(buf, pos, num_fields, depth)
        }

        // STRUCT_8
        0xDC => {
            let num_fields = read_byte(buf, pos)? as usize;
            read_struct(buf, pos, num_fields, depth)
        }

        // STRUCT_16
        0xDD => {
            let num_fields = read_u16(buf, pos)? as usize;
            read_struct(buf, pos, num_fields, depth)
        }

        // TINY_INT positive (0x00..=0x7F)
        m if m <= 0x7F => Ok(Value::Integer(m as i64)),

        // TINY_INT negative (0xF0..=0xFF) = -16..=-1
        m if m >= 0xF0 => Ok(Value::Integer(m as i8 as i64)),

        _ => Err(ServerError::PackStream(format!(
            "unknown marker byte: 0x{marker:02X}"
        ))),
    }
}

fn read_byte(buf: &[u8], pos: &mut usize) -> Result<u8> {
    if *pos >= buf.len() {
        return Err(ServerError::PackStream("unexpected end of data".into()));
    }
    let b = buf[*pos];
    *pos += 1;
    Ok(b)
}

fn read_bytes<'a>(buf: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8]> {
    // `*pos + len` could wrap on 32-bit with a hostile len near usize::MAX; compare against the
    // remaining space instead
    if len > buf.len().saturating_sub(*pos) {
        return Err(ServerError::PackStream("unexpected end of data".into()));
    }
    let slice = &buf[*pos..*pos + len];
    *pos += len;
    Ok(slice)
}

/// Rejects an element count that can't fit: every element needs at least one marker byte, so `len`
/// beyond the remaining buffer is malformed/hostile. Returns the count, safe for `with_capacity`
/// (bounded by the buffer, never a giant speculative allocation).
fn checked_count(buf: &[u8], pos: usize, len: usize, what: &str) -> Result<usize> {
    if len > buf.len().saturating_sub(pos) {
        return Err(ServerError::PackStream(format!(
            "{what} length {len} exceeds remaining {} bytes",
            buf.len().saturating_sub(pos)
        )));
    }
    Ok(len)
}

fn read_u16(buf: &[u8], pos: &mut usize) -> Result<u16> {
    let bytes = read_bytes(buf, pos, 2)?;
    Ok(u16::from_be_bytes(bytes.try_into().unwrap()))
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let bytes = read_bytes(buf, pos, 4)?;
    Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
}

fn read_string(buf: &[u8], pos: &mut usize, len: usize) -> Result<Value> {
    let data = read_bytes(buf, pos, len)?;
    let s = String::from_utf8(data.to_vec())
        .map_err(|e| ServerError::PackStream(format!("invalid UTF-8: {e}")))?;
    Ok(Value::String(s))
}

fn read_list(buf: &[u8], pos: &mut usize, len: usize, depth: usize) -> Result<Value> {
    let cap = checked_count(buf, *pos, len, "list")?;
    let mut items = Vec::with_capacity(cap);
    for _ in 0..len {
        items.push(decode_value_depth(buf, pos, depth + 1)?);
    }
    Ok(Value::List(items))
}

fn read_struct(buf: &[u8], pos: &mut usize, num_fields: usize, depth: usize) -> Result<Value> {
    let cap = checked_count(buf, *pos, num_fields, "struct")?;
    let tag = read_byte(buf, pos)?;
    let mut fields = Vec::with_capacity(cap);
    for _ in 0..num_fields {
        fields.push(decode_value_depth(buf, pos, depth + 1)?);
    }
    Ok(Value::Structure { tag, fields })
}

fn read_map(buf: &[u8], pos: &mut usize, len: usize, depth: usize) -> Result<Value> {
    // each entry is a string key + a value, at least 2 bytes: cap at remaining/2 to bound the
    // speculative allocation
    let cap = checked_count(buf, *pos, len, "map")?.min(buf.len().saturating_sub(*pos) / 2 + 1);
    let mut entries = Vec::with_capacity(cap);
    for _ in 0..len {
        let key = match decode_value_depth(buf, pos, depth + 1)? {
            Value::String(s) => s,
            other => {
                return Err(ServerError::PackStream(format!(
                    "expected string map key, got {other:?}"
                )));
            }
        };
        let val = decode_value_depth(buf, pos, depth + 1)?;
        entries.push((key, val));
    }
    Ok(Value::Map(entries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packstream::encode::encode_value;

    fn roundtrip(value: &Value) {
        let mut buf = Vec::new();
        encode_value(&mut buf, value);
        let mut pos = 0;
        let decoded = decode_value(&buf, &mut pos).unwrap();
        assert_eq!(&decoded, value);
        assert_eq!(pos, buf.len(), "not all bytes consumed");
    }

    #[test]
    fn roundtrip_null() {
        roundtrip(&Value::Null);
    }

    #[test]
    fn oversized_list_len_is_rejected_not_allocated() {
        // LIST_32 claiming 4 GiB of elements in a 5-byte packet: must error, not try a giant
        // Vec::with_capacity (which aborts)
        let buf = [0xD6u8, 0xFF, 0xFF, 0xFF, 0xFF];
        let mut pos = 0;
        assert!(decode_value(&buf, &mut pos).is_err());
    }

    #[test]
    fn oversized_map_and_struct_len_rejected() {
        let map = [0xDAu8, 0xFF, 0xFF, 0xFF, 0xFF]; // MAP_32, 4 GiB
        assert!(decode_value(&map, &mut 0).is_err());
        let strct = [0xDDu8, 0xFF, 0xFF, 0x00]; // STRUCT_16, 65535 fields, 1 tag byte
        assert!(decode_value(&strct, &mut 0).is_err());
    }

    #[test]
    fn deep_nesting_is_rejected_not_stack_overflow() {
        // MAX_DEPTH+10 nested TINY_LIST(len 1) markers; without the depth guard this recurses per
        // byte and overflows the stack
        let buf = vec![0x91u8; MAX_DEPTH + 10];
        let mut pos = 0;
        let err = decode_value(&buf, &mut pos);
        assert!(err.is_err(), "expected depth-limit error");
    }

    #[test]
    fn nesting_within_limit_still_decodes() {
        // a modestly nested list (10 deep) still round-trips
        let mut v = Value::Integer(1);
        for _ in 0..10 {
            v = Value::List(vec![v]);
        }
        roundtrip(&v);
    }

    #[test]
    fn roundtrip_bool() {
        roundtrip(&Value::Bool(true));
        roundtrip(&Value::Bool(false));
    }

    #[test]
    fn roundtrip_integers() {
        // Tiny int range
        for n in -16..=127i64 {
            roundtrip(&Value::Integer(n));
        }
        // INT_8
        roundtrip(&Value::Integer(-17));
        roundtrip(&Value::Integer(-128));
        // INT_16
        roundtrip(&Value::Integer(1000));
        roundtrip(&Value::Integer(-1000));
        roundtrip(&Value::Integer(i16::MIN as i64));
        roundtrip(&Value::Integer(i16::MAX as i64));
        // INT_32
        roundtrip(&Value::Integer(100_000));
        roundtrip(&Value::Integer(-100_000));
        roundtrip(&Value::Integer(i32::MIN as i64));
        roundtrip(&Value::Integer(i32::MAX as i64));
        // INT_64
        roundtrip(&Value::Integer(i64::MIN));
        roundtrip(&Value::Integer(i64::MAX));
    }

    #[test]
    fn roundtrip_float() {
        roundtrip(&Value::Float(0.0));
        roundtrip(&Value::Float(1.25));
        roundtrip(&Value::Float(-1.0e100));
        roundtrip(&Value::Float(f64::INFINITY));
    }

    #[test]
    fn roundtrip_bytes() {
        roundtrip(&Value::Bytes(vec![]));
        roundtrip(&Value::Bytes(vec![0xDE, 0xAD]));
        // 256 bytes -> BYTES_16
        roundtrip(&Value::Bytes(vec![0xAB; 256]));
    }

    #[test]
    fn roundtrip_strings() {
        roundtrip(&Value::String("".into()));
        roundtrip(&Value::String("hello".into()));
        roundtrip(&Value::String("a".repeat(15))); // max tiny
        roundtrip(&Value::String("a".repeat(16))); // STRING_8
        roundtrip(&Value::String("a".repeat(256))); // STRING_16
    }

    #[test]
    fn roundtrip_list() {
        roundtrip(&Value::List(vec![]));
        roundtrip(&Value::List(vec![
            Value::Integer(1),
            Value::String("two".into()),
            Value::Null,
        ]));
        // 16 items -> LIST_8
        let items: Vec<Value> = (0..16).map(Value::Integer).collect();
        roundtrip(&Value::List(items));
    }

    #[test]
    fn roundtrip_map() {
        roundtrip(&Value::Map(vec![]));
        roundtrip(&Value::Map(vec![
            ("name".into(), Value::String("Alice".into())),
            ("age".into(), Value::Integer(30)),
        ]));
    }

    #[test]
    fn roundtrip_structure() {
        roundtrip(&Value::Structure {
            tag: 0x01,
            fields: vec![Value::Map(vec![(
                "user_agent".into(),
                Value::String("test".into()),
            )])],
        });
    }

    #[test]
    fn roundtrip_nested() {
        let nested = Value::Map(vec![(
            "data".into(),
            Value::List(vec![Value::Structure {
                tag: 0x4E,
                fields: vec![
                    Value::Integer(42),
                    Value::List(vec![Value::String("Person".into())]),
                    Value::Map(vec![("name".into(), Value::String("Alice".into()))]),
                ],
            }]),
        )]);
        roundtrip(&nested);
    }
}
