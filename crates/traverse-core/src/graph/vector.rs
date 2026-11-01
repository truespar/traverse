//! HNSW approximate-nearest-neighbor index behind `CREATE VECTOR INDEX` /
//! `db.index.vector.*`.
//!
//! Same algorithm family as Neo4j (via Lucene): a multi-layer proximity graph
//! (Malkov & Yashunin, 2016), roughly logarithmic search over millions of
//! vectors. Defaults match Neo4j's build params (`M = 16`,
//! `ef_construction = 100`) so recall/latency are comparable.
//!
//! - Keyed by a dense internal `u32` id the caller maps to `NodeId`s.
//! - Deletes repair the graph in place (IP-DiskANN style, Xu et al. 2025): the
//!   deleted node's in-neighbors, found in its 2-hop neighborhood, are re-linked
//!   to its out-neighbors through the same diversity heuristic, so recall holds
//!   under churn without a rebuild. A deleted entry point is replaced at once.
//!   Freed slots are reused by later inserts (hnswlib's `replace_deleted`) after a
//!   cheap sweep drops any edge the local repair missed, so memory stays bounded.
//! - Level assignment uses a small seeded xorshift, so the same data always
//!   builds the same index (reproducible tests, stable plans). No `rand` dependency.

use rustc_hash::FxHashMap;

use super::types::{LabelId, NodeId, PropertyKeyId};

/// Deleted slots wait in `pending` until a sweep confirms no edge points at them; the sweep runs
/// once `pending` reaches this many slots or `1 / SWEEP_DIVISOR` of all slots, whichever is larger.
const SWEEP_MIN_PENDING: usize = 64;
const SWEEP_DIVISOR: usize = 20;

/// Named vector index: HNSW over one `LIST<FLOAT>` property of one label, plus
/// the HNSW id <-> `NodeId` mapping. Maintained as matching nodes are written.
#[derive(Clone, Debug)]
pub struct VectorIndex {
    /// User-visible index name (unique).
    pub name: String,
    /// Indexed node label.
    pub label_id: LabelId,
    /// Indexed vector property.
    pub property_key: PropertyKeyId,
    /// Vector dimensionality declared at CREATE (`vector.dimensions`).
    pub dim: usize,
    /// Similarity function (`vector.similarity_function`).
    pub sim: Similarity,
    /// HNSW build/search parameters.
    pub params: HnswParams,
    hnsw: Hnsw,
    /// HNSW id -> NodeId; stale for free slots, which search never returns.
    id_to_node: Vec<NodeId>,
    /// NodeId -> current HNSW id.
    node_to_id: FxHashMap<NodeId, u32>,
}

impl VectorIndex {
    fn new(
        name: String,
        label_id: LabelId,
        property_key: PropertyKeyId,
        dim: usize,
        sim: Similarity,
        params: HnswParams,
    ) -> Self {
        Self {
            name,
            label_id,
            property_key,
            dim,
            sim,
            params,
            hnsw: Hnsw::new(dim, sim, params),
            id_to_node: Vec::new(),
            node_to_id: FxHashMap::default(),
        }
    }

    /// Inserts or updates a node's vector. Wrong-dimension vectors are ignored (Neo4j skips them
    /// silently).
    pub fn upsert(&mut self, node: NodeId, vec: &[f32]) {
        if vec.len() != self.dim {
            return;
        }
        // update = delete the old entry (repairs its neighborhood), insert fresh
        if let Some(&old) = self.node_to_id.get(&node) {
            self.hnsw.delete(old);
        }
        let id = self.hnsw.insert(vec);
        if id as usize >= self.id_to_node.len() {
            self.id_to_node.resize(id as usize + 1, NodeId(u64::MAX));
        }
        self.id_to_node[id as usize] = node;
        self.node_to_id.insert(node, id);
    }

    /// Removes a node's vector (node deleted or lost the property).
    pub fn remove(&mut self, node: NodeId) {
        if let Some(id) = self.node_to_id.remove(&node) {
            self.hnsw.delete(id);
        }
    }

    /// k-approximate nearest neighbors as `(NodeId, score)`, best first.
    pub fn query(&self, query: &[f32], k: usize, ef: Option<usize>) -> Vec<(NodeId, f64)> {
        self.hnsw
            .search(query, k, ef)
            .into_iter()
            .map(|(id, score)| (self.id_to_node[id as usize], score))
            .collect()
    }

    /// Score of `vec` for `query` under this index's similarity, as [`Self::query`] reports it;
    /// `None` on a dimension mismatch.
    pub fn exact_score(&self, query: &[f32], vec: &[f32]) -> Option<f64> {
        if query.len() != self.dim || vec.len() != self.dim {
            return None;
        }
        let dist = match self.sim {
            Similarity::Cosine => {
                let denom = dot_product(query, query).sqrt() * dot_product(vec, vec).sqrt();
                if denom == 0.0 {
                    1.0
                } else {
                    1.0 - dot_product(query, vec) / denom
                }
            }
            Similarity::Euclidean => l2_squared(query, vec).sqrt(),
        };
        Some(distance_to_score(self.sim, dist))
    }

    /// Number of live vectors.
    pub fn len(&self) -> usize {
        self.hnsw.len()
    }

    /// True if no vectors are indexed.
    pub fn is_empty(&self) -> bool {
        self.hnsw.is_empty()
    }
}

/// Vector indexes of a graph, by name. Separate from the b-tree
/// [`super::index::IndexRegistry`]: different storage (HNSW vs sorted map) and
/// maintenance.
#[derive(Default, Clone, Debug)]
pub struct VectorIndexRegistry {
    by_name: FxHashMap<String, VectorIndex>,
    /// Bumped on create/drop so the plan cache can invalidate.
    generation: u64,
}

impl VectorIndexRegistry {
    /// Registers a new empty vector index. Errors if the name is taken.
    pub fn create(
        &mut self,
        name: String,
        label_id: LabelId,
        property_key: PropertyKeyId,
        dim: usize,
        sim: Similarity,
        params: HnswParams,
    ) -> Result<(), String> {
        if self.by_name.contains_key(&name) {
            return Err(format!("index '{name}' already exists"));
        }
        self.by_name.insert(
            name.clone(),
            VectorIndex::new(name, label_id, property_key, dim, sim, params),
        );
        self.generation += 1;
        Ok(())
    }

    /// Drop a vector index by name.
    pub fn drop(&mut self, name: &str) -> Result<(), String> {
        if self.by_name.remove(name).is_none() {
            return Err(format!("index '{name}' not found"));
        }
        self.generation += 1;
        Ok(())
    }

    /// The vector index called `name`, if any.
    pub fn get(&self, name: &str) -> Option<&VectorIndex> {
        self.by_name.get(name)
    }

