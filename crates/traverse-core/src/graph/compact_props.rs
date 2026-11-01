//! Compact binary property storage.
//!
//! `Vec<u8>` encoding instead of `BTreeMap<PropertyKeyId, PropertyValue>`: ~10x
//! less memory for typical 3-5 property nodes.
//!
//! Entry: `[metadata: u8] [key_id: 1-4 bytes] [value: variable]`
//!
//! Metadata byte:
//! - bits 0-3: type tag
//! - bits 4-5: key_id width (0=u8, 1=u16, 2=u32)
//! - bits 6-7: value width hint (per type)

use std::collections::BTreeMap;
use std::fmt;

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use serde::{Deserialize, Serialize};

use super::property::PropertyValue;
use super::types::PropertyKeyId;

// -- Type tags ------------------------------------------------------------

const TAG_NULL: u8 = 0;
const TAG_BOOL_FALSE: u8 = 1;
const TAG_BOOL_TRUE: u8 = 2;
const TAG_INT: u8 = 3;
const TAG_FLOAT: u8 = 4;
const TAG_STRING: u8 = 5;
const TAG_BYTES: u8 = 6;
const TAG_LIST: u8 = 7;
const TAG_MAP: u8 = 8;
const TAG_DATETIME: u8 = 9;
const TAG_DURATION: u8 = 10;
const TAG_POINT2D: u8 = 11;
const TAG_POINT3D: u8 = 12;
const TAG_VECTOR: u8 = 13;

// -- CompactPropertyStore -------------------------------------------------

/// Compact binary property storage on a `Vec<u8>`.
#[derive(Clone, Default, Archive, RkyvSerialize, RkyvDeserialize, Serialize, Deserialize)]
pub struct CompactPropertyStore {
    data: Vec<u8>,
    pub(crate) count: u16,
}

