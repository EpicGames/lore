// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The write sets: the fields of a node a `Word` changes, and the fields each section may change on
//! a node in each role, as the code writes them. A `Word` changing a field outside its set wrote
//! what its section never writes: a stale record written back over another's change, or a write the
//! log does not explain.
//!
//! `Discarded`, the parent and the name hash are in no section's set but an initialization's: the
//! structural records apply them before the section's `Word`s, so a section changing one with no
//! record of it is caught at the node's next `Word`, or by the compare where none follows: a change
//! of the parent or name hash alone makes no `Word`. A link's `child` is the same: its repin's
//! record applies it.

use std::fmt;
use std::ops::BitAnd;
use std::ops::BitOr;
use std::ops::Not;

use lore_base::types::Hash;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::state::node::hooks::log::Section;

/// Fields of a node's record, as a `Word` changes them.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Fields(u16);

/// The kind bits: file, link, the deprecated external name.
const KIND_BITS: u16 =
    NodeFlags::File.bits() | NodeFlags::Link.bits() | NodeFlags::ExternalNameDeprecated.bits();

// Every flag bit is in one field.
const _: () = assert!(
    KIND_BITS
        | NodeFlags::Dirty.bits()
        | NodeFlags::Staged.bits()
        | NodeFlags::ActionBits.bits()
        | NodeFlags::MergeBits.bits()
        | NodeFlags::Discarded.bits()
        == u16::MAX
);

impl Fields {
    pub const NONE: Self = Self(0);
    pub const KIND: Self = Self(1);
    pub const DIRTY: Self = Self(1 << 1);
    pub const STAGED: Self = Self(1 << 2);
    /// The action bits, which the dirty and staged halves share.
    pub const ACTION: Self = Self(1 << 3);
    pub const MERGE: Self = Self(1 << 4);
    pub const DISCARDED: Self = Self(1 << 5);
    /// The extension word.
    pub const EXT: Self = Self(1 << 6);
    /// The block's pending clear bit for the node.
    pub const CLEAR_PENDING: Self = Self(1 << 7);
    pub const ADDRESS: Self = Self(1 << 8);
    pub const SIZE: Self = Self(1 << 9);
    pub const MODE: Self = Self(1 << 10);
    /// A link's `child`, the node its pin exposes.
    pub const CHILD: Self = Self(1 << 11);
    pub const PARENT: Self = Self(1 << 12);
    pub const NAME_HASH: Self = Self(1 << 13);
    pub const CONTENT: Self = Self(Self::ADDRESS.0 | Self::SIZE.0 | Self::MODE.0);
    pub const ALL: Self = Self((1 << 14) - 1);

    const NAMES: [&'static str; 14] = [
        "kind",
        "Dirty",
        "Staged",
        "actions",
        "merge",
        "Discarded",
        "ext",
        "clear_pending",
        "address",
        "size",
        "mode",
        "child",
        "parent",
        "name_hash",
    ];

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Each field, alone.
    pub fn each() -> impl Iterator<Item = Self> {
        (0..Self::NAMES.len()).map(|bit| Self(1 << bit))
    }

    /// The fields a `Word` of `record` compares: a link's `child` is its pin, a directory's the
    /// head of its chain, which is the structure's.
    pub fn compared(record: &Node) -> Self {
        if record.is_link() {
            Self::ALL
        } else {
            Self::ALL & !Self::CHILD
        }
    }

    /// The fields that differ between `was` and `is`, each with its pending clear bit.
    pub fn changed((was, was_pending): (&Node, bool), (is, is_pending): (&Node, bool)) -> Self {
        let flags = was.flags ^ is.flags;
        let differs = [
            flags & KIND_BITS != 0,
            flags & NodeFlags::Dirty.bits() != 0,
            flags & NodeFlags::Staged.bits() != 0,
            flags & NodeFlags::ActionBits.bits() != 0,
            flags & NodeFlags::MergeBits.bits() != 0,
            flags & NodeFlags::Discarded.bits() != 0,
            was.ext_flags != is.ext_flags,
            was_pending != is_pending,
            was.address != is.address,
            was.size != is.size,
            was.mode != is.mode,
            (was.is_link() || is.is_link()) && was.child != is.child,
            was.parent != is.parent,
            was.name_hash != is.name_hash,
        ];
        differs
            .iter()
            .enumerate()
            .filter(|(_, differs)| **differs)
            .fold(Self::NONE, |fields, (bit, _)| fields | Self(1 << bit))
    }