    /// Mutable access to the vector index called `name`, if any.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut VectorIndex> {
        self.by_name.get_mut(name)
    }

    /// Indexes covering `label_id` (write-time maintenance).
    pub fn indexes_for_label(&self, label_id: LabelId) -> Vec<String> {
        self.by_name
            .values()
            .filter(|v| v.label_id == label_id)
            .map(|v| v.name.clone())
            .collect()
    }

    /// All indexes (SHOW INDEXES, rebuild on load).
    pub fn iter(&self) -> impl Iterator<Item = &VectorIndex> {
        self.by_name.values()
    }

    /// True when no vector index exists.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Vector index similarity function, as Neo4j's `vector.similarity_function`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Similarity {
    /// Cosine, normalised like Neo4j to `(1 + cos) / 2` in `[0, 1]` (1 = same direction).
    Cosine,
    /// Euclidean: Neo4j reports `1 / (1 + d^2)` in `(0, 1]`.
    Euclidean,
}

impl Similarity {
    /// Parses Neo4j's surface strings (`'cosine'` / `'euclidean'`, any case). `l2`
    /// and `dot`/`dotproduct` map to the closest supported metric for driver compatibility.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "cosine" | "dot" | "dotproduct" => Some(Similarity::Cosine),
            "euclidean" | "l2" => Some(Similarity::Euclidean),
            _ => None,
        }
    }

    /// Surface spelling for SHOW INDEXES / diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Similarity::Cosine => "cosine",
            Similarity::Euclidean => "euclidean",
        }
    }
}

/// Query prepared once per insert/search, in exactly one form: f32
/// (`vec`/`norm`) unquantized, or int8 (`codes`/`code_norm`) for quantized cosine.
struct Probe {
    vec: Vec<f32>,
    norm: f32,
    codes: Vec<i8>,
    code_norm: f32,
}

/// Unit-normalizes `v` and quantizes each component to int8 in `[-127, 127]`
/// (`round(unit_i * 127)`). Also returns the code vector's L2 norm so cosine is
/// exact on codes: `dot_i8(a,b) / (code_norm_a * code_norm_b)`.
fn quantize_unit(v: &[f32]) -> (Vec<i8>, f32) {
    let norm = dot_product(v, v).sqrt();
    let inv = if norm > 0.0 { 1.0 / norm } else { 0.0 };
    let mut codes = Vec::with_capacity(v.len());
    let mut sumsq: i64 = 0;
    for &x in v {
        let q = (x * inv * 127.0).round().clamp(-127.0, 127.0) as i32;
        sumsq += (q * q) as i64;
        codes.push(q as i8);
    }
    (codes, (sumsq as f64).sqrt() as f32)
}

/// Signed int8 dot product; AVX2 when available.
#[inline]
fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2() {
            // SAFETY: runtime avx2 check above.
            return unsafe { dot_i8_avx2(a, b) };
        }
    }
    dot_i8_scalar(a, b)
}

/// int8 dot with 4 int32 accumulators (auto-vectorizes to SSE2).
fn dot_i8_scalar(a: &[i8], b: &[i8]) -> i32 {
    let n = a.len().min(b.len());
    let mut acc = [0i32; 4];
    let chunks = n / 4;
    for c in 0..chunks {
        let base = c * 4;
        acc[0] += a[base] as i32 * b[base] as i32;
        acc[1] += a[base + 1] as i32 * b[base + 1] as i32;
        acc[2] += a[base + 2] as i32 * b[base + 2] as i32;
        acc[3] += a[base + 3] as i32 * b[base + 3] as i32;
    }
    let mut sum = acc[0] + acc[1] + acc[2] + acc[3];
    for i in chunks * 4..n {
        sum += a[i] as i32 * b[i] as i32;
    }
    sum
}

/// AVX2 int8 dot: sign-extend 16 int8 -> int16, `madd` adjacent pairs to int32,
/// accumulate. 16 dims per iteration.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_i8_avx2(a: &[i8], b: &[i8]) -> i32 {
    // SAFETY: only called after the runtime feature check; loops stay within slice bounds.
    unsafe {
        use std::arch::x86_64::*;
        let n = a.len().min(b.len());
        let chunks = n / 16;
        let mut acc = _mm256_setzero_si256();
        let pa = a.as_ptr();
        let pb = b.as_ptr();
        for c in 0..chunks {
            let i = c * 16;
            let va = _mm256_cvtepi8_epi16(_mm_loadu_si128(pa.add(i) as *const __m128i));
            let vb = _mm256_cvtepi8_epi16(_mm_loadu_si128(pb.add(i) as *const __m128i));
            acc = _mm256_add_epi32(acc, _mm256_madd_epi16(va, vb));
        }
        // horizontal sum of 8 int32 lanes
        let mut tmp = [0i32; 8];
        _mm256_storeu_si256(tmp.as_mut_ptr() as *mut __m256i, acc);
        let mut sum: i32 = tmp.iter().sum();
        for i in chunks * 16..n {
            sum += a[i] as i32 * b[i] as i32;
        }
        sum
    }
}

// Distance kernels dominate query cost at high dimensionality (4096-d = 4096
// mul-adds per comparison, thousands of comparisons per query). Release targets
// baseline x86_64 (SSE2), so an AVX2+FMA path (8-wide) is picked at runtime, as
// Lucene does via the Panama Vector API; fallback is an 8-accumulator scalar loop
// LLVM auto-vectorizes to SSE2. The stdlib caches the feature check, so each call
// pays one predictable branch.

/// AVX2 probe, cached once (relaxed; the value never changes).
#[cfg(target_arch = "x86_64")]
fn has_avx2() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static CACHE: AtomicU8 = AtomicU8::new(2); // 2 = unknown
    match CACHE.load(Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => {
            let v = is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma");
            CACHE.store(v as u8, Ordering::Relaxed);
            v
        }
    }
}

/// Dot product; AVX2 when available.
#[inline]
fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2() {
            // SAFETY: runtime avx2+fma check above.
            return unsafe { dot_product_avx2(a, b) };
        }
    }
    dot_product_scalar(a, b)
}

