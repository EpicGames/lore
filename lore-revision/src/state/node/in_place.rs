// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Changes of a node's own fields in place, under its block's lock.

use std::ops::Deref;
use std::sync::Arc;

use crate::errors::InvalidArguments;
use crate::errors::NodeNotFound;
use crate::lore::Address;
use crate::lore::Hash;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateError;
#[cfg(feature = "state-hooks")]
use crate::state::node::hooks::log;

/// A node's own fields as a change in place may set them ([`State::node_change_in_place`]): its
/// content, its merge resolution, and its marks where that leaves them consistent. It reads as
/// the record it wraps. It cannot change the node's kind, place, name or chain, nor make it
/// Discarded or dirty, so no chain, count or ancestor's mark rests on what it changed.
pub struct NodeContent<'a> {
    node: &'a mut Node,
}

impl Deref for NodeContent<'_> {
    type Target = Node;

    #[inline]
    fn deref(&self) -> &Node {
        self.node
    }
}

impl NodeContent<'_> {
    /// Sets a file's content: its address, size and mode. An address with no context keeps the
    /// node's own, its file ID.
    #[inline]
    pub fn set_content(&mut self, address: Address, size: u64, mode: u16) {
        let file_id = self.node.address.context;
        self.node.address = address;
        if self.node.address.context.is_zero() {
            self.node.address.context = file_id;
        }
        self.node.size = size;
        self.node.mode = mode;
    }

    /// Sets a node's content as another record holds it: its whole address, the file ID in its
    /// context included, its size and its mode.
    #[inline]
    pub fn overwrite_content(&mut self, address: Address, size: u64, mode: u16) {
        self.node.address = address;
        self.node.size = size;
        self.node.mode = mode;
    }

    /// Sets the node's size alone.
    #[inline]
    pub fn set_size(&mut self, size: u64) {
        self.node.size = size;
    }

    /// Sets the node's mode alone.
    #[inline]
    pub fn set_mode(&mut self, mode: u16) {
        self.node.mode = mode;
    }

    /// Sets a directory's content as a rehash derives it from its children: the hash of its
    /// address, whose context stays, and its size.
    #[inline]
    pub fn set_hash_and_size(&mut self, hash: Hash, size: u64) {
        self.node.address.hash = hash;
        self.node.size = size;
    }

    /// Zeroes a file's legacy V0 modification time, which its `child` held; answers false,
    /// changing nothing, on a directory or a link, whose `child` is a chain head or the node a link
    /// exposes.
    #[inline]
    pub fn clear_legacy_mtime(&mut self) -> bool {
        if !self.node.is_file() {
            return false;
        }
        self.node.child = 0;
        true
    }

    /// Drops the node's staged flags, its merge flags with them ([`Node::clear_staged_flags`]), as
    /// an unstage does; the caller keeps the staged marks of the directories above consistent.
    #[inline]
    pub fn clear_staged_flags(&mut self) {
        self.node.clear_staged_flags();
    }

    /// Drops the node's staged bits, its staged action and merge flags among them, and keeps its
    /// `Dirty` bit alone: exactly `flags &= !StagedBits`, as a commit leaves a node it committed.
    /// A dirty action goes too, as the halves share the action bits, where
    /// [`Self::clear_staged_flags`] keeps them on a dirty node.
    #[inline]
    pub fn clear_staged_and_action_bits(&mut self) {
        self.node.flags &= !NodeFlags::StagedBits.bits();
    }

    /// Drops a file's or link's every change flag and its extension flags
    /// ([`Node::clear_all_change_flags`]), as a commit leaves one it committed; answers false,
    /// changing nothing, on a directory, as [`Self::clear_dirty_flags`] does.
    #[inline]
    pub fn clear_change_flags(&mut self) -> bool {
        if self.node.is_directory() {
            return false;
        }
        self.node.clear_all_change_flags();
        true
    }

    /// Marks a staged merge conflict resolved, or unresolved again, dropping the side a resolve
    /// took; answers false, changing nothing, on a node that is no staged merge conflict.
    #[inline]
    pub fn set_merge_resolved(&mut self, resolved: bool) -> bool {
        if !self.node.is_staged_merge_conflict() {
            return false;
        }
        let bit = (NodeFlags::StagedMergeResolved ^ NodeFlags::StagedMergeConflict).bits();
        if resolved {
            self.node.flags |= bit;
        } else {
            let side = ((NodeFlags::StagedMergeMine | NodeFlags::StagedMergeTheirs)
                ^ NodeFlags::StagedMerge)
                .bits();
            self.node.flags &= !(bit | side);
        }
        true
    }

    /// Drops a file's or link's dirty flags and extension flags ([`Node::clear_dirty_flags`]);
    /// answers false, changing nothing, on a directory, whose clears keep the rule that no clean
    /// directory holds a dirty child (`State::node_clear`, `ServedNodes::node_clear`).
    #[inline]
    pub fn clear_dirty_flags(&mut self) -> bool {
        if self.node.is_directory() {
            return false;
        }
        self.node.clear_dirty_flags();
        true
    }

    /// Replaces the action of a node already dirty with `action`'s action bits, none for none;
    /// answers false, changing nothing, on a clean node, which only a mark makes dirty.
    #[inline]
    pub fn replace_dirty_action(&mut self, action: NodeFlags) -> bool {
        if !self.node.is_dirty() {
            return false;
        }
        self.node.flags &= !NodeFlags::ActionBits.bits();
        self.node.flags |= (action & NodeFlags::ActionBits).bits();
        true
    }
}

