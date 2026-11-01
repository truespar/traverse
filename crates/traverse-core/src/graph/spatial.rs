//! Spatial search: an R-tree over `POINT` properties behind `CREATE POINT INDEX`
//! and transparent acceleration of spatial predicates
//! (`point.distance(n.loc, q) < r`, `point.withinBBox(...)`,
//! `ORDER BY point.distance(...) LIMIT k`).
//!
//! Spatial counterpart of [`super::vector`] (HNSW) and [`super::fulltext`]
//! (BM25). Neo4j uses a Hilbert curve over a b-tree; a hand-rolled **R-tree**
//! needs no dependency and fits both kNN (best-first branch-and-bound) and
//! range/bbox queries better than a space-filling curve:
//!
//! * **Structure**: balanced R-tree of MBRs. Bulk-built with
//!   **Sort-Tile-Recursive** (STR) on load, then maintained incrementally
//!   (least-enlargement insert + quadratic split).
//! * **CRS-aware**: **partitioned by SRID**. Cartesian uses Euclidean distance,
//!   WGS-84 uses haversine, with an exact closed-form lower bound to a lat/lon
//!   rectangle so best-first kNN stays correct on the sphere.
//! * **Queries**: `within_bbox` (range), `within_distance` (CRS envelope + exact
//!   refinement), `knn` (best-first incremental, Hjaltason-Samet).
//!
//! Only the *definition* is persisted; the tree rebuilds from node points on load.

use rustc_hash::FxHashMap;

use super::property::PropertyValue;
use super::types::{LabelId, NodeId, PropertyKeyId};

/// `([x, y, z], srid)` of a stored value if it's a point.
///
/// Handles both stored forms: native [`PropertyValue::Point2D`]/[`Point3D`], and
/// the `{x, y, z?, srid, crs}` [`PropertyValue::Map`] made by the query-layer
/// `point()` (what's persisted today). A bare `{x, y}` map infers cartesian, like
/// the query layer's `unpack_point`.
///
/// [`Point3D`]: PropertyValue::Point3D
pub fn point_coords(pv: &PropertyValue) -> Option<([f64; 3], i64)> {
    match pv {
        PropertyValue::Point2D { x, y, srid } => Some(([*x, *y, 0.0], *srid as i64)),
        PropertyValue::Point3D { x, y, z, srid } => Some(([*x, *y, *z], *srid as i64)),
        PropertyValue::Map(m) => {
            let num = |k: &str| -> Option<f64> {
                match m.get(k)? {
                    PropertyValue::Float(f) => Some(*f),
                    PropertyValue::Int(i) => Some(*i as f64),
                    _ => None,
                }
            };
            let x = num("x")?;
            let y = num("y")?;
            let z = num("z");
            let srid = match m.get("srid") {
                Some(PropertyValue::Int(s)) => *s,
                _ => {
                    // no SRID: infer cartesian from dimensionality
                    if z.is_some() {
                        SRID_CARTESIAN_3D
                    } else {
                        SRID_CARTESIAN
                    }
                }
            };
            Some(([x, y, z.unwrap_or(0.0)], srid))
        }
        _ => None,
    }
}

/// WGS-84 equatorial radius (m), the sphere Neo4j's `point.distance` uses for
/// geographic CRSs. Same as the query-layer `point_distance` so index and scan
/// agree bit for bit.
const EARTH_RADIUS_M: f64 = 6_378_140.0;

/// Neo4j SRIDs.
const SRID_CARTESIAN: i64 = 7203;
const SRID_CARTESIAN_3D: i64 = 9157;
const SRID_WGS84: i64 = 4326;
const SRID_WGS84_3D: i64 = 4979;

/// Max entries per R-tree node before a split.
const MAX_ENTRIES: usize = 16;
/// Min entries on the larger side of a quadratic split.
const MIN_ENTRIES: usize = 6;

/// Geographic (lat/lon) CRS: haversine instead of Euclidean.
fn is_geographic(srid: i64) -> bool {
    srid == SRID_WGS84 || srid == SRID_WGS84_3D
}

/// Dimensionality of a SRID (2 or 3).
fn dims_for_srid(srid: i64) -> u8 {
    match srid {
        SRID_CARTESIAN_3D | SRID_WGS84_3D => 3,
        _ => 2,
    }
}

// -- Geometry ---------------------------------------------------------------

/// Axis-aligned bounding rectangle in native coordinates (up to 3D); `z` is 0.0 and ignored in 2D
/// partitions.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    min: [f64; 3],
    max: [f64; 3],
}

impl Rect {
    /// Degenerate rectangle around one point.
    fn point(c: [f64; 3]) -> Rect {
        Rect { min: c, max: c }
    }

    /// Smallest rectangle enclosing `self` and `other`.
    fn union(&self, other: &Rect) -> Rect {
        let mut r = *self;
        for i in 0..3 {
            r.min[i] = self.min[i].min(other.min[i]);
            r.max[i] = self.max[i].max(other.max[i]);
        }
        r
    }

    /// Grows in place to enclose `other`.
    fn extend(&mut self, other: &Rect) {
        for i in 0..3 {
            self.min[i] = self.min[i].min(other.min[i]);
            self.max[i] = self.max[i].max(other.max[i]);
        }
    }

    /// Area/volume over the first `dims` dimensions (split cost).
    fn volume(&self, dims: u8) -> f64 {
        let mut v = 1.0;
        for i in 0..dims as usize {
            v *= (self.max[i] - self.min[i]).max(0.0);
        }
        v
    }

