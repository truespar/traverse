use traverse_core::tx::view::GraphView;
use traverse_cypher::executor::materialize_value;
use traverse_cypher::frame::SlotValue;
use traverse_cypher::types::CypherValue;

use crate::bolt_type::cypher_value_to_packstream;

use super::value::Value;

// -- zero-alloc record encoding from raw frame slots -----------------

/// Encodes a Bolt Record message straight from frame slots, skipping the intermediate
/// `Vec<CypherValue>` and `Vec<Value>`. Scalar columns (Integer, Float, Bool, Null, String)
/// allocate
/// nothing; complex types (NodeRef, EdgeRef, ...) fall back to materialize ->
/// cypher_value_to_packstream -> encode_value.
pub fn encode_record_from_slots(
    buf: &mut Vec<u8>,
    frame: &[SlotValue],
    slot_indices: &[Option<usize>],
    graph: &dyn GraphView,
    bolt_major: u8,
) {
    // Record = Structure(tag=0x71, 1 field)
    buf.push(0xB1);
    buf.push(0x71);
    // list header for N columns
    encode_list_header(buf, slot_indices.len());
    // each column value
    for slot_idx in slot_indices {
        match slot_idx {
            Some(idx) => encode_slot_value(buf, &frame[*idx], graph, bolt_major),
            None => buf.push(0xC0), // Null
        }
    }
}

/// List header (length prefix only, no items).
#[inline]
fn encode_list_header(buf: &mut Vec<u8>, len: usize) {
    if len <= 0x0F {
        buf.push(0x90 | len as u8);
    } else if len <= 0xFF {
        buf.push(0xD4);
        buf.push(len as u8);
    } else if len <= 0xFFFF {
        buf.push(0xD5);
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        buf.push(0xD6);
        buf.extend_from_slice(&(len as u32).to_be_bytes());
    }
}

/// One SlotValue to PackStream bytes. Hot (0 allocs): Null, Bool, Integer, Float. Warm (0 allocs):
/// Heap(String). Cold (allocates): NodeRef, EdgeRef, SingleHopPath, Heap(other).
#[inline]
fn encode_slot_value(buf: &mut Vec<u8>, sv: &SlotValue, graph: &dyn GraphView, bolt_major: u8) {
    match sv {
        SlotValue::Null => buf.push(0xC0),
        SlotValue::Bool(false) => buf.push(0xC2),
        SlotValue::Bool(true) => buf.push(0xC3),
        SlotValue::Integer(n) => encode_integer(buf, *n),
        SlotValue::Float(f) => {
            buf.push(0xC1);
            buf.extend_from_slice(&f.to_bits().to_be_bytes());
        }
        SlotValue::Heap(boxed) => match boxed.as_ref() {
            CypherValue::String(s) => encode_string(buf, s),
            CypherValue::Integer(n) => encode_integer(buf, *n),
            CypherValue::Float(f) => {
                buf.push(0xC1);
                buf.extend_from_slice(&f.to_bits().to_be_bytes());
            }
            CypherValue::Bool(b) => buf.push(if *b { 0xC3 } else { 0xC2 }),
            CypherValue::Null => buf.push(0xC0),
            other => {
                // cold path: materialize, encode via the generic path
                let cv = materialize_value(other.clone(), graph);
                let pv = cypher_value_to_packstream(&cv, bolt_major);
                encode_value(buf, &pv);
            }
        },
        // cold path: SlotValue -> CypherValue -> materialize -> PackStream
        _ => {
            let cv = materialize_value(sv.to_cypher_value(), graph);
            let pv = cypher_value_to_packstream(&cv, bolt_major);
            encode_value(buf, &pv);
        }
    }
}

/// Encodes a PackStream value into `buf`.
pub fn encode_value(buf: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => buf.push(0xC0),

        Value::Bool(false) => buf.push(0xC2),
        Value::Bool(true) => buf.push(0xC3),

        Value::Integer(n) => encode_integer(buf, *n),
        Value::Float(f) => {
            buf.push(0xC1);
            buf.extend_from_slice(&f.to_bits().to_be_bytes());
        }

        Value::Bytes(b) => encode_bytes(buf, b),
        Value::String(s) => encode_string(buf, s),
        Value::List(items) => encode_list(buf, items),
        Value::Map(entries) => encode_map(buf, entries),

        Value::Structure { tag, fields } => {
            let len = fields.len();
            if len <= 15 {
                buf.push(0xB0 | len as u8);
            } else if len <= 0xFF {
                buf.push(0xDC);
                buf.push(len as u8);
            } else {
                buf.push(0xDD);
                buf.extend_from_slice(&(len as u16).to_be_bytes());
            }
            buf.push(*tag);
            for field in fields {
                encode_value(buf, field);
            }
        }
    }
}

fn encode_integer(buf: &mut Vec<u8>, n: i64) {
    if (-16..=127).contains(&n) {
        // TINY_INT: one byte
        buf.push(n as u8);
    } else if i8::MIN as i64 <= n && n <= i8::MAX as i64 {
        buf.push(0xC8);
        buf.push(n as i8 as u8);
    } else if i16::MIN as i64 <= n && n <= i16::MAX as i64 {
        buf.push(0xC9);
        buf.extend_from_slice(&(n as i16).to_be_bytes());
    } else if i32::MIN as i64 <= n && n <= i32::MAX as i64 {
        buf.push(0xCA);
        buf.extend_from_slice(&(n as i32).to_be_bytes());
    } else {
        buf.push(0xCB);
        buf.extend_from_slice(&n.to_be_bytes());
    }
}

