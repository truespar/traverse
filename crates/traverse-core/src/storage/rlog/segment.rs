//! Redo-log segment file.
//!
//! ```text
//! +-----------------------------------------------+
//! | Segment header (64 bytes)                     |
//! +-----------------------------------------------+
//! | Record 1                                      |
//! | Record 2                                      |
//! | ...                                           |
//! | Record N                                      |
//! +-----------------------------------------------+
//! | unused / zero region (pre-allocated)          |
//! +-----------------------------------------------+
//! ```
//!
//! # Segment header layout (64 bytes)
//!
//! ```text
//! offset  size  field
//! 0       4     magic         "TVRL"
//! 4       4     version       u32 LE = 1
//! 8       8     segment_id    u64 LE (zero-padded into filename)
//! 16      8     start_lsn     u64 LE (global LSN of first byte after this header)
//! 24      8     created_ns    u64 LE (wall-clock at create, debugging only)
//! 32      4     reserved      0
//! 36      4     reserved      0
//! 40      16    salt          [u8;16] random - rejects recycled-file bytes
//! 56      4     header_crc32  u32 LE (covers bytes 0..56)
//! 60      4     reserved      0
//! ```
//!
//! # Why segments
//!
//! - **Bounded recovery**: a single growing WAL would be walked from offset 0 on
//!   every open; segments limit replay to those newer than the last snapshot.
//! - **Truncation at flush**: once `Database::flush()` has the pages durable up
//!   to LSN K, every segment with `end_lsn <= K` can go.
//! - **Pre-allocation**: fixed size means one `set_len` at create and no file
//!   growth mid-write, so no metadata fsyncs.
//! - **Salt**: guards against recycled disk bytes (a reused block not yet
//!   zeroed) looking like a valid record. Not checked against record bytes yet
//!   (record CRC + in-file LSN sequence suffice); reserved so a SQLite-style
//!   salt-chained checksum can come without a format break.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Magic bytes in every segment header.
pub const SEGMENT_MAGIC: &[u8; 4] = b"TVRL";

/// Segment format version.
pub const SEGMENT_VERSION: u32 = 1;

/// On-disk segment header size in bytes.
pub const SEGMENT_HEADER_SIZE: usize = 64;

/// Default segment size (32 MiB). Tunable via `LogWriter::with_segment_size`.
pub const DEFAULT_SEGMENT_SIZE: u64 = 32 * 1024 * 1024;

/// Minimum segment size: header plus at least one record.
pub const MIN_SEGMENT_SIZE: u64 = 64 * 1024;

/// Segment filename, zero-padded so filename order = segment_id order.
pub fn segment_filename(stem: &str, segment_id: u64) -> String {
    format!("{}.rlog.{:010}", stem, segment_id)
}

/// Segment path from directory + db stem + segment id.
pub fn segment_path(dir: &Path, stem: &str, segment_id: u64) -> PathBuf {
    dir.join(segment_filename(stem, segment_id))
}

/// Decoded segment header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    /// Monotonic segment ID.
    pub segment_id: u64,
    /// Global LSN of the first record byte (file offset 64).
    pub start_lsn: u64,
    /// Wall-clock ns at create.
    pub created_ns: u64,
    /// Per-segment random salt.
    pub salt: [u8; 16],
}

impl SegmentHeader {
    /// To a 64-byte buffer.
    pub fn to_bytes(&self) -> [u8; SEGMENT_HEADER_SIZE] {
        let mut buf = [0u8; SEGMENT_HEADER_SIZE];
        buf[0..4].copy_from_slice(SEGMENT_MAGIC);
        buf[4..8].copy_from_slice(&SEGMENT_VERSION.to_le_bytes());
        buf[8..16].copy_from_slice(&self.segment_id.to_le_bytes());
        buf[16..24].copy_from_slice(&self.start_lsn.to_le_bytes());
        buf[24..32].copy_from_slice(&self.created_ns.to_le_bytes());
        buf[40..56].copy_from_slice(&self.salt);
        let crc = crc32fast::hash(&buf[0..56]);
        buf[56..60].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// From a 64-byte buffer; validates magic, version and CRC.
    pub fn from_bytes(buf: &[u8; SEGMENT_HEADER_SIZE]) -> Result<Self> {
        if &buf[0..4] != SEGMENT_MAGIC {
            return Err(Error::InvalidFormat(format!(
                "rlog segment: bad magic, expected TVRL, got {:?}",
                &buf[0..4]
            )));
        }
        let version = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        if version != SEGMENT_VERSION {
            return Err(Error::UnsupportedVersion(version));
        }
        let stored_crc = u32::from_le_bytes(buf[56..60].try_into().unwrap());
        let computed_crc = crc32fast::hash(&buf[0..56]);
        if stored_crc != computed_crc {
            return Err(Error::HeaderChecksumMismatch);
        }
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&buf[40..56]);
        Ok(Self {
            segment_id: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            start_lsn: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            created_ns: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
            salt,
        })
    }
}

