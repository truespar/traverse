//! Full-text search: BM25-ranked inverted index behind `CREATE FULLTEXT INDEX` /
//! `db.index.fulltext.queryNodes`.
//!
//! Lexical counterpart of the HNSW index in [`super::vector`]. Neo4j uses Lucene;
//! this implements the same core ideas natively, no dependencies:
//!
//! * **Analysis**: Unicode word split, lowercase, optional stop words and
//!   stemming, picked by a named [`Analyzer`] (`standard` / `english` /
//!   `keyword` / `whitespace` / `simple`) like Lucene's set.
//! * **Inverted index**: term dictionary with per-term postings of
//!   `(node, term frequency)`; document lengths and their total feed BM25 length
//!   normalisation.
//! * **Ranking**: Okapi **BM25** (`k1 = 1.2`, `b = 0.75`) with Lucene's
//!   non-negative IDF.
//!
//! Like the vector index only the *definition* is persisted; postings rebuild
//! from node properties on load and update as indexed nodes are written.

use rustc_hash::{FxHashMap, FxHashSet};

use super::types::{LabelId, NodeId, PropertyKeyId};

/// BM25 term-frequency saturation (Lucene/Elasticsearch default).
const BM25_K1: f64 = 1.2;
/// BM25 length normalisation (Lucene/Elasticsearch default).
const BM25_B: f64 = 0.75;

// -- Analyzer -------------------------------------------------------------

/// Text analysis for indexed documents and query strings. Names match Neo4j/Lucene's built-in
/// analyzers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Analyzer {
    /// Unicode word tokeniser + lowercase. No stop-words, no stemming.
    Standard,
    /// `Standard` + English stop-word removal + Porter stemming.
    English,
    /// Whole value as one token, case kept (exact match).
    Keyword,
    /// Split on whitespace only; no lowercasing or filtering.
    Whitespace,
    /// Unicode word tokeniser + lowercase (alias of `Standard`).
    Simple,
}

impl Analyzer {
    /// Analyzer by its Neo4j/Lucene name.
    pub fn from_name(name: &str) -> Option<Analyzer> {
        match name.to_ascii_lowercase().as_str() {
            "standard" | "standard-no-stop-words" => Some(Analyzer::Standard),
            "english" => Some(Analyzer::English),
            "keyword" => Some(Analyzer::Keyword),
            "whitespace" => Some(Analyzer::Whitespace),
            "simple" => Some(Analyzer::Simple),
            _ => None,
        }
    }

    /// Canonical name (`SHOW INDEXES`, persistence).
    pub fn name(&self) -> &'static str {
        match self {
            Analyzer::Standard => "standard",
            Analyzer::English => "english",
            Analyzer::Keyword => "keyword",
            Analyzer::Whitespace => "whitespace",
            Analyzer::Simple => "simple",
        }
    }

    /// `text` -> ordered index terms.
    pub fn analyze(&self, text: &str) -> Vec<String> {
        self.analyze_with_positions(text)
            .into_iter()
            .map(|(t, _)| t)
            .collect()
    }

    /// `text` -> `(term, position)` pairs. Positions index the raw token stream: the
    /// `english` analyzer drops stop words but they still take a position (Lucene's
    /// position increment), so phrase queries respect the original gaps.
    pub fn analyze_with_positions(&self, text: &str) -> Vec<(String, u32)> {
        match self {
            Analyzer::Keyword => {
                if text.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![(text.to_string(), 0)]
                }
            }
            Analyzer::Whitespace => text
                .split_whitespace()
                .enumerate()
                .map(|(i, t)| (t.to_string(), i as u32))
                .collect(),
            Analyzer::Simple | Analyzer::Standard => tokenize_lower(text)
                .into_iter()
                .enumerate()
                .map(|(i, t)| (t, i as u32))
                .collect(),
            Analyzer::English => tokenize_lower(text)
                .into_iter()
                .enumerate()
                .filter(|(_, t)| !is_english_stopword(t))
                .map(|(i, t)| (porter_stem(&t), i as u32))
                .collect(),
        }
    }

    /// Normalises one prefix/fuzzy query term. As in Lucene, multi-term queries skip
    /// full analysis: lowercase only for word-tokenising analyzers (no stemming; the
    /// pattern matches the indexed vocabulary), verbatim for keyword/whitespace.
    fn normalize_pattern_term(&self, body: &str) -> Option<String> {
        match self {
            Analyzer::Keyword | Analyzer::Whitespace => {
                if body.is_empty() {
                    None
                } else {
                    Some(body.to_string())
                }
            }
            Analyzer::Standard | Analyzer::Simple | Analyzer::English => {
                tokenize_lower(body).into_iter().next()
            }
        }
    }
}

/// Unicode word tokeniser: split on non-alphanumerics, lowercase.
fn tokenize_lower(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                cur.push(lc);
            }
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Stop words of [`Analyzer::English`]; public for `db.index.fulltext.listAvailableAnalyzers`.
pub fn english_stopwords() -> &'static [&'static str] {
    ENGLISH_STOPWORDS
}

const ENGLISH_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it",
    "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there", "these",
    "they", "this", "to", "was", "will", "with", "we", "you", "your", "i", "he", "she", "him",
    "her", "his", "hers", "our", "us", "from", "has", "have", "had", "were", "been", "being", "do",
    "does", "did", "so", "than", "too", "very", "can", "just", "about",
];

