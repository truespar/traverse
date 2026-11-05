//! Typed property columns: one property key projected over the dense id prefix as a typed
//! array with validity bits, kept in step with the row store by the index maintenance hooks.
//! A column is built the first time broad gathers have asked for its key often enough, so
//! point lookups never pay for one; from then on a gather over many entities in id order
//! streams the array instead of chasing every entity's property heap. Values of a type the
//! column doesn't hold (a key that is an integer on most entities and a string on a few, lists,
//! maps, temporals) are flagged and read from the row store, so every value stays exact.

use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustc_hash::{FxHashMap, FxHashSet};

use super::batch::{ColumnBound, ColumnCmp, ColumnSink};
use super::compact_props::Scalar;
use super::property::InternedPropertyMap;
use super::types::{NodeId, PropertyKeyId};

/// Lanes a key must have been gathered over before its column is built.
const BUILD_AFTER: usize = 32 * 1024;

/// A bit per id.
#[derive(Default, Clone)]
struct Bits(Vec<u64>);

impl Bits {
    #[inline]
    fn get(&self, i: usize) -> bool {
        match self.0.get(i / 64) {
            Some(w) => w & (1u64 << (i % 64)) != 0,
            None => false,
        }
    }

    #[inline]
    fn set(&mut self, i: usize, on: bool) {
        let w = i / 64;
        if w >= self.0.len() {
            if !on {
                return;
            }
            self.0.resize(w + 1, 0);
        }
        if on {
            self.0[w] |= 1u64 << (i % 64);
        } else {
            self.0[w] &= !(1u64 << (i % 64));
        }
    }

    fn with_len(n: usize) -> Self {
        Self(vec![0; n.div_ceil(64)])
    }

    /// Every bit in `[start, end)` set?
    fn all_set(&self, start: usize, end: usize) -> bool {
        self.count_range(start, end, true)
    }

    /// Whether every bit in `[start, end)` equals `want`, a word at a time.
    fn count_range(&self, start: usize, end: usize, want: bool) -> bool {
        let mut i = start;
        while i < end {
            let w = i / 64;
            let lo = i % 64;
            let hi = (end - w * 64).min(64);
            let mask = if hi == 64 {
                u64::MAX << lo
            } else {
                ((1u64 << hi) - 1) & (u64::MAX << lo)
            };
            let word = self.0.get(w).copied().unwrap_or(0);
            let got = word & mask;
            if (want && got != mask) || (!want && got != 0) {
                return false;
            }
            i = (w + 1) * 64;
        }
        true
    }

    fn bytes(&self) -> usize {
        self.0.capacity() * 8
    }
}

/// What a column holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Bool,
    Int,
    Float,
    Str,
}

/// A string column: spans into one byte arena; a replaced string is appended and its old span
/// left dead until the arena is rebuilt.
#[derive(Default, Clone)]
struct StrData {
    start: Vec<u64>,
    len: Vec<u32>,
    bytes: Vec<u8>,
    dead: usize,
}

#[derive(Clone)]
enum ColumnData {
    Bool(Bits),
    Int(Vec<i64>),
    Float(Vec<f64>),
    Str(StrData),
}

/// One key's column over ids `[0, len)`.
#[derive(Clone)]
pub(crate) struct TypedColumn {
    /// The entity holds a value of the column's type.
    valid: Bits,
    /// The entity holds a value of another type: read the row.
    other: Bits,
    data: ColumnData,
    len: usize,
}

impl TypedColumn {
    fn new(kind: Kind, len: usize) -> Self {
        let data = match kind {
            Kind::Bool => ColumnData::Bool(Bits::with_len(len)),
            Kind::Int => ColumnData::Int(vec![0; len]),
            Kind::Float => ColumnData::Float(vec![0.0; len]),
            Kind::Str => ColumnData::Str(StrData {
                start: vec![0; len],
                len: vec![0; len],
                bytes: Vec::new(),
                dead: 0,
            }),
        };
        Self {
            valid: Bits::with_len(len),
            other: Bits::with_len(len),
            data,
            len,
        }
    }

