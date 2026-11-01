use std::collections::HashMap;
use std::sync::Arc;

/// Bidirectional string <-> `u32` interner for labels and edge types: compact IDs instead of heap
/// strings.
#[derive(Debug, Clone, Default)]
pub struct StringTable {
    to_id: HashMap<String, u32>,
    to_str: Vec<String>,
}

impl StringTable {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Interns `s`; returns its ID (existing one if already interned).
    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.to_id.get(s) {
            return id;
        }
        let id = self.to_str.len() as u32;
        self.to_str.push(s.to_owned());
        self.to_id.insert(s.to_owned(), id);
        id
    }

    /// ID -> string.
    ///
    /// # Panics
    ///
    /// On an invalid ID.
    pub fn resolve(&self, id: u32) -> &str {
        &self.to_str[id as usize]
    }

    /// ID -> string; `None` if invalid.
    pub fn try_resolve(&self, id: u32) -> Option<&str> {
        self.to_str.get(id as usize).map(|s| s.as_str())
    }

    /// ID of an already-interned string, without interning.
    pub fn get(&self, s: &str) -> Option<u32> {
        self.to_id.get(s).copied()
    }

    /// Interned string count.
    pub fn len(&self) -> usize {
        self.to_str.len()
    }

    /// Nothing interned?
    pub fn is_empty(&self) -> bool {
        self.to_str.is_empty()
    }

    /// Rebuilds a table from previously interned strings, in original order (index == ID).
    pub fn from_strings(strings: Vec<String>) -> Self {
        let to_id = strings
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), i as u32))
            .collect();
        Self {
            to_id,
            to_str: strings,
        }
    }

    /// All interned strings, by ID.
    pub fn to_strings(&self) -> &[String] {
        &self.to_str
    }
}

/// Copy-on-write [`StringTable`]: readers keep an `Arc` snapshot that never changes under them;
/// interning a NEW string copies the table only while such a snapshot is out.
#[derive(Debug, Clone, Default)]
pub struct SharedStrings(Arc<StringTable>);

impl SharedStrings {
    /// Interns `s`; copies the table first if a snapshot shares it.
    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(id) = self.0.get(s) {
            return id;
        }
        Arc::make_mut(&mut self.0).intern(s)
    }

    /// Snapshot of the current table.
    pub fn snapshot(&self) -> Arc<StringTable> {
        Arc::clone(&self.0)
    }
}

impl From<Arc<StringTable>> for SharedStrings {
    fn from(table: Arc<StringTable>) -> Self {
        Self(table)
    }
}

impl From<StringTable> for SharedStrings {
    fn from(table: StringTable) -> Self {
        Self(Arc::new(table))
    }
}

impl std::ops::Deref for SharedStrings {
    type Target = StringTable;

    fn deref(&self) -> &StringTable {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_strings_copy_only_while_a_snapshot_is_out() {
        let mut shared = SharedStrings::default();
        let a = shared.intern("a");
        let snap = shared.snapshot();
        // known string: no copy, same allocation
        assert_eq!(shared.intern("a"), a);
        assert!(Arc::ptr_eq(&snap, &shared.snapshot()));
        // new string: the snapshot keeps the old table
        let b = shared.intern("b");
        assert_eq!(snap.get("b"), None);
        assert_eq!(shared.get("b"), Some(b));
        assert_eq!(snap.resolve(a), "a");
    }

    #[test]
    fn intern_returns_sequential_ids() {
        let mut table = StringTable::new();
        assert_eq!(table.intern("Person"), 0);
        assert_eq!(table.intern("Movie"), 1);
        assert_eq!(table.intern("Actor"), 2);
    }

    #[test]
    fn intern_is_idempotent() {
        let mut table = StringTable::new();
        let id1 = table.intern("Person");
        let id2 = table.intern("Person");
        assert_eq!(id1, id2);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn resolve_round_trips() {
        let mut table = StringTable::new();
        let id = table.intern("KNOWS");
        assert_eq!(table.resolve(id), "KNOWS");
    }

    #[test]
    fn try_resolve_returns_none_for_invalid_id() {
        let table = StringTable::new();
        assert!(table.try_resolve(42).is_none());
    }

    #[test]
    fn get_returns_none_for_unknown_string() {
        let table = StringTable::new();
        assert!(table.get("Unknown").is_none());
    }

    #[test]
    fn get_returns_id_for_known_string() {
        let mut table = StringTable::new();
        table.intern("Person");
        assert_eq!(table.get("Person"), Some(0));
    }

    #[test]
    fn empty_table() {
        let table = StringTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn from_strings_roundtrip() {
        let mut table = StringTable::new();
        table.intern("Person");
        table.intern("Movie");
        table.intern("KNOWS");

        let strings = table.to_strings().to_vec();
        let reconstructed = StringTable::from_strings(strings);

        assert_eq!(reconstructed.len(), 3);
        assert_eq!(reconstructed.resolve(0), "Person");
        assert_eq!(reconstructed.resolve(1), "Movie");
        assert_eq!(reconstructed.resolve(2), "KNOWS");
        assert_eq!(reconstructed.get("Person"), Some(0));
        assert_eq!(reconstructed.get("Movie"), Some(1));
        assert_eq!(reconstructed.get("KNOWS"), Some(2));
    }

    #[test]
    fn from_strings_empty() {
        let reconstructed = StringTable::from_strings(Vec::new());
        assert!(reconstructed.is_empty());
    }
}