/// Compact English stop-word set (the closed-class words Lucene's English analyzer strips).
fn is_english_stopword(t: &str) -> bool {
    ENGLISH_STOPWORDS.binary_search(&t).is_ok()
}

// -- Porter stemmer -------------------------------------------------------
//
// Porter's 1980 suffix-stripping algorithm, classic formulation. Works on
// ASCII-lowercased words; anything with non-ASCII bytes is returned unchanged
// (English only).

fn is_consonant(w: &[u8], i: usize) -> bool {
    match w[i] {
        b'a' | b'e' | b'i' | b'o' | b'u' => false,
        b'y' => i == 0 || !is_consonant(w, i - 1),
        _ => true,
    }
}

/// Porter's measure m: number of consonant-run/vowel-run (VC) pairs.
fn measure(w: &[u8]) -> usize {
    let len = w.len();
    let mut i = 0;
    while i < len && is_consonant(w, i) {
        i += 1;
    }
    let mut n = 0;
    while i < len {
        while i < len && !is_consonant(w, i) {
            i += 1;
        }
        if i >= len {
            break;
        }
        while i < len && is_consonant(w, i) {
            i += 1;
        }
        n += 1;
    }
    n
}

fn contains_vowel(w: &[u8]) -> bool {
    (0..w.len()).any(|i| !is_consonant(w, i))
}

fn ends_double_consonant(w: &[u8]) -> bool {
    let n = w.len();
    n >= 2 && w[n - 1] == w[n - 2] && is_consonant(w, n - 1)
}

/// Ends consonant-vowel-consonant with the last consonant not w, x or y (Porter's *o).
fn ends_cvc(w: &[u8]) -> bool {
    let n = w.len();
    n >= 3
        && is_consonant(w, n - 3)
        && !is_consonant(w, n - 2)
        && is_consonant(w, n - 1)
        && !matches!(w[n - 1], b'w' | b'x' | b'y')
}

fn ends_with(w: &[u8], suf: &[u8]) -> bool {
    w.len() >= suf.len() && &w[w.len() - suf.len()..] == suf
}

/// Replaces the `suf`-length suffix with `repl` when `cond(stem)` holds.
fn replace_if(w: &mut Vec<u8>, suf: &[u8], repl: &[u8], cond: impl Fn(&[u8]) -> bool) -> bool {
    if ends_with(w, suf) {
        let stem_len = w.len() - suf.len();
        if cond(&w[..stem_len]) {
            w.truncate(stem_len);
            w.extend_from_slice(repl);
            return true;
        }
    }
    false
}

/// Stem an English word with the Porter algorithm.
pub fn porter_stem(word: &str) -> String {
    if !word.is_ascii() || word.len() <= 2 {
        return word.to_string();
    }
    let mut w: Vec<u8> = word.bytes().collect();

    // Step 1a
    if !replace_if(&mut w, b"sses", b"ss", |_| true)
        && !replace_if(&mut w, b"ies", b"i", |_| true)
        && !ends_with(&w, b"ss")
    {
        replace_if(&mut w, b"s", b"", |_| true);
    }

    // Step 1b
    let mut step1b_flag = false;
    if replace_if(&mut w, b"eed", b"ee", |s| measure(s) > 0) {
        // handled
    } else if (ends_with(&w, b"ed") && contains_vowel(&w[..w.len() - 2]))
        || (ends_with(&w, b"ing") && contains_vowel(&w[..w.len() - 3]))
    {
        if ends_with(&w, b"ed") {
            w.truncate(w.len() - 2);
        } else {
            w.truncate(w.len() - 3);
        }
        step1b_flag = true;
    }
    if step1b_flag {
        if ends_with(&w, b"at") || ends_with(&w, b"bl") || ends_with(&w, b"iz") {
            w.push(b'e');
        } else if ends_double_consonant(&w) && !matches!(w[w.len() - 1], b'l' | b's' | b'z') {
            w.pop();
        } else if measure(&w) == 1 && ends_cvc(&w) {
            w.push(b'e');
        }
    }

    // Step 1c
    if ends_with(&w, b"y") && contains_vowel(&w[..w.len() - 1]) {
        let n = w.len();
        w[n - 1] = b'i';
    }

    // Step 2
    const STEP2: &[(&[u8], &[u8])] = &[
        (b"ational", b"ate"),
        (b"tional", b"tion"),
        (b"enci", b"ence"),
        (b"anci", b"ance"),
        (b"izer", b"ize"),
        (b"bli", b"ble"),
        (b"alli", b"al"),
        (b"entli", b"ent"),
        (b"eli", b"e"),
        (b"ousli", b"ous"),
        (b"ization", b"ize"),
        (b"ation", b"ate"),
        (b"ator", b"ate"),
        (b"alism", b"al"),
        (b"iveness", b"ive"),
        (b"fulness", b"ful"),
        (b"ousness", b"ous"),
        (b"aliti", b"al"),
        (b"iviti", b"ive"),
        (b"biliti", b"ble"),
        (b"logi", b"log"),
    ];
    for (suf, repl) in STEP2 {
        if replace_if(&mut w, suf, repl, |s| measure(s) > 0) {
            break;
        }
    }

    // Step 3
    const STEP3: &[(&[u8], &[u8])] = &[
        (b"icate", b"ic"),
        (b"ative", b""),
        (b"alize", b"al"),
        (b"iciti", b"ic"),
        (b"ical", b"ic"),
        (b"ful", b""),
        (b"ness", b""),
    ];
    for (suf, repl) in STEP3 {
        if replace_if(&mut w, suf, repl, |s| measure(s) > 0) {
            break;
        }
    }

    // Step 4
    const STEP4: &[&[u8]] = &[
        b"al", b"ance", b"ence", b"er", b"ic", b"able", b"ible", b"ant", b"ement", b"ment", b"ent",
        b"ou", b"ism", b"ate", b"iti", b"ous", b"ive", b"ize",
    ];
    for suf in STEP4 {
        if ends_with(&w, suf) {
            let stem_len = w.len() - suf.len();
            if measure(&w[..stem_len]) > 1 {
                w.truncate(stem_len);
            }
            break;
        }
    }
    // Step 4 special case: "ion" only after s or t.
    if ends_with(&w, b"ion") {
        let stem_len = w.len() - 3;
        if measure(&w[..stem_len]) > 1 && matches!(w.get(stem_len - 1), Some(b's') | Some(b't')) {
            w.truncate(stem_len);
        }
    }

    // Step 5a
    if ends_with(&w, b"e") {
        let stem = &w[..w.len() - 1];
        let m = measure(stem);
        if m > 1 || (m == 1 && !ends_cvc(stem)) {
            w.pop();
        }
    }
    // Step 5b
    if measure(&w) > 1 && ends_double_consonant(&w) && w[w.len() - 1] == b'l' {
        w.pop();
    }

    String::from_utf8(w).unwrap_or_else(|_| word.to_string())
}

