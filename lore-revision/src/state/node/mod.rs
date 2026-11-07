// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A State's node operations: the code that changes nodes, their chains and their flags.
//!
//! Many tasks change one State at once: a VFS provider's requests beside its own settles and swaps,
//! the C API's verbs on one handle, staging's parallel walks. The rules that keep the tree whole
//! while they race, the contract below, hold only if every change follows them, so every change
//! goes through this module: a node block's write methods are visible to `crate::state` alone,
//! where the block type is defined (`state::block`), so a writer outside it does not compile.
//! Beside this module, only the State's save and load, the root record's reset before a save
//! (`update_tree_root_hash`) and the clear of marks above a discarded node (`clear_propagation`)
//! write a block, and under `test-util` the tests, through `NodeBlock::write_for_tests` and the
//! write-path items that build makes public (`#[lore_macro::test_pub]`), the locks a section
//! takes and the slot grab among them.
//!
//! # Tiers
//!
//! Callers differ in what runs beside them, so each tier keeps the guarantees its callers need and
//! no more. A body shared between tiers is generic over the tier, so none tests its tier at
//! runtime.
//!
//! - **Served** ([`State::served`]): VFS requests on a State a provider serves, and the provider's
//!   own changes there but a swap's changes from the target. The provider's settles and swaps trust
//!   the dirty flags at every moment, so every chain change and every dirtying here keeps the
//!   served chain rule (C11).
//! - **Swap** ([`State::swap`]): a swap's apply and settle on a served State. Its changes from the
//!   target publish only clean copies, which the tier checks, detach only with `IfClean`, `IfEmpty`
//!   or `At`, which its caller keeps to, and never reattach, so they need only a parent not flagged
//!   `Discarded`. A swap also gives a clean file the target's content in place through the
//!   unserved [`State::node_rewrite`] with `Rewrite::IfClean`, which changes no chain and no flag.
//! - **Unserved**: the methods on [`State`], for the C API's verbs, staging, committing, the scan,
//!   the other commands, and the writers that build a State before it is served, which run as one
//!   task with no other reference to the State. Under a shared claim (the C API's add, modify, move
//!   and delete) writers run in parallel and no published slot is freed. Under an exclusive claim
//!   (stage, commit, merge, reset, unstage, the scan, `lore dirty` and the other commands that
//!   write a State) the slots of subtrees no other task references may be freed.
//! - **Discard** (`node_discard_pending`): the C API delete's detach of a subtree its handle added,
//!   which marks no parent, adding the subtree's root having marked it already.
//!
//! Saving a State is no tier: it runs no node operation, and no writer runs beside it (C14).
//!
//! The VFS provider for Linux (`crate::fs::fuse`) serves a State through the served and swap
//! tiers. It does not sweep that State yet (C15), so a slot a request left detached or in flight
//! stays until the next mount rebuilds the State.
//!
//! # Terms
//!
//! - **Live:** reachable from the root.
//! - **Detached:** flagged `Discarded` and in no chain.
//! - **In flight:** taken off its block's free list and not yet published, or in a tree under
//!   construction whose root is still `Discarded`.
//! - **Pending:** in a subtree the C API delete detached and flagged, waiting for the C API commit
//!   to free it.
//! - **Free:** on its block's free list.
//! - **Orphan:** a node attached or dirtied below a node after that node was detached. A swap's
//!   detach of a clean subtree (`IfClean`) leaves the subtree hanging below its detached root: its
//!   nodes are members of a detached subtree, not orphans.
//!
//! # Contract
//!
//! Each clause has tests: its semantics in `lore-revision`'s unit tests, its races in the
//! deterministic scenarios (`tests/state_scenarios`, and the C API's in the `lore` package) and in
//! the stress suite (`scripts/state-stress`).
//!
//! - **C1. One chain.** A live node is in exactly one chain at every moment. No node is ever in
//!   two.
//! - **C2. Publishing.** A node is filled in whole while in flight, still `Discarded`; publishing
//!   it clears the flag and prepends it in one section under its parent's and its own block write
//!   locks, so nothing sees a node before it is whole.
//! - **C3. Slots.** No published slot is freed under a shared claim, nor on a served State but by a
//!   sweep where the provider has paused its requests (C15); its owner may release an in-flight
//!   slot. A detached record keeps its `sibling`, so a reader that copied its predecessor walks on
//!   through it, and keeps its name through name-table repacks. So between sweeps on a served
//!   State, and while a shared claim lives, a node ID stays the same node, and a chain mark
//!   compared by ID never meets a reused slot; a sweep that frees one makes every older mark and
//!   absence proof stale. Staging, under its exclusive claim, defers its discards to its end, so
//!   its marks never meet one either.
//! - **C4. Order in a chain.** Adds and attaches prepend. Reattaching a node, and moving it to
//!   another directory, re-prepend it; renaming it in place (`node_rename`, or `node_move` within
//!   its directory) keeps its position. A search for a name added since a mark
//!   (`find_subnode_added_since`) takes a [`ChainMark`](crate::state::ChainMark): the directory's
//!   head and change count, read together where its listing began, with the directory's block held
//!   so the count cannot restart under the mark. The search walks from the head to the mark's head,
//!   or the whole chain where that head was detached, and answers a miss *stale* where the count
//!   grew by a move's weight, so it never silently misses a node moved behind its walk. A node it
//!   meets carrying the name is the answer, whatever moved. A mark of another State's block, or of
//!   a node no longer a directory, is stale. Staging counts its own adds into a marked directory
//!   (`ChainMark::count_own_prepend`), so they never make its search stale nor show as moves.
//!   Staging's claim of a child answers from its listing where that holds the name unclaimed, else
//!   from the search. A checked attach or reattach whose publish found the chain changed searches
//!   again with a mark read before its lookup that missed (`find_subnode_since`): on a served
//!   State and under a shared claim C3 keeps the mark's head the same node; under an exclusive
//!   claim the search rests on C9, no other writer adding the name, and on frees running where no
//!   lookup does.
//! - **C5. Counts and logs.** Every chain change but those this clause exempts is counted on its
//!   directory and, where a listing watches the directory, logged; or the log is skipped forward.
//!   Prepending or detaching a node counts 1, the discard tier's detach included. Moving a node
//!   out, or renaming it to another name hash, counts
//!   [`REATTACH_CHANGES`](crate::state::REATTACH_CHANGES), and respelling it in place counts 0.
//!   Respelling through a reattach re-prepends: detaching counts a move and prepending 1. A
//!   replaced holder's detach counts a move across directories and 0 within one. A discard's detach
//!   of its root (`node_discard_fused`, `node_discard_free`), a build's private add and abandoning
//!   a build count nothing. Counts belong to the slot and survive freeing and taking it again. A
//!   count is a `u32` per directory, kept in memory per block and restarting when the block loads
//!   again, so every comparison carries the block's `Arc` and compares it by pointer. A false
//!   "unchanged" needs the count to come back to the same value, 65,536 moves out within one
//!   comparison window: accepted. Counts are stored with Release and loaded with Acquire, since
//!   some readers load them without the block lock.
//! - **C6. Walks.** A walk whose head and count were read together, and whose count is unchanged at
//!   its end, has a valid answer. A caller walking a parent it reads anyway takes the parent's
//!   record from the walk (`with_parent`, `with_parent_in`). A caller holding a record read earlier
//!   walks from a fresh read (`new`), never from that record's head.
//! - **C7. Replacing.** While a reattach replaces the node holding its name, a lookup of the name
//!   finds one of the two nodes at every moment.
//! - **C8. Unique names on a served State.** Each checked attach, move and rename decides under its
//!   locks, against a walk checked by the chain's count or a listing's log, so it takes effect at
//!   one moment within its call, and a served directory never holds two live children of one name.
//! - **C9. Unserved writers.** Unserved callers keep each `(parent, name)` they add or move a node
//!   to distinct. Parallel writers may share ancestors: every change of several fields is under its
//!   blocks' write locks, and marks are idempotent bit sets. An unserved mark writes a node before
//!   it reads its parent, and an unserved clear writes a directory before it reads its children
//!   again, each under the block's lock, so dirty closure holds at every quiescent point among
//!   marks, clears, detaches, and adds whose parent is marked after the node is published. A writer
//!   that marks a parent before it dirties a node below it does not run beside a clear. The clear
//!   of marks above a discarded node reads before it writes, so it runs with no mark beside it
//!   (Limitations).
//! - **C10. The moves permit.** A directory changing parent through a reattach, every C API move
//!   (`node_move_staged`), a C API restore below an added directory, and every C API delete hold
//!   the State's moves permit. So two directories never move into each other's subtrees, but
//!   through `node_move`, whose callers rule that out, and C API moves and deletes never
//!   interleave.
//! - **C11. Dirty closure on a served State.**
//!   - (a) The served chain rule: every chain change and every dirtying requires, under the
//!     parent's block lock, a parent not flagged `Discarded`, dirty and with no clear pending. One
//!     inside a detached subtree passes, which leaves an orphan. The root always qualifies. The
//!     swap tier is exempt for its changes from the target.
//!   - (b) A directory turns clean only through `node_clear`'s phases. The second compares its
//!     children, all clean, none with local content, as a multiset of name hash, kind, content hash
//!     and, for files, mode, with the committed (or target) directory's. The last leaves it dirty
//!     unless the comparison held and it finds the directory live at its place; its name byte for
//!     byte the committed directory's, or for a swap the pre-swap revision's; its pending-clear bit
//!     intact, the bit any served change below it drops; and its count and block unchanged since
//!     the first phase.
//!   - (c) Hence, at every moment and after any cancellation, every live ancestor of a live dirty
//!     node but the root is dirty, and a clean served directory's children equal its committed
//!     children, but along a swap's changed paths from its apply until its settle. No mark changes
//!     the root's flags: one that reaches the root dirties block 0. So detaching `IfClean` never
//!     removes a dirty descendant, and settling never loses a chain change.
//! - **C12. Cancellation.** A dropped future leaves C1 to C8, C10 and C11 true, and C9's closure
//!   but for the unserved climbs under Limitations. A call that fails part way leaves the same, and
//!   a climb that fails on a block it cannot load leaves what Limitations, Unserved climbs,
//!   describes. At worst it leaves:
//!   - an in-flight slot or an unpublished tree: `Discarded` slots in no chain and on no free
//!     list, which the next sweep frees (C15);
//!   - a dirty directory that stays dirty;
//!   - for a C API add, modify, move or delete dropped after its first mark and before it changed
//!     its node, directories marked with no change below them. The call notes the change
//!     unfinished, so the C API commit counts only nodes carrying a staged action or merge state.
//!     For a move dropped after it moved, descendants without their staged-move marks;
//!   - for a VFS provider's commit dropped after it armed its settle, removals that keep recording
//!     their deletions for the rest of the mount.
//! - **C13. Locks.**
//!   - Order: a VFS provider's inode table shard guards, then the State's runtime lock, then node
//!     block locks in ascending order, at most six, or a file-metadata block lock. The State's data
//!     lock is taken alone.
//!   - The C API takes its shared claim before the moves permit. tokio's `RwLock` is fair, so a
//!     reader queues behind a waiting writer, and the reverse order deadlocks against a waiting
//!     commit.
//!   - A name read in place, a block read guard, is never held across a block lookup or any other
//!     lock, which would invert the block and runtime locks against retiring a full block. Nor is
//!     one taken under a guard of its own block, as the locks are not reentrant: a writer reads
//!     names through its own guard.
//!   - A section registers its blocks with the State only once its guards are dropped.
//!   - Nothing awaits under the inode table's, runtime, data or block locks but a save, which holds
//!     a node block's upgradable read and then its read across the store writes (C14), with no
//!     writer beside it. Borrowed block guards are not `Send`, so a `Send` future holding one
//!     across an await does not compile; the owned ones, a name read in place (`NodeNameLock`) and
//!     a save's holds, are `Send`, and only `state-hooks` catches a task parking with one.
//!   - Under `state-hooks`, the module asserts the block locks' ascending order and, where a
//!     scheduler is installed on the thread, that no task reaches a scheduling point holding a
//!     block guard, and the deterministic scheduler panics where a task finds a block's lock held
//!     (`contended`).
//! - **C14. Persistence.**
//!   - Each block's edit sequence is recorded with the image a save writes, and releasing the
//!     blocks after the save keeps every block whose sequence moved registered, and the State
//!     dirty. A node block's save holds it from writing its name table to taking its image under
//!     one upgradable read, so no edit lands between the two.
//!   - The State's dirty flag clears with its data written, before the release. A save that fails
//!     or is dropped in between marks the State dirty again.
//!   - A block that frees a slot while full stays registered until a save threads it onto the
//!     unused chain. The save threads such blocks right behind the chain's head, under the
//!     unused-slot permit, never in front of it: a full head behind another block would be threaded
//!     a second time and close a cycle.
//!   - A save repacks a block's name table where a name was appended to it, skipping only free
//!     slots, so detached and in-flight records keep their names.
//!   - Every path that turns a block dirty marks the State on that transition and registers the
//!     block once, by its registered bit, so a save never misses an edit. One write does not: the
//!     commit's reset of the root record's hash and size before its save (`update_tree_root_hash`),
//!     which runs under the commit's exclusive claim.
//!   - The pending-clear bit is never stored.
//!   - A save beside writers takes each block's image at its own moment, never one moment's
//!     snapshot of the tree; no save runs beside writers.
//! - **C15. Sweeps.** Nothing but a sweep frees a detached slot, nor a slot a dropped or failed
//!   call left in flight, and a save stores them as they are, so each would keep its record and
//!   name for good. A sweep frees each with the subtree below it.
//!   - Flags: a block flags itself, under the write lock its section holds, where a slot in it
//!     turns detached or is taken, and when it loads, as an earlier session may have left one. The
//!     State notes each block it loads or registers; a sweep scans each noted block in memory whose
//!     flag is set, once, and takes the flag. A block evicted first is scanned when it next loads.
//!   - Roots: a slot flagged `Discarded` and holding a node, on no free list and below its block's
//!     node count. Its subtree runs through the chains its nodes kept: never the root's own
//!     `sibling`, which is the chain it left; a chain ends at a slot holding no node, as a discard
//!     that fails part way frees in place, at a node of another parent, or at a cycle. A discard
//!     that fails on a block it cannot load makes the rest of its directory's chain the
//!     directory's, so the sweep reaches it. Links are leaves.
//!   - Points: where nothing else runs on the State, as a sweep frees every slot in flight. Each
//!     sweep drains the pending queue first. The unserved sweep (`State::node_sweep_detached`),
//!     under an exclusive claim: at the end of `file::stage::stage`, in the CLI commit after its
//!     discards, and in the C API commit after its last refusal, each before it saves the State
//!     it is of (Limitations, Linked and layer States). The served sweep
//!     (`ServedNodes::node_sweep_detached`): where every request, settle and swap the provider
//!     began has finished, none suspended part way, and it holds no unpublished build; it keeps
//!     whole every subtree any node of which the provider retains, the kernel still referencing
//!     it.
//!   - A root whose subtree reaches a block that fails to load, or that the provider retains, is
//!     kept and its block flagged again; a root nested in a kept subtree is kept with it.
//!   - The drain's frees fall in one poll, and so do the flags and notes the scan takes, the roots
//!     it flags again and its frees, each after the last await before them: a sweep dropped part
//!     way leaves every flag, and a drain dropped before its frees leaves the queue.
//!   - A freed slot's chain log is emptied, so a directory or a build taking the slot again
//!     inherits no patch.
//!   - Every chain mark and absence proof records the State's reclaim count, which a sweep's
//!     drain or scan moves where it frees a slot, in the poll of its frees, and answers stale once
//!     it moved.
//!
//! # Limitations
//!
//! What the contract leaves to its callers, or accepts:
//!
//! - **Staging's listings.** No one else writes a directory staging holds a listing of: a
//!   precondition, not a check. Staging deserializes a State of its own under the instance's
//!   per-path write lock, its tasks each list and write their own directory, and the C API writes
//!   an in-memory State of its own. One exception is reachable: a repository linked at two mount
//!   paths gives two staging tasks the one linked State, so both may add the same name. No writer
//!   runs beside a listing of staging's, so staging leaves four races open: it takes a child its
//!   listing answered as given though the child was renamed, moved or removed since; its update's
//!   reads of the child race a rename, where the case-mismatch path renames the child back or moves
//!   the file; its branch for a nested repository's root queues the claim's answer unchecked; and
//!   its unclaimed pass takes a child moved out for absent.
//! - **Unserved climbs.** An unserved mark dropped between levels, a clear dropped between clearing
//!   an ancestor and marking it again, and an add dropped between publishing its node and marking
//!   the parent leave a dirty node below a clean directory. A climb that fails midway, on a block
//!   it cannot load, leaves the levels below marked and those above not, so a later change stops at
//!   the marked levels and a commit misses it. The C API's add, modify, delete and move mark first,
//!   but for a restore, which climbs right after it changes its node, so dropping them once the
//!   climb is done leaves none of this, though a climb of theirs dropped while it loads a block, or
//!   failing on one it cannot load, still can; the delete's batch runs in a task a dropped call
//!   does not stop.
//! - **Clearing above a discarded node.** `clear_propagation` reads an ancestor's children and then
//!   clears the ancestor with nothing held between (H21's pattern), so it runs only with no mark
//!   beside it: after a walk's tasks drain (`apply_pending_discards`), and in `lore dirty`'s loop
//!   over its paths, one at a time.
//! - **No clear beside a C API move.** The C API move marks its destination before it moves, and a
//!   clear between could clean it. No C API verb clears beside it: the C API commit, the only one
//!   that clears, holds the handle's exclusive claim.
//! - **Linked and layer States.** A stage and the commits sweep the State they are of, not the
//!   linked and layer States they save beside it, so a slot one of those calls left detached or in
//!   flight there stays until a sweep of that State.
//! - **Chain logs.** A log keeps its largest ring while its block is in memory, at most
//!   [`crate::node::CHAIN_LOG_MAX`] patches of 24 bytes for each directory a listing watched, even
//!   once no listing watches it or its slot is freed: a block of watched directories may hold 12
//!   MiB of logs.
//! - **Detached names.** A detached record keeps its name (C3) until a sweep frees it (C15), so a
//!   served State's name tables keep the name of every node it detached until the provider's next
//!   sweep that finds it released.
//! - **Closing a C API handle** does not wait for the aborted tasks of a dropped call, which only
//!   read the store and change the closed tree's in-memory State.
//! - **A C API delete's batch task that panics** drops its claim as it unwinds, and a task of its
//!   fan-out running on another thread then goes on until it next yields, beside whatever takes
//!   the claim next.
//! - **A stale grab.** Taking a slot (a grab) reads the unused chain's head first. A grab whose
//!   read spans a block's retirement, a free in it and a save threading it can take that block's
//!   last free slot once the block is back on the chain; the block, full on the chain, is then
//!   threaded again when it next frees a slot, closing a cycle (C14). It is latent, as no save runs
//!   beside writers; closing it would cost every grab a check of the head under its block's lock.
//!
//! # Hazards
//!
//! The races and defects the operations are built to close, named so the code and its tests can
//! refer to them. H3 and H24 are unused.
//!
//! - **H1.** A move leaving its node in no chain between detaching and prepending it.
//! - **H2.** A refused conditional detach marking the parent before testing its condition.
//! - **H4.** An edit during a save losing its registration, or the unused head written back stale.
//! - **H5.** A move attaching its node to the new parent though the old chain did not reach it.
//! - **H6.** Freeing a subtree's nodes before detaching its root, so an error between leaves an
//!   attached directory over freed slots.
//! - **H7.** A mark stopping at an ancestor another climb marked but has not yet climbed above, so
//!   detaching `IfClean` removes a subtree holding a dirty node.
//! - **H8.** A dropped add leaving a typed slot in no chain, or a dropped change leaving the
//!   directories it changes below unmarked, so a commit misses it.
//! - **H9.** A search for names added since a mark missing a node moved to the front of the chain,
//!   or comparing a count that restarted when its block loaded again.
//! - **H10.** A walk taking its head from an earlier record and its count fresh.
//! - **H11.** Removing or replacing a directory on an emptiness check made outside its lock while a
//!   create or a swap attaches below it. RMDIR closes it by detaching under the directory's lock
//!   (`Detach::IfUnchanged`); a rename over a directory still replaces it unconditionally
//!   (`ReattachCheck::Replacing` takes no condition), a window between the VFS host layer's
//!   emptiness check and the State step.
//! - **H12.** A stale whole-record write breaking a chain a primitive changed meanwhile.
//! - **H13.** A directory's stale copy, `child` included, written over a concurrent add.
//! - **H14.** A copy published before it is filled, so a VFS create of one of its names duplicates
//!   it.
//! - **H15.** A settle cleaning a directory whose chain changed since it was staged, losing a
//!   removal or a move.
//! - **H16.** A dropped commit, or a failed mark, leaving a clean directory over a dirty child.
//! - **H17.** A link's pin written without its `child`.
//! - **H18.** The C API delete freeing slots while adds, modifications and moves run under the same
//!   shared claim.
//! - **H19.** A settle finding directories by path, so a rename during the commit's upload gives
//!   one directory another's committed content. Closed by a VFS provider's settle, which hands
//!   directories over by node; what a remount rebuilds is the provider's host layer's.
//! - **H20.** A settle leaving a committed directory dirty with its action bits, so removing it
//!   later is not recorded: a commit dropped or failed before its settle completes, a settle error,
//!   or an empty added directory never settled. Likewise an added node moved over one the revision
//!   holds, which takes that path over.
//! - **H21.** A clear checking a directory's children and then clearing it with nothing held
//!   between, losing a mark made in between.
//! - **H22.** A swap dropped while filling a copy published clean, leaving a clean partial
//!   directory.
//! - **H23.** A swap's copy recording a path as landed where a user's directory took the name, so
//!   the target's children never arrive.
//! - **H25.** Flipping a file to serve its committed content through an inode that stands for
//!   another node. Closed in a VFS provider's inode table, not in this module.

