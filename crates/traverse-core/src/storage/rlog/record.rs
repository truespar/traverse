//! Redo-log record encoding.
//!
//! Record = 16-byte header + rkyv payload + zero pad to 8 bytes + CRC32 trailer.
//!
//! # Layout
//!
//! ```text
//! offset  size  field
//! 0       1     kind         RecordKind tag
//! 1       1     flags        reserved (0)
//! 2       4     payload_len  u32 LE  (length of payload bytes, not including pad or CRC)
//! 6       2     reserved     0
//! 8       8     lsn          u64 LE  (segment-relative byte offset of this record's first byte)
//! 16      N     payload      rkyv-serialized Op (for DATA) or fixed 28-byte struct (for COMMIT)
//! 16+N    p     zero pad     0..7 bytes so the record ends on an 8-byte boundary
//! 20+N+p  4     crc32        u32 LE  (CRC32 of bytes [0 .. 16+N+p])
//! ```
//!
//! Size = `16 + N + p + 4`, always a multiple of 8.
//!
//! # Why this layout
//!
//! - 8-byte aligned header: a torn write inside a sector can't split it; at worst
//!   the payload truncates and the CRC catches it.
//! - Per-record CRC pinpoints where corruption starts.
//! - Length-prefixed, no framing: encoder stays allocation-free; group commit is
//!   one `pwritev`/`pwrite` per batch.
//! - rkyv payloads: same serializer the page format uses for `Node`/`Edge`, so one encoder.
//!
//! # CRC
//!
//! `crc32fast` (IEEE, CLMUL on x86-64, Crypto Extensions on aarch64), same as the
//! `.tvdb` header and page WAL. For 50-500 byte payloads it catches all 1- and
//! 2-bit errors, all bursts <= 32 bits and >= 99.999% of longer ones: as good as
//! CRC32C for torn writes and bit flips.

use crate::error::{Error, Result};

/// Size of the fixed record header in bytes.
pub const RECORD_HEADER_SIZE: usize = 16;

/// Size of the per-record CRC trailer in bytes.
pub const RECORD_CRC_SIZE: usize = 4;

/// Alignment of every record's start offset (and total size) on disk.
pub const RECORD_ALIGNMENT: usize = 8;

/// Size of the fixed COMMIT-record payload in bytes.
///
/// `[prev_commit_lsn:u64][tx_id:u64][commit_ts:u64][batch_crc:u32]` = 28 bytes.
pub const COMMIT_PAYLOAD_SIZE: usize = 28;

/// Max payload per record: what the 32-bit length field holds. A transaction is one record per
/// operation sharing one trailing COMMIT, so this bounds a single value (a property, a node with
/// its properties), not a transaction. Recovery checks every claimed length against the bytes the
/// segment holds before reading it.
pub const MAX_PAYLOAD_SIZE: usize = u32::MAX as usize;

/// Tag byte identifying what's in the record payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordKind {
    /// rkyv-serialized [`Op`]. May be one of several within the same transaction.
    Data = 1,
    /// Fixed 28-byte payload closing a group of `Data` records. Carries the batch
    /// CRC, so torn tails are easy to detect.
    Commit = 2,
    /// Pads to segment end so the next segment starts at offset 0. Zero-byte
    /// payload; reader skips it.
    NoopPad = 3,
}

impl RecordKind {
    fn from_u8(b: u8) -> Result<Self> {
        match b {
            1 => Ok(RecordKind::Data),
            2 => Ok(RecordKind::Commit),
            3 => Ok(RecordKind::NoopPad),
            _ => Err(Error::InvalidFormat(format!("rlog: bad record kind {}", b))),
        }
    }
}

/// Decoded record header. The payload bytes live separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    /// Record kind tag.
    pub kind: RecordKind,
    /// Length of the payload (not including pad or CRC).
    pub payload_len: u32,
    /// Segment-relative byte offset of this record's first byte.
    pub lsn: u64,
}

