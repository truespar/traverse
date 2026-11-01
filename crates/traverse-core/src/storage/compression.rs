//! Feature-gated LZ4 compression for page data.

use super::archive::{Aligned, aligned};
use crate::error::{Error, Result};

/// Below this size (bytes) compression costs more than it saves.
const MIN_COMPRESS_SIZE: usize = 64;

/// LZ4 block compression. `None` without the `compression` feature or when the data is too small to
/// gain.
#[cfg(feature = "compression")]
pub fn compress(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < MIN_COMPRESS_SIZE {
        return None;
    }
    let compressed = lz4_flex::compress_prepend_size(data);
    // keep it only if smaller
    if compressed.len() < data.len() {
        Some(compressed)
    } else {
        None
    }
}

/// No-op stub without the `compression` feature.
#[cfg(not(feature = "compression"))]
pub fn compress(_data: &[u8]) -> Option<Vec<u8>> {
    None
}

/// Upper bound on what an LZ4 block of `len` bytes can expand to: a match grows by at most 255
/// bytes per input byte. A size prefix above it is corrupt, and refusing it keeps a crafted record
/// from forcing a huge allocation.
#[cfg(feature = "compression")]
fn max_decompressed(len: usize) -> usize {
    len.saturating_mul(255).saturating_add(64)
}

/// Decompresses a size-prefixed LZ4 block, checking the claimed size against what the block can
/// produce. The output is aligned for rkyv.
#[cfg(feature = "compression")]
pub fn decompress(data: &[u8]) -> Result<Aligned> {
    let (size, block) = lz4_flex::block::uncompressed_size(data)
        .map_err(|e| Error::Decompression(e.to_string()))?;
    if size > max_decompressed(block.len()) {
        return Err(Error::Decompression(format!(
            "record claims {size} bytes, more than {} compressed bytes can hold",
            block.len()
        )));
    }
    let mut out = Aligned::with_capacity(size);
    out.resize(size, 0);
    let written = lz4_flex::block::decompress_into(block, &mut out)
        .map_err(|e| Error::Decompression(e.to_string()))?;
    if written != size {
        return Err(Error::Decompression(format!(
            "record decompressed to {written} bytes, expected {size}"
        )));
    }
    Ok(out)
}

/// Stub without the `compression` feature.
#[cfg(not(feature = "compression"))]
pub fn decompress(_data: &[u8]) -> Result<Aligned> {
    Err(Error::Decompression(
        "compression feature not enabled".into(),
    ))
}

/// Worth compressing?
fn should_compress(data: &[u8]) -> bool {
    cfg!(feature = "compression") && data.len() >= MIN_COMPRESS_SIZE
}

/// Record flag: raw.
const RECORD_FLAG_RAW: u8 = 0x00;
/// Record flag: LZ4.
const RECORD_FLAG_LZ4: u8 = 0x01;
/// Record flag of a slot's stub for a record continued on overflow pages (`storage::overflow`);
/// never one [`encode_record`] makes.
pub(crate) const RECORD_FLAG_OVERFLOW: u8 = 0x02;

/// Prefixes a record with a 1-byte flag: `0x01` + LZ4 data when that's smaller, else `0x00` + raw
/// data.
pub fn encode_record(data: &[u8]) -> Vec<u8> {
    if should_compress(data)
        && let Some(compressed) = compress(data)
    {
        let mut result = Vec::with_capacity(1 + compressed.len());
        result.push(RECORD_FLAG_LZ4);
        result.extend_from_slice(&compressed);
        return result;
    }
    let mut result = Vec::with_capacity(1 + data.len());
    result.push(RECORD_FLAG_RAW);
    result.extend_from_slice(data);
    result
}

/// Decodes a record by its 1-byte flag, decompressing if needed.
pub fn decode_record(data: &[u8]) -> Result<Aligned> {
    match data.first() {
        None => Err(Error::Decompression("empty record".into())),
        Some(&RECORD_FLAG_RAW) => Ok(aligned(&data[1..])),
        Some(&RECORD_FLAG_LZ4) => decompress(&data[1..]),
        Some(&RECORD_FLAG_OVERFLOW) => Err(Error::Decompression(
            "record continues on overflow pages; read it through them".into(),
        )),
        Some(flag) => Err(Error::Decompression(format!(
            "unknown record flag: {flag:#04x}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_compressible_data() {
        // highly compressible: repeated bytes
        let data = vec![0x42u8; 1024];
        if let Some(compressed) = compress(&data) {
            let decompressed = decompress(&compressed).unwrap();
            assert_eq!(&decompressed[..], &data[..]);
            assert!(compressed.len() < data.len());
        }
    }

    #[test]
    fn small_data_not_compressed() {
        let data = vec![0x42u8; 10];
        assert!(compress(&data).is_none());
    }

    #[test]
    fn empty_data_not_compressed() {
        let data: Vec<u8> = Vec::new();
        assert!(compress(&data).is_none());
    }

    #[test]
    fn encode_decode_small_record() {
        let data = b"hello";
        let encoded = encode_record(data);
        assert_eq!(encoded[0], RECORD_FLAG_RAW);
        let decoded = decode_record(&encoded).unwrap();
        assert_eq!(&decoded[..], &data[..]);
    }

    #[test]
    fn encode_decode_compressible_record() {
        let data = vec![0x42u8; 1024];
        let encoded = encode_record(&data);
        let decoded = decode_record(&encoded).unwrap();
        assert_eq!(&decoded[..], &data[..]);
        if cfg!(feature = "compression") {
            assert_eq!(encoded[0], RECORD_FLAG_LZ4);
            assert!(encoded.len() < data.len());
        }
    }

    #[test]
    fn decode_empty_record_fails() {
        assert!(decode_record(&[]).is_err());
    }

    #[test]
    fn decode_unknown_flag_fails() {
        assert!(decode_record(&[0xFF, 0x01]).is_err());
    }
}
