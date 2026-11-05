// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The log of the node operations: what each write section of the module did to a State,
//! numbered so a model can replay it, told to the scheduler installed on the thread while it
//! records. Built only with `state-hooks`.
//!
//! Every write hold of a block takes one `seq` from its State, once its locks are taken and before
//! any write, and numbers its records `sub` 0, 1, … in order. A section of several locks
//! ([`multi`]) takes one for all of them, after its last lock, its writers taking none of their
//! own. A record made with no section open takes a `seq` of its own.

use std::cell::Cell;
use std::cell::RefCell;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore_base::types::Hash;
use smallvec::SmallVec;

use super::HOLDER_SLOTS;
use super::Holder;
use super::Mode;
use super::Resource;
use super::StateHooks;
use super::task;
use super::with_current;
use crate::node::Node;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::state::StateError;

/// A record of the log: what one section, or one step of a State's life, did.
#[derive(Clone, Debug)]
pub struct Record {
    /// The section's place in the State's order: one per write hold, taken once its locks are.
    pub seq: u64,
    /// The record's place within its section.
    pub sub: u32,
    /// The label of the section writing it.
    pub section: Section,
    /// The State's process-unique id ([`StateHooks::id`]).
    pub state: u64,
    /// The scheduler's task the record was made in, `None` outside one.
    pub task: Option<usize>,
    pub effect: Effect,
}

/// What a record says.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// A slot taken from its block: the free list's head popped, or the slot at `node_count`.
    Grab { node: NodeID, instance: u64 },
    /// A slot pushed on its block's free list.
    Free { node: NodeID, instance: u64 },
    /// A fresh block installed, past the tree's.
    Allocated { block: usize, instance: u64 },
    /// A block installed from the store, or `zeroed` where the tree stores no image of it.
    Loaded {
        block: usize,
        instance: u64,
        zeroed: bool,
    },
    /// A node in flight published at the head of its parent's chain.
    Publish {
        node: NodeID,
        parent: NodeID,
        name_hash: u64,
        name: Name,
        kind: Kind,
        check: Check,
        expected: Expected,
        /// The parent's block's instance, whose count the publish bumps.
        instance: u64,
    },
    /// A build's node added below its parent in the unpublished tree.
    BuildAdd {
        node: NodeID,
        parent: NodeID,
        name_hash: u64,
        name: Name,
        kind: Kind,
    },
    /// A build's tree cut loose; its frees follow.
    Abandon { root: NodeID },
    /// A node taken out of its parent's chain.
    Detach {
        node: NodeID,
        parent: NodeID,
        how: How,
        expected: Expected,
        /// The parent's block's instance.
        instance: u64,
    },
    /// The C API delete's detached root, queued pending.
    Pend { root: NodeID },
    /// A node marked as its discard reaches it.
    Discarding { node: NodeID },
    /// A node moved to the head of `to`'s chain; `from == to` within one directory. `name` is
    /// the name the move gave it, `None` where it kept its own, which its block may hold no table
    /// for in memory.
    Move {
        node: NodeID,
        from: NodeID,
        to: NodeID,
        name_hash: u64,
        name: Option<Name>,
        replaced: Option<NodeID>,
        check: Check,
        expected: Expected,
        /// The instances of `from`'s and `to`'s blocks.
        instances: (u64, u64),
    },
    /// A node renamed in place, keeping its position.
    Rename {
        node: NodeID,
        parent: NodeID,
        name_hash: u64,
        name: Name,
        /// The parent's block's instance.
        instance: u64,
    },
    /// A link's pin set: the whole pin, its address and `child`, or the revision alone.
    Repin {
        node: NodeID,
        child: NodeID,
        whole: bool,
    },
    /// The State made, from `(repository, revision)` for a load of a stored one, the repository
    /// named by its context's address, which names it only while the context lives: within a run.
    Created { from: Option<(usize, Hash)> },
    /// The State served: the served rules apply from here.
    Served,
    /// One attempt of a served conditional detach tested its node.
    PreTest {
        node: NodeID,
        attempt: u32,
        refused: bool,
        expected: Expected,
    },
    /// The State's unfinished-change flag set or cleared.
    Unfinished { set: bool },
    /// An operation's `UnfinishedChange` armed, finished, or dropped armed.
    Guard { event: GuardEvent },
    /// A node's words as a section left them, where they changed: its record, and its block's
    /// pending clear bit for it.
    Word {
        node: NodeID,
        record: Words,
        clear_pending: bool,
    },
    /// A section refused on what it read under its lock. `parent` is the directory the decision
    /// read, or `node` where it read only the node's own word; `read` is what it decided on.
    Refused {
        node: NodeID,
        parent: NodeID,
        reason: Reason,
        read: Read,
        expected: Expected,
    },
    /// A section admitted its change of `node` on what it read under its locks, before its
    /// `Word`s: `expected` held of the node's word, or of the words beside it `expected` names,
    /// where no other record says so.
    Admitted { node: NodeID, expected: Expected },
    /// A race arm taken.
    Arm(Arm),
    /// A reader's walk read the head of directory `node`'s chain and its change count, `changes`,
    /// together under the lock of its block's instance `instance`, as it began, at a `seq` of its
    /// own: every section counted on the chain lies wholly before this record's `seq` or after it.
    Head {
        node: NodeID,
        changes: u32,
        instance: u64,
    },
    /// A reader read `node`'s parent word as `parent` under its block's read lock, at a `seq` of
    /// its own: a path's walk up its parents, one hop at a time.
    Parent { node: NodeID, parent: NodeID },
    /// A reader read `node`'s name as `name` through the State's name reads, under its block's
    /// read lock, at a `seq` of its own.
    Named { node: NodeID, name: Name },
    /// A path's resolution found `found` carrying the hash `name` in `dir`, or no node, at a `seq`
    /// of its own after the find: a hop of `State::find_relative_node_link`.
    Hop {
        dir: NodeID,
        name: u64,
        found: Option<NodeID>,
    },
    /// The moves permit taken.
    Permit,
    /// The moves permit about to be released.
    Unpermit,
}