    fn grow(&mut self, len: usize) {
        if len <= self.len {
            return;
        }
        match &mut self.data {
            ColumnData::Bool(_) => {}
            ColumnData::Int(v) => v.resize(len, 0),
            ColumnData::Float(v) => v.resize(len, 0.0),
            ColumnData::Str(s) => {
                s.start.resize(len, 0);
                s.len.resize(len, 0);
            }
        }
        self.len = len;
    }

    /// Records the entity's value for the key (`None`: absent or null).
    fn set(&mut self, id: usize, v: Option<Scalar<'_>>) {
        self.grow(id + 1);
        let typed = match (&mut self.data, v) {
            (_, None) => {
                self.valid.set(id, false);
                self.other.set(id, false);
                return;
            }
            (ColumnData::Bool(b), Some(Scalar::Bool(x))) => {
                b.set(id, x);
                true
            }
            (ColumnData::Int(v), Some(Scalar::Int(x))) => {
                v[id] = x;
                true
            }
            (ColumnData::Float(v), Some(Scalar::Float(x))) => {
                v[id] = x;
                true
            }
            (ColumnData::Str(s), Some(Scalar::Str(x))) => {
                if self.valid.get(id) {
                    let old = s.len[id] as usize;
                    let at = s.start[id] as usize;
                    if x.len() <= old {
                        // fits in place; the tail of the old span goes dead
                        s.bytes[at..at + x.len()].copy_from_slice(x.as_bytes());
                        s.dead += old - x.len();
                        s.len[id] = x.len() as u32;
                        self.other.set(id, false);
                        return;
                    }
                    s.dead += old;
                }
                s.start[id] = s.bytes.len() as u64;
                s.len[id] = x.len() as u32;
                s.bytes.extend_from_slice(x.as_bytes());
                true
            }
            _ => false,
        };
        self.valid.set(id, typed);
        self.other.set(id, !typed);
    }

    /// Pushes the entity's lane: the typed value, or `row` for a value the column doesn't hold.
    #[inline]
    fn push_lane(
        &self,
        id: usize,
        sink: &mut dyn ColumnSink,
        row: impl FnOnce(&mut dyn ColumnSink),
    ) {
        if id >= self.len {
            row(sink);
            return;
        }
        if self.valid.get(id) {
            match &self.data {
                ColumnData::Bool(b) => sink.push_bool(b.get(id)),
                ColumnData::Int(v) => sink.push_int(v[id]),
                ColumnData::Float(v) => sink.push_float(v[id]),
                ColumnData::Str(s) => {
                    let at = s.start[id] as usize;
                    let end = at + s.len[id] as usize;
                    // SAFETY: the arena only ever holds whole strings copied from the row store
                    sink.push_str(unsafe { std::str::from_utf8_unchecked(&s.bytes[at..end]) });
                }
            }
        } else if self.other.get(id) {
            row(sink);
        } else {
            sink.push_null();
        }
    }

    /// Pushes the lanes of ids `[start, start + n)` at once where the column holds them as
    /// one typed array; `row` reads any lane of another type. The run must lie within `len`.
    fn push_run(
        &self,
        start: usize,
        n: usize,
        sink: &mut dyn ColumnSink,
        mut row: impl FnMut(usize, &mut dyn ColumnSink),
    ) {
        let end = start + n;
        let typed = matches!(self.data, ColumnData::Int(_) | ColumnData::Float(_));
        if !typed {
            for i in start..end {
                self.push_lane(i, sink, |s| row(i, s));
            }
            return;
        }
        // typed stretches between the lanes of another type stream as slices
        let mut at = start;
        while at < end {
            let stretch_end = (at..end).find(|&i| self.other.get(i)).unwrap_or(end);
            if stretch_end > at {
                let valid: Vec<bool> = if self.valid.all_set(at, stretch_end) {
                    vec![true; stretch_end - at]
                } else {
                    (at..stretch_end).map(|i| self.valid.get(i)).collect()
                };
                match &self.data {
                    ColumnData::Int(v) => sink.push_ints(&v[at..stretch_end], &valid),
                    ColumnData::Float(v) => sink.push_floats(&v[at..stretch_end], &valid),
                    _ => unreachable!("typed"),
                }
            }
            if stretch_end < end {
                row(stretch_end, sink);
            }
            at = stretch_end + 1;
        }
    }