pub(in crate::state) mod attach;
pub(in crate::state) mod clear;
pub(in crate::state) mod detach;
pub(in crate::state) mod discard;
#[cfg(feature = "state-hooks")]
pub mod hooks;
pub(in crate::state) mod in_place;
pub(in crate::state) mod locks;
pub(in crate::state) mod mark;
pub(in crate::state) mod pending;
pub(in crate::state) mod relocate;
pub(in crate::state) mod rewrite;
pub(in crate::state) mod slots;
pub(in crate::state) mod sweep;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use crate::node::NodeBlock;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::state::Ext;
use crate::state::Mark;
use crate::state::State;
use crate::state::StateError;

/// Whether this build carries the scheduling hooks of `state-hooks`. Benchmarks assert it is
/// not set.
pub const HOOKS_ENABLED: bool = cfg!(feature = "state-hooks");

/// `$future`, for a task spawned on behalf of the running one: with `state-hooks`, the
/// scheduler's `hooks::inherit` of it; without, the expression itself. A macro, so a spawn moves
/// the future no more than it would bare: an identity function, inlined, still copied it once more
/// per spawn, measured at 20 instructions per discard of directories spread over blocks.
#[cfg(feature = "state-hooks")]
#[macro_export]
macro_rules! subtask {
    ($future:expr) => {
        $crate::state::node::hooks::inherit($future)
    };
}