    /// Volume growth needed to enclose `other` (least enlargement).
    fn enlargement(&self, other: &Rect, dims: u8) -> f64 {
        self.union(other).volume(dims) - self.volume(dims)
    }

    /// Overlap in every used dimension.
    fn intersects(&self, other: &Rect, dims: u8) -> bool {
        for i in 0..dims as usize {
            if self.min[i] > other.max[i] || self.max[i] < other.min[i] {
                return false;
            }
        }
        true
    }
}

// -- Distance & lower bounds --------------------------------------------------

/// Exact distance under the CRS metric: Euclidean (cartesian), haversine metres (geographic). Same
/// as `point_distance` in `traverse-cypher`.
pub(crate) fn point_dist(a: [f64; 3], b: [f64; 3], srid: i64) -> f64 {
    if is_geographic(srid) {
        haversine(a, b, srid)
    } else {
        let dims = dims_for_srid(srid);
        let mut s = 0.0;
        for i in 0..dims as usize {
            let d = b[i] - a[i];
            s += d * d;
        }
        s.sqrt()
    }
}

/// `p` inside the box spanned by `ll` / `ur` (either corner order) in the SRID's dimensions, as
/// [`SpatialIndex::within_bbox`] matches.
pub(crate) fn in_bbox(p: [f64; 3], ll: [f64; 3], ur: [f64; 3], srid: i64) -> bool {
    (0..dims_for_srid(srid) as usize).all(|i| {
        let (lo, hi) = if ll[i] <= ur[i] {
            (ll[i], ur[i])
        } else {
            (ur[i], ll[i])
        };
        lo <= p[i] && p[i] <= hi
    })
}

/// Great-circle distance in metres; `a`/`b` are `[lon, lat, height?]` in degrees. 3D adds height in
/// quadrature, like Neo4j.
fn haversine(a: [f64; 3], b: [f64; 3], srid: i64) -> f64 {
    let to_rad = std::f64::consts::PI / 180.0;
    let (lat1, lat2) = (a[1] * to_rad, b[1] * to_rad);
    let dlat = (b[1] - a[1]) * to_rad;
    let dlon = (b[0] - a[0]) * to_rad;
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    let great_circle = 2.0 * EARTH_RADIUS_M * h.sqrt().asin();
    if srid == SRID_WGS84_3D {
        let dz = b[2] - a[2];
        (great_circle * great_circle + dz * dz).sqrt()
    } else {
        great_circle
    }
}

/// Lower bound on the distance from `q` to *any* point in `r`; prunes subtrees in
/// best-first kNN, so it must never exceed the true minimum.
///
/// * Cartesian: exact distance to the closest point of the rect (clamp per axis).
/// * Geographic: exact minimum great-circle distance to the lat/lon box via the
///   closed-form maximiser of `cos(sigma) = sin(phi_q)*sin(phi) + cos(phi_q)*cos(phi)*cos(dlambda)` over the
///   box (height in quadrature). Assumes the box doesn't wrap the antimeridian,
///   as Neo4j's boxes don't.
fn rect_lower_bound(r: &Rect, q: [f64; 3], srid: i64) -> f64 {
    if !is_geographic(srid) {
        let dims = dims_for_srid(srid);
        let mut s = 0.0;
        for i in 0..dims as usize {
            let clamped = q[i].clamp(r.min[i], r.max[i]);
            let d = q[i] - clamped;
            s += d * d;
        }
        return s.sqrt();
    }

    // geographic: minimise the central angle to the box
    let to_rad = std::f64::consts::PI / 180.0;
    let (lon0, lon1) = (r.min[0], r.max[0]);
    let (lat0, lat1) = (r.min[1], r.max[1]);
    let lon_q = q[0];
    let lat_q = q[1];

    // smallest longitude gap from the query meridian to the box (0 if inside)
    let dlon_deg = if lon_q >= lon0 && lon_q <= lon1 {
        0.0
    } else {
        (lon_q - lon0).abs().min((lon_q - lon1).abs())
    };
    let dlon = dlon_deg * to_rad;

    // Maximise cos(sigma) = A*sin(phi) + B*cos(phi) over phi in [lat0,lat1], A=sin(phi_q), B=cos(phi_q)*cos(dlambda).
    let phi_q = lat_q * to_rad;
    let a = phi_q.sin();
    let b = phi_q.cos() * dlon.cos();
    let phi_star = a.atan2(b); // argmax of A*sin(phi) + B*cos(phi)
    let phi = phi_star.clamp(lat0 * to_rad, lat1 * to_rad);
    let cos_sigma = (a * phi.sin() + b * phi.cos()).clamp(-1.0, 1.0);
    let sigma = cos_sigma.acos();
    let great_circle = EARTH_RADIUS_M * sigma;

    if srid == SRID_WGS84_3D {
        let h_clamped = q[2].clamp(r.min[2], r.max[2]);
        let dz = q[2] - h_clamped;
        (great_circle * great_circle + dz * dz).sqrt()
    } else {
        great_circle
    }
}

// -- R-tree (arena) -----------------------------------------------------------

/// Leaf entry: node and its point.
#[derive(Clone, Copy, Debug)]
struct Entry {
    node: NodeId,
    coords: [f64; 3],
}

/// Arena tree node: leaf with entries or branch with child indices; caches its MBR.
struct TreeNode {
    mbr: Rect,
    leaf: bool,
    entries: Vec<Entry>,
    children: Vec<usize>,
}

impl TreeNode {
    fn new_leaf() -> TreeNode {
        TreeNode {
            mbr: Rect::point([0.0; 3]),
            leaf: true,
            entries: Vec::new(),
            children: Vec::new(),
        }
    }
    fn new_branch() -> TreeNode {
        TreeNode {
            mbr: Rect::point([0.0; 3]),
            leaf: false,
            entries: Vec::new(),
            children: Vec::new(),
        }
    }
}

