//! rkyv decoding for bytes read back from disk.
//!
//! Every archive is validated before anything reads it (offsets in bounds, enum tags and UTF-8
//! valid, and so on), so a damaged or crafted `.tvdb` file or log segment gives an error, never
//! undefined behavior. Page and record CRCs catch accidents, not files made to look valid.

use rkyv::api::high::{HighDeserializer, HighValidator};
use rkyv::bytecheck::CheckBytes;
use rkyv::rancor;
use rkyv::util::AlignedVec;

use crate::error::{Error, Result};

/// Bytes ready for rkyv: an archive needs its natural alignment, and records sit at arbitrary
/// offsets in pages and segments.
pub type Aligned = AlignedVec<16>;

/// Copies `bytes` into an [`Aligned`] buffer.
pub fn aligned(bytes: &[u8]) -> Aligned {
    let mut v = Aligned::with_capacity(bytes.len());
    v.extend_from_slice(bytes);
    v
}

/// Validates the archive in `bytes` and returns its root. `what` names it in errors.
pub fn access<'a, T>(bytes: &'a Aligned, what: &str) -> Result<&'a T::Archived>
where
    T: rkyv::Archive,
    T::Archived: for<'b> CheckBytes<HighValidator<'b, rancor::Error>>,
{
    rkyv::access::<T::Archived, rancor::Error>(bytes)
        .map_err(|e| Error::InvalidFormat(format!("{what}: {e}")))
}

/// Deserializes an archive [`access`] validated.
pub fn deserialize<T>(archived: &T::Archived, what: &str) -> Result<T>
where
    T: rkyv::Archive,
    T::Archived: rkyv::Deserialize<T, HighDeserializer<rancor::Error>>,
{
    rkyv::deserialize::<T, rancor::Error>(archived)
        .map_err(|e| Error::InvalidFormat(format!("{what}: {e}")))
}

/// Validates and deserializes a `T` from `bytes` at any alignment.
pub fn decode<T>(bytes: &[u8], what: &str) -> Result<T>
where
    T: rkyv::Archive,
    T::Archived: for<'b> CheckBytes<HighValidator<'b, rancor::Error>>
        + rkyv::Deserialize<T, HighDeserializer<rancor::Error>>,
{
    let bytes = aligned(bytes);
    deserialize(access::<T>(&bytes, what)?, what)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::node::Node;
    use crate::graph::property::PropertyValue;
    use crate::graph::types::NodeId;
    use std::collections::BTreeMap;

    #[test]
    fn round_trips_at_any_offset() {
        let mut map = BTreeMap::new();
        map.insert(
            "k".to_string(),
            PropertyValue::List(vec![PropertyValue::Int(1)]),
        );
        let value = PropertyValue::Map(map);
        let bytes = rkyv::to_bytes::<rancor::Error>(&value).unwrap();
        // misaligned on purpose: decode copies into aligned storage
        let mut shifted = vec![0u8];
        shifted.extend_from_slice(&bytes);
        assert_eq!(
            decode::<PropertyValue>(&shifted[1..], "value").unwrap(),
            value
        );
    }

    #[test]
    fn rejects_damaged_archives_without_reading_them() {
        let node = Node {
            id: NodeId(7),
            labels: Default::default(),
            properties: Default::default(),
        };
        let good = rkyv::to_bytes::<rancor::Error>(&node).unwrap().to_vec();
        assert!(decode::<Node>(&good, "node").is_ok());
        // every truncation and every single-byte corruption either decodes or is reported
        for len in 0..good.len() {
            let _ = decode::<Node>(&good[..len], "node");
        }
        for i in 0..good.len() {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut bad = good.clone();
                bad[i] ^= flip;
                let _ = decode::<Node>(&bad, "node");
            }
        }
        assert!(matches!(
            decode::<Node>(&[0xff; 64], "node"),
            Err(Error::InvalidFormat(_))
        ));
    }
}