impl RecordHeader {
    /// Serializes the header into a 16-byte buffer.
    pub fn to_bytes(&self) -> [u8; RECORD_HEADER_SIZE] {
        let mut buf = [0u8; RECORD_HEADER_SIZE];
        buf[0] = self.kind as u8;
        buf[1] = 0; // flags reserved
        buf[2..6].copy_from_slice(&self.payload_len.to_le_bytes());
        buf[6..8].copy_from_slice(&0u16.to_le_bytes()); // reserved
        buf[8..16].copy_from_slice(&self.lsn.to_le_bytes());
        buf
    }

    /// Deserializes a header. Doesn't check the segment length: caller makes sure
    /// `payload_len + pad + CRC` stays inside the readable region.
    pub fn from_bytes(buf: &[u8; RECORD_HEADER_SIZE]) -> Result<Self> {
        let kind = RecordKind::from_u8(buf[0])?;
        // buf[1] flags: reserved, ignored
        // any length the field holds is valid; the caller bounds it by the bytes there are
        let payload_len = u32::from_le_bytes(buf[2..6].try_into().unwrap());
        // buf[6..8] reserved: ignored, may be nonzero in future versions
        let lsn = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        Ok(Self {
            kind,
            payload_len,
            lsn,
        })
    }
}

/// Trailer of every [`RecordKind::Commit`] record, a fixed 28-byte LE struct.
///
/// `batch_crc` covers every byte after the previous COMMIT, i.e. this
/// transaction's DATA records. A torn `pwrite` either never got the COMMIT to
/// disk or left a mismatched `batch_crc`; recovery truncates at the prior COMMIT
/// in both cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitPayload {
    /// Segment-relative LSN of the previous COMMIT record (0 if first in segment).
    pub prev_commit_lsn: u64,
    /// Transaction ID (writer-assigned, monotonic per-process).
    pub tx_id: u64,
    /// Commit timestamp (encoded `HybridTimestamp`).
    pub commit_ts: u64,
    /// CRC32 of the record bytes between the previous COMMIT and this one (this batch's DATA
    /// records).
    pub batch_crc: u32,
}

impl CommitPayload {
    /// Serializes into a 28-byte buffer.
    pub fn to_bytes(&self) -> [u8; COMMIT_PAYLOAD_SIZE] {
        let mut buf = [0u8; COMMIT_PAYLOAD_SIZE];
        buf[0..8].copy_from_slice(&self.prev_commit_lsn.to_le_bytes());
        buf[8..16].copy_from_slice(&self.tx_id.to_le_bytes());
        buf[16..24].copy_from_slice(&self.commit_ts.to_le_bytes());
        buf[24..28].copy_from_slice(&self.batch_crc.to_le_bytes());
        buf
    }

    /// Deserializes from a 28-byte buffer.
    pub fn from_bytes(buf: &[u8; COMMIT_PAYLOAD_SIZE]) -> Self {
        Self {
            prev_commit_lsn: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            tx_id: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            commit_ts: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            batch_crc: u32::from_le_bytes(buf[24..28].try_into().unwrap()),
        }
    }
}

pub use op_def::{ArchivedOp, Op, OpResolver};

