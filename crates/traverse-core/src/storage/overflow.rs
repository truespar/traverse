//! Records larger than a page. The bytes go to a chain of overflow pages and the record's slot holds
//! a stub pointing at them, so no node, relationship, catalog entry or metadata blob is limited by
//! the page size. A record that fits a page stays in its slot as before, and a file without
//! overflow pages is still written as format v6.
//!
//! Stub, in the slot (21 bytes): `[0x02][len: u64 LE][first: u64 LE][crc32: u32 LE]`. `len` and the
//! CRC cover the record as [`compression::encode_record`](super::compression::encode_record) made
//! it (raw or LZ4).
//!
//! Overflow page: a slotted page of type [`PageType::Overflow`] holding one record,
//! `[next: u64 LE][chunk]`; `next` is `u64::MAX` on the last page.

use std::borrow::Cow;

use crate::error::{Error, Result};
use crate::storage::compression::RECORD_FLAG_OVERFLOW;
use crate::storage::format::PAGE_DATA_CAPACITY;
#[cfg(not(target_arch = "wasm32"))]
use crate::storage::format::PAGE_SIZE;
use crate::storage::page::{PageType, SLOT_ENTRY_SIZE, SlottedPage, SlottedPageRef};
use crate::storage::page_manager::PageManager;

/// Largest record kept in its slot: one that fills a page.
pub(crate) const MAX_INLINE: usize = PAGE_DATA_CAPACITY - SLOT_ENTRY_SIZE;

/// Record bytes per overflow page.
const CHUNK: usize = MAX_INLINE - 8;

/// `next` of the last page of a chain.
const END: u64 = u64::MAX;

/// Stub size.
const STUB_SIZE: usize = 21;

/// `encoded` as it goes in a slot: itself when it fits a page, else a stub to overflow pages
/// written here (the file is v7 from then on).
pub(crate) fn store(pm: &mut PageManager, encoded: Vec<u8>) -> Result<Vec<u8>> {
    if encoded.len() <= MAX_INLINE {
        return Ok(encoded);
    }
    spill(pm, encoded)
}

/// Writes `encoded` to overflow pages whatever its size; returns the stub for its slot. For a
/// page whose records fit one by one but not together (the catalog).
pub(crate) fn spill(pm: &mut PageManager, encoded: Vec<u8>) -> Result<Vec<u8>> {
    if encoded.is_empty() {
        return Err(Error::InvalidFormat(
            "an empty record has nothing to spill".into(),
        ));
    }
    let pages: Vec<u64> = (0..encoded.len().div_ceil(CHUNK))
        .map(|_| pm.allocate(PageType::Overflow))
        .collect();
    for (i, chunk) in encoded.chunks(CHUNK).enumerate() {
        let buf = pm.get_page_mut(pages[i])?;
        SlottedPage::new(buf.as_mut(), pages[i]).insert_record(&link(&pages, i, chunk))?;
    }
    pm.header_mut().note_overflow();
    Ok(stub(&encoded, pages[0]))
}

/// [`spill`] for a writer appending to a file without a page manager: the chain goes on new pages
/// numbered from `first`. Returns the stub and the pages, initialized but not yet stamped with
/// an LSN and checksum.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn spill_appended(
    encoded: &[u8],
    first: u64,
) -> Result<(Vec<u8>, Vec<(u64, Box<[u8; PAGE_SIZE]>)>)> {
    if encoded.is_empty() {
        return Err(Error::InvalidFormat(
            "an empty record has nothing to spill".into(),
        ));
    }
    let ids: Vec<u64> = (first..).take(encoded.len().div_ceil(CHUNK)).collect();
    let mut pages = Vec::with_capacity(ids.len());
    for (i, chunk) in encoded.chunks(CHUNK).enumerate() {
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        let mut page = SlottedPage::new(buf.as_mut(), ids[i]);
        page.init(PageType::Overflow);
        page.insert_record(&link(&ids, i, chunk))?;
        pages.push((ids[i], buf));
    }
    Ok((stub(encoded, first), pages))
}

/// Overflow page `i` of a chain on `pages`: the next page, then the chunk.
fn link(pages: &[u64], i: usize, chunk: &[u8]) -> Vec<u8> {
    let next = pages.get(i + 1).copied().unwrap_or(END);
    let mut record = Vec::with_capacity(8 + chunk.len());
    record.extend_from_slice(&next.to_le_bytes());
    record.extend_from_slice(chunk);
    record
}