    /// Calls `f` with every node of `words` (a label bitmap) whose value satisfies `cmp`
    /// against `bound`; `false` (and no call) when a lane holds a value of another type or the
    /// types don't compare, so the caller decides the exact way.
    fn for_each_where(
        &self,
        words: &[u64],
        first_word: usize,
        cmp: ColumnCmp,
        bound: &ColumnBound,
        mut f: impl FnMut(usize),
    ) -> bool {
        if self.other.0.iter().any(|w| *w != 0) {
            return false;
        }
        let len = self.len;
        // the hot shape: integers against an integer, a tight loop per full word
        if let (ColumnData::Int(v), ColumnBound::Int(k)) = (&self.data, bound) {
            let k = *k;
            for (w, &word) in words.iter().enumerate() {
                if word == 0 {
                    continue;
                }
                let base = (first_word + w) * 64;
                if word == u64::MAX && base + 64 <= len {
                    for i in base..base + 64 {
                        if self.valid.get(i) && cmp.holds(v[i].cmp(&k)) {
                            f(i);
                        }
                    }
                } else {
                    let mut bits = word;
                    while bits != 0 {
                        let i = base + bits.trailing_zeros() as usize;
                        bits &= bits - 1;
                        if i < len && self.valid.get(i) && cmp.holds(v[i].cmp(&k)) {
                            f(i);
                        }
                    }
                }
            }
            return true;
        }
        let test: Box<dyn Fn(usize) -> bool> = match (&self.data, bound) {
            (ColumnData::Int(v), ColumnBound::Float(k)) => {
                let k = *k;
                Box::new(move |i| (v[i] as f64).partial_cmp(&k).is_some_and(|o| cmp.holds(o)))
            }
            (ColumnData::Float(v), ColumnBound::Int(k)) => {
                let k = *k as f64;
                Box::new(move |i| v[i].partial_cmp(&k).is_some_and(|o| cmp.holds(o)))
            }
            (ColumnData::Float(v), ColumnBound::Float(k)) => {
                let k = *k;
                Box::new(move |i| v[i].partial_cmp(&k).is_some_and(|o| cmp.holds(o)))
            }
            (ColumnData::Str(s), ColumnBound::Str(k)) => {
                let k = k.as_bytes();
                Box::new(move |i| {
                    let at = s.start[i] as usize;
                    let end = at + s.len[i] as usize;
                    cmp.holds(s.bytes[at..end].cmp(k))
                })
            }
            _ => return false,
        };
        for (w, &word) in words.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let i = (first_word + w) * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if i < len && self.valid.get(i) && test(i) {
                    f(i);
                }
            }
        }
        true
    }

    /// The nodes of `words` whose value satisfies the comparison, counted; `None` when the
    /// column can't say exactly. Integers against an integer count branchlessly, a word block
    /// per task.
    fn count_where(&self, words: &[u64], cmp: ColumnCmp, bound: &ColumnBound) -> Option<usize> {
        if self.other.0.iter().any(|w| *w != 0) {
            return None;
        }
        if let (ColumnData::Int(v), ColumnBound::Int(k)) = (&self.data, bound) {
            let k = *k;
            let valid = &self.valid.0;
            let len = self.len;
            // one closure per comparison so the inner loop is a plain predicate
            macro_rules! count_with {
                ($pred:expr) => {{
                    let block = |ws: &[u64], first: usize| -> usize {
                        let mut c = 0usize;
                        for (w, &word) in ws.iter().enumerate() {
                            if word == 0 {
                                continue;
                            }
                            let wi = first + w;
                            let base = wi * 64;
                            let vbits = valid.get(wi).copied().unwrap_or(0);
                            if word == u64::MAX && base + 64 <= len {
                                let vs = &v[base..base + 64];
                                let mut hit = 0usize;
                                for (i, x) in vs.iter().enumerate() {
                                    hit += (((vbits >> i) & 1) != 0 && $pred(*x)) as usize;
                                }
                                c += hit;
                            } else {
                                let mut bits = word & vbits;
                                while bits != 0 {
                                    let b = bits.trailing_zeros() as usize;
                                    bits &= bits - 1;
                                    let i = base + b;
                                    if i < len && $pred(v[i]) {
                                        c += 1;
                                    }
                                }
                            }
                        }
                        c
                    };
                    if words.len() >= 2048 {
                        use rayon::prelude::*;
                        words
                            .par_chunks(1024)
                            .enumerate()
                            .map(|(i, ws)| block(ws, i * 1024))
                            .sum()
                    } else {
                        block(words, 0)
                    }
                }};
            }
            let count = match cmp {
                ColumnCmp::Lt => count_with!(|x: i64| x < k),
                ColumnCmp::Lte => count_with!(|x: i64| x <= k),
                ColumnCmp::Gt => count_with!(|x: i64| x > k),
                ColumnCmp::Gte => count_with!(|x: i64| x >= k),
                ColumnCmp::Eq => count_with!(|x: i64| x == k),
                ColumnCmp::Neq => count_with!(|x: i64| x != k),
            };
            return Some(count);
        }
        let mut count = 0usize;
        self.for_each_where(words, 0, cmp, bound, |_| count += 1)
            .then_some(count)
    }

    /// The nodes of `words` whose value satisfies the comparison, in id order; `None` when the
    /// column can't say exactly. A word block per task.
    fn ids_where(&self, words: &[u64], cmp: ColumnCmp, bound: &ColumnBound) -> Option<Vec<NodeId>> {
        if words.len() >= 2048 {
            use rayon::prelude::*;
            let parts: Vec<Option<Vec<NodeId>>> = words
                .par_chunks(1024)
                .enumerate()
                .map(|(i, ws)| {
                    let mut out = Vec::new();
                    self.for_each_where(ws, i * 1024, cmp, bound, |id| out.push(NodeId(id as u64)))
                        .then_some(out)
                })
                .collect();
            let mut all = Vec::new();
            for p in parts {
                all.extend(p?);
            }
            return Some(all);
        }
        let mut out = Vec::new();
        self.for_each_where(words, 0, cmp, bound, |i| out.push(NodeId(i as u64)))
            .then_some(out)
    }

    /// Rebuilds a string arena whose dead space has grown past its live bytes.
    fn maybe_compact(&mut self) {
        if let ColumnData::Str(s) = &mut self.data
            && s.dead > s.bytes.len() / 2
            && s.dead > 1 << 20
        {
            let mut bytes = Vec::with_capacity(s.bytes.len() - s.dead);
            for id in 0..self.len {
                if self.valid.get(id) {
                    let at = s.start[id] as usize;
                    let n = s.len[id] as usize;
                    s.start[id] = bytes.len() as u64;
                    bytes.extend_from_slice(&s.bytes[at..at + n]);
                }
            }
            s.bytes = bytes;
            s.dead = 0;
        }
    }

    fn bytes(&self) -> usize {
        let data = match &self.data {
            ColumnData::Bool(b) => b.bytes(),
            ColumnData::Int(v) => v.capacity() * 8,
            ColumnData::Float(v) => v.capacity() * 8,
            ColumnData::Str(s) => {
                s.start.capacity() * 8 + s.len.capacity() * 4 + s.bytes.capacity()
            }
        };
        data + self.valid.bytes() + self.other.bytes()
    }
}