// rkyv's `Archive` derive generates `ArchivedOp` and `OpResolver` next to `Op`.
// Resolver fields can't take docs or pass-through attributes, so missing_docs
// is allowed here and `Op` turns it back on for itself.
#[allow(missing_docs)]
mod op_def {
    use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};

    use crate::graph::property::PropertyValue;

    /// Logical operation in a redo-log record.
    ///
    /// Carries strings, not interned IDs: the `StringTable` is empty after a crash
    /// and recovery re-interns through the normal `Graph` mutation APIs. Costs
    /// ~10-30 bytes on a 100-500 byte payload, removes ID-ordering hazards.
    #[warn(missing_docs)]
    #[derive(Debug, Clone, PartialEq, Archive, RkyvSerialize, RkyvDeserialize)]
    pub enum Op {
        /// Create a node with the given ID and initial labels/properties.
        CreateNode {
            /// Writer-assigned node ID.
            node_id: u64,
            /// Label names in insertion order.
            labels: Vec<String>,
            /// Properties as key-value pairs.
            properties: Vec<(String, PropertyValue)>,
        },
        /// Delete a node; incident edges cascade as in `Graph`.
        DeleteNode {
            /// Node ID to delete.
            node_id: u64,
        },
        /// Add a label to a node. Idempotent on replay.
        AddLabel {
            /// Node to update.
            node_id: u64,
            /// Label to add.
            label: String,
        },
        /// Remove a label from an existing node. Replay is idempotent.
        RemoveLabel {
            /// Node to update.
            node_id: u64,
            /// Label to remove.
            label: String,
        },
        /// Replace the full property map of an existing node.
        SetNodeProperties {
            /// Node to update.
            node_id: u64,
            /// New property map (full replacement).
            properties: Vec<(String, PropertyValue)>,
        },
        /// Create an edge with the given ID, endpoints, type, and properties.
        CreateEdge {
            /// Writer-assigned edge ID.
            edge_id: u64,
            /// Source node.
            src: u64,
            /// Destination node.
            dst: u64,
            /// Edge type name.
            edge_type: String,
            /// Properties as key-value pairs.
            properties: Vec<(String, PropertyValue)>,
        },
        /// Delete an edge.
        DeleteEdge {
            /// Edge ID.
            edge_id: u64,
        },
        /// Replace the full property map of an existing edge.
        SetEdgeProperties {
            /// Edge to update.
            edge_id: u64,
            /// New property map.
            properties: Vec<(String, PropertyValue)>,
        },
        /// Create a property index on `label.property`.
        CreateIndex {
            /// Index name (uniqueness key).
            name: String,
            /// Label scope.
            label: String,
            /// Property key.
            property: String,
        },
        /// Drop a property index by name.
        DropIndex {
            /// Index name.
            name: String,
        },
        /// Create a constraint on `label.property`.
        CreateConstraint {
            /// Constraint name (uniqueness key).
            name: String,
            /// Label scope.
            label: String,
            /// Property key.
            property: String,
            /// Constraint kind: 1=unique, 2=exists.
            kind: u8,
        },
        /// Drop a constraint by name.
        DropConstraint {
            /// Constraint name.
            name: String,
        },
        /// Create an edge index on `edge_type[.property]`. `None` property indexes edge existence
        /// for the type.
        CreateEdgeIndex {
            /// Index name.
            name: String,
            /// Edge type scope.
            edge_type: String,
            /// Optional property key. `None` for a type-only index.
            property: Option<String>,
        },
        /// Drop an edge index by name.
        DropEdgeIndex {
            /// Index name.
            name: String,
        },
        /// Create a vector (HNSW) index on `label.property`. Proximity graph isn't
        /// logged: replay rebuilds it from node vectors, which precede the index create
        /// in commit order.
        CreateVectorIndex {
            /// Index name (uniqueness key).
            name: String,
            /// Label scope.
            label: String,
            /// Vector property key.
            property: String,
            /// Vector dimensionality.
            dimensions: u64,
            /// Similarity function (`"cosine"` / `"euclidean"`).
            similarity: String,
            /// HNSW max neighbors per node (`m`).
            m: u64,
            /// HNSW construction beam width.
            ef_construction: u64,
            /// HNSW default query beam width.
            ef_search: u64,
            /// Level-assignment RNG seed (reproducible rebuild).
            seed: u64,
            /// int8 scalar quantization enabled.
            quantize: bool,
        },
        /// Drop a vector index by name.
        DropVectorIndex {
            /// Index name.
            name: String,
        },
        /// Create a BM25 full-text index. Postings aren't logged: replay rebuilds them
        /// from the already-replayed node string properties.
        CreateFulltextIndex {
            /// Index name (uniqueness key).
            name: String,
            /// Label scope (one or more).
            labels: Vec<String>,
            /// Indexed string property keys (one or more).
            properties: Vec<String>,
            /// Analyzer name (`standard` / `english` / `keyword` / ...).
            analyzer: String,
        },
        /// Drop a full-text index by name.
        DropFulltextIndex {
            /// Index name.
            name: String,
        },
        /// Create an R-tree point index. Tree isn't logged: replay rebuilds it from the
        /// already-replayed node point properties.
        CreatePointIndex {
            /// Index name (uniqueness key).
            name: String,
            /// Indexed node label.
            label: String,
            /// Indexed point property key.
            property: String,
        },
        /// Drop a point index by name.
        DropPointIndex {
            /// Index name.
            name: String,
        },
        /// Create a trigram text index. Postings aren't logged: replay rebuilds them
        /// from the already-replayed node string properties.
        CreateTextIndex {
            /// Index name (uniqueness key).
            name: String,
            /// Indexed node label.
            label: String,
            /// Indexed string property key.
            property: String,
        },
        /// Drop a text index by name.
        DropTextIndex {
            /// Index name.
            name: String,
        },
    }
}

