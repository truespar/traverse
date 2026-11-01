//! Single-file `.tvdb` storage.
//!
//! 4096-byte file header, then 16384-byte slotted pages with 64-byte headers and
//! CRC32 checksums; records are LZ4-compressed one by one. `flush()` writes only
//! dirty/deleted entities in place, crash-safe via a WAL. `compact()` rewrites
//! atomically (temp + rename) to defragment and reclaim space.
//!
//! On `wasm32-unknown-unknown` the file-bound parts (WAL, `File`/`Path`
//! constructors, `Database`) are compiled out; the in-memory codecs (page
//! serialization, graph reconstruction, `PageManager::open_from_bytes` /
//! `PageManager::to_bytes`) remain, so `traverse-wasm` round-trips tvdb bytes
//! without `std::fs`.
//!
//! - [`archive`]: validated rkyv decoding for bytes read back from disk
//! - [`format`](mod@format): file header, magic, version, constants
//! - [`checksum`]: CRC32 helpers
//! - [`compression`]: feature-gated LZ4
//! - [`page`]: slotted pages (slot directory + record data + tombstones)
//! - [`catalog`]: catalog metadata (string table, ID counters)
//! - [`location`]: entity ID -> page/slot index
//! - [`mmap`]: memory-mapped reader
//! - [`page_manager`]: page allocation, I/O, free list, WAL-backed flush
//! - [`wal`]: page WAL for crash-safe flush
//! - [`database`]: `Database` open/flush/compact/close

pub mod archive;
pub mod catalog;
pub mod checksum;
pub mod codec;
pub mod compression;
// Database glues lock file, WAL recovery, save via rename: file-bound, so not on wasm32.
// codec/page_manager stay available for WASM's bytes in/out.
#[cfg(not(target_arch = "wasm32"))]
pub mod database;
pub mod format;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod free_space;
pub mod location;
pub mod mmap;
pub(crate) mod overflow;
pub mod page;
pub mod page_manager;
// file-bound (open, append, fsync); browsers have none. WASM flush = serialize and hand bytes to JS
// for OPFS
#[cfg(not(target_arch = "wasm32"))]
pub mod wal;
// logical redo log, per-commit durability; file-bound like `wal`, so not on wasm32
#[cfg(not(target_arch = "wasm32"))]
pub mod rlog;

#[cfg(not(target_arch = "wasm32"))]
pub use database::Database;
pub use format::{FileHeader, HEADER_SIZE, PAGE_HEADER_SIZE, PAGE_SIZE};
pub use location::{LocationIndex, RecordLocation};
pub use page::PageType;

/// Replaces `path` with `bytes` so that a crash leaves either the old file or the new one, never a
/// torn mix, and the new one survives once this returns: written to a temp file next to it,
/// synced, then [`rename_durable`]d over it. For small files saved whole (models, catalogs).
#[cfg(not(target_arch = "wasm32"))]
pub fn write_durable(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    // unique per process and call, so concurrent writers of one path never share a temp file
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut tmp_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?
        .to_os_string();
    tmp_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(tmp_name);
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        rename_durable(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Renames `from` over `to` and makes the rename itself durable, so a crash can't bring back the
/// old file: the parent directory is synced (Unix), or the move is write-through (Windows).
/// Installing a snapshot this way is what lets the redo log behind it be truncated.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn rename_durable(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        let wide = |p: &std::path::Path| -> Vec<u16> {
            p.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        };
        let (from_w, to_w) = (wide(from), wide(to));
        // SAFETY: both are NUL-terminated UTF-16 paths that outlive the call
        let ok = unsafe {
            MoveFileExW(
                from_w.as_ptr(),
                to_w.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)?;
        let dir = match to.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => std::path::Path::new("."),
        };
        std::fs::File::open(dir)?.sync_all()
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    #[test]
    fn rename_durable_replaces_the_target() {
        let dir = tempfile::TempDir::new().unwrap();
        let (a, b) = (dir.path().join("a.tmp"), dir.path().join("b.tvdb"));
        std::fs::write(&a, b"new").unwrap();
        std::fs::write(&b, b"old").unwrap();
        super::rename_durable(&a, &b).unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), b"new");
        assert!(!a.exists());
    }

    #[test]
    fn write_durable_replaces_whole_files_and_leaves_no_temp() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.bin");
        super::write_durable(&path, b"first version").unwrap();
        super::write_durable(&path, b"2nd").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"2nd");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("models.bin")]);
    }

    #[test]
    fn write_durable_into_a_missing_directory_fails_cleanly() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("gone").join("x.bin");
        assert!(super::write_durable(&path, b"x").is_err());
        assert!(!dir.path().join("gone").exists());
    }
}