fn encode_bytes(buf: &mut Vec<u8>, b: &[u8]) {
    let len = b.len();
    if len <= 0xFF {
        buf.push(0xCC);
        buf.push(len as u8);
    } else if len <= 0xFFFF {
        buf.push(0xCD);
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        buf.push(0xCE);
        buf.extend_from_slice(&(len as u32).to_be_bytes());
    }
    buf.extend_from_slice(b);
}

fn encode_string(buf: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    let len = b.len();
    if len <= 0x0F {
        buf.push(0x80 | len as u8);
    } else if len <= 0xFF {
        buf.push(0xD0);
        buf.push(len as u8);
    } else if len <= 0xFFFF {
        buf.push(0xD1);
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        buf.push(0xD2);
        buf.extend_from_slice(&(len as u32).to_be_bytes());
    }
    buf.extend_from_slice(b);
}

fn encode_list(buf: &mut Vec<u8>, items: &[Value]) {
    let len = items.len();
    if len <= 0x0F {
        buf.push(0x90 | len as u8);
    } else if len <= 0xFF {
        buf.push(0xD4);
        buf.push(len as u8);
    } else if len <= 0xFFFF {
        buf.push(0xD5);
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        buf.push(0xD6);
        buf.extend_from_slice(&(len as u32).to_be_bytes());
    }
    for item in items {
        encode_value(buf, item);
    }
}

fn encode_map(buf: &mut Vec<u8>, entries: &[(String, Value)]) {
    let len = entries.len();
    if len <= 0x0F {
        buf.push(0xA0 | len as u8);
    } else if len <= 0xFF {
        buf.push(0xD8);
        buf.push(len as u8);
    } else if len <= 0xFFFF {
        buf.push(0xD9);
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        buf.push(0xDA);
        buf.extend_from_slice(&(len as u32).to_be_bytes());
    }
    for (key, val) in entries {
        encode_string(buf, key);
        encode_value(buf, val);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_null() {
        let mut buf = Vec::new();
        encode_value(&mut buf, &Value::Null);
        assert_eq!(buf, vec![0xC0]);
    }

    #[test]
    fn encode_bool() {
        let mut buf = Vec::new();
        encode_value(&mut buf, &Value::Bool(true));
        assert_eq!(buf, vec![0xC3]);
        buf.clear();
        encode_value(&mut buf, &Value::Bool(false));
        assert_eq!(buf, vec![0xC2]);
    }

    #[test]
    fn encode_tiny_int() {
        let mut buf = Vec::new();
        encode_value(&mut buf, &Value::Integer(0));
        assert_eq!(buf, vec![0x00]);

        buf.clear();
        encode_value(&mut buf, &Value::Integer(42));
        assert_eq!(buf, vec![42]);

        buf.clear();
        encode_value(&mut buf, &Value::Integer(-1));
        assert_eq!(buf, vec![0xFF]); // -1 as u8

        buf.clear();
        encode_value(&mut buf, &Value::Integer(-16));
        assert_eq!(buf, vec![0xF0]); // -16 as u8
    }

    #[test]
    fn encode_int8() {
        let mut buf = Vec::new();
        encode_value(&mut buf, &Value::Integer(-17));
        assert_eq!(buf, vec![0xC8, 0xEF]); // INT_8 marker + -17 as i8
    }

    #[test]
    fn encode_int16() {
        let mut buf = Vec::new();
        encode_value(&mut buf, &Value::Integer(1000));
        assert_eq!(buf, vec![0xC9, 0x03, 0xE8]);
    }

    #[test]
    fn encode_int32() {
        let mut buf = Vec::new();
        encode_value(&mut buf, &Value::Integer(100_000));
        assert_eq!(buf, vec![0xCA, 0x00, 0x01, 0x86, 0xA0]);
    }

    #[test]
    fn encode_int64() {
        let mut buf = Vec::new();
        encode_value(&mut buf, &Value::Integer(i64::MAX));
        let mut expected = vec![0xCB];
        expected.extend_from_slice(&i64::MAX.to_be_bytes());
        assert_eq!(buf, expected);
    }

    #[test]
    fn encode_float() {
        let mut buf = Vec::new();
        encode_value(&mut buf, &Value::Float(1.25));
        assert_eq!(buf.len(), 9);
        assert_eq!(buf[0], 0xC1);
    }

    #[test]
    fn encode_tiny_string() {
        let mut buf = Vec::new();
        encode_value(&mut buf, &Value::String("abc".into()));
        assert_eq!(buf, vec![0x83, b'a', b'b', b'c']);
    }

    #[test]
    fn encode_empty_string() {
        let mut buf = Vec::new();
        encode_value(&mut buf, &Value::String("".into()));
        assert_eq!(buf, vec![0x80]);
    }

    #[test]
    fn encode_tiny_list() {
        let mut buf = Vec::new();
        encode_value(
            &mut buf,
            &Value::List(vec![Value::Integer(1), Value::Integer(2)]),
        );
        assert_eq!(buf, vec![0x92, 1, 2]);
    }

    #[test]
    fn encode_tiny_map() {
        let mut buf = Vec::new();
        encode_value(&mut buf, &Value::Map(vec![("a".into(), Value::Integer(1))]));
        // A1 81 61 01
        assert_eq!(buf, vec![0xA1, 0x81, b'a', 1]);
    }

    #[test]
    fn encode_structure() {
        let mut buf = Vec::new();
        encode_value(
            &mut buf,
            &Value::Structure {
                tag: 0x01,
                fields: vec![Value::String("hello".into())],
            },
        );
        assert_eq!(buf[0], 0xB1); // 1 field
        assert_eq!(buf[1], 0x01); // tag
    }

    #[test]
    fn encode_bytes_small() {
        let mut buf = Vec::new();
        encode_value(&mut buf, &Value::Bytes(vec![0xDE, 0xAD]));
        assert_eq!(buf, vec![0xCC, 0x02, 0xDE, 0xAD]);
    }
}