/// A node's record as a record of the log carries it, compared by its bytes.
#[derive(Clone, Copy, Debug)]
pub struct Words(pub Node);

impl PartialEq for Words {
    fn eq(&self, other: &Self) -> bool {
        use zerocopy::IntoBytes;
        self.0.as_bytes() == other.0.as_bytes()
    }
}

/// A node's kind, by its flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    File,
    Directory,
    Link,
}

impl Kind {
    pub fn of(node: &Node) -> Self {
        if node.flags & NodeFlags::Link.bits() != 0 {
            Self::Link
        } else if node.flags & NodeFlags::File.bits() != 0 {
            Self::File
        } else {
            Self::Directory
        }
    }
}

/// The check a publish or a move made under its locks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Check {
    /// The parent live.
    Live,
    /// The parent live and not staged for deletion.
    Unstaged,
    /// The parent's count unchanged since a lookup.
    Unchanged,
    /// The parent's log since an absence holding no attach of the name.
    Absent,
    /// The parent's log since a position holding no attach of the name, or, for a reattach,
    /// holding the mover's own name only.
    Logged,
    /// A walk of the new parent's chain.
    Walk,
    /// The name's holder replaced.
    Replacing,
    /// No name checked under the locks: `move_named`'s; a checked move's refusal of an unmovable
    /// node or parent is a record of its own.
    Unchecked,
}

/// What a check compared against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Expected {
    /// Nothing captured: the model has what the check read.
    Nothing,
    /// A directory's chain count, read with its block's instance.
    Count { changes: u32, instance: u64 },
    /// A chain log position.
    Log { seq: u32 },
    /// `At`'s parent and name hash.
    At { parent: NodeID, name_hash: u64 },
    /// `IfEmpty`'s chain head, as read.
    Head(NodeID),
    /// The node clean: an `IfClean` change's condition.
    Clean,
    /// The node a live file below this parent, itself live but for the root: a served rewrite's
    /// condition.
    Below(NodeID),
    /// A checked move's or rename's condition: the node not staged for deletion, and the parents
    /// it leaves and joins neither Discarded nor staged for deletion.
    Movable { from: NodeID, to: NodeID },
    /// A local-only reattach's condition: the node no directory, a directory holding local
    /// content, or one respelled where it stands, at the name hashing to `name_hash` below `to`.
    Local { to: NodeID, name_hash: u64 },
    /// A delete level's condition: the node live below this parent, neither added nor staged for
    /// deletion.
    Deletable(NodeID),
}

/// How a node left its parent's chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum How {
    Always,
    IfUnchanged,
    IfClean,
    At,
    IfEmpty,
    /// A discard's detach.
    Discard,
}

/// What happened to an operation's `UnfinishedChange`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GuardEvent {
    Armed,
    Finished,
    DroppedArmed,
}

