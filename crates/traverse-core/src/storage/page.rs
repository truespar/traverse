//! Slotted page layout for variable-size records.
//!
//! ```text
//! +------------------------ PAGE_SIZE (16384) ------------------------+
//! | Page Header (64 bytes)                                            |
//! +-------------------------------------------------------------------+
//! | Slot Directory -> grows forward (4 bytes per slot: offset + len)  |
//! | ...                                                               |
//! +-------- free_space_offset ---------- data_end_offset -------------+
//! |              (free space)                                         |
//! +-------------------------------------------------------------------+
//! | Record data <- grows backward from end of page                    |
//! +-------------------------------------------------------------------+
//! ```
//!
//! Page header (64 bytes):
//!
//! - `[0..8]`   page_id: u64 LE
//! - `[8]`      page_type: u8
//! - `[9]`      flags: u8 (reserved)
//! - `[10..12]` slot_count: u16 LE
//! - `[12..14]` free_space_offset: u16 LE (start of free space)
//! - `[14..16]` data_end_offset: u16 LE (end of record data region)
//! - `[16..24]` lsn: u64 LE (log sequence number)
//! - `[24..40]` reserved
//! - `[40..44]` checksum: u32 LE (CRC32)
//! - `[44..64]` reserved

use crate::error::{Error, Result};
use crate::storage::checksum;
use crate::storage::format::{PAGE_HEADER_SIZE, PAGE_SIZE};

/// Slot directory entry: 2-byte offset + 2-byte length.
pub(crate) const SLOT_ENTRY_SIZE: usize = 4;

/// Slot offset marking a tombstoned (deleted) slot.
const TOMBSTONE_OFFSET: u16 = 0xFFFF;

/// What a page stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    /// Node data records.
    NodeData = 1,
    /// Edge data records.
    EdgeData = 2,
    /// Catalog metadata.
    Catalog = 3,
    /// Free, reusable.
    Free = 4,
    /// Server metadata (auth catalog, ...).
    Metadata = 5,
    /// Part of a record larger than a page (see `storage::overflow`).
    Overflow = 6,
}

impl PageType {
    /// From a raw byte; `None` if invalid.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::NodeData),
            2 => Some(Self::EdgeData),
            3 => Some(Self::Catalog),
            4 => Some(Self::Free),
            5 => Some(Self::Metadata),
            6 => Some(Self::Overflow),
            _ => None,
        }
    }
}

/// Mutable view of a slotted page over a `PAGE_SIZE` buffer.
pub struct SlottedPage<'a> {
    buf: &'a mut [u8],
    page_id: u64,
}

impl<'a> SlottedPage<'a> {
    /// Wraps an existing buffer of exactly `PAGE_SIZE` bytes.
    pub fn new(buf: &'a mut [u8], page_id: u64) -> Self {
        debug_assert_eq!(buf.len(), PAGE_SIZE);
        Self { buf, page_id }
    }

    /// Initializes the page header.
    pub fn init(&mut self, page_type: PageType) {
        self.buf.fill(0);

        self.buf[0..8].copy_from_slice(&self.page_id.to_le_bytes());
        self.buf[8] = page_type as u8;
        self.buf[9] = 0; // flags
        self.set_slot_count(0);
        // free space starts right after the header (no slots yet)
        self.set_free_space_offset(PAGE_HEADER_SIZE as u16);
        // data end starts at the page end
        self.set_data_end_offset(PAGE_SIZE as u16);
    }

    /// Slot count.
    pub fn slot_count(&self) -> u16 {
        u16::from_le_bytes(self.buf[10..12].try_into().unwrap())
    }

    /// Free space for new records and slots.
    pub fn free_space(&self) -> usize {
        let fso = self.free_space_offset() as usize;
        let deo = self.data_end_offset() as usize;
        deo.saturating_sub(fso)
    }

