// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The conditional successes and the clear's last phase: a change decided on a condition is a
//! violation where the condition of its refusal held, by the model, at its record, for the
//! conditions its primitive tests, on the record's own words. A publish and a move check their name
//! and count, a detach its mode's condition and its parent, a section's `Word` the node it writes;
//! a change whose records say nothing of its condition records it as `Admitted`, which the `Word`
//! of an in-place change, a delete level's mark and a settle's first phase needs, and a settle's
//! clear. A served mark writes a live node below a live parent, an unserved mark's dirty half no
//! Discarded node. A settle's clear clears only where the pending clear its own first phase set is
//! still in force and its directory's count unchanged since. A served move of an added node over a
//! holder the revision holds takes the holder's path over, its actions no longer an add's (H20), as
//! its word holds once its section ends: the product writes no `Word` where the bits stay.
//!
//! Each rule reads the model before the record is applied, which is what the section read under
//! its locks, as `refusals` argues; the discard's unlink, one section after the link it rewrote,
//! is the exception there too, sound only under the exclusive claim.
//!
//! A name checked free is unique in its directory on a served State, whose names are unique by
//! hash (C8), and on an unserved one for a publish after a lookup's unchanged count. An unserved
//! State's adds may duplicate a name, which a walk taking the mover's own name as free, or a log
//! whose newest patch of the name is a duplicate's detach, does not see.

use lore_revision::node::Node;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::node::hooks::log::Check;
use lore_revision::state::node::hooks::log::Expected;
use lore_revision::state::node::hooks::log::How;
use lore_revision::state::node::hooks::log::Kind;
use lore_revision::state::node::hooks::log::Section;

use super::Replay;
use super::StateModel;
use super::split;

/// The flags a node's word must keep or take as its section writes them, here the action and
/// dirty bits of a served move's mover (H20).
const MOVED_BITS: u16 = NodeFlags::ActionBits.bits() | NodeFlags::Dirty.bits();

/// A served move's mover and the action and dirty bits its word must hold once its section ends,
/// found at the move's record.
#[derive(Clone, Copy, Debug)]
pub(super) struct MovedBits {
    pub(super) state: u64,
    pub(super) seq: u64,
    pub(super) sub: u32,
    mover: NodeID,
    bits: u16,
}

impl MovedBits {
    /// Why the mover's word in `state` does not hold the bits, if it does not.
    pub(super) fn unheld(&self, state: &StateModel) -> Option<String> {
        let held = state.slot(self.mover)?.record.flags & MOVED_BITS;
        (held != self.bits).then(|| {
            format!(
                "a served move of {} left its actions and dirty bits {held:#x}, where it moves to \
                 take {:#x} (H20)",
                self.mover, self.bits
            )
        })
    }
}