impl CompactPropertyStore {
    /// Empty store.
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            count: 0,
        }
    }

    /// Property count.
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Returns `true` if the store contains no properties.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Encoded data length in bytes.
    pub fn data_len(&self) -> usize {
        self.data.len()
    }

    /// Allocated capacity of the backing Vec.
    pub fn data_capacity(&self) -> usize {
        self.data.capacity()
    }

    /// Shrinks the backing Vec to the data length.
    pub fn shrink_to_fit(&mut self) {
        self.data.shrink_to_fit();
    }

    /// Property by key, owned.
    pub fn get(&self, key: PropertyKeyId) -> Option<PropertyValue> {
        let (offset, end) = self.find_entry(key)?;
        let (_, val, _) = decode_entry_at(&self.data, offset, end);
        Some(val)
    }

    /// String property as borrowed `&str`, no allocation. None if missing or not a string.
    pub fn get_str(&self, key: PropertyKeyId) -> Option<&str> {
        let (offset, _end) = self.find_entry(key)?;
        let meta = self.data[offset];
        let tag = meta & 0x0F;
        if tag != TAG_STRING {
            return None;
        }
        let kw = (meta >> 4) & 0x03;
        let key_size: usize = match kw {
            0 => 1,
            1 => 2,
            _ => 4,
        };
        let p = offset + 1 + key_size; // past metadata + key
        if p + 4 > self.data.len() {
            return None;
        }
        let len = u32::from_le_bytes(self.data[p..p + 4].try_into().ok()?) as usize;
        let start = p + 4;
        if start + len > self.data.len() {
            return None;
        }
        std::str::from_utf8(&self.data[start..start + len]).ok()
    }

    /// Integer property without building a PropertyValue. None if missing or not an int.
    pub fn get_int(&self, key: PropertyKeyId) -> Option<i64> {
        let (offset, _end) = self.find_entry(key)?;
        let meta = self.data[offset];
        let tag = meta & 0x0F;
        if tag != TAG_INT {
            return None;
        }
        let kw = (meta >> 4) & 0x03;
        let vw = (meta >> 6) & 0x03;
        let key_size: usize = match kw {
            0 => 1,
            1 => 2,
            _ => 4,
        };
        let p = offset + 1 + key_size; // past metadata + key
        Some(match vw {
            0 => self.data[p] as i8 as i64,
            1 => i16::from_le_bytes([self.data[p], self.data[p + 1]]) as i64,
            2 => i32::from_le_bytes(self.data[p..p + 4].try_into().unwrap()) as i64,
            _ => i64::from_le_bytes(self.data[p..p + 8].try_into().unwrap()),
        })
    }

    /// Several properties in one pass: `out[j]` = `keys[j]`; unset slots stay `None`.
    pub fn get_multi(&self, keys: &[PropertyKeyId], out: &mut [Option<PropertyValue>]) {
        for slot in out.iter_mut() {
            *slot = None;
        }
        let nk = keys.len();
        if nk == 0 || self.count == 0 {
            return;
        }
        let data = &self.data;
        let len = data.len();
        let mut pos = 0;
        let mut remaining = nk;
        for _ in 0..self.count {
            if pos >= len || remaining == 0 {
                break;
            }
            let entry_start = pos;
            let meta = data[pos];
            pos += 1;
            let kw = (meta >> 4) & 0x03;
            let kid = read_key_id(data, &mut pos, kw);
            // one of the wanted keys?
            if let Some(j) = keys.iter().position(|k| k.0 == kid) {
                if out[j].is_none() {
                    let tag = meta & 0x0F;
                    let vw = (meta >> 6) & 0x03;
                    out[j] = Some(decode_value_by_tag(data, &mut pos, tag, vw));
                    remaining -= 1;
                } else {
                    skip_value(data, &mut pos, meta);
                }
            } else {
                skip_value(data, &mut pos, meta);
            }
            let _ = entry_start; // silence unused warning
        }
    }

    /// Several integer properties in one pass; `out_valid[j] = false` for missing keys or
    /// non-integers.
    pub fn get_multi_int(
        &self,
        keys: &[PropertyKeyId],
        out_vals: &mut [i64],
        out_valid: &mut [bool],
    ) {
        for v in out_valid.iter_mut() {
            *v = false;
        }
        let nk = keys.len();
        if nk == 0 || self.count == 0 {
            return;
        }
        let data = &self.data;
        let len = data.len();
        let mut pos = 0;
        let mut remaining = nk;
        for _ in 0..self.count {
            if pos >= len || remaining == 0 {
                break;
            }
            let meta = data[pos];
            pos += 1;
            let tag = meta & 0x0F;
            let kw = (meta >> 4) & 0x03;
            let vw = (meta >> 6) & 0x03;
            let kid = read_key_id(data, &mut pos, kw);
            if tag == TAG_INT
                && let Some(j) = keys.iter().position(|k| k.0 == kid)
                && !out_valid[j]
            {
                let key_size: usize = match kw {
                    0 => 1,
                    1 => 2,
                    _ => 4,
                };
                // pos is already past metadata + key: value starts here
                let p = pos;
                out_vals[j] = match vw {
                    0 => data[p] as i8 as i64,
                    1 => i16::from_le_bytes([data[p], data[p + 1]]) as i64,
                    2 => i32::from_le_bytes(data[p..p + 4].try_into().unwrap()) as i64,
                    _ => i64::from_le_bytes(data[p..p + 8].try_into().unwrap()),
                };
                out_valid[j] = true;
                remaining -= 1;
                let _ = key_size; // silence unused warning
            }
            skip_value(data, &mut pos, meta);
        }
    }

    /// Inserts a property; returns the previous value.
    pub fn insert(&mut self, key: PropertyKeyId, value: PropertyValue) -> Option<PropertyValue> {
        if let Some((offset, end)) = self.find_entry(key) {
            let (_, old_val, _) = decode_entry_at(&self.data, offset, end);
            // drop the old entry, append the new
            self.data.drain(offset..end);
            self.count -= 1;
            encode_entry(&mut self.data, key, &value);
            self.count += 1;
            Some(old_val)
        } else {
            encode_entry(&mut self.data, key, &value);
            self.count += 1;
            None
        }
    }

    /// Removes a property; returns its value.
    pub fn remove(&mut self, key: PropertyKeyId) -> Option<PropertyValue> {
        if let Some((offset, end)) = self.find_entry(key) {
            let (_, old_val, _) = decode_entry_at(&self.data, offset, end);
            self.data.drain(offset..end);
            self.count -= 1;
            Some(old_val)
        } else {
            None
        }
    }

    /// All entries as owned `(PropertyKeyId, PropertyValue)`.
    pub fn iter(&self) -> CompactPropsIter<'_> {
        CompactPropsIter {
            data: &self.data,
            pos: 0,
            remaining: self.count,
        }
    }

    /// Keys only (skips value decoding where possible).
    pub fn keys(&self) -> CompactKeysIter<'_> {
        CompactKeysIter {
            data: &self.data,
            pos: 0,
            remaining: self.count,
        }
    }

    /// Checks the store is exactly `count` well-formed entries with key ids below `key_limit`.
    /// Loading calls it on every store read from disk: the decoders here index without checks,
    /// which is only sound for bytes this module encoded. Walks nested lists and maps with an
    /// explicit stack, so any nesting depth is checked without recursion.
    pub fn validate(&self, key_limit: u32) -> Result<(), String> {
        let mut r = Cursor {
            data: &self.data,
            pos: 0,
        };
        // values still to read per open list (`false`) or map (`true`, each entry keyed first)
        let mut open: Vec<(u32, bool)> = Vec::new();
        for _ in 0..self.count {
            let meta = r.u8()?;
            let kid = match (meta >> 4) & 0x03 {
                0 => r.u8()? as u32,
                1 => r.u16()? as u32,
                2 => r.u32()?,
                _ => return Err("invalid key width".into()),
            };
            if kid >= key_limit {
                return Err(format!("property key id {kid} names no string"));
            }
            r.value(meta, &mut open)?;
            while let Some((left, keyed)) = open.last_mut() {
                if *left == 0 {
                    open.pop();
                    continue;
                }
                *left -= 1;
                if *keyed {
                    let len = r.u32()? as usize;
                    r.utf8(len)?;
                }
                let meta = r.u8()?;
                if (meta >> 4) & 0x03 != 0 {
                    return Err("key bits set on a nested value".into());
                }
                r.value(meta, &mut open)?;
            }
        }
        if r.pos != self.data.len() {
            return Err(format!(
                "{} bytes after the last of {} properties",
                self.data.len() - r.pos,
                self.count
            ));
        }
        Ok(())
    }

    /// Linear scan for `key`: `Some((entry_start, entry_end))` byte offsets.
    fn find_entry(&self, key: PropertyKeyId) -> Option<(usize, usize)> {
        let target = key.0;
        let mut pos = 0;
        let data = &self.data;
        let len = data.len();
        for _ in 0..self.count {
            if pos >= len {
                break;
            }
            let entry_start = pos;
            let meta = data[pos];
            pos += 1;

            let key_width = (meta >> 4) & 0x03;
            let kid = read_key_id(data, &mut pos, key_width);
            skip_value(data, &mut pos, meta);

            if kid == target {
                return Some((entry_start, pos));
            }
        }
        None
    }
}

// -- Iterators ------------------------------------------------------------

/// Iterator over `(PropertyKeyId, PropertyValue)` pairs.
pub struct CompactPropsIter<'a> {
    data: &'a [u8],
    pos: usize,
    remaining: u16,
}

impl Iterator for CompactPropsIter<'_> {
    type Item = (PropertyKeyId, PropertyValue);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 || self.pos >= self.data.len() {
            return None;
        }
        self.remaining -= 1;
        let (kid, val, new_pos) = decode_entry_from(self.data, self.pos);
        self.pos = new_pos;
        Some((kid, val))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let r = self.remaining as usize;
        (r, Some(r))
    }
}

impl ExactSizeIterator for CompactPropsIter<'_> {}

/// Iterator over keys only.
pub struct CompactKeysIter<'a> {
    data: &'a [u8],
    pos: usize,
    remaining: u16,
}