/// Why a section refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    /// The node was Dirty.
    IfClean,
    /// A directory without local content moved elsewhere than a respell.
    NotLocal,
    /// The parent staged for deletion, for a staged add; a Discarded parent is `NotFound`.
    StagedDelete,
    /// The node, or a parent it leaves or joins, unmovable.
    Unmovable,
    /// The directory's count or block changed.
    IfUnchanged,
    /// The directory held a child.
    IfEmpty,
    /// The node no longer under the parent and hash expected.
    At,
    /// Another node held the name.
    Exists,
    /// The node, or the directory it changes, Discarded.
    NotFound,
    /// A build's add into a non-directory.
    NotDirectory,
    /// A pin on a node that is not a link; a Discarded one is `NotFound`.
    NotLink,
    /// A rewrite or a modify refused.
    Rewrite,
    /// A served rewrite refused.
    ServedRewrite,
    /// A restore refused.
    Restore,
    /// `node_delete_level`'s `Other`.
    DeleteLevelOther,
    /// `node_delete_level`'s `Staged`.
    DeleteLevelStaged,
    /// An in-place change's closure answering false.
    InPlace,
    /// An in-place change's node Discarded, or no longer under the parent and hash expected.
    InPlaceAway,
    /// An owned change's closure answering false, or an owned clear changing no flag.
    Owned,
    /// An owned change's or clear's node Discarded.
    OwnedGone,
    /// The name table full.
    NameTableFull,
}

/// What a refusal decided on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Read {
    /// A node's record.
    Word(Words),
    /// A directory's count, with its block's instance.
    Count { changes: u32, instance: u64 },
    /// The name's holder, and the hash of the name it holds.
    Holder { holder: NodeID, name_hash: u64 },
    /// A chain's head.
    Head(NodeID),
    /// The name table had no room.
    NameTableFull,
}

/// A race arm: a branch an operation takes only where a race or a refusal sent it, each named
/// apart, so a test can tell which it reached.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Arm {
    AttachNeedsMark,
    AttachChanged,
    AttachUncovered,
    DetachNeedsMark,
    DetachChanged,
    DetachMoving,
    DetachMissing,
    /// A detach's mark found its node moved.
    DetachMovedMark,
    ReattachNeedsMark,
    ReattachChanged,
    ReattachMoving,
    ReattachMissing,
    /// A reattach's mark found its node moved.
    ReattachMovedMark,
    ClearKept,
    ClearMoved,
    ClearRecheck,
    FindRestart,
    SearchRestart,
    /// The served walk found its chain changed after a per-child park.
    ServedWalkChanged,
    MarkRetry,
    MarkLoad,
    MarkDeep,
    MarkWaiting,
    /// The slot allocator found the chain's head moved under the permit.
    AllocatorHeadMoved,
    /// The retire's re-test.
    RetireRetest,
    /// The served rewrite followed a moved node.
    RewriteFollow,
    CopyUpRemark,
    RestoreMovedRetry,
    ModifyMovedClimb,
    BuildPublishRetry,
    /// `node_reattach_local`'s `NotLocal` under the locks.
    NotLocalLocked,
    UnservedDetachChanged,
    UnservedDetachMoving,
    UnservedDetachMissing,
    MoveNamedChanged,
    MoveNamedMoving,
    MoveNamedMissing,
    /// `publish_private`'s branch for `into` above the node.
    PublishPrivateAbove,
    /// A chain log installed between a lookup and a publish.
    LogBetweenLookupAndPublish,
    SlowDown,
    RenameRetry,
    /// A mover beaten to its node.
    MoverBeaten,
    /// A resident find, or a resident attach, met a block not in memory, which it does not load:
    /// the attach's tree or parent block, its lookup's, its served mark's, its grab's, or its
    /// slot's names or file metadata.
    NeedLoad,
    /// `watch_chain`'s install lost to another.
    WatchLostInstall,
    /// `watch_chain`'s grow lost to another.
    WatchLostGrow,
    /// A block's lock found held, once taken.
    Contended {
        block: Resource,
        waiter: Option<usize>,
        mode: Mode,
        holders: [Option<Holder>; HOLDER_SLOTS],
    },
}