/// The pin a link records ([`State::node_set_link_target`]): the revision of the linked repository
/// in its address's hash, and the node of the linked State it exposes in its `child`.
#[derive(Clone, Copy, Debug)]
pub enum LinkPin {
    /// The whole pin: the address, its context included, and the node exposed.
    Whole { address: Address, child: NodeID },
    /// The linked revision alone: the address's context and the node exposed stay.
    Revision(Hash),
}

impl State {
    /// Sets the target of link `node_id` as `target` says, under its block's write lock. Refuses
    /// a node that is not a link, and answers [`NodeNotFound`] for a detached one. Registers the
    /// block where it turned dirty, as every writer does: a save's release leaves a block it
    /// wrote clean and unregistered, so a repin after it registers the block again, and keeps one
    /// edited since its image dirty and registered, the State marked.
    pub async fn node_set_link_target(
        &self,
        repository: &Arc<RepositoryContext>,
        node_id: NodeID,
        target: LinkPin,
    ) -> Result<(), StateError> {
        let block_index = NodeBlock::index(node_id);
        let block = match self.resident_block(block_index) {
            Some(block) => block,
            None => self.block(repository.clone(), block_index).await?,
        };
        self.node_set_link_target_in(&block, node_id, target)
    }

    /// [`Self::node_set_link_target`] for a caller holding `block`, the link's block: reads and
    /// awaits nothing.
    pub fn node_set_link_target_in(
        &self,
        block: &Arc<NodeBlock>,
        node_id: NodeID,
        target: LinkPin,
    ) -> Result<(), StateError> {
        let dirtied = {
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::Repin);
            let mut block_writer = block.write();
            let node = block_writer.node(Node::index(node_id));
            if node.is_discarded() {
                #[cfg(feature = "state-hooks")]
                hook_refused(
                    self,
                    node_id,
                    log::Reason::NotFound,
                    node,
                    log::Expected::Nothing,
                );
                return Err(NodeNotFound.into());
            }
            if !node.is_link() {
                #[cfg(feature = "state-hooks")]
                hook_refused(
                    self,
                    node_id,
                    log::Reason::NotLink,
                    node,
                    log::Expected::Nothing,
                );
                drop(block_writer);
                return Err(StateError::from(InvalidArguments {
                    reason: format!("node {node_id} is not a link"),
                }));
            }
            match target {
                LinkPin::Whole { address, child } => {
                    node.address = address;
                    node.child = child;
                }
                LinkPin::Revision(hash) => node.address.hash = hash,
            }
            #[cfg(feature = "state-hooks")]
            log::record(
                &self.hooks,
                log::Effect::Repin {
                    node: node_id,
                    child: node.child,
                    whole: matches!(target, LinkPin::Whole { .. }),
                },
            );
            block_writer.mark_dirty()
        };
        if dirtied {
            self.block_modified(block.clone(), NodeBlock::index(node_id));
        }
        Ok(())
    }

    /// Runs `change` on node `node_id` under the write lock of `block`, the node's block, where the
    /// node is live and still the child of `parent` named under `name_hash`, as a request racing
    /// the caller may have moved or removed it. Answers whether `change` accepted the node, whose
    /// block is then recorded changed; a `change` that answers false has changed nothing. Reads
    /// and awaits nothing, so a caller may hold a lock of its own across it.
    pub fn node_change_in_place(
        &self,
        block: &Arc<NodeBlock>,
        node_id: NodeID,
        (parent, name_hash): (NodeID, u64),
        change: impl FnOnce(&mut NodeContent<'_>) -> bool,
    ) -> bool {
        let dirtied = {
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::InPlace);
            #[cfg(feature = "state-hooks")]
            let expected = log::Expected::At { parent, name_hash };
            let mut block_writer = block.write();
            let node = block_writer.node(Node::index(node_id));
            if node.is_discarded() || node.parent != parent || node.name_hash != name_hash {
                #[cfg(feature = "state-hooks")]
                hook_refused(self, node_id, log::Reason::InPlaceAway, node, expected);
                return false;
            }
            if !change(&mut NodeContent { node }) {
                #[cfg(feature = "state-hooks")]
                hook_refused(
                    self,
                    node_id,
                    log::Reason::InPlace,
                    block_writer.node(Node::index(node_id)),
                    expected,
                );
                return false;
            }
            #[cfg(feature = "state-hooks")]
            log::record(
                &self.hooks,
                log::Effect::Admitted {
                    node: node_id,
                    expected,
                },
            );
            block_writer.mark_dirty()
        };
        if dirtied {
            self.block_modified(block.clone(), NodeBlock::index(node_id));
        }
        true
    }
}