impl Iterator for CompactKeysIter<'_> {
    type Item = PropertyKeyId;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 || self.pos >= self.data.len() {
            return None;
        }
        self.remaining -= 1;
        let meta = self.data[self.pos];
        self.pos += 1;
        let key_width = (meta >> 4) & 0x03;
        let kid = read_key_id(self.data, &mut self.pos, key_width);
        skip_value(self.data, &mut self.pos, meta);
        Some(PropertyKeyId(kid))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let r = self.remaining as usize;
        (r, Some(r))
    }
}

impl ExactSizeIterator for CompactKeysIter<'_> {}

// -- Trait implementations ------------------------------------------------

impl FromIterator<(PropertyKeyId, PropertyValue)> for CompactPropertyStore {
    fn from_iter<I: IntoIterator<Item = (PropertyKeyId, PropertyValue)>>(iter: I) -> Self {
        let mut store = Self::new();
        for (k, v) in iter {
            encode_entry(&mut store.data, k, &v);
            store.count += 1;
        }
        store
    }
}

impl PartialEq for CompactPropertyStore {
    fn eq(&self, other: &Self) -> bool {
        if self.count != other.count {
            return false;
        }
        if self.data == other.data {
            return true;
        }
        // fallback: decode and compare (insertion order may differ)
        let mut a: Vec<_> = self.iter().collect();
        let mut b: Vec<_> = other.iter().collect();
        a.sort_by_key(|(k, _)| *k);
        b.sort_by_key(|(k, _)| *k);
        a == b
    }
}

impl Eq for CompactPropertyStore {}

impl fmt::Debug for CompactPropertyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries: Vec<_> = self.iter().collect();
        f.debug_map()
            .entries(entries.iter().map(|(k, v)| (k, v)))
            .finish()
    }
}

// -- Validation -----------------------------------------------------------

/// Bounds-checked reader for [`CompactPropertyStore::validate`].
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or("property data ends early")?;
        let bytes = &self.data[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().unwrap_or_default(),
        ))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().unwrap_or_default(),
        ))
    }

    fn utf8(&mut self, len: usize) -> Result<(), String> {
        std::str::from_utf8(self.take(len)?)
            .map(drop)
            .map_err(|_| "property string is not UTF-8".into())
    }

    /// Checks one value's payload after its metadata byte; a list or map opens a level on `open`.
    fn value(&mut self, meta: u8, open: &mut Vec<(u32, bool)>) -> Result<(), String> {
        let tag = meta & 0x0F;
        let width = meta >> 6;
        let fixed = |w: u8| -> Result<(), String> {
            if w == 0 {
                Ok(())
            } else {
                Err(format!("width bits on property type {tag}"))
            }
        };
        match tag {
            TAG_NULL | TAG_BOOL_FALSE | TAG_BOOL_TRUE => fixed(width),
            TAG_INT => self.take([1, 2, 4, 8][width as usize]).map(drop),
            TAG_FLOAT => match width {
                0 => self.take(4).map(drop),
                1 => self.take(8).map(drop),
                _ => Err("invalid float width".into()),
            },
            TAG_STRING => {
                fixed(width)?;
                let len = self.u32()? as usize;
                self.utf8(len)
            }
            TAG_BYTES => {
                fixed(width)?;
                let len = self.u32()? as usize;
                self.take(len).map(drop)
            }
            TAG_LIST | TAG_MAP => {
                fixed(width)?;
                let count = self.u32()?;
                open.push((count, tag == TAG_MAP));
                Ok(())
            }
            TAG_DATETIME | TAG_DURATION => {
                fixed(width)?;
                self.take(8).map(drop)
            }
            TAG_POINT2D => {
                fixed(width)?;
                self.take(20).map(drop)
            }
            TAG_POINT3D => {
                fixed(width)?;
                self.take(28).map(drop)
            }
            TAG_VECTOR => {
                fixed(width)?;
                let count = self.u32()? as usize;
                let len = count.checked_mul(4).ok_or("vector too long")?;
                self.take(len).map(drop)
            }
            _ => Err(format!("unknown property type {tag}")),
        }
    }
}

// -- Encoding helpers -----------------------------------------------------

/// Key width bits for the metadata byte.
fn key_width_bits(kid: u32) -> u8 {
    if kid <= u8::MAX as u32 {
        0
    } else if kid <= u16::MAX as u32 {
        1
    } else {
        2
    }
}

/// Writes a key ID at the given width.
fn write_key_id(buf: &mut Vec<u8>, kid: u32, width: u8) {
    match width {
        0 => buf.push(kid as u8),
        1 => buf.extend_from_slice(&(kid as u16).to_le_bytes()),
        _ => buf.extend_from_slice(&kid.to_le_bytes()),
    }
}

/// Reads a key ID at the given width.
fn read_key_id(data: &[u8], pos: &mut usize, width: u8) -> u32 {
    match width {
        0 => {
            let v = data[*pos] as u32;
            *pos += 1;
            v
        }
        1 => {
            let v = u16::from_le_bytes([data[*pos], data[*pos + 1]]) as u32;
            *pos += 2;
            v
        }
        _ => {
            let v =
                u32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
            *pos += 4;
            v
        }
    }
}

/// Integer value-width hint (metadata bits 6-7).
fn int_width_hint(v: i64) -> u8 {
    if v >= i8::MIN as i64 && v <= i8::MAX as i64 {
        0
    } else if v >= i16::MIN as i64 && v <= i16::MAX as i64 {
        1
    } else if v >= i32::MIN as i64 && v <= i32::MAX as i64 {
        2
    } else {
        3
    }
}

/// Float value-width hint.
fn float_width_hint(v: f64) -> u8 {
    let as_f32 = v as f32;
    if as_f32 as f64 == v {
        0 // f32 lossless
    } else {
        1 // f64
    }
}

