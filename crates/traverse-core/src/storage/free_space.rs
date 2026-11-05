//! Which data pages have room, so an in-place save finds a page for a record with a lookup instead
//! of a scan of the file. Built from the pages on the first save after a file is opened or
//! rewritten, then kept current as records go in and out. Room counts the bytes of deleted records
//! too: a page packs its live records together when a new one needs them.

use std::collections::BTreeSet;

use rustc_hash::FxHashMap;

use crate::error::Result;
use crate::storage::location::RecordLocation;
use crate::storage::page::{PageType, SlottedPage, SlottedPageRef};
use crate::storage::page_manager::PageManager;

/// Room below this isn't worth tracking: no record is that small.
const MIN_ROOM: usize = 16;

/// Pages with room, per record type.
#[derive(Debug, Default)]
pub(crate) struct FreeSpaceMap {
    nodes: Pages,
    edges: Pages,
}

#[derive(Debug, Default)]
struct Pages {
    /// `(room, page)`, least room first.
    by_room: BTreeSet<(usize, u64)>,
    room: FxHashMap<u64, usize>,
}

impl Pages {
    fn set(&mut self, page: u64, room: usize) {
        if let Some(old) = self.room.remove(&page) {
            self.by_room.remove(&(old, page));
        }
        if room >= MIN_ROOM {
            self.by_room.insert((room, page));
            self.room.insert(page, room);
        }
    }

    /// The page with the least room that still takes `len` bytes: big gaps stay for big records.
    fn fitting(&self, len: usize) -> Option<u64> {
        self.by_room.range((len, 0)..).next().map(|&(_, page)| page)
    }
}

impl FreeSpaceMap {
    /// The room of every data page of `pm`.
    pub(crate) fn build(pm: &PageManager) -> Result<Self> {
        let mut map = Self::default();
        for page_id in 0..pm.total_pages() {
            let buf = pm.get_page(page_id)?;
            let page = SlottedPageRef::new(buf.as_ref(), page_id);
            if let Some(pages) = page.page_type().and_then(|t| map.pages(t)) {
                pages.set(page_id, page.available());
            }
        }
        Ok(map)
    }

    /// Puts `record` in a page of `page_type` with room for it, or a new one.
    pub(crate) fn insert(
        &mut self,
        pm: &mut PageManager,
        record: &[u8],
        page_type: PageType,
    ) -> Result<RecordLocation> {
        if record.first() == Some(&crate::storage::compression::RECORD_FLAG_PROPLESS_EDGE) {
            pm.header_mut().note_compact_edges();
        }
        let found = self.pages(page_type).and_then(|p| p.fitting(record.len()));
        let page_id = match found {
            Some(page) => page,
            None => pm.allocate(page_type),
        };
        let buf = pm.get_page_mut(page_id)?;
        let mut page = SlottedPage::new(buf.as_mut(), page_id);
        let slot_idx = page.insert_record(record)?;
        let room = page.available();
        if let Some(pages) = self.pages(page_type) {
            pages.set(page_id, room);
        }
        Ok(RecordLocation { page_id, slot_idx })
    }

    /// Re-reads the room of `page_id` after a record on it was deleted.
    pub(crate) fn refresh(&mut self, pm: &PageManager, page_id: u64) -> Result<()> {
        let buf = pm.get_page(page_id)?;
        let page = SlottedPageRef::new(buf.as_ref(), page_id);
        if let Some(pages) = page.page_type().and_then(|t| self.pages(t)) {
            pages.set(page_id, page.available());
        }
        Ok(())
    }

    /// Room of every tracked page.
    pub(crate) fn total_room(&self) -> usize {
        self.nodes
            .room
            .values()
            .chain(self.edges.room.values())
            .sum()
    }

    /// `page_id` was freed: no room to offer until it's a data page again.
    pub(crate) fn forget(&mut self, page_id: u64) {
        self.nodes.set(page_id, 0);
        self.edges.set(page_id, 0);
    }

    fn pages(&mut self, page_type: PageType) -> Option<&mut Pages> {
        match page_type {
            PageType::NodeData => Some(&mut self.nodes),
            PageType::EdgeData => Some(&mut self.edges),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_go_where_they_fit_best_and_freed_room_is_found_again() {
        let mut pm = PageManager::create_in_memory();
        let mut map = FreeSpaceMap::build(&pm).unwrap();
        let big = vec![1u8; 10_000];
        let a = map.insert(&mut pm, &big, PageType::NodeData).unwrap();
        // 6 KB left on page a: a 5 KB record goes there, a second 5 KB one to a new page
        let b = map
            .insert(&mut pm, &[2u8; 5_000], PageType::NodeData)
            .unwrap();
        let c = map
            .insert(&mut pm, &[3u8; 5_000], PageType::NodeData)
            .unwrap();
        assert_eq!(b.page_id, a.page_id);
        assert_ne!(c.page_id, a.page_id);
        // relationships never share a page with nodes
        let e = map
            .insert(&mut pm, &[4u8; 100], PageType::EdgeData)
            .unwrap();
        assert!(e.page_id != a.page_id && e.page_id != c.page_id);

        // deleting the big record makes its bytes available again, in its old slot
        let buf = pm.get_page_mut(a.page_id).unwrap();
        SlottedPage::new(buf.as_mut(), a.page_id)
            .tombstone_slot(a.slot_idx)
            .unwrap();
        map.refresh(&pm, a.page_id).unwrap();
        let again = map
            .insert(&mut pm, &[5u8; 9_000], PageType::NodeData)
            .unwrap();
        assert_eq!(again, a);

        // a rebuilt map agrees
        let rebuilt = FreeSpaceMap::build(&pm).unwrap();
        assert_eq!(rebuilt.nodes.room, map.nodes.room);
        assert_eq!(rebuilt.edges.room, map.edges.room);
    }
}
