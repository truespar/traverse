//! Read-only decoder for the pre-0.8.3 blob layout: `bincode` 2.x with
//! `config::standard()` (little-endian, variable-length integers, no
//! size limit) as driven by `bincode::serde`.
//!
//! bincode is unmaintained (RUSTSEC-2025-0141); rather than keep it in
//! the dependency tree just to read old files, this module implements
//! the handful of encoding rules its serde integration used. They were
//! verified byte-for-byte against vectors produced by bincode 2.0.1 -
//! see `tests/golden.rs`:
//!
//! * `bool`: one byte, `0` or `1`.
//! * `u8` / `i8`: one raw byte.
//! * `u16`...`u128` and `usize` (as `u64`): *varint* - values below 251
//!   are a single byte; `251`, `252`, `253`, `254` prefix a
//!   little-endian `u16`, `u32`, `u64`, `u128`; `255` never occurs.
//! * `i16`...`i128` and `isize`: zigzag-mapped to the unsigned width,
//!   then varint.
//! * `f32` / `f64`: little-endian IEEE 754.
//! * `char`: UTF-8, 1-4 bytes.
//! * `str`, bytes, sequences, maps: varint length, then the elements.
//! * `Option`: one tag byte (`0` = `None`, `1` = `Some`) then the value.
//! * enums: the variant index as a varint `u32`, then the variant's
//!   fields.
//! * tuples, structs, newtypes, unit: the fields in order, no framing.
//!
//! The format is not self-describing, so `deserialize_any` (and with it
//! `#[serde(flatten)]`, untagged enums and `serde_json::Value`) is not
//! supported - exactly as in bincode.

use serde::de::{
    self, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess, SeqAccess, VariantAccess,
    Visitor,
};
use std::fmt;

/// Decode failure with a position-aware message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error(msg.to_string())
    }
}

/// Decode a `T` from the front of `bytes`. Returns the value and the
/// number of bytes consumed (bincode tolerated trailing bytes, so this
/// does too).
pub fn from_slice<'de, T: de::Deserialize<'de>>(bytes: &'de [u8]) -> Result<(T, usize), Error> {
    let mut d = Deserializer {
        input: bytes,
        pos: 0,
    };
    let value = T::deserialize(&mut d)?;
    Ok((value, d.pos))
}

/// Cursor over a legacy payload.
pub struct Deserializer<'de> {
    input: &'de [u8],
    pos: usize,
}

impl<'de> Deserializer<'de> {
    fn remaining(&self) -> usize {
        self.input.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'de [u8], Error> {
        if self.remaining() < n {
            return Err(Error(format!(
                "unexpected end of input at offset {}: needed {n} more byte(s), {} available",
                self.pos,
                self.remaining()
            )));
        }
        let s = &self.input[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn varint_u128(&mut self) -> Result<u128, Error> {
        let b = self.byte()?;
        Ok(match b {
            0..=250 => b as u128,
            251 => u16::from_le_bytes(self.take(2)?.try_into().unwrap()) as u128,
            252 => u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as u128,
            253 => u64::from_le_bytes(self.take(8)?.try_into().unwrap()) as u128,
            254 => u128::from_le_bytes(self.take(16)?.try_into().unwrap()),
            255 => {
                return Err(Error(format!(
                    "invalid varint prefix 0xFF at offset {}",
                    self.pos - 1
                )));
            }
        })
    }

    fn unsigned<T: TryFrom<u128>>(&mut self, what: &str) -> Result<T, Error> {
        let v = self.varint_u128()?;
        T::try_from(v).map_err(|_| Error(format!("integer {v} is out of range for {what}")))
    }

    fn signed<T: TryFrom<i128>>(&mut self, what: &str) -> Result<T, Error> {
        let u = self.varint_u128()?;
        // Inverse zigzag: 0 -> 0, 1 -> -1, 2 -> 1, 3 -> -2, ...
        let v = ((u >> 1) as i128) ^ -((u & 1) as i128);
        T::try_from(v).map_err(|_| Error(format!("integer {v} is out of range for {what}")))
    }

    fn len(&mut self) -> Result<usize, Error> {
        self.unsigned::<usize>("a length")
    }

    fn bytes(&mut self) -> Result<&'de [u8], Error> {
        let n = self.len()?;
        self.take(n)
    }

    fn str(&mut self) -> Result<&'de str, Error> {
        let start = self.pos;
        let raw = self.bytes()?;
        std::str::from_utf8(raw)
            .map_err(|e| Error(format!("invalid UTF-8 in string at offset {start}: {e}")))
    }
}

