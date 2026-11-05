// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The refusal rows: a section's `Refused` record read, under its lock, what the model holds at its
//! `seq`, and its reason follows from that read and from what it expected. The record is judged,
//! not the reason its operation reports. An in-place or owned change refused by its closure, which
//! the kind decides, is held to its guard having passed; a full name table, to its read alone. Each
//! row reads the word its section reads: the node's own, or the directory it changes, its parent
//! where it is one.
//!
//! A read equals the model's because every write of a word, a count, a chain or a holder takes
//! the write lock of the block it changes and its `seq` after it, and every refusal is recorded
//! under the locks covering its read. The discard's unlink, which rewrites its link in one
//! section and records its `Unlink` in the next, is the exception, sound only as the discard runs
//! under the exclusive claim, where no refusal reads beside it.
//!
//! An `IfUnchanged` detach tests the count of the directory it detaches, which is the directory
//! it reads: the record carries no other.

use lore_revision::node::Node;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeID;
use lore_revision::state::node::hooks::log::Expected;
use lore_revision::state::node::hooks::log::Read;
use lore_revision::state::node::hooks::log::Reason;
use lore_revision::state::node::hooks::log::Words;

use super::Replay;
use super::Status;
use super::fields::Fields;
use super::names_node;
use super::split;

impl Replay<'_> {
    /// A refusal of `node` for `reason`, on `read`, the word or count of `of`, the node or the
    /// directory the decision read.
    pub(super) fn refused(
        &mut self,
        node: NodeID,
        of: NodeID,
        reason: Reason,
        read: &Read,
        expected: Expected,
    ) {
        let read_as_held = match *read {
            Read::Word(Words(word)) => self.read_word(of, reason, &word),
            Read::Count { changes, instance } => self.read_count(of, reason, changes, instance),
            Read::Holder { holder, name_hash } => {
                self.read_holder(node, of, reason, (holder, name_hash))
            }
            Read::Head(head) => self.read_head(of, reason, head),
            Read::NameTableFull => true,
        };
        if !read_as_held {
            return;
        }
        // A refusal on its parent's word reads the parent it still hangs from.
        let parent_read = matches!(
            reason,
            Reason::Restore | Reason::ServedRewrite | Reason::NotFound | Reason::StagedDelete
        ) && node != of;
        if let Some(slot) = self
            .state
            .slot(node)
            .filter(|slot| slot.placed && parent_read)
            && slot.record.parent != of
        {
            let parent = slot.record.parent;
            self.fail(format!(
                "a {reason:?} refusal of {node} read {of}, where the model has it below {parent}"
            ));
            return;
        }
        if let Some(why) = unfounded(node == of, reason, read, expected) {
            self.fail(format!(
                "a {reason:?} refusal of {node} its read does not explain: {why}"
            ));
        }
    }

    /// Whether `word`, the word a refusal read of `of`, is the model's: every field a `Word`
    /// compares, the parent and name hash where the model knows them.
    fn read_word(&mut self, of: NodeID, reason: Reason, word: &Node) -> bool {
        let Some((held, placed)) = self.state.slot(of).map(|slot| (slot.record, slot.placed))
        else {
            self.fail(format!(
                "a {reason:?} refusal read {of}, which the model has no image of"
            ));
            return false;
        };
        let mut differs = Fields::changed((&held, false), (word, false));
        // A test's own edit of a parent or name hash makes no structural record.
        if !placed || self.admit_unlabelled {
            differs = differs & !(Fields::PARENT | Fields::NAME_HASH);
        }
        if differs.is_empty() {
            return true;
        }
        self.fail(format!(
            "a {reason:?} refusal read {of}'s {differs:?} otherwise than the model holds it"
        ));
        false
    }

    /// Whether `changes` and `instance`, the count a refusal read of directory `of` and its
    /// block's instance, are the model's.
    fn read_count(&mut self, of: NodeID, reason: Reason, changes: u32, instance: u64) -> bool {
        let held = self.state.slot(of).map(|slot| slot.changes);
        let held_instance = self.state.block(split(of).0).map(|block| block.instance);
        if held == Some(changes) && held_instance == Some(instance) {
            return true;
        }
        self.fail(format!(
            "a {reason:?} refusal read {of}'s count {changes} in instance {instance}, where the \
             model holds {held:?} in {held_instance:?}"
        ));
        false
    }

    /// Whether `holder`, the node a refusal of `node` read holding the name hashing to
    /// `name_hash` in directory `of`, holds it there: another node in `of`'s chain, not Discarded,
    /// carrying the hash.
    fn read_holder(
        &mut self,
        node: NodeID,
        of: NodeID,
        reason: Reason,
        (holder, name_hash): (NodeID, u64),
    ) -> bool {
        // An attach's slot in flight looked its own name up.
        let own_name = self
            .state
            .slot(node)
            .filter(|slot| slot.status == Status::InFlight && slot.placed)
            .is_none_or(|slot| slot.record.name_hash == name_hash);
        let holds = own_name
            && holder != node
            && self.state.slot(holder).is_some_and(|slot| {
                slot.status == Status::Chained
                    && slot.chained_in == Some(of)
                    && !slot.is_discarded()
                    && slot.record.name_hash == name_hash
            });
        if !holds {
            self.fail(format!(
                "a {reason:?} refusal of {node} named {holder} holding the name {name_hash:x} in \
                 {of}, which does not hold it there"
            ));
        }
        holds
    }

    /// Whether `head`, the head a refusal read of directory `of`'s chain, is the model's. Where a
    /// free reached the chain since `of` was detached, the State's `child` may still name a slot
    /// a grab took elsewhere: any head is the State's.
    fn read_head(&mut self, of: NodeID, reason: Reason, head: NodeID) -> bool {
        if self.state.slot(of).is_some_and(|slot| slot.chain_freed) {
            return true;
        }
        let front = self
            .state
            .chains
            .get(&of)
            .and_then(|chain| chain.front().copied());
        let agrees = if names_node(head) {
            front == Some(head)
        } else {
            front.is_none()
        };
        if !agrees {
            self.fail(format!(
                "a {reason:?} refusal read {of}'s head {head}, where the model's chain begins at \
                 {front:?}"
            ));
        }
        agrees
    }
}