    /// [`insert_record`](Self::insert_record) for a page nothing was deleted from: no slot
    /// is reused, so none is scanned.
    pub fn append_record(&mut self, data: &[u8]) -> Result<u16> {
        let needed = data.len() + SLOT_ENTRY_SIZE;
        if needed > self.free_space() {
            return Err(Error::PageFull(self.page_id));
        }
        let new_data_end = self.data_end_offset() as usize - data.len();
        self.buf[new_data_end..new_data_end + data.len()].copy_from_slice(data);
        self.set_data_end_offset(new_data_end as u16);
        let slot = self.slot_count();
        self.set_slot_count(slot + 1);
        self.set_free_space_offset(
            (PAGE_HEADER_SIZE + (slot as usize + 1) * SLOT_ENTRY_SIZE) as u16,
        );
        self.set_slot(slot, new_data_end as u16, data.len() as u16);
        Ok(slot)
    }

    /// Inserts a record; returns its slot. Reuses a deleted record's slot when there is one, and
    /// packs the live records together first when only the bytes of deleted ones make room.
    /// `Error::PageFull` if it doesn't fit.
    pub fn insert_record(&mut self, data: &[u8]) -> Result<u16> {
        let reuse = self.first_tombstone();
        let needed = data.len() + if reuse.is_some() { 0 } else { SLOT_ENTRY_SIZE };
        if needed > self.free_space() {
            if needed > self.free_space() + self.reclaimable() {
                return Err(Error::PageFull(self.page_id));
            }
            self.compact();
        }

        // record space grows backward from data_end
        let new_data_end = self.data_end_offset() as usize - data.len();
        self.buf[new_data_end..new_data_end + data.len()].copy_from_slice(data);
        self.set_data_end_offset(new_data_end as u16);

        // a deleted slot, or a new entry where the directory ends
        let slot_idx = match reuse {
            Some(slot) => slot,
            None => {
                let slot = self.slot_count();
                self.set_slot_count(slot + 1);
                self.set_free_space_offset(
                    (PAGE_HEADER_SIZE + (slot as usize + 1) * SLOT_ENTRY_SIZE) as u16,
                );
                slot
            }
        };
        self.set_slot(slot_idx, new_data_end as u16, data.len() as u16);

        Ok(slot_idx)
    }

    /// Bytes of deleted records still in the page, which [`compact`](Self::compact) frees. A
    /// deleted slot keeps its record's length until then.
    pub fn reclaimable(&self) -> usize {
        self.view().reclaimable()
    }

    /// The largest record [`insert_record`](Self::insert_record) can take: free space plus the
    /// bytes of deleted records, less a slot entry unless a deleted slot can be reused.
    pub fn available(&self) -> usize {
        self.view().available()
    }

    /// Moves the live records together at the end of the page, each keeping its slot, so the
    /// bytes of deleted ones become free space; deleted slots keep their entry with length 0.
    pub fn compact(&mut self) {
        let mut live: Vec<(u16, u16, u16)> = (0..self.slot_count())
            .filter_map(|slot| self.slot(slot).map(|(offset, len)| (slot, offset, len)))
            .filter(|&(_, offset, _)| offset != TOMBSTONE_OFFSET)
            .collect();
        // highest first: each record moves towards the end of the page, over bytes already read
        live.sort_unstable_by_key(|&(_, offset, _)| std::cmp::Reverse(offset));
        let mut end = PAGE_SIZE;
        for (slot, offset, len) in live {
            let (from, len) = (offset as usize, len as usize);
            self.buf.copy_within(from..from + len, end - len);
            end -= len;
            self.set_slot(slot, end as u16, len as u16);
        }
        for slot in 0..self.slot_count() {
            if self
                .slot(slot)
                .is_some_and(|(offset, _)| offset == TOMBSTONE_OFFSET)
            {
                self.set_slot(slot, TOMBSTONE_OFFSET, 0);
            }
        }
        self.set_data_end_offset(end as u16);
    }

    fn first_tombstone(&self) -> Option<u16> {
        self.view().first_tombstone()
    }

    fn slot(&self, slot_idx: u16) -> Option<(u16, u16)> {
        self.view().slot(slot_idx)
    }