impl Replay<'_> {
    /// The word in force of `node`, where the model holds it.
    fn word_of(&self, node: NodeID) -> Option<Node> {
        self.state.slot(node).map(|slot| slot.record)
    }

    /// The word in force of the parent `of` hangs from, but for the root.
    fn parent_of(&self, of: &Node) -> Option<Node> {
        self.word_of(of.parent).filter(|_| of.parent != ROOT_NODE)
    }

    /// Whether the count of directory `of` and its block's instance are `expected`'s, where it is
    /// a count; failing where not, naming `what`.
    pub(super) fn counted(&mut self, of: NodeID, expected: Expected, what: &str) {
        let Expected::Count { changes, instance } = expected else {
            return;
        };
        let held = self.state.slot(of).map(|slot| slot.changes);
        let held_instance = self.state.block(split(of).0).map(|block| block.instance);
        if held != Some(changes) || held_instance != Some(instance) {
            self.fail(format!(
                "{what} on {of}'s count {changes} in instance {instance}, where the model holds \
                 {held:?} in {held_instance:?}"
            ));
        }
    }

    /// A publish of `node` below `parent`, as `check` checked it against `expected`: the parent
    /// live, for a C API add not staged for deletion, and a directory where the check tests it;
    /// a lookup's count, unchanged, and the name it found free still free. A served name is
    /// unique whatever the check (C8).
    pub(super) fn published_on(
        &mut self,
        (node, parent): (NodeID, NodeID),
        name_hash: u64,
        (check, expected): (Check, Expected),
    ) {
        let checked = matches!(check, Check::Unchanged | Check::Absent | Check::Logged);
        if let Some(word) = self.word_of(parent) {
            if word.is_discarded() {
                self.fail(format!("{node} published below Discarded {parent}"));
            } else if checked && Kind::of(&word) != Kind::Directory {
                self.fail(format!("{node} published below {parent}, no directory"));
            } else if check == Check::Unstaged && word.is_staged_delete() {
                self.fail(format!(
                    "{node} published by a C API add below {parent}, staged for deletion"
                ));
            }
        }
        if check == Check::Unchanged {
            self.counted(parent, expected, "a publish checked unchanged");
        }
        if self.state.served || check == Check::Unchanged {
            self.unique(parent, name_hash, node);
        }
    }

    /// A move of `mover` into `to`, as `check` checked it against `expected`, before the model
    /// applies it: a mover not Discarded; a checked move's `to` a live directory, and the count
    /// it compared unchanged. Whether its name is unique in `to` but for the mover: a served
    /// one's (C8).
    pub(super) fn moving(
        &mut self,
        mover: NodeID,
        to: NodeID,
        check: Check,
        expected: Expected,
    ) -> bool {
        if self.word_of(mover).is_some_and(|word| word.is_discarded()) {
            self.fail(format!("a move of Discarded {mover}"));
        }
        let checked = check != Check::Unchecked;
        if checked
            && self
                .word_of(to)
                .is_some_and(|word| word.is_discarded() || Kind::of(&word) != Kind::Directory)
        {
            self.fail(format!(
                "a checked move of {mover} into {to}, no live directory"
            ));
        }
        if matches!(check, Check::Walk | Check::Replacing) {
            self.counted(to, expected, "a move checked unchanged");
        }
        self.state.served
    }

    /// A rename in place of `node`, `was` its word in force: of a node not Discarded.
    pub(super) fn renaming(&mut self, node: NodeID, was: &Node) {
        if was.is_discarded() {
            self.fail(format!("a rename of Discarded {node}"));
        }
    }

    /// Under a served move of `mover` over `holder`, the action and dirty bits the mover's word
    /// must hold once the section ends, as the reattach writes them: a directory moved clean
    /// takes `Dirty` alone; an added mover over a holder carrying no add takes the holder's path
    /// over, a file's action a modify, a directory's none (H20); otherwise its bits stay.
    pub(super) fn moved_bits(&mut self, mover: NodeID, holder: Option<NodeID>) {
        if !self.state.served {
            return;
        }
        let Some(mut word) = self.word_of(mover) else {
            return;
        };
        let holder_added = holder
            .and_then(|holder| self.word_of(holder))
            .is_none_or(|held| held.is_dirty_add());
        if word.is_directory() && !word.is_dirty() {
            word.flags &= !NodeFlags::DirtyBits.bits();
            word.flags |= NodeFlags::Dirty.bits();
        }
        if !holder_added && word.is_dirty_add() {
            word.flags &= !NodeFlags::ActionBits.bits();
            if word.is_file() {
                word.flags |= NodeFlags::DirtyModify.bits();
            }
        }
        *self.moved_bits = Some(MovedBits {
            state: self.record.state,
            seq: self.record.seq,
            sub: self.record.sub,
            mover,
            bits: word.flags & MOVED_BITS,
        });
    }

    /// An unlink of `node` from `parent`, `how` it was decided against `expected`, before the
    /// model applies it: a detach below a parent not Discarded, as each detach tests; an `IfClean`
    /// one of a clean node, or data is lost (H16, H22); `IfUnchanged`, the node's count; `At`, its
    /// place; `IfEmpty`, no child.
    pub(super) fn unlinking(
        &mut self,
        (node, parent): (NodeID, NodeID),
        how: How,
        expected: Expected,
    ) {
        if how == How::Discard {
            return;
        }
        if self.word_of(parent).is_some_and(|word| word.is_discarded()) {
            self.fail(format!("a detach of {node} below Discarded {parent}"));
        }
        let Some((word, placed)) = self.state.slot(node).map(|slot| (slot.record, slot.placed))
        else {
            return;
        };
        match (how, expected) {
            (How::IfClean, _) if word.is_dirty() => {
                self.fail(format!(
                    "an IfClean detach of {node}, which was Dirty: data lost"
                ));
            }
            (How::IfUnchanged, _) => self.counted(node, expected, "a detach checked unchanged"),
            (How::At, Expected::At { parent, name_hash })
                if placed && (word.parent != parent || word.name_hash != name_hash) =>
            {
                self.fail(format!(
                    "an At detach of {node}, which was not where it expected"
                ));
            }
            (How::IfEmpty, _) if self.state.chains.contains_key(&node) => {
                self.fail(format!("an IfEmpty detach of {node}, which held a child"));
            }
            _ => {}
        }
    }

    /// A `Word` of `node` in `section`, `was` its word in force, `is` the one written, before the
    /// model applies it: the node each section's condition admits, and the root's flags kept by
    /// every section but a test's own edit, as no mark changes them (C11c). A pending clear dropped
    /// outside a settle's last phase ends every settle of the node in flight.
    pub(super) fn writing(
        &mut self,
        node: NodeID,
        section: Section,
        (was, was_pending): (&Node, bool),
        (is, is_pending): (&Node, bool),
    ) {
        let edit = section == Section::Unlabelled && self.admit_unlabelled;
        if node == ROOT_NODE && was.flags != is.flags && !edit {
            self.fail(format!(
                "{section:?} changed the root's flags from {:#x} to {:#x} (C11c)",
                was.flags, is.flags
            ));
        }
        let live_file = was.is_file() && !was.is_discarded();
        if was_pending && !is_pending && section != Section::SettlePhase3 {
            self.state
                .settles
                .retain(|&(settled, _), _| settled != node);
        }
        match section {
            Section::Rewrite if !live_file || (!self.state.served && was.is_staged_delete()) => {
                self.fail(format!("a rewrite of {node}, no live file"));
            }
            Section::StagedModify if !live_file || was.is_staged_delete() => {
                self.fail(format!("a modify of {node}, no live file"));
            }
            Section::Restore if was.is_discarded() || !was.is_staged_delete() => {
                self.fail(format!("a restore of {node}, not staged for deletion"));
            }
            Section::Restore
                if self
                    .parent_of(was)
                    .is_some_and(|parent| parent.is_discarded() || parent.is_staged_delete()) =>
            {
                self.fail(format!(
                    "a restore of {node} below its parent {}, Discarded or staged for deletion",
                    was.parent
                ));
            }
            Section::DeleteLevel
                if was.is_discarded() || was.is_staged_add() || was.is_staged_delete() =>
            {
                self.fail(format!(
                    "a delete level's mark of {node}, Discarded, added or staged for deletion"
                ));
            }
            Section::Owned if was.is_discarded() => {
                self.fail(format!("an owned change of Discarded {node}"));
            }
            // A rename in place refuses a Discarded node before it writes, its mark too.
            Section::RenameInPlace if was.is_discarded() => {
                self.fail(format!("a rename of Discarded {node}, which wrote it"));
            }
            Section::InPlace | Section::DeleteLevel | Section::SettlePhase1
                if *self.admitted != Some(node) =>
            {
                self.fail(format!(
                    "{section:?} wrote {node}, no change of it admitted"
                ));
            }
            Section::ServedMark
                if was.is_discarded()
                    || self
                        .parent_of(was)
                        .is_some_and(|parent| parent.is_discarded()) =>
            {
                self.fail(format!(
                    "a served mark of {node}, Discarded or below a Discarded parent"
                ));
            }
            // The action bits are the staged half's as much, which marks a Discarded node.
            Section::UnservedMark
                if was.is_discarded()
                    && ((was.flags ^ is.flags) & NodeFlags::Dirty.bits() != 0
                        || was.ext_flags != is.ext_flags) =>
            {
                self.fail(format!("an unserved mark dirtied Discarded {node}"));
            }
            Section::SettlePhase1 if !was_pending && is_pending => {
                let changes = self.state.slot(node).map_or(0, |slot| slot.changes);
                self.state.settles.insert((node, self.record.task), changes);
            }
            Section::SettlePhase3 => self.settle_finished(node, (was, was_pending), is),
            _ => {}
        }
    }

    /// A settle's last phase on directory `node`: cleared, its content written or its dirty bits
    /// dropped, only where the pending clear its own first phase set was in force and its count
    /// unchanged since.
    fn settle_finished(&mut self, node: NodeID, (was, was_pending): (&Node, bool), is: &Node) {
        let begun = self.state.settles.remove(&(node, self.record.task));
        let cleared =
            (was.is_dirty() && !is.is_dirty()) || was.address != is.address || was.size != is.size;
        if !cleared {
            return;
        }
        if *self.admitted != Some(node) {
            self.fail(format!("a settle cleared {node}, no change of it admitted"));
        }
        if !was_pending {
            self.fail(format!(
                "a settle cleared {node} with no pending clear in force"
            ));
            return;
        }
        let changes = self.state.slot(node).map(|slot| slot.changes);
        if begun.is_none() {
            self.fail(format!(
                "a settle cleared {node}, whose pending clear its own first phase did not set"
            ));
        } else if begun != changes {
            self.fail(format!(
                "a settle cleared {node}, whose count moved from {begun:?} at its first phase to \
                 {changes:?}"
            ));
        }
    }

    /// A change of `node` admitted on `expected`: an in-place change or a settle's phase of a live
    /// node at its place; a delete level's mark of a live node still below its parent, neither
    /// added nor staged for deletion; an `IfClean` rewrite of a clean live file; a
    /// served rewrite of a live file below its live parent; a checked move of a node not staged
    /// for deletion between parents neither Discarded nor staged for deletion; a local-only
    /// reattach of no directory, a directory holding local content, or a respell.
    pub(super) fn admitted(&mut self, node: NodeID, expected: Expected) {
        *self.admitted = Some(node);
        let Some((word, placed)) = self.state.slot(node).map(|slot| (slot.record, slot.placed))
        else {
            self.fail(format!(
                "a change admitted of {node}, which the model has no image of"
            ));
            return;
        };
        let live = |word: Option<Node>| word.is_none_or(|word| !word.is_discarded());
        let holds = match expected {
            Expected::At { parent, name_hash } => {
                !word.is_discarded()
                    && (!placed || (word.parent == parent && word.name_hash == name_hash))
            }
            // A rewrite that writes the content the file holds writes no `Word`.
            Expected::Clean => {
                word.is_file()
                    && !word.is_discarded()
                    && !word.is_staged_delete()
                    && !word.is_dirty()
            }
            Expected::Below(parent) => {
                word.is_file()
                    && !word.is_discarded()
                    && (!placed || word.parent == parent)
                    && (parent == ROOT_NODE || live(self.word_of(parent)))
            }
            Expected::Movable { from, to } => {
                !word.is_staged_delete()
                    && [from, to].into_iter().all(|parent| {
                        self.word_of(parent).is_none_or(|parent| {
                            !parent.is_discarded() && !parent.is_staged_delete()
                        })
                    })
            }
            Expected::Deletable(parent) => {
                !word.is_discarded()
                    && (!placed || word.parent == parent)
                    && !word.is_staged_add()
                    && !word.is_staged_delete()
            }
            Expected::Local { to, name_hash } => {
                !word.is_directory()
                    || word.ext_flags().contains(NodeExtFlags::LocalContent)
                    || !placed
                    || (word.parent == to && word.name_hash == name_hash)
            }
            _ => false,
        };
        if !holds {
            self.fail(format!(
                "a change of {node} admitted on {expected:?}, which the words it read did not hold"
            ));
        }
    }
}
