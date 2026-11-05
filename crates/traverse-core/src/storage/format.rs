//! `.tvdb` file header and format constants.

use crate::error::{Error, Result};

/// Magic bytes of a TVDB file.
pub const MAGIC: &[u8; 4] = b"TVDB";

/// Format v1: no tombstones.
pub const FORMAT_VERSION_V1: u32 = 1;

/// Format v2: tombstones, incremental flush.
pub const FORMAT_VERSION_V2: u32 = 2;

/// Format v3: WAL crash safety, LSN tracking.
pub const FORMAT_VERSION_V3: u32 = 3;

/// Format v4: compact binary property store.
pub const FORMAT_VERSION_V4: u32 = 4;

/// Format v5: metadata page (auth catalog).
pub const FORMAT_VERSION_V5: u32 = 5;

/// Format v6: redo-log recovery LSN.
pub const FORMAT_VERSION_V6: u32 = 6;

/// Format v7: records, catalog entries and metadata larger than a page continue on overflow
/// pages. Only a file that has some is written as v7; the rest stay v6, which earlier releases
/// read.
pub const FORMAT_VERSION_V7: u32 = 7;

/// Format v8: a relationship without properties is a fixed 29-byte record (flag, id, type,
/// start, end) instead of a serialized structure. Only a file that has some is written as v8;
/// the rest stay v6 or v7.
pub const FORMAT_VERSION_V8: u32 = 8;

/// Newest format version this build reads and writes.
pub const CURRENT_FORMAT_VERSION: u32 = FORMAT_VERSION_V8;

/// File header size (4 KiB).
pub const HEADER_SIZE: usize = 4096;

/// Data page size (16 KiB).
pub const PAGE_SIZE: usize = 16384;

/// Page header size (64 bytes).
pub const PAGE_HEADER_SIZE: usize = 64;

/// Max record data per page.
pub const PAGE_DATA_CAPACITY: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

/// PAGE_SIZE must fit u16 for slot directory offsets.
const _: () = assert!(PAGE_SIZE <= u16::MAX as usize);

/// Header at offset 0 of every `.tvdb` file.
///
/// v1/v2 layout (4096 bytes total):
/// - `[0..4]`   magic: `b"TVDB"`
/// - `[4..8]`   version: u32 LE
/// - `[8..16]`  page_count: u64 LE
/// - `[16..24]` catalog_page_id: u64 LE
/// - `[24..28]` flags: u32 LE
/// - `[28..32]` checksum: CRC32 of bytes `[0..28]`
/// - `[32..4096]` reserved (zeroed)
///
/// v3 adds:
/// - `[32..40]` lsn: u64 LE (flush log sequence number)
/// - `[40..44]` checksum: CRC32 of bytes `[0..40]` (moved from `[28..32]`)
/// - `[44..4096]` reserved (zeroed)
///
/// v5 adds:
/// - `[44..52]` metadata_page_id: u64 LE (server metadata page, 0 = none)
/// - `[52..56]` checksum: CRC32 of bytes `[0..52]` (moved from `[40..44]`)
/// - `[56..4096]` reserved (zeroed)
///
/// v6 adds:
/// - `[52..60]` recovery_lsn: u64 LE (highest redo-log LSN reflected in the
///   page data; replay skips records at or below it)
/// - `[60..64]` checksum: CRC32 of bytes `[0..60]` (moved from `[52..56]`)
/// - `[64..4096]` reserved (zeroed)
///
/// v7 has the v6 header; the version says the file has overflow pages.
#[derive(Debug, Clone)]
pub struct FileHeader {
    /// Version the file is written as: v6, or v7 once it has overflow pages (see
    /// [`note_overflow`](Self::note_overflow)). Older files are rewritten as v6.
    pub version: u32,
    /// Pages in the file (header excluded).
    pub page_count: u64,
    /// Catalog page ID.
    pub catalog_page_id: u64,
    /// Feature flags (reserved).
    pub flags: u32,
    /// Flush log sequence number (monotonic; 0 for v1/v2 files).
    pub lsn: u64,
    /// Metadata page ID (auth catalog, ...); 0 = none.
    pub metadata_page_id: u64,
    /// Highest redo-log LSN reflected in the page data (v6+); 0 before v6 or before the first
    /// flush.
    pub recovery_lsn: u64,
}

impl FileHeader {
    /// New header.
    pub fn new(page_count: u64, catalog_page_id: u64) -> Self {
        Self {
            version: FORMAT_VERSION_V6,
            page_count,
            catalog_page_id,
            flags: 0,
            lsn: 0,
            metadata_page_id: 0,
            recovery_lsn: 0,
        }
    }

    /// The file has overflow pages: written as v7 from now on.
    pub fn note_overflow(&mut self) {
        self.version = self.version.max(FORMAT_VERSION_V7);
    }