/// Squared L2 distance; AVX2 when available.
#[inline]
fn l2_squared(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2() {
            // SAFETY: runtime avx2+fma check above.
            return unsafe { l2_squared_avx2(a, b) };
        }
    }
    l2_squared_scalar(a, b)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn dot_product_avx2(a: &[f32], b: &[f32]) -> f32 {
    // SAFETY: only called after the runtime feature check; loops stay within slice bounds.
    unsafe {
        use std::arch::x86_64::*;
        let n = a.len().min(b.len());
        let chunks = n / 8;
        // two accumulators hide FMA latency
        let mut acc0 = _mm256_setzero_ps();
        let mut acc1 = _mm256_setzero_ps();
        let pa = a.as_ptr();
        let pb = b.as_ptr();
        let pairs = chunks / 2;
        for c in 0..pairs {
            let i = c * 16;
            acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)), acc0);
            acc1 = _mm256_fmadd_ps(
                _mm256_loadu_ps(pa.add(i + 8)),
                _mm256_loadu_ps(pb.add(i + 8)),
                acc1,
            );
        }
        if chunks % 2 == 1 {
            let i = pairs * 16;
            acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)), acc0);
        }
        let mut sum = hsum256(_mm256_add_ps(acc0, acc1));
        for i in chunks * 8..n {
            sum += a[i] * b[i];
        }
        sum
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn l2_squared_avx2(a: &[f32], b: &[f32]) -> f32 {
    // SAFETY: only called after the runtime feature check; loops stay within slice bounds.
    unsafe {
        use std::arch::x86_64::*;
        let n = a.len().min(b.len());
        let chunks = n / 8;
        let mut acc0 = _mm256_setzero_ps();
        let mut acc1 = _mm256_setzero_ps();
        let pa = a.as_ptr();
        let pb = b.as_ptr();
        let pairs = chunks / 2;
        for c in 0..pairs {
            let i = c * 16;
            let d0 = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)));
            let d1 = _mm256_sub_ps(
                _mm256_loadu_ps(pa.add(i + 8)),
                _mm256_loadu_ps(pb.add(i + 8)),
            );
            acc0 = _mm256_fmadd_ps(d0, d0, acc0);
            acc1 = _mm256_fmadd_ps(d1, d1, acc1);
        }
        if chunks % 2 == 1 {
            let i = pairs * 16;
            let d = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)));
            acc0 = _mm256_fmadd_ps(d, d, acc0);
        }
        let mut sum = hsum256(_mm256_add_ps(acc0, acc1));
        for i in chunks * 8..n {
            let d = a[i] - b[i];
            sum += d * d;
        }
        sum
    }
}

/// Horizontal sum of an `__m256` (8 x f32).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn hsum256(v: std::arch::x86_64::__m256) -> f32 {
    use std::arch::x86_64::*;
    let lo = _mm256_castps256_ps128(v);
    let hi = _mm256_extractf128_ps(v, 1);
    let s = _mm_add_ps(lo, hi);
    let s = _mm_hadd_ps(s, s);
    let s = _mm_hadd_ps(s, s);
    _mm_cvtss_f32(s)
}

/// Dot product with 8 accumulator lanes; LLVM auto-vectorizes to SSE2 without AVX2.
fn dot_product_scalar(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut acc = [0.0f32; 8];
    let chunks = n / 8;
    for c in 0..chunks {
        let base = c * 8;
        acc[0] += a[base] * b[base];
        acc[1] += a[base + 1] * b[base + 1];
        acc[2] += a[base + 2] * b[base + 2];
        acc[3] += a[base + 3] * b[base + 3];
        acc[4] += a[base + 4] * b[base + 4];
        acc[5] += a[base + 5] * b[base + 5];
        acc[6] += a[base + 6] * b[base + 6];
        acc[7] += a[base + 7] * b[base + 7];
    }
    let mut sum = acc[0] + acc[1] + acc[2] + acc[3] + acc[4] + acc[5] + acc[6] + acc[7];
    for i in chunks * 8..n {
        sum += a[i] * b[i];
    }
    sum
}

/// Squared L2, same 8-lane shape as [`dot_product_scalar`].
fn l2_squared_scalar(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut acc = [0.0f32; 8];
    let chunks = n / 8;
    for c in 0..chunks {
        let base = c * 8;
        let d0 = a[base] - b[base];
        let d1 = a[base + 1] - b[base + 1];
        let d2 = a[base + 2] - b[base + 2];
        let d3 = a[base + 3] - b[base + 3];
        let d4 = a[base + 4] - b[base + 4];
        let d5 = a[base + 5] - b[base + 5];
        let d6 = a[base + 6] - b[base + 6];
        let d7 = a[base + 7] - b[base + 7];
        acc[0] += d0 * d0;
        acc[1] += d1 * d1;
        acc[2] += d2 * d2;
        acc[3] += d3 * d3;
        acc[4] += d4 * d4;
        acc[5] += d5 * d5;
        acc[6] += d6 * d6;
        acc[7] += d7 * d7;
    }
    let mut sum = acc[0] + acc[1] + acc[2] + acc[3] + acc[4] + acc[5] + acc[6] + acc[7];
    for i in chunks * 8..n {
        let d = a[i] - b[i];
        sum += d * d;
    }
    sum
}

/// Raw distance (smaller = closer) -> Neo4j score (larger = closer). HNSW works in
/// distances; scores only appear at the API boundary.
fn distance_to_score(sim: Similarity, dist: f32) -> f64 {
    match sim {
        // dist is `1 - cos` (see `distance`), so score = (1 + cos) / 2 = (2 - dist) / 2
        Similarity::Cosine => ((2.0 - dist as f64) / 2.0).clamp(0.0, 1.0),
        // dist is L2; Neo4j reports 1 / (1 + d^2)
        Similarity::Euclidean => 1.0 / (1.0 + (dist as f64) * (dist as f64)),
    }
}

/// Search frontier candidate `(distance, node)`, ordered by distance so a max-heap keeps the worst
/// on top.
#[derive(Clone, Copy, PartialEq)]
struct Candidate {
    dist: f32,
    id: u32,
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // NaN-safe: NaN sorts as the worst
        self.dist
            .partial_cmp(&other.dist)
            .unwrap_or(std::cmp::Ordering::Greater)
    }
}

/// Build/search params. Defaults match Neo4j (Lucene) for comparable behaviour.
#[derive(Debug, Clone, Copy)]
pub struct HnswParams {
    /// Max neighbors per node on layers > 0 (`vector.hnsw.m`, Neo4j default 16). Layer 0 uses
    /// `2*m`.
    pub m: usize,
    /// Candidate list width while building (`vector.hnsw.ef_construction`, Neo4j default 100).
    pub ef_construction: usize,
    /// Default candidate list width at query time; callers can override per query. Neo4j uses
    /// `max(ef, k)`.
    pub ef_search: usize,
    /// Seed for the level-assignment RNG (reproducible builds).
    pub seed: u64,
    /// int8 scalar quantization (cosine only): one byte per dim instead of four.
    /// Traversal at high dimensionality is bound by loading candidate vectors, so it
    /// moves 4x less data (Neo4j/Lucene quantize for the same reason). Ignored for euclidean.
    pub quantize: bool,
}

impl Default for HnswParams {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 100,
            ef_search: 100,
            seed: 0x9E3779B97F4A7C15,
            quantize: true,
        }
    }
}