// -- Query parsing --------------------------------------------------------

/// One unit of a full-text query (Lucene's core term kinds).
#[derive(Clone, Debug)]
enum QueryTerm {
    /// Bare word, run through the index analyzer.
    Exact(String),
    /// `wor*`: every vocabulary term with the prefix (lowercased, NOT stemmed, as in Lucene).
    Prefix(String),
    /// `word~` / `word~1` / `word~2`: vocabulary terms within that Levenshtein distance (default
    /// and max 2).
    Fuzzy(String, u32),
    /// `"quoted phrase"`: analysed terms at their relative positions (stop words take positions, so
    /// gaps count).
    Phrase(Vec<(String, u32)>),
}

/// Query unit resolved against the index: matching docs and their score contributions.
struct ResolvedTerm {
    scores: rustc_hash::FxHashMap<NodeId, f64>,
}

/// Parsed query: units tagged by boolean role.
struct ParsedQuery {
    /// Scoring units; matching any qualifies a doc (unless required units exist).
    should: Vec<QueryTerm>,
    /// `+unit`: all must be present.
    must: Vec<QueryTerm>,
    /// `-unit`: none may be present.
    must_not: Vec<QueryTerm>,
}

/// Parses a Lucene-style query: whitespace-separated units, each optionally `+`
/// (required) / `-` (prohibited); a unit is a `"phrase"`, `prefix*`, `fuzzy~[N]`
/// or a bare term (analysed; one raw token may give several exact terms).
fn parse_query(query: &str, analyzer: Analyzer) -> ParsedQuery {
    let mut pq = ParsedQuery {
        should: Vec::new(),
        must: Vec::new(),
        must_not: Vec::new(),
    };
    let chars: Vec<char> = query.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        // modifier
        let bucket = match chars[i] {
            '+' => {
                i += 1;
                1u8
            }
            '-' => {
                i += 1;
                2u8
            }
            _ => 0u8,
        };
        let push = |pq: &mut ParsedQuery, t: QueryTerm, bucket: u8| match bucket {
            1 => pq.must.push(t),
            2 => pq.must_not.push(t),
            _ => pq.should.push(t),
        };
        if i < chars.len() && chars[i] == '"' {
            // quoted phrase: read to the closing quote (or end)
            i += 1;
            let start = i;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
            }
            let body: String = chars[start..i].iter().collect();
            if i < chars.len() {
                i += 1; // closing quote
            }
            let terms = analyzer.analyze_with_positions(&body);
            match terms.len() {
                0 => {}
                // one-word "phrase" = exact term
                1 => push(&mut pq, QueryTerm::Exact(terms[0].0.clone()), bucket),
                _ => push(&mut pq, QueryTerm::Phrase(terms), bucket),
            }
            continue;
        }
        // bare token: read to whitespace
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        let raw: String = chars[start..i].iter().collect();
        // trailing `*` (prefix) or `~[N]` (fuzzy)
        if let Some(body) = raw.strip_suffix('*') {
            if let Some(t) = analyzer.normalize_pattern_term(body) {
                push(&mut pq, QueryTerm::Prefix(t), bucket);
            }
            continue;
        }
        if let Some(tilde_at) = raw.rfind('~') {
            let (body, suffix) = raw.split_at(tilde_at);
            let dist_str = &suffix[1..];
            let dist = if dist_str.is_empty() {
                Some(2)
            } else {
                dist_str.parse::<u32>().ok().map(|d| d.min(2))
            };
            if let Some(dist) = dist {
                if let Some(t) = analyzer.normalize_pattern_term(body) {
                    if dist == 0 {
                        push(&mut pq, QueryTerm::Exact(t), bucket);
                    } else {
                        push(&mut pq, QueryTerm::Fuzzy(t, dist), bucket);
                    }
                }
                continue;
            }
            // `~garbage`: fall through, token taken verbatim
        }
        for term in analyzer.analyze(&raw) {
            push(&mut pq, QueryTerm::Exact(term), bucket);
        }
    }
    pq
}