impl Op {
    /// Serializes the op via rkyv to a fresh `Vec<u8>`.
    pub fn to_payload_bytes(&self) -> Result<Vec<u8>> {
        rkyv::to_bytes::<rkyv::rancor::Error>(self)
            .map(|b| b.to_vec())
            .map_err(|e| Error::Serialization(format!("rlog: encode op: {}", e)))
    }

    /// Deserializes an op from exactly the payload bytes (no header, CRC or pad), validating the
    /// archive first.
    pub fn from_payload_bytes(bytes: &[u8]) -> Result<Self> {
        crate::storage::archive::decode(bytes, "rlog op")
    }
}

/// On-disk size of a record: header + payload + pad to 8 bytes + CRC.
pub const fn record_size(payload_len: usize) -> usize {
    let raw = RECORD_HEADER_SIZE + payload_len + RECORD_CRC_SIZE;
    // round up to RECORD_ALIGNMENT

    raw.div_ceil(RECORD_ALIGNMENT) * RECORD_ALIGNMENT
}

/// Pad bytes needed to reach 8-byte alignment after `header + payload + CRC`.
pub const fn record_padding(payload_len: usize) -> usize {
    record_size(payload_len) - (RECORD_HEADER_SIZE + payload_len + RECORD_CRC_SIZE)
}

/// Encodes a full record (header + payload + pad + CRC) into `out`; returns bytes written.
///
/// `lsn` is the segment-relative offset the writer assigned for this record.
pub fn encode_record(
    out: &mut Vec<u8>,
    kind: RecordKind,
    lsn: u64,
    payload: &[u8],
) -> Result<usize> {
    if payload.len() > MAX_PAYLOAD_SIZE {
        return Err(Error::InvalidFormat(format!(
            "rlog: payload {} exceeds max {}",
            payload.len(),
            MAX_PAYLOAD_SIZE
        )));
    }
    let header = RecordHeader {
        kind,
        payload_len: payload.len() as u32,
        lsn,
    };
    let start = out.len();
    out.extend_from_slice(&header.to_bytes());
    out.extend_from_slice(payload);
    let pad = record_padding(payload.len());
    for _ in 0..pad {
        out.push(0);
    }
    // CRC over header + payload + pad
    let crc = crc32fast::hash(&out[start..]);
    out.extend_from_slice(&crc.to_le_bytes());
    Ok(out.len() - start)
}

/// Checks a record's CRC; returns its header and payload slice.
///
/// `record_bytes` must be exactly `record_size(payload_len)` bytes.
pub fn decode_record(record_bytes: &[u8]) -> Result<(RecordHeader, &[u8])> {
    if record_bytes.len() < RECORD_HEADER_SIZE + RECORD_CRC_SIZE {
        return Err(Error::InvalidFormat(format!(
            "rlog: record too short ({} bytes)",
            record_bytes.len()
        )));
    }
    let header_buf: &[u8; RECORD_HEADER_SIZE] =
        record_bytes[..RECORD_HEADER_SIZE].try_into().unwrap();
    let header = RecordHeader::from_bytes(header_buf)?;

    let expected_size = record_size(header.payload_len as usize);
    if record_bytes.len() != expected_size {
        return Err(Error::InvalidFormat(format!(
            "rlog: record size mismatch - header says payload_len={} (expected total {}), got {}",
            header.payload_len,
            expected_size,
            record_bytes.len()
        )));
    }

    let (data, crc_buf) = record_bytes.split_at(record_bytes.len() - RECORD_CRC_SIZE);
    let stored_crc = u32::from_le_bytes(crc_buf.try_into().unwrap());
    let computed_crc = crc32fast::hash(data);
    if stored_crc != computed_crc {
        return Err(Error::HeaderChecksumMismatch);
    }

    let payload_start = RECORD_HEADER_SIZE;
    let payload_end = payload_start + header.payload_len as usize;
    let payload = &record_bytes[payload_start..payload_end];
    Ok((header, payload))
}

