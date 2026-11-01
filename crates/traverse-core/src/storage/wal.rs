//! Page WAL for crash-safe flush: dirty page after-images go to a sidecar
//! `<path>.wal` with a commit record before touching the main `.tvdb`. Recovery
//! replays a committed WAL and discards an uncommitted one.
//!
//! # WAL file format (v2)
//!
//! ```text
//! +-------------------------------------------------+
//! | WAL Header (32 bytes)                           |
//! |   [0..4]   magic: b"TVWL"                      |
//! |   [4..8]   version: u32 LE (2)                  |
//! |   [8..16]  lsn: u64 LE                         |
//! |   [16..20] entry_count: u32 LE                  |
//! |   [20..28] tx_id: u64 LE                        |
//! |   [28..32] checksum: u32 LE (CRC32 of [0..28]) |
//! +-------------------------------------------------+
//! | Entries (PageWrite / HeaderWrite)               |
//! +-------------------------------------------------+
//! | Commit Record (5 bytes)                         |
//! |   [0]     entry_type: 0xFF                      |
//! |   [1..5]  checksum: u32 LE (CRC32 of [0])      |
//! +-------------------------------------------------+
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::storage::format::{HEADER_SIZE, PAGE_SIZE};

const WAL_MAGIC: &[u8; 4] = b"TVWL";
const WAL_VERSION_V1: u32 = 1;
const WAL_VERSION: u32 = 2;
const WAL_HEADER_SIZE: usize = 32;
const ENTRY_TYPE_PAGE: u8 = 1;
const ENTRY_TYPE_HEADER: u8 = 2;
const ENTRY_TYPE_COMMIT: u8 = 0xFF;

/// WAL sidecar path for a database path.
pub fn wal_path(db_path: &Path) -> PathBuf {
    let mut p = db_path.as_os_str().to_owned();
    p.push(".wal");
    PathBuf::from(p)
}

/// WAL entry read back from disk.
#[derive(Debug)]
pub enum WalEntry {
    /// Full page after-image.
    PageWrite {
        /// Page ID of the image.
        page_id: u64,
        /// Full page buffer.
        data: Box<[u8; PAGE_SIZE]>,
    },
    /// File header after-image.
    HeaderWrite {
        /// Full header buffer.
        data: Box<[u8; HEADER_SIZE]>,
    },
    /// Commit sentinel: the WAL is valid.
    Commit,
}

/// Writes a WAL file atomically: create -> write_page / write_header -> commit (consumes self).
pub struct WalWriter {
    file: File,
    entry_count: u32,
    lsn: u64,
    tx_id: u64,
}

impl WalWriter {
    /// Creates a WAL file; header gets a placeholder entry count.
    pub fn create(path: &Path, lsn: u64, tx_id: u64) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;

        let mut writer = Self {
            file,
            entry_count: 0,
            lsn,
            tx_id,
        };

        // v2 header; entry_count is patched on commit
        let mut hdr = [0u8; WAL_HEADER_SIZE];
        hdr[0..4].copy_from_slice(WAL_MAGIC);
        hdr[4..8].copy_from_slice(&WAL_VERSION.to_le_bytes());
        hdr[8..16].copy_from_slice(&lsn.to_le_bytes());
        // entry_count = 0 placeholder at [16..20]
        hdr[20..28].copy_from_slice(&tx_id.to_le_bytes());
        let crc = crc32fast::hash(&hdr[0..28]);
        hdr[28..32].copy_from_slice(&crc.to_le_bytes());
        writer.file.write_all(&hdr)?;