/// The label of a writing section, one per row of the write sets the stress harness's model holds
/// each section to, and the slot and save sections besides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Section {
    /// No section labelled: a write made here is a finding.
    #[default]
    Unlabelled,
    /// A slot's grab and initialization, for an attach or a build.
    Grab,
    /// An unpublished slot handed back: after a failed load or initialization, a refused
    /// publish, an attach that ended unattached, a refused build add, an abandon's frees.
    Release,
    /// A publish: an attach's, an add's, a build's root.
    Publish,
    /// A build's private add.
    BuildAdd,
    /// A build's abandon.
    Abandon,
    /// A detach, every mode.
    Detach,
    /// A served mark's level.
    ServedMark,
    /// An unserved mark and the climbs.
    UnservedMark,
    /// A reattach's section, in every tier: the served write set applies on a served State.
    Reattach,
    /// `move_named`'s section.
    MoveNamed,
    /// `rename_in_place`.
    RenameInPlace,
    /// A settle's phase 1.
    SettlePhase1,
    /// A settle's phase 3, and `PendingClear::drop`.
    SettlePhase3,
    /// An unserved clear.
    UnservedClear,
    /// A rewrite and a copy-up.
    Rewrite,
    /// A staged modify.
    StagedModify,
    /// A restore.
    Restore,
    /// The delete level.
    DeleteLevel,
    /// The pending marks, a discard's detach and its mark, and `free_below`'s marks.
    DiscardMarks,
    /// A discard's frees and a pending drain's.
    DiscardFree,
    /// `clear_propagation`.
    ClearPropagation,
    /// An in-place change.
    InPlace,
    /// An owned change or clear.
    Owned,
    /// A link's repin.
    Repin,
    /// A save's threading of the unused chain, and a commit's reset of the root's hash and size
    /// (`update_tree_root_hash`).
    Save,
    /// The slot allocator's chain upkeep: a retire, a push.
    Allocator,
    /// A sweep's scan of a block for detached slots, which writes no record.
    Sweep,
}

/// How a State installs a block ([`Effect::Allocated`], [`Effect::Loaded`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Install {
    Allocated,
    Loaded { zeroed: bool },
}

/// Whether the thread's scheduler records.
#[inline]
#[lore_macro::test_pub]
pub(crate) fn recording() -> bool {
    with_current(|scheduler| scheduler.recording()).unwrap_or(false)
}

/// A section open on this thread, and the next `sub` its records take.
#[derive(Clone, Copy)]
struct Open {
    id: u32,
    state: u64,
    seq: u64,
    sub: u32,
}

/// The most sections one thread holds open at once.
const OPEN_MOST: usize = 16;

struct Opened {
    open: [Option<Open>; OPEN_MOST],
    len: usize,
    next_id: u32,
}

impl Opened {
    /// Whether a section of the State `id` is open.
    fn holds(&self, id: u64) -> bool {
        self.open[..self.len]
            .iter()
            .flatten()
            .any(|open| open.state == id)
    }
}

thread_local! {
    static OPENED: RefCell<Opened> = const {
        RefCell::new(Opened {
            open: [None; OPEN_MOST],
            len: 0,
            next_id: 0,
        })
    };
    /// Multi-lock sections taking their locks on this thread, whose writers open no section.
    static PENDING: Cell<u32> = const { Cell::new(0) };
    /// The label of the section writing on this thread.
    static LABEL: Cell<Section> = const { Cell::new(Section::Unlabelled) };
    /// Whether a build's publish is checking on this thread ([`build`]).
    static BUILD: Cell<bool> = const { Cell::new(false) };
}

/// A section opened by [`open`], closed on drop.
#[lore_macro::test_pub]
pub(crate) struct SectionHold {
    id: u32,
    /// Not `Send`: the section is this thread's.
    _thread: PhantomData<*const ()>,
}

impl Drop for SectionHold {
    fn drop(&mut self) {
        let _ = OPENED.try_with(|opened| {
            // A drop on a path that holds the stack, which none does, leaves the entry, counted
            // for the driver to fail the run on: a panic here could be one during unwinding.
            let Ok(mut opened) = opened.try_borrow_mut() else {
                fault();
                return;
            };
            let len = opened.len;
            if let Some(at) = opened.open[..len]
                .iter()
                .rposition(|open| open.is_some_and(|open| open.id == self.id))
            {
                opened.open.copy_within(at + 1..len, at);
                opened.open[len - 1] = None;
                opened.len -= 1;
            }
        });
    }
}

/// Opens a section of `state` with a `seq` of its own, where the thread records: taken once the
/// section's locks are.
#[lore_macro::test_pub]
pub(crate) fn open(state: &StateHooks) -> Option<SectionHold> {
    open_if(recording(), state)
}