/// Which of its own flags [`State::node_clear_own`] drops from a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnFlags {
    /// Its staged flags, its merge flags with them ([`Node::clear_staged_flags`]), the action bits
    /// staying where it is dirty.
    Staged,
    /// Its dirty and extension flags ([`Node::clear_dirty_flags`]), the action bits staying where
    /// it is staged.
    Dirty,
    /// Every change flag and its extension flags ([`Node::clear_all_change_flags`]).
    Change,
}

impl State {
    /// [`Self::node_change_in_place`] for a caller beside which no task frees, recycles or moves
    /// the node, so it stands where the caller read it: only that the node is live is checked.
    /// Other tasks may add, rename and mark other nodes meanwhile.
    pub fn node_change_owned(
        &self,
        block: &Arc<NodeBlock>,
        node_id: NodeID,
        change: impl FnOnce(&mut NodeContent<'_>) -> bool,
    ) -> bool {
        #[cfg(feature = "state-hooks")]
        assert!(!self.is_served(), "an owned change of a served State");
        let dirtied = {
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::Owned);
            let mut block_writer = block.write();
            let node = block_writer.node(Node::index(node_id));
            if node.is_discarded() {
                #[cfg(feature = "state-hooks")]
                hook_refused(
                    self,
                    node_id,
                    log::Reason::OwnedGone,
                    node,
                    log::Expected::Nothing,
                );
                return false;
            }
            if !change(&mut NodeContent { node }) {
                #[cfg(feature = "state-hooks")]
                hook_refused(
                    self,
                    node_id,
                    log::Reason::Owned,
                    block_writer.node(Node::index(node_id)),
                    log::Expected::Nothing,
                );
                return false;
            }
            block_writer.mark_dirty()
        };
        if dirtied {
            self.block_modified(block.clone(), NodeBlock::index(node_id));
        }
        true
    }

    /// Drops live node `node_id`'s own `flags`, held in `block`, whatever its kind, a directory's
    /// included, and nothing above it, for a caller keeping dirty closure itself (C9), beside
    /// which no task frees, recycles or moves the node. A directory's dirty flags go whatever its
    /// children hold, which no served change may do, the settle's clear excepted
    /// ([`ServedNodes::node_clear`]). Answers whether a flag changed, the block then recorded
    /// changed; a Discarded node changes nothing.
    ///
    /// [`ServedNodes::node_clear`]: crate::state::ServedNodes::node_clear
    pub fn node_clear_own(&self, block: &Arc<NodeBlock>, node_id: NodeID, flags: OwnFlags) -> bool {
        #[cfg(feature = "state-hooks")]
        assert!(!self.is_served(), "an owned clear of a served State");
        let dirtied = {
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::Owned);
            let mut block_writer = block.write();
            let node = block_writer.node(Node::index(node_id));
            if node.is_discarded() {
                #[cfg(feature = "state-hooks")]
                hook_refused(
                    self,
                    node_id,
                    log::Reason::OwnedGone,
                    node,
                    log::Expected::Nothing,
                );
                return false;
            }
            let was = (node.flags, node.ext_flags);
            match flags {
                OwnFlags::Staged => node.clear_staged_flags(),
                OwnFlags::Dirty => node.clear_dirty_flags(),
                OwnFlags::Change => node.clear_all_change_flags(),
            }
            if (node.flags, node.ext_flags) == was {
                #[cfg(feature = "state-hooks")]
                hook_refused(
                    self,
                    node_id,
                    log::Reason::Owned,
                    node,
                    log::Expected::Nothing,
                );
                return false;
            }
            block_writer.mark_dirty()
        };
        if dirtied {
            self.block_modified(block.clone(), NodeBlock::index(node_id));
        }
        true
    }
}

/// Under `state-hooks`, the log's record of a change in place refused on `read`, the node's own
/// word.
#[cfg(feature = "state-hooks")]
fn hook_refused(
    state: &State,
    node: NodeID,
    reason: log::Reason,
    read: &Node,
    expected: log::Expected,
) {
    log::record(
        &state.hooks,
        log::Effect::Refused {
            node,
            parent: node,
            reason,
            read: log::Read::Word(log::Words(*read)),
            expected,
        },
    );
}

/// The accessors, wrappers and constructors a `test-util` build adds to this module's types, for
/// tests.
#[cfg(feature = "test-util")]
mod test_util {
    use super::*;

    impl State {
        /// For tests: runs `edit` on node `node_id`, held in `block`, under the block's write lock,
        /// whatever it changes, and records the block changed, as a fixture builds a record no
        /// primitive would make.
        pub fn node_edit_for_tests(
            &self,
            block: &Arc<NodeBlock>,
            node_id: NodeID,
            edit: impl FnOnce(&mut Node),
        ) {
            let dirtied = {
                let mut block_writer = block.write();
                edit(block_writer.node(Node::index(node_id)));
                block_writer.mark_dirty()
            };
            if dirtied {
                self.block_modified(block.clone(), NodeBlock::index(node_id));
            }
        }
    }
}