/// One HNSW index over `dim`-dimensional vectors.
#[derive(Clone, Debug)]
pub struct Hnsw {
    dim: usize,
    sim: Similarity,
    params: HnswParams,
    /// Vectors stored int8-quantized (cosine + `quantize`): `codes`/`code_norms`
    /// filled, `vectors` keeps f32 for rerank. Otherwise only `vectors`.
    quantized: bool,
    /// Flat f32 storage, `id * dim .. (id+1) * dim`.
    vectors: Vec<f32>,
    /// Flat int8 codes, same layout (quantized cosine); unit-normalized then scaled to `[-127,
    /// 127]`.
    codes: Vec<i8>,
    /// L2 norm of each code vector (quantized cosine) for exact `cos = dot_i8(a,b) / (code_norm_a *
    /// code_norm_b)`.
    code_norms: Vec<f32>,
    /// L2 norm per vector (cosine); empty for euclidean.
    norms: Vec<f32>,
    /// `links[layer][id]` = neighbors of `id` on `layer`. Layer 0 is dense, higher layers sparse.
    links: Vec<Vec<Vec<u32>>>,
    /// Per-node top layer.
    node_layer: Vec<u8>,
    /// Slot not live (deleted, pending a sweep, or free for reuse): never returned or linked to.
    deleted: Vec<bool>,
    /// Live vectors.
    live: usize,
    /// Deleted slots whose in-edges may not all be gone yet (local repair can miss some).
    pending: Vec<u32>,
    /// Swept slots, reused by the next inserts.
    free: Vec<u32>,
    /// Top-layer entry point (`None` until the first insert).
    entry: Option<u32>,
    /// RNG state for level assignment.
    rng: u64,
}

impl Hnsw {
    /// Create an empty index over `dim`-dimensional vectors.
    pub fn new(dim: usize, sim: Similarity, params: HnswParams) -> Self {
        Self {
            dim,
            sim,
            params,
            quantized: params.quantize && sim == Similarity::Cosine,
            vectors: Vec::new(),
            codes: Vec::new(),
            code_norms: Vec::new(),
            norms: Vec::new(),
            links: vec![Vec::new()], // layer 0 always present
            node_layer: Vec::new(),
            deleted: Vec::new(),
            live: 0,
            pending: Vec::new(),
            free: Vec::new(),
            entry: None,
            rng: params.seed | 1, // non-zero
        }
    }

    /// Number of live vectors.
    pub fn len(&self) -> usize {
        self.live
    }