/// Single-SRID R-tree: same dimensionality and metric for all points.
struct RTree {
    nodes: Vec<TreeNode>,
    root: usize,
    srid: i64,
    dims: u8,
    len: usize,
}

impl RTree {
    fn new(srid: i64) -> RTree {
        RTree {
            nodes: vec![TreeNode::new_leaf()],
            root: 0,
            srid,
            dims: dims_for_srid(srid),
            len: 0,
        }
    }

    /// Recomputes a node's MBR from its children/entries.
    fn recompute_mbr(&mut self, idx: usize) {
        let (leaf, first) = {
            let n = &self.nodes[idx];
            if n.leaf {
                (true, n.entries.first().map(|e| Rect::point(e.coords)))
            } else {
                (false, n.children.first().map(|&c| self.nodes[c].mbr))
            }
        };
        let Some(mut mbr) = first else { return };
        if leaf {
            let entries = self.nodes[idx].entries.clone();
            for e in &entries[1..] {
                mbr.extend(&Rect::point(e.coords));
            }
        } else {
            let children = self.nodes[idx].children.clone();
            for &c in &children[1..] {
                let cm = self.nodes[c].mbr;
                mbr.extend(&cm);
            }
        }
        self.nodes[idx].mbr = mbr;
    }

    // ---- Bulk load (Sort-Tile-Recursive) ------------------------------------

