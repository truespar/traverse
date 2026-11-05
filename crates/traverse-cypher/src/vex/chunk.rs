//! The result set operators read and write: groups of vectors with selection and flattening
//! state, whose cross product is the chunk's rows.
//!
//! A [`Group`] holds the columns one source produced together (a scan's node, an expansion's
//! edge and neighbor, a projection's output). While a group is *unflat* every selected lane is a
//! row; a *flat* group contributes the single lane at its current position. The rows of a chunk
//! are the cross product of its active groups, so an expansion that produced 100 neighbors for one
//! source is 100 rows without copying the source 100 times (factorized intermediates). Operators
//! that need rows one at a time iterate the product with [`ResultSet::for_each_row`].

use crate::error::CypherError;

use super::vector::{ValueRef, Vector};

/// Index of a group in a [`ResultSet`].
pub type GroupId = u16;

/// Where a variable lives: a column of a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DataPos {
    /// The group.
    pub group: GroupId,
    /// The column within it.
    pub col: u16,
}

impl DataPos {
    /// A position.
    pub const fn new(group: GroupId, col: u16) -> Self {
        Self { group, col }
    }
}

/// Columns one source produced together, with their selection and flattening state.
#[derive(Debug, Default, Clone)]
pub struct Group {
    /// Columns; every active column has `len` lanes.
    pub cols: Vec<Vector>,
    /// Lanes.
    pub len: usize,
    /// Lanes that survive the filters applied so far, in order; `None` = all of them.
    pub sel: Option<Vec<u32>>,
    /// `Some(pos)`: the group contributes the single selected lane `pos` (an index into the
    /// selection, or into the lanes when there is none).
    pub flat: Option<usize>,
    /// Has a source filled it in this chunk? Inactive groups take no part in the row product.
    pub active: bool,
}

impl Group {
    /// A group with `ncols` empty columns.
    pub fn with_cols(ncols: usize) -> Self {
        Self {
            cols: (0..ncols).map(|_| Vector::empty()).collect(),
            len: 0,
            sel: None,
            flat: None,
            active: false,
        }
    }

    /// Selected lanes.
    #[inline]
    pub fn selected_len(&self) -> usize {
        match &self.sel {
            Some(s) => s.len(),
            None => self.len,
        }
    }

    /// Rows this group contributes to the product: 0 when inactive, 1 when flat, else the
    /// selected lanes.
    #[inline]
    pub fn multiplicity(&self) -> usize {
        if !self.active {
            return 1;
        }
        if self.flat.is_some() {
            1
        } else {
            self.selected_len()
        }
    }

    /// The lane of selected position `pos`.
    #[inline]
    pub fn lane(&self, pos: usize) -> usize {
        match &self.sel {
            Some(s) => s[pos] as usize,
            None => pos,
        }
    }

    /// The lane a flat group currently stands on.
    #[inline]
    pub fn current_lane(&self) -> Option<usize> {
        self.flat.map(|p| self.lane(p))
    }

    /// Resets the group for a new fill of `len` lanes: no selection, unflat, active.
    #[inline]
    pub fn reset(&mut self, len: usize) {
        self.len = len;
        self.sel = None;
        self.flat = None;
        self.active = true;
    }

    /// Marks the group empty and inactive.
    pub fn clear(&mut self) {
        self.len = 0;
        self.sel = None;
        self.flat = None;
        self.active = false;
    }

    /// Keeps only the selected lanes `keep` (positions into the current selection).
    pub fn narrow(&mut self, keep: Vec<u32>) {
        let sel = match &self.sel {
            Some(old) => keep.into_iter().map(|p| old[p as usize]).collect(),
            None => keep,
        };
        self.sel = Some(sel);
    }

    /// Selected lanes, in order.
    pub fn selected_lanes(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.selected_len()).map(move |p| self.lane(p))
    }
}

/// The chunk: every group of the pipeline; see the module docs.
#[derive(Debug, Default)]
pub struct ResultSet {
    /// Groups by id.
    pub groups: Vec<Group>,
}

impl ResultSet {
    /// A result set with the given column counts per group.
    pub fn new(group_widths: &[u16]) -> Self {
        Self {
            groups: group_widths
                .iter()
                .map(|&w| Group::with_cols(w as usize))
                .collect(),
        }
    }

    /// The column at `pos`.
    #[inline]
    pub fn col(&self, pos: DataPos) -> &Vector {
        &self.groups[pos.group as usize].cols[pos.col as usize]
    }

    /// The column at `pos`, mutable.
    #[inline]
    pub fn col_mut(&mut self, pos: DataPos) -> &mut Vector {
        &mut self.groups[pos.group as usize].cols[pos.col as usize]
    }

    /// The group of `pos`.
    #[inline]
    pub fn group(&self, g: GroupId) -> &Group {
        &self.groups[g as usize]
    }

    /// The group of `pos`, mutable.
    #[inline]
    pub fn group_mut(&mut self, g: GroupId) -> &mut Group {
        &mut self.groups[g as usize]
    }

    /// Rows in the product of `groups` (every group's multiplicity multiplied).
    pub fn row_count(&self, groups: &[GroupId]) -> usize {
        Self::rows_of(&self.groups, groups)
    }