    /// `record` and its pending clear bit with the one field `self` changed: for a test holding a
    /// write set to the code.
    pub fn alter(self, record: &mut Node, clear_pending: &mut bool) {
        let flip = |flags: &mut u16, mask: u16| *flags ^= 1 << mask.trailing_zeros();
        match self {
            Self::KIND => flip(&mut record.flags, NodeFlags::File.bits()),
            Self::DIRTY => flip(&mut record.flags, NodeFlags::Dirty.bits()),
            Self::STAGED => flip(&mut record.flags, NodeFlags::Staged.bits()),
            Self::ACTION => flip(&mut record.flags, NodeFlags::ActionBits.bits()),
            Self::MERGE => flip(&mut record.flags, NodeFlags::MergeBits.bits()),
            Self::DISCARDED => flip(&mut record.flags, NodeFlags::Discarded.bits()),
            Self::EXT => record.ext_flags ^= 1,
            Self::CLEAR_PENDING => *clear_pending = !*clear_pending,
            Self::ADDRESS => {
                let (one, two) = (Hash::from_u64(1), Hash::from_u64(2));
                record.address.hash = if record.address.hash == one { two } else { one };
            }
            Self::SIZE => record.size ^= 1,
            Self::MODE => record.mode ^= 1,
            Self::CHILD => record.child ^= 1,
            Self::PARENT => record.parent ^= 1,
            Self::NAME_HASH => record.name_hash ^= 1,
            _ => panic!("one field to alter, not {self:?}"),
        }
    }
}

impl BitOr for Fields {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl BitAnd for Fields {
    type Output = Self;

    fn bitand(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

impl Not for Fields {
    type Output = Self;

    fn not(self) -> Self {
        Self(!self.0 & Self::ALL.0)
    }
}

impl fmt::Debug for Fields {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = Self::NAMES
            .iter()
            .enumerate()
            .filter(|(bit, _)| self.0 & (1 << bit) != 0)
            .map(|(_, name)| *name)
            .collect();
        write!(f, "{{{}}}", names.join(", "))
    }
}

/// What a section may change on a node in one role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Set {
    pub fields: Fields,
    /// The merge bits change only to a word holding merge state: a mark keeps them unless it
    /// sets merge state of its own (mark.rs, `Halves::apply`).
    pub merge_to_state: bool,
}

impl Set {
    /// Everything: a test's own edit, which no section makes.
    pub const ALL: Self = Self {
        fields: Fields::ALL,
        merge_to_state: false,
    };

    /// Whether a `Word` changing `changed`, to `is`, stays inside this set.
    pub fn admits(&self, changed: Fields, is: &Node) -> bool {
        (changed & !self.fields).is_empty()
            && !(self.merge_to_state
                && changed.contains(Fields::MERGE)
                && is.flags & NodeFlags::MergeBits.bits() == 0)
    }
}

/// How a `Word` was judged: the set it was held to, and the model's words before it.
#[derive(Clone, Copy, Debug)]
pub struct Judged {
    pub set: Set,
    pub was: Node,
    pub was_pending: bool,
}

impl Judged {
    /// Whether a `Word` writing `is` and `is_pending` in its place is caught: it changes, against
    /// the model's words before it, a field outside the set.
    pub fn catches(&self, is: &Node, is_pending: bool) -> bool {
        let changed = Fields::changed((&self.was, self.was_pending), (is, is_pending));
        !self.set.admits(changed, is)
    }
}

/// A node's role in the section writing it, from the section's structural records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The node a structural record of the section names.
    Target,
    /// A reattach's replaced holder.
    Holder,
    /// A node no structural record names, in a section that has one.
    Other,
    /// A node of a section with no structural record.
    Any,
}