/// Segment open for append, owned by the `LogWriter`.
pub struct SegmentWriter {
    file: File,
    path: PathBuf,
    header: SegmentHeader,
    /// Bytes written **after** the header; always `file.stream_position() - SEGMENT_HEADER_SIZE`.
    write_offset: u64,
    /// Max bytes the segment holds (pre-allocated length).
    capacity: u64,
}

impl SegmentWriter {
    /// Creates (or truncates) a segment file at `path`, writes the header at offset 0,
    /// pre-allocates `capacity` bytes. No fsync; the caller syncs at the next commit.
    pub fn create(path: PathBuf, header: SegmentHeader, capacity: u64) -> Result<Self> {
        if capacity < MIN_SEGMENT_SIZE {
            return Err(Error::InvalidFormat(format!(
                "rlog segment: capacity {} below minimum {}",
                capacity, MIN_SEGMENT_SIZE
            )));
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)?;
        // pre-allocate via set_len. Sparse on most filesystems: blocks aren't reserved
        // until written. A real `fallocate(FALLOC_FL_ZERO_RANGE)` would rule out ENOSPC
        // mid-commit; set_len keeps it portable.
        file.set_len(capacity)?;
        file.write_all(&header.to_bytes())?;
        // cursor right after the header, so writes append
        file.seek(SeekFrom::Start(SEGMENT_HEADER_SIZE as u64))?;
        Ok(Self {
            file,
            path,
            header,
            write_offset: 0,
            capacity,
        })
    }

    /// Reopens an existing segment for append, validating the header.
    /// `seed_write_offset` = record bytes already written (where to resume); recovery
    /// determines it by replaying records.
    pub fn open_for_append(path: PathBuf, seed_write_offset: u64) -> Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let mut header_buf = [0u8; SEGMENT_HEADER_SIZE];
        file.read_exact(&mut header_buf)?;
        let header = SegmentHeader::from_bytes(&header_buf)?;
        let capacity = file.metadata()?.len();
        if seed_write_offset > capacity - SEGMENT_HEADER_SIZE as u64 {
            return Err(Error::InvalidFormat(format!(
                "rlog segment: seed_write_offset {} > capacity {}",
                seed_write_offset,
                capacity - SEGMENT_HEADER_SIZE as u64
            )));
        }
        file.seek(SeekFrom::Start(
            SEGMENT_HEADER_SIZE as u64 + seed_write_offset,
        ))?;
        Ok(Self {
            file,
            path,
            header,
            write_offset: seed_write_offset,
            capacity,
        })
    }

    /// Path of this segment file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Decoded header.
    pub fn header(&self) -> &SegmentHeader {
        &self.header
    }

    /// Free capacity remaining in this segment, in bytes.
    pub fn remaining(&self) -> u64 {
        self.capacity - (SEGMENT_HEADER_SIZE as u64) - self.write_offset
    }

    /// Global LSN of the next byte: `header.start_lsn + write_offset`.
    pub fn next_lsn(&self) -> u64 {
        self.header.start_lsn + self.write_offset
    }

    /// Appends raw bytes; returns the global LSN of the first one. No fsync: the
    /// caller batches syncs at commit boundaries (group commit).
    ///
    /// Errors when there's no room; callers check `remaining()` and rotate first.
    pub fn append(&mut self, bytes: &[u8]) -> Result<u64> {
        if (bytes.len() as u64) > self.remaining() {
            return Err(Error::InvalidFormat(format!(
                "rlog segment: append {} bytes won't fit in remaining {}",
                bytes.len(),
                self.remaining()
            )));
        }
        let lsn = self.next_lsn();
        self.file.write_all(bytes)?;
        self.write_offset += bytes.len() as u64;
        Ok(lsn)
    }

    /// `fdatasync` (falls back to `fsync` on Windows and macOS). Enough because the
    /// file is pre-allocated and its length never changes, so there's no metadata
    /// to flush.
    pub fn fdatasync(&self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            // `sync_data` is `fdatasync(2)` on Linux
            self.file.sync_data()?;
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.file.sync_all()?;
        }
        Ok(())
    }
}

/// Read-only segment handle for replay during recovery.
pub struct SegmentReader {
    file: File,
    header: SegmentHeader,
    /// File capacity (`set_len` at create).
    capacity: u64,
}

