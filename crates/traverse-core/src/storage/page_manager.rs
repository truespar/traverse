//! Page allocation, I/O and free-list management.
//!
//! `PageManager` caches pages in memory. Native targets also hold the `.tvdb`
//! `File` (standard load path); the bytes-only path (WASM) has none. `flush()`
//! writes dirty pages to a WAL first for crash safety, then to the main file.
//! Bytes-only mode skips WAL and file writes; [`PageManager::to_bytes`] produces
//! the on-disk format as one buffer.

use std::collections::HashSet;
#[cfg(not(target_arch = "wasm32"))]
use std::fs::{self, File, OpenOptions};
#[cfg(not(target_arch = "wasm32"))]
use std::io::{Seek, SeekFrom, Write};
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::storage::checksum;
use crate::storage::format::{FileHeader, HEADER_SIZE, PAGE_SIZE};
use crate::storage::mmap::MmapReader;
use crate::storage::page::{PageType, SlottedPage, SlottedPageRef};
#[cfg(not(target_arch = "wasm32"))]
use crate::storage::wal::{self, WalEntry, WalWriter};

/// Page allocation, caching and I/O for a TVDB file.
pub struct PageManager {
    /// Backing file; `None` in memory (WASM, scratch fixtures), where writes no-op and callers use
    /// [`PageManager::to_bytes`].
    #[cfg(not(target_arch = "wasm32"))]
    file: Option<File>,
    header: FileHeader,
    /// Page cache, indexed by page_id (contiguous).
    pages: Vec<Box<[u8; PAGE_SIZE]>>,
    /// Pages modified since the last flush.
    dirty: HashSet<u64>,
    /// Free page IDs for reuse.
    free_list: Vec<u64>,
    /// Flush log sequence number (monotonic).
    lsn: u64,
    /// Database path (to derive the WAL path). Empty in memory mode. Native only: wasm32 has no
    /// WAL.
    #[cfg(not(target_arch = "wasm32"))]
    db_path: PathBuf,
    /// The last flush committed its WAL but failed to apply it to the main file: the flush is
    /// durable (recovery replays the WAL), the file isn't settled.
    #[cfg(not(target_arch = "wasm32"))]
    wal_unapplied: bool,
}

impl PageManager {
    /// Creates a new TVDB file at `path`; it must not exist (or be empty).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        let header = FileHeader::new(0, 0);

        let mut pm = Self {
            file: Some(file),
            header,
            pages: Vec::new(),
            dirty: HashSet::new(),
            #[cfg(not(target_arch = "wasm32"))]
            wal_unapplied: false,
            free_list: Vec::new(),
            lsn: 0,
            db_path: path.to_path_buf(),
        };

        pm.write_header()?;

