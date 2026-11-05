// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The operation catalogue: each operation the scenarios drive as a production caller makes it is a
//! kind of its own, named once, with the tier it runs on and how its caller runs it. Helpers that
//! stand for no caller, such as the fixtures' edits and the entry points no production caller
//! makes, have no kind.

pub mod call;
pub mod modelled;
pub mod ops;

use lore_revision::state::node::hooks::log::Section;

/// What the contract runs an operation beside: the State's tier. `Served` is the node module's
/// served tier (`lore_revision::state::node`); `Swap` is what a VFS provider runs one at a time on
/// a served State: the module's swap tier, a settle and a commit's flip; `Shared` and `Exclusive`
/// split its unserved tier by claim; `Any` is the readers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// A served State's kinds: the `ServedNodes` entry points and their compositions, the
    /// provider's in-place changes, and the readers of what only a served State holds.
    Served,
    /// What a VFS provider runs one at a time on a served State, beside the served kinds: a
    /// settle, a swap-tier kind, a commit's flip.
    Swap,
    /// An unserved State under the C API's shared claim.
    Shared,
    /// An unserved State under an exclusive claim: stage, commit, merge, reset, `os_diff`, the
    /// sync movers, unstage, the pre-serving writers, status, `lore dirty`, the link operations.
    Exclusive,
    /// Any State: the readers, which every tier runs beside its writers.
    Any,
}

/// Whether an operation changes its State.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Writer,
    Reader,
}

/// What the contract runs a kind beside on its State.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runs {
    /// Beside the kinds the contract runs with its tier's, under the filters: a swap-tier kind
    /// beside the served kinds, one at a time.
    Beside,
    /// Beside readers and the few writers the contract admits with it, once the State's other
    /// writers drain.
    WriterExclusive,
    /// Alone on its State.
    Solo,
}

/// How a kind writes its State, for the model's rules ([`Kind::writes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Writes {
    /// Through a `ServedNodes` entry point, under the served chain rule.
    Served,
    /// A staged change, which climbs first (H8).
    Staged,
    /// A detach of its target, a served conditional one pre-testing each attempt (H2).
    Detaches,
    /// The C API delete's pend: an unlink of its target.
    Pends,
    /// Holding the moves permit across its own steps, or inside its batch's (C10).
    Permitted,
}

/// A kind of operation: one primitive in one mode, or a caller's composition, as its production
/// caller runs it.
#[derive(Clone, Copy, Debug)]
pub struct Kind {
    /// Its stable name, which names it in traces and reports.
    pub name: &'static str,
    /// The module's entry points it changes its State through, in order, or a reader's it reads
    /// through; the reads a writer makes on the way go unnamed.
    pub primitive: &'static str,
    /// The mode it calls them in.
    pub mode: &'static str,
    pub tier: Tier,
    pub role: Role,
    pub runs: Runs,
    /// Whether its production caller may drop it part way: not the C API delete's batch, which
    /// runs in a task the call awaits, nor the commit's discard walk and free. The C API commit
    /// runs in its call, so its drain, its snapshot's save and its prune may be dropped.
    pub cancellable: bool,
    /// How it writes its State, which picks the model's rules for its calls.
    pub writes: &'static [Writes],
    /// The labelled sections its calls' records come in: those its primitives write, on every
    /// path.
    pub sections: &'static [Section],
}

/// The kind named `name`.
pub fn kind(name: &str) -> Option<&'static Kind> {
    KINDS.iter().copied().find(|kind| kind.name == name)
}