    fn view(&self) -> SlottedPageRef<'_> {
        SlottedPageRef::new(self.buf, self.page_id)
    }

    fn set_slot(&mut self, slot_idx: u16, offset: u16, len: u16) {
        let at = PAGE_HEADER_SIZE + (slot_idx as usize) * SLOT_ENTRY_SIZE;
        self.buf[at..at + 2].copy_from_slice(&offset.to_le_bytes());
        self.buf[at + 2..at + 4].copy_from_slice(&len.to_le_bytes());
    }

    /// Tombstones a slot (deleted). Idempotent.
    pub fn tombstone_slot(&mut self, slot_idx: u16) -> Result<()> {
        if slot_idx >= self.slot_count() {
            return Err(Error::PageNotFound(self.page_id));
        }

        let slot_offset = PAGE_HEADER_SIZE + (slot_idx as usize) * SLOT_ENTRY_SIZE;
        self.buf[slot_offset..slot_offset + 2].copy_from_slice(&TOMBSTONE_OFFSET.to_le_bytes());
        Ok(())
    }

    /// Slot tombstoned?
    pub fn is_tombstone(&self, slot_idx: u16) -> bool {
        if slot_idx >= self.slot_count() {
            return false;
        }
        let slot_offset = PAGE_HEADER_SIZE + (slot_idx as usize) * SLOT_ENTRY_SIZE;
        let offset = u16::from_le_bytes(self.buf[slot_offset..slot_offset + 2].try_into().unwrap());
        offset == TOMBSTONE_OFFSET
    }

    /// Live (non-tombstoned) slots.
    pub fn live_slot_count(&self) -> u16 {
        let total = self.slot_count();
        let mut live = 0u16;
        for i in 0..total {
            if !self.is_tombstone(i) {
                live += 1;
            }
        }
        live
    }

    /// Sets the header LSN `[16..24]`.
    pub fn set_lsn(&mut self, lsn: u64) {
        self.buf[16..24].copy_from_slice(&lsn.to_le_bytes());
    }

    /// Record bytes in slot `slot_idx` (tests).
    #[cfg(test)]
    fn read_record(&self, slot_idx: u16) -> Result<&[u8]> {
        SlottedPageRef::new(self.buf, self.page_id).read_record(slot_idx)
    }

    /// Header LSN `[16..24]` (tests).
    #[cfg(test)]
    fn lsn(&self) -> u64 {
        u64::from_le_bytes(self.buf[16..24].try_into().unwrap())
    }

    /// Writes the header CRC32.
    pub fn write_checksum(&mut self) {
        checksum::write_page_checksum(self.buf);
    }

    /// Verifies the header CRC32.
    pub fn verify_checksum(&self) -> bool {
        checksum::verify_page_checksum(self.buf)
    }

    // -- Private helpers ----------------------------------------------

    fn free_space_offset(&self) -> u16 {
        u16::from_le_bytes(self.buf[12..14].try_into().unwrap())
    }

    fn data_end_offset(&self) -> u16 {
        u16::from_le_bytes(self.buf[14..16].try_into().unwrap())
    }

    fn set_slot_count(&mut self, count: u16) {
        self.buf[10..12].copy_from_slice(&count.to_le_bytes());
    }

    fn set_free_space_offset(&mut self, offset: u16) {
        self.buf[12..14].copy_from_slice(&offset.to_le_bytes());
    }

    fn set_data_end_offset(&mut self, offset: u16) {
        self.buf[14..16].copy_from_slice(&offset.to_le_bytes());
    }
}

/// Read-only view of a slotted page.
pub struct SlottedPageRef<'a> {
    buf: &'a [u8],
    page_id: u64,
}

impl<'a> SlottedPageRef<'a> {
    /// Wraps a byte slice read-only.
    pub fn new(buf: &'a [u8], page_id: u64) -> Self {
        debug_assert_eq!(buf.len(), PAGE_SIZE);
        Self { buf, page_id }
    }

    /// Page type.
    pub fn page_type(&self) -> Option<PageType> {
        PageType::from_u8(self.buf[8])
    }

    /// Slot count.
    pub fn slot_count(&self) -> u16 {
        u16::from_le_bytes(self.buf[10..12].try_into().unwrap())
    }

