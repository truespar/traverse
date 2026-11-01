//! Substring search: trigram inverted index behind `CREATE TEXT INDEX` and
//! transparent acceleration of `CONTAINS` / `ENDS WITH` / `STARTS WITH` / `=`.
//!
//! Counterpart of the BM25 index ([`super::fulltext`]): full-text ranks
//! *tokenised words*, a text index filters *raw substrings*. Neo4j uses a Lucene
//! trigram index, PostgreSQL's `pg_trgm` the same idea; this is native and
//! dependency-free:
//!
//! * **Boundary sentinels**: values get two start sentinels and one end sentinel
//!   before trigram extraction (`pg_trgm` convention). The *anchored* trigrams
//!   let prefix, suffix and equality queries filter on the same postings as
//!   `CONTAINS`.
//! * **Candidates**: a string containing `p` contains every trigram of `p`, so
//!   intersecting `p`'s postings gives a **superset** of the matches. The caller
//!   keeps the predicate as an exact refinement; the index only prunes. Patterns
//!   too short for a trigram return all indexed docs (still label+property
//!   scoped, far smaller than a scan).
//! * **Case-sensitive** like Cypher's string predicates: values are indexed as
//!   is (unlike `pg_trgm`).
//!
//! Only the *definition* is persisted; postings rebuild from node string
//! properties on load and update as indexed nodes are written.

use rustc_hash::FxHashMap;

use super::types::{LabelId, NodeId, PropertyKeyId};

/// Start sentinel, prepended twice (`pg_trgm` convention) so even 1-char prefixes get an anchored
/// trigram.
const PAD_START: char = '\u{2}';
/// End sentinel, appended once.
const PAD_END: char = '\u{3}';

/// String predicate a text-index query accelerates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextQueryKind {
    /// `n.p = pattern`
    Equals,
    /// `n.p STARTS WITH pattern`
    StartsWith,
    /// `n.p ENDS WITH pattern`
    EndsWith,
    /// `n.p CONTAINS pattern`
    Contains,
}

/// Pushes the character trigrams of `text` into `out`; `TextIndex` dedups.
fn trigrams_of(text: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < 3 {
        return;
    }
    for w in chars.windows(3) {
        out.push(w.iter().collect());
    }
}

/// Padded form of an indexed value: `PAD_START PAD_START value PAD_END`.
fn padded(value: &str) -> String {
    let mut s = String::with_capacity(value.len() + 3);
    s.push(PAD_START);
    s.push(PAD_START);
    s.push_str(value);
    s.push(PAD_END);
    s
}

/// Named trigram index over one `(label, property)` string property. Maintained incrementally.
#[derive(Clone, Debug)]
pub struct TextIndex {
    name: String,
    label_id: LabelId,
    property_key: PropertyKeyId,
    /// Trigram -> dense id.
    trigram_ids: FxHashMap<String, u32>,
    /// Posting lists (node ids) by trigram id.
    postings: Vec<Vec<NodeId>>,
    /// Per-document distinct trigram ids (removal on update/delete); the key set doubles as the
    /// short-pattern fallback.
    doc_trigrams: FxHashMap<NodeId, Vec<u32>>,
}

impl TextIndex {
    /// Empty text index on `(label, property)`.
    pub fn new(name: &str, label_id: LabelId, property_key: PropertyKeyId) -> Self {
        Self {
            name: name.to_string(),
            label_id,
            property_key,
            trigram_ids: FxHashMap::default(),
            postings: Vec::new(),
            doc_trigrams: FxHashMap::default(),
        }
    }

    /// The index name.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The indexed label id.
    pub fn label_id(&self) -> LabelId {
        self.label_id
    }
    /// The indexed property-key id.
    pub fn property_key(&self) -> PropertyKeyId {
        self.property_key
    }
    /// Number of indexed documents.
    pub fn doc_count(&self) -> usize {
        self.doc_trigrams.len()
    }

    fn intern(&mut self, trigram: &str) -> u32 {
        if let Some(&id) = self.trigram_ids.get(trigram) {
            return id;
        }
        let id = self.postings.len() as u32;
        self.trigram_ids.insert(trigram.to_string(), id);
        self.postings.push(Vec::new());
        id
    }

    /// (Re-)indexes a node's string; re-indexing removes old postings first.
    pub fn index_value(&mut self, node: NodeId, value: &str) {
        if self.doc_trigrams.contains_key(&node) {
            self.remove(node);
        }
        let mut grams = Vec::new();
        trigrams_of(&padded(value), &mut grams);
        grams.sort_unstable();
        grams.dedup();
        let mut ids = Vec::with_capacity(grams.len());
        for g in &grams {
            let id = self.intern(g);
            self.postings[id as usize].push(node);
            ids.push(id);
        }
        // values shorter than a trigram still count as documents (padding gives any value, even "",
        // >=1 trigram)
        self.doc_trigrams.insert(node, ids);
    }