    /// True if the index holds no live vectors.
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// xorshift64*: cheap, decent, deterministic.
    fn next_rand(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        // map to (0, 1)
        (x.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Level from HNSW's exponential distribution: `floor(-ln(U) * mL)`, `mL = 1 / ln(M)`.
    fn random_level(&mut self) -> usize {
        let m_l = 1.0 / (self.params.m as f64).max(2.0).ln();
        let u = self.next_rand().max(1e-12);
        (-u.ln() * m_l).floor() as usize
    }

    /// Query prepared once per insert/search in the index's representation, so the hot `distance`
    /// loop has no per-call setup.
    fn make_probe(&self, q: &[f32]) -> Probe {
        if self.quantized {
            // both reps: int8 for traversal, f32 for the final rerank
            let (codes, code_norm) = quantize_unit(q);
            Probe {
                vec: q.to_vec(),
                norm: Self::norm_of(q),
                codes,
                code_norm,
            }
        } else {
            let norm = if self.sim == Similarity::Cosine {
                Self::norm_of(q)
            } else {
                0.0
            };
            Probe {
                vec: q.to_vec(),
                norm,
                codes: Vec::new(),
                code_norm: 0.0,
            }
        }
    }

    /// Exact f32 distance between a probe and stored `id`, for reranking final candidates in
    /// quantized mode (traversal runs on int8).
    #[inline]
    fn distance_exact(&self, p: &Probe, id: u32) -> f32 {
        let base = id as usize * self.dim;
        let v = &self.vectors[base..base + self.dim];
        match self.sim {
            Similarity::Cosine => {
                let denom = p.norm * self.norms[id as usize];
                if denom == 0.0 {
                    1.0
                } else {
                    1.0 - dot_product(&p.vec, v) / denom
                }
            }
            Similarity::Euclidean => l2_squared(&p.vec, v).sqrt(),
        }
    }

    /// Distance from a [`Probe`] to stored `id`: cosine gives `1 - cos` (smaller = closer),
    /// euclidean the L2 distance.
    #[inline]
    fn distance(&self, p: &Probe, id: u32) -> f32 {
        let base = id as usize * self.dim;
        if self.quantized {
            let v = &self.codes[base..base + self.dim];
            let denom = p.code_norm * self.code_norms[id as usize];
            if denom == 0.0 {
                1.0
            } else {
                1.0 - dot_i8(&p.codes, v) as f32 / denom
            }
        } else {
            let v = &self.vectors[base..base + self.dim];
            match self.sim {
                Similarity::Cosine => {
                    let dot = dot_product(&p.vec, v);
                    let denom = p.norm * self.norms[id as usize];
                    if denom == 0.0 { 1.0 } else { 1.0 - dot / denom }
                }
                Similarity::Euclidean => l2_squared(&p.vec, v).sqrt(),
            }
        }
    }

    #[inline]
    fn norm_of(v: &[f32]) -> f32 {
        dot_product(v, v).sqrt()
    }

    /// Inserts a vector; returns its internal id, a reused free slot when there is one. Panics
    /// unless `vec.len() == dim` (API layer validates).
    pub fn insert(&mut self, vec: &[f32]) -> u32 {
        assert_eq!(vec.len(), self.dim, "vector dimension mismatch");
        let level = self.random_level();
        let norm = if self.sim == Similarity::Cosine {
            Self::norm_of(vec)
        } else {
            0.0
        };
        // quantized: keep f32 too; traversal runs on int8 (4x less bandwidth), the top candidates
        // are re-scored on f32 to remove quantization ranking error
        let quant = self.quantized.then(|| quantize_unit(vec));
        let id = if let Some(id) = self.free.pop() {
            let base = id as usize * self.dim;
            self.vectors[base..base + self.dim].copy_from_slice(vec);
            self.norms[id as usize] = norm;
            if let Some((codes, code_norm)) = &quant {
                self.codes[base..base + self.dim].copy_from_slice(codes);
                self.code_norms[id as usize] = *code_norm;
            }
            self.deleted[id as usize] = false;
            self.node_layer[id as usize] = level as u8;
            id
        } else {
            let id = self.node_layer.len() as u32;
            self.vectors.extend_from_slice(vec);
            self.norms.push(norm);
            if let Some((codes, code_norm)) = &quant {
                self.codes.extend_from_slice(codes);
                self.code_norms.push(*code_norm);
            }
            self.deleted.push(false);
            self.node_layer.push(level as u8);
            id
        };
        self.live += 1;

        while self.links.len() <= level {
            self.links.push(Vec::new());
        }
        for l in 0..=level {
            if self.links[l].len() <= id as usize {
                self.links[l].resize(id as usize + 1, Vec::new());
            }
        }

        let probe = self.make_probe(vec);

        // first node becomes the entry point
        let Some(mut ep) = self.entry else {
            self.entry = Some(id);
            return id;
        };

        // phase 1: greedy descent from the top layer to `level+1`, one best entry point per layer
        let top = self.node_layer[ep as usize] as usize;
        let mut cur_dist = self.distance(&probe, ep);
        for l in (level + 1..=top).rev() {
            let mut changed = true;
            while changed {
                changed = false;
                if let Some(neighbors) = self.links.get(l).and_then(|layer| layer.get(ep as usize))
                {
                    for &n in neighbors {
                        if self.deleted[n as usize] {
                            continue;
                        }
                        let d = self.distance(&probe, n);
                        if d < cur_dist {
                            cur_dist = d;
                            ep = n;
                            changed = true;
                        }
                    }
                }
            }
        }

        // phase 2: on each layer from min(level, top) to 0, full ef_construction search, connect to
        // the M best
        for l in (0..=level.min(top)).rev() {
            let mut candidates = self.search_layer(&probe, &[ep], self.params.ef_construction, l);
            // closest becomes the entry point for the next layer down
            if let Some(best) = candidates.iter().min_by(|a, b| a.cmp(b)) {
                ep = best.id;
            }
            let m = if l == 0 {
                self.params.m * 2
            } else {
                self.params.m
            };
            self.connect(id, &mut candidates, m, l);
        }

        // promote the entry point if this node reaches higher
        if level > self.node_layer[self.entry.unwrap() as usize] as usize {
            self.entry = Some(id);
        }
        id
    }

    /// Distance between two stored vectors (neighbor diversification, reverse-link pruning). No
    /// query prep.
    #[inline]
    fn dist_ids(&self, a: u32, b: u32) -> f32 {
        let ba = a as usize * self.dim;
        let bb = b as usize * self.dim;
        if self.quantized {
            let ca = &self.codes[ba..ba + self.dim];
            let cb = &self.codes[bb..bb + self.dim];
            let denom = self.code_norms[a as usize] * self.code_norms[b as usize];
            if denom == 0.0 {
                1.0
            } else {
                1.0 - dot_i8(ca, cb) as f32 / denom
            }
        } else {
            let va = &self.vectors[ba..ba + self.dim];
            let vb = &self.vectors[bb..bb + self.dim];
            match self.sim {
                Similarity::Cosine => {
                    let denom = self.norms[a as usize] * self.norms[b as usize];
                    if denom == 0.0 {
                        1.0
                    } else {
                        1.0 - dot_product(va, vb) / denom
                    }
                }
                Similarity::Euclidean => l2_squared(va, vb).sqrt(),
            }
        }
    }

    /// HNSW neighbor-selection heuristic (Malkov & Yashunin, Algorithm 4).
    ///
    /// Taking the `m` nearest connects each point only to its own cluster and
    /// fragments the graph (recall collapses on clustered, real embedding data).
    /// Instead keep a candidate only if it's closer to `base` than to every neighbor
    /// already chosen, so edges point in diverse directions and search can hop
    /// between clusters. `candidates` must be sorted nearest-first (`dist` = base->candidate).
    fn select_neighbors_heuristic(
        &self,
        base: u32,
        candidates: &[Candidate],
        m: usize,
    ) -> Vec<u32> {
        let mut chosen: Vec<u32> = Vec::with_capacity(m);
        for c in candidates {
            if chosen.len() >= m {
                break;
            }
            if c.id == base || self.deleted[c.id as usize] {
                continue;
            }
            let diverse = chosen.iter().all(|&r| self.dist_ids(c.id, r) >= c.dist);
            if diverse {
                chosen.push(c.id);
            }
        }
        // heuristic pruned below m: top up with next-nearest to keep degree high
        // (`keepPrunedConnections`)
        if chosen.len() < m {
            for c in candidates {
                if chosen.len() >= m {
                    break;
                }
                if c.id != base && !self.deleted[c.id as usize] && !chosen.contains(&c.id) {
                    chosen.push(c.id);
                }
            }
        }
        chosen
    }

    /// Connects `id` to a diverse subset of the candidates on `layer` and adds reverse
    /// links, re-running the heuristic to keep each neighbor's list in budget and navigable.
    fn connect(&mut self, id: u32, candidates: &mut [Candidate], m: usize, layer: usize) {
        candidates.sort();
        let chosen = self.select_neighbors_heuristic(id, candidates, m);

        if self.links[layer].len() <= id as usize {
            self.links[layer].resize(id as usize + 1, Vec::new());
        }
        self.links[layer][id as usize] = chosen.clone();

        for &nbr in &chosen {
            if self.links[layer].len() <= nbr as usize {
                self.links[layer].resize(nbr as usize + 1, Vec::new());
            }
            self.links[layer][nbr as usize].push(id);
            // re-prune the neighbor's list with the same heuristic
            if self.links[layer][nbr as usize].len() > m {
                let mut scored: Vec<Candidate> = self.links[layer][nbr as usize]
                    .iter()
                    .map(|&other| Candidate {
                        dist: self.dist_ids(nbr, other),
                        id: other,
                    })
                    .collect();
                scored.sort();
                self.links[layer][nbr as usize] = self.select_neighbors_heuristic(nbr, &scored, m);
            }
        }
    }

    /// Greedy best-first search on one layer from `entry_points`; up to `ef` closest.
    fn search_layer(
        &self,
        probe: &Probe,
        entry_points: &[u32],
        ef: usize,
        layer: usize,
    ) -> Vec<Candidate> {
        use std::collections::BinaryHeap;
        let mut visited: FxHashMap<u32, ()> = FxHashMap::default();
        // frontier: min-heap by distance (Reverse)
        let mut frontier: BinaryHeap<std::cmp::Reverse<Candidate>> = BinaryHeap::new();
        // results: max-heap by distance, worst on top
        let mut result: BinaryHeap<Candidate> = BinaryHeap::new();

        for &ep in entry_points {
            if self.deleted.get(ep as usize).copied().unwrap_or(true) {
                continue;
            }
            let d = self.distance(probe, ep);
            let c = Candidate { dist: d, id: ep };
            frontier.push(std::cmp::Reverse(c));
            result.push(c);
            visited.insert(ep, ());
        }

        while let Some(std::cmp::Reverse(cur)) = frontier.pop() {
            // stop when the closest unexpanded candidate is worse than our worst and we have ef
            // results
            if let Some(worst) = result.peek()
                && cur.dist > worst.dist
                && result.len() >= ef
            {
                break;
            }
            let neighbors = self
                .links
                .get(layer)
                .and_then(|l| l.get(cur.id as usize))
                .cloned()
                .unwrap_or_default();
            for n in neighbors {
                if visited.insert(n, ()).is_some() {
                    continue;
                }
                if self.deleted[n as usize] {
                    continue;
                }
                let d = self.distance(probe, n);
                let worst_dist = result.peek().map(|c| c.dist).unwrap_or(f32::INFINITY);
                if d < worst_dist || result.len() < ef {
                    let c = Candidate { dist: d, id: n };
                    frontier.push(std::cmp::Reverse(c));
                    result.push(c);
                    if result.len() > ef {
                        result.pop();
                    }
                }
            }
        }
        result.into_vec()
    }

    /// k approximate nearest neighbors of `query` as `(id, score)`, highest score
    /// first (Neo4j convention). `ef` widens the beam; `None` = `max(ef_search, k)`.
    pub fn search(&self, query: &[f32], k: usize, ef: Option<usize>) -> Vec<(u32, f64)> {
        if query.len() != self.dim || self.entry.is_none() || k == 0 {
            return Vec::new();
        }
        let probe = self.make_probe(query);
        let ef = ef.unwrap_or(self.params.ef_search).max(k);

        // greedy descent through the upper layers to a good entry point
        let mut ep = self.entry.unwrap();
        let top = self.node_layer[ep as usize] as usize;
        let mut cur_dist = self.distance(&probe, ep);
        for l in (1..=top).rev() {
            let mut changed = true;
            while changed {
                changed = false;
                if let Some(neighbors) = self.links.get(l).and_then(|layer| layer.get(ep as usize))
                {
                    for &n in neighbors {
                        if self.deleted[n as usize] {
                            continue;
                        }
                        let d = self.distance(&probe, n);
                        if d < cur_dist {
                            cur_dist = d;
                            ep = n;
                            changed = true;
                        }
                    }
                }
            }
        }

        // full search on layer 0 (int8 distances when quantized)
        let mut found = self.search_layer(&probe, &[ep], ef, 0);
        found.retain(|c| !self.deleted[c.id as usize]);

        if self.quantized {
            // rerank the ef candidates on exact f32 to undo int8 ranking error: float-level recall,
            // while the expensive traversal stayed on 4x smaller int8 data
            for c in found.iter_mut() {
                c.dist = self.distance_exact(&probe, c.id);
            }
        }
        found.sort();
        found
            .into_iter()
            .take(k)
            .map(|c| (c.id, distance_to_score(self.sim, c.dist)))
            .collect()
    }

    /// Exhaustive top-k (ground truth): measures recall in tests, fallback for tiny indexes. Skips
    /// deleted slots.
    pub fn search_exact(&self, query: &[f32], k: usize) -> Vec<(u32, f64)> {
        if query.len() != self.dim {
            return Vec::new();
        }
        let probe = self.make_probe(query);
        let mut all: Vec<Candidate> = (0..self.node_layer.len() as u32)
            .filter(|&id| !self.deleted[id as usize])
            .map(|id| Candidate {
                dist: self.distance(&probe, id),
                id,
            })
            .collect();
        all.sort();
        all.into_iter()
            .take(k)
            .map(|c| (c.id, distance_to_score(self.sim, c.dist)))
            .collect()
    }

    /// Deletes `id` and repairs the graph around it, so search quality doesn't decay under churn.
    ///
    /// Per layer: the in-neighbors of `id` (approximated by its 2-hop neighborhood, which HNSW's
    /// mostly bidirectional links make near-complete) each get `id`'s out-neighbors as extra
    /// candidates and re-run the neighbor heuristic (IP-DiskANN's in-place delete). The slot is
    /// reused once a sweep has removed any in-edge the repair missed.
    pub fn delete(&mut self, id: u32) {
        let idx = id as usize;
        if self.deleted.get(idx).copied().unwrap_or(true) {
            return;
        }
        self.deleted[idx] = true;
        self.live -= 1;
        let level = self.node_layer[idx] as usize;
        for l in 0..=level.min(self.links.len() - 1) {
            self.repair_in_neighbors(id, l);
        }
        if self.entry == Some(id) {
            self.entry = self.replacement_entry(id, level);
        }
        for layer in self.links.iter_mut().take(level + 1) {
            if let Some(list) = layer.get_mut(idx) {
                *list = Vec::new();
            }
        }
        self.pending.push(id);
        if self.pending.len() >= SWEEP_MIN_PENDING.max(self.node_layer.len() / SWEEP_DIVISOR) {
            self.sweep();
        }
    }

    /// Re-links every in-neighbor of the deleted `p` on `layer` past it.
    fn repair_in_neighbors(&mut self, p: u32, layer: usize) {
        let out: Vec<u32> = match self.links[layer].get(p as usize) {
            Some(list) if !list.is_empty() => list.clone(),
            _ => return,
        };
        let points_at_p = |z: u32| {
            z != p
                && !self.deleted[z as usize]
                && self.links[layer]
                    .get(z as usize)
                    .is_some_and(|list| list.contains(&p))
        };
        let mut seen: rustc_hash::FxHashSet<u32> = rustc_hash::FxHashSet::default();
        let mut in_nbrs = Vec::new();
        for &y in &out {
            let two_hop = self.links[layer].get(y as usize).map_or(&[][..], |l| l);
            for z in std::iter::once(y).chain(two_hop.iter().copied()) {
                if seen.insert(z) && points_at_p(z) {
                    in_nbrs.push(z);
                }
            }
        }
        let m = if layer == 0 {
            self.params.m * 2
        } else {
            self.params.m
        };
        for z in in_nbrs {
            let mut ids: Vec<u32> = self.links[layer][z as usize]
                .iter()
                .chain(out.iter())
                .copied()
                .filter(|&c| c != p && c != z && !self.deleted[c as usize])
                .collect();
            ids.sort_unstable();
            ids.dedup();
            let mut candidates: Vec<Candidate> = ids
                .into_iter()
                .map(|c| Candidate {
                    dist: self.dist_ids(z, c),
                    id: c,
                })
                .collect();
            candidates.sort();
            self.links[layer][z as usize] = self.select_neighbors_heuristic(z, &candidates, m);
        }
    }

    /// New entry point after deleting the entry `p` (top layer `level`): a live top-layer neighbor
    /// if `p` had one, else the live node with the highest layer. `None` once nothing is live.
    fn replacement_entry(&self, p: u32, level: usize) -> Option<u32> {
        let on_top = self
            .links
            .get(level)
            .and_then(|layer| layer.get(p as usize))
            .and_then(|list| list.iter().copied().find(|&n| !self.deleted[n as usize]));
        on_top.or_else(|| {
            (0..self.node_layer.len() as u32)
                .filter(|&n| !self.deleted[n as usize])
                .max_by_key(|&n| self.node_layer[n as usize])
        })
    }

    /// Drops edges still pointing at pending slots, then makes those slots reusable. No distance
    /// work: one pass over the adjacency lists.
    fn sweep(&mut self) {
        let mut gone = vec![false; self.node_layer.len()];
        for &id in &self.pending {
            gone[id as usize] = true;
        }
        for layer in &mut self.links {
            for list in layer.iter_mut() {
                if list.iter().any(|&n| gone[n as usize]) {
                    list.retain(|&n| !gone[n as usize]);
                }
            }
        }
        self.free.append(&mut self.pending);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random vectors for tests.
    fn make_vectors(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut s = seed | 1;
        let mut next = || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            (s.wrapping_mul(0x2545F4914F6CDD1D) >> 40) as f32 / (1u32 << 24) as f32 - 0.5
        };
        (0..n).map(|_| (0..dim).map(|_| next()).collect()).collect()
    }

    fn recall_at_k(sim: Similarity, n: usize, dim: usize, k: usize) -> f64 {
        let vecs = make_vectors(n, dim, 42);
        let mut hnsw = Hnsw::new(dim, sim, HnswParams::default());
        for v in &vecs {
            hnsw.insert(v);
        }
        let queries = make_vectors(50, dim, 7);
        let mut hits = 0usize;
        let mut total = 0usize;
        for q in &queries {
            let approx: Vec<u32> = hnsw
                .search(q, k, None)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let exact: Vec<u32> = hnsw
                .search_exact(q, k)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let exact_set: std::collections::HashSet<u32> = exact.iter().copied().collect();
            hits += approx.iter().filter(|id| exact_set.contains(id)).count();
            total += k;
        }
        hits as f64 / total as f64
    }

    fn make_clustered(n: usize, dim: usize, seed: u64, clusters: usize) -> Vec<Vec<f32>> {
        let mut st = seed | 1;
        let mut next = move || {
            st ^= st >> 12;
            st ^= st << 25;
            st ^= st >> 27;
            (st.wrapping_mul(0x2545F4914F6CDD1D) >> 40) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let centers: Vec<Vec<f32>> = (0..clusters)
            .map(|_| (0..dim).map(|_| next()).collect())
            .collect();
        (0..n)
            .map(|i| {
                let c = &centers[i % clusters];
                (0..dim).map(|j| c[j] + 0.15 * next()).collect()
            })
            .collect()
    }

    #[test]
    fn clustered_recall_is_high() {
        let dim = 64;
        let n = 3000;
        let k = 10;
        let vecs = make_clustered(n, dim, 42, n / 10);
        let mut h = Hnsw::new(dim, Similarity::Cosine, HnswParams::default());
        for v in &vecs {
            h.insert(v);
        }
        let queries = make_clustered(50, dim, 7, 8);
        let mut hits = 0;
        let mut tot = 0;
        for q in &queries {
            let approx: std::collections::HashSet<u32> =
                h.search(q, k, None).into_iter().map(|(i, _)| i).collect();
            let exact: Vec<u32> = h.search_exact(q, k).into_iter().map(|(i, _)| i).collect();
            hits += exact.iter().filter(|i| approx.contains(i)).count();
            tot += k;
        }
        let r = hits as f64 / tot as f64;
        assert!(r >= 0.90, "clustered recall {r} too low");
    }

    #[test]
    fn quantized_recall_vs_float_ground_truth() {
        // int8 must keep recall against the TRUE float neighbors, not just the quantized
        // exact search: ground truth comes from an unquantized index.
        let dim = 128;
        let n = 3000;
        let k = 10;
        let vecs = make_clustered(n, dim, 42, n / 10);
        let queries = make_clustered(60, dim, 7, 8);

        let float_params = HnswParams {
            quantize: false,
            ..HnswParams::default()
        };
        let mut float_idx = Hnsw::new(dim, Similarity::Cosine, float_params);
        let mut quant_idx = Hnsw::new(dim, Similarity::Cosine, HnswParams::default());
        assert!(quant_idx.quantized, "default params should quantize cosine");
        assert!(!float_idx.quantized);
        for v in &vecs {
            float_idx.insert(v);
            quant_idx.insert(v);
        }

        let mut hits = 0;
        let mut tot = 0;
        for q in &queries {
            // float exact top-k = ground truth
            let truth: std::collections::HashSet<u32> = float_idx
                .search_exact(q, k)
                .into_iter()
                .map(|(i, _)| i)
                .collect();
            let got: Vec<u32> = quant_idx
                .search(q, k, None)
                .into_iter()
                .map(|(i, _)| i)
                .collect();
            hits += got.iter().filter(|i| truth.contains(i)).count();
            tot += k;
        }
        let r = hits as f64 / tot as f64;
        assert!(
            r >= 0.90,
            "quantized recall vs float ground truth {r} too low"
        );
    }

    /// Micro-benchmark of pure HNSW search (no Bolt, no planning): int8 vs f32
    /// traversal. Run with
    /// `cargo test --release -p traverse-core quantized_search_speedup -- --ignored --nocapture`.
    /// Once vectors exceed cache, int8 (4x less bandwidth) should be clearly faster;
    /// the f32 rerank keeps recall.
    #[test]
    #[ignore]
    fn quantized_search_speedup() {
        use std::time::Instant;
        let env = |k: &str, d: usize| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let dim = env("BENCH_DIM", 1024);
        let n = env("BENCH_N", 30_000);
        let iters = env("BENCH_ITERS", 2000);
        let vecs = make_clustered(n, dim, 42, n / 20);
        let queries = make_clustered(iters, dim, 7, 200);

        let build = |quant: bool| {
            let p = HnswParams {
                quantize: quant,
                ..Default::default()
            };
            let mut h = Hnsw::new(dim, Similarity::Cosine, p);
            for v in &vecs {
                h.insert(v);
            }
            h
        };
        let fidx = build(false);
        let qidx = build(true);

        let time = |h: &Hnsw| {
            // warm up
            for q in queries.iter().take(100) {
                std::hint::black_box(h.search(q, 10, None));
            }
            let t0 = Instant::now();
            for q in &queries {
                std::hint::black_box(h.search(q, 10, None));
            }
            t0.elapsed().as_secs_f64() * 1e6 / iters as f64 // us/query
        };
        let f_us = time(&fidx);
        let q_us = time(&qidx);
        eprintln!(
            "HNSW search us/query - f32: {f_us:.1}  int8+rerank: {q_us:.1}  speedup: {:.2}x  \
             (dim={dim}, n={n})",
            f_us / q_us
        );
    }

    #[test]
    fn quantized_scores_track_float_cosine() {
        // quantized cosine scores should closely match exact float cosine
        let dim = 256;
        let vecs = make_clustered(200, dim, 5, 20);
        let mut q = Hnsw::new(dim, Similarity::Cosine, HnswParams::default());
        let mut f = Hnsw::new(
            dim,
            Similarity::Cosine,
            HnswParams {
                quantize: false,
                ..Default::default()
            },
        );
        for v in &vecs {
            q.insert(v);
            f.insert(v);
        }
        // compare top-1 self-similarity for a few probes
        for probe in vecs.iter().take(10) {
            let qs = q.search(probe, 1, None)[0].1;
            let fs = f.search(probe, 1, None)[0].1;
            assert!(
                (qs - fs).abs() < 0.02,
                "quantized score {qs} vs float {fs} diverged"
            );
        }
    }

    #[test]
    fn cosine_recall_is_high() {
        // HNSW should recover >90% of the true top-10 on random data
        let r = recall_at_k(Similarity::Cosine, 2000, 32, 10);
        assert!(r >= 0.90, "cosine recall@10 = {r}");
    }

    #[test]
    fn euclidean_recall_is_high() {
        let r = recall_at_k(Similarity::Euclidean, 2000, 32, 10);
        assert!(r >= 0.90, "euclidean recall@10 = {r}");
    }

    #[test]
    fn exact_neighbor_is_found_first() {
        let dim = 16;
        let vecs = make_vectors(500, dim, 1);
        let mut hnsw = Hnsw::new(dim, Similarity::Cosine, HnswParams::default());
        for v in &vecs {
            hnsw.insert(v);
        }
        // exact copy of vector 123 must rank #1
        let res = hnsw.search(&vecs[123], 1, None);
        assert_eq!(res[0].0, 123, "exact match not returned first");
        assert!(
            res[0].1 > 0.999,
            "self-similarity score {} too low",
            res[0].1
        );
    }

    #[test]
    fn deleted_vectors_are_skipped() {
        let dim = 8;
        let vecs = make_vectors(200, dim, 3);
        let mut hnsw = Hnsw::new(dim, Similarity::Euclidean, HnswParams::default());
        for v in &vecs {
            hnsw.insert(v);
        }
        hnsw.delete(50);
        let res = hnsw.search(&vecs[50], 5, None);
        assert!(res.iter().all(|(id, _)| *id != 50), "deleted id returned");
    }

    #[test]
    fn deleting_entry_point_keeps_search_working() {
        let dim = 8;
        let vecs = make_vectors(300, dim, 9);
        let mut hnsw = Hnsw::new(dim, Similarity::Euclidean, HnswParams::default());
        for v in &vecs {
            hnsw.insert(v);
        }
        let entry = hnsw.entry.unwrap();
        hnsw.delete(entry);
        assert_ne!(hnsw.entry, Some(entry), "deleted entry point kept");
        // a query near the deleted entry keeps greedy descent parked on it
        let res = hnsw.search(&vecs[entry as usize], 5, None);
        assert_eq!(
            res.len(),
            5,
            "search returned {} hits after the entry point was deleted",
            res.len()
        );
    }

    /// Recall@k of `h` against exhaustive search over its live vectors.
    fn recall_vs_exact(h: &Hnsw, queries: &[Vec<f32>], k: usize) -> f64 {
        let mut hits = 0;
        for q in queries {
            let approx: std::collections::HashSet<u32> =
                h.search(q, k, None).into_iter().map(|(i, _)| i).collect();
            hits += h
                .search_exact(q, k)
                .into_iter()
                .filter(|(i, _)| approx.contains(i))
                .count();
        }
        hits as f64 / (queries.len() * k) as f64
    }

    #[test]
    fn recall_holds_under_churn() {
        let dim = 32;
        let n = 2000;
        let k = 10;
        let mut h = Hnsw::new(dim, Similarity::Euclidean, HnswParams::default());
        let mut ids: Vec<u32> = make_clustered(n, dim, 11, n / 20)
            .iter()
            .map(|v| h.insert(v))
            .collect();
        let queries = make_clustered(100, dim, 5, n / 20);
        // three rounds replacing half the vectors each (1.5n deletes + inserts)
        let fresh = make_clustered(n, dim, 99, n / 20);
        for round in 0..3 {
            for i in (round % 2..n).step_by(2) {
                h.delete(ids[i]);
                ids[i] = h.insert(&fresh[(i + round * 7) % n]);
            }
        }
        assert_eq!(h.len(), n);
        let after = recall_vs_exact(&h, &queries, k);
        let live: Vec<Vec<f32>> = (0..h.node_layer.len() as u32)
            .filter(|&i| !h.deleted[i as usize])
            .map(|i| h.vectors[i as usize * dim..(i as usize + 1) * dim].to_vec())
            .collect();
        let mut fresh_h = Hnsw::new(dim, Similarity::Euclidean, HnswParams::default());
        for v in &live {
            fresh_h.insert(v);
        }
        // in-place repair should match an index built from scratch over the same vectors
        let rebuilt = recall_vs_exact(&fresh_h, &queries, k);
        assert!(
            after >= rebuilt - 0.02,
            "recall {after:.3} after churn, {rebuilt:.3} for a fresh build"
        );
        // freed slots get reused, so storage tracks the live count
        assert!(
            h.node_layer.len() <= n + n / SWEEP_DIVISOR + SWEEP_MIN_PENDING,
            "{} slots for {n} live vectors",
            h.node_layer.len()
        );
    }

    #[test]
    fn delete_everything_then_reinsert() {
        let dim = 8;
        let vecs = make_vectors(150, dim, 4);
        let mut h = Hnsw::new(dim, Similarity::Euclidean, HnswParams::default());
        let ids: Vec<u32> = vecs.iter().map(|v| h.insert(v)).collect();
        for id in ids {
            h.delete(id);
        }
        assert!(h.is_empty());
        assert_eq!(h.entry, None);
        assert!(h.search(&vecs[0], 3, None).is_empty());
        let a = h.insert(&vecs[0]);
        h.insert(&vecs[1]);
        let res = h.search(&vecs[0], 2, None);
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].0, a);
    }

    #[test]
    fn repeated_updates_reuse_slots() {
        let mut idx = VectorIndex::new(
            "v".into(),
            LabelId(1),
            PropertyKeyId(1),
            4,
            Similarity::Euclidean,
            HnswParams::default(),
        );
        for i in 0..1000 {
            idx.upsert(NodeId(7), &[i as f32, 1.0, 2.0, 3.0]);
        }
        assert_eq!(idx.len(), 1);
        assert!(idx.hnsw.node_layer.len() <= SWEEP_MIN_PENDING + 1);
        let hit = idx.query(&[999.0, 1.0, 2.0, 3.0], 1, None);
        assert_eq!(hit, vec![(NodeId(7), hit[0].1)]);
    }

    #[test]
    fn empty_and_dim_mismatch_are_safe() {
        let hnsw = Hnsw::new(4, Similarity::Cosine, HnswParams::default());
        assert!(hnsw.search(&[1.0, 2.0, 3.0, 4.0], 5, None).is_empty());
        let mut h2 = Hnsw::new(4, Similarity::Cosine, HnswParams::default());
        h2.insert(&[1.0, 0.0, 0.0, 0.0]);
        assert!(h2.search(&[1.0, 2.0], 5, None).is_empty(), "dim mismatch");
    }
}