/// [`open`] for a caller that asked whether the thread records, as `recording` answers.
pub(crate) fn open_if(recording: bool, state: &StateHooks) -> Option<SectionHold> {
    if !recording {
        return None;
    }
    let seq = state.next_seq();
    OPENED.with_borrow_mut(|opened| {
        assert!(
            opened.len < OPEN_MOST,
            "more than {OPEN_MOST} sections open"
        );
        // A record goes to the innermost section of its State, so two of one State open at once
        // would number one writer's records in the other's.
        assert!(
            !opened.holds(state.id()),
            "two sections of one State open on a thread"
        );
        let id = opened.next_id;
        opened.next_id = id.wrapping_add(1);
        let len = opened.len;
        opened.open[len] = Some(Open {
            id,
            state: state.id(),
            seq,
            sub: 0,
        });
        opened.len += 1;
        Some(SectionHold {
            id,
            _thread: PhantomData,
        })
    })
}

/// A section of several locks taking them: its writers open no section of their own until
/// [`Multi::opened`] opens the section's, after its last lock.
#[lore_macro::test_pub]
pub(crate) struct Multi {
    done: bool,
    /// Not `Send`: the count of pending sections is this thread's.
    _thread: PhantomData<*const ()>,
}

/// Begins a section of several locks.
#[lore_macro::test_pub]
pub(crate) fn multi() -> Multi {
    PENDING.with(|pending| pending.set(pending.get() + 1));
    Multi {
        done: false,
        _thread: PhantomData,
    }
}

impl Multi {
    /// Every lock taken: the section's `seq`, from `state`, where the thread records.
    #[lore_macro::test_pub]
    pub(crate) fn opened(mut self, state: Option<&StateHooks>) -> Option<SectionHold> {
        self.done = true;
        PENDING.with(|pending| pending.set(pending.get() - 1));
        state.and_then(open)
    }
}

impl Drop for Multi {
    fn drop(&mut self) {
        if !self.done {
            let _ = PENDING.try_with(|pending| pending.set(pending.get() - 1));
        }
    }
}

/// Whether the thread is outside every section: no label set, none open, no section of several
/// locks taking its locks, no build's publish checking. A task parks only there, as a label or a
/// section held across a park would number or label another task's writes.
#[lore_macro::test_pub]
pub(crate) fn idle() -> bool {
    LABEL.with(Cell::get) == Section::Unlabelled
        && OPENED.with_borrow(|opened| opened.len == 0)
        && PENDING.with(Cell::get) == 0
        && !BUILD.with(Cell::get)
}

/// Whether a section of several locks is taking its locks on this thread.
#[inline]
#[lore_macro::test_pub]
pub(crate) fn in_multi() -> bool {
    PENDING.with(Cell::get) > 0
}

/// Labels the sections this thread writes in until the guard drops.
#[lore_macro::test_pub]
pub(crate) fn label(section: Section) -> Label {
    Label {
        previous: LABEL.with(|label| label.replace(section)),
        _thread: PhantomData,
    }
}

/// A label set by [`label`]; restores the one before on drop. Not `Send`: the label is this
/// thread's.
#[lore_macro::test_pub]
pub(crate) struct Label {
    previous: Section,
    _thread: PhantomData<*const ()>,
}

impl Drop for Label {
    fn drop(&mut self) {
        let _ = LABEL.try_with(|label| label.set(self.previous));
    }
}

/// Faults of the log's own upkeep, every thread's: a section left open or a writer's words lost
/// where a drop found its thread's state borrowed.
static FAULTS: AtomicU64 = AtomicU64::new(0);

/// Counts a fault of the log's upkeep ([`faults`]).
pub(crate) fn fault() {
    FAULTS.fetch_add(1, Ordering::Relaxed);
}

/// How many faults the log's upkeep met: a driver fails its run on any.
pub fn faults() -> u64 {
    FAULTS.load(Ordering::Relaxed)
}

/// Records a section's refusal for a name table with no room for `node`'s name.
pub(crate) fn name_table_full(state: &StateHooks, node: NodeID) {
    record(
        state,
        Effect::Refused {
            node,
            parent: node,
            reason: Reason::NameTableFull,
            read: Read::NameTableFull,
            expected: Expected::Nothing,
        },
    );
}

/// Records a race arm on `state`.
#[inline]
pub(crate) fn arm(state: &StateHooks, arm: Arm) {
    record(state, Effect::Arm(arm));
}