    /// [`row_count`](Self::row_count) over a chunk's groups taken out of their result set.
    pub fn rows_of(all: &[Group], groups: &[GroupId]) -> usize {
        groups.iter().fold(1usize, |acc, &g| {
            acc.saturating_mul(all[g as usize].multiplicity())
        })
    }

    /// The value at `pos` for the lane `lanes[pos.group]`.
    #[inline]
    pub fn value_at(&self, pos: DataPos, lanes: &[usize]) -> ValueRef<'_> {
        self.col(pos).get(lanes[pos.group as usize])
    }

    /// The single value at `pos` when its group is flat (or has one selected lane).
    pub fn scalar_at(&self, pos: DataPos) -> ValueRef<'_> {
        let g = &self.groups[pos.group as usize];
        match g.current_lane() {
            Some(lane) => g.cols[pos.col as usize].get(lane),
            None if g.active && g.selected_len() == 1 => g.cols[pos.col as usize].get(g.lane(0)),
            None => ValueRef::Null,
        }
    }

    /// Calls `f` once per row of the product of `groups`, with the lane each group stands on
    /// (indexed by group id; groups outside `groups` keep their current lane when flat, else 0).
    /// Rows run in group-major order: the first group in `groups` varies slowest.
    pub fn for_each_row<F>(&self, groups: &[GroupId], mut f: F) -> Result<(), CypherError>
    where
        F: FnMut(&[usize]) -> Result<(), CypherError>,
    {
        let mut lanes = vec![0usize; self.groups.len()];
        for (g, group) in self.groups.iter().enumerate() {
            if let Some(l) = group.current_lane() {
                lanes[g] = l;
            }
        }
        // the groups that vary
        let varying: Vec<GroupId> = groups
            .iter()
            .copied()
            .filter(|&g| {
                let gr = &self.groups[g as usize];
                gr.active && gr.flat.is_none()
            })
            .collect();
        if varying
            .iter()
            .any(|&g| self.groups[g as usize].selected_len() == 0)
        {
            return Ok(());
        }
        if varying.is_empty() {
            return f(&lanes);
        }
        let mut pos = vec![0usize; varying.len()];
        for (i, &g) in varying.iter().enumerate() {
            lanes[g as usize] = self.groups[g as usize].lane(pos[i]);
        }
        let mut n = 0u32;
        loop {
            f(&lanes)?;
            // a product of several groups can be enormous: honour the deadline inside it
            n = n.wrapping_add(1);
            if n & 0xFFF == 0 {
                crate::query::check()?;
            }
            // odometer: last group varies fastest
            let mut i = varying.len();
            loop {
                if i == 0 {
                    return Ok(());
                }
                i -= 1;
                let g = varying[i] as usize;
                pos[i] += 1;
                if pos[i] < self.groups[g].selected_len() {
                    lanes[g] = self.groups[g].lane(pos[i]);
                    break;
                }
                pos[i] = 0;
                lanes[g] = self.groups[g].lane(0);
            }
        }
    }

    /// Lanes per group of row `row` of the product of `groups` (group-major), for random access.
    pub fn row_lanes(&self, groups: &[GroupId], mut row: usize, lanes: &mut Vec<usize>) {
        lanes.clear();
        lanes.resize(self.groups.len(), 0);
        for (g, group) in self.groups.iter().enumerate() {
            if let Some(l) = group.current_lane() {
                lanes[g] = l;
            }
        }
        let varying: Vec<GroupId> = groups
            .iter()
            .copied()
            .filter(|&g| {
                let gr = &self.groups[g as usize];
                gr.active && gr.flat.is_none()
            })
            .collect();
        for &g in varying.iter().rev() {
            let n = self.groups[g as usize].selected_len().max(1);
            let p = row % n;
            row /= n;
            lanes[g as usize] = self.groups[g as usize].lane(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_of_groups() {
        let mut rs = ResultSet::new(&[1, 1]);
        rs.groups[0].cols[0] = Vector::ints(vec![10, 20, 30]);
        rs.groups[0].reset(3);
        rs.groups[0].sel = Some(vec![0, 2]);
        rs.groups[1].cols[0] = Vector::ints(vec![1, 2]);
        rs.groups[1].reset(2);
        assert_eq!(rs.row_count(&[0, 1]), 4);
        let mut seen = Vec::new();
        rs.for_each_row(&[0, 1], |lanes| {
            let a = rs.value_at(DataPos::new(0, 0), lanes).to_owned();
            let b = rs.value_at(DataPos::new(1, 0), lanes).to_owned();
            seen.push(format!("{a}{b}"));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, ["101", "102", "301", "302"]);

        rs.groups[0].flat = Some(1);
        assert_eq!(rs.row_count(&[0, 1]), 2);
        let mut seen = Vec::new();
        rs.for_each_row(&[0, 1], |lanes| {
            seen.push(rs.value_at(DataPos::new(0, 0), lanes).to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            seen,
            [
                crate::types::CypherValue::Integer(30),
                crate::types::CypherValue::Integer(30)
            ]
        );
    }
}