/// The slot's stub for `encoded` on a chain starting at `first`.
fn stub(encoded: &[u8], first: u64) -> Vec<u8> {
    let mut stub = Vec::with_capacity(STUB_SIZE);
    stub.push(RECORD_FLAG_OVERFLOW);
    stub.extend_from_slice(&(encoded.len() as u64).to_le_bytes());
    stub.extend_from_slice(&first.to_le_bytes());
    stub.extend_from_slice(&crc32fast::hash(encoded).to_le_bytes());
    stub
}

/// The record a slot holds: the slot itself, or a stub's record read back from its overflow pages
/// (chain, length and CRC checked).
pub(crate) fn load<'a>(pm: &PageManager, slot: &'a [u8]) -> Result<Cow<'a, [u8]>> {
    let Some(stub) = Stub::parse(slot)? else {
        return Ok(Cow::Borrowed(slot));
    };
    stub.check_size(pm)?;
    let mut record = Vec::with_capacity(stub.len);
    stub.walk(pm, |_, chunk| record.extend_from_slice(chunk))?;
    if crc32fast::hash(&record) != stub.crc {
        return Err(Error::InvalidFormat(format!(
            "overflow record at page {}: checksum mismatch",
            stub.first
        )));
    }
    Ok(Cow::Owned(record))
}

/// Frees the overflow pages of the record in `slot`, if it has any: before the slot is tombstoned
/// or the page holding it is freed. In-place saves free them; those are native-only.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn free(pm: &mut PageManager, slot: &[u8]) -> Result<()> {
    let Some(stub) = Stub::parse(slot)? else {
        return Ok(());
    };
    stub.check_size(pm)?;
    let mut pages = Vec::new();
    stub.walk(pm, |page, _| pages.push(page))?;
    for page in pages {
        pm.free_page(page);
    }
    Ok(())
}

/// [`free`] for the record in slot `slot_idx` of page `page_id`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn free_slot(pm: &mut PageManager, page_id: u64, slot_idx: u16) -> Result<()> {
    let buf = pm.get_page(page_id)?;
    let slot = SlottedPageRef::new(buf.as_ref(), page_id).read_record(slot_idx)?;
    if slot.first() != Some(&RECORD_FLAG_OVERFLOW) {
        return Ok(());
    }
    let stub = slot.to_vec();
    free(pm, &stub)
}

/// [`free`] for every live record of page `page_id`: before the page is freed or replaced.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn free_page_records(pm: &mut PageManager, page_id: u64) -> Result<()> {
    let buf = pm.get_page(page_id)?;
    let page = SlottedPageRef::new(buf.as_ref(), page_id);
    let mut stubs = Vec::new();
    for slot in 0..page.slot_count() {
        if page.is_tombstone(slot) {
            continue;
        }
        let record = page.read_record(slot)?;
        if record.first() == Some(&RECORD_FLAG_OVERFLOW) {
            stubs.push(record.to_vec());
        }
    }
    for stub in stubs {
        free(pm, &stub)?;
    }
    Ok(())
}

/// A slot's pointer to a record on overflow pages.
struct Stub {
    len: usize,
    first: u64,
    crc: u32,
}

impl Stub {
    /// `None` for a record kept in its slot.
    fn parse(slot: &[u8]) -> Result<Option<Self>> {
        if slot.first() != Some(&RECORD_FLAG_OVERFLOW) {
            return Ok(None);
        }
        let bytes: &[u8; STUB_SIZE] = slot
            .try_into()
            .map_err(|_| Error::InvalidFormat(format!("overflow stub of {} bytes", slot.len())))?;
        let len = u64::from_le_bytes(bytes[1..9].try_into().unwrap());
        Ok(Some(Self {
            len: usize::try_from(len)
                .map_err(|_| Error::InvalidFormat(format!("overflow record of {len} bytes")))?,
            first: u64::from_le_bytes(bytes[9..17].try_into().unwrap()),
            crc: u32::from_le_bytes(bytes[17..21].try_into().unwrap()),
        }))
    }

    /// Pages the chain takes.
    fn pages(&self) -> usize {
        self.len.div_ceil(CHUNK)
    }

    /// The claimed length is one a chain in this file can hold: checked before anything is sized
    /// by it.
    fn check_size(&self, pm: &PageManager) -> Result<()> {
        if self.len == 0 || self.pages() as u64 > pm.total_pages() {
            return Err(Error::InvalidFormat(format!(
                "overflow record at page {} claims {} bytes, the file holds {} pages",
                self.first,
                self.len,
                pm.total_pages()
            )));
        }
        Ok(())
    }

