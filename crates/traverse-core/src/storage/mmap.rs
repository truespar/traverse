//! Reader for loading pages in `Database::open()`.
//!
//! * `mmap` (default on native): memory-mapped via memmap2.
//! * without `mmap` (WASM, locked-down sandboxes): whole file in a `Vec<u8>`,
//!   built with `from_data()` since `new(&File)` needs `std::fs::File`, which
//!   wasm32 lacks.

#[cfg(any(feature = "mmap", not(target_arch = "wasm32")))]
use std::fs::File;

use crate::error::{Error, Result};
use crate::storage::format::{HEADER_SIZE, PAGE_SIZE};

/// Read-only view of a TVDB file for the initial page load; dropped before any writes.
pub struct MmapReader {
    #[cfg(feature = "mmap")]
    mmap: Option<memmap2::Mmap>,
    /// Owned-bytes fallback: `from_data()` everywhere, `new(&File)` without the `mmap` feature.
    data: Vec<u8>,
}

impl MmapReader {
    /// Reader for `file`: memory-mapped with the `mmap` feature, else the whole file read into a
    /// `Vec<u8>`.
    #[cfg(feature = "mmap")]
    pub fn new(file: &File) -> Result<Self> {
        // SAFETY: read-only map for the initial load, dropped before this process writes the file;
        // like any mmap it assumes no other process truncates the file meanwhile (`file-locking`
        // enforces that)
        let mmap = unsafe { memmap2::Mmap::map(file)? };
        Ok(Self {
            mmap: Some(mmap),
            data: Vec::new(),
        })
    }

    /// Reader holding the whole file in memory, for runtimes without mmap. Not on wasm32 (no
    /// `std::fs::File`).
    #[cfg(all(not(feature = "mmap"), not(target_arch = "wasm32")))]
    pub fn new(file: &File) -> Result<Self> {
        use std::io::Read;
        let len = file.metadata()?.len() as usize;
        let mut data = vec![0u8; len];
        let mut reader = std::io::BufReader::new(file);
        reader.read_exact(&mut data)?;
        Ok(Self { data })
    }

    /// Reader from bytes already read. Portable, including WASM, where bytes come from `fetch()` /
    /// OPFS / the File API.
    pub fn from_data(data: Vec<u8>) -> Self {
        Self {
            #[cfg(feature = "mmap")]
            mmap: None,
            data,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        #[cfg(feature = "mmap")]
        {
            if let Some(m) = &self.mmap {
                return &m[..];
            }
        }
        &self.data
    }

    /// Pages that fit in the file (tests).
    #[cfg(test)]
    fn page_count(&self) -> u64 {
        let len = self.as_bytes().len();
        if len <= HEADER_SIZE {
            return 0;
        }
        ((len - HEADER_SIZE) / PAGE_SIZE) as u64
    }

    /// Raw header bytes (first 4096).
    pub fn header_bytes(&self) -> Result<&[u8; HEADER_SIZE]> {
        let buf = self.as_bytes();
        if buf.len() < HEADER_SIZE {
            return Err(Error::InvalidFormat("file too small for header".into()));
        }
        Ok(buf[..HEADER_SIZE].try_into().unwrap())
    }

    /// Raw bytes of a page, at `HEADER_SIZE + page_id * PAGE_SIZE`.
    pub fn page_bytes(&self, page_id: u64) -> Result<&[u8]> {
        let buf = self.as_bytes();
        usize::try_from(page_id)
            .ok()
            .and_then(|id| id.checked_mul(PAGE_SIZE))
            .and_then(|at| at.checked_add(HEADER_SIZE))
            .and_then(|offset| buf.get(offset..offset.checked_add(PAGE_SIZE)?))
            .ok_or(Error::PageNotFound(page_id))
    }

    /// Whole pages the data holds after the header.
    pub fn page_capacity(&self) -> u64 {
        (self.as_bytes().len().saturating_sub(HEADER_SIZE) / PAGE_SIZE) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::format::FileHeader;
    use std::io::Write;

    #[test]
    fn mmap_reads_header_and_page() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tvdb");

        // minimal file: header + 1 page
        let header = FileHeader::new(1, 0);
        let mut file = File::create(&path).unwrap();
        file.write_all(&header.to_bytes()).unwrap();

        let mut page_buf = vec![0u8; PAGE_SIZE];
        page_buf[0] = 0x42; // marker byte
        file.write_all(&page_buf).unwrap();
        file.flush().unwrap();
        drop(file);

        let file = File::open(&path).unwrap();
        let reader = MmapReader::new(&file).unwrap();

        assert_eq!(reader.page_count(), 1);
        let hdr = reader.header_bytes().unwrap();
        assert_eq!(&hdr[0..4], b"TVDB");

        let page = reader.page_bytes(0).unwrap();
        assert_eq!(page[0], 0x42);
    }

    #[test]
    fn mmap_invalid_page_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tvdb");

        let header = FileHeader::new(0, 0);
        let mut file = File::create(&path).unwrap();
        file.write_all(&header.to_bytes()).unwrap();
        file.flush().unwrap();
        drop(file);

        let file = File::open(&path).unwrap();
        let reader = MmapReader::new(&file).unwrap();
        assert!(reader.page_bytes(0).is_err());
    }
}