/// Parses the next record in `buf`: `(header, payload, size)`; caller advances by `size`.
/// `Ok(None)` when `buf` is too short for a full record. Tests use it to walk written bytes.
#[cfg(test)]
pub(crate) fn peek_next_record(buf: &[u8]) -> Result<Option<(RecordHeader, &[u8], usize)>> {
    if buf.len() < RECORD_HEADER_SIZE + RECORD_CRC_SIZE {
        return Ok(None);
    }
    let header_buf: &[u8; RECORD_HEADER_SIZE] = buf[..RECORD_HEADER_SIZE].try_into().unwrap();
    let header = RecordHeader::from_bytes(header_buf)?;
    let total = record_size(header.payload_len as usize);
    if buf.len() < total {
        return Ok(None);
    }
    let (header, payload) = decode_record(&buf[..total])?;
    Ok(Some((header, payload, total)))
}

/// Test helper: encode an Op at the next aligned position; returns bytes written.
#[cfg(test)]
pub(crate) fn encode_op_for_test(out: &mut Vec<u8>, lsn: u64, op: &Op) -> Result<usize> {
    let payload = op.to_payload_bytes()?;
    encode_record(out, RecordKind::Data, lsn, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::property::PropertyValue;

    #[test]
    fn record_header_round_trip() {
        let h = RecordHeader {
            kind: RecordKind::Data,
            payload_len: 42,
            lsn: 0xCAFEBABE_DEADBEEF,
        };
        let bytes = h.to_bytes();
        let restored = RecordHeader::from_bytes(&bytes).unwrap();
        assert_eq!(restored, h);
    }

    #[test]
    fn record_size_is_8_byte_aligned() {
        for payload_len in 0..64 {
            let size = record_size(payload_len);
            assert_eq!(size % 8, 0, "payload_len={}", payload_len);
            assert!(size >= RECORD_HEADER_SIZE + payload_len + RECORD_CRC_SIZE);
            assert!(size < RECORD_HEADER_SIZE + payload_len + RECORD_CRC_SIZE + 8);
        }
    }

    #[test]
    fn commit_payload_round_trip() {
        let c = CommitPayload {
            prev_commit_lsn: 1024,
            tx_id: 7,
            commit_ts: 0x1234_5678_9abc_def0,
            batch_crc: 0xDEADBEEF,
        };
        let bytes = c.to_bytes();
        let restored = CommitPayload::from_bytes(&bytes);
        assert_eq!(restored, c);
    }

    #[test]
    fn op_round_trip_create_node() {
        let op = Op::CreateNode {
            node_id: 42,
            labels: vec!["Person".to_string(), "Employee".to_string()],
            properties: vec![
                (
                    "name".to_string(),
                    PropertyValue::String("Alice".to_string()),
                ),
                ("age".to_string(), PropertyValue::Int(30)),
            ],
        };
        let bytes = op.to_payload_bytes().unwrap();
        let restored = Op::from_payload_bytes(&bytes).unwrap();
        assert_eq!(restored, op);
    }

    #[test]
    fn op_round_trip_create_edge() {
        let op = Op::CreateEdge {
            edge_id: 99,
            src: 1,
            dst: 2,
            edge_type: "Friend".to_string(),
            properties: vec![("since".to_string(), PropertyValue::Int(2026))],
        };
        let bytes = op.to_payload_bytes().unwrap();
        let restored = Op::from_payload_bytes(&bytes).unwrap();
        assert_eq!(restored, op);
    }

    #[test]
    fn encode_decode_record_round_trip() {
        let op = Op::CreateNode {
            node_id: 1,
            labels: vec!["N".to_string()],
            properties: vec![],
        };
        let payload = op.to_payload_bytes().unwrap();

        let mut buf = Vec::new();
        let written = encode_record(&mut buf, RecordKind::Data, 0, &payload).unwrap();
        assert_eq!(written, buf.len());
        assert_eq!(buf.len() % RECORD_ALIGNMENT, 0);

        let (header, payload_back) = decode_record(&buf).unwrap();
        assert_eq!(header.kind, RecordKind::Data);
        assert_eq!(header.payload_len as usize, payload.len());
        assert_eq!(header.lsn, 0);
        assert_eq!(payload_back, &payload[..]);

        let restored = Op::from_payload_bytes(payload_back).unwrap();
        assert_eq!(restored, op);
    }

    #[test]
    fn decode_detects_bit_flip_in_payload() {
        let mut buf = Vec::new();
        encode_record(&mut buf, RecordKind::Data, 0, &[1, 2, 3, 4, 5]).unwrap();
        // flip a payload bit (offset 17, past the header)
        buf[17] ^= 0x01;
        let err = decode_record(&buf).unwrap_err();
        assert!(
            matches!(err, Error::HeaderChecksumMismatch),
            "expected CRC error, got {:?}",
            err
        );
    }

    #[test]
    fn decode_detects_bit_flip_in_header() {
        let mut buf = Vec::new();
        encode_record(&mut buf, RecordKind::Data, 12345, &[1, 2, 3, 4, 5]).unwrap();
        // Flip a bit in the LSN field
        buf[10] ^= 0x80;
        let err = decode_record(&buf).unwrap_err();
        assert!(
            matches!(err, Error::HeaderChecksumMismatch),
            "expected CRC error, got {:?}",
            err
        );
    }

    #[test]
    fn decode_detects_truncated_record() {
        let mut buf = Vec::new();
        encode_record(&mut buf, RecordKind::Data, 0, &[1, 2, 3, 4, 5]).unwrap();
        // Truncate the final CRC byte
        buf.pop();
        let err = decode_record(&buf).unwrap_err();
        assert!(
            matches!(err, Error::InvalidFormat(_)),
            "expected size-mismatch error, got {:?}",
            err
        );
    }

    #[test]
    fn decode_rejects_unknown_kind() {
        let mut buf = Vec::new();
        encode_record(&mut buf, RecordKind::Data, 0, &[]).unwrap();
        buf[0] = 99; // unknown kind
        let buf_len = buf.len();
        let crc = crc32fast::hash(&buf[..buf_len - 4]);
        buf[buf_len - 4..].copy_from_slice(&crc.to_le_bytes());
        let err = decode_record(&buf).unwrap_err();
        assert!(matches!(err, Error::InvalidFormat(_)));
    }

    #[test]
    fn peek_next_record_returns_none_when_truncated() {
        let mut buf = Vec::new();
        encode_record(&mut buf, RecordKind::Data, 0, &[1, 2, 3]).unwrap();
        let half = &buf[..buf.len() / 2];
        let r = peek_next_record(half).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn peek_next_record_walks_back_to_back_records() {
        let mut buf = Vec::new();
        let r1_size = encode_record(&mut buf, RecordKind::Data, 0, &[1, 2, 3]).unwrap();
        let batch_crc = crc32fast::hash(&buf[..r1_size]);
        let commit_bytes = CommitPayload {
            prev_commit_lsn: 0,
            tx_id: 1,
            commit_ts: 1000,
            batch_crc,
        }
        .to_bytes();
        let r2_size =
            encode_record(&mut buf, RecordKind::Commit, r1_size as u64, &commit_bytes).unwrap();
        assert_eq!(r1_size + r2_size, buf.len());

        let (h1, _p1, sz1) = peek_next_record(&buf).unwrap().unwrap();
        assert_eq!(h1.kind, RecordKind::Data);
        assert_eq!(sz1, r1_size);

        let (h2, _p2, sz2) = peek_next_record(&buf[sz1..]).unwrap().unwrap();
        assert_eq!(h2.kind, RecordKind::Commit);
        assert_eq!(sz2, r2_size);

        let r = peek_next_record(&buf[sz1 + sz2..]).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn encode_op_test_helper_round_trips() {
        let mut buf = Vec::new();
        let op = Op::DeleteNode { node_id: 7 };
        encode_op_for_test(&mut buf, 0, &op).unwrap();
        let (header, payload) = decode_record(&buf).unwrap();
        assert_eq!(header.kind, RecordKind::Data);
        let restored = Op::from_payload_bytes(payload).unwrap();
        assert_eq!(restored, op);
    }
}