    /// Slot directory entry `(offset, len)`; `None` past the slot count or the page.
    fn slot(&self, slot_idx: u16) -> Option<(u16, u16)> {
        if slot_idx >= self.slot_count() {
            return None;
        }
        let at = PAGE_HEADER_SIZE + (slot_idx as usize) * SLOT_ENTRY_SIZE;
        let entry = self.buf.get(at..at + SLOT_ENTRY_SIZE)?;
        Some((
            u16::from_le_bytes([entry[0], entry[1]]),
            u16::from_le_bytes([entry[2], entry[3]]),
        ))
    }

    /// Record by slot.
    pub fn read_record(&self, slot_idx: u16) -> Result<&'a [u8]> {
        let (offset, len) = self
            .slot(slot_idx)
            .ok_or(Error::PageNotFound(self.page_id))?;
        if offset == TOMBSTONE_OFFSET {
            return Err(Error::SlotTombstoned {
                page_id: self.page_id,
                slot_idx,
            });
        }
        let start = offset as usize;
        let buf: &'a [u8] = self.buf;
        buf.get(start..start + len as usize).ok_or_else(|| {
            Error::InvalidFormat(format!(
                "page {}: slot {slot_idx} points past the page",
                self.page_id
            ))
        })
    }

    /// Slot tombstoned?
    pub fn is_tombstone(&self, slot_idx: u16) -> bool {
        self.slot(slot_idx)
            .is_some_and(|(offset, _)| offset == TOMBSTONE_OFFSET)
    }

    /// See [`SlottedPage::reclaimable`].
    pub fn reclaimable(&self) -> usize {
        (0..self.slot_count())
            .filter_map(|slot| self.slot(slot))
            .filter(|&(offset, _)| offset == TOMBSTONE_OFFSET)
            .map(|(_, len)| len as usize)
            .sum()
    }

    /// See [`SlottedPage::available`].
    pub fn available(&self) -> usize {
        let room = self.free_space() + self.reclaimable();
        if self.first_tombstone().is_some() {
            room
        } else {
            room.saturating_sub(SLOT_ENTRY_SIZE)
        }
    }

    /// The first deleted slot, for a new record to reuse.
    fn first_tombstone(&self) -> Option<u16> {
        (0..self.slot_count()).find(|&slot| {
            self.slot(slot)
                .is_some_and(|(offset, _)| offset == TOMBSTONE_OFFSET)
        })
    }

    /// Checks the header and slot directory agree, so neither reads nor later writes leave the
    /// page: the directory ends at the free-space offset, free space ends where the record region
    /// starts, and every live record lies in the record region. Pages of an unknown type are
    /// never used as slotted pages and pass.
    pub fn check_layout(&self) -> Result<()> {
        if self.page_type().is_none() {
            return Ok(());
        }
        let bad = |what: String| {
            Err(Error::InvalidFormat(format!(
                "page {}: {what}",
                self.page_id
            )))
        };
        let dir_end = PAGE_HEADER_SIZE + self.slot_count() as usize * SLOT_ENTRY_SIZE;
        let free = u16::from_le_bytes([self.buf[12], self.buf[13]]) as usize;
        let data_end = u16::from_le_bytes([self.buf[14], self.buf[15]]) as usize;
        if free != dir_end {
            return bad(format!(
                "{} slots end at {dir_end}, free space starts at {free}",
                self.slot_count()
            ));
        }
        if data_end > PAGE_SIZE || free > data_end {
            return bad(format!("free space {free}..{data_end} is outside the page"));
        }
        for slot in 0..self.slot_count() {
            let Some((offset, len)) = self.slot(slot) else {
                return bad(format!("slot {slot} is past the page"));
            };
            if offset == TOMBSTONE_OFFSET {
                continue;
            }
            let (start, end) = (offset as usize, offset as usize + len as usize);
            if start < data_end || end > PAGE_SIZE {
                return bad(format!(
                    "slot {slot} ({start}..{end}) is outside the record region"
                ));
            }
        }
        Ok(())
    }

    /// Free space in this page.
    pub fn free_space(&self) -> usize {
        let fso = u16::from_le_bytes(self.buf[12..14].try_into().unwrap()) as usize;
        let deo = u16::from_le_bytes(self.buf[14..16].try_into().unwrap()) as usize;
        deo.saturating_sub(fso)
    }

    /// Verifies the header CRC32.
    pub fn verify_checksum(&self) -> bool {
        checksum::verify_page_checksum(self.buf)
    }

    /// Header LSN `[16..24]` (tests).
    #[cfg(test)]
    fn lsn(&self) -> u64 {
        u64::from_le_bytes(self.buf[16..24].try_into().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_page(page_id: u64, page_type: PageType) -> Vec<u8> {
        let mut buf = vec![0u8; PAGE_SIZE];
        let mut page = SlottedPage::new(&mut buf, page_id);
        page.init(page_type);
        buf
    }

    #[test]
    fn init_sets_header_fields() {
        let buf = make_page(7, PageType::NodeData);
        let page = SlottedPageRef::new(&buf, 7);
        assert_eq!(page.page_id, 7);
        assert_eq!(page.page_type(), Some(PageType::NodeData));
        assert_eq!(page.slot_count(), 0);
    }

    #[test]
    fn insert_and_read_single_record() {
        let mut buf = make_page(0, PageType::NodeData);
        let mut page = SlottedPage::new(&mut buf, 0);
        let data = b"hello world";
        let slot = page.insert_record(data).unwrap();
        assert_eq!(slot, 0);
        assert_eq!(page.slot_count(), 1);
        assert_eq!(page.read_record(0).unwrap(), data);
    }

    #[test]
    fn insert_and_read_multiple_records() {
        let mut buf = make_page(0, PageType::EdgeData);
        let mut page = SlottedPage::new(&mut buf, 0);

        let records: Vec<Vec<u8>> = (0..10).map(|i| vec![i; 100]).collect();
        for (i, rec) in records.iter().enumerate() {
            let slot = page.insert_record(rec).unwrap();
            assert_eq!(slot, i as u16);
        }

        assert_eq!(page.slot_count(), 10);
        for (i, rec) in records.iter().enumerate() {
            assert_eq!(page.read_record(i as u16).unwrap(), rec.as_slice());
        }
    }

    #[test]
    fn page_full_error() {
        let mut buf = make_page(5, PageType::NodeData);
        let mut page = SlottedPage::new(&mut buf, 5);

        // one record filling almost the whole data area
        let capacity = PAGE_SIZE - PAGE_HEADER_SIZE;
        let big = vec![0xAA; capacity - SLOT_ENTRY_SIZE];
        page.insert_record(&big).unwrap();

        // second record must fail
        let err = page.insert_record(&[1]).unwrap_err();
        assert!(matches!(err, Error::PageFull(5)));
    }

    #[test]
    fn checksum_write_and_verify() {
        let mut buf = make_page(0, PageType::Catalog);
        let mut page = SlottedPage::new(&mut buf, 0);
        page.insert_record(b"test data").unwrap();
        page.write_checksum();
        assert!(page.verify_checksum());
    }

    #[test]
    fn tampered_data_fails_checksum() {
        let mut buf = make_page(0, PageType::NodeData);
        {
            let mut page = SlottedPage::new(&mut buf, 0);
            page.insert_record(b"test data").unwrap();
            page.write_checksum();
        }
        // tamper with the data area
        buf[PAGE_SIZE - 1] ^= 0xFF;
        let page = SlottedPageRef::new(&buf, 0);
        assert!(!page.verify_checksum());
    }

    #[test]
    fn read_invalid_slot_returns_error() {
        let buf = make_page(0, PageType::NodeData);
        let page = SlottedPageRef::new(&buf, 0);
        assert!(page.read_record(0).is_err());
    }

    #[test]
    fn tombstone_then_read_fails() {
        let mut buf = make_page(0, PageType::NodeData);
        {
            let mut page = SlottedPage::new(&mut buf, 0);
            page.insert_record(b"hello").unwrap();
            page.tombstone_slot(0).unwrap();
        }
        let page = SlottedPageRef::new(&buf, 0);
        let err = page.read_record(0).unwrap_err();
        assert!(matches!(
            err,
            Error::SlotTombstoned {
                page_id: 0,
                slot_idx: 0
            }
        ));
    }

    #[test]
    fn tombstone_is_idempotent() {
        let mut buf = make_page(0, PageType::NodeData);
        let mut page = SlottedPage::new(&mut buf, 0);
        page.insert_record(b"hello").unwrap();
        page.tombstone_slot(0).unwrap();
        page.tombstone_slot(0).unwrap(); // second call is fine
        assert!(page.is_tombstone(0));
    }

    #[test]
    fn live_slot_count_tracks_tombstones() {
        let mut buf = make_page(0, PageType::NodeData);
        let mut page = SlottedPage::new(&mut buf, 0);
        page.insert_record(b"aaa").unwrap();
        page.insert_record(b"bbb").unwrap();
        page.insert_record(b"ccc").unwrap();
        assert_eq!(page.live_slot_count(), 3);

        page.tombstone_slot(1).unwrap();
        assert_eq!(page.live_slot_count(), 2);
        assert_eq!(page.slot_count(), 3);
    }

    #[test]
    fn tombstone_preserves_other_slots() {
        let mut buf = make_page(0, PageType::NodeData);
        {
            let mut page = SlottedPage::new(&mut buf, 0);
            page.insert_record(b"first").unwrap();
            page.insert_record(b"second").unwrap();
            page.insert_record(b"third").unwrap();
            page.tombstone_slot(1).unwrap();
        }
        let page = SlottedPageRef::new(&buf, 0);
        assert_eq!(page.read_record(0).unwrap(), b"first");
        assert!(page.read_record(1).is_err());
        assert_eq!(page.read_record(2).unwrap(), b"third");
    }

    #[test]
    fn deleted_records_make_room_and_their_slots_are_reused() {
        let mut buf = vec![0u8; PAGE_SIZE];
        let mut page = SlottedPage::new(&mut buf, 3);
        page.init(PageType::NodeData);
        let big = (PAGE_SIZE - PAGE_HEADER_SIZE) / 3 - SLOT_ENTRY_SIZE;
        let (a, b, c) = (vec![1u8; big], vec![2u8; big], vec![3u8; big]);
        assert_eq!(page.insert_record(&a).unwrap(), 0);
        assert_eq!(page.insert_record(&b).unwrap(), 1);
        assert_eq!(page.insert_record(&c).unwrap(), 2);
        assert!(page.insert_record(&[9u8; 64]).is_err(), "full");
        page.tombstone_slot(1).unwrap();
        assert_eq!(page.reclaimable(), big);
        assert!(page.available() >= big);
        // the middle record's bytes are reused for a new one in its slot; the others stay put
        let d = vec![4u8; big - 10];
        assert_eq!(page.insert_record(&d).unwrap(), 1);
        assert_eq!(page.read_record(0).unwrap(), &a[..]);
        assert_eq!(page.read_record(1).unwrap(), &d[..]);
        assert_eq!(page.read_record(2).unwrap(), &c[..]);
        assert_eq!(page.reclaimable(), 0);
        assert_eq!(page.slot_count(), 3);
        SlottedPageRef::new(page.buf, 3).check_layout().unwrap();
    }

    #[test]
    fn page_lsn_read_write() {
        let mut buf = make_page(0, PageType::NodeData);
        {
            let mut page = SlottedPage::new(&mut buf, 0);
            assert_eq!(page.lsn(), 0);
            page.set_lsn(42);
            assert_eq!(page.lsn(), 42);
        }
        let page = SlottedPageRef::new(&buf, 0);
        assert_eq!(page.lsn(), 42);
    }

    #[test]
    fn slotted_page_ref_free_space() {
        let mut buf = make_page(0, PageType::NodeData);
        let initial_free;
        {
            let page = SlottedPageRef::new(&buf, 0);
            initial_free = page.free_space();
            assert!(initial_free > 0);
        }
        {
            let mut page = SlottedPage::new(&mut buf, 0);
            page.insert_record(&[0u8; 100]).unwrap();
        }
        let page = SlottedPageRef::new(&buf, 0);
        assert!(page.free_space() < initial_free);
    }
}