macro_rules! deserialize_unsigned {
    ($method:ident, $visit:ident, $ty:ty) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            visitor.$visit(self.unsigned::<$ty>(stringify!($ty))?)
        }
    };
}

macro_rules! deserialize_signed {
    ($method:ident, $visit:ident, $ty:ty) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            visitor.$visit(self.signed::<$ty>(stringify!($ty))?)
        }
    };
}

impl<'de> de::Deserializer<'de> for &mut Deserializer<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Error> {
        Err(Error(
            "legacy payloads are not self-describing (deserialize_any is unsupported)".into(),
        ))
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.byte()? {
            0 => visitor.visit_bool(false),
            1 => visitor.visit_bool(true),
            b => Err(Error(format!(
                "invalid boolean byte 0x{b:02x} at offset {}",
                self.pos - 1
            ))),
        }
    }

    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i8(self.byte()? as i8)
    }

    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u8(self.byte()?)
    }

    deserialize_signed!(deserialize_i16, visit_i16, i16);
    deserialize_signed!(deserialize_i32, visit_i32, i32);
    deserialize_signed!(deserialize_i64, visit_i64, i64);
    deserialize_signed!(deserialize_i128, visit_i128, i128);
    deserialize_unsigned!(deserialize_u16, visit_u16, u16);
    deserialize_unsigned!(deserialize_u32, visit_u32, u32);
    deserialize_unsigned!(deserialize_u64, visit_u64, u64);
    deserialize_unsigned!(deserialize_u128, visit_u128, u128);

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_f32(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_f64(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let start = self.pos;
        let lead = self.byte()?;
        let width = match lead {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => {
                return Err(Error(format!(
                    "invalid UTF-8 lead byte 0x{lead:02x} for char at offset {start}"
                )));
            }
        };
        self.pos = start;
        let raw = self.take(width)?;
        let s = std::str::from_utf8(raw)
            .map_err(|e| Error(format!("invalid UTF-8 char at offset {start}: {e}")))?;
        match s.chars().next() {
            Some(c) => visitor.visit_char(c),
            None => Err(Error(format!("empty char at offset {start}"))),
        }
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_borrowed_str(self.str()?)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_borrowed_str(self.str()?)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_borrowed_bytes(self.bytes()?)
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_borrowed_bytes(self.bytes()?)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.byte()? {
            0 => visitor.visit_none(),
            1 => visitor.visit_some(self),
            b => Err(Error(format!(
                "invalid Option tag 0x{b:02x} at offset {}",
                self.pos - 1
            ))),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let len = self.len()?;
        visitor.visit_seq(Counted {
            de: self,
            remaining: len,
        })
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_seq(Counted {
            de: self,
            remaining: len,
        })
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_seq(Counted {
            de: self,
            remaining: len,
        })
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let len = self.len()?;
        visitor.visit_map(Counted {
            de: self,
            remaining: len,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_seq(Counted {
            de: self,
            remaining: fields.len(),
        })
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_enum(self)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Error> {
        Err(Error(
            "legacy payloads carry no field identifiers (fields are positional)".into(),
        ))
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Error> {
        Err(Error(
            "legacy payloads are not self-describing (cannot skip an unknown value)".into(),
        ))
    }

    fn is_human_readable(&self) -> bool {
        false
    }
}

/// Fixed-count sequence / map walker (the count is either a decoded
/// length prefix or the static arity of a tuple / struct).
struct Counted<'a, 'de> {
    de: &'a mut Deserializer<'de>,
    remaining: usize,
}

impl<'de> SeqAccess<'de> for Counted<'_, 'de> {
    type Error = Error;

    fn next_element_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, Error> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        seed.deserialize(&mut *self.de).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.remaining)
    }
}

impl<'de> MapAccess<'de> for Counted<'_, 'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        seed.deserialize(&mut *self.de).map(Some)
    }

    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value, Error> {
        seed.deserialize(&mut *self.de)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.remaining)
    }
}

impl<'de> EnumAccess<'de> for &mut Deserializer<'de> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<(S::Value, Self), Error> {
        let index: u32 = self.unsigned("an enum variant index")?;
        let value = seed.deserialize(IntoDeserializer::<Error>::into_deserializer(index))?;
        Ok((value, self))
    }
}

impl<'de> VariantAccess<'de> for &mut Deserializer<'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        Ok(())
    }

    fn newtype_variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<S::Value, Error> {
        seed.deserialize(self)
    }

    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_tuple(self, len, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_tuple(self, fields.len(), visitor)
    }
}
