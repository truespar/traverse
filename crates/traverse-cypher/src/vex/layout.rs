//! Where each variable lives at a point of the pipeline.
//!
//! A [`Layout`] is the planner's view of the result set after one operator: variable name to
//! [`DataPos`], and which groups exist with how many columns. Operators are built with the
//! positions resolved from their input layout; a row-consuming operator (projection, aggregation,
//! sort, join, write) places its outputs in a fresh group, so a variable's position can change
//! along the pipeline. Group ids are allocated once per plan through [`GroupAlloc`] and never
//! reused, so every group of a plan has its own slot in the result set.

use rustc_hash::FxHashMap;

use super::chunk::{DataPos, GroupId};

/// Physical type the planner knows for a variable; `Any` when it doesn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotType {
    /// Node reference.
    Node,
    /// Relationship reference.
    Edge,
    /// Path value.
    Path,
    /// List value.
    List,
    /// Float.
    Float,
    /// Integer.
    Int,
    /// Unknown or mixed.
    Any,
}

/// Group ids of one plan, handed out in lowering order.
#[derive(Debug, Default)]
pub struct GroupAlloc {
    widths: Vec<u16>,
}

impl GroupAlloc {
    /// No groups yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A new empty group.
    pub fn new_group(&mut self) -> GroupId {
        let id = self.widths.len() as GroupId;
        self.widths.push(0);
        id
    }

    /// Allocates a column in `group`.
    pub fn add_col(&mut self, group: GroupId) -> u16 {
        let w = &mut self.widths[group as usize];
        let col = *w;
        *w += 1;
        col
    }

    /// Columns per group, for [`super::chunk::ResultSet::new`].
    pub fn widths(&self) -> &[u16] {
        &self.widths
    }

    /// Groups allocated.
    pub fn group_count(&self) -> usize {
        self.widths.len()
    }
}

/// Variable placement after one operator; see the module docs.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    vars: FxHashMap<String, (DataPos, SlotType)>,
    /// Names in binding order, for stable output ordering.
    order: Vec<String>,
    /// Groups flattened below this point.
    flat: Vec<GroupId>,
}

impl Layout {
    /// No variables.
    pub fn new() -> Self {
        Self::default()
    }

    /// Position of `name`, if bound.
    pub fn pos(&self, name: &str) -> Option<DataPos> {
        self.vars.get(name).map(|(p, _)| *p)
    }

    /// Type of `name`, if bound.
    pub fn ty(&self, name: &str) -> Option<SlotType> {
        self.vars.get(name).map(|(_, t)| *t)
    }

    /// Is `name` bound?
    pub fn has(&self, name: &str) -> bool {
        self.vars.contains_key(name)
    }

    /// Binds `name` at `pos`; a rebinding replaces the old position.
    pub fn bind(&mut self, name: &str, pos: DataPos, ty: SlotType) {
        if self.vars.insert(name.to_string(), (pos, ty)).is_none() {
            self.order.push(name.to_string());
        }
    }

    /// Binds `name` in a new column of `group`.
    pub fn bind_in(
        &mut self,
        alloc: &mut GroupAlloc,
        name: &str,
        group: GroupId,
        ty: SlotType,
    ) -> DataPos {
        let col = alloc.add_col(group);
        let pos = DataPos::new(group, col);
        self.bind(name, pos, ty);
        pos
    }

    /// Removes `name`.
    pub fn unbind(&mut self, name: &str) {
        if self.vars.remove(name).is_some() {
            self.order.retain(|n| n != name);
        }
    }

    /// Every bound variable in binding order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.order.iter().map(String::as_str)
    }

    /// Every binding in binding order.
    pub fn bindings(&self) -> impl Iterator<Item = (&str, DataPos, SlotType)> {
        self.order.iter().map(move |n| {
            let (p, t) = self.vars[n];
            (n.as_str(), p, t)
        })
    }

    /// Bound variables.
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    /// No variables?
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// Is `group` flattened at this point?
    pub fn is_flat(&self, group: GroupId) -> bool {
        self.flat.contains(&group)
    }

    /// Records that `group` is flattened from here on.
    pub fn mark_flat(&mut self, group: GroupId) {
        if !self.flat.contains(&group) {
            self.flat.push(group);
        }
    }

    /// The unflat groups among the groups of `positions`, ascending, each once.
    pub fn unflat_among(&self, positions: &[DataPos]) -> Vec<GroupId> {
        let mut gs: Vec<GroupId> = positions
            .iter()
            .map(|p| p.group)
            .filter(|g| !self.is_flat(*g))
            .collect();
        gs.sort_unstable();
        gs.dedup();
        gs
    }

    /// The unflat groups any variable lives in, ascending.
    pub fn unflat_groups(&self) -> Vec<GroupId> {
        self.groups()
            .into_iter()
            .filter(|g| !self.is_flat(*g))
            .collect()
    }

    /// The groups any variable lives in, ascending, each once.
    pub fn groups(&self) -> Vec<GroupId> {
        let mut gs: Vec<GroupId> = self.vars.values().map(|(p, _)| p.group).collect();
        gs.sort_unstable();
        gs.dedup();
        gs
    }

    /// A layout with every variable moved into the fresh group `group` (in binding order), as a
    /// row-consuming operator produces. Returns the new layout and the `(old, new)` positions.
    pub fn flattened_into(
        &self,
        alloc: &mut GroupAlloc,
        group: GroupId,
    ) -> (Layout, Vec<(DataPos, DataPos)>) {
        let mut out = Layout::new();
        let mut moves = Vec::with_capacity(self.vars.len());
        for name in &self.order {
            let (old, ty) = self.vars[name];
            let new = out.bind_in(alloc, name, group, ty);
            moves.push((old, new));
        }
        (out, moves)
    }
}
