//! Framed, checksummed, versioned container for Traverse's small
//! persisted blobs: `.tvmodel` files, the ML pipeline catalog and the
//! auth catalog stored in a tvdb's metadata page.
//!
//! # Layout
//!
//! ```text
//! offset  size  field
//! 0       3     magic    0xFF 'T' 'V'
//! 3       1     kind     what the payload is ([`Kind`]) - a `.tvmodel`
//!                        can never be loaded as an auth catalog
//! 4       1     version  payload encoding; 1 = postcard
//! 5       n     payload  postcard encoding of the value's serde data model
//! 5+n     4     crc32    CRC-32 (IEEE, little-endian) over offsets 0..5+n
//! ```
//!
//! The payload is [postcard](https://docs.rs/postcard) - a compact,
//! stable, actively maintained wire format for the serde data model.
//! The frame adds what a bare serde encoding lacks: a way to tell what a
//! file is before parsing it, a version so the payload encoding can
//! change later, and a checksum so truncation or corruption is reported
//! as such instead of as a puzzling parse error (or, worse, a silently
//! wrong value inside an opaque byte field).
//!
//! # Legacy payloads
//!
//! Before 0.8.3 both blobs were raw `bincode` 2 (`config::standard()`)
//! with no header. [`decode`] recognises those by the absence of the
//! magic - a bincode-2 stream for these types starts with a length
//! varint, whose first byte is never `0xFF` - and reads them with the
//! built-in [`legacy`] decoder, so existing files and catalogs load
//! unchanged. They are rewritten in the framed layout the next time
//! they are saved.

#![forbid(unsafe_code)]

pub mod legacy;

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt;

/// First three bytes of every framed blob.
pub const MAGIC: [u8; 3] = [0xFF, b'T', b'V'];
/// Magic + kind + version.
pub const HEADER_LEN: usize = 5;
/// CRC-32 trailer.
pub const TRAILER_LEN: usize = 4;
/// Payload encoding version written by this build.
pub const FORMAT_VERSION: u8 = 1;

/// What a framed blob contains. The byte value is part of the on-disk
/// format - never renumber, only append.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    /// A trained ML model (`.tvmodel`).
    Model = b'M',
    /// The user/role catalog kept in a tvdb's metadata page.
    AuthCatalog = b'A',
    /// The ML pipeline catalog (`ml-pipelines.tvpipelines`).
    Pipelines = b'P',
}

impl Kind {
    /// Map a kind byte back to a [`Kind`].
    pub fn from_byte(b: u8) -> Option<Kind> {
        match b {
            b'M' => Some(Kind::Model),
            b'A' => Some(Kind::AuthCatalog),
            b'P' => Some(Kind::Pipelines),
            _ => None,
        }
    }