/// Declares each kind as a static of its own and [`KINDS`] holding every one, so no declared kind
/// goes unregistered.
macro_rules! kinds {
    ($($(#[$attr:meta])* $kind:ident { $($field:ident: $value:expr),* $(,)? })*) => {
        $($(#[$attr])* pub static $kind: Kind = Kind { $($field: $value),* };)*

        /// Every kind of the catalogue, each a static, so a kind found in it is the one declared.
        pub static KINDS: &[&Kind] = &[$(&$kind),*];
    };
}

kinds! {
    /// The kind of [`ops::served_mark_dir`].
    SERVED_MARK {
        name: "served mark",
        primitive: "ServedNodes::node_mark",
        mode: "a directory holding a restored name",
        tier: Tier::Swap,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Served],
        sections: &[Section::ServedMark],
    }

    /// The kind of [`ops::served_create`].
    SERVED_CREATE {
        name: "served create",
        primitive: "ServedNodes::node_attach",
        mode: "AttachCheck::Walk, local content",
        tier: Tier::Served,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Served],
        // Reached only on paths the runs rarely take:
        // - Allocator: a grab retesting a retired block, or contention (slots.rs, retire)
        sections: &[
            Section::Allocator,
            Section::Grab,
            Section::Publish,
            Section::Release,
            Section::ServedMark,
        ],
    }

    /// The kind of [`ops::served_rename`].
    SERVED_RENAME {
        name: "served rename",
        primitive: "ServedNodes::node_reattach_local",
        mode: "ReattachCheck::Walk",
        tier: Tier::Served,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Served],
        sections: &[Section::Reattach, Section::ServedMark],
    }

    /// The kind of [`ops::listing_create`] and [`ops::listing_create_waiting`].
    LISTING_CREATE {
        name: "listing create",
        primitive: "ServedNodes::node_attach",
        mode: "AttachCheck::Absent where a listing's lookup found one, else Walk, local content",
        tier: Tier::Served,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Served],
        // Reached only on paths the runs rarely take, as for a served create:
        // - Allocator: a grab retesting a retired block, or contention (slots.rs, retire)
        sections: &[
            Section::Allocator,
            Section::Grab,
            Section::Publish,
            Section::Release,
            Section::ServedMark,
        ],
    }

    /// The kind of [`ops::listing_rename`] and [`ops::listing_rename_waiting`].
    LISTING_RENAME {
        name: "listing rename",
        primitive: "ServedNodes::node_reattach_local",
        mode: "ReattachCheck::Absent where a listing's lookup found one, else Walk",
        tier: Tier::Served,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Served],
        sections: &[Section::Reattach, Section::ServedMark],
    }

    /// The kind of [`ops::served_rename_replacing`].
    SERVED_RENAME_REPLACING {
        name: "served replacing rename",
        primitive: "ServedNodes::node_reattach_local",
        mode: "ReattachCheck::Replacing",
        tier: Tier::Served,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Served],
        sections: &[Section::Reattach, Section::ServedMark],
    }

    /// The kind of [`ops::served_detach`].
    SERVED_DETACH {
        name: "served detach",
        primitive: "ServedNodes::node_detach",
        mode: "Detach::Always",
        tier: Tier::Served,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Served, Writes::Detaches],
        sections: &[Section::Detach, Section::ServedMark],
    }

    /// The kind of [`ops::served_rmdir`].
    SERVED_RMDIR {
        name: "served RMDIR",
        primitive: "ServedNodes::node_detach",
        mode: "Detach::IfUnchanged, after a named walk",
        tier: Tier::Served,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Served, Writes::Detaches],
        sections: &[Section::Detach, Section::ServedMark],
    }

    /// The kind of [`ops::served_copy_up`].
    SERVED_COPY_UP {
        name: "served copy-up",
        primitive: "ServedNodes::node_copy_up",
        mode: "dirty modify, local content",
        tier: Tier::Served,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Served],
        sections: &[Section::Rewrite, Section::ServedMark],
    }

    /// The kind of [`ops::swap_attach`].
    SWAP_ATTACH {
        name: "swap attach",
        primitive: "SwapNodes::node_attach_tree, TreeBuild::publish",
        mode: "a file",
        tier: Tier::Swap,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        // Reached only on paths the runs rarely take:
        // - Allocator: a grab retesting a retired block, or contention (slots.rs, retire)
        sections: &[
            Section::Abandon,
            Section::Allocator,
            Section::Grab,
            Section::Publish,
            Section::Release,
        ],
    }

    /// The kind of [`ops::swap_copy_tree`] and [`ops::swap_copy_paced`].
    SWAP_COPY_TREE {
        name: "swap copy",
        primitive: "SwapNodes::node_attach_tree, TreeBuild::add, TreeBuild::publish",
        mode: "a directory of files",
        tier: Tier::Swap,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        // Reached only on paths the runs rarely take:
        // - Allocator: a grab retesting a retired block, or contention (slots.rs, retire)
        sections: &[
            Section::Abandon,
            Section::Allocator,
            Section::BuildAdd,
            Section::Grab,
            Section::Publish,
            Section::Release,
        ],
    }

    /// The kind of [`ops::swap_rewrite_if_clean`] and [`ops::swap_rewrite`].
    SWAP_REWRITE {
        name: "swap rewrite",
        primitive: "State::node_rewrite",
        mode: "Rewrite::IfClean",
        tier: Tier::Swap,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[Section::Rewrite],
    }

    /// The kind of [`ops::swap_detach_if_clean`].
    SWAP_DETACH_IF_CLEAN {
        name: "swap clean detach",
        primitive: "SwapNodes::node_detach",
        mode: "Detach::IfClean",
        tier: Tier::Swap,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Detaches],
        sections: &[Section::Detach],
    }

    /// The kind of [`ops::settle`], [`ops::settle_waiting`] and [`ops::served_clear`].
    SETTLE {
        name: "settle",
        primitive: "ServedNodes::node_clear",
        mode: "against the committed tree",
        tier: Tier::Swap,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Served],
        sections: &[Section::SettlePhase1, Section::SettlePhase3],
    }

    /// The kind of [`ops::flip`].
    FLIP {
        name: "flip",
        primitive: "State::node_change_in_place",
        mode: "the committed content, clean",
        tier: Tier::Swap,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[Section::InPlace],
    }

    /// The kind of [`ops::find`].
    FIND {
        name: "find",
        primitive: "State::find_subnode",
        mode: "one name",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::find_resident`].
    FIND_RESIDENT {
        name: "resident find",
        primitive: "State::find_subnode_resident",
        mode: "blocks in memory",
        tier: Tier::Served,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::find_dir`].
    FIND_PATH {
        name: "find by path",
        primitive: "State::find_subnode",
        mode: "a hop per name",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::find_linked`].
    FIND_LINKED {
        name: "find through links",
        primitive: "State::find_relative_node_link",
        mode: "a hop per name, entering links",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::children`].
    CHILDREN {
        name: "children",
        primitive: "StateNodeChildrenIterator::new",
        mode: "from the directory",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::children_from_parent`].
    CHILDREN_FROM_PARENT {
        name: "children from the parent",
        primitive: "StateNodeChildrenIterator::with_parent",
        mode: "from the parent's record",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::named_children`].
    NAMED_CHILDREN {
        name: "named children",
        primitive: "StateNodeChildrenWithNameIterator::new",
        mode: "with their names",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::listed_children`].
    LISTED_CHILDREN {
        name: "listed children",
        primitive: "State::node_children",
        mode: "by their ids",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::listed_name_hashes`].
    LISTED_NAME_HASHES {
        name: "listed name hashes",
        primitive: "State::node_children_with_name_hash",
        mode: "with their names' hashes",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::listed_marked`].
    LISTED_MARKED {
        name: "listed marked",
        primitive: "State::node_children_with_name_hash_marked",
        mode: "with their names' hashes and the chain's mark",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::added_since`].
    ADDED_SINCE {
        name: "added since",
        primitive: "State::find_subnode_added_since",
        mode: "by a listing's mark",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::reattached_since`].
    REATTACHED_SINCE {
        name: "reattached since",
        primitive: "ChainMark::reattached",
        mode: "by a listing's mark",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::name_read`] by [`ops::NameRead::Clone`].
    NAME_CLONE {
        name: "name clone",
        primitive: "State::node_name_clone",
        mode: "copied out",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::name_read`] by [`ops::NameRead::Ref`].
    NAME_REF {
        name: "name ref",
        primitive: "State::node_name_ref",
        mode: "under its block's read lock",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::name_read`] by [`ops::NameRead::RefOrSkip`].
    NAME_REF_OR_SKIP {
        name: "name ref or skip",
        primitive: "State::node_name_ref_or_skip",
        mode: "under its block's read lock, a refused name skipped",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::name_read`] by [`ops::NameRead::CloneOrSkip`].
    NAME_CLONE_OR_SKIP {
        name: "name clone or skip",
        primitive: "State::node_name_clone_or_skip",
        mode: "copied out, a refused name skipped",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::name_is`].
    NAME_IS {
        name: "name is",
        primitive: "State::node_name_is",
        mode: "compared where it is stored",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::node_path`] from the root.
    NODE_PATH {
        name: "node path",
        primitive: "State::node_path",
        mode: "from the root",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::node_path`] from an ancestor.
    NODE_PATH_BELOW {
        name: "node path below",
        primitive: "State::node_path_below",
        mode: "from an ancestor",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::has_children`] by [`ops::Flagged::Staged`].
    HAS_STAGED_CHILDREN {
        name: "has staged children",
        primitive: "State::node_has_staged_children",
        mode: "a listing, then each child's staged bit",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::has_children`] by [`ops::Flagged::Dirty`].
    HAS_DIRTY_CHILDREN {
        name: "has dirty children",
        primitive: "State::node_has_dirty_children",
        mode: "a listing, then each child's dirty bit",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::has_children`] by [`ops::Flagged::StagedOrDirty`].
    HAS_STAGED_OR_DIRTY_CHILDREN {
        name: "has staged or dirty children",
        primitive: "State::node_has_staged_or_dirty_children",
        mode: "a listing, then each child's staged and dirty bits",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::dirty_paths`].
    DIRTY_PATHS {
        name: "dirty paths",
        primitive: "State::collect_dirty_paths",
        mode: "one State, below a directory",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::collected_children`].
    COLLECTED_CHILDREN {
        name: "collected children",
        primitive: "State::collect_children_unsorted",
        mode: "staged deletes left out, links followed",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::collected_versioned_children`].
    COLLECTED_VERSIONED_CHILDREN {
        name: "collected versioned children",
        primitive: "State::collect_children_unsorted_where",
        mode: "staged deletes kept, symbolic links left out, links followed",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::collected_named_children`].
    COLLECTED_NAMED_CHILDREN {
        name: "collected named children",
        primitive: "State::collect_named_children_unsorted",
        mode: "staged deletes left out, links not followed",
        tier: Tier::Any,
        role: Role::Reader,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::checked_create`].
    CHECKED_CREATE {
        name: "checked create",
        primitive: "State::node_attach, State::node_clear, State::node_mark",
        mode: "a file added dirty, its name's holder undeleted or marked, then its parent marked",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        // Reached only on paths the runs rarely take:
        // - Allocator: a grab retesting a retired block, or contention (slots.rs, retire)
        // - Release: an attach ending unattached gives its slot back (attach.rs, attach_child)
        // - UnservedClear: a holder staged for deletion taken back (checked_attach)
        sections: &[
            Section::Allocator,
            Section::Grab,
            Section::Publish,
            Section::Release,
            Section::UnservedClear,
            Section::UnservedMark,
        ],
    }

    /// The kind of [`ops::scan_add`].
    SCAN_ADD {
        name: "scan add",
        primitive: "State::node_add, State::node_mark",
        mode: "a directory added dirty, then its parent marked",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        // Reached only on paths the runs rarely take:
        // - Allocator: a grab retesting a retired block, or contention (slots.rs, retire)
        // - Release: an add refused under its lock gives its slot back (attach.rs, add_child)
        sections: &[
            Section::Allocator,
            Section::Grab,
            Section::Publish,
            Section::Release,
            Section::UnservedMark,
        ],
    }

    /// The kind of [`ops::scan_chain`].
    SCAN_CHAIN {
        name: "scan chain",
        primitive: "State::node_add, State::node_mark",
        mode: "each directory of a path below a node found, or added dirty without a name check, \
               the directory above the first added marked",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        // Reached only on paths the runs rarely take, as for a scan's add.
        sections: &[
            Section::Allocator,
            Section::Grab,
            Section::Publish,
            Section::Release,
            Section::UnservedMark,
        ],
    }

    /// The kind of [`ops::diff_add`].
    DIFF_ADD {
        name: "diff add",
        primitive: "State::node_add, State::node_mark",
        mode: "a file or directory added dirty without a name check into a directory the walk \
               listed, then that directory marked",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        // Reached only on paths the runs rarely take, as for a scan's add.
        sections: &[
            Section::Allocator,
            Section::Grab,
            Section::Publish,
            Section::Release,
            Section::UnservedMark,
        ],
    }

    /// The kind of [`ops::dirty_add`].
    DIRTY_ADD {
        name: "dirty add",
        primitive: "State::node_add, State::node_mark",
        mode: "the missing directories of a path below a node added clean and marked dirty, \
               forced, then a clean file or directory added below them without a name check and \
               marked dirty-added, forced, with its ancestors, in a call nothing drops",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: false,
        writes: &[],
        // Reached only on paths the runs rarely take, as for a scan's add.
        sections: &[
            Section::Allocator,
            Section::Grab,
            Section::Publish,
            Section::Release,
            Section::UnservedMark,
        ],
    }

    /// The kind of [`ops::stage_claim`], [`ops::stage_claim_waiting`],
    /// [`ops::stage_claim_descending`] and [`ops::stage_claim_windowed`].
    STAGE_CLAIM {
        name: "stage claim",
        primitive: "State::node_add, State::node_mark",
        mode: "a directory listed with its chain's mark, each entry claimed from the listing or \
               searched for since the mark, a node found claimed in the listing too where the \
               mark found a reattach, a missing entry added clean and marked staged and dirty \
               added, forced, with its ancestors, counted in the mark once marked, in a call \
               nothing drops",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: false,
        writes: &[],
        // Reached only on paths the runs rarely take, as for a scan's add.
        sections: &[
            Section::Allocator,
            Section::Grab,
            Section::Publish,
            Section::Release,
            Section::UnservedMark,
        ],
    }

    /// The kind of [`ops::stage_respell`].
    STAGE_RESPELL {
        name: "stage respell",
        primitive: "State::node_rename, State::node_mark",
        mode: "the node's name read, then NameChange::Rename of its case alone where it differs \
               and the node's own staged mark, StagedMove, forced, as `lore stage --case rename` \
               makes them in a stage claim's window after the node's claim, in a call nothing \
               drops",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: false,
        writes: &[],
        sections: &[Section::RenameInPlace, Section::UnservedMark],
    }

    /// The kind of [`ops::scan_mark_modified`].
    SCAN_MARK_MODIFIED {
        name: "scan mark",
        primitive: "State::node_mark",
        mode: "dirty modify, forced",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[Section::UnservedMark],
    }

    /// The kind of [`ops::scan_clear`].
    SCAN_CLEAR {
        name: "scan clear",
        primitive: "State::node_clear",
        mode: "Clear::Dirty",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[Section::UnservedClear, Section::UnservedMark],
    }

    /// The kind of [`ops::vacated_clear`].
    VACATED_CLEAR {
        name: "vacated clear",
        primitive: "State::node_clear",
        mode: "Clear::Vacated",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[Section::UnservedClear, Section::UnservedMark],
    }

    /// The kind of [`ops::staged_clear`].
    STAGED_CLEAR {
        name: "staged clear",
        primitive: "State::node_clear",
        mode: "Clear::Staged",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[Section::UnservedClear],
    }

    /// The kind of [`ops::unserved_detach`].
    UNSERVED_DETACH {
        name: "unserved detach",
        primitive: "State::node_detach",
        mode: "Detach::Always",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Detaches],
        sections: &[Section::Detach, Section::UnservedMark],
    }

    /// The kind of [`ops::add_discard`].
    ADD_DISCARD {
        name: "add then discard",
        primitive: "State::node_add, node_discard_fused",
        mode: "a file",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        // Reached only on paths the runs rarely take:
        // - Allocator: a grab retesting a retired block, or contention (slots.rs, retire)
        sections: &[
            Section::Allocator,
            Section::DiscardMarks,
            Section::Grab,
            Section::Publish,
            Section::Release,
        ],
    }

    /// The kind of [`ops::add_discard_tree`].
    ADD_DISCARD_TREE {
        name: "add then discard a tree",
        primitive: "State::node_add, node_discard_fused",
        mode: "a directory holding a file and a directory of a file",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        // Reached only on paths the runs rarely take:
        // - Allocator: a grab retesting a retired block, or contention (slots.rs, retire)
        // - Release: an add refused under its lock gives its slot back (attach.rs, add_child)
        sections: &[
            Section::Allocator,
            Section::DiscardFree,
            Section::DiscardMarks,
            Section::Grab,
            Section::Publish,
            Section::Release,
        ],
    }

    /// The kind of [`ops::rename_then_mark`].
    RENAME {
        name: "rename",
        primitive: "State::node_rename, State::node_mark",
        mode: "the node's name read, then NameChange::Rename where it differs and the node's own \
               mark, Dirty, Keep, as the pre-serving respelling makes them",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[Section::RenameInPlace, Section::UnservedMark],
    }

    /// The kind of [`ops::dirty_rewrite`].
    DIRTY_REWRITE {
        name: "dirty rewrite",
        primitive: "State::node_rewrite, State::node_mark",
        mode: "Rewrite::Dirty with no extension flags, then the parent's mark",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        // Reached only on paths the runs rarely take:
        // - UnservedMark: the parent's mark where an ancestor lacks Dirty (mark.rs)
        sections: &[Section::Rewrite, Section::UnservedMark],
    }

    /// The kind of [`ops::in_place_size`].
    IN_PLACE {
        name: "in-place change",
        primitive: "State::node_change_in_place",
        mode: "the size set",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[Section::InPlace],
    }

    /// The kind of [`ops::owned_size`].
    OWNED {
        name: "owned change",
        primitive: "State::node_change_owned",
        mode: "the size set",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[Section::Owned],
    }

    /// The kind of [`ops::owned_clear`].
    OWNED_CLEAR {
        name: "owned clear",
        primitive: "State::node_clear_own",
        mode: "OwnFlags::Dirty",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[Section::Owned],
    }

    /// The kind of [`ops::repin`].
    REPIN {
        name: "repin",
        primitive: "State::node_set_link_target_in, State::node_set_link_target",
        mode: "a whole pin through the link's block in hand, as staging, unstaging and merging \
               repin one; or a revision's alone, as a link's update does",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[Section::Repin],
    }

    /// The kind of [`ops::drain`].
    DRAIN {
        name: "drain",
        primitive: "State::node_free_pending",
        mode: "every queued subtree",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        sections: &[Section::DiscardFree],
    }

    /// The kind of [`ops::sweep`].
    SWEEP {
        name: "sweep",
        primitive: "State::node_sweep_detached",
        mode: "the pending queue drained, then each flagged block in memory scanned and its detached \
               and leaked slots freed with their subtrees, as the stage, the CLI commit and the C \
               API commit run it before they save",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Solo,
        cancellable: true,
        writes: &[],
        // The scan writes no record.
        sections: &[Section::DiscardFree],
    }

    /// The kind of [`ops::served_sweep`].
    SERVED_SWEEP {
        name: "served sweep",
        primitive: "ServedNodes::node_sweep_detached",
        mode: "each flagged block in memory scanned and its detached and leaked slots freed with \
               their subtrees, but a subtree holding a node the provider retains, where it paused \
               its requests, settles and swaps",
        tier: Tier::Served,
        role: Role::Writer,
        runs: Runs::Solo,
        cancellable: true,
        writes: &[],
        sections: &[Section::DiscardFree],
    }

    /// The kind of [`ops::dirty_move`].
    DIRTY_MOVE {
        name: "dirty move",
        primitive: "State::node_move, State::node_mark_destination, State::node_mark, \
                    State::node_mark_left",
        mode: "The trusted move as `lore dirty move` makes it: the node read, moved dirty, its \
               destination climbed, a directory's subtree marked moved, its adds as adds and its \
               deleted nodes left, then the directory it left marked dirty, in a call nothing \
               drops",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::WriterExclusive,
        cancellable: false,
        writes: &[],
        // Reached only on paths the runs rarely take:
        // - RenameInPlace: a move within one directory (relocate.rs, move_named)
        sections: &[Section::MoveNamed, Section::RenameInPlace, Section::UnservedMark],
    }

    /// The kind of [`ops::stage_move`].
    STAGE_MOVE {
        name: "stage move",
        primitive: "State::node_move, State::node_mark_destination, State::node_mark, \
                    State::node_mark_left",
        mode: "The trusted move as `lore stage move` makes it: the node read, moved staged, the \
               node its name replaces staged deleted, its destination climbed, dirty where the \
               node is dirty, a directory's subtree marked moved, its adds as adds and its deleted \
               nodes left, then the directory it left staged where a commit recorded it, in a \
               call nothing drops",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::WriterExclusive,
        cancellable: false,
        writes: &[],
        // Reached only on paths the runs rarely take, as for the dirty move.
        sections: &[Section::MoveNamed, Section::RenameInPlace, Section::UnservedMark],
    }

    /// The kind of [`ops::realize_move`].
    REALIZE_MOVE {
        name: "realize move",
        primitive: "State::node_move, State::node_mark_destination, State::node_mark_left",
        mode: "The trusted move as a merge's or restore's realize makes it: moved staged, with \
               StagedMerge in a merge, its destination climbed, then the directory it left staged, \
               in a call nothing drops",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::WriterExclusive,
        cancellable: false,
        writes: &[],
        // Reached only on paths the runs rarely take, as for the dirty move.
        sections: &[Section::MoveNamed, Section::RenameInPlace, Section::UnservedMark],
    }

    /// The kind of [`crate::harness::fixtures::Tree::save`].
    SERIALIZE {
        name: "serialize",
        primitive: "State::serialize",
        mode: "the State saved as it stands",
        tier: Tier::Exclusive,
        role: Role::Writer,
        runs: Runs::Solo,
        cancellable: true,
        writes: &[],
        sections: &[],
    }

    /// The kind of [`ops::staged_add`].
    STAGED_ADD {
        name: "staged add",
        primitive: "State::node_add_staged",
        mode: "the C API's add",
        tier: Tier::Shared,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Staged],
        // Reached only on paths the runs rarely take:
        // - Allocator: a grab retesting a retired block, or contention (slots.rs, retire)
        sections: &[
            Section::Allocator,
            Section::Grab,
            Section::Publish,
            Section::Release,
            Section::UnservedMark,
        ],
    }

    /// The kind of [`ops::staged_modify`].
    STAGED_MODIFY {
        name: "staged modify",
        primitive: "State::node_modify_staged",
        mode: "the C API's modify",
        tier: Tier::Shared,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Staged],
        sections: &[Section::StagedModify, Section::UnservedMark],
    }

    /// The kind of [`ops::staged_delete_level`].
    STAGED_DELETE {
        name: "staged delete",
        primitive: "State::moves_permit, State::node_delete_level",
        mode: "the C API's delete of a file",
        tier: Tier::Shared,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: false,
        writes: &[Writes::Staged, Writes::Permitted],
        sections: &[Section::DeleteLevel, Section::UnservedMark],
    }

    /// The kind of [`ops::restore`].
    RESTORE {
        name: "restore",
        primitive: "State::node_restore",
        mode: "the C API's add of a deleted name",
        tier: Tier::Shared,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[],
        // Reached only on paths the runs rarely take:
        // - UnservedMark: a climb past an ancestor lacking Staged or Dirty (rewrite.rs, climb_from)
        sections: &[Section::Restore, Section::UnservedMark],
    }

    /// The kind of [`ops::staged_move`] and [`ops::staged_move_planned`].
    STAGED_MOVE {
        name: "staged move",
        primitive: "State::moves_permit, State::node_move_staged",
        mode: "the C API's move, after its plan's walk of the destination's ancestors or walking \
               them itself",
        tier: Tier::Shared,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: true,
        writes: &[Writes::Staged, Writes::Permitted],
        sections: &[Section::MoveNamed, Section::RenameInPlace, Section::UnservedMark],
    }

    /// The kind of [`ops::pend`].
    PEND {
        name: "pend",
        primitive: "State::moves_permit, State::node_discard_pending",
        mode: "the C API's delete of an added directory",
        tier: Tier::Shared,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: false,
        writes: &[Writes::Pends, Writes::Permitted],
        sections: &[Section::Detach, Section::DiscardMarks],
    }

    /// The kind of [`ops::delete_batch`]'s own steps.
    DELETE_BATCH {
        name: "delete batch",
        primitive: "State::moves_permit",
        mode: "the C API's delete of a batch, its entries read, its State::node_delete_level and \
               State::node_discard_pending steps each a call of its own in its window",
        tier: Tier::Shared,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: false,
        writes: &[Writes::Permitted],
        sections: &[],
    }

    /// The kind of each level of a node in [`ops::delete_batch`].
    DELETE_LEVEL {
        name: "delete level",
        primitive: "State::node_delete_level",
        mode: "a tracked node of a C API delete's batch staged, under its batch's moves permit, \
               then its children read from the chain head its mark read",
        tier: Tier::Shared,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: false,
        writes: &[Writes::Staged, Writes::Permitted],
        sections: &[Section::DeleteLevel, Section::UnservedMark],
    }

    /// The kind of each detach of an added root in [`ops::delete_batch`].
    DETACH_ADDED {
        name: "detach added",
        primitive: "State::node_discard_pending",
        mode: "an added root of a C API delete's batch, under its batch's moves permit",
        tier: Tier::Shared,
        role: Role::Writer,
        runs: Runs::Beside,
        cancellable: false,
        writes: &[Writes::Pends, Writes::Permitted],
        sections: &[Section::Detach, Section::DiscardMarks],
    }
}