    /// Removes a node (delete, or first half of a re-index).
    pub fn remove(&mut self, node: NodeId) {
        if let Some(ids) = self.doc_trigrams.remove(&node) {
            for id in ids {
                let list = &mut self.postings[id as usize];
                if let Some(pos) = list.iter().position(|n| *n == node) {
                    list.swap_remove(pos);
                }
            }
        }
    }

    /// Candidates for a string predicate: a **superset** of the matches (caller refines exactly).
    /// Patterns too short to filter return every indexed doc.
    pub fn candidates(&self, kind: TextQueryKind, pattern: &str) -> Vec<NodeId> {
        // anchored search string per predicate
        let search = match kind {
            TextQueryKind::Contains => pattern.to_string(),
            TextQueryKind::StartsWith => {
                let mut s = String::with_capacity(pattern.len() + 2);
                s.push(PAD_START);
                s.push(PAD_START);
                s.push_str(pattern);
                s
            }
            TextQueryKind::EndsWith => {
                let mut s = String::with_capacity(pattern.len() + 1);
                s.push_str(pattern);
                s.push(PAD_END);
                s
            }
            TextQueryKind::Equals => padded(pattern),
        };
        let mut grams = Vec::new();
        trigrams_of(&search, &mut grams);
        grams.sort_unstable();
        grams.dedup();

        if grams.is_empty() {
            // too short to filter: every indexed doc is a candidate
            return self.doc_trigrams.keys().copied().collect();
        }

        // intersect postings rarest first; a missing trigram means no match
        let mut lists: Vec<&Vec<NodeId>> = Vec::with_capacity(grams.len());
        for g in &grams {
            match self.trigram_ids.get(g) {
                Some(&id) => lists.push(&self.postings[id as usize]),
                None => return Vec::new(),
            }
        }
        lists.sort_by_key(|l| l.len());
        let mut result: Vec<NodeId> = lists[0].clone();
        for list in &lists[1..] {
            if result.is_empty() {
                break;
            }
            let set: rustc_hash::FxHashSet<NodeId> = list.iter().copied().collect();
            result.retain(|n| set.contains(n));
        }
        result
    }
}

// -- Registry -------------------------------------------------------------

/// Text indexes of a graph, by name.
#[derive(Clone, Debug, Default)]
pub struct TextIndexRegistry {
    by_name: FxHashMap<String, TextIndex>,
    generation: u64,
}

impl TextIndexRegistry {
    /// Registers a new empty index. Errors if the name is taken.
    pub fn create(
        &mut self,
        name: &str,
        label_id: LabelId,
        property_key: PropertyKeyId,
    ) -> Result<(), String> {
        if self.by_name.contains_key(name) {
            return Err(format!("text index '{name}' already exists"));
        }
        self.by_name.insert(
            name.to_string(),
            TextIndex::new(name, label_id, property_key),
        );
        self.generation += 1;
        Ok(())
    }

    /// Drop an index by name.
    pub fn drop(&mut self, name: &str) -> Result<(), String> {
        if self.by_name.remove(name).is_none() {
            return Err(format!("no such text index: {name}"));
        }
        self.generation += 1;
        Ok(())
    }