/// `subtask!` without `state-hooks`: the future itself.
#[cfg(not(feature = "state-hooks"))]
#[macro_export]
macro_rules! subtask {
    ($future:expr) => {
        $future
    };
}

/// The tier a node operation's body runs in (the module docs' Tiers). Each tier has its own entry
/// points, and a body shared between tiers is generic over the tier, so each compiles its own copy
/// and none switches tier at runtime. The marks are the tier's own futures, so a body's future
/// holds only the marks its tier makes.
pub(in crate::state) trait Tier {
    /// Served: every chain change and dirtying requires a parent not flagged `Discarded` that is
    /// dirty and has no clear pending (C11a).
    const SERVED: bool;

    /// Under `state-hooks`, whether a detach of the tier is the C API delete's, whose root goes on
    /// the pending queue: its log records the queueing (`Pend`).
    #[cfg(feature = "state-hooks")]
    const PENDS: bool = false;

    /// The served mark of directory `node`, held in `block` below `parent`, before a change
    /// below it that the served chain rule refused; nothing in the tiers without the rule.
    fn served_mark<'a>(
        _state: &'a State,
        _repository: &'a Arc<RepositoryContext>,
        _node: NodeID,
        _below: (&Arc<NodeBlock>, NodeID),
    ) -> impl Future<Output = Result<(), StateError>> + Send + 'a {
        NoMark
    }

    /// [`Self::served_mark`] reading only blocks in memory, for the resident attach: whether the
    /// mark is made, `false` where it needs a block not in memory, which it does not read; made
    /// at once in the tiers without the rule.
    fn served_mark_resident<'a>(
        _state: &'a State,
        _repository: &'a Arc<RepositoryContext>,
        _node: NodeID,
        _below: (&Arc<NodeBlock>, NodeID),
    ) -> impl Future<Output = Result<bool, StateError>> + Send + 'a {
        MarkMade
    }

    /// The mark of a chain change's parent before the change, ancestors included, as the
    /// unserved tier makes it; nothing in the served, swap and discard tiers: the served tier
    /// marks by the chain rule, the swap tier's changes from the target need only a parent not
    /// flagged `Discarded`, and the discard tier's parent is marked already.
    fn premark<'a>(
        _state: &'a State,
        _repository: &'a Arc<RepositoryContext>,
        _node: NodeID,
    ) -> impl Future<Output = Result<(), StateError>> + Send + 'a {
        NoMark
    }
}