    /// Packed R-tree from a full point set, `O(n log n)`, near optimal. Used on index create and
    /// load.
    fn bulk_load(srid: i64, mut items: Vec<Entry>) -> RTree {
        let dims = dims_for_srid(srid);
        if items.is_empty() {
            return RTree::new(srid);
        }
        let len = items.len();

        // STR leaves: slice on x, tile on y within each slice
        let leaf_count = items.len().div_ceil(MAX_ENTRIES);
        let slice_count = (leaf_count as f64).sqrt().ceil() as usize;
        let per_slice = slice_count * MAX_ENTRIES;

        items.sort_by(|a, b| {
            a.coords[0]
                .partial_cmp(&b.coords[0])
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut nodes: Vec<TreeNode> = Vec::new();
        let mut level: Vec<usize> = Vec::new(); // node indices on the current level

        for slice in items.chunks_mut(per_slice.max(1)) {
            slice.sort_by(|a, b| {
                a.coords[1]
                    .partial_cmp(&b.coords[1])
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for chunk in slice.chunks(MAX_ENTRIES) {
                let mut leaf = TreeNode::new_leaf();
                leaf.entries.extend_from_slice(chunk);
                let mut mbr = Rect::point(chunk[0].coords);
                for e in &chunk[1..] {
                    mbr.extend(&Rect::point(e.coords));
                }
                leaf.mbr = mbr;
                level.push(nodes.len());
                nodes.push(leaf);
            }
        }

        // pack branch levels until one root remains
        while level.len() > 1 {
            // sort by MBR centroid x, tile like the leaves
            level.sort_by(|&a, &b| {
                let ca = nodes[a].mbr.min[0] + nodes[a].mbr.max[0];
                let cb = nodes[b].mbr.min[0] + nodes[b].mbr.max[0];
                ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
            });
            let node_count = level.len().div_ceil(MAX_ENTRIES);
            let slices = (node_count as f64).sqrt().ceil() as usize;
            let per = (slices * MAX_ENTRIES).max(1);

            let mut next: Vec<usize> = Vec::new();
            let mut si = 0;
            while si < level.len() {
                let end = (si + per).min(level.len());
                let mut slice: Vec<usize> = level[si..end].to_vec();
                si = end;
                slice.sort_by(|&a, &b| {
                    let ca = nodes[a].mbr.min[1] + nodes[a].mbr.max[1];
                    let cb = nodes[b].mbr.min[1] + nodes[b].mbr.max[1];
                    ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
                });
                for chunk in slice.chunks(MAX_ENTRIES) {
                    let mut branch = TreeNode::new_branch();
                    branch.children.extend_from_slice(chunk);
                    let mut mbr = nodes[chunk[0]].mbr;
                    for &c in &chunk[1..] {
                        let cm = nodes[c].mbr;
                        mbr.extend(&cm);
                    }
                    branch.mbr = mbr;
                    next.push(nodes.len());
                    nodes.push(branch);
                }
            }
            level = next;
        }

        let root = level[0];
        RTree {
            nodes,
            root,
            srid,
            dims,
            len,
        }
    }

    // ---- Incremental insert -------------------------------------------------

    /// Inserts one point, splitting overflowing nodes bottom-up.
    fn insert(&mut self, node: NodeId, coords: [f64; 3]) {
        let entry = Entry { node, coords };
        let rect = Rect::point(coords);
        let root = self.root;
        if let Some((new_a, new_b)) = self.insert_rec(root, entry, &rect) {
            // root split: new root one level up
            let mut new_root = TreeNode::new_branch();
            new_root.children = vec![new_a, new_b];
            let mut mbr = self.nodes[new_a].mbr;
            let bm = self.nodes[new_b].mbr;
            mbr.extend(&bm);
            new_root.mbr = mbr;
            self.root = self.nodes.len();
            self.nodes.push(new_root);
        }
        self.len += 1;
    }

    /// Recursive insert. `Some((a, b))` when `idx` split in two (`a` reuses `idx`'s slot); caller
    /// re-links.
    fn insert_rec(&mut self, idx: usize, entry: Entry, rect: &Rect) -> Option<(usize, usize)> {
        if self.nodes[idx].leaf {
            self.nodes[idx].entries.push(entry);
            self.nodes[idx].mbr.extend(rect);
            if self.nodes[idx].entries.len() > MAX_ENTRIES {
                return Some(self.split_leaf(idx));
            }
            return None;
        }

        // child needing the least enlargement for `rect`
        let best = self.choose_child(idx, rect);
        self.nodes[idx].mbr.extend(rect);
        if let Some((a, b)) = self.insert_rec(best, entry, rect) {
            // replace `best` (== a) and add sibling `b`
            let children = &mut self.nodes[idx].children;
            if let Some(pos) = children.iter().position(|&c| c == a) {
                children.insert(pos + 1, b);
            } else {
                children.push(b);
            }
            if self.nodes[idx].children.len() > MAX_ENTRIES {
                return Some(self.split_branch(idx));
            }
        }
        None
    }

    /// Child of branch `idx` with least MBR enlargement for `rect` (ties: smaller volume).
    fn choose_child(&self, idx: usize, rect: &Rect) -> usize {
        let dims = self.dims;
        let mut best = self.nodes[idx].children[0];
        let mut best_enl = f64::INFINITY;
        let mut best_vol = f64::INFINITY;
        for &c in &self.nodes[idx].children {
            let m = &self.nodes[c].mbr;
            let enl = m.enlargement(rect, dims);
            let vol = m.volume(dims);
            if enl < best_enl || (enl == best_enl && vol < best_vol) {
                best = c;
                best_enl = enl;
                best_vol = vol;
            }
        }
        best
    }

    /// Quadratic split of an overflowing leaf; `(reused_idx, new_idx)`.
    fn split_leaf(&mut self, idx: usize) -> (usize, usize) {
        let entries = std::mem::take(&mut self.nodes[idx].entries);
        let rects: Vec<Rect> = entries.iter().map(|e| Rect::point(e.coords)).collect();
        let (group_a, _group_b) = self.quadratic_split(&rects);

        let mut node_b = TreeNode::new_leaf();
        let (mut a_entries, mut b_entries) = (Vec::new(), Vec::new());
        for i in 0..entries.len() {
            if group_a.contains(&i) {
                a_entries.push(entries[i]);
            } else {
                b_entries.push(entries[i]);
            }
        }
        node_b.entries = b_entries;
        self.nodes[idx].entries = a_entries;
        let b_idx = self.nodes.len();
        self.nodes.push(node_b);
        self.recompute_mbr(idx);
        self.recompute_mbr(b_idx);
        (idx, b_idx)
    }

    /// Quadratic split of an overflowing branch; `(reused_idx, new_idx)`.
    fn split_branch(&mut self, idx: usize) -> (usize, usize) {
        let children = std::mem::take(&mut self.nodes[idx].children);
        let rects: Vec<Rect> = children.iter().map(|&c| self.nodes[c].mbr).collect();
        let (group_a, _group_b) = self.quadratic_split(&rects);

        let mut node_b = TreeNode::new_branch();
        let (mut a_children, mut b_children) = (Vec::new(), Vec::new());
        for i in 0..children.len() {
            if group_a.contains(&i) {
                a_children.push(children[i]);
            } else {
                b_children.push(children[i]);
            }
        }
        node_b.children = b_children;
        self.nodes[idx].children = a_children;
        let b_idx = self.nodes.len();
        self.nodes.push(node_b);
        self.recompute_mbr(idx);
        self.recompute_mbr(b_idx);
        (idx, b_idx)
    }

    /// Guttman's quadratic split: seed both groups with the pair wasting the most
    /// space together, assign the rest by strongest preference, respecting
    /// `MIN_ENTRIES`. Returns group A's indices.
    fn quadratic_split(&self, rects: &[Rect]) -> (Vec<usize>, Vec<usize>) {
        let dims = self.dims;
        let n = rects.len();
        // seeds: the pair with the most wasteful combined MBR
        let (mut s1, mut s2, mut worst) = (0usize, 1usize, f64::NEG_INFINITY);
        for i in 0..n {
            for j in (i + 1)..n {
                let d = rects[i].union(&rects[j]).volume(dims)
                    - rects[i].volume(dims)
                    - rects[j].volume(dims);
                if d > worst {
                    worst = d;
                    s1 = i;
                    s2 = j;
                }
            }
        }

        let mut group_a = vec![s1];
        let mut group_b = vec![s2];
        let mut mbr_a = rects[s1];
        let mut mbr_b = rects[s2];
        let mut assigned = vec![false; n];
        assigned[s1] = true;
        assigned[s2] = true;
        let mut remaining = n - 2;

        while remaining > 0 {
            // if a group would end below MIN_ENTRIES, it takes the rest
            if group_a.len() + remaining == MIN_ENTRIES {
                for (i, done) in assigned.iter().enumerate() {
                    if !done {
                        group_a.push(i);
                    }
                }
                break;
            }
            if group_b.len() + remaining == MIN_ENTRIES {
                for (i, done) in assigned.iter().enumerate() {
                    if !done {
                        group_b.push(i);
                    }
                }
                break;
            }

            // assign the entry with the strongest preference
            let (mut pick, mut pick_to_a, mut best_pref) = (usize::MAX, true, f64::NEG_INFINITY);
            for i in 0..n {
                if assigned[i] {
                    continue;
                }
                let da = mbr_a.enlargement(&rects[i], dims);
                let db = mbr_b.enlargement(&rects[i], dims);
                let pref = (da - db).abs();
                if pref > best_pref {
                    best_pref = pref;
                    pick = i;
                    pick_to_a = da < db || (da == db && mbr_a.volume(dims) <= mbr_b.volume(dims));
                }
            }
            assigned[pick] = true;
            remaining -= 1;
            if pick_to_a {
                group_a.push(pick);
                mbr_a.extend(&rects[pick]);
            } else {
                group_b.push(pick);
                mbr_b.extend(&rects[pick]);
            }
        }
        (group_a, group_b)
    }

    // ---- Remove -------------------------------------------------------------

    /// Removes `node`'s entry (its coordinates guide the descent). Under-full nodes
    /// are tolerated, not condensed; the occasional bulk rebuild restores packing.
    /// `true` if found.
    fn remove(&mut self, node: NodeId, coords: [f64; 3]) -> bool {
        let rect = Rect::point(coords);
        let removed = self.remove_rec(self.root, node, &rect);
        if removed {
            self.len -= 1;
        }
        removed
    }

    fn remove_rec(&mut self, idx: usize, node: NodeId, rect: &Rect) -> bool {
        if self.nodes[idx].leaf {
            if let Some(pos) = self.nodes[idx].entries.iter().position(|e| e.node == node) {
                self.nodes[idx].entries.swap_remove(pos);
                self.recompute_mbr(idx);
                return true;
            }
            return false;
        }
        let dims = self.dims;
        let children = self.nodes[idx].children.clone();
        for c in children {
            if self.nodes[c].mbr.intersects(rect, dims) && self.remove_rec(c, node, rect) {
                self.recompute_mbr(idx);
                return true;
            }
        }
        false
    }

    // ---- Queries ------------------------------------------------------------

    /// Every entry whose point lies in `rect`.
    fn search_rect(&self, rect: &Rect, out: &mut Vec<Entry>) {
        self.search_rect_rec(self.root, rect, out);
    }

    fn search_rect_rec(&self, idx: usize, rect: &Rect, out: &mut Vec<Entry>) {
        let node = &self.nodes[idx];
        if !node.mbr.intersects(rect, self.dims) {
            return;
        }
        if node.leaf {
            for e in &node.entries {
                if Rect::point(e.coords).intersects(rect, self.dims) {
                    out.push(*e);
                }
            }
        } else {
            for &c in &node.children {
                self.search_rect_rec(c, rect, out);
            }
        }
    }

    /// Best-first incremental kNN (Hjaltason-Samet): up to `k` `(node, distance)` ascending.
    fn knn(&self, q: [f64; 3], k: usize) -> Vec<(NodeId, f64)> {
        use std::cmp::Ordering;
        use std::collections::BinaryHeap;

        if k == 0 || self.len == 0 {
            return Vec::new();
        }

        // heap item by ascending key (lower bound for tree nodes, exact distance for
        // points); `is_point` tells which.
        //
        // ties resolve to a `(distance, node_id)` order: at equal distance tree nodes
        // expand *before* points emit, so every point at that distance is already in the
        // heap, then points emit by ascending node id
        struct Item {
            key: f64,
            is_point: bool,
            idx: usize,
            node: NodeId,
        }
        impl Item {
            /// smaller rank pops first; nodes (0) before points (1)
            fn rank(&self) -> (f64, u8, u64) {
                (self.key, u8::from(self.is_point), self.node.0)
            }
        }
        impl PartialEq for Item {
            fn eq(&self, o: &Self) -> bool {
                self.rank() == o.rank()
            }
        }
        impl Eq for Item {}
        impl PartialOrd for Item {
            fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
                Some(self.cmp(o))
            }
        }
        impl Ord for Item {
            fn cmp(&self, o: &Self) -> Ordering {
                // reversed so the max-heap yields the smallest rank
                let (ak, ap, an) = o.rank();
                let (bk, bp, bn) = self.rank();
                ak.partial_cmp(&bk)
                    .unwrap_or(Ordering::Equal)
                    .then(ap.cmp(&bp))
                    .then(an.cmp(&bn))
            }
        }

        let mut heap: BinaryHeap<Item> = BinaryHeap::new();
        heap.push(Item {
            key: rect_lower_bound(&self.nodes[self.root].mbr, q, self.srid),
            is_point: false,
            idx: self.root,
            node: NodeId(0),
        });
        let mut out = Vec::with_capacity(k);

        while let Some(it) = heap.pop() {
            if it.is_point {
                out.push((it.node, it.key));
                if out.len() == k {
                    break;
                }
                continue;
            }
            let node = &self.nodes[it.idx];
            if node.leaf {
                for e in &node.entries {
                    let d = point_dist(e.coords, q, self.srid);
                    heap.push(Item {
                        key: d,
                        is_point: true,
                        idx: 0,
                        node: e.node,
                    });
                }
            } else {
                for &c in &node.children {
                    let lb = rect_lower_bound(&self.nodes[c].mbr, q, self.srid);
                    heap.push(Item {
                        key: lb,
                        is_point: false,
                        idx: c,
                        node: NodeId(0),
                    });
                }
            }
        }
        out
    }
}

// -- Per-index facade ---------------------------------------------------------

/// Named point index over one `(label, property)`: one R-tree per SRID so
/// cartesian and geographic points never mix metrics. Maintained incrementally.
pub struct SpatialIndex {
    name: String,
    label_id: LabelId,
    property_key: PropertyKeyId,
    /// One R-tree per SRID in the data.
    partitions: FxHashMap<i64, RTree>,
    /// node -> (srid, coords), so update/delete finds the partition.
    node_loc: FxHashMap<NodeId, (i64, [f64; 3])>,
}

impl Clone for SpatialIndex {
    fn clone(&self) -> Self {
        // rebuild trees from `node_loc` instead of deep-cloning the arena: Graph clones are rare
        // (snapshots) and the tree types stay free of a manual deep clone
        let mut c = SpatialIndex {
            name: self.name.clone(),
            label_id: self.label_id,
            property_key: self.property_key,
            partitions: FxHashMap::default(),
            node_loc: self.node_loc.clone(),
        };
        c.rebuild();
        c
    }
}

impl std::fmt::Debug for SpatialIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpatialIndex")
            .field("name", &self.name)
            .field("points", &self.node_loc.len())
            .finish()
    }
}

impl SpatialIndex {
    /// Empty point index on `(label, property)`.
    pub fn new(name: &str, label_id: LabelId, property_key: PropertyKeyId) -> SpatialIndex {
        SpatialIndex {
            name: name.to_string(),
            label_id,
            property_key,
            partitions: FxHashMap::default(),
            node_loc: FxHashMap::default(),
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

    /// Rebuilds every partition from `node_loc` (STR bulk load).
    fn rebuild(&mut self) {
        let mut by_srid: FxHashMap<i64, Vec<Entry>> = FxHashMap::default();
        for (node, (srid, coords)) in &self.node_loc {
            by_srid.entry(*srid).or_default().push(Entry {
                node: *node,
                coords: *coords,
            });
        }
        self.partitions.clear();
        for (srid, items) in by_srid {
            self.partitions.insert(srid, RTree::bulk_load(srid, items));
        }
    }

    /// (Re-)indexes a node's point. `[x, y, z]`, `z` unused for 2D SRIDs. Re-indexing
    /// removes the old location first, even across a SRID change.
    pub fn index_point(&mut self, node: NodeId, coords: [f64; 3], srid: i64) {
        if let Some((old_srid, old_coords)) = self.node_loc.insert(node, (srid, coords))
            && let Some(tree) = self.partitions.get_mut(&old_srid)
        {
            tree.remove(node, old_coords);
        }
        self.partitions
            .entry(srid)
            .or_insert_with(|| RTree::new(srid))
            .insert(node, coords);
    }

    /// Removes a node (delete or property cleared).
    pub fn remove_point(&mut self, node: NodeId) {
        if let Some((srid, coords)) = self.node_loc.remove(&node)
            && let Some(tree) = self.partitions.get_mut(&srid)
        {
            tree.remove(node, coords);
        }
    }

    /// Bulk-loads `(node, coords, srid)`: faster and better packed than repeated `index_point`.
    /// Used on load.
    pub fn bulk_index(&mut self, points: impl IntoIterator<Item = (NodeId, [f64; 3], i64)>) {
        for (node, coords, srid) in points {
            self.node_loc.insert(node, (srid, coords));
        }
        self.rebuild();
    }

    /// Nodes whose point is in the box `[ll, ur]` of this SRID (`point.withinBBox`); other SRIDs
    /// never match.
    pub fn within_bbox(&self, ll: [f64; 3], ur: [f64; 3], srid: i64) -> Vec<NodeId> {
        let Some(tree) = self.partitions.get(&srid) else {
            return Vec::new();
        };
        // corners may come in either order
        let mut rect = Rect { min: ll, max: ur };
        for i in 0..3 {
            if rect.min[i] > rect.max[i] {
                rect.min[i] = ur[i];
                rect.max[i] = ll[i];
            }
        }
        let mut hits = Vec::new();
        tree.search_rect(&rect, &mut hits);
        hits.into_iter().map(|e| e.node).collect()
    }

    /// Nodes within `radius` of `q` (metres if geographic, else coordinate units) as `(node,
    /// distance)`: `point.distance(...) <= r`.
    pub fn within_distance(&self, q: [f64; 3], radius: f64, srid: i64) -> Vec<(NodeId, f64)> {
        let Some(tree) = self.partitions.get(&srid) else {
            return Vec::new();
        };
        if radius < 0.0 {
            return Vec::new();
        }
        // coarse envelope that surely encloses the radius, then exact refinement
        let env = self.radius_envelope(q, radius, srid);
        let mut cand = Vec::new();
        tree.search_rect(&env, &mut cand);
        let mut out = Vec::new();
        for e in cand {
            let d = point_dist(e.coords, q, srid);
            if d <= radius {
                out.push((e.node, d));
            }
        }
        out
    }

    /// Up to `k` nearest nodes in this SRID, ascending (`ORDER BY point.distance(...) LIMIT k`).
    pub fn knn(&self, q: [f64; 3], k: usize, srid: i64) -> Vec<(NodeId, f64)> {
        let Some(tree) = self.partitions.get(&srid) else {
            return Vec::new();
        };
        tree.knn(q, k)
    }

    /// Native-coordinate rectangle containing every point within `radius` of `q` (superset; caller
    /// refines).
    fn radius_envelope(&self, q: [f64; 3], radius: f64, srid: i64) -> Rect {
        if !is_geographic(srid) {
            let mut min = q;
            let mut max = q;
            let dims = dims_for_srid(srid);
            for i in 0..dims as usize {
                min[i] = q[i] - radius;
                max[i] = q[i] + radius;
            }
            return Rect { min, max };
        }
        // geographic: metres -> degree envelope; longitude degrees shrink with latitude, near the
        // poles use the full range
        let deg_per_m_lat = 180.0 / (std::f64::consts::PI * EARTH_RADIUS_M);
        let dlat = radius * deg_per_m_lat;
        let cos_lat = (q[1] * std::f64::consts::PI / 180.0).cos().abs();
        let dlon = if cos_lat < 1e-9 {
            180.0
        } else {
            (dlat / cos_lat).min(180.0)
        };
        let mut min = [q[0] - dlon, (q[1] - dlat).max(-90.0), q[2]];
        let mut max = [q[0] + dlon, (q[1] + dlat).min(90.0), q[2]];
        if srid == SRID_WGS84_3D {
            min[2] = q[2] - radius;
            max[2] = q[2] + radius;
        }
        Rect { min, max }
    }
}

// -- Registry -----------------------------------------------------------------

/// Point indexes of a graph, by name.
#[derive(Clone, Debug, Default)]
pub struct SpatialIndexRegistry {
    by_name: FxHashMap<String, SpatialIndex>,
    generation: u64,
}

impl SpatialIndexRegistry {
    /// Registers a new empty index. Errors if the name is taken.
    pub fn create(
        &mut self,
        name: &str,
        label_id: LabelId,
        property_key: PropertyKeyId,
    ) -> Result<(), String> {
        if self.by_name.contains_key(name) {
            return Err(format!("point index '{name}' already exists"));
        }
        self.by_name.insert(
            name.to_string(),
            SpatialIndex::new(name, label_id, property_key),
        );
        self.generation += 1;
        Ok(())
    }

    /// Drop an index by name.
    pub fn drop(&mut self, name: &str) -> Result<(), String> {
        if self.by_name.remove(name).is_none() {
            return Err(format!("no such point index: {name}"));
        }
        self.generation += 1;
        Ok(())
    }

    /// Look up an index by name.
    pub fn get(&self, name: &str) -> Option<&SpatialIndex> {
        self.by_name.get(name)
    }
    /// Mutable lookup (maintenance).
    pub fn get_mut(&mut self, name: &str) -> Option<&mut SpatialIndex> {
        self.by_name.get_mut(name)
    }
    /// Iterate all indexes.
    pub fn iter(&self) -> impl Iterator<Item = &SpatialIndex> {
        self.by_name.values()
    }
    /// No point indexes (write fast path).
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
    /// spatial predicates through it.
    pub fn index_for(&self, label: LabelId, property: PropertyKeyId) -> Option<&SpatialIndex> {
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

    /// Brute-force kNN to cross-check the R-tree.
    fn brute_knn(pts: &[(NodeId, [f64; 3])], q: [f64; 3], k: usize, srid: i64) -> Vec<NodeId> {
        let mut d: Vec<(NodeId, f64)> = pts
            .iter()
            .map(|(n, c)| (*n, point_dist(*c, q, srid)))
            .collect();
        d.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap()
                .then_with(|| a.0.0.cmp(&b.0.0))
        });
        d.into_iter().take(k).map(|(n, _)| n).collect()
    }

    #[test]
    fn bulk_load_and_bbox_cartesian() {
        let pts: Vec<(NodeId, [f64; 3])> = (0..200)
            .map(|i| (nid(i), [(i % 20) as f64, (i / 20) as f64, 0.0]))
            .collect();
        let mut idx = SpatialIndex::new("p", LabelId(1), PropertyKeyId(1));
        idx.bulk_index(pts.iter().map(|(n, c)| (*n, *c, SRID_CARTESIAN)));
        assert_eq!(idx.node_loc.len(), 200);

        // box x in [0,4], y in [0,2]
        let hits = idx.within_bbox([0.0, 0.0, 0.0], [4.0, 2.0, 0.0], SRID_CARTESIAN);
        let expected: Vec<NodeId> = pts
            .iter()
            .filter(|(_, c)| c[0] >= 0.0 && c[0] <= 4.0 && c[1] >= 0.0 && c[1] <= 2.0)
            .map(|(n, _)| *n)
            .collect();
        let mut got = hits.clone();
        got.sort_by_key(|n| n.0);
        let mut exp = expected.clone();
        exp.sort_by_key(|n| n.0);
        assert_eq!(got, exp);
    }

    #[test]
    fn incremental_insert_matches_bulk() {
        let pts: Vec<(NodeId, [f64; 3])> = (0..100)
            .map(|i| (nid(i), [((i * 7) % 50) as f64, ((i * 13) % 50) as f64, 0.0]))
            .collect();
        let mut idx = SpatialIndex::new("p", LabelId(1), PropertyKeyId(1));
        for (n, c) in &pts {
            idx.index_point(*n, *c, SRID_CARTESIAN);
        }
        assert_eq!(idx.node_loc.len(), 100);
        let q = [25.0, 25.0, 0.0];
        let got: Vec<NodeId> = idx
            .knn(q, 5, SRID_CARTESIAN)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        let exp = brute_knn(&pts, q, 5, SRID_CARTESIAN);
        assert_eq!(got, exp);
    }

    #[test]
    fn knn_cartesian_matches_bruteforce() {
        let pts: Vec<(NodeId, [f64; 3])> = (0..500)
            .map(|i| {
                (
                    nid(i),
                    [((i * 31) % 97) as f64, ((i * 17) % 89) as f64, 0.0],
                )
            })
            .collect();
        let mut idx = SpatialIndex::new("p", LabelId(1), PropertyKeyId(1));
        idx.bulk_index(pts.iter().map(|(n, c)| (*n, *c, SRID_CARTESIAN)));
        for &q in &[[10.0, 10.0, 0.0], [50.0, 40.0, 0.0], [0.0, 0.0, 0.0]] {
            let got: Vec<NodeId> = idx
                .knn(q, 10, SRID_CARTESIAN)
                .into_iter()
                .map(|(n, _)| n)
                .collect();
            let exp = brute_knn(&pts, q, 10, SRID_CARTESIAN);
            assert_eq!(got, exp, "kNN mismatch at {q:?}");
        }
    }

    #[test]
    fn within_distance_cartesian() {
        let pts: Vec<(NodeId, [f64; 3])> = (0..200)
            .map(|i| (nid(i), [(i % 20) as f64, (i / 20) as f64, 0.0]))
            .collect();
        let mut idx = SpatialIndex::new("p", LabelId(1), PropertyKeyId(1));
        idx.bulk_index(pts.iter().map(|(n, c)| (*n, *c, SRID_CARTESIAN)));
        let q = [5.0, 5.0, 0.0];
        let r = 3.0;
        let mut got: Vec<NodeId> = idx
            .within_distance(q, r, SRID_CARTESIAN)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        got.sort_by_key(|n| n.0);
        let mut exp: Vec<NodeId> = pts
            .iter()
            .filter(|(_, c)| point_dist(*c, q, SRID_CARTESIAN) <= r)
            .map(|(n, _)| *n)
            .collect();
        exp.sort_by_key(|n| n.0);
        assert_eq!(got, exp);
    }

    #[test]
    fn knn_geographic_matches_bruteforce() {
        // lon/lat scatter around Europe
        let pts: Vec<(NodeId, [f64; 3])> = (0..300)
            .map(|i| {
                let lon = -10.0 + (i as f64 * 0.13) % 40.0;
                let lat = 35.0 + (i as f64 * 0.07) % 25.0;
                (nid(i), [lon, lat, 0.0])
            })
            .collect();
        let mut idx = SpatialIndex::new("p", LabelId(1), PropertyKeyId(1));
        idx.bulk_index(pts.iter().map(|(n, c)| (*n, *c, SRID_WGS84)));
        for &q in &[[2.35, 48.85, 0.0], [13.4, 52.5, 0.0], [-3.7, 40.4, 0.0]] {
            let got: Vec<NodeId> = idx
                .knn(q, 8, SRID_WGS84)
                .into_iter()
                .map(|(n, _)| n)
                .collect();
            let exp = brute_knn(&pts, q, 8, SRID_WGS84);
            assert_eq!(got, exp, "geographic kNN mismatch at {q:?}");
        }
    }

    #[test]
    fn within_distance_geographic() {
        let pts: Vec<(NodeId, [f64; 3])> = (0..300)
            .map(|i| {
                let lon = -10.0 + (i as f64 * 0.13) % 40.0;
                let lat = 35.0 + (i as f64 * 0.07) % 25.0;
                (nid(i), [lon, lat, 0.0])
            })
            .collect();
        let mut idx = SpatialIndex::new("p", LabelId(1), PropertyKeyId(1));
        idx.bulk_index(pts.iter().map(|(n, c)| (*n, *c, SRID_WGS84)));
        let q = [2.35, 48.85, 0.0]; // Paris
        let r = 500_000.0; // 500 km
        let mut got: Vec<NodeId> = idx
            .within_distance(q, r, SRID_WGS84)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        got.sort_by_key(|n| n.0);
        let mut exp: Vec<NodeId> = pts
            .iter()
            .filter(|(_, c)| point_dist(*c, q, SRID_WGS84) <= r)
            .map(|(n, _)| *n)
            .collect();
        exp.sort_by_key(|n| n.0);
        assert_eq!(got, exp);
    }

    #[test]
    fn update_and_delete() {
        let mut idx = SpatialIndex::new("p", LabelId(1), PropertyKeyId(1));
        for i in 0..50 {
            idx.index_point(nid(i), [i as f64, 0.0, 0.0], SRID_CARTESIAN);
        }
        // move node 10 far away; it leaves a near-origin query
        idx.index_point(nid(10), [1000.0, 1000.0, 0.0], SRID_CARTESIAN);
        let near: Vec<NodeId> = idx
            .knn([5.0, 0.0, 0.0], 5, SRID_CARTESIAN)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert!(!near.contains(&nid(10)));
        // delete node 5; it must vanish
        idx.remove_point(nid(5));
        assert_eq!(idx.node_loc.len(), 49);
        let all: Vec<NodeId> = idx.within_bbox([-1.0, -1.0, 0.0], [50.0, 1.0, 0.0], SRID_CARTESIAN);
        assert!(!all.contains(&nid(5)));
    }

    #[test]
    fn srid_partitions_are_isolated() {
        let mut idx = SpatialIndex::new("p", LabelId(1), PropertyKeyId(1));
        idx.index_point(nid(1), [0.0, 0.0, 0.0], SRID_CARTESIAN);
        idx.index_point(nid(2), [0.0, 0.0, 0.0], SRID_WGS84);
        // cartesian query sees only the cartesian point
        let c = idx.knn([0.0, 0.0, 0.0], 10, SRID_CARTESIAN);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, nid(1));
        // geographic query sees only the geographic point
        let g = idx.knn([0.0, 0.0, 0.0], 10, SRID_WGS84);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].0, nid(2));
    }

    #[test]
    fn registry_create_drop_and_lookup() {
        let mut reg = SpatialIndexRegistry::default();
        reg.create("loc", LabelId(1), PropertyKeyId(2)).unwrap();
        assert!(reg.create("loc", LabelId(1), PropertyKeyId(2)).is_err());
        assert!(reg.index_for(LabelId(1), PropertyKeyId(2)).is_some());
        assert!(reg.index_for(LabelId(9), PropertyKeyId(2)).is_none());
        assert_eq!(reg.indexes_for_label(LabelId(1)), vec!["loc".to_string()]);
        reg.drop("loc").unwrap();
        assert!(reg.is_empty());
    }
}