/// Why `reason` does not follow from `read`, the node's own word where `own`, else the word of
/// the directory it changes, and from `expected`, if it does not.
fn unfounded(own: bool, reason: Reason, read: &Read, expected: Expected) -> Option<&'static str> {
    const OTHER_KIND: &str = "a read of another kind than its reason decides on";
    let refuses = match (reason, read, expected) {
        (Reason::IfUnchanged, &Read::Count { changes, instance }, Expected::Count { .. }) => {
            own && expected != Expected::Count { changes, instance }
        }
        (Reason::IfEmpty, &Read::Head(head), Expected::Head(as_read)) if as_read == head => {
            own && names_node(head)
        }
        // The holder is the read's to hold.
        (Reason::Exists, Read::Holder { .. }, _)
        | (Reason::NameTableFull, Read::NameTableFull, _) => true,
        (_, Read::Word(Words(word)), _) => match refuses_on(own, reason, word, expected) {
            Some(refuses) => refuses,
            None => return Some(OTHER_KIND),
        },
        _ => return Some(OTHER_KIND),
    };
    (!refuses).then_some("its condition did not hold")
}

/// Whether `reason`'s row holds of `word`, the node's own where `own`, else its directory's, as
/// `expected` says; `None` where the row reads no word.
fn refuses_on(own: bool, reason: Reason, word: &Node, expected: Expected) -> Option<bool> {
    let discarded = word.is_discarded();
    let staged_delete = word.is_staged_delete();
    let elsewhere = match expected {
        Expected::At { parent, name_hash } => word.parent != parent || word.name_hash != name_hash,
        _ => false,
    };
    // The delete level tests the parent alone: its record's name hash is the word's own.
    let moved = matches!(expected, Expected::At { parent, .. } if word.parent != parent);
    let other = discarded || moved || word.is_staged_add();
    Some(match reason {
        // Rows on the directory's word.
        Reason::StagedDelete => !own && !discarded && staged_delete,
        Reason::NotDirectory => !own && !word.is_directory(),
        // Rows on the node's own word, or on its directory's.
        Reason::Unmovable if own => !discarded && staged_delete,
        Reason::Restore if own => discarded || !staged_delete,
        // A directory left or joined, or the parent restored below.
        Reason::Unmovable | Reason::Restore => discarded || staged_delete,
        Reason::ServedRewrite if own => discarded || !word.is_file(),
        // No section's `NotFound` tests the parent's kind: one under a file is refused elsewhere.
        // A served rewrite's parent is refused where it is Discarded alone.
        Reason::NotFound | Reason::ServedRewrite => discarded,
        // Rows on the node's own word alone.
        _ if !own => false,
        Reason::IfClean => word.is_dirty(),
        // Of a live node; a respell within its directory moves anything.
        Reason::NotLocal => {
            !discarded
                && word.is_directory()
                && !word.ext_flags().contains(NodeExtFlags::LocalContent)
                && elsewhere
        }
        Reason::At => elsewhere,
        Reason::NotLink => !discarded && !word.is_link(),
        Reason::Rewrite => discarded || !word.is_file() || staged_delete,
        // Tested once `Other`'s conditions failed.
        Reason::DeleteLevelStaged => staged_delete && !other,
        Reason::DeleteLevelOther => other,
        Reason::InPlaceAway => discarded || elsewhere,
        // A closure declining, the kind's to judge, once the guard passed.
        Reason::InPlace => !(discarded || elsewhere),
        Reason::OwnedGone => discarded,
        Reason::Owned => !discarded,
        Reason::IfUnchanged | Reason::IfEmpty | Reason::Exists | Reason::NameTableFull => {
            return None;
        }
    })
}
