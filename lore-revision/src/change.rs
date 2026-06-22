// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bitflags::bitflags;
use lore_error_set::prelude::*;

use crate::bitflagsops;
use crate::lore::Address;
use crate::node::*;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateError;
use crate::util::path::RelativePath;

#[error_set]
pub enum ChangeError {}

/// cbindgen:prefix-with-name
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    Keep = 0,
    Add = 1,
    Delete = 2,
    Move = 3,
    Copy = 4,
}

impl FileAction {
    pub fn as_string_short(self) -> &'static str {
        match self {
            Self::Add => "A",
            Self::Delete => "D",
            Self::Move => "V",
            Self::Copy => "C",
            Self::Keep => "M",
        }
    }

    pub fn from_node_flags(flags: u16) -> Self {
        if flags & NodeFlags::StagedDelete == NodeFlags::StagedDelete {
            Self::Delete
        } else if flags & NodeFlags::StagedAdd == NodeFlags::StagedAdd {
            Self::Add
        } else if flags & NodeFlags::StagedMove == NodeFlags::StagedMove {
            Self::Move
        } else if flags & NodeFlags::StagedCopy == NodeFlags::StagedCopy {
            Self::Copy
        } else {
            Self::Keep
        }
    }
}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Flags: u16 {
        const None = 0;
        // Change is a content modification
        const Modify = 0b1;
        // Change is a merge
        const Merge = 0b10;
        // Change is a merge resulting in a conflict
        const Conflict = 0b110;
        // Change is a merge resulting in a conflict, where the conflict
        // was resolved
        const ConflictResolved = 0b1110;
        // Change is a merge resulting in a conflict, where the conflict
        // was successfully resolved in-file without any line conflicts
        const ConflictAutomerged = 0b10110;
        // Change is a merge resulting in a conflict, where the conflict
        // was successfully resolved by choosing the mine version
        const ConflictMine = 0b100110;
        // Change is a merge resulting in a conflict, where the conflict
        // was successfully resolved by choosing the theirs version
        const ConflictTheirs = 0b1000110;
        // Change is staged
        const Staged = 0b10000000;
        // Change is dirty (filesystem modification detected)
        const Dirty = 0b100000000;
    }
}
bitflagsops!(Flags, u16);

impl Flags {
    pub fn is_stage(&self) -> bool {
        self.contains(Self::Staged)
    }

    pub fn is_dirty(&self) -> bool {
        self.contains(Self::Dirty)
    }

    pub fn is_merge(&self) -> bool {
        self.contains(Self::Merge)
    }

    pub fn is_conflict(&self) -> bool {
        self.contains(Self::Conflict)
    }

    pub fn is_conflict_automerged(&self) -> bool {
        self.contains(Self::ConflictAutomerged)
    }

    pub fn is_conflict_mine(&self) -> bool {
        self.contains(Self::ConflictMine)
    }

    pub fn is_conflict_theirs(&self) -> bool {
        self.contains(Self::ConflictTheirs)
    }

    pub fn is_conflict_unresolved(&self) -> bool {
        self.contains(Self::Conflict) && !self.contains(Self::ConflictResolved)
    }
}

#[derive(Clone, Debug)]
pub struct NodeChangeState {
    pub repository: Arc<RepositoryContext>,
    pub state: Arc<State>,
    pub node: NodeID,
    pub flags: NodeFlags,
    pub address: Address,
}

impl NodeChangeState {
    pub fn invalid(&self) -> Self {
        Self {
            repository: self.repository.clone(),
            state: self.state.clone(),
            node: INVALID_NODE,
            flags: NodeFlags::NoFlags,
            address: Address::default(),
        }
    }

    /// Create a `NodeChangeState` for a child node, inheriting repository and state from parent.
    pub fn from_child(&self, child_id: NodeID, child_node: &Node) -> Self {
        Self {
            repository: self.repository.clone(),
            state: self.state.clone(),
            node: child_id,
            flags: NodeFlags::from_bits_retain(child_node.flags),
            address: child_node.address,
        }
    }

    pub async fn subtree(&self, node_id: NodeID) -> Self {
        let Ok(node) = self
            .state
            .node(self.repository.clone(), node_id)
            .await
            .internal("Node not found")
        else {
            return self.invalid();
        };
        Self {
            repository: self.repository.clone(),
            state: self.state.clone(),
            node: node_id,
            flags: NodeFlags::from_bits_retain(node.flags),
            address: node.address,
        }
    }

    pub async fn get_node(&self) -> Result<Node, StateError> {
        self.state.node(self.repository.clone(), self.node).await
    }
}

#[derive(Clone, Debug)]
pub struct NodeChange {
    pub action: FileAction,
    pub flags: Flags,
    pub from: NodeChangeState,
    pub to: NodeChangeState,
    pub path: RelativePath,
    pub from_path: Option<RelativePath>,
}