        Ok(writer)
    }

    /// Appends a page after-image.
    pub fn write_page(&mut self, page_id: u64, data: &[u8; PAGE_SIZE]) -> Result<()> {
        // entry_type(1) + page_id(8) + data_len(4) + data(PAGE_SIZE) + crc(4)
        let data_len = PAGE_SIZE as u32;
        let prefix_len = 1 + 8 + 4;
        let mut buf = vec![0u8; prefix_len + PAGE_SIZE + 4];

        buf[0] = ENTRY_TYPE_PAGE;
        buf[1..9].copy_from_slice(&page_id.to_le_bytes());
        buf[9..13].copy_from_slice(&data_len.to_le_bytes());
        buf[13..13 + PAGE_SIZE].copy_from_slice(data);

        let crc = crc32fast::hash(&buf[..prefix_len + PAGE_SIZE]);
        buf[prefix_len + PAGE_SIZE..].copy_from_slice(&crc.to_le_bytes());

        self.file.write_all(&buf)?;
        self.entry_count += 1;
        Ok(())
    }

    /// Appends a file header after-image.
    pub fn write_header(&mut self, data: &[u8; HEADER_SIZE]) -> Result<()> {
        let data_len = HEADER_SIZE as u32;
        let prefix_len = 1 + 8 + 4;
        let mut buf = vec![0u8; prefix_len + HEADER_SIZE + 4];

        buf[0] = ENTRY_TYPE_HEADER;
        // page_id unused for header writes, stays 0
        buf[9..13].copy_from_slice(&data_len.to_le_bytes());
        buf[13..13 + HEADER_SIZE].copy_from_slice(data);

        let crc = crc32fast::hash(&buf[..prefix_len + HEADER_SIZE]);
        buf[prefix_len + HEADER_SIZE..].copy_from_slice(&crc.to_le_bytes());

        self.file.write_all(&buf)?;
        self.entry_count += 1;
        Ok(())
    }

    /// Writes the commit record, patches the entry count, fsyncs. Seals the WAL (consumes self).
    pub fn commit(mut self) -> Result<()> {
        // commit record (5 bytes)
        let mut commit = [0u8; 5];
        commit[0] = ENTRY_TYPE_COMMIT;
        let crc = crc32fast::hash(&commit[0..1]);
        commit[1..5].copy_from_slice(&crc.to_le_bytes());
        self.file.write_all(&commit)?;

        // rebuild the final v2 header in memory (no read back)
        let mut hdr = [0u8; WAL_HEADER_SIZE];
        hdr[0..4].copy_from_slice(WAL_MAGIC);
        hdr[4..8].copy_from_slice(&WAL_VERSION.to_le_bytes());
        hdr[8..16].copy_from_slice(&self.lsn.to_le_bytes());
        hdr[16..20].copy_from_slice(&self.entry_count.to_le_bytes());
        hdr[20..28].copy_from_slice(&self.tx_id.to_le_bytes());
        let crc = crc32fast::hash(&hdr[0..28]);
        hdr[28..32].copy_from_slice(&crc.to_le_bytes());

        // write the final header
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&hdr)?;

        self.file.sync_all()?;
        Ok(())
    }
}

/// Reads and validates a WAL. `Some(entries)` if it exists with a valid commit
/// record; `None` if missing, empty or uncommitted (then it's deleted).
pub fn read_wal(path: &Path) -> Result<Option<Vec<WalEntry>>> {
    if !path.exists() {
        return Ok(None);
    }

    let metadata = fs::metadata(path)?;
    if metadata.len() < WAL_HEADER_SIZE as u64 {
        // truncated/empty WAL: discard
        let _ = fs::remove_file(path);
        return Ok(None);
    }

    let mut file = File::open(path)?;

    // read + validate the header
    let mut hdr = [0u8; WAL_HEADER_SIZE];
    if file.read_exact(&mut hdr).is_err() {
        let _ = fs::remove_file(path);
        return Ok(None);
    }

    if &hdr[0..4] != WAL_MAGIC {
        let _ = fs::remove_file(path);
        return Ok(None);
    }

    let version = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
    match version {
        WAL_VERSION_V1 => {
            // v1: checksum at [20..24] covering [0..20]
            let stored_crc = u32::from_le_bytes(hdr[20..24].try_into().unwrap());
            let computed_crc = crc32fast::hash(&hdr[0..20]);
            if stored_crc != computed_crc {
                let _ = fs::remove_file(path);
                return Ok(None);
            }
        }
        WAL_VERSION => {
            // v2: checksum at [28..32] covering [0..28]
            let stored_crc = u32::from_le_bytes(hdr[28..32].try_into().unwrap());
            let computed_crc = crc32fast::hash(&hdr[0..28]);
            if stored_crc != computed_crc {
                let _ = fs::remove_file(path);
                return Ok(None);
            }
        }
        _ => {
            let _ = fs::remove_file(path);
            return Ok(None);
        }
    }

    let entry_count = u32::from_le_bytes(hdr[16..20].try_into().unwrap());

    let mut entries = Vec::with_capacity(entry_count as usize);
    for _ in 0..entry_count {
        match read_entry(&mut file) {
            Ok(entry) => entries.push(entry),
            Err(_) => {
                // truncated or corrupt entry: treat as uncommitted
                let _ = fs::remove_file(path);
                return Ok(None);
            }
        }
    }

    // commit record
    match read_entry(&mut file) {
        Ok(WalEntry::Commit) => {}
        _ => {
            // no valid commit: discard
            let _ = fs::remove_file(path);
            return Ok(None);
        }
    }

    entries.push(WalEntry::Commit);
    Ok(Some(entries))
}