/// The set of `section` for a node in `role`, on a served State where `served`, for a link where
/// `link`: a link's pin changes only at its initialization, and its address in a repin or a
/// restore. Each row as the section's site writes, read there (state/node/*.rs).
pub fn set(section: Section, role: Role, served: bool, link: bool) -> Set {
    use Fields as F;
    // A mark's own halves and the extension word, with the merge bits it keeps.
    let marks = F::DIRTY | F::STAGED | F::ACTION | F::EXT;
    let (fields, merge_to_state) = match section {
        // Their records apply what they change: `Discarded`, the chains, the slots.
        Section::Unlabelled
        | Section::Release
        | Section::Publish
        | Section::BuildAdd
        | Section::Abandon
        | Section::Detach
        | Section::DiscardMarks
        | Section::DiscardFree
        | Section::Allocator
        | Section::Sweep => (F::NONE, false),
        // `initialize_node` writes the whole record of a slot in flight, `Discarded` kept; no grab
        // touches the pending clear bits.
        Section::Grab => (F::ALL & !(F::DISCARDED | F::CLEAR_PENDING), false),
        // `served_mark_level`: `Dirty` replacing the dirty bits, the pending clear dropped.
        Section::ServedMark => (F::DIRTY | F::ACTION | F::CLEAR_PENDING, false),
        // `Halves::apply`: the halves, the target's extension word, merge state set or kept.
        Section::UnservedMark => (marks | F::MERGE, true),
        // A served move of a directory dirties it and drops its pending clear; H20's actions on
        // a file; the holder's `Discarded` is the record's.
        Section::Reattach => match role {
            Role::Target if served => (F::DIRTY | F::ACTION | F::CLEAR_PENDING, false),
            _ => (F::NONE, false),
        },
        // The own mark, which sets merge state where a merge's realize moves (realize.rs).
        Section::MoveNamed | Section::RenameInPlace => match role {
            Role::Target | Role::Any => (marks | F::MERGE, true),
            Role::Holder | Role::Other => (F::NONE, false),
        },
        // Phase 1 drops the actions and sets the pending clear; phase 3 drops it, and its clear
        // writes the content and `clear_dirty_flags`.
        Section::SettlePhase1 => (F::ACTION | F::CLEAR_PENDING, false),
        Section::SettlePhase3 => (
            F::CLEAR_PENDING | F::DIRTY | F::ACTION | F::EXT | F::ADDRESS | F::SIZE,
            false,
        ),
        // `clear_staged_flags` and `clear_dirty_flags`.
        Section::UnservedClear | Section::ClearPropagation => {
            (F::DIRTY | F::STAGED | F::ACTION | F::MERGE | F::EXT, false)
        }
        // `rewrite_record`; the edit adds its staged half (`edit_record`).
        Section::Rewrite => (F::CONTENT | F::DIRTY | F::ACTION | F::EXT, false),
        Section::StagedModify => (F::CONTENT | marks, false),
        Section::DeleteLevel => (marks, false),
        // `restore_record` drops the staged flags, merge and the action with them, and its mark
        // follows; the `NodeContent` methods and the owned clears.
        Section::Restore | Section::InPlace | Section::Owned => {
            (F::CONTENT | marks | F::MERGE, false)
        }
        // The pin's address; its `child` is the record's.
        Section::Repin => match role {
            Role::Target => (F::ADDRESS, false),
            _ => (F::NONE, false),
        },
        // A commit's `update_tree_root_hash`.
        Section::Save => (F::ADDRESS | F::SIZE, false),
    };
    let fields = if link {
        let pin = match section {
            Section::Grab => F::ADDRESS | F::CHILD,
            Section::Repin | Section::Restore => F::ADDRESS,
            _ => F::NONE,
        };
        (fields & !(F::ADDRESS | F::CHILD)) | (fields & pin)
    } else {
        fields
    };
    Set {
        fields,
        merge_to_state,
    }
}