    /// The file has fixed-layout property-less relationship records: v8 from now on.
    pub fn note_compact_edges(&mut self) {
        self.version = self.version.max(FORMAT_VERSION_V8);
    }

    /// To a 4096-byte buffer (v6 layout, version 6 or 7).
    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(MAGIC);
        buf[4..8].copy_from_slice(&self.version.max(FORMAT_VERSION_V6).to_le_bytes());
        buf[8..16].copy_from_slice(&self.page_count.to_le_bytes());
        buf[16..24].copy_from_slice(&self.catalog_page_id.to_le_bytes());
        buf[24..28].copy_from_slice(&self.flags.to_le_bytes());
        buf[32..40].copy_from_slice(&self.lsn.to_le_bytes());
        buf[44..52].copy_from_slice(&self.metadata_page_id.to_le_bytes());
        buf[52..60].copy_from_slice(&self.recovery_lsn.to_le_bytes());

        // v6: checksum covers [0..60], stored at [60..64]
        let crc = crc32fast::hash(&buf[0..60]);
        buf[60..64].copy_from_slice(&crc.to_le_bytes());

        buf
    }

    /// From a 4096-byte buffer; validates magic, version and checksum.
    pub fn from_bytes(buf: &[u8; HEADER_SIZE]) -> Result<Self> {
        if &buf[0..4] != MAGIC {
            return Err(Error::InvalidFormat(format!(
                "bad magic: expected TVDB, got {:?}",
                &buf[0..4]
            )));
        }

        // accept v1 through v7
        let version = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        if !(FORMAT_VERSION_V1..=CURRENT_FORMAT_VERSION).contains(&version) {
            return Err(Error::UnsupportedVersion(version));
        }

        // checksum layout depends on the version
        let (lsn, metadata_page_id, recovery_lsn) = if version <= FORMAT_VERSION_V2 {
            // v1/v2: checksum covers [0..28], stored at [28..32]
            let stored_crc = u32::from_le_bytes(buf[28..32].try_into().unwrap());
            let computed_crc = crc32fast::hash(&buf[0..28]);
            if stored_crc != computed_crc {
                return Err(Error::HeaderChecksumMismatch);
            }
            (0, 0, 0)
        } else if version <= FORMAT_VERSION_V4 {
            // v3/v4: checksum covers [0..40], stored at [40..44]
            let stored_crc = u32::from_le_bytes(buf[40..44].try_into().unwrap());
            let computed_crc = crc32fast::hash(&buf[0..40]);
            if stored_crc != computed_crc {
                return Err(Error::HeaderChecksumMismatch);
            }
            (u64::from_le_bytes(buf[32..40].try_into().unwrap()), 0, 0)
        } else if version == FORMAT_VERSION_V5 {
            // v5: checksum covers [0..52], stored at [52..56]
            let stored_crc = u32::from_le_bytes(buf[52..56].try_into().unwrap());
            let computed_crc = crc32fast::hash(&buf[0..52]);
            if stored_crc != computed_crc {
                return Err(Error::HeaderChecksumMismatch);
            }
            (
                u64::from_le_bytes(buf[32..40].try_into().unwrap()),
                u64::from_le_bytes(buf[44..52].try_into().unwrap()),
                0,
            )
        } else {
            // v6 and v7: checksum covers [0..60], stored at [60..64]
            let stored_crc = u32::from_le_bytes(buf[60..64].try_into().unwrap());
            let computed_crc = crc32fast::hash(&buf[0..60]);
            if stored_crc != computed_crc {
                return Err(Error::HeaderChecksumMismatch);
            }
            (
                u64::from_le_bytes(buf[32..40].try_into().unwrap()),
                u64::from_le_bytes(buf[44..52].try_into().unwrap()),
                u64::from_le_bytes(buf[52..60].try_into().unwrap()),
            )
        };

        let page_count = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let catalog_page_id = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        let flags = u32::from_le_bytes(buf[24..28].try_into().unwrap());

        Ok(Self {
            version,
            page_count,
            catalog_page_id,
            flags,
            lsn,
            metadata_page_id,
            recovery_lsn,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let header = FileHeader::new(42, 0);
        let bytes = header.to_bytes();
        let restored = FileHeader::from_bytes(&bytes).unwrap();
        assert_eq!(restored.page_count, 42);
        assert_eq!(restored.catalog_page_id, 0);
        assert_eq!(restored.flags, 0);
        assert_eq!(restored.lsn, 0);
        assert_eq!(restored.metadata_page_id, 0);
    }

    #[test]
    fn header_roundtrip_with_lsn() {
        let mut header = FileHeader::new(10, 3);
        header.lsn = 42;
        let bytes = header.to_bytes();
        let restored = FileHeader::from_bytes(&bytes).unwrap();
        assert_eq!(restored.page_count, 10);
        assert_eq!(restored.catalog_page_id, 3);
        assert_eq!(restored.lsn, 42);
        assert_eq!(restored.metadata_page_id, 0);
    }

    #[test]
    fn header_roundtrip_with_metadata() {
        let mut header = FileHeader::new(10, 3);
        header.lsn = 5;
        header.metadata_page_id = 7;
        let bytes = header.to_bytes();
        let restored = FileHeader::from_bytes(&bytes).unwrap();
        assert_eq!(restored.page_count, 10);
        assert_eq!(restored.catalog_page_id, 3);
        assert_eq!(restored.lsn, 5);
        assert_eq!(restored.metadata_page_id, 7);
    }

    #[test]
    fn v2_header_readable_with_lsn_zero() {
        // hand-built v2 header
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(MAGIC);
        buf[4..8].copy_from_slice(&FORMAT_VERSION_V2.to_le_bytes());
        buf[8..16].copy_from_slice(&5u64.to_le_bytes());
        buf[16..24].copy_from_slice(&1u64.to_le_bytes());
        buf[24..28].copy_from_slice(&0u32.to_le_bytes());
        let crc = crc32fast::hash(&buf[0..28]);
        buf[28..32].copy_from_slice(&crc.to_le_bytes());

        let restored = FileHeader::from_bytes(&buf).unwrap();
        assert_eq!(restored.page_count, 5);
        assert_eq!(restored.catalog_page_id, 1);
        assert_eq!(restored.lsn, 0);
    }

    #[test]
    fn header_checksum_validated() {
        let header = FileHeader::new(1, 0);
        let mut bytes = header.to_bytes();
        // tamper with page_count (checksummed range)
        bytes[8] = 0xFF;
        let err = FileHeader::from_bytes(&bytes).unwrap_err();
        assert!(matches!(err, Error::HeaderChecksumMismatch));
    }

    #[test]
    fn header_invalid_magic() {
        let mut bytes = [0u8; HEADER_SIZE];
        bytes[0..4].copy_from_slice(b"NOPE");
        let err = FileHeader::from_bytes(&bytes).unwrap_err();
        assert!(matches!(err, Error::InvalidFormat(_)));
    }

    #[test]
    fn header_roundtrip_v6_with_recovery_lsn() {
        let mut header = FileHeader::new(7, 4);
        header.lsn = 13;
        header.metadata_page_id = 21;
        header.recovery_lsn = 0xCAFEBABE_DEADBEEF;
        let bytes = header.to_bytes();
        let restored = FileHeader::from_bytes(&bytes).unwrap();
        assert_eq!(restored.page_count, 7);
        assert_eq!(restored.catalog_page_id, 4);
        assert_eq!(restored.lsn, 13);
        assert_eq!(restored.metadata_page_id, 21);
        assert_eq!(restored.recovery_lsn, 0xCAFEBABE_DEADBEEF);
    }

    #[test]
    fn v5_header_readable_with_recovery_lsn_zero() {
        // hand-built v5 header: v6 code accepts it with recovery_lsn = 0
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(MAGIC);
        buf[4..8].copy_from_slice(&FORMAT_VERSION_V5.to_le_bytes());
        buf[8..16].copy_from_slice(&5u64.to_le_bytes());
        buf[16..24].copy_from_slice(&1u64.to_le_bytes());
        buf[32..40].copy_from_slice(&42u64.to_le_bytes());
        buf[44..52].copy_from_slice(&7u64.to_le_bytes());
        let crc = crc32fast::hash(&buf[0..52]);
        buf[52..56].copy_from_slice(&crc.to_le_bytes());

        let restored = FileHeader::from_bytes(&buf).unwrap();
        assert_eq!(restored.page_count, 5);
        assert_eq!(restored.catalog_page_id, 1);
        assert_eq!(restored.lsn, 42);
        assert_eq!(restored.metadata_page_id, 7);
        assert_eq!(restored.recovery_lsn, 0);
    }

    #[test]
    fn v6_checksum_validated_in_extended_range() {
        let mut header = FileHeader::new(1, 0);
        header.recovery_lsn = 0xDEADBEEF;
        let mut bytes = header.to_bytes();
        // tamper with recovery_lsn (v6 checksummed range)
        bytes[53] ^= 0xFF;
        let err = FileHeader::from_bytes(&bytes).unwrap_err();
        assert!(matches!(err, Error::HeaderChecksumMismatch));
    }

    #[test]
    fn header_bad_version() {
        let mut bytes = [0u8; HEADER_SIZE];
        bytes[0..4].copy_from_slice(MAGIC);
        bytes[4..8].copy_from_slice(&999u32.to_le_bytes());
        // fix the checksum for this bad-version header (v1/v2 layout)
        let crc = crc32fast::hash(&bytes[0..28]);
        bytes[28..32].copy_from_slice(&crc.to_le_bytes());
        let err = FileHeader::from_bytes(&bytes).unwrap_err();
        assert!(matches!(err, Error::UnsupportedVersion(999)));
    }
}