/// Records a reader's read of directory `node`'s head and count on `state` ([`Effect::Head`]),
/// made under its block's read lock.
#[lore_macro::test_pub]
pub(crate) fn head(state: &StateHooks, node: NodeID, changes: u32, instance: u64) {
    read(state, (node, "head"), || Effect::Head {
        node,
        changes,
        instance,
    });
}

/// Records a reader's read of `node`'s parent word as `parent` on `state` ([`Effect::Parent`]),
/// made under its block's read lock.
#[lore_macro::test_pub]
pub(crate) fn parent(state: &StateHooks, node: NodeID, parent: NodeID) {
    read(state, (node, "parent"), || Effect::Parent { node, parent });
}

/// Records a reader's read of `node`'s name as `name` on `state` ([`Effect::Named`]), made under
/// its block's read lock.
#[lore_macro::test_pub]
pub(crate) fn named(state: &StateHooks, node: NodeID, name: &str) {
    read(state, (node, "name"), || Effect::Named {
        node,
        name: Name::new(name.as_bytes()),
    });
}

/// Records a path resolution's hop on `state` ([`Effect::Hop`]): its find of the hash `name` in
/// `dir`, which answered `found`. A find failing other than by a miss records nothing.
#[lore_macro::test_pub]
pub(crate) fn hop(
    state: &StateHooks,
    (dir, name): (NodeID, u64),
    found: &Result<NodeID, StateError>,
) {
    let found = match found {
        Ok(node) => Some(*node),
        Err(error) if error.is_node_not_found() => None,
        Err(_) => return,
    };
    read(state, (dir, "child"), || Effect::Hop { dir, name, found });
}

/// Records a reader's read of `node`'s `what` on `state`, the record `effect` makes. A section of
/// `state` open on the thread would number it inside that section, so no section could be told
/// before or after it.
fn read(state: &StateHooks, (node, what): (NodeID, &str), effect: impl FnOnce() -> Effect) {
    if !recording() {
        return;
    }
    assert!(
        !OPENED.with_borrow(|opened| opened.holds(state.id())),
        "a reader read {node}'s {what} inside a section of its State"
    );
    record(state, effect());
}

/// A name as a record carries it: inline up to 64 bytes, so a record made inside a section
/// allocates nothing for the names a scenario uses.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Name(SmallVec<[u8; 64]>);

impl Name {
    pub fn new(bytes: &[u8]) -> Self {
        Self(SmallVec::from_slice(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        self.0.as_slice() == other.as_bytes()
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", String::from_utf8_lossy(&self.0))
    }
}

/// A name read from a block's table, empty where it is not there.
pub(crate) fn name(bytes: Option<&[u8]>) -> Name {
    bytes.map_or_else(Name::default, Name::new)
}

/// Marks a build's publish checking on this thread until the guard drops, so its `Unchanged`
/// check, meeting a chain log under the publish's locks, records the arm only a build's takes.
#[lore_macro::test_pub]
pub(crate) fn build() -> Building {
    BUILD.with(|build| build.set(true));
    Building {
        _thread: PhantomData,
    }
}

/// A build's publish checking ([`build`]). Not `Send`: the flag is this thread's.
#[lore_macro::test_pub]
pub(crate) struct Building {
    _thread: PhantomData<*const ()>,
}

impl Drop for Building {
    fn drop(&mut self) {
        let _ = BUILD.try_with(|build| build.set(false));
    }
}

/// Whether a build's publish is checking on this thread.
pub(crate) fn building() -> bool {
    BUILD.with(Cell::get)
}

/// Records `effect` on `state` where the thread records: in the innermost section open on this
/// thread for `state`, or with a `seq` of its own.
#[lore_macro::test_pub]
pub(crate) fn record(state: &StateHooks, effect: Effect) {
    // The scheduler borrowed, its count untouched: no shared write beyond the State's `seq`.
    with_current(|scheduler| {
        if !scheduler.recording() {
            return;
        }
        let id = state.id();
        let numbered = OPENED.with_borrow_mut(|opened| {
            let len = opened.len;
            opened.open[..len]
                .iter_mut()
                .rev()
                .flatten()
                .find(|open| open.state == id)
                .map(|open| {
                    let sub = open.sub;
                    open.sub += 1;
                    (open.seq, sub)
                })
        });
        let (seq, sub) = numbered.unwrap_or_else(|| (state.next_seq(), 0));
        scheduler.effect(Record {
            seq,
            sub,
            section: LABEL.with(Cell::get),
            state: id,
            task: task(),
            effect,
        });
    });
}