/// Whether `a` and `b` are within Levenshtein distance `max` (unit costs, by char). Early exit on
/// length.
fn levenshtein_within(a: &str, b: &str, max: u32) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n.abs_diff(m) > max as usize {
        return false;
    }
    // classic DP over the shorter side, early exit per row
    let mut prev: Vec<u32> = (0..=m as u32).collect();
    let mut cur = vec![0u32; m + 1];
    for i in 1..=n {
        cur[0] = i as u32;
        let mut row_min = cur[0];
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        if row_min > max {
            return false;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m] <= max
}

// -- Inverted index -------------------------------------------------------

/// Position gap between indexed property values of one document, so phrases
/// never match across fields (Lucene's position-increment-gap).
const FIELD_POSITION_GAP: u32 = 100;

/// Posting: document, term frequency, ascending token positions (for phrases).
#[derive(Clone, Debug)]
struct Posting {
    node: NodeId,
    tf: u32,
    positions: Vec<u32>,
}

/// Named full-text index: BM25 inverted index over string properties of one or more labels.
/// Maintained incrementally.
#[derive(Clone, Debug)]
pub struct FulltextIndex {
    name: String,
    label_ids: Vec<LabelId>,
    property_keys: Vec<PropertyKeyId>,
    analyzer: Analyzer,
    /// Term -> dense term id.
    term_ids: FxHashMap<String, u32>,
    /// Postings by term id.
    postings: Vec<Vec<Posting>>,
    /// Per-document token count (BM25 length normalisation).
    doc_len: FxHashMap<NodeId, u32>,
    /// Per-document distinct term ids (to remove a doc on update/delete).
    doc_terms: FxHashMap<NodeId, Vec<u32>>,
    /// Sum of document lengths (BM25 avgdl = total_len / |docs|).
    total_len: u64,
}

impl FulltextIndex {
    /// Empty index over the given labels/properties.
    pub fn new(
        name: &str,
        label_ids: Vec<LabelId>,
        property_keys: Vec<PropertyKeyId>,
        analyzer: Analyzer,
    ) -> Self {
        Self {
            name: name.to_string(),
            label_ids,
            property_keys,
            analyzer,
            term_ids: FxHashMap::default(),
            postings: Vec::new(),
            doc_len: FxHashMap::default(),
            doc_terms: FxHashMap::default(),
            total_len: 0,
        }
    }

    /// The index name.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The indexed label ids.
    pub fn label_ids(&self) -> &[LabelId] {
        &self.label_ids
    }
    /// The indexed property-key ids.
    pub fn property_keys(&self) -> &[PropertyKeyId] {
        &self.property_keys
    }
    /// The analyzer this index uses.
    pub fn analyzer(&self) -> Analyzer {
        self.analyzer
    }
    /// Number of indexed documents.
    pub fn doc_count(&self) -> usize {
        self.doc_len.len()
    }

    fn intern_term(&mut self, term: &str) -> u32 {
        if let Some(&id) = self.term_ids.get(term) {
            return id;
        }
        let id = self.postings.len() as u32;
        self.term_ids.insert(term.to_string(), id);
        self.postings.push(Vec::new());
        id
    }

    /// (Re-)indexes a document from its indexed property values; re-indexing removes
    /// old postings first. Positions are kept per term (with a gap between values) so
    /// phrase queries can check adjacency.
    pub fn index_doc(&mut self, node: NodeId, texts: &[&str]) {
        if self.doc_terms.contains_key(&node) {
            self.remove_doc(node);
        }
        let analyzer = self.analyzer;
        let mut occurrences: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        let mut len: u32 = 0;
        let mut offset: u32 = 0;
        for text in texts {
            let toks = analyzer.analyze_with_positions(text);
            let mut max_pos: u32 = 0;
            for (term, pos) in &toks {
                let tid = self.intern_term(term);
                occurrences.entry(tid).or_default().push(offset + pos);
                max_pos = max_pos.max(*pos);
                len += 1;
            }
            if !toks.is_empty() {
                offset += max_pos + 1 + FIELD_POSITION_GAP;
            }
        }
        if len == 0 {
            return; // nothing to index
        }
        let mut term_list = Vec::with_capacity(occurrences.len());
        for (tid, positions) in occurrences {
            self.postings[tid as usize].push(Posting {
                node,
                tf: positions.len() as u32,
                positions,
            });
            term_list.push(tid);
        }
        self.doc_len.insert(node, len);
        self.doc_terms.insert(node, term_list);
        self.total_len += len as u64;
    }

    /// Removes a document (delete, or first half of a re-index).
    pub fn remove_doc(&mut self, node: NodeId) {
        if let Some(terms) = self.doc_terms.remove(&node) {
            for tid in terms {
                let list = &mut self.postings[tid as usize];
                if let Some(pos) = list.iter().position(|p| p.node == node) {
                    list.swap_remove(pos);
                }
            }
        }
        if let Some(len) = self.doc_len.remove(&node) {
            self.total_len -= len as u64;
        }
    }