    /// Human-readable name for error messages.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Model => "model (.tvmodel)",
            Kind::AuthCatalog => "auth catalog",
            Kind::Pipelines => "ML pipeline catalog",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Everything that can go wrong while framing or unframing a blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The value could not be serialised.
    Encode(String),
    /// Starts with [`MAGIC`] but is shorter than header + trailer.
    Truncated { len: usize },
    /// Starts with `0xFF` but not with the full magic, so it is neither a
    /// framed blob nor a legacy payload.
    BadMagic,
    /// The frame holds a different [`Kind`] than the caller asked for.
    WrongKind { expected: Kind, found: u8 },
    /// The frame was written by a newer Traverse.
    UnsupportedVersion { kind: Kind, version: u8 },
    /// The stored CRC-32 does not match the bytes.
    Checksum { stored: u32, computed: u32 },
    /// The payload did not parse as the requested type.
    Payload(String),
    /// Not framed, and the legacy bincode fallback failed as well.
    Legacy(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Encode(e) => write!(f, "encoding failed: {e}"),
            Error::Truncated { len } => write!(
                f,
                "truncated container: {len} bytes, need at least {}",
                HEADER_LEN + TRAILER_LEN
            ),
            Error::BadMagic => f.write_str("not a Traverse container (bad magic)"),
            Error::WrongKind { expected, found } => match Kind::from_byte(*found) {
                Some(k) => write!(f, "container holds a {k} but a {expected} was expected"),
                None => write!(
                    f,
                    "container kind byte 0x{found:02x} is unknown; expected a {expected}"
                ),
            },
            Error::UnsupportedVersion { kind, version } => write!(
                f,
                "{kind} container version {version} is newer than this build supports (max {FORMAT_VERSION}); upgrade Traverse"
            ),
            Error::Checksum { stored, computed } => write!(
                f,
                "CRC-32 mismatch (stored {stored:08x}, computed {computed:08x}): the data is corrupt or truncated"
            ),
            Error::Payload(e) => write!(f, "payload does not decode: {e}"),
            Error::Legacy(e) => write!(f, "legacy (pre-0.8.3) payload does not decode: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// True if `bytes` carries the frame header (as opposed to being a
/// legacy bincode payload or something else entirely).
pub fn is_framed(bytes: &[u8]) -> bool {
    bytes.starts_with(&MAGIC)
}

/// The [`Kind`] recorded in a framed blob's header, if it has one.
pub fn kind_of(bytes: &[u8]) -> Option<Kind> {
    if is_framed(bytes) {
        bytes.get(3).copied().and_then(Kind::from_byte)
    } else {
        None
    }
}

/// Frame `value` as a `kind` blob: header, postcard payload, CRC-32.
pub fn encode<T: Serialize + ?Sized>(kind: Kind, value: &T) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&MAGIC);
    out.push(kind as u8);
    out.push(FORMAT_VERSION);
    let mut out = postcard::to_extend(value, out).map_err(|e| Error::Encode(e.to_string()))?;
    let crc = crc32fast::hash(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    Ok(out)
}

/// Read a `kind` blob back. Framed input is validated (kind, version,
/// checksum) before the payload is parsed; input without the frame
/// header is read as a legacy bincode-2 payload.
pub fn decode<T: DeserializeOwned>(kind: Kind, bytes: &[u8]) -> Result<T, Error> {
    if !is_framed(bytes) {
        if bytes.first() == Some(&MAGIC[0]) {
            return Err(Error::BadMagic);
        }
        return legacy::from_slice(bytes)
            .map(|(value, _consumed)| value)
            .map_err(|e| Error::Legacy(e.to_string()));
    }
    if bytes.len() < HEADER_LEN + TRAILER_LEN {
        return Err(Error::Truncated { len: bytes.len() });
    }
    let found = bytes[3];
    if found != kind as u8 {
        return Err(Error::WrongKind {
            expected: kind,
            found,
        });
    }
    let version = bytes[4];
    if version != FORMAT_VERSION {
        return Err(Error::UnsupportedVersion { kind, version });
    }
    let (body, trailer) = bytes.split_at(bytes.len() - TRAILER_LEN);
    let stored = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let computed = crc32fast::hash(body);
    if stored != computed {
        return Err(Error::Checksum { stored, computed });
    }
    let (value, rest) = postcard::take_from_bytes::<T>(&body[HEADER_LEN..])
        .map_err(|e| Error::Payload(e.to_string()))?;
    if !rest.is_empty() {
        return Err(Error::Payload(format!(
            "{} trailing byte(s) after the payload",
            rest.len()
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::collections::HashMap;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Blob {
        name: String,
        weights: Vec<u8>,
        metrics: HashMap<String, f32>,
        tag: Option<u64>,
    }

    fn sample() -> Blob {
        Blob {
            name: "gcn-2layer".into(),
            weights: (0..=255).collect(),
            metrics: HashMap::from([("loss".to_string(), 0.125f32), ("acc".to_string(), 0.9)]),
            tag: Some(1 << 40),
        }
    }

    #[test]
    fn round_trip() {
        let bytes = encode(Kind::Model, &sample()).unwrap();
        assert!(is_framed(&bytes));
        assert_eq!(kind_of(&bytes), Some(Kind::Model));
        assert_eq!(&bytes[..3], &MAGIC);
        assert_eq!(bytes[3], b'M');
        assert_eq!(bytes[4], FORMAT_VERSION);
        let back: Blob = decode(Kind::Model, &bytes).unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn wrong_kind_is_rejected_before_parsing() {
        let bytes = encode(Kind::AuthCatalog, &sample()).unwrap();
        let err = decode::<Blob>(Kind::Model, &bytes).unwrap_err();
        assert_eq!(
            err,
            Error::WrongKind {
                expected: Kind::Model,
                found: b'A'
            }
        );
        assert!(err.to_string().contains("auth catalog"), "{err}");
    }

    #[test]
    fn newer_version_is_rejected() {
        let mut bytes = encode(Kind::Model, &sample()).unwrap();
        bytes[4] = FORMAT_VERSION + 1;
        // Re-seal so only the version check can fire.
        let n = bytes.len() - TRAILER_LEN;
        let crc = crc32fast::hash(&bytes[..n]);
        bytes[n..].copy_from_slice(&crc.to_le_bytes());
        let err = decode::<Blob>(Kind::Model, &bytes).unwrap_err();
        assert!(
            matches!(err, Error::UnsupportedVersion { version, .. } if version == FORMAT_VERSION + 1),
            "{err}"
        );
    }

    #[test]
    fn corruption_anywhere_fails_the_checksum() {
        let clean = encode(Kind::Model, &sample()).unwrap();
        for i in HEADER_LEN..clean.len() - TRAILER_LEN {
            let mut bytes = clean.clone();
            bytes[i] ^= 0x40;
            let err = decode::<Blob>(Kind::Model, &bytes).unwrap_err();
            assert!(matches!(err, Error::Checksum { .. }), "offset {i}: {err}");
        }
    }

    #[test]
    fn truncation_is_reported() {
        let clean = encode(Kind::Model, &sample()).unwrap();
        let err = decode::<Blob>(Kind::Model, &clean[..HEADER_LEN + 2]).unwrap_err();
        assert_eq!(
            err,
            Error::Truncated {
                len: HEADER_LEN + 2
            }
        );
        let err = decode::<Blob>(Kind::Model, &clean[..clean.len() - 1]).unwrap_err();
        assert!(matches!(err, Error::Checksum { .. }), "{err}");
    }

    #[test]
    fn trailing_bytes_inside_the_frame_are_rejected() {
        let mut bytes = encode(Kind::Model, &sample()).unwrap();
        let n = bytes.len() - TRAILER_LEN;
        bytes.truncate(n);
        bytes.push(0);
        let crc = crc32fast::hash(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        let err = decode::<Blob>(Kind::Model, &bytes).unwrap_err();
        assert!(matches!(err, Error::Payload(_)), "{err}");
    }

    #[test]
    fn bad_magic_is_not_mistaken_for_legacy() {
        let err = decode::<Blob>(Kind::Model, &[0xFF, b'T', b'X', 1, 2, 3, 4, 5, 6]).unwrap_err();
        assert_eq!(err, Error::BadMagic);
    }

    #[test]
    fn empty_input_is_an_error_not_a_panic() {
        let err = decode::<Blob>(Kind::Model, &[]).unwrap_err();
        assert!(matches!(err, Error::Legacy(_)), "{err}");
    }

    #[test]
    fn unframed_input_takes_the_legacy_path() {
        // bincode 2 `standard()` encoding of `Blob { name: "x", weights: [7],
        // metrics: {}, tag: None }` (length varints, Option tag byte).
        let legacy = [1, b'x', 1, 7, 0, 0];
        let back: Blob = decode(Kind::Model, &legacy).unwrap();
        assert_eq!(
            back,
            Blob {
                name: "x".into(),
                weights: vec![7],
                metrics: HashMap::new(),
                tag: None
            }
        );
    }
}