    /// Calls `f(page, chunk)` along the chain: overflow pages only, no more of them than the
    /// length needs (so no loop), exactly `len` bytes in all.
    fn walk(&self, pm: &PageManager, mut f: impl FnMut(u64, &[u8])) -> Result<()> {
        let bad = |page: u64, what: &str| {
            Err(Error::InvalidFormat(format!(
                "overflow record at page {}: page {page} {what}",
                self.first
            )))
        };
        let (mut page, mut visited, mut total) = (self.first, 0, 0);
        while page != END {
            visited += 1;
            if visited > self.pages() {
                return bad(page, "makes the chain longer than its record");
            }
            let buf = pm.get_page(page)?;
            let overflow = SlottedPageRef::new(buf.as_ref(), page);
            if overflow.page_type() != Some(PageType::Overflow) || overflow.slot_count() != 1 {
                return bad(page, "isn't an overflow page");
            }
            let record = overflow.read_record(0)?;
            let Some((next, chunk)) = record.split_first_chunk::<8>() else {
                return bad(page, "is empty");
            };
            total += chunk.len();
            if total > self.len {
                return bad(page, "holds more than the record");
            }
            f(page, chunk);
            page = u64::from_le_bytes(*next);
        }
        if total != self.len {
            return Err(Error::InvalidFormat(format!(
                "overflow record at page {}: {total} of {} bytes",
                self.first, self.len
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page_manager::PageManager;

    fn noise(len: usize) -> Vec<u8> {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn small_records_stay_in_their_slot() {
        let mut pm = PageManager::create_in_memory();
        let record = noise(MAX_INLINE);
        assert_eq!(store(&mut pm, record.clone()).unwrap(), record);
        assert_eq!(pm.total_pages(), 0);
        assert_eq!(
            pm.header().version,
            crate::storage::format::FORMAT_VERSION_V6
        );
    }

    #[test]
    fn a_large_record_round_trips_through_its_chain_and_frees_it() {
        let mut pm = PageManager::create_in_memory();
        let record = noise(3 * CHUNK + 17);
        let stub = store(&mut pm, record.clone()).unwrap();
        assert_eq!(stub.len(), STUB_SIZE);
        assert_eq!(pm.total_pages(), 4);
        assert_eq!(
            pm.header().version,
            crate::storage::format::FORMAT_VERSION_V7
        );
        assert_eq!(load(&pm, &stub).unwrap().as_ref(), record.as_slice());

        free(&mut pm, &stub).unwrap();
        // the freed pages come back for the next record
        let again = store(&mut pm, noise(2 * CHUNK)).unwrap();
        assert_eq!(pm.total_pages(), 4);
        assert_eq!(load(&pm, &again).unwrap().len(), 2 * CHUNK);
    }

    #[test]
    fn damaged_chains_are_refused() {
        let mut pm = PageManager::create_in_memory();
        let stub = store(&mut pm, noise(2 * CHUNK + 1)).unwrap();
        let with = |patch: &dyn Fn(&mut Vec<u8>)| {
            let mut s = stub.clone();
            patch(&mut s);
            load(&pm, &s).unwrap_err().to_string()
        };
        // a length the file can't hold, one the chain doesn't match, a wrong checksum
        assert!(with(&|s| s[1..9].copy_from_slice(&u64::MAX.to_le_bytes())).contains("claims"));
        assert!(
            with(&|s| s[1..9].copy_from_slice(&((2 * CHUNK) as u64).to_le_bytes()))
                .contains("longer than its record")
        );
        // one byte more than the chain holds
        assert!(
            with(&|s| s[1..9].copy_from_slice(&((2 * CHUNK + 2) as u64).to_le_bytes()))
                .contains(&format!("{} of {} bytes", 2 * CHUNK + 1, 2 * CHUNK + 2))
        );
        assert!(with(&|s| s[17] ^= 1).contains("checksum"));
        // a chain looping back on itself
        let first = u64::from_le_bytes(stub[9..17].try_into().unwrap());
        let mut looped = PageManager::create_in_memory();
        let page = looped.allocate(PageType::Overflow);
        // room for the claimed length, so the walk itself finds the loop
        looped.allocate(PageType::NodeData);
        looped.allocate(PageType::NodeData);
        let mut record = page.to_le_bytes().to_vec();
        record.extend_from_slice(&noise(CHUNK));
        let buf = looped.get_page_mut(page).unwrap();
        SlottedPage::new(buf.as_mut(), page)
            .insert_record(&record)
            .unwrap();
        let mut s = stub.clone();
        s[9..17].copy_from_slice(&page.to_le_bytes());
        assert!(load(&looped, &s).is_err());
        // a stub pointing at a page that isn't an overflow page
        let data = pm.allocate(PageType::NodeData);
        let mut s = stub.clone();
        s[9..17].copy_from_slice(&data.to_le_bytes());
        assert!(
            load(&pm, &s)
                .unwrap_err()
                .to_string()
                .contains("isn't an overflow page")
        );
        assert_ne!(first, data);
    }
}