/// The mark of a tier that makes none there: ready at once and holding nothing, so a body awaiting
/// it keeps no more across that point than it holds anyway.
pub(in crate::state) struct NoMark;

impl Future for NoMark {
    type Output = Result<(), StateError>;

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(Ok(()))
    }
}

/// [`Tier::served_mark_resident`]'s answer in a tier that makes no mark: made, ready at once and
/// holding nothing, as [`NoMark`].
pub(in crate::state) struct MarkMade;

impl Future for MarkMade {
    type Output = Result<bool, StateError>;

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(Ok(true))
    }
}

/// The unserved tiers: the C API, stage, commit and the other commands, and the writers before
/// a State is served.
pub(in crate::state) struct Unserved;

/// The served tier: VFS requests on the State a provider serves.
pub(in crate::state) struct Served;

/// The swap tier: a swap's apply and settle, whose changes from the target publish and detach
/// clean nodes and so need only a parent not flagged `Discarded`.
pub(in crate::state) struct Swap;

/// The tier of a C API delete's detach of an added node under a shared claim: unserved, and it
/// marks no parent, the parent being marked already by the add of the node it detaches.
pub(in crate::state) struct Discard;

impl Tier for Unserved {
    const SERVED: bool = false;

    fn premark<'a>(
        state: &'a State,
        repository: &'a Arc<RepositoryContext>,
        node: NodeID,
    ) -> impl Future<Output = Result<(), StateError>> + Send + 'a {
        state.node_mark(
            repository,
            node,
            Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
        )
    }
}