    /// Runs a query; up to `limit` `(node, BM25 score)` pairs, best first. Lucene core
    /// syntax: bare terms (OR), `+required` / `-prohibited`, `"phrases"`
    /// (position-checked), `prefix*`, `fuzzy~[N]` (edit distance <= 2).
    pub fn query(&self, query: &str, limit: usize) -> Vec<(NodeId, f64)> {
        let n = self.doc_len.len();
        if n == 0 || limit == 0 {
            return Vec::new();
        }
        let pq = parse_query(query, self.analyzer);
        if pq.should.is_empty() && pq.must.is_empty() {
            return Vec::new();
        }

        // resolve each unit once: matching docs and per-doc score contributions
        let resolved_must: Vec<ResolvedTerm> =
            pq.must.iter().map(|t| self.resolve_term(t)).collect();
        let resolved_should: Vec<ResolvedTerm> =
            pq.should.iter().map(|t| self.resolve_term(t)).collect();

        // candidates: intersection of the `must` units if any, else union of scoring units
        let mut candidates: FxHashSet<NodeId> = FxHashSet::default();
        if !resolved_must.is_empty() {
            // start from the smallest required set, intersect the rest
            let mut sets: Vec<&FxHashMap<NodeId, f64>> =
                resolved_must.iter().map(|r| &r.scores).collect();
            if sets.iter().any(|s| s.is_empty()) {
                return Vec::new(); // required unit matches nothing
            }
            sets.sort_by_key(|s| s.len());
            candidates.extend(sets[0].keys().copied());
            for set in &sets[1..] {
                candidates.retain(|n| set.contains_key(n));
            }
        } else {
            for r in &resolved_should {
                candidates.extend(r.scores.keys().copied());
            }
        }

        // drop prohibited docs
        for t in &pq.must_not {
            let r = self.resolve_term(t);
            for node in r.scores.keys() {
                candidates.remove(node);
            }
        }
        if candidates.is_empty() {
            return Vec::new();
        }

        // score = sum of contributions of every scoring unit
        let mut scores: FxHashMap<NodeId, f64> = FxHashMap::default();
        for r in resolved_should.iter().chain(resolved_must.iter()) {
            for (node, s) in &r.scores {
                if candidates.contains(node) {
                    *scores.entry(*node).or_insert(0.0) += s;
                }
            }
        }

        let mut ranked: Vec<(NodeId, f64)> = scores.into_iter().collect();
        // best first; ties by node id (deterministic)
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.0.cmp(&b.0.0))
        });
        ranked.truncate(limit);
        ranked
    }

    /// BM25 score this index would give a document with `texts` for `query`, using the index's
    /// corpus statistics; `None` when the document doesn't match. Same query semantics as
    /// [`Self::query`], for scoring a version of a document the index no longer holds.
    pub fn score_texts(&self, query: &str, texts: &[&str]) -> Option<f64> {
        let pq = parse_query(query, self.analyzer);
        if pq.should.is_empty() && pq.must.is_empty() {
            return None;
        }
        // term positions, laid out as `index_doc` does
        let mut positions: FxHashMap<String, Vec<u32>> = FxHashMap::default();
        let mut len: u32 = 0;
        let mut offset: u32 = 0;
        for text in texts {
            let toks = self.analyzer.analyze_with_positions(text);
            let mut max_pos: u32 = 0;
            for (term, pos) in toks.iter() {
                positions
                    .entry(term.clone())
                    .or_default()
                    .push(offset + pos);
                max_pos = max_pos.max(*pos);
                len += 1;
            }
            if !toks.is_empty() {
                offset += max_pos + 1 + FIELD_POSITION_GAP;
            }
        }
        if len == 0 {
            return None;
        }
        let bm25 = |term: &str, tf: usize| self.term_score(term, tf, len);
        let best = |hits: Vec<f64>| hits.into_iter().reduce(f64::max);
        let unit = |t: &QueryTerm| -> Option<f64> {
            match t {
                QueryTerm::Exact(term) => positions.get(term).map(|p| bm25(term, p.len())),
                QueryTerm::Prefix(prefix) => best(
                    positions
                        .iter()
                        .filter(|(t, _)| t.starts_with(prefix.as_str()))
                        .map(|(t, p)| bm25(t, p.len()))
                        .collect(),
                ),
                QueryTerm::Fuzzy(term, dist) => best(
                    positions
                        .iter()
                        .filter(|(t, _)| levenshtein_within(term, t, *dist))
                        .map(|(t, p)| bm25(t, p.len()))
                        .collect(),
                ),
                QueryTerm::Phrase(terms) => {
                    let lists: Option<Vec<&Vec<u32>>> =
                        terms.iter().map(|(t, _)| positions.get(t)).collect();
                    let lists = lists?;
                    let base = terms[0].1;
                    let matched = lists[0].iter().any(|&p0| {
                        terms.iter().zip(&lists).skip(1).all(|((_, rel), ps)| {
                            let want = p0 as i64 + (*rel as i64 - base as i64);
                            want >= 0 && ps.contains(&(want as u32))
                        })
                    });
                    matched.then(|| {
                        terms
                            .iter()
                            .zip(&lists)
                            .map(|((t, _), ps)| bm25(t, ps.len()))
                            .sum()
                    })
                }
            }
        };
        if pq.must_not.iter().any(|t| unit(t).is_some()) {
            return None;
        }
        let mut score = 0.0;
        for t in &pq.must {
            score += unit(t)?;
        }
        let mut any_should = false;
        for t in &pq.should {
            if let Some(s) = unit(t) {
                score += s;
                any_should = true;
            }
        }
        (!pq.must.is_empty() || any_should).then_some(score)
    }

    /// BM25 contribution of `term` occurring `tf` times in a document of `doc_len` tokens.
    fn term_score(&self, term: &str, tf: usize, doc_len: u32) -> f64 {
        let n = self.doc_len.len().max(1) as f64;
        let avgdl = (self.total_len as f64 / n).max(1.0);
        let df = self
            .term_ids
            .get(term)
            .map_or(0.0, |&tid| self.postings[tid as usize].len() as f64);
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        let tf = tf as f64;
        let norm = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * doc_len as f64 / avgdl);
        idf * (tf * (BM25_K1 + 1.0)) / norm
    }

    /// BM25 contribution of one vocabulary term per matching doc, folded into `out` with `combine`.
    fn accumulate_term_scores(
        &self,
        tid: u32,
        out: &mut FxHashMap<NodeId, f64>,
        combine: fn(&mut f64, f64),
    ) {
        let n = self.doc_len.len() as f64;
        let avgdl = self.total_len as f64 / n;
        let list = &self.postings[tid as usize];
        let df = list.len() as f64;
        // Lucene BM25 IDF, never negative
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        for p in list {
            let dl = self.doc_len[&p.node] as f64;
            let tf = p.tf as f64;
            let norm = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / avgdl);
            let s = idf * (tf * (BM25_K1 + 1.0)) / norm;
            combine(out.entry(p.node).or_insert(0.0), s);
        }
    }

    /// One query unit -> matching docs + per-doc scores.
    fn resolve_term(&self, term: &QueryTerm) -> ResolvedTerm {
        let mut scores: FxHashMap<NodeId, f64> = FxHashMap::default();
        match term {
            QueryTerm::Exact(t) => {
                if let Some(&tid) = self.term_ids.get(t) {
                    self.accumulate_term_scores(tid, &mut scores, |slot, s| *slot += s);
                }
            }
            QueryTerm::Prefix(prefix) => {
                // expand against the vocabulary; each doc scores its BEST expansion (Lucene
                // rewrites multi-term queries, doesn't sum variants)
                for (t, &tid) in &self.term_ids {
                    if t.starts_with(prefix.as_str()) {
                        self.accumulate_term_scores(tid, &mut scores, |slot, s| {
                            *slot = slot.max(s)
                        });
                    }
                }
            }
            QueryTerm::Fuzzy(t, dist) => {
                for (vocab, &tid) in &self.term_ids {
                    if levenshtein_within(t, vocab, *dist) {
                        self.accumulate_term_scores(tid, &mut scores, |slot, s| {
                            *slot = slot.max(s)
                        });
                    }
                }
            }
            QueryTerm::Phrase(terms) => {
                scores = self.resolve_phrase(terms);
            }
        }
        ResolvedTerm { scores }
    }

    /// Docs with the phrase terms at their relative positions; score = sum of the terms' BM25
    /// contributions.
    fn resolve_phrase(&self, terms: &[(String, u32)]) -> FxHashMap<NodeId, f64> {
        let mut out: FxHashMap<NodeId, f64> = FxHashMap::default();
        // every term must be in the vocabulary
        let mut tids: Vec<u32> = Vec::with_capacity(terms.len());
        for (t, _) in terms {
            match self.term_ids.get(t) {
                Some(&tid) => tids.push(tid),
                None => return out,
            }
        }
        // candidates: docs in every term's postings; walk the rarest list, probe the others
        let base = terms[0].1;
        let rarest = *tids
            .iter()
            .min_by_key(|&&tid| self.postings[tid as usize].len())
            .unwrap();
        'docs: for p in &self.postings[rarest as usize] {
            let node = p.node;
            // each term's positions in this doc
            let mut pos_lists: Vec<&[u32]> = Vec::with_capacity(tids.len());
            for &tid in &tids {
                match self.postings[tid as usize].iter().find(|q| q.node == node) {
                    Some(q) => pos_lists.push(&q.positions),
                    None => continue 'docs,
                }
            }
            // match: some position of term 0 lines up every other term at its offset
            let matched = pos_lists[0].iter().any(|&p0| {
                terms
                    .iter()
                    .zip(&pos_lists)
                    .skip(1)
                    .all(|((_, rel), positions)| {
                        let want = p0 as i64 + (*rel as i64 - base as i64);
                        want >= 0 && positions.binary_search(&(want as u32)).is_ok()
                    })
            });
            if matched {
                out.insert(node, 0.0);
            }
        }
        if out.is_empty() {
            return out;
        }
        // score matches: sum of term BM25 contributions
        let mut term_scores: FxHashMap<NodeId, f64> = FxHashMap::default();
        for &tid in &tids {
            self.accumulate_term_scores(tid, &mut term_scores, |slot, s| *slot += s);
        }
        for (node, s) in out.iter_mut() {
            *s = term_scores.get(node).copied().unwrap_or(0.0);
        }
        out
    }
}