/// Counts of the kinds seen for a key while scanning: picks the column's type.
#[derive(Default, Clone, Copy)]
pub(crate) struct KindTally {
    bools: usize,
    ints: usize,
    floats: usize,
    strs: usize,
    others: usize,
}

impl KindTally {
    #[inline]
    pub(crate) fn add(&mut self, v: Option<Scalar<'_>>) {
        match v {
            Some(Scalar::Bool(_)) => self.bools += 1,
            Some(Scalar::Int(_)) => self.ints += 1,
            Some(Scalar::Float(_)) => self.floats += 1,
            Some(Scalar::Str(_)) => self.strs += 1,
            Some(Scalar::Other) => self.others += 1,
            None => {}
        }
    }

    /// The majority scalar kind, `None` when most values are of kinds a column can't hold.
    fn kind(&self) -> Option<Kind> {
        let best = [
            (self.ints, Kind::Int),
            (self.floats, Kind::Float),
            (self.strs, Kind::Str),
            (self.bools, Kind::Bool),
        ]
        .into_iter()
        .max_by_key(|(n, _)| *n)?;
        if best.0 == 0 || self.others > best.0 {
            return None;
        }
        Some(best.1)
    }
}

/// Builds one column from a scan of the entity table.
pub(crate) struct ColumnBuilder {
    col: TypedColumn,
}