/// Encodes one entry.
fn encode_entry(buf: &mut Vec<u8>, key: PropertyKeyId, value: &PropertyValue) {
    let kid = key.0;
    let kw = key_width_bits(kid);

    match value {
        PropertyValue::Null => {
            buf.push(TAG_NULL | (kw << 4));
            write_key_id(buf, kid, kw);
        }
        PropertyValue::Bool(false) => {
            buf.push(TAG_BOOL_FALSE | (kw << 4));
            write_key_id(buf, kid, kw);
        }
        PropertyValue::Bool(true) => {
            buf.push(TAG_BOOL_TRUE | (kw << 4));
            write_key_id(buf, kid, kw);
        }
        PropertyValue::Int(v) => {
            let vw = int_width_hint(*v);
            buf.push(TAG_INT | (kw << 4) | (vw << 6));
            write_key_id(buf, kid, kw);
            match vw {
                0 => buf.push(*v as i8 as u8),
                1 => buf.extend_from_slice(&(*v as i16).to_le_bytes()),
                2 => buf.extend_from_slice(&(*v as i32).to_le_bytes()),
                _ => buf.extend_from_slice(&v.to_le_bytes()),
            }
        }
        PropertyValue::Float(v) => {
            let vw = float_width_hint(*v);
            buf.push(TAG_FLOAT | (kw << 4) | (vw << 6));
            write_key_id(buf, kid, kw);
            match vw {
                0 => buf.extend_from_slice(&(*v as f32).to_le_bytes()),
                _ => buf.extend_from_slice(&v.to_le_bytes()),
            }
        }
        PropertyValue::String(s) => {
            buf.push(TAG_STRING | (kw << 4));
            write_key_id(buf, kid, kw);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        PropertyValue::Bytes(b) => {
            buf.push(TAG_BYTES | (kw << 4));
            write_key_id(buf, kid, kw);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        PropertyValue::List(items) => {
            buf.push(TAG_LIST | (kw << 4));
            write_key_id(buf, kid, kw);
            buf.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for item in items {
                encode_value(buf, item);
            }
        }
        PropertyValue::Map(map) => {
            buf.push(TAG_MAP | (kw << 4));
            write_key_id(buf, kid, kw);
            buf.extend_from_slice(&(map.len() as u32).to_le_bytes());
            for (k, v) in map {
                let kb = k.as_bytes();
                buf.extend_from_slice(&(kb.len() as u32).to_le_bytes());
                buf.extend_from_slice(kb);
                encode_value(buf, v);
            }
        }
        PropertyValue::DateTime(ms) => {
            buf.push(TAG_DATETIME | (kw << 4));
            write_key_id(buf, kid, kw);
            buf.extend_from_slice(&ms.to_le_bytes());
        }
        PropertyValue::Duration(ms) => {
            buf.push(TAG_DURATION | (kw << 4));
            write_key_id(buf, kid, kw);
            buf.extend_from_slice(&ms.to_le_bytes());
        }
        PropertyValue::Point2D { x, y, srid } => {
            buf.push(TAG_POINT2D | (kw << 4));
            write_key_id(buf, kid, kw);
            buf.extend_from_slice(&x.to_le_bytes());
            buf.extend_from_slice(&y.to_le_bytes());
            buf.extend_from_slice(&srid.to_le_bytes());
        }
        PropertyValue::Point3D { x, y, z, srid } => {
            buf.push(TAG_POINT3D | (kw << 4));
            write_key_id(buf, kid, kw);
            buf.extend_from_slice(&x.to_le_bytes());
            buf.extend_from_slice(&y.to_le_bytes());
            buf.extend_from_slice(&z.to_le_bytes());
            buf.extend_from_slice(&srid.to_le_bytes());
        }
        PropertyValue::Vector(v) => {
            buf.push(TAG_VECTOR | (kw << 4));
            write_key_id(buf, kid, kw);
            buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
            for f in v {
                buf.extend_from_slice(&f.to_le_bytes());
            }
        }
    }
}

/// Encodes a bare value (no key), inside List and Map.
fn encode_value(buf: &mut Vec<u8>, value: &PropertyValue) {
    match value {
        PropertyValue::Null => buf.push(TAG_NULL),
        PropertyValue::Bool(false) => buf.push(TAG_BOOL_FALSE),
        PropertyValue::Bool(true) => buf.push(TAG_BOOL_TRUE),
        PropertyValue::Int(v) => {
            let vw = int_width_hint(*v);
            buf.push(TAG_INT | (vw << 6));
            match vw {
                0 => buf.push(*v as i8 as u8),
                1 => buf.extend_from_slice(&(*v as i16).to_le_bytes()),
                2 => buf.extend_from_slice(&(*v as i32).to_le_bytes()),
                _ => buf.extend_from_slice(&v.to_le_bytes()),
            }
        }
        PropertyValue::Float(v) => {
            let vw = float_width_hint(*v);
            buf.push(TAG_FLOAT | (vw << 6));
            match vw {
                0 => buf.extend_from_slice(&(*v as f32).to_le_bytes()),
                _ => buf.extend_from_slice(&v.to_le_bytes()),
            }
        }
        PropertyValue::String(s) => {
            buf.push(TAG_STRING);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        PropertyValue::Bytes(b) => {
            buf.push(TAG_BYTES);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        PropertyValue::List(items) => {
            buf.push(TAG_LIST);
            buf.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for item in items {
                encode_value(buf, item);
            }
        }
        PropertyValue::Map(map) => {
            buf.push(TAG_MAP);
            buf.extend_from_slice(&(map.len() as u32).to_le_bytes());
            for (k, v) in map {
                let kb = k.as_bytes();
                buf.extend_from_slice(&(kb.len() as u32).to_le_bytes());
                buf.extend_from_slice(kb);
                encode_value(buf, v);
            }
        }
        PropertyValue::DateTime(ms) => {
            buf.push(TAG_DATETIME);
            buf.extend_from_slice(&ms.to_le_bytes());
        }
        PropertyValue::Duration(ms) => {
            buf.push(TAG_DURATION);
            buf.extend_from_slice(&ms.to_le_bytes());
        }
        PropertyValue::Point2D { x, y, srid } => {
            buf.push(TAG_POINT2D);
            buf.extend_from_slice(&x.to_le_bytes());
            buf.extend_from_slice(&y.to_le_bytes());
            buf.extend_from_slice(&srid.to_le_bytes());
        }
        PropertyValue::Point3D { x, y, z, srid } => {
            buf.push(TAG_POINT3D);
            buf.extend_from_slice(&x.to_le_bytes());
            buf.extend_from_slice(&y.to_le_bytes());
            buf.extend_from_slice(&z.to_le_bytes());
            buf.extend_from_slice(&srid.to_le_bytes());
        }
        PropertyValue::Vector(v) => {
            buf.push(TAG_VECTOR);
            buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
            for f in v {
                buf.extend_from_slice(&f.to_le_bytes());
            }
        }
    }
}

// -- Decoding helpers -----------------------------------------------------

/// Decodes a bare value (inside List / Map).
fn decode_value(data: &[u8], pos: &mut usize) -> PropertyValue {
    let meta = data[*pos];
    *pos += 1;
    let tag = meta & 0x0F;
    let vw = (meta >> 6) & 0x03;
    decode_value_by_tag(data, pos, tag, vw)
}

/// Decodes a value from its type tag and width hint.
fn decode_value_by_tag(data: &[u8], pos: &mut usize, tag: u8, vw: u8) -> PropertyValue {
    match tag {
        TAG_NULL => PropertyValue::Null,
        TAG_BOOL_FALSE => PropertyValue::Bool(false),
        TAG_BOOL_TRUE => PropertyValue::Bool(true),
        TAG_INT => {
            let v = match vw {
                0 => {
                    let v = data[*pos] as i8 as i64;
                    *pos += 1;
                    v
                }
                1 => {
                    let v = i16::from_le_bytes([data[*pos], data[*pos + 1]]) as i64;
                    *pos += 2;
                    v
                }
                2 => {
                    let v = i32::from_le_bytes([
                        data[*pos],
                        data[*pos + 1],
                        data[*pos + 2],
                        data[*pos + 3],
                    ]) as i64;
                    *pos += 4;
                    v
                }
                _ => {
                    let v = i64::from_le_bytes([
                        data[*pos],
                        data[*pos + 1],
                        data[*pos + 2],
                        data[*pos + 3],
                        data[*pos + 4],
                        data[*pos + 5],
                        data[*pos + 6],
                        data[*pos + 7],
                    ]);
                    *pos += 8;
                    v
                }
            };
            PropertyValue::Int(v)
        }
        TAG_FLOAT => {
            let v = match vw {
                0 => {
                    let v = f32::from_le_bytes([
                        data[*pos],
                        data[*pos + 1],
                        data[*pos + 2],
                        data[*pos + 3],
                    ]) as f64;
                    *pos += 4;
                    v
                }
                _ => {
                    let v = f64::from_le_bytes([
                        data[*pos],
                        data[*pos + 1],
                        data[*pos + 2],
                        data[*pos + 3],
                        data[*pos + 4],
                        data[*pos + 5],
                        data[*pos + 6],
                        data[*pos + 7],
                    ]);
                    *pos += 8;
                    v
                }
            };
            PropertyValue::Float(v)
        }
        TAG_STRING => {
            let len = read_u32_le(data, pos) as usize;
            let s = std::str::from_utf8(&data[*pos..*pos + len])
                .unwrap()
                .to_string();
            *pos += len;
            PropertyValue::String(s)
        }
        TAG_BYTES => {
            let len = read_u32_le(data, pos) as usize;
            let b = data[*pos..*pos + len].to_vec();
            *pos += len;
            PropertyValue::Bytes(b)
        }
        TAG_LIST => {
            let count = read_u32_le(data, pos) as usize;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                items.push(decode_value(data, pos));
            }
            PropertyValue::List(items)
        }
        TAG_MAP => {
            let count = read_u32_le(data, pos) as usize;
            let mut map = BTreeMap::new();
            for _ in 0..count {
                let klen = read_u32_le(data, pos) as usize;
                let k = std::str::from_utf8(&data[*pos..*pos + klen])
                    .unwrap()
                    .to_string();
                *pos += klen;
                let v = decode_value(data, pos);
                map.insert(k, v);
            }
            PropertyValue::Map(map)
        }
        TAG_DATETIME => {
            let ms = read_i64_le(data, pos);
            PropertyValue::DateTime(ms)
        }
        TAG_DURATION => {
            let ms = read_i64_le(data, pos);
            PropertyValue::Duration(ms)
        }
        TAG_POINT2D => {
            let x = read_f64_le(data, pos);
            let y = read_f64_le(data, pos);
            let srid = read_u32_le(data, pos);
            PropertyValue::Point2D { x, y, srid }
        }
        TAG_POINT3D => {
            let x = read_f64_le(data, pos);
            let y = read_f64_le(data, pos);
            let z = read_f64_le(data, pos);
            let srid = read_u32_le(data, pos);
            PropertyValue::Point3D { x, y, z, srid }
        }
        TAG_VECTOR => {
            let count = read_u32_le(data, pos) as usize;
            let mut v = Vec::with_capacity(count);
            for _ in 0..count {
                let f = f32::from_le_bytes([
                    data[*pos],
                    data[*pos + 1],
                    data[*pos + 2],
                    data[*pos + 3],
                ]);
                *pos += 4;
                v.push(f);
            }
            PropertyValue::Vector(v)
        }
        _ => PropertyValue::Null, // unknown tag (defensive)
    }
}

/// Decodes a full entry (key + value) at `start`.
fn decode_entry_from(data: &[u8], start: usize) -> (PropertyKeyId, PropertyValue, usize) {
    let mut pos = start;
    let meta = data[pos];
    pos += 1;
    let tag = meta & 0x0F;
    let kw = (meta >> 4) & 0x03;
    let vw = (meta >> 6) & 0x03;
    let kid = read_key_id(data, &mut pos, kw);
    let val = decode_value_by_tag(data, &mut pos, tag, vw);
    (PropertyKeyId(kid), val, pos)
}

/// Decodes an entry in `start..end`.
fn decode_entry_at(
    data: &[u8],
    start: usize,
    _end: usize,
) -> (PropertyKeyId, PropertyValue, usize) {
    decode_entry_from(data, start)
}

/// Skips a value without decoding it (keys-only iteration).
fn skip_value(data: &[u8], pos: &mut usize, meta: u8) {
    let tag = meta & 0x0F;
    let vw = (meta >> 6) & 0x03;
    match tag {
        TAG_NULL | TAG_BOOL_FALSE | TAG_BOOL_TRUE => {}
        TAG_INT => {
            *pos += match vw {
                0 => 1,
                1 => 2,
                2 => 4,
                _ => 8,
            };
        }
        TAG_FLOAT => {
            *pos += match vw {
                0 => 4,
                _ => 8,
            };
        }
        TAG_STRING | TAG_BYTES => {
            let len = read_u32_le(data, pos) as usize;
            *pos += len;
        }
        TAG_LIST => {
            let count = read_u32_le(data, pos) as usize;
            for _ in 0..count {
                skip_standalone_value(data, pos);
            }
        }
        TAG_MAP => {
            let count = read_u32_le(data, pos) as usize;
            for _ in 0..count {
                let klen = read_u32_le(data, pos) as usize;
                *pos += klen;
                skip_standalone_value(data, pos);
            }
        }
        TAG_DATETIME | TAG_DURATION => {
            *pos += 8;
        }
        TAG_POINT2D => {
            *pos += 20; // 2 * f64 + u32
        }
        TAG_POINT3D => {
            *pos += 28; // 3 * f64 + u32
        }
        TAG_VECTOR => {
            let count = read_u32_le(data, pos) as usize;
            *pos += count * 4;
        }
        _ => {}
    }
}

/// Skips a bare value (meta byte + payload).
fn skip_standalone_value(data: &[u8], pos: &mut usize) {
    let meta = data[*pos];
    *pos += 1;
    skip_value(data, pos, meta);
}

// -- Primitive read helpers -----------------------------------------------

fn read_u32_le(data: &[u8], pos: &mut usize) -> u32 {
    let v = u32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
    *pos += 4;
    v
}

fn read_i64_le(data: &[u8], pos: &mut usize) -> i64 {
    let v = i64::from_le_bytes([
        data[*pos],
        data[*pos + 1],
        data[*pos + 2],
        data[*pos + 3],
        data[*pos + 4],
        data[*pos + 5],
        data[*pos + 6],
        data[*pos + 7],
    ]);
    *pos += 8;
    v
}

fn read_f64_le(data: &[u8], pos: &mut usize) -> f64 {
    let v = f64::from_le_bytes([
        data[*pos],
        data[*pos + 1],
        data[*pos + 2],
        data[*pos + 3],
        data[*pos + 4],
        data[*pos + 5],
        data[*pos + 6],
        data[*pos + 7],
    ]);
    *pos += 8;
    v
}

// -- Tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn kid(v: u32) -> PropertyKeyId {
        PropertyKeyId(v)
    }

    #[test]
    fn roundtrip_null() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(1), PropertyValue::Null);
        assert_eq!(s.get(kid(1)), Some(PropertyValue::Null));
    }

    #[test]
    fn roundtrip_bool() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(1), PropertyValue::Bool(true));
        s.insert(kid(2), PropertyValue::Bool(false));
        assert_eq!(s.get(kid(1)), Some(PropertyValue::Bool(true)));
        assert_eq!(s.get(kid(2)), Some(PropertyValue::Bool(false)));
    }

    #[test]
    fn roundtrip_int_widths() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(1), PropertyValue::Int(42)); // i8
        s.insert(kid(2), PropertyValue::Int(1000)); // i16
        s.insert(kid(3), PropertyValue::Int(100_000)); // i32
        s.insert(kid(4), PropertyValue::Int(i64::MAX)); // i64
        s.insert(kid(5), PropertyValue::Int(-128)); // i8 min
        s.insert(kid(6), PropertyValue::Int(-32768)); // i16 min

        assert_eq!(s.get(kid(1)), Some(PropertyValue::Int(42)));
        assert_eq!(s.get(kid(2)), Some(PropertyValue::Int(1000)));
        assert_eq!(s.get(kid(3)), Some(PropertyValue::Int(100_000)));
        assert_eq!(s.get(kid(4)), Some(PropertyValue::Int(i64::MAX)));
        assert_eq!(s.get(kid(5)), Some(PropertyValue::Int(-128)));
        assert_eq!(s.get(kid(6)), Some(PropertyValue::Int(-32768)));
    }

    #[test]
    fn roundtrip_float_f32_lossless() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(1), PropertyValue::Float(1.5)); // lossless f32
        s.insert(kid(2), PropertyValue::Float(std::f64::consts::PI)); // requires f64
        assert_eq!(s.get(kid(1)), Some(PropertyValue::Float(1.5)));
        assert_eq!(
            s.get(kid(2)),
            Some(PropertyValue::Float(std::f64::consts::PI))
        );
    }

    #[test]
    fn roundtrip_string() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(1), PropertyValue::String("hello".into()));
        assert_eq!(s.get(kid(1)), Some(PropertyValue::String("hello".into())));
    }

    #[test]
    fn roundtrip_bytes() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(1), PropertyValue::Bytes(vec![1, 2, 3]));
        assert_eq!(s.get(kid(1)), Some(PropertyValue::Bytes(vec![1, 2, 3])));
    }

    #[test]
    fn roundtrip_list() {
        let mut s = CompactPropertyStore::new();
        let list = PropertyValue::List(vec![
            PropertyValue::Int(1),
            PropertyValue::String("two".into()),
        ]);
        s.insert(kid(1), list.clone());
        assert_eq!(s.get(kid(1)), Some(list));
    }

    #[test]
    fn roundtrip_map() {
        let mut s = CompactPropertyStore::new();
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), PropertyValue::Int(1));
        m.insert("b".to_string(), PropertyValue::Bool(true));
        let map = PropertyValue::Map(m);
        s.insert(kid(1), map.clone());
        assert_eq!(s.get(kid(1)), Some(map));
    }

    #[test]
    fn roundtrip_datetime_duration() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(1), PropertyValue::DateTime(1700000000000));
        s.insert(kid(2), PropertyValue::Duration(60_000));
        assert_eq!(s.get(kid(1)), Some(PropertyValue::DateTime(1700000000000)));
        assert_eq!(s.get(kid(2)), Some(PropertyValue::Duration(60_000)));
    }

    #[test]
    fn roundtrip_point2d() {
        let mut s = CompactPropertyStore::new();
        let p = PropertyValue::Point2D {
            x: 1.0,
            y: 2.0,
            srid: 4326,
        };
        s.insert(kid(1), p.clone());
        assert_eq!(s.get(kid(1)), Some(p));
    }

    #[test]
    fn roundtrip_point3d() {
        let mut s = CompactPropertyStore::new();
        let p = PropertyValue::Point3D {
            x: 1.0,
            y: 2.0,
            z: 3.0,
            srid: 4326,
        };
        s.insert(kid(1), p.clone());
        assert_eq!(s.get(kid(1)), Some(p));
    }

    #[test]
    fn roundtrip_vector() {
        let mut s = CompactPropertyStore::new();
        let v = PropertyValue::Vector(vec![1.0, 2.0, 3.0]);
        s.insert(kid(1), v.clone());
        assert_eq!(s.get(kid(1)), Some(v));
    }

    #[test]
    fn key_width_u8_u16_u32() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(0), PropertyValue::Int(0));
        s.insert(kid(255), PropertyValue::Int(1));
        s.insert(kid(256), PropertyValue::Int(2));
        s.insert(kid(65535), PropertyValue::Int(3));
        s.insert(kid(65536), PropertyValue::Int(4));
        assert_eq!(s.get(kid(0)), Some(PropertyValue::Int(0)));
        assert_eq!(s.get(kid(255)), Some(PropertyValue::Int(1)));
        assert_eq!(s.get(kid(256)), Some(PropertyValue::Int(2)));
        assert_eq!(s.get(kid(65535)), Some(PropertyValue::Int(3)));
        assert_eq!(s.get(kid(65536)), Some(PropertyValue::Int(4)));
    }

    #[test]
    fn insert_replace() {
        let mut s = CompactPropertyStore::new();
        assert_eq!(s.insert(kid(1), PropertyValue::Int(10)), None);
        assert_eq!(
            s.insert(kid(1), PropertyValue::Int(20)),
            Some(PropertyValue::Int(10))
        );
        assert_eq!(s.len(), 1);
        assert_eq!(s.get(kid(1)), Some(PropertyValue::Int(20)));
    }

    #[test]
    fn remove_existing_and_missing() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(1), PropertyValue::Int(10));
        s.insert(kid(2), PropertyValue::Int(20));
        assert_eq!(s.remove(kid(1)), Some(PropertyValue::Int(10)));
        assert_eq!(s.len(), 1);
        assert_eq!(s.get(kid(1)), None);
        assert_eq!(s.remove(kid(99)), None);
    }

    #[test]
    fn clone_independence() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(1), PropertyValue::Int(10));
        let mut s2 = s.clone();
        s2.insert(kid(1), PropertyValue::Int(20));
        assert_eq!(s.get(kid(1)), Some(PropertyValue::Int(10)));
        assert_eq!(s2.get(kid(1)), Some(PropertyValue::Int(20)));
    }

    #[test]
    fn partial_eq_different_order() {
        let mut a = CompactPropertyStore::new();
        a.insert(kid(1), PropertyValue::Int(10));
        a.insert(kid(2), PropertyValue::String("hello".into()));

        let mut b = CompactPropertyStore::new();
        b.insert(kid(2), PropertyValue::String("hello".into()));
        b.insert(kid(1), PropertyValue::Int(10));

        assert_eq!(a, b);
    }

    #[test]
    fn from_iterator() {
        let entries = vec![
            (kid(1), PropertyValue::Int(10)),
            (kid(2), PropertyValue::String("hello".into())),
        ];
        let s: CompactPropertyStore = entries.into_iter().collect();
        assert_eq!(s.len(), 2);
        assert_eq!(s.get(kid(1)), Some(PropertyValue::Int(10)));
        assert_eq!(s.get(kid(2)), Some(PropertyValue::String("hello".into())));
    }

    #[test]
    fn iter_yields_all() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(1), PropertyValue::Int(10));
        s.insert(kid(2), PropertyValue::String("x".into()));
        let mut entries: Vec<_> = s.iter().collect();
        entries.sort_by_key(|(k, _)| *k);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], (kid(1), PropertyValue::Int(10)));
        assert_eq!(entries[1], (kid(2), PropertyValue::String("x".into())));
    }

    #[test]
    fn keys_iter() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(5), PropertyValue::Int(10));
        s.insert(kid(3), PropertyValue::String("x".into()));
        let mut keys: Vec<_> = s.keys().collect();
        keys.sort();
        assert_eq!(keys, vec![kid(3), kid(5)]);
    }

    #[test]
    fn pokec_3_prop_memory_size() {
        // typical small node: 3 properties (int, int, string)
        let mut s = CompactPropertyStore::new();
        s.insert(kid(0), PropertyValue::Int(1));
        s.insert(kid(1), PropertyValue::Int(25));
        s.insert(kid(2), PropertyValue::String("Bratislava".into()));
        // well under 50 bytes of payload
        assert!(
            s.data.len() < 50,
            "data.len() = {} (expected < 50)",
            s.data.len()
        );
    }

    #[test]
    fn get_int_all_widths() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(1), PropertyValue::Int(42)); // i8
        s.insert(kid(2), PropertyValue::Int(1000)); // i16
        s.insert(kid(3), PropertyValue::Int(100_000)); // i32
        s.insert(kid(4), PropertyValue::Int(i64::MAX)); // i64
        s.insert(kid(5), PropertyValue::Int(-128)); // i8 min
        s.insert(kid(6), PropertyValue::Int(-32768)); // i16 min

        assert_eq!(s.get_int(kid(1)), Some(42));
        assert_eq!(s.get_int(kid(2)), Some(1000));
        assert_eq!(s.get_int(kid(3)), Some(100_000));
        assert_eq!(s.get_int(kid(4)), Some(i64::MAX));
        assert_eq!(s.get_int(kid(5)), Some(-128));
        assert_eq!(s.get_int(kid(6)), Some(-32768));
    }

    #[test]
    fn get_int_returns_none_for_non_int() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(1), PropertyValue::Float(1.25));
        s.insert(kid(2), PropertyValue::String("hello".into()));
        s.insert(kid(3), PropertyValue::Bool(true));
        s.insert(kid(4), PropertyValue::Null);

        assert_eq!(s.get_int(kid(1)), None);
        assert_eq!(s.get_int(kid(2)), None);
        assert_eq!(s.get_int(kid(3)), None);
        assert_eq!(s.get_int(kid(4)), None);
        assert_eq!(s.get_int(kid(99)), None); // missing key
    }

    #[test]
    fn empty_store() {
        let s = CompactPropertyStore::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.get(kid(1)), None);
        assert_eq!(s.iter().count(), 0);
        assert_eq!(s.keys().count(), 0);
    }

    #[test]
    fn validate_accepts_what_the_encoder_writes() {
        let mut map = BTreeMap::new();
        map.insert(
            "k".to_string(),
            PropertyValue::List(vec![PropertyValue::Int(-3)]),
        );
        let mut nested = PropertyValue::Int(1);
        for _ in 0..2_000 {
            nested = PropertyValue::List(vec![nested]);
        }
        let store: CompactPropertyStore = vec![
            (kid(0), PropertyValue::Null),
            (kid(1), PropertyValue::Bool(true)),
            (kid(2), PropertyValue::Int(i64::MIN)),
            (kid(3), PropertyValue::Float(0.1)),
            (kid(4), PropertyValue::Float(1.5)),
            (kid(300), PropertyValue::String("ü".into())),
            (kid(70_000), PropertyValue::Bytes(vec![1, 2])),
            (kid(5), PropertyValue::Map(map)),
            (kid(6), PropertyValue::DateTime(1)),
            (
                kid(7),
                PropertyValue::Point3D {
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                    srid: 9157,
                },
            ),
            (kid(8), PropertyValue::Vector(vec![0.5; 3])),
            (kid(9), nested),
        ]
        .into_iter()
        .collect();
        store.validate(70_001).unwrap();
        assert!(
            store
                .validate(70_000)
                .unwrap_err()
                .contains("names no string")
        );
    }

    #[test]
    fn validate_rejects_every_damaged_encoding() {
        let store: CompactPropertyStore = vec![
            (kid(1), PropertyValue::String("hello".into())),
            (kid(2), PropertyValue::List(vec![PropertyValue::Int(7)])),
        ]
        .into_iter()
        .collect();
        store.validate(10).unwrap();
        // truncation anywhere is caught
        for len in 0..store.data.len() {
            let mut bad = store.clone();
            bad.data.truncate(len);
            assert!(bad.validate(10).is_err(), "truncated to {len}");
        }
        // trailing bytes and a wrong count are caught
        let mut bad = store.clone();
        bad.data.push(TAG_NULL);
        assert!(bad.validate(10).is_err());
        let mut bad = store.clone();
        bad.count = 3;
        assert!(bad.validate(10).is_err());
        // invalid UTF-8, unknown tags, absurd lengths
        let mut bad = store.clone();
        let at = bad.data.iter().position(|&b| b == b'h').unwrap();
        bad.data[at] = 0xff;
        assert!(bad.validate(10).is_err());
        let mut bad = store.clone();
        bad.data[0] = 0x0e;
        assert!(bad.validate(10).is_err());
        let mut bad = store.clone();
        bad.data[2..6].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(bad.validate(10).is_err());
        // whatever a single corrupted byte does, validation decides without panicking, and a
        // store it accepts decodes without panicking
        for i in 0..store.data.len() {
            for flip in [0x01u8, 0x10, 0x40, 0x80, 0xff] {
                let mut bad = store.clone();
                bad.data[i] ^= flip;
                if bad.validate(10).is_ok() {
                    let _: Vec<_> = bad.iter().collect();
                }
            }
        }
    }

    #[test]
    fn get_multi_roundtrip() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(1), PropertyValue::Int(42));
        s.insert(kid(2), PropertyValue::String("hello".into()));
        s.insert(kid(3), PropertyValue::Bool(true));

        // 2 of 3 keys in one pass
        let keys = [kid(1), kid(3)];
        let mut out = [None, None];
        s.get_multi(&keys, &mut out);
        assert_eq!(out[0], Some(PropertyValue::Int(42)));
        assert_eq!(out[1], Some(PropertyValue::Bool(true)));

        // missing key
        let keys2 = [kid(1), kid(99)];
        let mut out2 = [None, None];
        s.get_multi(&keys2, &mut out2);
        assert_eq!(out2[0], Some(PropertyValue::Int(42)));
        assert_eq!(out2[1], None);

        // no keys
        let mut out3: [Option<PropertyValue>; 0] = [];
        s.get_multi(&[], &mut out3);

        // empty store
        let empty = CompactPropertyStore::new();
        let mut out4 = [None];
        empty.get_multi(&[kid(1)], &mut out4);
        assert_eq!(out4[0], None);
    }

    #[test]
    fn get_multi_int_roundtrip() {
        let mut s = CompactPropertyStore::new();
        s.insert(kid(1), PropertyValue::Int(42));
        s.insert(kid(2), PropertyValue::Float(1.25));
        s.insert(kid(3), PropertyValue::Int(1000));
        s.insert(kid(4), PropertyValue::String("x".into()));
        s.insert(kid(5), PropertyValue::Int(i64::MAX));

        // 3 keys: 2 ints + 1 non-int
        let keys = [kid(1), kid(2), kid(3)];
        let mut vals = [0i64; 3];
        let mut valid = [false; 3];
        s.get_multi_int(&keys, &mut vals, &mut valid);
        assert!(valid[0]);
        assert_eq!(vals[0], 42);
        assert!(!valid[1]); // float, not int
        assert!(valid[2]);
        assert_eq!(vals[2], 1000);

        // all ints + missing
        let keys2 = [kid(1), kid(5), kid(99)];
        let mut vals2 = [0i64; 3];
        let mut valid2 = [false; 3];
        s.get_multi_int(&keys2, &mut vals2, &mut valid2);
        assert!(valid2[0]);
        assert_eq!(vals2[0], 42);
        assert!(valid2[1]);
        assert_eq!(vals2[1], i64::MAX);
        assert!(!valid2[2]); // missing
    }
}