impl SegmentReader {
    /// Opens a segment for sequential reads; validates the header.
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let mut header_buf = [0u8; SEGMENT_HEADER_SIZE];
        file.read_exact(&mut header_buf)?;
        let header = SegmentHeader::from_bytes(&header_buf)?;
        let capacity = file.metadata()?.len();
        Ok(Self {
            file,
            header,
            capacity,
        })
    }

    /// Whole record region (after the header) in one buffer: one ~32 MiB allocation beats many
    /// small reads.
    pub fn read_all_records(&mut self) -> Result<Vec<u8>> {
        let region_size = self.capacity - SEGMENT_HEADER_SIZE as u64;
        let mut buf = vec![0u8; region_size as usize];
        self.file
            .seek(SeekFrom::Start(SEGMENT_HEADER_SIZE as u64))?;
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Decoded header.
    pub fn header(&self) -> &SegmentHeader {
        &self.header
    }
}

/// Redo-log segments of the `.tvdb` at `tvdb_path`, sorted by segment_id.
pub fn segments_of(tvdb_path: &Path) -> Result<Vec<PathBuf>> {
    let (Some(dir), Some(stem)) = (
        tvdb_path.parent(),
        tvdb_path.file_name().and_then(|n| n.to_str()),
    ) else {
        return Ok(Vec::new());
    };
    discover_segments(dir, stem)
}