impl ColumnBuilder {
    /// A builder for a key whose values tallied as `tally`, over `len` ids; `None` when the key
    /// doesn't suit a column.
    pub(crate) fn new(tally: &KindTally, len: usize) -> Option<Self> {
        Some(Self {
            col: TypedColumn::new(tally.kind()?, len),
        })
    }

    #[inline]
    pub(crate) fn set(&mut self, id: usize, v: Option<Scalar<'_>>) {
        self.col.set(id, v);
    }

    fn finish(self) -> TypedColumn {
        self.col
    }
}

/// The columns of one entity table, plus a presence bit per id once any column exists.
#[derive(Default)]
struct Table {
    cols: FxHashMap<PropertyKeyId, TypedColumn>,
    /// Keys whose values don't suit a column.
    skip: FxHashSet<PropertyKeyId>,
    /// Lanes gathered per key so far, until its column is built.
    demand: FxHashMap<PropertyKeyId, usize>,
    /// Ids that name a live entity, over the prefix; maintained once `active`.
    present: Bits,
    active: bool,
}

impl Table {
    fn bytes(&self) -> usize {
        self.cols.values().map(TypedColumn::bytes).sum::<usize>() + self.present.bytes()
    }
}

/// Which table a column set call concerns.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Entity {
    Node,
    Edge,
}

/// A gather's plan for one key: where its lanes come from.
pub(crate) enum KeyPlan<'c> {
    /// From the column.
    Column(&'c TypedColumn),
    /// From the row store.
    Rows,
}

/// Read access to the columns of one table for the length of a gather.
pub(crate) struct Gather<'c> {
    table: std::sync::RwLockReadGuard<'c, Table>,
}

impl Gather<'_> {
    /// Are the presence bits maintained (so missing entities are counted from them)?
    pub(crate) fn active(&self) -> bool {
        self.table.active
    }

    /// Live entities among ids `[start, start + n)`; `None` when the run leaves the prefix.
    #[inline]
    pub(crate) fn present_count(&self, start: usize, n: usize) -> Option<usize> {
        let bits = &self.table.present;
        if start + n > bits.0.len() * 64 {
            return None;
        }
        let mut count = 0usize;
        let mut i = start;
        let end = start + n;
        while i < end {
            let w = i / 64;
            let lo = i % 64;
            let hi = (end - w * 64).min(64);
            let mask = if hi == 64 {
                u64::MAX << lo
            } else {
                ((1u64 << hi) - 1) & (u64::MAX << lo)
            };
            count += (bits.0[w] & mask).count_ones() as usize;
            i = (w + 1) * 64;
        }
        Some(count)
    }

    /// The nodes of `words` whose value in `col` satisfies the comparison, in id order.
    pub(crate) fn ids_where(
        &self,
        col: &TypedColumn,
        words: &[u64],
        cmp: ColumnCmp,
        bound: &ColumnBound,
    ) -> Option<Vec<NodeId>> {
        col.ids_where(words, cmp, bound)
    }

    /// Counts the nodes of `words` whose value in `col` satisfies the comparison.
    pub(crate) fn count_where(
        &self,
        col: &TypedColumn,
        words: &[u64],
        cmp: ColumnCmp,
        bound: &ColumnBound,
    ) -> Option<usize> {
        col.count_where(words, cmp, bound)
    }

    /// Does `id` name a live entity in the prefix? `None` past the prefix.
    #[inline]
    pub(crate) fn present(&self, id: usize) -> Option<bool> {
        if id < self.table.present.0.len() * 64 {
            Some(self.table.present.get(id))
        } else {
            None
        }
    }

    /// The plan for `key`.
    pub(crate) fn plan(&self, key: PropertyKeyId) -> KeyPlan<'_> {
        match self.table.cols.get(&key) {
            Some(c) => KeyPlan::Column(c),
            None => KeyPlan::Rows,
        }
    }

    /// Pushes the lanes of ids `[start, start + n)` from `col` at once; `row` reads a lane
    /// the column doesn't hold.
    #[inline]
    pub(crate) fn push_run(
        &self,
        col: &TypedColumn,
        start: usize,
        n: usize,
        sink: &mut dyn ColumnSink,
        row: impl FnMut(usize, &mut dyn ColumnSink),
    ) {
        if start + n <= col.len {
            col.push_run(start, n, sink, row);
        } else {
            let mut row = row;
            for i in start..start + n {
                col.push_lane(i, sink, |s| row(i, s));
            }
        }
    }

    /// Pushes `id`'s lane from `col`, reading the row through `row` where needed.
    #[inline]
    pub(crate) fn push_lane(
        &self,
        col: &TypedColumn,
        id: usize,
        sink: &mut dyn ColumnSink,
        row: impl FnOnce(&mut dyn ColumnSink),
    ) {
        col.push_lane(id, sink, row);
    }
}