impl NodeChange {
    pub fn reverse(&mut self) {
        // Reverse add/delete/copy - other actions are transitive
        if self.action == FileAction::Delete {
            if self.flags.is_conflict() && self.to.flags.contains(NodeFlags::StagedDelete) {
                // If the change is a conflict where the "from" state is a deleted node and the "to"
                // state is staged delete, keep it as delete
            } else {
                self.action = FileAction::Add;
            }
        } else if self.action == FileAction::Add || self.action == FileAction::Copy {
            self.action = FileAction::Delete;
        } else if self.action == FileAction::Move
            && let Some(from_path) = self.from_path.take()
        {
            let path = self.path.clone();
            self.path = from_path;
            self.from_path = Some(path);
        }

        // Reverse nodes
        std::mem::swap(&mut self.from, &mut self.to);

        // Only modify flag is valid when reversing change
        if self.flags.contains(Flags::Modify) {
            self.flags = Flags::Modify;
        } else {
            self.flags = Flags::None;
        }
    }

    pub async fn is_directory(&self) -> Result<bool, StateError> {
        if self.to.node.is_valid_node_id() {
            let iblock = NodeBlock::index(self.to.node);
            let inode = Node::index(self.to.node);
            let block = self
                .to
                .state
                .block(self.to.repository.clone(), iblock)
                .await?;
            let noderef = block.node(inode);
            Ok(noderef.is_directory())
        } else {
            let iblock = NodeBlock::index(self.from.node);
            let inode = Node::index(self.from.node);
            let block = self
                .from
                .state
                .block(self.from.repository.clone(), iblock)
                .await?;
            let noderef = block.node(inode);
            Ok(noderef.is_directory())
        }
    }

    /// Translate paths from inner path inside the layer to the outer path in the main repository
    pub fn translate_from_layer_path(&mut self, inner_path: &str, outer_path: &str) {
        if self.path.as_str().starts_with(inner_path) {
            self.path = RelativePath::new_from_clean_parts(
                outer_path,
                &self.path.as_str()[inner_path.len()..],
            );
        }
        if let Some(from_path) = self.from_path.as_mut()
            && from_path.as_str().starts_with(inner_path)
        {
            *from_path = RelativePath::new_from_clean_parts(
                outer_path,
                &from_path.as_str()[inner_path.len()..],
            );
        }
    }
}

pub async fn is_conflict(
    first: &NodeChange,
    second: &NodeChange,
    path_equal: bool,
) -> Result<bool, StateError> {
    // Conflict is when both paths are the same unless one of
    // - both are directories
    // - both are deletes
    // - both are files and target hash is equal
    if first.action == FileAction::Delete && second.action == FileAction::Delete {
        return Ok(false);
    }
    let is_first_directory = first.is_directory().await?;
    let is_second_directory = second.is_directory().await?;
    if is_first_directory != is_second_directory {
        // One is a directory, on is a file. Conflicts if exact same path, or
        // if the shorter path is a file (the prerequisite for this function
        // is that the paths overlap, so if the shorter is a file it is a conflict)
        // or the shorter is a directory and it is being deleted
        if path_equal {
            return Ok(true);
        }
        if first.path.len() <= second.path.len()
            && (!is_first_directory || first.action == FileAction::Delete)
        {
            return Ok(true);
        }
        if second.path.len() <= first.path.len()
            && (!is_second_directory || second.action == FileAction::Delete)
        {
            return Ok(true);
        }
        return Ok(false);
    }
    if is_first_directory {
        // Both are directories
        return Ok(false);
    }
    if first.to.address.hash != second.to.address.hash {
        // Both are files and hashes do not match
        return Ok(true);
    }
    // Both are files and hashes match
    Ok(false)
}

pub fn sort_by_path(changes: &mut [NodeChange]) {
    changes.sort_unstable_by(|lhs, rhs| lhs.path.as_str().cmp(rhs.path.as_str()));
}

pub fn sort_conflict_by_path(conflicts: &mut [(NodeChange, NodeChange)]) {
    conflicts.sort_unstable_by(|lhs, rhs| lhs.1.path.as_str().cmp(rhs.1.path.as_str()));
}

pub fn reverse(changes: &mut [NodeChange]) {
    // Change both order and action
    let count = changes.len() / 2;
    let (first_half, second_half) = changes.split_at_mut(count);
    for (index, change) in first_half.iter_mut().enumerate() {
        change.reverse();

        // The index of the change to swap with
        let index_swap = second_half.len() - (index + 1);

        // Reverse the change to swap with
        let swap_change = &mut second_half[index_swap];
        swap_change.reverse();

        // Swap the changes
        std::mem::swap(change, swap_change);
    }

    // Reverse the middle change if not iterated yet
    if (first_half.len() % 2) != (second_half.len() % 2) {
        second_half[0].reverse();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_flag_exists_and_is_independent() {
        let dirty = Flags::Dirty;
        let staged = Flags::Staged;
        // Dirty and Staged don't overlap
        assert_eq!(dirty & staged, Flags::None);
    }

    #[test]
    fn is_dirty_method() {
        let flags = Flags::Dirty;
        assert!(flags.is_dirty());
        assert!(!flags.is_stage());

        let flags = Flags::Staged;
        assert!(!flags.is_dirty());
        assert!(flags.is_stage());

        let flags = Flags::Dirty | Flags::Staged;
        assert!(flags.is_dirty());
        assert!(flags.is_stage());
    }

    #[test]
    fn dirty_and_modify_combine() {
        let flags = Flags::Dirty | Flags::Modify;
        assert!(flags.is_dirty());
        assert!(flags.contains(Flags::Modify));
        assert!(!flags.is_stage());
    }
}