impl Tier for Served {
    const SERVED: bool = true;

    fn served_mark<'a>(
        state: &'a State,
        repository: &'a Arc<RepositoryContext>,
        node: NodeID,
        below: (&Arc<NodeBlock>, NodeID),
    ) -> impl Future<Output = Result<(), StateError>> + Send + 'a {
        state.served_mark_below(repository, node, below)
    }

    fn served_mark_resident<'a>(
        state: &'a State,
        repository: &'a Arc<RepositoryContext>,
        node: NodeID,
        below: (&Arc<NodeBlock>, NodeID),
    ) -> impl Future<Output = Result<bool, StateError>> + Send + 'a {
        state.served_mark_below(repository, node, below)
    }
}

impl Tier for Swap {
    const SERVED: bool = false;
}

impl Tier for Discard {
    const SERVED: bool = false;
    #[cfg(feature = "state-hooks")]
    const PENDS: bool = true;
}

/// The served tier's entry points, for the VFS requests on a served State.
#[derive(Clone, Copy)]
pub struct ServedNodes<'a> {
    pub(in crate::state) state: &'a State,
}

/// The swap tier's entry points, for a swap's apply and settle on a served State.
#[derive(Clone, Copy)]
pub struct SwapNodes<'a> {
    pub(in crate::state) state: &'a State,
}

impl State {
    /// The served tier's entry points.
    pub fn served(&self) -> ServedNodes<'_> {
        ServedNodes { state: self }
    }

    /// The swap tier's entry points.
    pub fn swap(&self) -> SwapNodes<'_> {
        SwapNodes { state: self }
    }
}