/// Deletes redo-log segments (of a database that's gone, or covered by a snapshot elsewhere), then
/// syncs their directory so the deletions stick. Already-missing files are fine.
pub fn remove_segments(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    // a directory handle only opens on unix; elsewhere the deletion is already durable metadata
    if let Some(dir) = paths.first().and_then(|p| p.parent())
        && let Ok(dir) = std::fs::File::open(dir)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Redo-log segments of db `stem` in `dir`, sorted by segment_id (= filename).
pub fn discover_segments(dir: &Path, stem: &str) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let prefix = format!("{}.rlog.", stem);
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if name_str.starts_with(&prefix) {
            // defensive: suffix must parse as u64
            let suffix = &name_str[prefix.len()..];
            if suffix.parse::<u64>().is_ok() {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::rlog::record::{
        COMMIT_PAYLOAD_SIZE, CommitPayload, Op, RecordKind, encode_record, peek_next_record,
    };
    use tempfile::TempDir;

    fn make_header(segment_id: u64, start_lsn: u64) -> SegmentHeader {
        SegmentHeader {
            segment_id,
            start_lsn,
            created_ns: 1_000_000_000,
            salt: [
                1, 2, 3, 4, 5, 6, 7, 8, //
                9, 10, 11, 12, 13, 14, 15, 16,
            ],
        }
    }

    #[test]
    fn segment_header_round_trip() {
        let h = make_header(42, 1024);
        let bytes = h.to_bytes();
        let restored = SegmentHeader::from_bytes(&bytes).unwrap();
        assert_eq!(restored, h);
    }

    #[test]
    fn segment_header_rejects_bad_magic() {
        let h = make_header(1, 0);
        let mut bytes = h.to_bytes();
        bytes[0] = b'X';
        let err = SegmentHeader::from_bytes(&bytes).unwrap_err();
        assert!(matches!(err, Error::InvalidFormat(_)));
    }

    #[test]
    fn segment_header_rejects_bad_crc() {
        let h = make_header(1, 0);
        let mut bytes = h.to_bytes();
        bytes[16] ^= 0xFF; // flip start_lsn
        let err = SegmentHeader::from_bytes(&bytes).unwrap_err();
        assert!(matches!(err, Error::HeaderChecksumMismatch));
    }

    #[test]
    fn segment_create_and_open() {
        let tmp = TempDir::new().unwrap();
        let path = segment_path(tmp.path(), "test", 1);
        let header = make_header(1, 100);
        {
            let writer = SegmentWriter::create(path.clone(), header, MIN_SEGMENT_SIZE).unwrap();
            assert_eq!(writer.header(), &header);
            assert_eq!(writer.next_lsn(), 100);
            assert_eq!(writer.write_offset, 0);
        }
        let reader = SegmentReader::open(&path).unwrap();
        assert_eq!(reader.header(), &header);
        assert_eq!(reader.capacity, MIN_SEGMENT_SIZE);
    }

    #[test]
    fn segment_append_advances_offset_and_lsn() {
        let tmp = TempDir::new().unwrap();
        let path = segment_path(tmp.path(), "test", 1);
        let header = make_header(1, 1000);
        let mut writer = SegmentWriter::create(path, header, MIN_SEGMENT_SIZE).unwrap();
        let first_lsn = writer.append(b"hello").unwrap();
        assert_eq!(first_lsn, 1000);
        assert_eq!(writer.write_offset, 5);
        let second_lsn = writer.append(b"world").unwrap();
        assert_eq!(second_lsn, 1005);
        assert_eq!(writer.write_offset, 10);
    }

    #[test]
    fn segment_append_rejects_oversized_write() {
        let tmp = TempDir::new().unwrap();
        let path = segment_path(tmp.path(), "test", 1);
        let mut writer = SegmentWriter::create(path, make_header(1, 0), MIN_SEGMENT_SIZE).unwrap();
        let too_big = vec![0u8; (MIN_SEGMENT_SIZE - SEGMENT_HEADER_SIZE as u64 + 1) as usize];
        let err = writer.append(&too_big).unwrap_err();
        assert!(matches!(err, Error::InvalidFormat(_)));
    }

    #[test]
    fn segment_writer_records_and_reader_replays() {
        let tmp = TempDir::new().unwrap();
        let path = segment_path(tmp.path(), "test", 1);
        let header = make_header(1, 0);

        let mut buf = Vec::new();
        let op1 = Op::CreateNode {
            node_id: 1,
            labels: vec!["N".to_string()],
            properties: vec![],
        };
        let op2 = Op::DeleteNode { node_id: 2 };

        let p1 = op1.to_payload_bytes().unwrap();
        let lsn1 = 0u64;
        encode_record(&mut buf, RecordKind::Data, lsn1, &p1).unwrap();

        let lsn2 = buf.len() as u64;
        let p2 = op2.to_payload_bytes().unwrap();
        encode_record(&mut buf, RecordKind::Data, lsn2, &p2).unwrap();

        let lsn3 = buf.len() as u64;
        let commit = CommitPayload {
            prev_commit_lsn: 0,
            tx_id: 1,
            commit_ts: 42,
            batch_crc: crc32fast::hash(&buf[..lsn3 as usize]),
        };
        encode_record(&mut buf, RecordKind::Commit, lsn3, &commit.to_bytes()).unwrap();

        {
            let mut writer = SegmentWriter::create(path.clone(), header, MIN_SEGMENT_SIZE).unwrap();
            writer.append(&buf).unwrap();
            writer.fdatasync().unwrap();
        }

        let mut reader = SegmentReader::open(&path).unwrap();
        let region = reader.read_all_records().unwrap();

        // walk back the three records
        let mut cursor = 0usize;
        let (h1, _, sz1) = peek_next_record(&region[cursor..]).unwrap().unwrap();
        assert_eq!(h1.kind, RecordKind::Data);
        assert_eq!(h1.lsn, lsn1);
        cursor += sz1;

        let (h2, _, sz2) = peek_next_record(&region[cursor..]).unwrap().unwrap();
        assert_eq!(h2.kind, RecordKind::Data);
        assert_eq!(h2.lsn, lsn2);
        cursor += sz2;

        let (h3, p3, _sz3) = peek_next_record(&region[cursor..]).unwrap().unwrap();
        assert_eq!(h3.kind, RecordKind::Commit);
        assert_eq!(p3.len(), COMMIT_PAYLOAD_SIZE);
        let commit_back = CommitPayload::from_bytes(p3.try_into().unwrap());
        assert_eq!(commit_back, commit);
    }

    #[test]
    fn segment_open_for_append_resumes_at_offset() {
        let tmp = TempDir::new().unwrap();
        let path = segment_path(tmp.path(), "test", 1);
        let header = make_header(1, 500);
        {
            let mut writer = SegmentWriter::create(path.clone(), header, MIN_SEGMENT_SIZE).unwrap();
            writer.append(&[0xAB; 100]).unwrap();
            writer.fdatasync().unwrap();
        }
        let writer = SegmentWriter::open_for_append(path.clone(), 100).unwrap();
        assert_eq!(writer.write_offset, 100);
        assert_eq!(writer.next_lsn(), 600);
    }

    #[test]
    fn discover_segments_sorts_by_id() {
        let tmp = TempDir::new().unwrap();
        for id in &[5u64, 1, 3, 2, 10, 4] {
            let path = segment_path(tmp.path(), "db", *id);
            std::fs::write(&path, b"").unwrap();
        }
        // a foreign file that must be skipped
        std::fs::write(tmp.path().join("db.tvdb"), b"").unwrap();
        let segments = discover_segments(tmp.path(), "db").unwrap();
        let ids: Vec<u64> = segments
            .iter()
            .map(|p| {
                let name = p.file_name().unwrap().to_str().unwrap();
                name.strip_prefix("db.rlog.").unwrap().parse().unwrap()
            })
            .collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5, 10]);
    }

    #[test]
    fn discover_segments_skips_malformed_suffix() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("db.rlog.notanumber"), b"").unwrap();
        std::fs::write(tmp.path().join("db.rlog.0000000001"), b"").unwrap();
        let segments = discover_segments(tmp.path(), "db").unwrap();
        assert_eq!(segments.len(), 1);
    }
}