/// Reads one WAL entry at the current position.
fn read_entry(file: &mut File) -> Result<WalEntry> {
    let mut type_buf = [0u8; 1];
    file.read_exact(&mut type_buf)
        .map_err(|e| Error::WalCorrupted(format!("failed to read entry type: {e}")))?;

    match type_buf[0] {
        ENTRY_TYPE_PAGE => {
            let mut meta = [0u8; 12]; // page_id(8) + data_len(4)
            file.read_exact(&mut meta)
                .map_err(|e| Error::WalCorrupted(format!("failed to read page entry meta: {e}")))?;
            let page_id = u64::from_le_bytes(meta[0..8].try_into().unwrap());
            let data_len = u32::from_le_bytes(meta[8..12].try_into().unwrap()) as usize;
            if data_len != PAGE_SIZE {
                return Err(Error::WalCorrupted(format!(
                    "page entry data_len {data_len} != PAGE_SIZE {PAGE_SIZE}"
                )));
            }

            let mut data = Box::new([0u8; PAGE_SIZE]);
            file.read_exact(data.as_mut())
                .map_err(|e| Error::WalCorrupted(format!("failed to read page data: {e}")))?;

            let mut crc_buf = [0u8; 4];
            file.read_exact(&mut crc_buf)
                .map_err(|e| Error::WalCorrupted(format!("failed to read page entry crc: {e}")))?;

            // checksum covers type + meta + data
            let stored_crc = u32::from_le_bytes(crc_buf);
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&type_buf);
            hasher.update(&meta);
            hasher.update(data.as_ref());
            let computed_crc = hasher.finalize();
            if stored_crc != computed_crc {
                return Err(Error::WalCorrupted("page entry checksum mismatch".into()));
            }

            Ok(WalEntry::PageWrite { page_id, data })
        }
        ENTRY_TYPE_HEADER => {
            let mut meta = [0u8; 12]; // page_id(8, unused) + data_len(4)
            file.read_exact(&mut meta).map_err(|e| {
                Error::WalCorrupted(format!("failed to read header entry meta: {e}"))
            })?;
            let data_len = u32::from_le_bytes(meta[8..12].try_into().unwrap()) as usize;
            if data_len != HEADER_SIZE {
                return Err(Error::WalCorrupted(format!(
                    "header entry data_len {data_len} != HEADER_SIZE {HEADER_SIZE}"
                )));
            }

            let mut data = Box::new([0u8; HEADER_SIZE]);
            file.read_exact(data.as_mut())
                .map_err(|e| Error::WalCorrupted(format!("failed to read header data: {e}")))?;

            let mut crc_buf = [0u8; 4];
            file.read_exact(&mut crc_buf).map_err(|e| {
                Error::WalCorrupted(format!("failed to read header entry crc: {e}"))
            })?;

            let stored_crc = u32::from_le_bytes(crc_buf);
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&type_buf);
            hasher.update(&meta);
            hasher.update(data.as_ref());
            let computed_crc = hasher.finalize();
            if stored_crc != computed_crc {
                return Err(Error::WalCorrupted("header entry checksum mismatch".into()));
            }

            Ok(WalEntry::HeaderWrite { data })
        }
        ENTRY_TYPE_COMMIT => {
            let mut crc_buf = [0u8; 4];
            file.read_exact(&mut crc_buf)
                .map_err(|e| Error::WalCorrupted(format!("failed to read commit crc: {e}")))?;
            let stored_crc = u32::from_le_bytes(crc_buf);
            let computed_crc = crc32fast::hash(&[ENTRY_TYPE_COMMIT]);
            if stored_crc != computed_crc {
                return Err(Error::WalCorrupted(
                    "commit record checksum mismatch".into(),
                ));
            }
            Ok(WalEntry::Commit)
        }
        other => Err(Error::WalCorrupted(format!("unknown entry type: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_path_appends_extension() {
        let p = wal_path(Path::new("/tmp/test.tvdb"));
        assert_eq!(p, PathBuf::from("/tmp/test.tvdb.wal"));
    }

    #[test]
    fn write_and_read_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tvdb.wal");

        // WAL with one page entry and one header entry
        let page_data = Box::new([0xABu8; PAGE_SIZE]);
        let header_data = Box::new([0xCDu8; HEADER_SIZE]);

        {
            let mut w = WalWriter::create(&path, 7, 7).unwrap();
            w.write_page(42, &page_data).unwrap();
            w.write_header(&header_data).unwrap();
            w.commit().unwrap();
        }

        let entries = read_wal(&path).unwrap().expect("should have entries");
        assert_eq!(entries.len(), 3); // page + header + commit

        match &entries[0] {
            WalEntry::PageWrite { page_id, data } => {
                assert_eq!(*page_id, 42);
                assert_eq!(data.as_ref(), page_data.as_ref());
            }
            other => panic!("expected PageWrite, got {other:?}"),
        }
        match &entries[1] {
            WalEntry::HeaderWrite { data } => {
                assert_eq!(data.as_ref(), header_data.as_ref());
            }
            other => panic!("expected HeaderWrite, got {other:?}"),
        }
        assert!(matches!(entries[2], WalEntry::Commit));
    }

    #[test]
    fn uncommitted_wal_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tvdb.wal");

        // WAL without commit
        {
            let mut w = WalWriter::create(&path, 1, 1).unwrap();
            w.write_page(0, &[0u8; PAGE_SIZE]).unwrap();
            // dropped uncommitted
        }

        let result = read_wal(&path).unwrap();
        assert!(result.is_none());
        // WAL cleaned up
        assert!(!path.exists());
    }

    #[test]
    fn nonexistent_wal_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.wal");
        let result = read_wal(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn empty_wal_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.wal");
        File::create(&path).unwrap();
        let result = read_wal(&path).unwrap();
        assert!(result.is_none());
        assert!(!path.exists());
    }

    #[test]
    fn corrupt_entry_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.wal");

        // valid WAL
        {
            let mut w = WalWriter::create(&path, 1, 1).unwrap();
            w.write_page(0, &[0u8; PAGE_SIZE]).unwrap();
            w.commit().unwrap();
        }

        // corrupt a byte of the page entry data
        {
            let mut file = OpenOptions::new().write(true).open(&path).unwrap();
            // into the page data (header 32 + entry prefix 13)
            file.seek(SeekFrom::Start(32 + 13 + 100)).unwrap();
            file.write_all(&[0xFF]).unwrap();
        }

        let result = read_wal(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn v1_wal_still_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.wal");

        // hand-made v1 WAL: one page entry + commit record
        let page_data = [0xABu8; PAGE_SIZE];
        let page_id: u64 = 5;
        {
            let mut file = File::create(&path).unwrap();

            // v1 header: magic, version=1, lsn=10, entry_count=1, crc of [0..20], reserved
            let mut hdr = [0u8; WAL_HEADER_SIZE];
            hdr[0..4].copy_from_slice(b"TVWL");
            hdr[4..8].copy_from_slice(&1u32.to_le_bytes()); // version 1
            hdr[8..16].copy_from_slice(&10u64.to_le_bytes()); // lsn
            hdr[16..20].copy_from_slice(&1u32.to_le_bytes()); // entry_count
            let crc = crc32fast::hash(&hdr[0..20]);
            hdr[20..24].copy_from_slice(&crc.to_le_bytes());
            // [24..32] reserved zeros
            file.write_all(&hdr).unwrap();

            // Page entry: type(1) + page_id(8) + data_len(4) + data + crc(4)
            let data_len = PAGE_SIZE as u32;
            let prefix_len = 1 + 8 + 4;
            let mut buf = vec![0u8; prefix_len + PAGE_SIZE + 4];
            buf[0] = 1; // ENTRY_TYPE_PAGE
            buf[1..9].copy_from_slice(&page_id.to_le_bytes());
            buf[9..13].copy_from_slice(&data_len.to_le_bytes());
            buf[13..13 + PAGE_SIZE].copy_from_slice(&page_data);
            let entry_crc = crc32fast::hash(&buf[..prefix_len + PAGE_SIZE]);
            buf[prefix_len + PAGE_SIZE..].copy_from_slice(&entry_crc.to_le_bytes());
            file.write_all(&buf).unwrap();

            // Commit record: type(1) + crc(4)
            let mut commit = [0u8; 5];
            commit[0] = 0xFF; // ENTRY_TYPE_COMMIT
            let commit_crc = crc32fast::hash(&commit[0..1]);
            commit[1..5].copy_from_slice(&commit_crc.to_le_bytes());
            file.write_all(&commit).unwrap();

            file.sync_all().unwrap();
        }

        // read_wal accepts v1
        let entries = read_wal(&path).unwrap().expect("v1 WAL should be readable");
        assert_eq!(entries.len(), 2); // page + commit
        match &entries[0] {
            WalEntry::PageWrite { page_id: pid, data } => {
                assert_eq!(*pid, page_id);
                assert_eq!(data.as_ref(), &page_data);
            }
            other => panic!("expected PageWrite, got {other:?}"),
        }
        assert!(matches!(entries[1], WalEntry::Commit));
    }
}