/// The typed columns of a graph.
pub(crate) struct ColumnSet {
    nodes: RwLock<Table>,
    edges: RwLock<Table>,
    bytes: AtomicUsize,
}

impl Default for ColumnSet {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ColumnSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ColumnSet")
            .field("bytes", &self.bytes.load(Ordering::Relaxed))
            .finish()
    }
}

impl ColumnSet {
    /// No columns.
    pub(crate) fn new() -> Self {
        Self {
            nodes: RwLock::new(Table::default()),
            edges: RwLock::new(Table::default()),
            bytes: AtomicUsize::new(0),
        }
    }

    fn table(&self, which: Entity) -> &RwLock<Table> {
        match which {
            Entity::Node => &self.nodes,
            Entity::Edge => &self.edges,
        }
    }

    /// Bytes the columns hold.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    fn recount(&self) {
        let n = self.nodes.read().map(|t| t.bytes()).unwrap_or(0)
            + self.edges.read().map(|t| t.bytes()).unwrap_or(0);
        self.bytes.store(n, Ordering::Relaxed);
    }

    /// Read access for a gather.
    pub(crate) fn gather(&self, which: Entity) -> Gather<'_> {
        Gather {
            table: self.table(which).read().unwrap_or_else(|e| e.into_inner()),
        }
    }

    /// Notes a gather of `lanes` lanes over `keys`; the keys whose column is now due.
    pub(crate) fn note_demand(
        &self,
        which: Entity,
        keys: &[PropertyKeyId],
        lanes: usize,
    ) -> Vec<PropertyKeyId> {
        let mut due = Vec::new();
        {
            // the usual case takes a read lock only: every key already has its column (or none)
            let table = self.table(which).read().unwrap_or_else(|e| e.into_inner());
            if keys
                .iter()
                .all(|k| table.cols.contains_key(k) || table.skip.contains(k))
            {
                return due;
            }
        }
        let mut table = self.table(which).write().unwrap_or_else(|e| e.into_inner());
        for &k in keys {
            if table.cols.contains_key(&k) || table.skip.contains(&k) {
                continue;
            }
            let d = table.demand.entry(k).or_insert(0);
            *d += lanes;
            if *d >= BUILD_AFTER {
                due.push(k);
            }
        }
        due
    }

    /// Installs built columns (or skips) for `keys`, with the presence bits of the table's
    /// prefix when they weren't maintained yet.
    pub(crate) fn install(
        &self,
        which: Entity,
        built: Vec<(PropertyKeyId, Option<ColumnBuilder>)>,
        present: Option<Vec<u64>>,
    ) {
        {
            let mut table = self.table(which).write().unwrap_or_else(|e| e.into_inner());
            if let Some(bits) = present
                && !table.active
            {
                table.present = Bits(bits);
                table.active = true;
            }
            for (k, b) in built {
                table.demand.remove(&k);
                match b {
                    Some(b) => {
                        table.cols.entry(k).or_insert_with(|| b.finish());
                    }
                    None => {
                        table.skip.insert(k);
                    }
                }
            }
        }
        self.recount();
    }

    fn table_mut(&mut self, which: Entity) -> &mut Table {
        match which {
            Entity::Node => self.nodes.get_mut().unwrap_or_else(|e| e.into_inner()),
            Entity::Edge => self.edges.get_mut().unwrap_or_else(|e| e.into_inner()),
        }
    }

    /// The entity at `id` now holds `props` (created, replaced, restored, or changed in place).
    /// `in_prefix`: the id lies in the table's dense prefix, which the columns cover.
    pub(crate) fn refresh(
        &mut self,
        which: Entity,
        id: usize,
        props: &InternedPropertyMap,
        in_prefix: bool,
    ) {
        let t = self.table_mut(which);
        if !t.active || !in_prefix {
            return;
        }
        t.present.set(id, true);
        for (k, col) in t.cols.iter_mut() {
            col.set(id, props.get_scalar(*k));
            col.maybe_compact();
        }
    }

    /// Only `key` of the entity at `id` changed.
    pub(crate) fn refresh_key(
        &mut self,
        which: Entity,
        id: usize,
        key: PropertyKeyId,
        props: &InternedPropertyMap,
        in_prefix: bool,
    ) {
        let t = self.table_mut(which);
        if !t.active || !in_prefix {
            return;
        }
        if let Some(col) = t.cols.get_mut(&key) {
            col.set(id, props.get_scalar(key));
            col.maybe_compact();
        }
    }

    /// The entity at `id` is gone.
    pub(crate) fn removed(&mut self, which: Entity, id: usize) {
        let t = self.table_mut(which);
        if !t.active {
            return;
        }
        t.present.set(id, false);
        for col in t.cols.values_mut() {
            if id < col.len {
                col.set(id, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::property::PropertyValue;

    #[test]
    fn column_holds_typed_values_and_falls_back_for_others() {
        let mut tally = KindTally::default();
        tally.add(Some(Scalar::Int(1)));
        tally.add(Some(Scalar::Int(2)));
        tally.add(Some(Scalar::Str("x")));
        let mut b = ColumnBuilder::new(&tally, 4).unwrap();
        b.set(0, Some(Scalar::Int(7)));
        b.set(1, None);
        b.set(2, Some(Scalar::Str("s")));
        let col = b.finish();
        let mut out: Vec<Option<PropertyValue>> = Vec::new();
        for id in 0..5 {
            col.push_lane(id, &mut out, |s| {
                s.push_value(PropertyValue::String("row".into()))
            });
        }
        assert_eq!(
            out,
            vec![
                Some(PropertyValue::Int(7)),
                None,
                Some(PropertyValue::String("row".into())),
                None,
                Some(PropertyValue::String("row".into())),
            ]
        );
    }

    #[test]
    fn string_column_replaces_in_place_and_appends() {
        let mut tally = KindTally::default();
        tally.add(Some(Scalar::Str("a")));
        let mut b = ColumnBuilder::new(&tally, 2).unwrap();
        b.set(0, Some(Scalar::Str("hello")));
        b.set(1, Some(Scalar::Str("world")));
        let mut col = b.finish();
        col.set(0, Some(Scalar::Str("hi")));
        col.set(1, Some(Scalar::Str("a longer one")));
        let mut out: Vec<Option<PropertyValue>> = Vec::new();
        col.push_lane(0, &mut out, |_| unreachable!());
        col.push_lane(1, &mut out, |_| unreachable!());
        assert_eq!(
            out,
            vec![
                Some(PropertyValue::String("hi".into())),
                Some(PropertyValue::String("a longer one".into()))
            ]
        );
        if let ColumnData::Str(s) = &col.data {
            assert_eq!(s.dead, 3 + 5);
        } else {
            panic!("not a string column");
        }
    }

    #[test]
    fn mostly_other_keys_get_no_column() {
        let mut tally = KindTally::default();
        tally.add(Some(Scalar::Other));
        tally.add(Some(Scalar::Other));
        tally.add(Some(Scalar::Int(1)));
        assert!(ColumnBuilder::new(&tally, 3).is_none());
        assert!(ColumnBuilder::new(&KindTally::default(), 3).is_none());
    }
}