// -- Registry -------------------------------------------------------------

/// Full-text indexes of a graph, by name.
#[derive(Clone, Debug, Default)]
pub struct FulltextIndexRegistry {
    by_name: FxHashMap<String, FulltextIndex>,
    generation: u64,
}

impl FulltextIndexRegistry {
    /// Registers a new empty index. Errors if the name is taken.
    pub fn create(
        &mut self,
        name: &str,
        label_ids: Vec<LabelId>,
        property_keys: Vec<PropertyKeyId>,
        analyzer: Analyzer,
    ) -> Result<(), String> {
        if self.by_name.contains_key(name) {
            return Err(format!("fulltext index '{name}' already exists"));
        }
        self.by_name.insert(
            name.to_string(),
            FulltextIndex::new(name, label_ids, property_keys, analyzer),
        );
        self.generation += 1;
        Ok(())
    }

    /// Drop an index by name.
    pub fn drop(&mut self, name: &str) -> Result<(), String> {
        if self.by_name.remove(name).is_none() {
            return Err(format!("no such fulltext index: {name}"));
        }
        self.generation += 1;
        Ok(())
    }

    /// Look up an index by name.
    pub fn get(&self, name: &str) -> Option<&FulltextIndex> {
        self.by_name.get(name)
    }
    /// Mutable lookup (maintenance).
    pub fn get_mut(&mut self, name: &str) -> Option<&mut FulltextIndex> {
        self.by_name.get_mut(name)
    }
    /// Iterate all indexes.
    pub fn iter(&self) -> impl Iterator<Item = &FulltextIndex> {
        self.by_name.values()
    }
    /// No full-text indexes (write fast path).
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
    /// Indexes covering `label` (write maintenance).
    pub fn indexes_for_label(&self, label: LabelId) -> Vec<String> {
        self.by_name
            .values()
            .filter(|idx| idx.label_ids.contains(&label))
            .map(|idx| idx.name.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nid(n: u64) -> NodeId {
        NodeId(n)
    }

    #[test]
    fn standard_analyzer_tokenizes_and_lowercases() {
        let a = Analyzer::Standard;
        assert_eq!(a.analyze("Hello, World! 42"), vec!["hello", "world", "42"]);
        assert_eq!(a.analyze("  "), Vec::<String>::new());
    }

    #[test]
    fn keyword_and_whitespace_analyzers() {
        assert_eq!(Analyzer::Keyword.analyze("New York"), vec!["New York"]);
        assert_eq!(
            Analyzer::Whitespace.analyze("New York"),
            vec!["New", "York"]
        );
    }

    #[test]
    fn english_analyzer_strips_stopwords_and_stems() {
        let terms = Analyzer::English.analyze("The cats are running quickly");
        // "the"/"are" are stop words; cats->cat, running->run, quickly->quickli
        assert_eq!(terms, vec!["cat", "run", "quickli"]);
    }

    #[test]
    fn porter_stemmer_known_cases() {
        for (word, stem) in [
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("cats", "cat"),
            ("running", "run"),
            ("agreed", "agre"),
            ("happiness", "happi"),
            ("relational", "relat"),
            ("conditional", "condit"),
            ("rational", "ration"),
            ("plastered", "plaster"),
            ("nationalization", "nation"),
        ] {
            assert_eq!(porter_stem(word), stem, "stemming {word}");
        }
    }

    const DOCS: &[(u64, &[&str])] = &[
        (1, &["the quick brown fox jumps over the lazy dog"]),
        (2, &["a quick brown dog outpaces a quick fox"]),
        (3, &["lazy cats sleep all day"]),
    ];

    fn build() -> FulltextIndex {
        let mut idx = FulltextIndex::new(
            "ft",
            vec![LabelId(1)],
            vec![PropertyKeyId(1)],
            Analyzer::English,
        );
        for (n, texts) in DOCS {
            idx.index_doc(nid(*n), texts);
        }
        idx
    }

    #[test]
    fn score_texts_matches_query_scores() {
        let idx = build();
        for q in [
            "fox",
            "quick fox",
            "+lazy dog",
            "\"brown fox\"",
            "qui*",
            "foz~1",
            "fox -lazy",
        ] {
            let ranked = idx.query(q, 10);
            for (node, score) in &ranked {
                let texts: Vec<&str> = DOCS
                    .iter()
                    .find(|(n, _)| nid(*n) == *node)
                    .map(|(_, t)| t.to_vec())
                    .unwrap();
                let s = idx.score_texts(q, &texts).expect("indexed doc matches");
                assert!((s - score).abs() < 1e-9, "{q}: {s} vs {score}");
            }
            // non-matching docs score None
            for (n, texts) in DOCS {
                if !ranked.iter().any(|(m, _)| *m == nid(*n)) {
                    assert!(idx.score_texts(q, texts).is_none(), "{q} matched doc {n}");
                }
            }
        }
    }

    #[test]
    fn bm25_ranks_more_relevant_docs_higher() {
        let idx = build();
        let hits = idx.query("quick fox", 10);
        assert!(!hits.is_empty());
        // doc 2 has "quick" twice and "fox" once: ranks above doc 1
        assert_eq!(hits[0].0, nid(2));
        assert!(hits.iter().any(|(n, _)| *n == nid(1)));
        // Doc 3 has neither term.
        assert!(!hits.iter().any(|(n, _)| *n == nid(3)));
        // Scores strictly descending.
        for w in hits.windows(2) {
            assert!(w[0].1 >= w[1].1);
        }
    }

    #[test]
    fn required_and_prohibited_terms() {
        let idx = build();
        // +fox requires "fox"; only docs 1 and 2 have it.
        let must = idx.query("+fox lazy", 10);
        let ids: Vec<NodeId> = must.iter().map(|(n, _)| *n).collect();
        assert!(ids.contains(&nid(1)) && ids.contains(&nid(2)) && !ids.contains(&nid(3)));
        // -lazy prohibits "lazy"; excludes doc 1.
        let not = idx.query("quick -lazy", 10);
        let ids2: Vec<NodeId> = not.iter().map(|(n, _)| *n).collect();
        assert!(ids2.contains(&nid(2)) && !ids2.contains(&nid(1)));
    }

    #[test]
    fn update_and_delete_maintains_index() {
        let mut idx = build();
        // Re-index doc 3 to now mention "fox".
        idx.index_doc(nid(3), &["a clever fox appears"]);
        let hits = idx.query("fox", 10);
        assert!(hits.iter().any(|(n, _)| *n == nid(3)));
        // Delete doc 1; it must vanish from results.
        idx.remove_doc(nid(1));
        let hits2 = idx.query("fox", 10);
        assert!(!hits2.iter().any(|(n, _)| *n == nid(1)));
        assert_eq!(idx.doc_count(), 2);
    }

    #[test]
    fn phrase_query_requires_adjacency() {
        let idx = build();
        // "quick brown" adjacent in docs 1 and 2; "brown quick" nowhere
        let hits = idx.query("\"quick brown\"", 10);
        let ids: Vec<NodeId> = hits.iter().map(|(n, _)| *n).collect();
        assert!(ids.contains(&nid(1)) && ids.contains(&nid(2)), "{ids:?}");
        assert!(idx.query("\"brown quick\"", 10).is_empty());
        // "quick fox" isn't adjacent in doc 1 ("quick brown fox") but is in doc 2 ("a quick fox")
        let hits2 = idx.query("\"quick fox\"", 10);
        let ids2: Vec<NodeId> = hits2.iter().map(|(n, _)| *n).collect();
        assert_eq!(ids2, vec![nid(2)]);
    }

    #[test]
    fn phrase_respects_stopword_gaps() {
        // english analyzer: in "jumps over the lazy dog" the stop word "the" takes a
        // position, so "jump lazi" isn't adjacent, yet the original phrase matches
        // through the same gap
        let idx = build();
        let hits = idx.query("\"over the lazy dog\"", 10);
        let ids: Vec<NodeId> = hits.iter().map(|(n, _)| *n).collect();
        assert_eq!(ids, vec![nid(1)]);
        // dropping the stop word from the query shifts positions: no match
        assert!(idx.query("\"over lazy\"", 10).is_empty());
    }

    #[test]
    fn phrase_does_not_match_across_fields() {
        let mut idx = FulltextIndex::new(
            "ft",
            vec![LabelId(1)],
            vec![PropertyKeyId(1)],
            Analyzer::Standard,
        );
        // two property values: "alpha beta" must not match across the boundary
        idx.index_doc(nid(1), &["ends with alpha", "beta starts here"]);
        idx.index_doc(nid(2), &["alpha beta together"]);
        let hits = idx.query("\"alpha beta\"", 10);
        let ids: Vec<NodeId> = hits.iter().map(|(n, _)| *n).collect();
        assert_eq!(ids, vec![nid(2)]);
    }

    #[test]
    fn prefix_query_expands_vocabulary() {
        let idx = build();
        // stems present: quick, brown, fox, jump, lazi, dog, outpac, cat, sleep, dai
        let hits = idx.query("qui*", 10);
        let ids: Vec<NodeId> = hits.iter().map(|(n, _)| *n).collect();
        assert!(ids.contains(&nid(1)) && ids.contains(&nid(2)));
        assert!(!ids.contains(&nid(3)));
        // "d*" matches dog (1,2) and dai (3)
        let hits2 = idx.query("d*", 10);
        assert_eq!(hits2.len(), 3);
        // No vocabulary term starts with zz.
        assert!(idx.query("zz*", 10).is_empty());
    }

    #[test]
    fn fuzzy_query_matches_within_edit_distance() {
        let idx = build();
        // "quik" ~1 -> "quick" (one insertion)
        let hits = idx.query("quik~1", 10);
        let ids: Vec<NodeId> = hits.iter().map(|(n, _)| *n).collect();
        assert!(ids.contains(&nid(1)) && ids.contains(&nid(2)), "{ids:?}");
        // bare ~ means distance 2: "quicc" -> quick (1 sub)
        assert!(!idx.query("quicc~", 10).is_empty());
        // distance 1 can't fix 3 edits
        assert!(idx.query("qxxxk~1", 10).is_empty());
    }

    #[test]
    fn operators_compose_with_new_forms() {
        let idx = build();
        // Required phrase + prohibited term.
        let hits = idx.query("+\"quick brown\" -outpaces", 10);
        let ids: Vec<NodeId> = hits.iter().map(|(n, _)| *n).collect();
        assert_eq!(ids, vec![nid(1)], "doc 2 excluded by -outpaces");
        // Required prefix.
        let hits2 = idx.query("+sle* cats", 10);
        let ids2: Vec<NodeId> = hits2.iter().map(|(n, _)| *n).collect();
        assert_eq!(ids2, vec![nid(3)]);
        // required unit matches nothing -> empty
        assert!(idx.query("+zz* quick", 10).is_empty());
    }

    #[test]
    fn levenshtein_bounds() {
        assert!(levenshtein_within("kitten", "kitten", 0));
        assert!(levenshtein_within("kitten", "sitten", 1));
        assert!(!levenshtein_within("kitten", "sittin", 1));
        assert!(levenshtein_within("kitten", "sittin", 2));
        assert!(!levenshtein_within("kitten", "sitting", 2));
        assert!(levenshtein_within("", "ab", 2));
        assert!(!levenshtein_within("", "abc", 2));
    }

    #[test]
    fn empty_and_missing_terms() {
        let idx = build();
        assert!(idx.query("", 10).is_empty());
        assert!(idx.query("nonexistentword", 10).is_empty());
        let empty = FulltextIndex::new("e", vec![], vec![], Analyzer::Standard);
        assert!(empty.query("anything", 10).is_empty());
    }
}