        Ok(pm)
    }

    /// In-memory PageManager from tvdb bytes; works on every target and is the
    /// WASM load path. Same deserializer as native (header, per-page checksum, slot
    /// iteration), so the format is identical.
    ///
    /// No backing file: `flush()` no-ops, persist via [`PageManager::to_bytes`].
    pub fn open_from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let reader = MmapReader::from_data(bytes);
        let header = FileHeader::from_bytes(reader.header_bytes()?)?;
        // the count comes from the file: it must match what the file holds before it sizes anything
        if header.page_count > reader.page_capacity() {
            return Err(Error::InvalidFormat(format!(
                "header claims {} pages, the file holds {}",
                header.page_count,
                reader.page_capacity()
            )));
        }
        let mut pages = Vec::with_capacity(header.page_count as usize);
        let mut free_list = Vec::new();
        for page_id in 0..header.page_count {
            let page_bytes = reader.page_bytes(page_id)?;
            if !checksum::verify_page_checksum(page_bytes) {
                let stored = checksum::read_page_checksum(page_bytes);
                let computed = checksum::compute_page_checksum(page_bytes);
                return Err(Error::ChecksumMismatch {
                    page_id,
                    expected: stored,
                    actual: computed,
                });
            }
            let page_ref = SlottedPageRef::new(page_bytes, page_id);
            page_ref.check_layout()?;
            if page_ref.page_type() == Some(PageType::Free) {
                free_list.push(page_id);
            }
            let mut buf = Box::new([0u8; PAGE_SIZE]);
            buf.copy_from_slice(page_bytes);
            pages.push(buf);
        }
        let lsn = header.lsn;
        drop(reader);
        Ok(Self {
            #[cfg(not(target_arch = "wasm32"))]
            file: None,
            header,
            pages,
            dirty: HashSet::new(),
            #[cfg(not(target_arch = "wasm32"))]
            wal_unapplied: false,
            free_list,
            lsn,
            #[cfg(not(target_arch = "wasm32"))]
            db_path: PathBuf::new(),
        })
    }

    /// Empty in-memory PageManager for fresh WASM databases, filled via Cypher and saved via
    /// [`PageManager::to_bytes`] to OPFS.
    pub fn create_in_memory() -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            file: None,
            header: FileHeader::new(0, 0),
            pages: Vec::new(),
            dirty: HashSet::new(),
            #[cfg(not(target_arch = "wasm32"))]
            wal_unapplied: false,
            free_list: Vec::new(),
            lsn: 0,
            #[cfg(not(target_arch = "wasm32"))]
            db_path: PathBuf::new(),
        }
    }

    /// Whole page state (header + every page by page_id) as tvdb bytes: what
    /// `flush()` would write, but handed to the caller (an OPFS sync access handle
    /// in a WASM worker, or a download).
    ///
    /// The persistence path on wasm32. Works natively too, but `flush()` is faster
    /// (no double buffer).
    pub fn to_bytes(&mut self) -> Vec<u8> {
        // stamp LSN + checksums on dirty pages first, so the bytes match what flush() writes
        self.lsn += 1;
        let dirty_ids: Vec<u64> = self.dirty.iter().copied().collect();
        for &page_id in &dirty_ids {
            if let Some(buf) = self.pages.get_mut(page_id as usize) {
                let mut page = SlottedPage::new(buf.as_mut(), page_id);
                page.set_lsn(self.lsn);
                page.write_checksum();
            }
        }
        self.header.page_count = self.pages.len() as u64;
        self.header.lsn = self.lsn;
        self.dirty.clear();

        let total = HEADER_SIZE + self.pages.len() * PAGE_SIZE;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&self.header.to_bytes());
        for buf in &self.pages {
            out.extend_from_slice(buf.as_ref());
        }
        out
    }

    /// Opens an existing TVDB file, mmap'd for the initial load. Not on wasm32 (no real `File` in a
    /// browser); use [`PageManager::open_from_bytes`].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;

        let metadata = file.metadata()?;
        if metadata.len() < HEADER_SIZE as u64 {
            return Err(Error::InvalidFormat("file too small".into()));
        }

        let reader = MmapReader::new(&file)?;
        let header = FileHeader::from_bytes(reader.header_bytes()?)?;
        // the count comes from the file: it must match what the file holds before it sizes anything
        if header.page_count > reader.page_capacity() {
            return Err(Error::InvalidFormat(format!(
                "header claims {} pages, the file holds {}",
                header.page_count,
                reader.page_capacity()
            )));
        }
        let mut pages = Vec::with_capacity(header.page_count as usize);
        let mut free_list = Vec::new();

        // load every page (contiguous by page_id)
        for page_id in 0..header.page_count {
            let page_bytes = reader.page_bytes(page_id)?;

            if !checksum::verify_page_checksum(page_bytes) {
                let stored = checksum::read_page_checksum(page_bytes);
                let computed = checksum::compute_page_checksum(page_bytes);
                return Err(Error::ChecksumMismatch {
                    page_id,
                    expected: stored,
                    actual: computed,
                });
            }

            let page_ref = SlottedPageRef::new(page_bytes, page_id);
            page_ref.check_layout()?;
            if page_ref.page_type() == Some(PageType::Free) {
                free_list.push(page_id);
            }

            let mut buf = Box::new([0u8; PAGE_SIZE]);
            buf.copy_from_slice(page_bytes);
            pages.push(buf);
        }

        let lsn = header.lsn;

        // drop the mmap before any writes
        drop(reader);

        Ok(Self {
            file: Some(file),
            header,
            pages,
            dirty: HashSet::new(),
            #[cfg(not(target_arch = "wasm32"))]
            wal_unapplied: false,
            free_list,
            lsn,
            db_path: path.to_path_buf(),
        })
    }

    /// Allocates a page, reusing free pages before growing the file; returns its ID.
    pub fn allocate(&mut self, page_type: PageType) -> u64 {
        let page_id = if let Some(id) = self.free_list.pop() {
            id
        } else {
            let id = self.pages.len() as u64;
            self.pages.push(Box::new([0u8; PAGE_SIZE]));
            id
        };

        let buf = &mut self.pages[page_id as usize];
        let mut page = SlottedPage::new(buf.as_mut(), page_id);
        page.init(page_type);
        self.dirty.insert(page_id);

        page_id
    }

    /// Marks a page free for reuse.
    pub fn free_page(&mut self, page_id: u64) {
        if let Some(buf) = self.pages.get_mut(page_id as usize) {
            let mut page = SlottedPage::new(buf.as_mut(), page_id);
            page.init(PageType::Free);
            self.dirty.insert(page_id);
            self.free_list.push(page_id);
        }
    }

    /// Page buffer.
    pub fn get_page(&self, page_id: u64) -> Result<&[u8; PAGE_SIZE]> {
        self.pages
            .get(page_id as usize)
            .map(|b| b.as_ref())
            .ok_or(Error::PageNotFound(page_id))
    }

    /// Mutable page buffer; marks it dirty.
    pub fn get_page_mut(&mut self, page_id: u64) -> Result<&mut [u8; PAGE_SIZE]> {
        self.dirty.insert(page_id);
        self.pages
            .get_mut(page_id as usize)
            .map(|b| b.as_mut())
            .ok_or(Error::PageNotFound(page_id))
    }

    /// File header.
    pub fn header(&self) -> &FileHeader {
        &self.header
    }

    /// Mutable file header.
    pub fn header_mut(&mut self) -> &mut FileHeader {
        &mut self.header
    }

    /// Pages free for reuse.
    pub fn free_page_count(&self) -> usize {
        self.free_list.len()
    }

    /// Total pages, free ones included.
    pub fn total_pages(&self) -> u64 {
        self.pages.len() as u64
    }

    /// Flushes dirty pages and the header via a WAL: stamp LSN -> write WAL -> fsync
    /// WAL -> apply to main -> fsync main -> delete WAL.
    ///
    /// In memory mode: stamps LSN + checksums and clears the dirty set, no I/O
    /// (persist via [`PageManager::to_bytes`]).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn flush(&mut self) -> Result<()> {
        let Some(file) = self.file.as_mut() else {
            // in memory: update the header, no I/O
            self.header.page_count = self.pages.len() as u64;
            self.dirty.clear();
            return Ok(());
        };
        if self.dirty.is_empty() {
            // page count may have changed via allocate, even with nothing dirty
            self.header.page_count = self.pages.len() as u64;
            let bytes = self.header.to_bytes();
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            return Ok(());
        }

        self.lsn += 1;

        // stamp LSN and checksums into dirty pages
        let dirty_ids: Vec<u64> = self.dirty.iter().copied().collect();
        for &page_id in &dirty_ids {
            if let Some(buf) = self.pages.get_mut(page_id as usize) {
                let mut page = SlottedPage::new(buf.as_mut(), page_id);
                page.set_lsn(self.lsn);
                page.write_checksum();
            }
        }

        self.header.page_count = self.pages.len() as u64;
        self.header.lsn = self.lsn;
        let header_bytes = self.header.to_bytes();

        // phase 1: write the WAL
        let wal_path = wal::wal_path(&self.db_path);
        let mut wal_writer = WalWriter::create(&wal_path, self.lsn, self.lsn)?;
        for &page_id in &dirty_ids {
            if let Some(buf) = self.pages.get(page_id as usize) {
                wal_writer.write_page(page_id, buf)?;
            }
        }
        wal_writer.write_header(&header_bytes)?;
        wal_writer.commit()?; // fsync WAL
        // durable from here on: a failure below leaves the WAL for recovery to apply
        self.wal_unapplied = true;

        // phase 2: apply to the main file
        for &page_id in &dirty_ids {
            if let Some(buf) = self.pages.get(page_id as usize) {
                let offset = HEADER_SIZE as u64 + page_id * PAGE_SIZE as u64;
                file.seek(SeekFrom::Start(offset))?;
                file.write_all(buf.as_ref())?;
            }
        }
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header_bytes)?;
        file.sync_all()?;

        // phase 3: checkpoint = delete the WAL. The header `lsn` is the checkpoint LSN;
        // it only reaches the main file after the WAL committed and all pages applied.
        let _ = fs::remove_file(&wal_path);
        self.wal_unapplied = false;

        self.dirty.clear();
        Ok(())
    }

    /// After a failed [`flush`](Self::flush): whether it got past committing its WAL, i.e. its
    /// pages are durable but the main file needs the WAL applied (recovery does that).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn wal_unapplied(&self) -> bool {
        self.wal_unapplied
    }

    /// Replays a committed WAL left by a crash into the main file. No-op if the WAL is uncommitted
    /// or absent.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn recover(path: &Path) -> Result<()> {
        let wal_path = wal::wal_path(path);
        let entries = match wal::read_wal(&wal_path)? {
            Some(e) => e,
            None => return Ok(()),
        };

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| Error::WalRecoveryFailed(format!("cannot open main file: {e}")))?;

        for entry in &entries {
            match entry {
                WalEntry::PageWrite { page_id, data } => {
                    let offset = HEADER_SIZE as u64 + page_id * PAGE_SIZE as u64;
                    // extend the file if needed
                    let needed = offset + PAGE_SIZE as u64;
                    let len = file.metadata()?.len();
                    if len < needed {
                        file.set_len(needed)?;
                    }
                    file.seek(SeekFrom::Start(offset))?;
                    file.write_all(data.as_ref())?;
                }
                WalEntry::HeaderWrite { data } => {
                    file.seek(SeekFrom::Start(0))?;
                    file.write_all(data.as_ref())?;
                }
                WalEntry::Commit => {}
            }
        }

        file.sync_all()?;
        let _ = fs::remove_file(&wal_path);
        Ok(())
    }

    /// Current flush LSN.
    pub fn lsn(&self) -> u64 {
        self.lsn
    }

    /// Underlying file; `None` in memory.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn file(&self) -> Option<&File> {
        self.file.as_ref()
    }

    // -- Private helpers ----------------------------------------------

    #[cfg(not(target_arch = "wasm32"))]
    fn write_header(&mut self) -> Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Ok(()); // in memory: header lives in self.header
        };
        let bytes = self.header.to_bytes();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&bytes)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_allocate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tvdb");

        let mut pm = PageManager::create(&path).unwrap();
        let p0 = pm.allocate(PageType::NodeData);
        let p1 = pm.allocate(PageType::EdgeData);
        assert_eq!(p0, 0);
        assert_eq!(p1, 1);
        assert_eq!(pm.total_pages(), 2);
    }

    #[test]
    fn free_and_reallocate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tvdb");

        let mut pm = PageManager::create(&path).unwrap();
        let p0 = pm.allocate(PageType::NodeData);
        let _p1 = pm.allocate(PageType::EdgeData);

        pm.free_page(p0);
        // reallocating reuses the freed page
        let p2 = pm.allocate(PageType::NodeData);
        assert_eq!(p2, p0);
    }

    #[test]
    fn flush_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tvdb");

        // Create and write data
        {
            let mut pm = PageManager::create(&path).unwrap();
            let page_id = pm.allocate(PageType::NodeData);
            {
                let buf = pm.get_page_mut(page_id).unwrap();
                let mut page = SlottedPage::new(buf.as_mut(), page_id);
                page.insert_record(b"hello").unwrap();
            }
            pm.flush().unwrap();
        }

        // Reopen and verify
        {
            let pm = PageManager::open(&path).unwrap();
            assert_eq!(pm.total_pages(), 1);
            let buf = pm.get_page(0).unwrap();
            let page = SlottedPageRef::new(buf.as_ref(), 0);
            assert_eq!(page.read_record(0).unwrap(), b"hello");
        }
    }

    #[test]
    fn free_list_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tvdb");

        {
            let mut pm = PageManager::create(&path).unwrap();
            let p0 = pm.allocate(PageType::NodeData);
            pm.allocate(PageType::NodeData);
            pm.free_page(p0);
            pm.flush().unwrap();
        }

        {
            let mut pm = PageManager::open(&path).unwrap();
            // freed page is reusable
            let reallocated = pm.allocate(PageType::NodeData);
            assert_eq!(reallocated, 0);
        }
    }

    #[test]
    fn get_nonexistent_page_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tvdb");

        let pm = PageManager::create(&path).unwrap();
        assert!(pm.get_page(999).is_err());
    }

    #[test]
    fn flush_creates_and_removes_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tvdb");
        let wal = crate::storage::wal::wal_path(&path);

        let mut pm = PageManager::create(&path).unwrap();
        pm.allocate(PageType::NodeData);
        pm.flush().unwrap();

        // no WAL left after a successful flush
        assert!(!wal.exists(), "WAL file should be removed after flush");
    }

    #[test]
    fn lsn_increments_on_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tvdb");

        let mut pm = PageManager::create(&path).unwrap();
        assert_eq!(pm.lsn(), 0);

        pm.allocate(PageType::NodeData);
        pm.flush().unwrap();
        assert_eq!(pm.lsn(), 1);

        // another page, to make it dirty
        pm.allocate(PageType::EdgeData);
        pm.flush().unwrap();
        assert_eq!(pm.lsn(), 2);
    }

    #[test]
    fn lsn_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tvdb");

        {
            let mut pm = PageManager::create(&path).unwrap();
            pm.allocate(PageType::NodeData);
            pm.flush().unwrap();
            pm.allocate(PageType::EdgeData);
            pm.flush().unwrap();
            assert_eq!(pm.lsn(), 2);
        }

        {
            let pm = PageManager::open(&path).unwrap();
            assert_eq!(pm.lsn(), 2);
        }
    }

    #[test]
    fn recovery_replays_committed_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tvdb");

        // Create a file with one page
        {
            let mut pm = PageManager::create(&path).unwrap();
            let page_id = pm.allocate(PageType::NodeData);
            {
                let buf = pm.get_page_mut(page_id).unwrap();
                let mut page = SlottedPage::new(buf.as_mut(), page_id);
                page.insert_record(b"original").unwrap();
            }
            pm.flush().unwrap();
        }

        // simulate crash: WAL with an updated page, never applied
        {
            let pm = PageManager::open(&path).unwrap();
            let mut modified_page = *pm.get_page(0).unwrap();
            let mut page = SlottedPage::new(&mut modified_page, 0);
            page.insert_record(b"recovered").unwrap();
            page.set_lsn(2);
            page.write_checksum();

            let mut hdr = pm.header().clone();
            hdr.lsn = 2;
            let hdr_bytes = hdr.to_bytes();

            let wal_path = crate::storage::wal::wal_path(&path);
            let mut w = crate::storage::wal::WalWriter::create(&wal_path, 2, 2).unwrap();
            w.write_page(0, &modified_page).unwrap();
            w.write_header(&hdr_bytes).unwrap();
            w.commit().unwrap();
        }

        // recovery replays the WAL
        PageManager::recover(&path).unwrap();

        let pm = PageManager::open(&path).unwrap();
        let buf = pm.get_page(0).unwrap();
        let page = SlottedPageRef::new(buf.as_ref(), 0);
        // both the original and the recovered record
        assert_eq!(page.read_record(0).unwrap(), b"original");
        assert_eq!(page.read_record(1).unwrap(), b"recovered");
        assert_eq!(pm.lsn(), 2);
    }

    #[test]
    fn recovery_discards_uncommitted_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tvdb");

        // Create a file
        {
            let mut pm = PageManager::create(&path).unwrap();
            let page_id = pm.allocate(PageType::NodeData);
            {
                let buf = pm.get_page_mut(page_id).unwrap();
                let mut page = SlottedPage::new(buf.as_mut(), page_id);
                page.insert_record(b"original").unwrap();
            }
            pm.flush().unwrap();
        }

        // uncommitted WAL (no commit)
        {
            let wal_path = crate::storage::wal::wal_path(&path);
            let mut w = crate::storage::wal::WalWriter::create(&wal_path, 99, 99).unwrap();
            w.write_page(0, &[0xFFu8; PAGE_SIZE]).unwrap();
            // Drop without commit
        }

        // recovery discards it; original intact
        PageManager::recover(&path).unwrap();

        let pm = PageManager::open(&path).unwrap();
        let buf = pm.get_page(0).unwrap();
        let page = SlottedPageRef::new(buf.as_ref(), 0);
        assert_eq!(page.read_record(0).unwrap(), b"original");
        assert_eq!(pm.lsn(), 1);
    }
}