    /// Look up an index by name.
    pub fn get(&self, name: &str) -> Option<&TextIndex> {
        self.by_name.get(name)
    }
    /// Mutable lookup (maintenance).
    pub fn get_mut(&mut self, name: &str) -> Option<&mut TextIndex> {
        self.by_name.get_mut(name)
    }
    /// Iterate all indexes.
    pub fn iter(&self) -> impl Iterator<Item = &TextIndex> {
        self.by_name.values()
    }
    /// No text indexes (write fast path).
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
    /// Indexes covering `label` (write maintenance).
    pub fn indexes_for_label(&self, label: LabelId) -> Vec<String> {
        self.by_name
            .values()
            .filter(|idx| idx.label_id == label)
            .map(|idx| idx.name.clone())
            .collect()
    }
    /// Index covering `(label, property)` (first by name, deterministic); the optimizer routes
    /// string predicates through it.
    pub fn index_for(&self, label: LabelId, property: PropertyKeyId) -> Option<&TextIndex> {
        self.by_name
            .values()
            .filter(|idx| idx.label_id == label && idx.property_key == property)
            .min_by(|a, b| a.name.cmp(&b.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nid(n: u64) -> NodeId {
        NodeId(n)
    }

    fn corpus() -> (TextIndex, Vec<(NodeId, &'static str)>) {
        let values: Vec<(NodeId, &'static str)> = vec![
            (nid(1), "alice@example.com"),
            (nid(2), "bob@example.org"),
            (nid(3), "carol@test.com"),
            (nid(4), "Dave"),
            (nid(5), ""),
            (nid(6), "ab"),
            (nid(7), "тест-строка"), // non-ASCII
        ];
        let mut idx = TextIndex::new("t", LabelId(1), PropertyKeyId(1));
        for (n, v) in &values {
            idx.index_value(*n, v);
        }
        (idx, values)
    }

    /// Candidates must be a superset of the matches and equal them after exact refinement.
    fn check(kind: TextQueryKind, pattern: &str) {
        let (idx, values) = corpus();
        let truth: Vec<NodeId> = values
            .iter()
            .filter(|(_, v)| match kind {
                TextQueryKind::Equals => *v == pattern,
                TextQueryKind::StartsWith => v.starts_with(pattern),
                TextQueryKind::EndsWith => v.ends_with(pattern),
                TextQueryKind::Contains => v.contains(pattern),
            })
            .map(|(n, _)| *n)
            .collect();
        let cand = idx.candidates(kind, pattern);
        for t in &truth {
            assert!(
                cand.contains(t),
                "{kind:?} {pattern:?}: candidate set must contain {t:?}"
            );
        }
        // refinement must recover exactly the truth set
        let mut refined: Vec<NodeId> = cand
            .into_iter()
            .filter(|n| {
                let v = values.iter().find(|(vn, _)| vn == n).unwrap().1;
                match kind {
                    TextQueryKind::Equals => v == pattern,
                    TextQueryKind::StartsWith => v.starts_with(pattern),
                    TextQueryKind::EndsWith => v.ends_with(pattern),
                    TextQueryKind::Contains => v.contains(pattern),
                }
            })
            .collect();
        refined.sort_by_key(|n| n.0);
        let mut expect = truth.clone();
        expect.sort_by_key(|n| n.0);
        assert_eq!(refined, expect, "{kind:?} {pattern:?}");
    }

    #[test]
    fn contains_candidates_and_refinement() {
        check(TextQueryKind::Contains, "example");
        check(TextQueryKind::Contains, "@ex");
        check(TextQueryKind::Contains, ".com");
        check(TextQueryKind::Contains, "zzz");
        check(TextQueryKind::Contains, "строк");
    }

    #[test]
    fn ends_with_candidates_and_refinement() {
        check(TextQueryKind::EndsWith, ".com");
        check(TextQueryKind::EndsWith, ".org");
        check(TextQueryKind::EndsWith, "e");
        check(TextQueryKind::EndsWith, "ка");
    }

    #[test]
    fn starts_with_candidates_and_refinement() {
        check(TextQueryKind::StartsWith, "alice");
        check(TextQueryKind::StartsWith, "D");
        check(TextQueryKind::StartsWith, "т");
    }

    #[test]
    fn equals_candidates_and_refinement() {
        check(TextQueryKind::Equals, "Dave");
        check(TextQueryKind::Equals, "ab");
        check(TextQueryKind::Equals, "");
        check(TextQueryKind::Equals, "nope");
    }

    #[test]
    fn short_contains_falls_back_to_all_docs() {
        let (idx, values) = corpus();
        // 1-2 char CONTAINS patterns form no trigram -> all docs
        let cand = idx.candidates(TextQueryKind::Contains, "ab");
        assert_eq!(cand.len(), values.len());
        check(TextQueryKind::Contains, "ab");
        check(TextQueryKind::Contains, "a");
    }

    #[test]
    fn case_sensitive() {
        let (idx, _) = corpus();
        let cand = idx.candidates(TextQueryKind::Contains, "DAVE");
        // nothing contains uppercase DAVE; the trigram filter alone rules it out
        assert!(cand.is_empty());
    }

    #[test]
    fn update_and_delete_maintain_postings() {
        let (mut idx, _) = corpus();
        // re-index node 4 with a new value
        idx.index_value(nid(4), "changed@example.com");
        assert!(
            !idx.candidates(TextQueryKind::Equals, "Dave")
                .contains(&nid(4))
        );
        assert!(
            idx.candidates(TextQueryKind::Contains, "changed")
                .contains(&nid(4))
        );
        // delete node 1
        idx.remove(nid(1));
        assert!(
            !idx.candidates(TextQueryKind::Contains, "alice")
                .contains(&nid(1))
        );
        assert_eq!(idx.doc_count(), 6);
    }

    #[test]
    fn registry_create_drop_lookup() {
        let mut reg = TextIndexRegistry::default();
        reg.create("t", LabelId(1), PropertyKeyId(2)).unwrap();
        assert!(reg.create("t", LabelId(1), PropertyKeyId(2)).is_err());
        assert!(reg.index_for(LabelId(1), PropertyKeyId(2)).is_some());
        assert!(reg.index_for(LabelId(2), PropertyKeyId(2)).is_none());
        assert_eq!(reg.indexes_for_label(LabelId(1)), vec!["t".to_string()]);
        reg.drop("t").unwrap();
        assert!(reg.is_empty());
    }
}
