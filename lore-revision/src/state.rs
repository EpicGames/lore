// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub(crate) mod block;
#[cfg(not(feature = "test-util"))]
mod diff;
#[cfg(feature = "test-util")]
pub mod diff;
pub mod dump;
pub mod node;
#[cfg(not(feature = "test-util"))]
pub(crate) mod os_diff;
#[cfg(feature = "test-util")]
pub mod os_diff;
#[cfg(not(feature = "test-util"))]
mod stream;
#[cfg(feature = "test-util")]
pub mod stream;

use core::str;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::future::Future;
use std::io::Write;
use std::mem::size_of;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bitflags::bitflags;
use bytes::Bytes;
pub(crate) use diff::DiffFlags;
pub use diff::DiffWalkStats;
pub use diff::GraftOracle;
#[cfg(target_os = "linux")]
pub(crate) use diff::diff_states_into;
pub(crate) use diff::diff_subtree_flagged;
use futures::future::Either;
use lore_base::error::InvalidPath;
use lore_base::lore_drain_tasks;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
pub use node::ServedNodes;
pub use node::SwapNodes;
pub use node::attach::Absent;
pub use node::attach::AttachCheck;
pub use node::attach::AttachOutcome;
pub use node::attach::Built;
pub use node::attach::TreeAttach;
pub use node::attach::TreeBuild;
pub use node::clear::Clear;
pub use node::clear::ClearAgainst;
pub use node::clear::ClearOutcome;
pub use node::clear::ClearScratch;
pub use node::clear::ServedClear;
pub use node::detach::Detach;
pub use node::detach::IfUnchanged;
pub use node::discard::DiscardCounts;
pub use node::discard::WalkDiscards;
pub(crate) use node::discard::apply_pending_discards;
pub use node::discard::node_discard_free;
pub use node::discard::node_discard_fused;
pub use node::discard::node_discard_walk;
pub use node::in_place::LinkPin;
pub use node::in_place::NodeContent;
pub use node::in_place::OwnFlags;
#[cfg(feature = "test-util")]
pub use node::locks::BlockLocks;
#[cfg(feature = "test-util")]
pub use node::locks::ChainAttempts;
#[cfg(feature = "test-util")]
pub use node::locks::ChainPosition;
#[cfg(feature = "test-util")]
pub use node::locks::FoundPredecessor;
#[cfg(feature = "test-util")]
pub use node::locks::LockedBlock;
#[cfg(feature = "test-util")]
pub use node::locks::Predecessor;
#[cfg(feature = "test-util")]
pub use node::locks::chain_unchanged;
pub use node::mark::Ext;
pub use node::mark::Mark;
pub use node::relocate::MovesPermit;
pub use node::relocate::NameChange;
pub use node::relocate::ReattachCheck;
pub use node::relocate::Reattached;
pub use node::rewrite::Content;
pub use node::rewrite::LevelMark;
pub use node::rewrite::Rewrite;
use serde::Serialize;
pub use stream::ChangeSender;
pub use stream::ChangeStream;
use stream::emit;
use tokio::join;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use zerocopy::FromZeros;
use zerocopy::Immutable;

use crate::bitflagsops;
use crate::branch;
use crate::change;
use crate::change::FileAction;
use crate::change::NodeChange;
use crate::change::NodeChangeState;
use crate::errors::LinkNotFound;
use crate::errors::NodeNotFound;
use crate::errors::NotFound;
use crate::errors::Oversized;
use crate::errors::SlowDown;
use crate::errors::StateErrors;
use crate::filter::FilterMode;
use crate::filter::FilterPath;
use crate::filter::FilterStates;
use crate::filter::WalkPath;
use crate::fragment::FragmentFlags;
use crate::fs::filesystem_provider::ChainRoot;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::FilesystemDiffContext;
use crate::fs::filesystem_provider::FilesystemDiffIntent;
use crate::fs::filesystem_provider::FilesystemDiffTree;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::hash;
use crate::immutable;
use crate::immutable::ImmutableError;
use crate::immutable::ReadBoxFromImmutable;
use crate::immutable::ReadFromImmutable;
use crate::immutable::WriteToImmutable;
use crate::immutable::read_options_from_repository;
use crate::instance::InstanceId;
use crate::interface::LoreString;
use crate::link::LinkFlags;
use crate::link::LinkedState;
use crate::lore::Address;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::FragmentReference;
use crate::lore::FromBytes;
use crate::lore::Hash;
use crate::lore::IntoBytes;
use crate::lore::RepositoryId;
use crate::lore::TypedBytes;
use crate::lore::TypedBytesMut;
use crate::lore::ZeroHeapAlloc;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_info;
use crate::lore_trace;
use crate::lore_warn;
use crate::metadata;
use crate::metadata::Metadata;
use crate::metadata::MetadataType;
use crate::nametable::NameTable;
use crate::node::INVALID_BLOCK;
use crate::node::INVALID_NODE;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeBlockData;
use crate::node::NodeBlockDataV0;
use crate::node::NodeDelta;
use crate::node::NodeExtFlags;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeFileMetadataBlockData;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::NodeLink;
use crate::node::NodeNameLock;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::node::node_to_file_metadata;
use crate::records::InstanceRecords;
use crate::records::Lookup;
use crate::records::Record;
use crate::records::RecordBatch;
use crate::records::RecordKey;
use crate::repository::DOT_LORE;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::repository::is_reserved_node_name;
use crate::revision::RevisionMetadata;
use crate::stage::stage_delete;
use crate::store::KeyType;
use crate::store::StoreMatch;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;
use crate::util::request_tracker::StoreRequestTracker;

/// Data for an event summarizing a dumped repository state.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryStateDumpEventData {
    /// Sequence number of the revision.
    pub revision_number: u64,
    /// Hash of the revision.
    pub revision: Hash,
    /// Hash of the state's node tree.
    pub tree_hash: Hash,
    /// Size of the node tree in bytes.
    pub tree_size: u64,
}

/// Data for an event describing a single node in a dumped repository state.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryStateDumpNodeEventData {
    /// Name of the node.
    pub name: LoreString,
    /// Identifier of the node.
    pub id: u32,
    /// Identifier of the parent node.
    pub parent: u32,
    /// Identifier of the next sibling node.
    pub sibling: u32,
    /// File mode of the node.
    pub mode: u16,
    /// Size of the node's content in bytes.
    pub size: u64,
    /// Node flags.
    pub flags: u16,
    /// Type-specific detail for the node.
    pub type_data: LoreString,
}

pub type StateError = StateErrors;

#[lore_macro::test_pub]
#[derive(Debug, Clone, Copy)]
pub struct StateNamedNode {
    node: NodeID,
    /// Position in the walk that collected it, above the node's flags [`Self::FLAGS`] keeps: one
    /// word, so a listed child takes 16 bytes.
    walk: u32,
    name: u64,
}

impl StateNamedNode {
    /// The node's flags kept: what kind it is and what is staged or marked at or below it, all
    /// [`State::tracks`] and [`State::holds_tracked_change`] read.
    const FLAGS: u16 = NodeFlags::File.bits()
        | NodeFlags::Link.bits()
        | NodeFlags::Dirty.bits()
        | NodeFlags::Staged.bits();
    /// Where the walk index starts, past [`Self::FLAGS`].
    const SHIFT: u32 = u16::BITS - Self::FLAGS.leading_zeros();

    fn new(node: NodeID, walk_index: u32, name: u64, flags: NodeFlags) -> Self {
        #[cfg(feature = "state-hooks")]
        assert!(
            walk_index < 1 << (u32::BITS - Self::SHIFT),
            "a walk of 2^27 children"
        );
        Self {
            node,
            walk: walk_index << Self::SHIFT | u32::from(flags.bits() & Self::FLAGS),
            name,
        }
    }

    /// Position in the walk that collected it.
    fn walk_index(&self) -> u32 {
        self.walk >> Self::SHIFT
    }

    /// The node's flags [`Self::FLAGS`] keeps.
    fn flags(&self) -> NodeFlags {
        NodeFlags::from_bits_retain(self.walk as u16 & Self::FLAGS)
    }
}

pub struct StateChildrenNodes {
    pub repository: Arc<RepositoryContext>,
    pub state: Arc<State>,
    pub children: Vec<StateNamedNode>,
}

#[derive(Debug)]
pub struct StateNamedStringNode {
    pub node: NodeID,
    /// Position in the walk that collected it
    walk_index: u32,
    pub name: u64,
    pub name_string: String,
}

pub struct StateNamedChildrenNodes {
    pub repository: Arc<RepositoryContext>,
    pub state: Arc<State>,
    pub children: Vec<StateNamedStringNode>,
}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    pub struct StateFlags: u32 {
        /// No flags
        const NoFlags = 0;
        /// State is dirty
        const Dirty = 0b1;
        /// State is in conflict
        const Conflict = 0b10;
        /// State is merged (branch merge)
        const Merge = 0b100;
        /// State is cherry-picked
        const CherryPick = 0b1000;
        /// State is a revert operation
        const Revert = 0b10000;
    }
}
bitflagsops!(StateFlags, u32);

/// Most times one walk of a directory's children starts over from the directory's head,
/// after a node moved away or a revisit while the chain changes, before it reports
/// [`SlowDown`].
pub(crate) const CHAIN_WALK_RESTARTS: u32 = 64;

/// Most attempts one change to a directory's child chain makes when the chain changes
/// between loading its blocks and locking them.
#[lore_macro::test_pub]
pub(crate) const CHAIN_CHANGE_ATTEMPTS: u32 = 256;

/// What a reattach adds to the chain change count of each directory it takes a node out of, as
/// does a rename in place changing a name's hash, where an attach, a detach or a prepend adds 1.
/// A reattach puts its node at a chain's head, behind a walk that has passed the head and ahead
/// of a head a reader remembers, so a reader holding a count tells a reattach from other changes
/// by how far the count moved: a find walks again, and a listing's mark reads stale
/// ([`ChainMark::reattached`]). The count wraps at 2^32, so a reader misses a reattach only if
/// 2^16 of them pass between its two reads.
pub const REATTACH_CHANGES: u32 = 1 << 16;

/// Per-thread counts of reads of a State's runtime lock, while enabled, so a test can show a
/// path takes none. Built only for tests: a build without `test-util`, or a bench build
/// (`bench`), counts nothing and pays nothing.
#[cfg(feature = "test-util")]
pub mod lock_counting {
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    static COUNTING: AtomicBool = AtomicBool::new(false);

    thread_local! {
        static THREAD: Cell<u64> = const { Cell::new(0) };
    }

    /// Enables or disables counting process-wide. Disabled by default.
    pub fn set_runtime_read_counting(enabled: bool) {
        COUNTING.store(enabled, Ordering::Relaxed);
    }

    /// Runtime reads counted on the calling thread.
    pub fn thread_runtime_read_count() -> u64 {
        THREAD.with(Cell::get)
    }

    #[cfg(not(feature = "bench"))]
    #[inline]
    pub(crate) fn enabled() -> bool {
        COUNTING.load(Ordering::Relaxed)
    }

    #[cfg(not(feature = "bench"))]
    #[inline]
    pub(crate) fn record() {
        THREAD.with(|count| count.set(count.get() + 1));
    }
}

/// Counts of the chain nodes a find by name ([`State::find_subnode`],
/// [`State::find_subnode_resident`]) steps through, while enabled, per thread and process-wide,
/// so a test can show what a path's finds cost, and apart from them, per thread, those a children
/// walk steps through (a listing, a search since a mark). A find or a walk counts its steps
/// locally and adds them once as it ends, on the thread it ends on. Disabled, it costs one relaxed
/// load of a flag nothing writes. Built with `test-util` only.
#[cfg(feature = "test-util")]
pub mod step_counting {
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    static COUNTING: AtomicBool = AtomicBool::new(false);
    static TOTAL: AtomicU64 = AtomicU64::new(0);

    thread_local! {
        static THREAD: Cell<u64> = const { Cell::new(0) };
        static WALK_THREAD: Cell<u64> = const { Cell::new(0) };
    }

    /// Enables or disables counting process-wide. Disabled by default.
    pub fn set_chain_step_counting(enabled: bool) {
        COUNTING.store(enabled, Ordering::Relaxed);
    }

    /// Chain steps counted on the calling thread.
    pub fn thread_chain_steps() -> u64 {
        THREAD.with(Cell::get)
    }

    /// Chain steps counted on every thread.
    pub fn total_chain_steps() -> u64 {
        TOTAL.load(Ordering::Relaxed)
    }

    /// Children walk steps counted on the calling thread.
    pub fn thread_walk_steps() -> u64 {
        WALK_THREAD.with(Cell::get)
    }

    /// One find's or walk's steps, added to its counts when it drops, however it ends.
    pub(crate) struct Steps {
        count: u64,
        walk: bool,
    }

    impl Steps {
        pub(crate) const fn new() -> Self {
            Self {
                count: 0,
                walk: false,
            }
        }

        /// A children walk's steps, counted apart from the finds'.
        pub(crate) const fn walk() -> Self {
            Self {
                count: 0,
                walk: true,
            }
        }

        #[inline]
        pub(crate) fn step(&mut self) {
            self.count += 1;
        }
    }

    impl Drop for Steps {
        fn drop(&mut self) {
            if self.count == 0 || !COUNTING.load(Ordering::Relaxed) {
                return;
            }
            if self.walk {
                WALK_THREAD.with(|count| count.set(count.get() + self.count));
            } else {
                THREAD.with(|count| count.set(count.get() + self.count));
                TOTAL.fetch_add(self.count, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(not(feature = "test-util"))]
pub(crate) mod step_counting {
    /// One find's or walk's steps, which nothing counts.
    pub(crate) struct Steps;

    impl Steps {
        pub(crate) const fn new() -> Self {
            Self
        }

        pub(crate) const fn walk() -> Self {
            Self
        }

        #[inline]
        #[allow(clippy::unused_self)]
        pub(crate) fn step(&mut self) {}
    }
}

/// What [`State::walk_children`] found, whether it may have visited a child twice, and
/// whether it ran to the chain's end rather than stopping where it was told to.
#[lore_macro::test_pub]
struct Walked<T> {
    answer: Option<T>,
    may_repeat: bool,
    reached_end: bool,
}

/// What a walk of one directory's child chain does with the node it has just read.
#[lore_macro::test_pub]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChainStep {
    /// A live child of the directory: visit it, then follow its `sibling`.
    Visit,
    /// A detached child, or one passed over on the way back to where a restarted walk
    /// stood: follow its `sibling`.
    Skip,
    /// A node a reattach moved to another directory, or a cycle guard trip while the
    /// directory's change count moves: start again from the head.
    Restart,
}

/// The checks and position of one walk over a directory's child chain.
///
/// A reader copies one node at a time under its block's read lock while namespace
/// changes rewrite the chain under write locks, so a walk meets what a static chain never
/// shows. A detached node keeps its `sibling`, so the walk skips it and goes on. A node a
/// reattach moved to another directory has a `sibling` in that directory's chain, and a
/// node moved to the head of the walked directory leads the walk back over nodes it has
/// visited, which can trip the sibling cycle guard. On either the walk starts again from
/// the directory's current head, passing over the nodes up to the last one it visited, and
/// reports that it may have visited a node twice ([`Self::may_repeat`]), as it does when
/// the directory's chain change count, which prepends, removals and reattaches move, moves
/// between the walk's start and its end. A find by name ([`Self::searching`]) passes over
/// nothing on a restart, and walks again from the head when it misses its name after a
/// reattach ([`Self::missed_a_reattach`]).
///
/// Revisiting a node needs a prepend to the directory, so a cycle guard trip while the
/// count stands still since the walk's last start is a real cycle. A node that fails the
/// parent check twice running, the count standing still in between, is a broken chain: a
/// node moved out leaves the chain. Running out of restarts is churn, [`SlowDown`].
#[lore_macro::test_pub]
struct ChainWalk {
    parent: NodeID,
    cycle: SiblingCycleGuard,
    restarts: u32,
    /// The last node visited, [`INVALID_NODE`] before the first.
    last_visited: NodeID,
    /// The node a restarted walk resumes after, [`INVALID_NODE`] where it resumes at the head.
    resume_after: NodeID,
    /// The last node that failed the parent check and the count read then, [`INVALID_NODE`]
    /// before the first.
    last_misparented: (NodeID, u32),
    may_repeat: bool,
    /// A find by name, which walks the whole chain again on a restart rather than resuming
    /// after the last node it visited.
    searching: bool,
    parent_block: Option<Arc<NodeBlock>>,
    changes: u32,
    /// The most restarts before [`SlowDown`]: [`CHAIN_WALK_RESTARTS`], or, under `state-hooks`,
    /// the bound a test set on the walked State.
    #[cfg(feature = "state-hooks")]
    restart_bound: u32,
}

impl ChainWalk {
    /// A walk of `parent`'s chain, whose change count in `parent_block` read `changes` with
    /// the head.
    #[lore_macro::test_pub]
    fn new(parent: NodeID, parent_block: Arc<NodeBlock>, changes: u32) -> Self {
        let mut walk = Self::empty(parent);
        #[cfg(feature = "state-hooks")]
        if let Some(bound) = parent_block.hook_retry_bound() {
            walk.restart_bound = bound;
        }
        walk.parent_block = Some(parent_block);
        walk.changes = changes;
        walk
    }

    /// A walk of a chain with no nodes, under a parent id that names no node.
    const fn empty(parent: NodeID) -> Self {
        Self {
            parent,
            cycle: SiblingCycleGuard::new(parent),
            restarts: 0,
            last_visited: INVALID_NODE,
            resume_after: INVALID_NODE,
            last_misparented: (INVALID_NODE, 0),
            may_repeat: false,
            searching: false,
            parent_block: None,
            changes: 0,
            #[cfg(feature = "state-hooks")]
            restart_bound: CHAIN_WALK_RESTARTS,
        }
    }

    /// A find by name over `parent`'s chain, whose change count in `parent_block` read `changes`
    /// with the head: a restart walks the whole chain again, since a node a reattach put at the
    /// head lies before the last one visited.
    fn searching(parent: NodeID, parent_block: Arc<NodeBlock>, changes: u32) -> Self {
        let mut walk = Self::new(parent, parent_block, changes);
        walk.searching = true;
        walk
    }

    /// A walk of `parent`'s children in `state`, watching its chain change count, and the first
    /// child, read under one lock with the count.
    async fn watched(
        state: &State,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
    ) -> Result<(Self, Option<NodeID>), StateError> {
        let parent_block = state.block(repository, NodeBlock::index(parent)).await?;
        let (parent_node, changes) = parent_block.node_with_chain_head(parent);
        Ok((
            Self::new(parent, parent_block, changes),
            parent_node.child(),
        ))
    }

    fn chain_changed(&self) -> bool {
        self.parent_block
            .as_ref()
            .is_some_and(|block| block.chain_changes(Node::index(self.parent)) != self.changes)
    }

    /// The directory's head as it is now, read with its change count, which later checks
    /// compare against.
    #[lore_macro::test_pub]
    fn head_again(&mut self) -> Option<NodeID> {
        let block = self.parent_block.as_ref()?;
        let (parent, changes) = block.node_with_chain_changes(Node::index(self.parent));
        self.changes = changes;
        parent.child()
    }

    /// Classifies `node`, read for `node_id`.
    #[lore_macro::test_pub]
    fn step(&mut self, node_id: NodeID, node: &Node) -> Result<ChainStep, StateError> {
        if node.parent != self.parent {
            if self.last_misparented == (node_id, self.changes) && !self.chain_changed() {
                node.check_parent_link(node_id, self.parent)?;
            }
            self.last_misparented = (node_id, self.changes);
            return self.restart();
        }
        if let Err(trip) = self.cycle.observe(node_id) {
            if !self.chain_changed() {
                return Err(StateError::from(trip));
            }
            return self.restart();
        }
        if self.resume_after != INVALID_NODE {
            if self.resume_after == node_id {
                self.resume_after = INVALID_NODE;
            }
            return Ok(ChainStep::Skip);
        }
        if node.is_discarded() {
            return Ok(ChainStep::Skip);
        }
        self.last_visited = node_id;
        Ok(ChainStep::Visit)
    }

    /// Starts the walk again after the last node it visited, or for a find from the head.
    fn restart(&mut self) -> Result<ChainStep, StateError> {
        self.begin_again()?;
        if !self.searching {
            self.resume_after = self.last_visited;
        }
        Ok(ChainStep::Restart)
    }

    /// Whether a find that reached the end of the chain without its name walks again from the
    /// head: it does when a reattach moved the directory's change count since the walk read the
    /// head, since the node it put at the head lies behind the walk. A miss with only attaches
    /// and detaches since stands, as it would had the find run before them.
    fn missed_a_reattach(&mut self) -> Result<bool, StateError> {
        let Some(block) = &self.parent_block else {
            return Ok(false);
        };
        let changed = block
            .chain_changes(Node::index(self.parent))
            .wrapping_sub(self.changes);
        if changed < REATTACH_CHANGES {
            return Ok(false);
        }
        self.begin_again()?;
        self.last_visited = INVALID_NODE;
        Ok(true)
    }

    /// Whether the walk, having reached the end of the chain, starts again from the head:
    /// it does when the node it was to resume after has left the chain, which only a
    /// concurrent removal does. Otherwise the walk is over, and a chain change counted
    /// since its start marks it as possibly repeating.
    fn restart_at_end(&mut self) -> Result<bool, StateError> {
        if std::mem::replace(&mut self.resume_after, INVALID_NODE) == INVALID_NODE {
            if self.chain_changed() {
                self.may_repeat = true;
            }
            return Ok(false);
        }
        self.begin_again()?;
        self.last_visited = INVALID_NODE;
        Ok(true)
    }

    /// Whether this walk may have visited a node more than once.
    fn may_repeat(&self) -> bool {
        self.may_repeat
    }

    fn begin_again(&mut self) -> Result<(), StateError> {
        #[cfg(feature = "state-hooks")]
        let bound = self.restart_bound;
        #[cfg(not(feature = "state-hooks"))]
        let bound = CHAIN_WALK_RESTARTS;
        if self.restarts == bound {
            #[cfg(feature = "state-hooks")]
            if let Some(state) = self
                .parent_block
                .as_ref()
                .and_then(|block| block.hook_state())
            {
                crate::state::node::hooks::log::arm(
                    state,
                    crate::state::node::hooks::log::Arm::SlowDown,
                );
            }
            return Err(StateError::from(SlowDown));
        }
        self.restarts += 1;
        self.may_repeat = true;
        self.cycle = SiblingCycleGuard::new(self.parent);
        Ok(())
    }
}

/// A find by name over one directory's chain, stepped by its caller, which reads each node from
/// a block it obtains its own way.
struct NameSearch {
    walk: ChainWalk,
    /// The node to read next, `None` at the chain's end.
    next: Option<NodeID>,
    restart: bool,
    name_hash: u64,
    steps: step_counting::Steps,
}

impl NameSearch {
    /// A search of `parent_node`'s chain for `name_hash`, from `parent`, read with `changes` from
    /// `parent_block`.
    fn new(
        parent_node: NodeID,
        parent: &Node,
        parent_block: Arc<NodeBlock>,
        changes: u32,
        name_hash: u64,
    ) -> Self {
        Self {
            walk: ChainWalk::searching(parent_node, parent_block, changes),
            next: parent.child(),
            restart: false,
            name_hash,
            steps: step_counting::Steps::new(),
        }
    }

    /// The node to read next, after any restart the last step called for; `None` where the
    /// chain holds no node of the name. Inlined for the step to the next sibling, which every
    /// node of the walk takes; restarts and the chain's end take [`Self::next_after_end`].
    #[inline]
    fn next(&mut self, state: &State) -> Result<Option<NodeID>, StateError> {
        if !self.restart
            && let Some(node_id) = self.next
        {
            self.steps.step();
            return Ok(Some(node_id));
        }
        self.next_after_end(state)
    }

    /// [`Self::next`] after a restart or at the chain's end.
    #[cold]
    #[inline(never)]
    fn next_after_end(&mut self, state: &State) -> Result<Option<NodeID>, StateError> {
        loop {
            if std::mem::take(&mut self.restart) {
                state.find_restarts.fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::log::arm(
                    &state.hooks,
                    crate::state::node::hooks::log::Arm::FindRestart,
                );
                self.next = self.walk.head_again();
            }
            if let Some(node_id) = self.next {
                self.steps.step();
                return Ok(Some(node_id));
            }
            if !(self.walk.restart_at_end()? || self.walk.missed_a_reattach()?) {
                return Ok(None);
            }
            self.restart = true;
        }
    }

    /// Steps over `node`, read for the node [`Self::next`] answered: whether it holds the name.
    #[inline]
    fn found(&mut self, node_id: NodeID, node: &Node) -> Result<bool, StateError> {
        match self.walk.step(node_id, node)? {
            ChainStep::Restart => self.restart = true,
            ChainStep::Visit if node.name_hash == self.name_hash => return Ok(true),
            ChainStep::Skip | ChainStep::Visit => self.next = node.sibling(),
        }
        Ok(false)
    }
}

/// Keeps one copy of each node in `items`, the one with the highest walk index, for a
/// consumer of a walk that reported it may repeat: a node renamed between two visits is
/// current at its last. `key` gives each item's node id and walk index. Leaves `items` in
/// node id order; `keep_last_in_walk_order` restores walk order for a consumer that needs
/// it.
pub fn drop_repeated_nodes<T>(items: &mut Vec<T>, key: impl Fn(&T) -> (NodeID, u32)) {
    items.sort_unstable_by_key(|item| {
        let (id, index) = key(item);
        (id, std::cmp::Reverse(index))
    });
    items.dedup_by_key(|item| key(item).0);
}

/// Keeps, for a consumer that needs walk order, the copy of each node with the highest walk
/// index, and puts the survivors back in walk order. `key` gives each item's node id and
/// walk index. A caller filtering each visit as it comes keeps an earlier copy of a node its
/// last visit filtered out, as a listing that read the node before a change beside it would.
#[lore_macro::test_pub]
fn keep_last_in_walk_order<T>(items: &mut Vec<T>, key: impl Fn(&T) -> (NodeID, u32)) {
    drop_repeated_nodes(items, &key);
    items.sort_unstable_by_key(|item| key(item).1);
}

/// Iterator over child nodes of a directory, loading blocks with nametable.
/// Yields `(NodeID, Node, NodeNameLock)` — the child's ID, node data, and name.
///
/// The walk skips detached children and restarts past a child a reattach moved away
/// (see `ChainWalk`). It yields each live child once unless [`Self::may_repeat`], when a
/// consumer that must see a child once drops repeats with [`drop_repeated_nodes`]; a child moved
/// during the walk may be passed over.
///
/// The yielded [`NodeNameLock`] holds a read lock on the node block (zero-copy
/// name access), not an owned string. Drop it before any call that may take
/// another lock — recursing into this iterator, or a `State` method that loads a
/// block such as `block_with_nametable`, which can *write*-lock the same block to
/// deserialize its nametable. The locks are not reentrant, so holding the name
/// across such a call (especially an `.await`) risks deadlock. Copy it out with
/// [`NodeNameLock::freeze`] first if you need it past that point.
pub struct StateNodeChildrenWithNameIterator {
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    current_node_id: Option<NodeID>,
    current_block: Option<Arc<NodeBlock>>,
    current_iblock: usize,
    walk: ChainWalk,
    /// Live children the walk passed over because their block could not give back their name.
    skipped_invalid: u32,
}

impl StateNodeChildrenWithNameIterator {
    /// Live children the walk passed over because their block could not give back their name.
    #[lore_macro::test_pub]
    pub(crate) fn skipped_invalid(&self) -> u32 {
        self.skipped_invalid
    }

    /// Create a new iterator starting from the first child of the given parent node.
    /// Loads blocks with nametable for name lookup via `next()`.
    pub async fn new(
        state: Arc<State>,
        repository: Arc<RepositoryContext>,
        parent_node_id: NodeID,
    ) -> Result<Self, StateError> {
        if !parent_node_id.is_valid_or_root_node_id() {
            return Ok(Self {
                state,
                repository,
                current_node_id: None,
                current_block: None,
                current_iblock: 0,
                walk: ChainWalk::empty(parent_node_id),
                skipped_invalid: 0,
            });
        }
        let (walk, first_child) =
            ChainWalk::watched(&state, repository.clone(), parent_node_id).await?;

        let (block, iblock) = if let Some(child_id) = first_child {
            let iblock = NodeBlock::index(child_id);
            let block = state
                .block_with_nametable(repository.clone(), iblock)
                .await?;
            (Some(block), iblock)
        } else {
            (None, 0)
        };

        Ok(Self {
            state,
            repository,
            current_node_id: first_child,
            current_block: block,
            current_iblock: iblock,
            walk,
            skipped_invalid: 0,
        })
    }

    /// Get the next child node with its name.
    ///
    /// The returned [`NodeNameLock`] holds a read lock on the node block. It is
    /// `Send`, but drop it before any call that may take another lock (see the
    /// type docs) — copy it out with [`NodeNameLock::freeze`] if needed.
    pub async fn next(&mut self) -> Result<Option<(NodeID, Node, NodeNameLock)>, StateError> {
        let mut restart = false;
        loop {
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached_reader(crate::state::node::hooks::Point::IterStep)
                .await;
            if restart {
                restart = false;
                self.current_node_id = self.walk.head_again();
            }
            let Some(node_id) = self.current_node_id else {
                if self.walk.restart_at_end()? {
                    restart = true;
                    continue;
                }
                return Ok(None);
            };

            let iblock = NodeBlock::index(node_id);
            if iblock != self.current_iblock || self.current_block.is_none() {
                self.current_iblock = iblock;
                self.current_block = Some(
                    self.state
                        .block_with_nametable(self.repository.clone(), iblock)
                        .await?,
                );
            }

            let block = self.current_block.as_ref().unwrap();
            let (node, name) = block.node_and_name_ref(Node::index(node_id));
            match self.walk.step(node_id, &node)? {
                ChainStep::Restart => {
                    restart = true;
                    continue;
                }
                ChainStep::Skip => {
                    self.current_node_id = node.sibling();
                    continue;
                }
                ChainStep::Visit => self.current_node_id = node.sibling(),
            }
            match name {
                Ok(name) => return Ok(Some((node_id, node, name))),
                Err(err) => {
                    self.skipped_invalid += 1;
                    lore_warn!("Skipping node {node_id} with invalid name: {err}");
                }
            }
        }
    }

    /// Whether a restart or a change to the chain since the walk began may have made it
    /// yield a child twice.
    pub fn may_repeat(&self) -> bool {
        self.walk.may_repeat()
    }

    /// Get the next child node, with the bytes of the name it stores appended to `name`,
    /// unchecked against the name rules, so a child [`Self::next`] skips for its name is yielded
    /// too. Walks as [`Self::next`] does; a child the walk passes over leaves `name` as it was.
    pub async fn next_with_name_bytes(
        &mut self,
        name: &mut Vec<u8>,
    ) -> Result<Option<(NodeID, Node)>, StateError> {
        let mut restart = false;
        loop {
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached_reader(crate::state::node::hooks::Point::IterStep)
                .await;
            if restart {
                restart = false;
                self.current_node_id = self.walk.head_again();
            }
            let Some(node_id) = self.current_node_id else {
                if self.walk.restart_at_end()? {
                    restart = true;
                    continue;
                }
                return Ok(None);
            };

            let iblock = NodeBlock::index(node_id);
            if iblock != self.current_iblock || self.current_block.is_none() {
                self.current_iblock = iblock;
                self.current_block = Some(
                    self.state
                        .block_with_nametable(self.repository.clone(), iblock)
                        .await?,
                );
            }

            let block = self.current_block.as_ref().unwrap();
            let start = name.len();
            let node = block
                .node_and_name_bytes(Node::index(node_id), name)
                .forward::<StateError>("Node name")?;
            match self.walk.step(node_id, &node)? {
                ChainStep::Restart => {
                    name.truncate(start);
                    restart = true;
                }
                ChainStep::Skip => {
                    name.truncate(start);
                    self.current_node_id = node.sibling();
                }
                ChainStep::Visit => {
                    self.current_node_id = node.sibling();
                    return Ok(Some((node_id, node)));
                }
            }
        }
    }
}

/// Iterator over child nodes of a directory, loading blocks without nametable.
/// Yields `(NodeID, Node)` — the child's ID and node data, without the name string.
///
/// Walks as [`StateNodeChildrenWithNameIterator`] does, skipping detached children and
/// restarting past a child a reattach moved away.
pub struct StateNodeChildrenIterator {
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    current_node_id: Option<NodeID>,
    current_block: Option<Arc<NodeBlock>>,
    current_iblock: usize,
    walk: ChainWalk,
}

/// Pushes `block` onto `blocks` unless it holds it already; answers whether it pushed. A
/// sibling chain may leave a block and come back to it, and the one just seen is the likeliest
/// match.
#[lore_macro::test_pub]
fn push_distinct(blocks: &mut Vec<Arc<NodeBlock>>, block: &Arc<NodeBlock>) -> bool {
    if blocks.iter().rev().any(|seen| Arc::ptr_eq(seen, block)) {
        return false;
    }
    blocks.push(block.clone());
    true
}

impl StateNodeChildrenIterator {
    /// The node block holding the child [`Self::next`] returned last.
    #[lore_macro::test_pub]
    pub(crate) fn block(&self) -> Option<&Arc<NodeBlock>> {
        self.current_block.as_ref()
    }

    /// Create a new iterator starting from the first child of the given parent node.
    /// Loads blocks without nametable — use when only node data is needed.
    pub async fn new(
        state: Arc<State>,
        repository: Arc<RepositoryContext>,
        parent_node_id: NodeID,
    ) -> Result<Self, StateError> {
        if !parent_node_id.is_valid_or_root_node_id() {
            return Ok(Self {
                state,
                repository,
                current_node_id: None,
                current_block: None,
                current_iblock: 0,
                walk: ChainWalk::empty(parent_node_id),
            });
        }
        let (walk, first_child) =
            ChainWalk::watched(&state, repository.clone(), parent_node_id).await?;
        Self::starting_at(state, repository, first_child, walk).await
    }

    /// An iterator over `parent_node_id`'s children, with the parent's record, read with the
    /// head the walk begins at and its change count (H10), for a caller that inspects the
    /// parent too; [`Self::new`] reads them the same way, keeping only the walk. A walk begun
    /// at the head of a record read earlier, its count read fresh, would miss what was prepended
    /// between. `parent_node_id` names a directory: there is no empty walk for an invalid id, as
    /// [`Self::new`] makes.
    pub async fn with_parent(
        state: Arc<State>,
        repository: Arc<RepositoryContext>,
        parent_node_id: NodeID,
    ) -> Result<(Node, Self), StateError> {
        let parent_block = state
            .block(repository.clone(), NodeBlock::index(parent_node_id))
            .await?;
        Ok(Self::with_parent_in(
            state,
            repository,
            parent_node_id,
            parent_block,
        ))
    }

    /// [`Self::with_parent`] in `parent_block`, the block holding the parent, which the caller
    /// holds already. The first child's block loads at the first [`Self::next`].
    pub fn with_parent_in(
        state: Arc<State>,
        repository: Arc<RepositoryContext>,
        parent_node_id: NodeID,
        parent_block: Arc<NodeBlock>,
    ) -> (Node, Self) {
        let (parent, changes) = parent_block.node_with_chain_head(parent_node_id);
        let children = Self {
            state,
            repository,
            current_node_id: parent.child(),
            current_block: None,
            current_iblock: 0,
            walk: ChainWalk::new(parent_node_id, parent_block, changes),
        };
        (parent, children)
    }

    async fn starting_at(
        state: Arc<State>,
        repository: Arc<RepositoryContext>,
        first_child: Option<NodeID>,
        walk: ChainWalk,
    ) -> Result<Self, StateError> {
        let (block, iblock) = if let Some(child_id) = first_child {
            let iblock = NodeBlock::index(child_id);
            let block = state.block(repository.clone(), iblock).await?;
            (Some(block), iblock)
        } else {
            (None, 0)
        };

        Ok(Self {
            state,
            repository,
            current_node_id: first_child,
            current_block: block,
            current_iblock: iblock,
            walk,
        })
    }

    /// Get the next child node.
    pub async fn next(&mut self) -> Result<Option<(NodeID, Node)>, StateError> {
        let mut restart = false;
        loop {
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached_reader(crate::state::node::hooks::Point::IterStep)
                .await;
            if restart {
                restart = false;
                self.current_node_id = self.walk.head_again();
            }
            let Some(node_id) = self.current_node_id else {
                if self.walk.restart_at_end()? {
                    restart = true;
                    continue;
                }
                return Ok(None);
            };

            let iblock = NodeBlock::index(node_id);
            if iblock != self.current_iblock || self.current_block.is_none() {
                self.current_iblock = iblock;
                self.current_block = Some(self.state.block(self.repository.clone(), iblock).await?);
            }

            let block = self.current_block.as_ref().unwrap();
            let node = block.node(Node::index(node_id));
            match self.walk.step(node_id, &node)? {
                ChainStep::Restart => restart = true,
                ChainStep::Skip => self.current_node_id = node.sibling(),
                ChainStep::Visit => {
                    self.current_node_id = node.sibling();
                    return Ok(Some((node_id, node)));
                }
            }
        }
    }

    /// Whether a restart or a change to the chain since the walk began may have made it
    /// yield a child twice.
    pub fn may_repeat(&self) -> bool {
        self.walk.may_repeat()
    }
}

/// Number of permits gating block deserialization, taken modulo the block
/// index.
///
/// A fixed set rather than one permit per block: the permits then cost one
/// allocation per state, no lookup and no growth as a tree gets bigger,
/// against two blocks whose indices collide deserializing one after the other
/// instead of together.
///
/// The count trades that collision rate against the size of the array every
/// state that loads a block carries. A walk fans out to one task per
/// processor, so a count below that collides on machines that wide; 256 covers
/// the largest and costs ten kilobytes. A collision is never a correctness
/// matter - the permits gate duplicate work, and the publish path re-checks
/// residency whatever they do.
const BLOCK_LOADING_PERMITS: usize = 256;

/// The blocks a serialize wrote ([`State::serialize_keeping`]), held in memory while this lives.
pub struct SerializedBlocks {
    _nodes: Vec<(Arc<NodeBlock>, usize)>,
    _metadata: Vec<(Arc<NodeFileMetadataBlock>, usize)>,
}

/// Marks a State dirty when dropped: a save clears the State's flag as it writes its data, and
/// releases the blocks it wrote only after, so a save that fails or is dropped between leaves
/// them registered, which the next save writes only with the State dirty. Disarmed once the
/// blocks are released.
struct Redirty<'a>(Option<&'a State>);

impl Redirty<'_> {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for Redirty<'_> {
    fn drop(&mut self) {
        if let Some(state) = self.0 {
            state.mark_dirty();
        }
    }
}

/// Under `state-hooks`: `block` is the State's block at `index`, which a registration takes, as a
/// block's registered bit answers for the State holding it alone.
#[cfg(feature = "state-hooks")]
fn assert_resident<B>(resident: &[Weak<B>], block: &Arc<B>, index: usize) {
    assert!(
        resident
            .get(index)
            .and_then(Weak::upgrade)
            .is_some_and(|held| Arc::ptr_eq(&held, block)),
        "a registration of a block not resident at {index}"
    );
}

/// Revision state control structure, internally mutable through r/w locks
#[lore_macro::test_pub]
pub struct State {
    /// Serialized data
    data: parking_lot::RwLock<StateData>,
    /// Runtime in memory data
    runtime: parking_lot::RwLock<StateRuntime>,
    /// A VFS provider serves this State, so its chains change only through the primitives
    /// that count and log ([`Self::assert_unserved`]).
    served: AtomicBool,
    /// A C API change failed or was dropped after its climb of the directories above its node
    /// began, so a directory may be staged with no change below it
    /// ([`Self::has_unfinished_change`]).
    unfinished_change: AtomicBool,
    /// Deserializing semaphore
    deserialize: tokio::sync::Semaphore,
    /// Unused node/block semaphore
    unused: tokio::sync::Semaphore,
    /// Held by a reattach moving a directory to another parent, from its subtree check to
    /// its move; by a C API move or delete for its whole batch; and by a C API restore of a node
    /// below an added directory
    moves: tokio::sync::Semaphore,
    /// The roots of the subtrees C API deletes detached, kept until the C API commit frees
    /// them ([`Self::node_free_pending`]); made by the first, as most States are never deleted
    /// from through the C API and the queue is 256 bytes.
    pending: std::sync::OnceLock<Box<crossbeam_queue::SegQueue<NodeID>>>,
    /// How many times a find by name walked a chain again, read by tests.
    find_restarts: AtomicU64,
    /// How many times an attach looked its name up again, read by tests.
    attach_retries: AtomicU64,
    /// How many times the served chain rule refused a change under its locks, read by tests.
    served_refusals: AtomicU64,
    /// How many settles' clears a change overtook, read by tests.
    clears_overtaken: AtomicU64,
    /// How many unserved clears marked a directory again, read by tests.
    clears_rechecked: AtomicU64,
    /// How many of those found the directory detached, read by tests.
    clears_found_gone: AtomicU64,
    /// How many times a sweep's scan or a pending drain freed a slot
    /// ([`Self::node_sweep_detached`], [`Self::node_free_pending`]): a chain mark or an absence
    /// proof records it when taken, and answers stale once it moved, as a freed slot may have
    /// been taken again since (C15).
    reclaims: AtomicU32,
    /// How many children a hierarchy walk left out as moved since it collected them
    /// ([`Self::walk_moves`])
    walk_moves: AtomicU64,
    /// Block deserialization semaphore
    block_deserialize: tokio::sync::Semaphore,
    /// File metadata block deserialization semaphore
    metadata_deserialize: tokio::sync::Semaphore,
    /// Under `state-hooks`, what names this State to the scheduler, shared with the blocks it
    /// installs.
    #[cfg(feature = "state-hooks")]
    hooks: Arc<crate::state::node::hooks::StateHooks>,
    /// Permits held while a block is deserialized, shared by a node block and
    /// its file metadata block. Allocated on the first block load: most states
    /// are read for their header alone and never load a block, and held inline
    /// the array would make every `State` 11 KiB.
    block_loading: std::sync::OnceLock<Box<[tokio::sync::Semaphore]>>,
    /// The revision this tree was built on, with its signature, read apart from this tree the
    /// first time [`State::tracks`] asks about it.
    base: parking_lot::Mutex<Option<(Hash, Arc<State>)>>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "State({})", self.runtime_read().signature)
    }
}

/// Mutable store function for file timestamp
#[lore_macro::test_pub]
const FILE_MTIME: &str = "file-mtime";

/// Magic identifier
const STATE_MAGIC: u32 = 0xD37A208Eu32;

/// State format version identifiers
#[repr(u32)]
pub enum StateFormat {
    /// Initial version
    Initial = 1,
    /// Node name hash is lower case
    LowerCaseHash = 2,
}

#[repr(C)]
#[derive(Copy, Clone, Default, IntoBytes, FromBytes, Immutable)]
pub struct StateData {
    /// Magic identifier
    magic: u32,
    /// Format version
    format: u32,
    /// State flags
    flags: u32,
    /// Reserved for future extensions
    reserved_header: u32,
    /// Reserved for future extensions
    reserved_uint32: [u32; 2],
    /// Revision number
    pub revision_number: u64,
    /// Parent state signatures
    pub parent: [Hash; 2],
    /// Immutable merkle tree fragment
    hash_tree: Hash,
    /// Immutable metadata fragment
    hash_metadata: Hash,
    /// Immutable link list
    hash_link: Hash,
    /// Link merge state (transient, local only — zeroed before commit)
    hash_link_merge: Hash,
    /// Reserved for future extensions
    hash_reserved: Hash,
    /// Parent repository in case of merge/integrate from other repository
    parent_repository: RepositoryId,
    /// Unused (for future extension)
    reserved_buffer_first: [u8; 16],
    /// Unused (for future extension)
    reserved_buffer_second: [u8; 32],
}

impl StateData {
    /// The hash of the revision's metadata.
    pub fn metadata_hash(&self) -> Hash {
        self.hash_metadata
    }

    /// Whether the header carries the state magic, which every revision state's does, whatever its
    /// format version.
    pub(crate) fn has_state_magic(&self) -> bool {
        self.magic == STATE_MAGIC
    }
}

#[lore_macro::test_pub]
#[repr(C)]
#[derive(Copy, Clone, Default, IntoBytes, FromBytes, Immutable)]
pub struct LinkReference {
    /// Repository identifier
    pub(crate) repository: RepositoryId,
    /// Branch identifier
    pub(crate) branch: BranchId,
    /// Revision signature
    pub(crate) signature: Hash,
    /// Node containing the link
    pub(crate) local_node: u32,
    /// Flags
    pub(crate) flags: u32,
    /// Unused
    pub(crate) unused: u32,
}

impl LinkReference {
    pub fn resolve_branch(&self, parent_branch: BranchId) -> BranchId {
        if self.branch.is_zero() {
            parent_branch
        } else {
            self.branch
        }
    }

    /// Whether the link tracks its parent's branch (zero branch) rather than
    /// being pinned to an explicit one.
    pub fn is_tracking(&self) -> bool {
        self.branch.is_zero()
    }

    /// The branch to store for this link to resolve to `branch` under a parent on
    /// `parent_branch`.
    ///
    /// An auto-following link on its parent's branch stores zero, so it follows the parent onto
    /// every branch made from it. Any other link stores `branch`.
    pub fn branch_to_store(&self, branch: BranchId, parent_branch: BranchId) -> BranchId {
        if branch == parent_branch && self.flags & LinkFlags::DisableAutoFollow == 0 {
            BranchId::default()
        } else {
            branch
        }
    }

    pub fn repository(&self) -> RepositoryId {
        self.repository
    }

    /// Branch the link is pinned to. Zero for a tracking link; use
    /// [`LinkReference::resolve_branch`] to resolve it against the parent's
    /// branch.
    pub fn branch(&self) -> BranchId {
        self.branch
    }

    pub fn signature(&self) -> Hash {
        self.signature
    }

    pub fn local_node(&self) -> NodeID {
        self.local_node
    }

    /// See [`crate::link::LinkFlags`].
    pub fn flags(&self) -> u32 {
        self.flags
    }
}

/// Tracks a single link's merge state for rollback.
#[repr(C)]
#[derive(Copy, Clone, Default, IntoBytes, FromBytes, Immutable)]
pub struct LinkMergeEntry {
    /// Link path node ID (correlates with `LinkReference.local_node`)
    pub local_node: u32,
    /// Reserved for future use
    pub reserved: u32,
    /// Pre-merge (base) link reference snapshot for rollback
    pub base: LinkReference,
}

/// Header for the serialized link merge state blob.
#[repr(C)]
#[derive(Copy, Clone, Default, IntoBytes, FromBytes, Immutable)]
pub struct LinkMergeState {
    /// Number of `LinkMergeEntry` items following this header
    pub count: u32,
    /// Flags (reserved for future use)
    pub flags: u32,
}
const MAX_BLOCK_CACHE: usize = 5000;
/// How many file metadata blocks a state keeps from checking recycled slots, at 65,568 bytes
/// each. Adds land in the few blocks with free slots, so a handful is the usual working set.
#[lore_macro::test_pub]
const MAX_CHECKED_FILE_METADATA_BLOCKS: usize = 64;

struct StateRuntime {
    /// Signature state was deserialized from
    signature: Hash,
    /// Deserialized merkle tree data
    tree: Option<Tree>,
    /// Memory buffer holding all block addresses
    block_address: Bytes,
    /// Weak references to each block
    block: Vec<Weak<NodeBlock>>,
    /// Dirty blocks kept in memory
    block_dirty: Vec<(Arc<NodeBlock>, usize)>,
    /// Cached blocks kept in memory
    block_cache: Vec<Arc<NodeBlock>>,
    /// Cache counter
    block_cache_counter: AtomicU64,
    /// Memory buffer holding all file metadata block addresses
    block_file_metadata_address: Bytes,
    /// Weak references to each file metadata block
    block_file_metadata: Vec<Weak<NodeFileMetadataBlock>>,
    /// Dirty blocks kept in memory
    block_file_metadata_dirty: Vec<(Arc<NodeFileMetadataBlock>, usize)>,
    /// The file metadata block of the block attaches last took a slot from, held so the next
    /// attach finds it resident
    attach_file_metadata: Option<Arc<NodeFileMetadataBlock>>,
    /// The tree's block count when new nodes began to come from fresh blocks only
    /// ([`State::take_new_nodes_from_fresh_blocks`]): no block below it gives a slot, and no
    /// block at or past it has stored file metadata
    fresh_blocks_from: Option<usize>,
    /// Blocks read to check a recycled slot, kept in memory so the next check in the same block
    /// does not read it again
    block_file_metadata_checked: Vec<Arc<NodeFileMetadataBlock>>,
    /// Slot the next checked block replaces once the list is full
    block_file_metadata_checked_next: usize,
    /// Link list
    link_list: Option<Vec<LinkReference>>,
    /// Name table (read only, for old data formats)
    name_table_deprecated: Option<Arc<NameTable>>,
    /// Rehash node names
    rehash_node_names: bool,
    /// The node blocks a sweep scans, a bit per block index: every block loaded or registered
    /// since the last sweep, every block noted and not in memory then, and every block registered
    /// then, which may flag itself again before a save releases it (C15).
    unswept: Vec<u64>,
}

impl StateRuntime {
    pub fn new(signature: Hash, rehash_node_names: bool) -> Self {
        StateRuntime {
            signature,
            tree: None,
            block_address: Bytes::default(),
            block: vec![],
            block_dirty: vec![],
            block_cache: vec![],
            block_cache_counter: AtomicU64::new(0),
            block_file_metadata_address: Bytes::default(),
            block_file_metadata: vec![],
            block_file_metadata_dirty: vec![],
            attach_file_metadata: None,
            fresh_blocks_from: None,
            block_file_metadata_checked: vec![],
            block_file_metadata_checked_next: 0,
            link_list: None,
            name_table_deprecated: None,
            rehash_node_names,
            unswept: Vec::new(),
        }
    }

    /// Node blocks the tree has, whether or not any of them is in memory.
    fn block_count(&self) -> usize {
        self.tree
            .as_ref()
            .map_or(0, |tree| tree.block_count as usize)
            .max(self.block.len())
    }

    /// Notes block `index` for the next sweep to look at.
    fn note_unswept(&mut self, index: usize) {
        set_block_bit(&mut self.unswept, index);
    }

    /// Keeps `block` in memory, in place of the oldest block kept once there are
    /// [`MAX_CHECKED_FILE_METADATA_BLOCKS`].
    fn keep_checked_file_metadata(&mut self, block: &Arc<NodeFileMetadataBlock>) {
        let checked = &mut self.block_file_metadata_checked;
        if checked.iter().any(|kept| Arc::ptr_eq(kept, block)) {
            return;
        }
        if checked.len() < MAX_CHECKED_FILE_METADATA_BLOCKS {
            checked.push(block.clone());
        } else {
            let slot = self.block_file_metadata_checked_next % MAX_CHECKED_FILE_METADATA_BLOCKS;
            checked[slot] = block.clone();
            self.block_file_metadata_checked_next = slot + 1;
        }
    }
}

/// Sets block `index`'s bit in `bits`, growing it to hold the bit.
fn set_block_bit(bits: &mut Vec<u64>, index: usize) {
    let word = index / 64;
    if word >= bits.len() {
        bits.resize(word + 1, 0);
    }
    bits[word] |= 1 << (index % 64);
}

/// Whether block `index`'s bit is set in `bits`.
fn block_bit(bits: &[u64], index: usize) -> bool {
    bits.get(index / 64)
        .is_some_and(|word| word & (1 << (index % 64)) != 0)
}

/// How wide a directory the child collection sizes for once it knows there is a child.
///
/// The nodes are a child-and-sibling list with no count to read, so the width is guessed. A
/// directory holding more costs one growth from here rather than a doubling every few children.
const DIRECTORY_CHILDREN_RESERVE: usize = 16;

/// Push `child` onto `children`, sizing the vector on the first child rather than up front so
/// a directory the state holds nothing in costs no vector at all.
fn push_named_child(children: &mut Vec<StateNamedNode>, child: StateNamedNode) {
    if children.is_empty() {
        children.reserve(DIRECTORY_CHILDREN_RESERVE);
    }
    children.push(child);
}

/// The failure a read of a revision's state data reports: a missing address or payload is
/// [`NotFound`], and a slow-down and the remote's refusal of the caller keep their variants.
fn state_data_read_failure(err: ImmutableError) -> StateError {
    match err {
        ImmutableError::AddressNotFound(traced) => {
            StateError::NotFound(NotFound.chain_err(traced, "state data address not found"))
        }
        ImmutableError::PayloadNotFound(traced) => {
            StateError::NotFound(NotFound.chain_err(traced, "state data payload not found"))
        }
        ImmutableError::SlowDown(traced) => StateError::SlowDown(traced),
        ImmutableError::NotAuthenticated(traced) => StateError::NotAuthenticated(traced),
        ImmutableError::NotAuthorized(traced) => StateError::NotAuthorized(traced),
        err => StateError::internal_with_context(err, "Failed to read state data"),
    }
}

/// Outcome of [`State::find_subnode_added_since`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddedSince {
    /// A child carrying the name hash, met by the walk from the chain's head to the head the
    /// mark saw: one prepended or reattached there since, or, where that head was detached, any
    /// child of the chain.
    Found(NodeID),
    /// No child the walk met carries the name hash: none prepended or reattached ahead of the
    /// head the mark saw, or, where that head was detached, none in the chain.
    Absent,
    /// What lies ahead of the head seen cannot be told: a reattach may have put a node in front
    /// of children prepended since, a sweep freed a slot since the mark (C15), the marked node is
    /// no longer a directory, or the mark is not of this State's directory.
    Stale,
}

/// A directory's chain as a listing of it began: its head and its change count, read together
/// ([`State::node_children_with_name_hash_marked`]), for [`State::find_subnode_added_since`]; or
/// as a checked attach or reattach read it before its lookup that missed
/// (`State::find_subnode_since`). The mark holds the directory's block, which keeps it in
/// memory: a count restarts only where its block is loaded again (H9). Its head is compared by id,
/// so no published slot may be freed while a mark lives, as C3 has it for a served State and
/// under a shared claim, and as the stage holds by deferring its discards to its end; a sweep
/// that frees one moves the State's reclaim count, which makes every older mark stale (C15).
#[derive(Clone)]
pub struct ChainMark {
    parent: NodeID,
    head: Option<NodeID>,
    block: Arc<NodeBlock>,
    changes: u32,
    reclaims: u32,
}

impl ChainMark {
    /// Counts a prepend to the directory the mark's holder made itself, which the walks since
    /// the mark cover as they cover any other, so the holder's own adds never make a search
    /// stale. A holder counting a prepend that did not land would hide a reattach.
    pub fn count_own_prepend(&mut self) {
        self.changes = self.changes.wrapping_add(1);
    }

    /// Whether the directory's count moved by [`REATTACH_CHANGES`] since the mark: a reattach
    /// out of the directory or within it, or a rename in place changing a name's hash, may have
    /// moved a child the listing holds, or renamed it. An Acquire load, no lock.
    pub fn reattached(&self) -> bool {
        self.block
            .chain_changes(Node::index(self.parent))
            .wrapping_sub(self.changes)
            >= REATTACH_CHANGES
    }
}

/// What a listing keeps of the chain its walk began from: nothing (`()`), so a listing that
/// needs no mark costs nothing for one, or a [`ChainMark`].
trait ListingMark {
    /// The State's reclaim count, for a mark that records one, read before the head the mark
    /// takes, so a sweep freeing that head between the two reads leaves the mark stale.
    fn reclaims(state: &State) -> u32;
    fn of(
        reclaims: u32,
        parent: NodeID,
        head: Option<NodeID>,
        block: &Arc<NodeBlock>,
        changes: u32,
    ) -> Self;
    fn none() -> Self;
}

impl ListingMark for () {
    fn reclaims(_: &State) -> u32 {
        0
    }
    fn of(_: u32, _: NodeID, _: Option<NodeID>, _: &Arc<NodeBlock>, _: u32) -> Self {}
    fn none() -> Self {}
}

impl ListingMark for Option<ChainMark> {
    fn reclaims(state: &State) -> u32 {
        state.reclaims()
    }

    fn of(
        reclaims: u32,
        parent: NodeID,
        head: Option<NodeID>,
        block: &Arc<NodeBlock>,
        changes: u32,
    ) -> Self {
        Some(ChainMark {
            parent,
            head,
            block: block.clone(),
            changes,
            reclaims,
        })
    }

    fn none() -> Self {
        None
    }
}

/// A listed child with its walk index and name hash.
fn with_name_hash(child_id: NodeID, child: &Node, index: u32) -> (NodeID, u32, u64) {
    (child_id, index, child.name_hash)
}

/// Keeps each listed child once, at its last place in the walk.
fn keep_last_with_name_hash(children: &mut Vec<(NodeID, u32, u64)>) {
    keep_last_in_walk_order(children, |&(id, index, _)| (id, index));
}

/// A listing's children with their name hashes, the walk index each was kept at dropped.
fn without_walk_index(children: Vec<(NodeID, u32, u64)>) -> Vec<(NodeID, u64)> {
    // Collected in place: the element sizes match, so this reuses the vector.
    children
        .into_iter()
        .map(|(id, _, name_hash)| (id, name_hash))
        .collect()
}

impl State {
    /// An empty state, in the `Arc` every state is shared through.
    pub fn new() -> Arc<Self> {
        let state = Self::new_shared(&StateData::new_zeroed(), Hash::default(), false);
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::log::record(
            &state.hooks,
            crate::state::node::hooks::log::Effect::Created { from: None },
        );
        state
    }

    /// The runtime lock's write guard.
    #[inline]
    pub(in crate::state) fn runtime_write(
        &self,
    ) -> parking_lot::RwLockWriteGuard<'_, StateRuntime> {
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::touched(
            crate::state::node::hooks::Resource::Runtime(self.hooks.ordinal()),
            true,
        );
        self.runtime.write()
    }

    /// The data lock's write guard; under `state-hooks`, touched once taken.
    #[inline]
    pub(in crate::state) fn data_write(&self) -> parking_lot::RwLockWriteGuard<'_, StateData> {
        let guard = self.data.write();
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::touched(
            crate::state::node::hooks::Resource::Data(self.hooks.ordinal()),
            true,
        );
        guard
    }

    /// The data lock's read guard; under `state-hooks`, touched once taken.
    #[inline]
    pub(in crate::state) fn data_read(&self) -> parking_lot::RwLockReadGuard<'_, StateData> {
        let guard = self.data.read();
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::touched(
            crate::state::node::hooks::Resource::Data(self.hooks.ordinal()),
            false,
        );
        guard
    }

    /// The runtime lock's read guard, counted under `test-util` while `lock_counting` is
    /// enabled.
    #[inline]
    fn runtime_read(&self) -> parking_lot::RwLockReadGuard<'_, StateRuntime> {
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::touched(
            crate::state::node::hooks::Resource::Runtime(self.hooks.ordinal()),
            false,
        );
        #[cfg(all(feature = "test-util", not(feature = "bench")))]
        if lock_counting::enabled() {
            lock_counting::record();
        }
        self.runtime.read()
    }

    /// Marks this State as served by a VFS provider, for good.
    pub fn mark_served(&self) {
        self.served.store(true, Ordering::Relaxed);
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::log::record(
            &self.hooks,
            crate::state::node::hooks::log::Effect::Served,
        );
    }

    /// Whether a VFS provider serves this State ([`Self::mark_served`]).
    pub fn is_served(&self) -> bool {
        self.served.load(Ordering::Relaxed)
    }

    /// Notes that a C API change failed or was dropped after its climb of the directories above
    /// its node began and before the node changed ([`Self::node_move_staged`],
    /// [`Self::node_add_staged`], [`Self::node_modify_staged`], [`Self::node_delete_level`]),
    /// which may leave the directories it marked staged with no change of its own below them.
    ///
    /// Relaxed, as the flag's readers are ordered after its writers by the handle's claims: a C
    /// API change notes it under its shared claim, which the tasks a call spawns hold shares of
    /// until each is dropped, and the commit reads it under its exclusive one.
    pub fn note_unfinished_change(&self) {
        self.unfinished_change.store(true, Ordering::Relaxed);
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::log::record(
            &self.hooks,
            crate::state::node::hooks::log::Effect::Unfinished { set: true },
        );
    }

    /// Whether a C API change failed or was dropped after its climb began since this State was
    /// built or last committed through the C API, so a staged directory may hold no change: the
    /// in-memory commit then counts only nodes carrying an action as staged.
    pub fn has_unfinished_change(&self) -> bool {
        self.unfinished_change.load(Ordering::Relaxed)
    }

    /// Forgets the unfinished changes noted, for the C API commit once its in-memory commit
    /// published the tree it froze, clearing the staged marks it reached.
    pub fn clear_unfinished_change(&self) {
        self.unfinished_change.store(false, Ordering::Relaxed);
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::log::record(
            &self.hooks,
            crate::state::node::hooks::log::Effect::Unfinished { set: false },
        );
    }

    /// For a `writer` changing chains or names past the primitives, which neither count nor log,
    /// so a served State's listings would never see the change.
    #[track_caller]
    pub(in crate::state) fn assert_unserved(&self, writer: &str) {
        debug_assert!(
            !self.is_served(),
            "{writer} changed a chain past the primitives of a served State, whose listings \
             never see it"
        );
    }

    /// The most attempts one change to a directory's chain makes before [`SlowDown`]:
    /// [`CHAIN_CHANGE_ATTEMPTS`], or, under `state-hooks`, the bound a test set
    /// ([`Self::set_retry_bound_for_tests`]).
    #[inline]
    #[cfg_attr(not(feature = "state-hooks"), allow(clippy::unused_self))]
    pub(crate) fn chain_change_attempts(&self) -> u32 {
        #[cfg(feature = "state-hooks")]
        if let Some(bound) = self.hooks.retry_bound() {
            return bound;
        }
        CHAIN_CHANGE_ATTEMPTS
    }

    /// Under `state-hooks`, what names this State to the scheduler, and what a test reads of it;
    /// shared with the State's blocks, so a count read after the State drops shows the blocks that
    /// outlived it.
    #[cfg(feature = "state-hooks")]
    pub fn hooks(&self) -> &Arc<crate::state::node::hooks::StateHooks> {
        &self.hooks
    }

    /// Under `state-hooks`, lowers the most attempts a change to a chain makes, and the most
    /// restarts a walk of one makes, before [`SlowDown`], to `bound`; `None` restores the
    /// product's. For tests, before any operation runs on the State: the attach's loops compare
    /// their counts with the bound at each check, so one lowered below a count already reached
    /// would never stop them, and `ChainAttempts::exhausted` would answer `SlowDown` where its
    /// misses show a broken hierarchy.
    #[cfg(feature = "state-hooks")]
    pub fn set_retry_bound_for_tests(&self, bound: Option<u32>) {
        self.hooks.set_retry_bound(bound);
    }

    /// Builds the state in its `Arc` allocation, one field at a time. Built as a
    /// whole, even through `Arc::new_cyclic`, it is assembled on the stack and
    /// copied into the allocation. The exhaustive pattern stops this compiling
    /// when `State` gains a field it does not write.
    fn new_shared(data: &StateData, signature: Hash, rehash_node_names: bool) -> Arc<Self> {
        let _ = |state: &Self| {
            let Self {
                data: _,
                runtime: _,
                served: _,
                unfinished_change: _,
                unused: _,
                moves: _,
                pending: _,
                find_restarts: _,
                attach_retries: _,
                served_refusals: _,
                clears_overtaken: _,
                clears_rechecked: _,
                clears_found_gone: _,
                reclaims: _,
                walk_moves: _,
                deserialize: _,
                block_deserialize: _,
                metadata_deserialize: _,
                block_loading: _,
                #[cfg(feature = "state-hooks")]
                    hooks: _,
                base: _,
            } = state;
        };
        let state = Arc::<Self>::new_uninit();
        let fields = Arc::as_ptr(&state).cast::<Self>().cast_mut();
        // SAFETY: `state` has no other owner yet, so nothing can observe the
        // fields before they are written, and every field is written before
        // `assume_init`.
        unsafe {
            (&raw mut (*fields).data).write(parking_lot::RwLock::new(*data));
            (&raw mut (*fields).runtime).write(parking_lot::RwLock::new(StateRuntime::new(
                signature,
                rehash_node_names,
            )));
            (&raw mut (*fields).served).write(AtomicBool::new(false));
            (&raw mut (*fields).unfinished_change).write(AtomicBool::new(false));
            (&raw mut (*fields).unused).write(tokio::sync::Semaphore::new(1));
            (&raw mut (*fields).moves).write(tokio::sync::Semaphore::new(1));
            (&raw mut (*fields).pending).write(std::sync::OnceLock::new());
            (&raw mut (*fields).find_restarts).write(AtomicU64::new(0));
            (&raw mut (*fields).attach_retries).write(AtomicU64::new(0));
            (&raw mut (*fields).served_refusals).write(AtomicU64::new(0));
            (&raw mut (*fields).clears_overtaken).write(AtomicU64::new(0));
            (&raw mut (*fields).clears_rechecked).write(AtomicU64::new(0));
            (&raw mut (*fields).clears_found_gone).write(AtomicU64::new(0));
            (&raw mut (*fields).reclaims).write(AtomicU32::new(0));
            (&raw mut (*fields).walk_moves).write(AtomicU64::new(0));
            (&raw mut (*fields).deserialize).write(tokio::sync::Semaphore::new(1));
            (&raw mut (*fields).block_deserialize).write(tokio::sync::Semaphore::new(1));
            (&raw mut (*fields).metadata_deserialize).write(tokio::sync::Semaphore::new(1));
            (&raw mut (*fields).block_loading).write(std::sync::OnceLock::new());
            #[cfg(feature = "state-hooks")]
            (&raw mut (*fields).hooks).write(crate::state::node::hooks::StateHooks::new());
            (&raw mut (*fields).base).write(parking_lot::Mutex::new(None));
            state.assume_init()
        }
    }

    /// Load the current state and branch.
    pub async fn deserialize_current(
        repository: Arc<RepositoryContext>,
    ) -> Result<(Arc<Self>, BranchId), StateError> {
        let (current_revision, branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<StateError>("Failed to deserialize anchor")?;
        Ok((
            State::deserialize(repository.clone(), current_revision).await?,
            branch,
        ))
    }

    /// Load current and optionally staged states, plus the current branch.
    ///
    /// Returns `(current_state, staged_state, branch)` where `staged_state`
    /// is `None` when nothing is staged.
    pub async fn deserialize_current_and_staged(
        repository: Arc<RepositoryContext>,
    ) -> Result<(Arc<Self>, Option<Arc<Self>>, BranchId), StateError> {
        let (current_revision, branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<StateError>("Failed to deserialize anchor")?;
        let state_current = State::deserialize(repository.clone(), current_revision).await?;

        let state_staged = match crate::instance::load_staged_revision(&repository)
            .await
            .ok()
            .flatten()
        {
            Some(staged_revision) if staged_revision != current_revision => {
                Some(State::deserialize(repository.clone(), staged_revision).await?)
            }
            _ => None,
        };

        Ok((state_current, state_staged, branch))
    }

    /// The state of revision `signature`, empty for a zero signature.
    ///
    /// The read is boxed. Inline, it would make every future awaiting a state, several at once
    /// in a three-way diff, as large as the read.
    pub async fn deserialize(
        repository: Arc<RepositoryContext>,
        signature: Hash,
    ) -> Result<Arc<Self>, StateError> {
        if signature.is_zero() {
            return Ok(State::new());
        }
        let address = Address::zero_context_hash(signature);
        let options = read_options_from_repository(&repository);
        // Under `state-hooks`, the repository the State is read from, as the log names it.
        #[cfg(feature = "state-hooks")]
        let from = Arc::as_ptr(&repository) as usize;
        let mut data =
            match Box::pin(StateData::read_from_immutable(repository, address, options)).await {
                Ok(data) => data,
                Err(err) => return Err(state_data_read_failure(err)),
            };

        if !data.has_state_magic() {
            Err(StateError::internal("Corrupt header"))
        } else if data.format == 0 || data.format > StateFormat::LowerCaseHash as u32 {
            if data.format > StateFormat::LowerCaseHash as u32 && data.format < 0xFFF {
                Err(StateError::internal(format!(
                    "Upgrade format: {}",
                    data.format
                )))
            } else {
                Err(StateError::internal(format!(
                    "Invalid format: {}",
                    data.format
                )))
            }
        } else {
            // If old version, set rehash flag
            let rehash_node_names = data.format < StateFormat::LowerCaseHash as u32;
            // Clean flags
            data.flags &= !StateFlags::Dirty;
            let state = State::new_shared(&data, signature, rehash_node_names);
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::log::record(
                &state.hooks,
                crate::state::node::hooks::log::Effect::Created {
                    from: Some((from, signature)),
                },
            );
            Ok(state)
        }
    }

    /// Writes the state's dirty blocks, link list, tree, and data to the immutable store, and
    /// returns its signature. A state that is not dirty returns its signature unwritten.
    ///
    /// Takes the state's `Arc`: a state that rehashes its node names, as one read in a format
    /// before [`StateFormat::LowerCaseHash`] does, loads every block in a task of its own, and
    /// each task holds the state.
    ///
    /// Not an `async fn`: it answers [`Self::serialize_keeping`]'s future itself, which keeps
    /// nothing, so no second copy of the arguments sits beside it.
    pub fn serialize<'a>(
        self: &'a Arc<Self>,
        repository: Arc<RepositoryContext>,
        token: &'a RepositoryWriteToken,
    ) -> impl Future<Output = Result<Hash, StateError>> + Send + 'a {
        self.serialize_keeping(repository, token, drop::<SerializedBlocks>)
    }

    /// [`Self::serialize`], handing `keep` the blocks it wrote once written: a block the State
    /// made, which only its registration held until the write released it, stays in memory while
    /// `keep` holds it, so a reader of the revision just written finds it there rather than
    /// loading it back from the store. A State that is not dirty writes and hands nothing.
    ///
    /// No writer runs beside a save, nor another save of this State (the Serialize tier is
    /// exclusive): each block records one save's edit sequence. A writer that does run beside one
    /// is not lost: what it changed goes into the next save, which then holds everything the
    /// State does. The signature this save answers is not a snapshot of any one moment then: a
    /// parent block imaged after a publish can name a child whose block was imaged before it, and
    /// a block threaded onto the unused chain behind a fresh head this save does not write is
    /// reached in the next save alone.
    pub async fn serialize_keeping(
        self: &Arc<Self>,
        repository: Arc<RepositoryContext>,
        _token: &RepositoryWriteToken,
        keep: impl FnOnce(SerializedBlocks) + Send,
    ) -> Result<Hash, StateError> {
        debug_assert!(
            self.runtime_read().fresh_blocks_from.is_none(),
            "a State taking new nodes from fresh blocks is never serialized: its stored unused \
             chain would leave out the tree's free slots"
        );
        let is_dirty = self.is_dirty();
        if !is_dirty {
            lore_trace!(
                "State not dirtied, return previously serialized signature {}",
                self.revision()
            );
            return Ok(self.revision());
        }

        if self.runtime_read().rehash_node_names {
            // Deserialize all blocks to force update the node name hashes, as state format
            // requires all blocks to have same format
            lore_info!("Updating all state block name hashes");
            let block_count = self.tree(repository.clone()).await?.block_count as usize;
            let mut tasks = JoinSet::new();
            let mut result = Ok(());
            for block_index in 0..block_count {
                let state = self.clone();
                let repository = repository.clone();
                lore_spawn!(
                    tasks,
                    crate::subtask!(async move {
                        lore_trace!("  block {}/{}", block_index + 1, block_count);
                        let block = state.block(repository, block_index).await?;
                        {
                            block.write().mark_dirty();
                        }
                        state.block_modified(block, block_index);
                        Ok(())
                    })
                );
                if let Some(task_result) = tasks.try_join_next() {
                    match task_result {
                        Ok(inner_result) => {
                            if result.is_ok() {
                                result = inner_result;
                            }
                        }
                        Err(err) => {
                            result = Err(StateError::internal_with_context(err, "Task failure"));
                        }
                    }
                }
            }
            while let Some(task_result) = tasks.join_next().await {
                match task_result {
                    Ok(inner_result) => {
                        if result.is_ok() {
                            result = inner_result;
                        }
                    }
                    Err(err) => {
                        result = Err(StateError::internal_with_context(err, "Task failure"));
                    }
                }
            }
            result?;
        }

        let (mut block_dirty, block_file_metadata_dirty) = {
            let lock = self.runtime_read();
            // Room for the unused chain's head, which a threading may add.
            let block_dirty = if lock.block_dirty.is_empty() {
                Vec::new()
            } else {
                let mut block_dirty = Vec::with_capacity(lock.block_dirty.len() + 1);
                block_dirty.extend(lock.block_dirty.iter().cloned());
                block_dirty
            };
            (block_dirty, lock.block_file_metadata_dirty.clone())
        };

        // Each phase reads the tree again where it updates it, rather than sharing a copy: a copy
        // would be held across every write.
        let block_count = self.tree(repository.clone()).await?.block_count as usize;

        if !block_dirty.is_empty() {
            self.serialize_node_blocks(&repository, &mut block_dirty, block_count)
                .await?;
        }
        if !block_file_metadata_dirty.is_empty() {
            self.serialize_file_metadata_blocks(
                &repository,
                &block_file_metadata_dirty,
                block_count,
            )
            .await?;
        }

        let link_list = { self.runtime_read().link_list.clone() };
        if let Some(link_list) = link_list {
            let list_hash = hash::hash_slice(link_list.as_bytes());
            if list_hash != self.data_read().hash_link {
                let rehashed_list = if link_list.is_empty() {
                    lore_debug!("Link list empty, write default hash");
                    Hash::default()
                } else {
                    let bytes = Bytes::copy_from_slice(link_list.as_bytes());
                    let address = immutable::write(
                        repository.clone(),
                        Context::default(),
                        bytes,
                        immutable::write_options_from_repository(repository.clone())
                            .with_local_cache_priority()
                            .with_max_size_chunk(),
                    )
                    .await
                    .forward::<StateError>("Failed to serialize link list")?;

                    address.hash
                };

                lore_debug!("Serialized link list to {rehashed_list}");
                let mut data = self.data_write();
                data.hash_link = rehashed_list;
                data.flags |= StateFlags::Dirty;
            }
        }

        self.serialize_tree(&repository).await?;

        // Serialize the state
        let redirty = Redirty(Some(self));
        let address = {
            let buffer = {
                let mut data = self.data_write();
                data.flags &= !StateFlags::Dirty;
                data.format = StateFormat::LowerCaseHash as u32;
                data.magic = STATE_MAGIC;

                Bytes::copy_from_slice(data.as_bytes())
            };
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached(crate::state::node::hooks::Point::Serialize).await;

            immutable::write(
                repository.clone(),
                Context::default(),
                buffer,
                immutable::write_options_from_repository(repository.clone())
                    .with_revision_state()
                    .with_local_cache_priority()
                    .with_max_size_chunk(),
            )
            .await
            .forward::<StateError>("Failed to serialize state")?
        };

        {
            let mut runtime = self.runtime_write();
            runtime.signature = address.hash;
        }

        self.release_serialized_blocks(&block_dirty, &block_file_metadata_dirty);
        redirty.disarm();

        lore_trace!(
            "Serialized state to {} in repository {}",
            address.hash,
            repository.id
        );

        keep(SerializedBlocks {
            _nodes: block_dirty,
            _metadata: block_file_metadata_dirty,
        });
        Ok(address.hash)
    }

    /// Writes the dirty node blocks and the list of their addresses, and points the tree at the
    /// list.
    ///
    /// Its own future, as are the other phases of [`Self::serialize`]: inline, what each phase
    /// holds across its awaits would be reserved in every state of it.
    async fn serialize_node_blocks(
        &self,
        repository: &Arc<RepositoryContext>,
        block_dirty: &mut Vec<(Arc<NodeBlock>, usize)>,
        block_count: usize,
    ) -> Result<(), StateError> {
        lore_debug!("Serializing {} dirty blocks", block_dirty.len());
        self.thread_unused_blocks(repository, block_dirty, block_count)
            .await?;
        let mut tasks: JoinSet<Result<(Address, usize), StateError>> = JoinSet::new();
        for (block, block_index) in block_dirty.iter() {
            let block = block.clone();
            let block_index = *block_index;
            lore_trace!("Queue serialization of dirty node block {}", block_index);
            let repository = repository.clone();
            lore_spawn!(
                tasks,
                crate::subtask!(async move {
                    lore_trace!("Serializing dirty node block {}", block_index);
                    #[cfg(feature = "state-hooks")]
                    crate::state::node::hooks::reached(crate::state::node::hooks::Point::BlockSave)
                        .await;
                    let node_block = {
                        block.deserialize_nametable(repository.clone()).await?;
                        lore_trace!("Serializing dirty node block {} name table", block_index);
                        // The table and the image under one hold: an edit between them would store
                        // a name the table written lacks.
                        let saving = block.saving();
                        let name_table = if saving.name_table().is_empty() {
                            Address::default()
                        } else {
                            immutable::write_borrowed(
                                repository.clone(),
                                Context::default(),
                                saving.name_table(),
                                immutable::write_options_from_repository(repository.clone())
                                    .with_local_cache_priority()
                                    .with_max_size_chunk(),
                            )
                            .await
                            .forward::<StateError>("Failed to serialize node block")?
                        };
                        saving.image(name_table.hash)
                    };
                    let address = node_block
                        .node_block()
                        .write_to_immutable(
                            repository.clone(),
                            Context::default(),
                            immutable::write_options_from_repository(repository.clone())
                                .with_local_cache_priority()
                                .with_max_size_chunk(),
                        )
                        .await
                        .forward::<StateError>("Failed to serialize node block")?;
                    Ok((address, block_index))
                })
            );
        }

        let hash_node = self.tree_readonly()?.hash_node;
        let mut block_hash_bytes = self
            .node_block_addresses(repository, hash_node, block_count)
            .await?
            .clone_and_resize_zeroed::<Hash>(block_count);
        {
            let block_hash = block_hash_bytes.as_type_slice_mut();

            let mut final_error = Ok(());
            let mut task_error = Ok(());
            while let Some(task) = tasks.join_next().await {
                if let Ok(result) = task {
                    if let Ok((address, block_index)) = result {
                        block_hash[block_index] = address.hash;
                    } else {
                        final_error = Err(result.unwrap_err());
                    }
                } else {
                    task_error = Err(StateError::internal_with_context(
                        task.unwrap_err(),
                        "Failed to serialize node block task",
                    ));
                }
            }
            final_error?;
            task_error?;
        }

        // Write out the block address list
        let block_hash_bytes = block_hash_bytes.freeze();
        let list_address = immutable::write(
            repository.clone(),
            Context::default(),
            block_hash_bytes.clone(),
            immutable::write_options_from_repository(repository.clone())
                .with_local_cache_priority()
                .with_max_size_chunk(),
        )
        .await
        .forward::<StateError>("Failed to serialize node block list")?;

        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::reached(crate::state::node::hooks::Point::Serialize).await;
        // Update the tree node block list address, in place: a copy written back would undo a
        // grab's change to the tree since it was read.
        {
            let mut lock = self.runtime_write();
            let tree = lock
                .tree
                .as_mut()
                .ok_or_else(|| StateError::internal("Tree is None"))?;
            lore_trace!(
                "Update tree node block list from {} to {}",
                tree.hash_node,
                list_address.hash
            );
            tree.hash_node = list_address.hash;
            tree.flags |= TreeFlags::Dirty;
            lock.block_address = block_hash_bytes;
        }
        Ok(())
    }

    /// Threads each block of `block_dirty` that freed a slot while full, which no grab reaches
    /// until then, onto the tree's unused chain right behind its head, under the unused permit,
    /// so no retire or push moves the head meanwhile, and none is written over (H4).
    ///
    /// The head never moves for it: grabs take only the head's slots, so a block behind the head
    /// never fills, and a full head never ends up behind another block, where no grab retires it
    /// and the slot it later frees would thread it a second time, a cycle. A grab filling the
    /// head meanwhile is the usual case: the next grab retires it onto the blocks threaded
    /// behind it. Where the chain is empty the blocks threaded make it, its head set under the
    /// permit, which a grab finding no head waits for.
    ///
    /// Not threaded: the head, full when it freed its slot (it would follow itself), and a marked
    /// block full again, its slot taken back while it was the head before a grab retired it. A
    /// block threaded is marked dirty before its image, and so is the head linked to them, which
    /// this save writes though it was clean: left to the next save, its link would be in memory
    /// alone and keep the State dirty. A fresh head past the save's address list, dirty from its
    /// allocation, goes into the next save.
    ///
    /// A grab that read the head before a retire moved it can still take the last free slot of
    /// that block, once a discard or a slot's release freed one and a save threaded it behind the
    /// new head: the block, full on the chain, frees a slot later and is threaded again. The
    /// grab's head read has to span a retire, a free and a save; no save runs beside writers
    /// (the Serialize tier is exclusive), so it is latent, as H4 is. Closing it would take every
    /// grab re-checking, under its block's lock, that the block is still the head, a cost on the
    /// grab's fast path a latent race does not justify.
    ///
    /// A block marked after the scan, or after its pass, stays registered for the next save
    /// ([`NodeBlock::release_saved`]).
    async fn thread_unused_blocks(
        &self,
        repository: &Arc<RepositoryContext>,
        block_dirty: &mut Vec<(Arc<NodeBlock>, usize)>,
        block_count: usize,
    ) -> Result<(), StateError> {
        // Few saves have a block to thread: the permit is for those.
        let Some(from) = block_dirty
            .iter()
            .position(|(block, _)| block.read().has_first_unused_node())
        else {
            return Ok(());
        };
        // The head loaded before the permit, as a grab loads it: only a head a retire moved to
        // meanwhile loads under it.
        let mut held = self.head_block(repository, None).await?;
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::reached(crate::state::node::hooks::Point::Serialize).await;
        let _permit = self.acquire_unused().await?;
        let first = self
            .runtime_read()
            .tree
            .as_ref()
            .ok_or_else(|| StateError::internal("Tree is None"))?
            .block_unused_first;
        if held.as_ref().is_none_or(|(at, _)| *at != first) {
            held = self.head_block(repository, Some(first)).await?;
        }
        let behind = held.as_ref().map_or(INVALID_BLOCK, |(_, block)| {
            block.read().node_block().block_unused_next
        });
        let mut next = behind;
        for (block, block_index) in &block_dirty[from..] {
            // Read first: few blocks carry the mark, and the write lock is for those.
            if !block.read().has_first_unused_node() {
                continue;
            }
            #[cfg(feature = "state-hooks")]
            let _label = crate::state::node::hooks::log::label(
                crate::state::node::hooks::log::Section::Save,
            );
            let mut writer = block.write();
            if writer.take_first_unused_node() && !writer.is_full() && *block_index as u32 != first
            {
                writer.node_block().block_unused_next = next;
                writer.mark_dirty();
                next = *block_index as u32;
            }
        }
        if next == behind {
            return Ok(());
        }
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::reached(crate::state::node::hooks::Point::Threading).await;
        if let Some((_, block)) = held {
            let dirtied = {
                #[cfg(feature = "state-hooks")]
                let _label = crate::state::node::hooks::log::label(
                    crate::state::node::hooks::log::Section::Save,
                );
                let mut writer = block.write();
                writer.node_block().block_unused_next = next;
                writer.mark_dirty()
            };
            if dirtied {
                self.block_modified(block.clone(), first as usize);
            }
            // Written by this save wherever its list lacks it, turned dirty by the link or by a
            // writer after the list was taken, and once: but for a fresh block past the save's
            // address list, dirty from its allocation, which the next save writes.
            if (first as usize) < block_count
                && !block_dirty.iter().any(|(_, at)| *at == first as usize)
            {
                block_dirty.push((block, first as usize));
            }
        } else {
            let mut runtime = self.runtime_write();
            let tree = runtime
                .tree
                .as_mut()
                .ok_or_else(|| StateError::internal("Tree is None"))?;
            tree.block_unused_first = next;
            tree.flags |= TreeFlags::Dirty;
        }
        Ok(())
    }

    /// The unused chain's head, `at` or else the one the tree names now, with its block; `None`
    /// where the chain is empty.
    async fn head_block(
        &self,
        repository: &Arc<RepositoryContext>,
        at: Option<u32>,
    ) -> Result<Option<(u32, Arc<NodeBlock>)>, StateError> {
        let head = at.unwrap_or_else(|| self.unused_head());
        if head as usize >= self.block_count() {
            return Ok(None);
        }
        Ok(Some((
            head,
            self.block(repository.clone(), head as usize).await?,
        )))
    }

    /// Writes the dirty file metadata blocks and the list of their addresses, and points the tree
    /// at the list.
    async fn serialize_file_metadata_blocks(
        &self,
        repository: &Arc<RepositoryContext>,
        block_file_metadata_dirty: &[(Arc<NodeFileMetadataBlock>, usize)],
        block_count: usize,
    ) -> Result<(), StateError> {
        lore_trace!(
            "Serializing {} dirty file metadata blocks",
            block_file_metadata_dirty.len()
        );
        let mut tasks: JoinSet<Result<(Address, usize), StateError>> = JoinSet::new();
        for (block, block_index) in block_file_metadata_dirty.iter() {
            let block = block.clone();
            let block_index = *block_index;
            let repository = repository.clone();
            lore_trace!(
                "Queue serialization of dirty file metadata node block {}",
                block_index
            );

            lore_spawn!(
                tasks,
                crate::subtask!(async move {
                    lore_trace!("Serializing dirty file metadata node block {}", block_index);
                    #[cfg(feature = "state-hooks")]
                    crate::state::node::hooks::reached(crate::state::node::hooks::Point::BlockSave)
                        .await;
                    let node_block = block.read_saving();
                    let address = node_block
                        .node_block()
                        .write_to_immutable(
                            repository.clone(),
                            Context::default(),
                            immutable::write_options_from_repository(repository.clone())
                                .with_local_cache_priority()
                                .with_max_size_chunk(),
                        )
                        .await
                        .forward::<StateError>("Failed to serialize file metadata block")?;
                    Ok((address, block_index))
                })
            );
        }

        let mut block_hash_bytes = {
            let lock = self.runtime_read();
            // Resize buffer with empty hashes if needed
            lock.block_file_metadata_address
                .clone_and_resize_zeroed::<Hash>(block_count)
        };
        {
            let block_hash = block_hash_bytes.as_type_slice_mut();

            let mut final_error = Ok(());
            let mut task_error = Ok(());
            while let Some(task) = tasks.join_next().await {
                if let Ok(result) = task {
                    if let Ok((address, block_index)) = result {
                        block_hash[block_index] = address.hash;
                    } else {
                        final_error = Err(result.unwrap_err());
                    }
                } else {
                    task_error = Err(StateError::internal_with_context(
                        task.unwrap_err(),
                        "Failed to serialize file metadata block task",
                    ));
                }
            }
            final_error?;
            task_error?;
        }

        // Write out the block address list
        let block_hash_bytes = block_hash_bytes.freeze();
        let list_address = immutable::write(
            repository.clone(),
            Context::default(),
            block_hash_bytes.clone(),
            immutable::write_options_from_repository(repository.clone())
                .with_local_cache_priority()
                .with_max_size_chunk(),
        )
        .await
        .forward::<StateError>("Failed to serialize file metadata block list")?;

        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::reached(crate::state::node::hooks::Point::Serialize).await;
        // Update the tree file metadata node block list address, in place, as the node blocks'.
        {
            let mut lock = self.runtime_write();
            let tree = lock
                .tree
                .as_mut()
                .ok_or_else(|| StateError::internal("Tree is None"))?;
            lore_trace!(
                "Update tree file metadata node block list from {} to {}",
                tree.hash_file_metadata,
                list_address.hash
            );
            tree.hash_file_metadata = list_address.hash;
            tree.flags |= TreeFlags::Dirty;
            lock.block_file_metadata_address = block_hash_bytes;
        }
        Ok(())
    }

    /// Writes the tree to the immutable store if it is dirty, and points the state at it.
    ///
    /// Its own future, so that the copy of the tree it writes from is not held in every state
    /// of [`Self::serialize`].
    async fn serialize_tree(&self, repository: &Arc<RepositoryContext>) -> Result<(), StateError> {
        let tree = { self.runtime_read().tree.unwrap_or_default() };
        if tree.flags & TreeFlags::Dirty == 0 {
            return Ok(());
        }
        lore_trace!("Serializing dirty tree");
        let address = tree
            .write_to_immutable(
                repository.clone(),
                Context::default(),
                immutable::write_options_from_repository(repository.clone())
                    .with_local_cache_priority()
                    .with_max_size_chunk(),
            )
            .await
            .forward::<StateError>("Failed to serialize tree")?;
        lore_trace!("Serialized tree to {}", address.hash);
        lore_trace!("  node block {}", tree.hash_node);
        lore_trace!("  file metadata block {}", tree.hash_file_metadata);
        let mut data = self.data_write();
        data.hash_tree = address.hash;
        data.flags |= StateFlags::Dirty;
        Ok(())
    }

    /// Drop the written blocks' dirty flags and registrations, so the state matches the
    /// store it was just written to.
    ///
    /// Without this a serialized state cannot be serialized again: the blocks stay
    /// flagged dirty, so the next edit's `mark_dirty` reports "already dirty", its
    /// caller registers nothing and does not mark the state dirty, and the next
    /// `serialize` returns the previous signature having written none of the edits.
    ///
    /// A block an edit marked since the save took its image stays dirty: that edit found it
    /// dirty already and registered nothing, so only this keeps it in a save (H4). The dirty
    /// flag is then the membership test, so the registrations are filtered on it: a block
    /// still dirty was either registered while this call ran or edited since its image, and
    /// either way it still needs writing. The State, its flag cleared when its data was
    /// written, is marked dirty again while any block stays registered.
    fn release_serialized_blocks(
        &self,
        block_dirty: &[(Arc<NodeBlock>, usize)],
        block_file_metadata_dirty: &[(Arc<NodeFileMetadataBlock>, usize)],
    ) {
        for (block, _) in block_dirty.iter() {
            block.release_saved();
        }
        for (block, _) in block_file_metadata_dirty.iter() {
            block.release_saved();
        }

        let registered = {
            let mut runtime = self.runtime_write();
            // A block read clean drops its registration under the same read lock, so a write
            // turning it dirty after registers it again, behind this runtime lock.
            runtime
                .block_dirty
                .retain(|(block, _)| block.keep_registration());
            runtime
                .block_file_metadata_dirty
                .retain(|(block, _)| block.keep_registration());
            !runtime.block_dirty.is_empty() || !runtime.block_file_metadata_dirty.is_empty()
        };
        if registered {
            self.mark_dirty();
        }
    }

    pub fn format(&self) -> u32 {
        self.data_read().format
    }

    pub fn flags(&self) -> u32 {
        self.data_read().flags
    }

    pub async fn update_tree_root_hash(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<(), StateError> {
        let root_block = {
            let runtime = self.runtime_read();
            if !runtime.block.is_empty() {
                runtime.block[0].upgrade()
            } else {
                None
            }
        };

        let root_data = {
            if let Some(root_block) = root_block {
                #[cfg(feature = "state-hooks")]
                let _label = crate::state::node::hooks::log::label(
                    crate::state::node::hooks::log::Section::Save,
                );
                let mut block = root_block.write();
                let root_node = block.node(0);
                let root_hash = root_node.address.hash;
                let size = root_node.size;

                // By always resetting root node hash and size to zero we avoid first block
                // updating for every revision - if the updated subtree is fully
                // contained in another block(s) it should not affect the first block.
                root_node.address.hash.zero();
                root_node.size = 0;

                Some((root_hash, size))
            } else {
                None
            }
        };
        lore_trace!("Merkle tree root data {:?}", root_data);

        let tree = {
            if let Some((root_hash, size)) = root_data {
                let mut tree = self.tree(repository.clone()).await?;
                if root_hash != tree.hash_root {
                    tree.hash_root = root_hash;
                    tree.size = size;
                    tree.flags |= TreeFlags::Dirty;
                    Some(tree)
                } else {
                    None
                }
            } else {
                None
            }
        };

        let dirty = {
            if let Some(tree) = tree {
                let mut runtime = self.runtime_write();
                runtime.tree = Some(tree);
                true
            } else {
                false
            }
        };

        if dirty {
            let mut data = self.data_write();
            data.flags |= StateFlags::Dirty;
        }

        Ok(())
    }

    /// The branch this state's metadata records, which is the branch it was committed on, or the
    /// failure to read it, a refusal keeping its variant. [`Self::branch`] answers the zero branch
    /// ID instead of a failure. Reads only the branch, without the strings
    /// [`Self::revision_metadata`] allocates.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[inline]
    pub fn recorded_branch(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> impl Future<Output = Result<Context, StateError>> + Send + use<> {
        futures::FutureExt::map(
            metadata::Metadata::deserialize(repository, self.metadata_hash()),
            |metadata| -> Result<Context, StateError> {
                metadata
                    .forward::<StateError>("deserializing revision metadata")?
                    .get_branch()
                    .forward::<StateError>("reading the branch a revision records")
            },
        )
    }

    /// [`Self::recorded_branch`], or the zero branch ID where it cannot be read.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[inline]
    pub fn branch(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> impl Future<Output = Context> + Send + use<> {
        futures::FutureExt::map(self.recorded_branch(repository), Result::unwrap_or_default)
    }

    pub fn metadata_hash(&self) -> Hash {
        self.data_read().hash_metadata
    }

    /// The hash of the tree record, or zero when the state has no stored tree.
    pub fn tree_hash(&self) -> Hash {
        self.data_read().hash_tree
    }

    pub fn set_metadata_hash(&self, metadata: Hash) {
        let mut data = self.data_write();
        data.hash_metadata = metadata;
        data.flags |= StateFlags::Dirty;
    }

    pub fn set_delta_block(&self, delta_block: Hash, delta_count: usize) -> Result<(), StateError> {
        let mut tree = self.tree_readonly()?;
        if tree.hash_delta != delta_block {
            tree.hash_delta = delta_block;
            tree.delta_count = delta_count as u32;
            tree.flags |= TreeFlags::Dirty;
            self.runtime_write().tree = Some(tree);
        }
        Ok(())
    }

    pub async fn delta_block(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<Bytes, StateError> {
        let tree = self.tree(repository.clone()).await?;
        let options = immutable::read_options_from_repository(&repository)
            .with_cache()
            .with_priority();
        immutable::read(
            repository,
            Address::zero_context_hash(tree.hash_delta),
            None, /* Full range */
            options,
        )
        .await
        .forward::<StateError>("Failed to deserialize delta block")
    }

    pub async fn node_delta(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<Option<NodeDelta>, StateError> {
        let delta_block = self
            .delta_block(repository.clone())
            .await?
            .to_aligned::<NodeDelta>();

        for node_delta in delta_block.as_type_slice::<NodeDelta>().iter() {
            if node_delta.node == node {
                return Ok(Some(*node_delta));
            }
        }

        Ok(None)
    }

    /// Node blocks the tree has, whether or not any of them is in memory.
    pub fn block_count(&self) -> usize {
        self.runtime_read().block_count()
    }

    pub async fn block(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Result<Arc<NodeBlock>, StateError> {
        {
            let lock = self.runtime_read();
            if lock.block.len() > block_index
                && let Some(block) = lock.block[block_index].upgrade()
            {
                return Ok(block);
            }
        }

        Box::pin(self.block_deserialize(repository, block_index)).await
    }

    /// The heap bytes the chain logs of this State's blocks in memory hold
    /// ([`NodeBlock::chain_log_bytes`]). Reads each block once its runtime lock is released.
    #[doc(hidden)]
    pub fn chain_log_bytes(&self) -> usize {
        let resident: Vec<Arc<NodeBlock>> = self
            .runtime_read()
            .block
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        resident.iter().map(|block| block.chain_log_bytes()).sum()
    }

    /// Node block `block_index` where it is in memory, reading nothing.
    pub fn resident_block(&self, block_index: usize) -> Option<Arc<NodeBlock>> {
        let lock = self.runtime_read();
        lock.block.get(block_index).and_then(Weak::upgrade)
    }

    pub async fn try_block(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Option<Arc<NodeBlock>> {
        {
            let lock = self.runtime_read();
            if lock.block.len() > block_index
                && let Some(block) = lock.block[block_index].upgrade()
            {
                return Some(block);
            }
        }

        Box::pin(self.try_block_deserialize(repository, block_index)).await
    }

    async fn try_block_deserialize(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Option<Arc<NodeBlock>> {
        let (block_count, _hash_node) = {
            let Ok(tree) = self.tree(repository.clone()).await else {
                return None;
            };
            (tree.block_count as usize, tree.hash_node)
        };
        if block_index >= block_count {
            return None;
        }

        self.block_deserialize(repository, block_index).await.ok()
    }

    /// The permit held while block `block_index` is deserialized.
    fn block_loading_permit(&self, block_index: usize) -> &tokio::sync::Semaphore {
        let permits = self.block_loading.get_or_init(|| {
            (0..BLOCK_LOADING_PERMITS)
                .map(|_| tokio::sync::Semaphore::new(1))
                .collect()
        });
        &permits[block_index % BLOCK_LOADING_PERMITS]
    }

    async fn block_deserialize(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Result<Arc<NodeBlock>, StateError> {
        let (block_count, hash_node) = {
            let tree = self.tree(repository.clone()).await?;
            (tree.block_count as usize, tree.hash_node)
        };
        if block_index >= block_count {
            return Err(StateError::internal(format!(
                "Invalid block index: {block_index}"
            )));
        }

        #[cfg(feature = "state-hooks")]
        let waiting =
            crate::state::node::hooks::waiting(crate::state::node::hooks::Wait::BlockLoad);
        let _loading_permit = self
            .block_loading_permit(block_index)
            .acquire()
            .await
            .internal("Failed to deserialize node block")?;
        #[cfg(feature = "state-hooks")]
        drop(waiting);
        // One permit covers many blocks, so the task that held it before this
        // one may have published this block, or a different one.
        {
            let lock = self.runtime_read();
            if lock.block.len() > block_index
                && let Some(block) = lock.block[block_index].upgrade()
            {
                return Ok(block);
            }
        }

        let (mut block_hash_bytes, rehash_node_names) = {
            let lock = self.runtime_read();
            (lock.block_address.clone(), lock.rehash_node_names)
        };

        if block_index >= block_hash_bytes.count::<Hash>() {
            // Avoid multiple tasks deserializing block list and block at the same time
            #[cfg(feature = "state-hooks")]
            let waiting =
                crate::state::node::hooks::waiting(crate::state::node::hooks::Wait::BlockLoad);
            let _guard = self
                .block_deserialize
                .acquire()
                .await
                .internal("Failed to deserialize node block")?;
            #[cfg(feature = "state-hooks")]
            drop(waiting);

            block_hash_bytes = {
                let lock = self.runtime_read();
                lock.block_address.clone()
            };

            // Check if deserialize still needed after getting lock
            if block_index >= block_hash_bytes.count::<Hash>() {
                if hash_node.is_zero() {
                    let block = NodeBlock::new_zeroed();
                    if block_index == 0 {
                        // Reserve root node
                        let block = block.clone();
                        let mut block_writer = block.write();
                        let node_block = block_writer.node_block();
                        node_block.node_count = 1;
                    }
                    {
                        let mut lock = self.runtime_write();
                        if block_index >= lock.block.len() {
                            lock.block.resize(block_count, Weak::default());
                        }
                        if let Some(prev_block) = lock.block[block_index].upgrade() {
                            return Ok(prev_block);
                        }
                        lock.block[block_index] = Arc::downgrade(&block);
                        #[cfg(feature = "state-hooks")]
                        block.set_hook_index(
                            &self.hooks,
                            block_index,
                            crate::state::node::hooks::log::Install::Loaded { zeroed: true },
                        );
                    }
                    return Ok(block);
                }

                block_hash_bytes = self
                    .read_node_block_addresses(&repository, hash_node, block_count)
                    .await?;
            }
        }

        let block_hash = block_hash_bytes.as_type_slice::<Hash>();
        if block_index >= block_hash.len() {
            return Err(StateError::internal(format!(
                "Invalid block index: {block_index}"
            )));
        }

        if block_hash[block_index].is_zero() {
            let block = NodeBlock::new_zeroed();
            let mut lock = self.runtime_write();
            if block_index >= lock.block.len() {
                lock.block.resize(block_count, Weak::default());
            }
            if let Some(prev_block) = lock.block[block_index].upgrade() {
                return Ok(prev_block);
            }
            if block_index == 0 {
                // Reserve root node
                let block = block.clone();
                let mut block_writer = block.write();
                let node_block = block_writer.node_block();
                node_block.node_count = 1;
            }
            lock.block[block_index] = Arc::downgrade(&block);
            #[cfg(feature = "state-hooks")]
            block.set_hook_index(
                &self.hooks,
                block_index,
                crate::state::node::hooks::log::Install::Loaded { zeroed: true },
            );
            if lock.block_cache.len() < MAX_BLOCK_CACHE {
                lock.block_cache.push(block.clone());
            } else {
                let cache_index = lock
                    .block_cache_counter
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    as usize;
                lock.block_cache[cache_index % MAX_BLOCK_CACHE] = block.clone();
            }
            return Ok(block);
        }

        lore_trace!("Deserialize state node block {block_index}");

        let address = Address::zero_context_hash(block_hash[block_index]);
        #[cfg(feature = "state-hooks")]
        let waiting =
            crate::state::node::hooks::waiting(crate::state::node::hooks::Wait::BlockLoad);
        let deserialized = NodeBlock::deserialize(repository, self, address).await;
        #[cfg(feature = "state-hooks")]
        drop(waiting);
        let block = deserialized?;
        {
            let mut lock = self.runtime_write();
            if block_index >= lock.block.len() {
                lock.block.resize(block_count, Weak::default());
            }
            if let Some(prev_block) = lock.block[block_index].upgrade() {
                return Ok(prev_block);
            }

            if rehash_node_names && block.claim_registration() {
                lock.block_dirty.push((block.clone(), block_index));
            }
            // Loaded flagged, as its records may hold slots an earlier session left detached.
            lock.note_unswept(block_index);

            lock.block[block_index] = Arc::downgrade(&block);
            #[cfg(feature = "state-hooks")]
            block.set_hook_index(
                &self.hooks,
                block_index,
                crate::state::node::hooks::log::Install::Loaded { zeroed: false },
            );
            if lock.block_cache.len() < MAX_BLOCK_CACHE {
                lock.block_cache.push(block.clone());
            } else {
                let cache_index = lock
                    .block_cache_counter
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    as usize;
                lock.block_cache[cache_index % MAX_BLOCK_CACHE] = block.clone();
            }
        }
        lore_trace!("Deserialized state node block {block_index}");

        Ok(block)
    }

    /// Loads every node block holding a child of `parent`, with its name table, and returns the
    /// blocks so they stay resident while the caller walks the children.
    ///
    /// The children form a sibling chain across blocks, so a block is known only once the block
    /// before it is loaded: the blocks load in turn, and each name table loads while the walk
    /// goes on.
    #[lore_macro::test_pub]
    pub(crate) async fn load_children_with_nametables(
        self: &Arc<Self>,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
    ) -> Result<Vec<Arc<NodeBlock>>, StateError> {
        let mut children =
            StateNodeChildrenIterator::new(self.clone(), repository.clone(), parent).await?;
        let mut blocks: Vec<Arc<NodeBlock>> = Vec::new();
        let mut nametables: JoinSet<Result<(), StateError>> = JoinSet::new();
        while children.next().await?.is_some() {
            let Some(block) = children.block() else {
                continue;
            };
            if !push_distinct(&mut blocks, block) {
                continue;
            }
            if !block.is_nametable_deserialized() {
                let block = block.clone();
                let repository = repository.clone();
                lore_spawn!(
                    nametables,
                    crate::subtask!(async move { block.deserialize_nametable(repository).await })
                );
            }
        }
        lore_drain_tasks!(
            nametables,
            StateError::internal("Failed to load a node name table")
        )?;
        Ok(blocks)
    }

    pub async fn block_with_nametable(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Result<Arc<NodeBlock>, StateError> {
        let block = self.block(repository.clone(), block_index).await?;

        block.deserialize_nametable(repository).await?;

        Ok(block)
    }

    /// The file-metadata block for `block_index`, but only when one exists:
    /// already resident, or recorded in the tree and therefore worth reading.
    ///
    /// `None` means no metadata has ever been stored for this block, so every
    /// slot in it reads as zero. It is the answer to "is there anything here to
    /// clear", asked without paying for the answer:
    /// [`Self::block_file_metadata`] would hand back a freshly zeroed block in
    /// this case, and that block is 65,568 bytes, is zeroed on allocation, and is
    /// published only as a `Weak` — so nothing keeps it alive and the next caller
    /// allocates another one.
    ///
    /// Nothing is stored for a tree with no address list, for a block the list holds
    /// a zero hash for, or for a block past the end of the list, added since it was
    /// read. A list not read yet is read by [`Self::block_file_metadata`].
    ///
    /// A resident block is always returned, whatever the tree has stored: it may
    /// hold metadata from a slot that has since been freed, which a caller
    /// recycling that slot has to clear.
    ///
    /// A block this reads from the store stays resident, up to `MAX_CHECKED_FILE_METADATA_BLOCKS`
    /// of them. [`Self::node_add`] asks for the block on every add into it, and a block held only
    /// as a `Weak` would be read again for each of them. A block with nothing stored is not kept:
    /// the address list answers for it once read.
    pub async fn try_block_file_metadata_existing(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Result<Option<Arc<NodeFileMetadataBlock>>, StateError> {
        {
            let lock = self.runtime_read();
            if lock.block_file_metadata.len() > block_index
                && let Some(block) = lock.block_file_metadata[block_index].upgrade()
            {
                return Ok(Some(block));
            }
        }

        let Tree { block_count, .. } = self.tree(repository.clone()).await?;
        if block_index >= block_count as usize {
            return Err(StateError::internal(format!(
                "Invalid block index: {block_index}"
            )));
        }

        // A tree that records no file metadata, or an address list in memory with no entry or a
        // zero hash at this index, answers without a read. Otherwise `block_file_metadata` reads
        // the list, and repeats the same test against it before deserializing.
        if let Ok(answer) = self.resident_file_metadata(block_index) {
            return Ok(answer);
        }

        let block = self.block_file_metadata(repository, block_index).await?;
        let mut runtime = self.runtime_write();
        let stored = runtime
            .block_file_metadata_address
            .as_type_slice::<Hash>()
            .get(block_index)
            .is_some_and(|hash| !hash.is_zero());
        if stored {
            runtime.keep_checked_file_metadata(&block);
        }
        Ok(Some(block))
    }

    /// The file-metadata block for `block_index`, loaded unless it is resident.
    ///
    /// Only the load is boxed, as in [`Self::block`]. Inline, it would make every future awaiting
    /// a lookup as large as the load's, whether or not that lookup loads.
    pub async fn block_file_metadata(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Result<Arc<NodeFileMetadataBlock>, StateError> {
        {
            let lock = self.runtime_read();
            if lock.block_file_metadata.len() > block_index
                && let Some(block) = lock.block_file_metadata[block_index].upgrade()
            {
                return Ok(block);
            }
        }

        Box::pin(self.block_file_metadata_load(repository, block_index)).await
    }

    /// [`Self::block_file_metadata`] for a block that was not resident when asked for.
    #[lore_macro::test_pub]
    async fn block_file_metadata_load(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Result<Arc<NodeFileMetadataBlock>, StateError> {
        let Tree {
            block_count,
            hash_file_metadata,
            ..
        } = self.tree(repository.clone()).await?;
        if block_index >= block_count as usize {
            return Err(StateError::internal(format!(
                "Invalid block index: {block_index}"
            )));
        }

        #[cfg(feature = "state-hooks")]
        let waiting =
            crate::state::node::hooks::waiting(crate::state::node::hooks::Wait::FileMetadata);
        let _loading_permit = self
            .block_loading_permit(block_index)
            .acquire()
            .await
            .internal("Failed to deserialize metadata")?;
        #[cfg(feature = "state-hooks")]
        drop(waiting);
        // One permit covers many blocks, so the task that held it before this
        // one may have published this block, or a different one.
        {
            let lock = self.runtime_read();
            if lock.block_file_metadata.len() > block_index
                && let Some(block) = lock.block_file_metadata[block_index].upgrade()
            {
                return Ok(block);
            }
        }

        {
            let mut lock = self.runtime_write();
            if block_index >= lock.block_file_metadata.len() {
                lock.block_file_metadata
                    .resize(block_count as usize, Weak::default());
            }
        }

        let stored = self
            .file_metadata_addresses(&repository, hash_file_metadata, block_count as usize)
            .await?
            .as_type_slice::<Hash>()
            .get(block_index)
            .copied()
            .filter(|hash| !hash.is_zero());
        let block = Arc::new(match stored {
            Some(hash) => NodeFileMetadataBlock::new({
                #[cfg(feature = "state-hooks")]
                let waiting = crate::state::node::hooks::waiting(
                    crate::state::node::hooks::Wait::FileMetadata,
                );
                let data = NodeFileMetadataBlockData::read_box_from_immutable_compat(
                    repository.clone(),
                    Address::zero_context_hash(hash),
                    true,
                )
                .await
                .forward::<StateError>("Failed to deserialize file metadata block")?;
                #[cfg(feature = "state-hooks")]
                drop(waiting);
                data
            }),
            None => NodeFileMetadataBlock::default(),
        });
        #[cfg(feature = "state-hooks")]
        block.set_hook_name(self.hooks.ordinal(), block_index);

        let mut lock = self.runtime_write();
        if let Some(prev_block) = lock.block_file_metadata[block_index].upgrade() {
            return Ok(prev_block);
        }
        lock.block_file_metadata[block_index] = Arc::downgrade(&block);
        Ok(block)
    }

    /// The file metadata block address list, read from the store once.
    ///
    /// The list held covers every block the tree had when the list was read or written, so a
    /// block past its end was added since and has nothing stored. It is empty for a tree with no
    /// list. Every block's lookup wants the one list, so it is read under `metadata_deserialize`
    /// rather than once by each. The read is boxed: it runs once per state, and inline it would
    /// make every lookup's future as large as the read.
    async fn file_metadata_addresses(
        &self,
        repository: &Arc<RepositoryContext>,
        hash_file_metadata: Hash,
        block_count: usize,
    ) -> Result<Bytes, StateError> {
        {
            let held = self.runtime_read().block_file_metadata_address.clone();
            if !held.is_empty() || hash_file_metadata.is_zero() {
                return Ok(held);
            }
        }

        #[cfg(feature = "state-hooks")]
        let waiting =
            crate::state::node::hooks::waiting(crate::state::node::hooks::Wait::FileMetadata);
        let _guard = self
            .metadata_deserialize
            .acquire()
            .await
            .internal("Failed to deserialize metadata")?;
        #[cfg(feature = "state-hooks")]
        drop(waiting);
        {
            let held = self.runtime_read().block_file_metadata_address.clone();
            if !held.is_empty() {
                return Ok(held);
            }
        }

        // TODO(mjansson): To support huge trees we might want to selectively
        // read the block addresses instead of all in one big buffer
        #[cfg(feature = "state-hooks")]
        let waiting =
            crate::state::node::hooks::waiting(crate::state::node::hooks::Wait::FileMetadata);
        let mut addresses = Box::pin(immutable::read(
            repository.clone(),
            Address::zero_context_hash(hash_file_metadata),
            None, /* Read the full array of block hashes */
            immutable::read_options_from_repository(repository)
                .with_cache()
                .with_priority(),
        ))
        .await
        .forward::<StateError>("Failed to deserialize file metadata block list")?;
        #[cfg(feature = "state-hooks")]
        drop(waiting);
        if addresses.count::<Hash>() < block_count {
            addresses = addresses
                .clone_and_resize_zeroed::<Hash>(block_count)
                .freeze();
        }
        self.runtime_write().block_file_metadata_address = addresses.clone();
        Ok(addresses)
    }

    /// The node block address list, read from the store once: as
    /// [`Self::file_metadata_addresses`], for the node blocks, under `block_deserialize`.
    ///
    /// Serialize writes the list from this, so a State that loaded no block still writes the
    /// stored address of every block it did not change.
    async fn node_block_addresses(
        &self,
        repository: &Arc<RepositoryContext>,
        hash_node: Hash,
        block_count: usize,
    ) -> Result<Bytes, StateError> {
        {
            let held = self.runtime_read().block_address.clone();
            if !held.is_empty() || hash_node.is_zero() {
                return Ok(held);
            }
        }

        #[cfg(feature = "state-hooks")]
        let waiting =
            crate::state::node::hooks::waiting(crate::state::node::hooks::Wait::BlockLoad);
        let _guard = self
            .block_deserialize
            .acquire()
            .await
            .internal("Failed to deserialize node block")?;
        #[cfg(feature = "state-hooks")]
        drop(waiting);
        {
            let held = self.runtime_read().block_address.clone();
            if !held.is_empty() {
                return Ok(held);
            }
        }
        Box::pin(self.read_node_block_addresses(repository, hash_node, block_count)).await
    }

    /// Every node block's address hash and every file-metadata block's address hash, by block
    /// index, each list read once per state. A zero hash, or an index past a list's end, is a block
    /// with nothing stored.
    pub async fn block_address_lists(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<(Bytes, Bytes), StateError> {
        let Tree {
            block_count,
            hash_node,
            hash_file_metadata,
            ..
        } = self.tree(repository.clone()).await?;
        let block_count = block_count as usize;
        tokio::try_join!(
            self.node_block_addresses(&repository, hash_node, block_count),
            self.file_metadata_addresses(&repository, hash_file_metadata, block_count),
        )
    }

    /// Reads the node block address list `hash_node` names, padded with zero addresses to
    /// `block_count`, and holds it. The caller holds `block_deserialize`.
    async fn read_node_block_addresses(
        &self,
        repository: &Arc<RepositoryContext>,
        hash_node: Hash,
        block_count: usize,
    ) -> Result<Bytes, StateError> {
        // TODO(mjansson): To support huge trees we might want to selectively
        // read the block addresses instead of all in one big buffer
        lore_trace!("Deserialize block address list");
        #[cfg(feature = "state-hooks")]
        let waiting =
            crate::state::node::hooks::waiting(crate::state::node::hooks::Wait::BlockLoad);
        let mut addresses = immutable::read(
            repository.clone(),
            Address::zero_context_hash(hash_node),
            None, /* Read the full array of block hashes */
            immutable::read_options_from_repository(repository)
                .with_cache()
                .with_priority(),
        )
        .await
        .forward::<StateError>("Failed to deserialize node block list")?;
        #[cfg(feature = "state-hooks")]
        drop(waiting);
        if addresses.count::<Hash>() < block_count {
            addresses = addresses
                .clone_and_resize_zeroed::<Hash>(block_count)
                .freeze();
        }
        self.runtime_write().block_address = addresses.clone();
        lore_trace!("Deserialized block address list");
        Ok(addresses)
    }

    pub fn parents(&self) -> [Hash; 2] {
        self.data_read().parent
    }

    pub fn parent_self(&self) -> Hash {
        self.data_read().parent[0]
    }

    pub fn parent_other(&self) -> Hash {
        self.data_read().parent[1]
    }

    pub fn set_parent_self(&self, signature: Hash) {
        let mut lock = self.data_write();
        lock.parent[0] = signature;
        lock.flags |= StateFlags::Dirty;
    }

    pub fn set_parent_other(&self, signature: Hash) {
        let mut lock = self.data_write();
        lock.parent[1] = signature;
        if !signature.is_zero() {
            lock.flags |= StateFlags::Merge;
        } else {
            lock.flags &= !StateFlags::Merge;
        }
        lock.flags |= StateFlags::Dirty;
    }

    pub fn revision(&self) -> Hash {
        self.runtime_read().signature
    }

    /// Point the state at the revision it is based on.
    ///
    /// [`Self::serialize`] leaves the signature at whatever it last wrote, which is
    /// what a caller restoring a pre-commit snapshot has to undo: the state is based
    /// on the revision the handle was loaded at, not on the snapshot it was rebuilt
    /// from. Pair it with [`Self::mark_dirty`] — a state with unserialized edits and
    /// a signature is exactly what an edited handle looks like.
    pub fn set_revision(&self, signature: Hash) {
        self.runtime_write().signature = signature;
    }

    pub fn state_data(&self) -> StateData {
        *self.data_read()
    }

    pub fn revision_number(&self) -> u64 {
        self.data_read().revision_number
    }

    pub fn committed_revision(&self) -> Hash {
        if self.revision_number() == 0 {
            self.parent_self()
        } else {
            self.revision()
        }
    }

    /// Whether this tree tracks the node it holds at `id`, carrying `flags`: one it stages, or one the revision
    /// it was built on holds, wherever this tree has since moved it. Everything else this tree
    /// holds is only marked, as an add or a copy is, and the ignore rules apply to it
    /// ([ADR-00027](../../docs/developing/decisions/00027-ignore-filter-applies-to-untracked-paths-only.md)).
    ///
    /// A tree shares its node IDs with the revision it was built on, so a node that revision
    /// holds sits at the same ID, with the same type, in the revision's tree. The revision is read
    /// apart from this tree, which a walk may be adding nodes to, the first time it is asked, and
    /// kept for as long as this tree is built on it.
    pub async fn tracks(
        &self,
        repository: &Arc<RepositoryContext>,
        id: NodeID,
        flags: NodeFlags,
    ) -> Result<bool, StateError> {
        if flags.contains(NodeFlags::Staged) || id == ROOT_NODE {
            return Ok(true);
        }
        // Boxed: a walk reaches this only where the ignore rules would leave a node out, and
        // unboxed the revision's read would sit in every walk future that asks.
        Box::pin(self.base_holds(repository, self.committed_revision(), id, flags.node_type()))
            .await
    }

    /// Whether `base`, a revision this tree shares its node IDs with, holds a node of `node_type`
    /// at `id`.
    pub(crate) async fn base_holds(
        &self,
        repository: &Arc<RepositoryContext>,
        base: Hash,
        id: NodeID,
        node_type: crate::interface::LoreNodeType,
    ) -> Result<bool, StateError> {
        self.base(repository, base)
            .await?
            .holds_node(repository, id, node_type)
            .await
    }

    /// The nodes of `nodes`, each with the flags this tree holds it with, that `base`, a revision
    /// this tree shares its node IDs with, does not hold live as a node of the same type, sorted.
    ///
    /// [`Self::base_holds`] for many nodes at once: it reads the base and its block count once for
    /// the run, and keeps the block of the node last asked about with the slots on its free list
    /// ([`node::discard::free_list_slots`]), read again where the next node lies in another block.
    /// A slot holds a node only where it is in use, below the block's node count and off its free
    /// list, and the node is not `Discarded`, as [`Self::holds_node`] answers: a freed slot's flags
    /// are exactly `Discarded`, which reads as a directory, and a directory a discard detaches
    /// keeps its type with `Discarded` set. Only the read of `base` is boxed, as only it can load
    /// the revision.
    pub(crate) async fn base_lacks(
        &self,
        repository: &Arc<RepositoryContext>,
        base: Hash,
        nodes: impl IntoIterator<Item = (NodeID, NodeFlags)>,
    ) -> Result<Vec<NodeID>, StateError> {
        let held = Box::pin(self.base(repository, base)).await?;
        let block_count = held.tree(repository.clone()).await?.block_count as usize;
        let mut last: Option<(usize, Arc<NodeBlock>, [u64; node::discard::SLOT_WORDS])> = None;
        let mut lacking = Vec::new();
        for (id, flags) in nodes {
            if id == ROOT_NODE {
                continue;
            }
            let block_index = NodeBlock::index(id);
            if block_index >= block_count {
                lacking.push(id);
                continue;
            }
            let (block, free) = match &last {
                Some((index, block, free)) if *index == block_index => (block, free),
                _ => {
                    let block = held.block(repository.clone(), block_index).await?;
                    let free = node::discard::free_list_slots(block.read().node_block());
                    let (_, block, free) = last.insert((block_index, block, free));
                    (&*block, &*free)
                }
            };
            let index = Node::index(id);
            let holds = free[index / 64] & 1 << (index % 64) == 0 && {
                let reader = block.read();
                index < reader.node_block().node_count as usize && {
                    let node = reader.node(index);
                    !node.is_discarded() && node.node_type() == flags.node_type()
                }
            };
            if !holds {
                lacking.push(id);
            }
        }
        lacking.sort_unstable();
        Ok(lacking)
    }

    /// The parent `base`, a revision this tree shares its node IDs with, holds a node at `id` under;
    /// `None` where it holds none there. Only the first read of `base` is boxed, as only it loads
    /// the revision.
    pub(crate) async fn base_parent(
        &self,
        repository: &Arc<RepositoryContext>,
        base: Hash,
        id: NodeID,
    ) -> Result<Option<NodeID>, StateError> {
        let cached = self
            .base
            .lock()
            .as_ref()
            .filter(|(signature, _)| *signature == base)
            .map(|(_, held)| held.clone());
        let held = match cached {
            Some(held) => held,
            None => Box::pin(self.base(repository, base)).await?,
        };
        Ok(held
            .node_in_use(repository, id)
            .await?
            .and_then(|(node, _)| (!node.is_discarded()).then_some(node.parent)))
    }

    /// Whether this tree tracks the node it holds at `id`, carrying `flags`, as a directory where
    /// `is_directory` says and as a file otherwise, a link counting as a directory. A working-tree
    /// entry of the other kind at the node's path is new content.
    pub async fn tracks_as(
        &self,
        repository: &Arc<RepositoryContext>,
        id: NodeID,
        flags: NodeFlags,
        is_directory: bool,
    ) -> Result<bool, StateError> {
        Ok(flags.contains(NodeFlags::File) != is_directory
            && self.tracks(repository, id, flags).await?)
    }

    /// Whether something this tree tracks is staged or marked dirty at or below the node it holds
    /// at `id`, carrying `flags`. Staged and dirty marks propagate to the ancestors, so a node
    /// carrying neither holds no change below it. A dirty mark on a node the tree does not track
    /// counts only where it holds a staged one below it.
    pub async fn holds_tracked_change(
        &self,
        repository: &Arc<RepositoryContext>,
        id: NodeID,
        flags: NodeFlags,
    ) -> Result<bool, StateError> {
        Ok(flags.contains(NodeFlags::Staged)
            || (flags.contains(NodeFlags::Dirty) && self.tracks(repository, id, flags).await?))
    }

    /// Holds `base`, the State of a revision this tree shares its node IDs with, as the one
    /// [`Self::base`] answers for that revision, for a caller that has read it already.
    pub(crate) fn hold_base(&self, base: Arc<State>) {
        *self.base.lock() = Some((base.revision(), base));
    }

    /// The State of `revision`, a revision this tree shares its node IDs with, read once for as
    /// long as it is the one asked for.
    async fn base(
        &self,
        repository: &Arc<RepositoryContext>,
        revision: Hash,
    ) -> Result<Arc<State>, StateError> {
        let cached = self
            .base
            .lock()
            .as_ref()
            .filter(|(signature, _)| *signature == revision)
            .map(|(_, base)| base.clone());
        if let Some(base) = cached {
            return Ok(base);
        }
        let base = State::deserialize(repository.clone(), revision).await?;
        *self.base.lock() = Some((revision, base.clone()));
        Ok(base)
    }

    /// Whether this tree holds a live node of `node_type` at `id`: one in use and not `Discarded`.
    pub(crate) async fn holds_node(
        &self,
        repository: &Arc<RepositoryContext>,
        id: NodeID,
        node_type: crate::interface::LoreNodeType,
    ) -> Result<bool, StateError> {
        if id == ROOT_NODE {
            return Ok(true);
        }
        Ok(self
            .node_in_use(repository, id)
            .await?
            .is_some_and(|(node, _)| !node.is_discarded() && node.node_type() == node_type))
    }

    /// The record this tree holds at `id`, with its block; `None` where the slot holds no node.
    async fn node_in_use(
        &self,
        repository: &Arc<RepositoryContext>,
        id: NodeID,
    ) -> Result<Option<(Node, Arc<NodeBlock>)>, StateError> {
        let block_index = NodeBlock::index(id);
        if block_index >= self.tree(repository.clone()).await?.block_count as usize {
            return Ok(None);
        }
        let block = self.block(repository.clone(), block_index).await?;
        let index = Node::index(id);
        let node = {
            let reader = block.read();
            reader.is_node_in_use(index).then(|| *reader.node(index))
        };
        Ok(node.map(|node| (node, block)))
    }

    /// Moves `id`, which this tree holds marked dirty as moved and not staged, back to the parent
    /// and name `base` holds it at, `base` being the revision this tree shares its node IDs with,
    /// and answers what became of it, with the path it stood at where `want_path` asks for it and
    /// it does not stand there already. `name` is a buffer the caller keeps across nodes.
    ///
    /// Where a node that may still leave holds the name, the node waits under the parent by a name
    /// of its own ([`Return::Parked`]), so its own place, or a directory that is to be discarded,
    /// holds it no longer. Asked again, it goes back once the name is free. A directory marked
    /// dirty as added and not staged is such a holder: the caller discards it, unless it holds a
    /// staged node, which stages it. A staged node holding the name, or a parent that is no live
    /// directory, keeps the node out for good ([`Return::Displaced`]). A file marked dirty as added
    /// and not staged does not keep it out: the caller discards each such file before it asks of
    /// the name there again, so the two hold one name only until then.
    ///
    /// The caller asks of a node that holds no staged node, once it has asked of every node below
    /// it. Each of those then stands under the directory `base` holds it in, is no node of `base`,
    /// or has left the tree: only a move takes a node from that directory, and none below is
    /// staged, so each moved one has gone back, waits there, or has left the tree. So the parent,
    /// which `base` holds above the node, lies inside the node's subtree only where `base` holds a
    /// cycle, which is an internal error.
    ///
    /// Nothing else changes the tree while this runs, so what the checks read stands when the
    /// node moves ([`Self::node_move`] checks nothing). Only a node that does not stand where
    /// `base` holds it takes the checks and the move, which are boxed: inline, they would sit in
    /// the future of every node this is asked about.
    pub(crate) async fn node_return_unstaged_move(
        &self,
        repository: &Arc<RepositoryContext>,
        base: Hash,
        id: NodeID,
        name: &mut String,
        want_path: bool,
    ) -> Result<(Return, Option<RelativePath>), StateError> {
        let (parent, name_hash, flags, context) = {
            let node = self.node(repository.clone(), id).await?;
            if node.is_discarded() {
                return Ok((Return::Gone, None));
            }
            (
                node.parent,
                node.name_hash,
                NodeFlags::from_bits_retain(node.flags),
                node.address.context,
            )
        };
        let Some(committed) = self
            .base_place(repository, (base, id), (flags, context), name)
            .await?
        else {
            return Ok((Return::Stays, None));
        };
        let renamed =
            name_hash != committed.1 || !self.node_name_is(repository.clone(), id, name).await?;
        if parent == committed.0 && !renamed {
            return Ok((Return::Stays, None));
        }
        let path = if want_path {
            self.node_path_below(repository.clone(), id, ROOT_NODE)
                .await?
        } else {
            None
        };
        let is_directory = !flags.intersects(NodeFlags::File | NodeFlags::Link);
        let moved = (id, parent, is_directory);
        let name = renamed.then_some(name.as_str());
        let returned = Box::pin(self.node_return_to(repository, moved, committed, name)).await?;
        Ok((returned, path))
    }

    /// Whether `id`, this tree's node `node`, stands where `base`, the revision this tree shares
    /// its node IDs with, holds it, under the same parent by the same name, or `base` holds no
    /// node of its kind and context at `id`, as [`Self::node_return_unstaged_move`] finds a node
    /// that stays. `name` is a buffer the caller keeps across nodes.
    pub(crate) async fn stands_at_base_place(
        &self,
        repository: &Arc<RepositoryContext>,
        base: Hash,
        (id, node): (NodeID, &Node),
        name: &mut String,
    ) -> Result<bool, StateError> {
        let flags = NodeFlags::from_bits_retain(node.flags);
        let Some(committed) = self
            .base_place(repository, (base, id), (flags, node.address.context), name)
            .await?
        else {
            return Ok(true);
        };
        Ok(node.parent == committed.0
            && node.name_hash == committed.1
            && self.node_name_is(repository.clone(), id, name).await?)
    }

    /// Moves `node` to the directory `parent`, renamed to `name` where one is given, or parks it
    /// there, as [`Self::node_return_unstaged_move`] describes. `from` is the directory that holds
    /// the node now, `is_directory` whether the node is one, and `name_hash` the hash of the name
    /// it takes.
    ///
    /// Flags neither directory, where the unserved tier's reattach ([`Self::node_reattach`]) flags
    /// both before it moves a node: the node goes back to the place the base revision holds it at,
    /// so neither directory needs rehashing for it, and the caller clears each dirty flag it
    /// reaches.
    async fn node_return_to(
        &self,
        repository: &Arc<RepositoryContext>,
        (node, from, is_directory): (NodeID, NodeID, bool),
        (parent, name_hash): (NodeID, u64),
        name: Option<&str>,
    ) -> Result<Return, StateError> {
        if !self.is_live_directory(repository, parent).await? {
            return Ok(Return::Displaced);
        }
        if is_directory
            && parent != from
            && self
                .is_inside_subtree(repository.clone(), parent, node)
                .await?
        {
            return Err(StateError::internal(
                "A move marked dirty would return into its own subtree",
            ));
        }
        let holder = match self
            .find_subnode(repository.clone(), parent, name_hash)
            .await
        {
            Ok(holder) => (holder != node).then_some(holder),
            Err(error) if error.is_node_not_found() => None,
            Err(error) => return Err(error),
        };
        if let Some(holder) = holder {
            let (staged, added_file) = {
                let held = self.node(repository.clone(), holder).await?;
                (
                    held.is_staged(),
                    held.is_dirty_add() && !held.is_directory(),
                )
            };
            if staged {
                return Ok(Return::Displaced);
            }
            if !added_file {
                let parked = ParkedName::of(node);
                if from != parent
                    || !self
                        .node_name_is(repository.clone(), node, parked.as_str())
                        .await?
                {
                    self.node_move(
                        repository.clone(),
                        node,
                        parent,
                        NameChange::Rename(parked.as_str()),
                        Mark::NONE,
                    )
                    .await?;
                }
                return Ok(Return::Parked);
            }
        }
        let change = name.map_or(NameChange::Keep, NameChange::Rename);
        self.node_move(repository.clone(), node, parent, change, Mark::NONE)
            .await?;
        Ok(Return::Back)
    }

    /// Where `base` holds `id`, a node of this tree carrying `flags` and the address `context`: its
    /// parent and name hash, with its name copied into `name`. `None` where `base` holds no node of
    /// the same kind and context at `id`.
    async fn base_place(
        &self,
        repository: &Arc<RepositoryContext>,
        (base, id): (Hash, NodeID),
        (flags, context): (NodeFlags, Context),
        name: &mut String,
    ) -> Result<Option<(NodeID, u64)>, StateError> {
        let base = self.base(repository, base).await?;
        let (place, block) = {
            let Some((held, block)) = base.node_in_use(repository, id).await? else {
                return Ok(None);
            };
            if held.is_discarded()
                || held.node_type() != flags.node_type()
                || held.address.context != context
            {
                return Ok(None);
            }
            ((held.parent, held.name_hash), block)
        };
        block.deserialize_nametable(repository.clone()).await?;
        block
            .read_node_and_name(Node::index(id), |_, stored| {
                name.clear();
                stored.map(|stored| name.push_str(stored))
            })
            .forward::<StateError>("Node name")?;
        Ok(Some(place))
    }

    /// Whether `node` is a directory this tree holds, not discarded.
    async fn is_live_directory(
        &self,
        repository: &Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<bool, StateError> {
        let node = self.node(repository.clone(), node).await?;
        Ok(!node.is_discarded() && node.is_directory())
    }

    /// Whether `parent` can take `node`, a directory where `is_directory` says: `parent` is a live
    /// directory, outside the node's own subtree.
    pub(crate) async fn can_take(
        &self,
        repository: &Arc<RepositoryContext>,
        parent: NodeID,
        (node, is_directory): (NodeID, bool),
    ) -> Result<bool, StateError> {
        if !self.is_live_directory(repository, parent).await? {
            return Ok(false);
        }
        Ok(!is_directory
            || !self
                .is_inside_subtree(repository.clone(), parent, node)
                .await?)
    }

    /// Renames `node` where it stands to the name it waits under ([`ParkedName`]), so no lookup of
    /// a name it held meets it.
    pub(crate) async fn node_park(
        &self,
        repository: &Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<(), StateError> {
        let parked = ParkedName::of(node);
        self.node_rename(
            repository.clone(),
            node,
            NameChange::Rename(parked.as_str()),
        )
        .await
    }

    pub fn set_revision_number(&self, revision_number: u64) {
        let mut data = self.data_write();
        if data.revision_number != revision_number {
            data.revision_number = revision_number;
            data.flags |= StateFlags::Dirty;
        }
    }

    /// Make the state a staged state on top of `current_revision`.
    ///
    /// A state that is the committed revision `current_revision` itself drops that revision's
    /// second parent and metadata, which belong to the revision and not to the one staged on top
    /// of it. A staged state, numbered 0, is left as it is when `current_revision` is its own
    /// signature, which is no revision to stage on. A merge that
    /// [reconciles divergent history](Self::reconciles_divergent_history) keeps its first parent,
    /// the branch latest it merged; `current_revision` is already its second parent.
    pub fn reparent_onto(&self, current_revision: Hash) {
        let based_on_current = self.revision() == current_revision;
        if based_on_current && self.revision_number() == 0 {
            return;
        }

        self.set_revision_number(0);
        if !self.reconciles_divergent_history(current_revision) {
            self.set_parent_self(current_revision);
        }

        if based_on_current {
            self.set_parent_other(Hash::default());
            self.set_metadata_hash(Hash::default());
        }
    }

    /// [Reparent](Self::reparent_onto) a staged state onto `current_revision`, the head of a new
    /// branch, in place of `previous_revision`.
    ///
    /// A merge that [reconciles divergent history](Self::reconciles_divergent_history) takes the
    /// branch latest it merged as its second parent, so the new branch's first-parent line reaches
    /// its branch point.
    pub fn reparent_replacing(&self, previous_revision: Hash, current_revision: Hash) {
        if self.reconciles_divergent_history(previous_revision) {
            self.set_parent_other(self.parent_self());
        }
        self.reparent_onto(current_revision);
    }

    pub fn is_merge(&self) -> bool {
        self.data_read().flags & StateFlags::Merge != 0
    }

    /// Whether the state is a merge whose second parent is `current_revision`, which is how a
    /// merge of the branch's latest into `current_revision` is staged: the branch latest is the
    /// first parent and stays the first-parent line.
    pub fn reconciles_divergent_history(&self, current_revision: Hash) -> bool {
        self.is_merge() && self.parent_other() == current_revision
    }

    pub fn is_cherry_pick(&self) -> bool {
        self.data_read().flags & StateFlags::CherryPick != 0
    }

    pub fn is_revert(&self) -> bool {
        self.data_read().flags & StateFlags::Revert != 0
    }

    pub fn is_merge_or_cherry_pick_or_revert(&self) -> bool {
        self.data_read().flags & (StateFlags::Merge | StateFlags::CherryPick | StateFlags::Revert)
            != 0
    }

    pub fn is_conflict(&self) -> bool {
        self.data_read().flags & StateFlags::Conflict != 0
    }

    pub fn is_dirty(&self) -> bool {
        self.data_read().flags & StateFlags::Dirty != 0
    }

    pub fn set_merge(&self) {
        let mut data = self.data_write();
        data.flags |= StateFlags::Merge;
    }

    pub fn set_cherry_pick(&self) {
        let mut data = self.data_write();
        data.flags |= StateFlags::CherryPick;
    }

    pub fn set_revert(&self) {
        let mut data = self.data_write();
        data.flags |= StateFlags::Revert;
    }

    pub fn set_conflict(&self) {
        let mut data = self.data_write();
        data.flags |= StateFlags::Conflict;
        data.flags |= StateFlags::Dirty;
    }

    pub fn set_merge_conflict(&self) {
        let mut data = self.data_write();
        data.flags |= StateFlags::Conflict | StateFlags::Merge | StateFlags::Dirty;
    }

    pub fn set_cherry_pick_conflict(&self) {
        let mut data = self.data_write();
        data.flags |= StateFlags::Conflict | StateFlags::CherryPick | StateFlags::Dirty;
    }

    pub fn set_revert_conflict(&self) {
        let mut data = self.data_write();
        data.flags |= StateFlags::Conflict | StateFlags::Revert | StateFlags::Dirty;
    }

    pub fn reset_merge_conflict_flags(&self) {
        let mut data = self.data_write();
        data.flags &= !(StateFlags::Conflict
            | StateFlags::Merge
            | StateFlags::CherryPick
            | StateFlags::Revert);
    }

    /// Registers `block`, which turned dirty, for the next save, and marks the State dirty with
    /// it: a save of a State not dirty writes nothing, so a block registered under a clean State
    /// would go into no save (C14). A block registered already, as its registered bit says, is
    /// not registered twice, and takes no runtime lock. Only the State's own code registers a node
    /// block: every writer outside it goes through the node operations, which do.
    pub(in crate::state) fn block_modified(&self, block: Arc<NodeBlock>, index: usize) {
        if block.claim_registration() {
            lore_trace!("Node block {index} marked dirty");
            let mut runtime = self.runtime_write();
            #[cfg(feature = "state-hooks")]
            assert_resident(&runtime.block, &block, index);
            runtime.block_dirty.push((block, index));
            runtime.note_unswept(index);
        }
        self.mark_dirty();
    }

    /// The node blocks in memory a sweep scans ([`StateRuntime::unswept`]), with the notes they
    /// were found by, which stay as they are until [`Self::settle_unswept`] takes them, so a sweep
    /// dropped before takes nothing. The note of a block not in memory is not among them: it
    /// stays for the sweep after the block loads, as a walk may load it flagged meanwhile.
    pub(in crate::state) fn unswept_blocks(&self) -> (Vec<(usize, Arc<NodeBlock>)>, Vec<u64>) {
        let runtime = self.runtime_read();
        let mut noted = runtime.unswept.clone();
        let mut found =
            Vec::with_capacity(noted.iter().map(|word| word.count_ones() as usize).sum());
        for (word_index, word) in noted.iter_mut().enumerate() {
            let mut bits = *word;
            while bits != 0 {
                let bit = bits.trailing_zeros();
                let index = word_index * 64 + bit as usize;
                bits &= bits - 1;
                match runtime.block.get(index).and_then(Weak::upgrade) {
                    Some(resident) => found.push((index, resident)),
                    None => *word &= !(1 << bit),
                }
            }
        }
        drop(runtime);
        (found, noted)
    }

    /// Takes `noted`, the notes a sweep found its blocks by, and notes again every block
    /// registered now, which may flag itself again before a save releases it, and `kept`, the
    /// blocks of the roots the sweep kept.
    pub(in crate::state) fn settle_unswept(
        &self,
        noted: &[u64],
        kept: impl Iterator<Item = usize>,
    ) {
        let mut runtime = self.runtime_write();
        let StateRuntime {
            unswept,
            block_dirty,
            ..
        } = &mut *runtime;
        for (word, taken) in unswept.iter_mut().zip(noted) {
            *word &= !taken;
        }
        for &(_, index) in block_dirty.iter() {
            set_block_bit(unswept, index);
        }
        for index in kept {
            set_block_bit(unswept, index);
        }
    }

    /// Counts a sweep's scan or a pending drain that freed a slot, which every chain mark and
    /// absence proof taken before answers stale after ([`Self::reclaims`]).
    pub(in crate::state) fn note_reclaim(&self) {
        self.reclaims.fetch_add(1, Ordering::Release);
    }

    /// How many times a sweep's scan or a pending drain freed a slot over this State's life,
    /// which a chain mark or an absence proof records when taken.
    #[lore_macro::test_pub]
    pub(in crate::state) fn reclaims(&self) -> u32 {
        self.reclaims.load(Ordering::Acquire)
    }

    /// `block_modified` for a file metadata block, which must be this State's block at
    /// `index`: its registered bit answers for this State alone.
    pub fn block_file_metadata_modified(&self, block: Arc<NodeFileMetadataBlock>, index: usize) {
        if block.claim_registration() {
            lore_trace!("File metadata block {index} marked dirty");
            let mut runtime = self.runtime_write();
            #[cfg(feature = "state-hooks")]
            assert_resident(&runtime.block_file_metadata, &block, index);
            runtime.block_file_metadata_dirty.push((block, index));
        }
        self.mark_dirty();
    }

    pub fn mark_dirty(&self) {
        let mut data = self.data_write();
        data.flags |= StateFlags::Dirty;
    }

    /// The live children of the directory `walk` walks, in sibling order, from `child_node`, until
    /// `step` answers for one of them or `stop_at` heads what is left of the chain.
    ///
    /// `child_node` is the directory's head, read together with the chain change count `walk`
    /// began from. Detached children are skipped and a child a reattach
    /// moved away restarts the walk (see `ChainWalk`), which may then visit a
    /// child twice, as [`Walked::may_repeat`] says. A run of siblings sharing a
    /// block is walked under one lock on it, so `step` must not read that block
    /// again: a second shared lock behind a queued writer deadlocks. The answer is
    /// `None` where `step` answered for none of them.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[lore_macro::test_pub]
    #[allow(clippy::manual_async_fn)]
    fn walk_children<T, F>(
        &self,
        repository: Arc<RepositoryContext>,
        mut child_node: Option<NodeID>,
        mut walk: ChainWalk,
        stop_at: Option<NodeID>,
        mut step: F,
    ) -> impl Future<Output = Result<Walked<T>, StateError>>
    where
        F: FnMut(NodeID, &Node) -> Option<T>,
    {
        async move {
            let mut restart = false;
            let mut steps = step_counting::Steps::walk();
            // A scheduling point where the walk goes on to another block, or starts again, so a
            // walk within one block that never restarts, as most are, adds none.
            #[cfg(feature = "state-hooks")]
            let mut onward = false;
            loop {
                if restart {
                    restart = false;
                    #[cfg(feature = "state-hooks")]
                    crate::state::node::hooks::log::arm(
                        &self.hooks,
                        crate::state::node::hooks::log::Arm::SearchRestart,
                    );
                    child_node = walk.head_again();
                }
                let Some(first_in_block) = child_node else {
                    if walk.restart_at_end()? {
                        restart = true;
                        continue;
                    }
                    return Ok(Walked {
                        answer: None,
                        may_repeat: walk.may_repeat(),
                        reached_end: true,
                    });
                };
                if Some(first_in_block) == stop_at {
                    return Ok(Walked {
                        answer: None,
                        may_repeat: walk.may_repeat(),
                        reached_end: false,
                    });
                }
                #[cfg(feature = "state-hooks")]
                {
                    if onward {
                        crate::state::node::hooks::reached(
                            crate::state::node::hooks::Point::IterStep,
                        )
                        .await;
                    }
                    onward = true;
                }
                let iblock = NodeBlock::index(first_in_block);
                let block = self.block(repository.clone(), iblock).await?;
                let mut next = Some(first_in_block);
                {
                    let reader = block.read();
                    while let Some(child_id) = next {
                        if Some(child_id) == stop_at || NodeBlock::index(child_id) != iblock {
                            break;
                        }
                        let child = reader.node(Node::index(child_id));
                        steps.step();
                        match walk.step(child_id, child)? {
                            ChainStep::Restart => {
                                restart = true;
                                break;
                            }
                            ChainStep::Skip => {}
                            ChainStep::Visit => {
                                if let Some(answer) = step(child_id, child) {
                                    return Ok(Walked {
                                        answer: Some(answer),
                                        may_repeat: walk.may_repeat(),
                                        reached_end: false,
                                    });
                                }
                            }
                        }
                        next = child.sibling();
                    }
                }
                child_node = next;
            }
        }
    }

    /// The children of a directory node in sibling order, each taken from the
    /// record the walk read for it by `extract`.
    ///
    /// `extract` gets each child's position in the walk too, and runs under the
    /// lock [`Self::walk_children`] holds, so the constraint that carries is that
    /// it must not read the block again. After a walk that may have repeated,
    /// `drop_repeats` makes each child come once. A link resolves to the children
    /// of the node it points at. A directory's listing comes with what `M` keeps of the head
    /// and count its walk began from: nothing, or a [`ChainMark`]; a link's and a file's with
    /// none.
    async fn node_children_map<T, F, M: ListingMark>(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
        extract: F,
        drop_repeats: fn(&mut Vec<T>),
    ) -> Result<(Vec<T>, M), StateError>
    where
        F: Fn(NodeID, &Node, u32) -> T + Copy,
    {
        let parent_id = node;
        let block = self
            .block(repository.clone(), NodeBlock::index(parent_id))
            .await?;
        let reclaims = M::reclaims(self);
        let (node, changes) = block.node_with_chain_head(parent_id);
        if node.is_directory() {
            let head = node.child();
            let mark = M::of(reclaims, parent_id, head, &block, changes);
            let mut children = vec![];
            let walked = self
                .walk_children(
                    repository.clone(),
                    head,
                    ChainWalk::new(parent_id, block, changes),
                    None,
                    |child_id, child| {
                        children.push(extract(child_id, child, children.len() as u32));
                        None::<()>
                    },
                )
                .await?;
            if walked.may_repeat {
                drop_repeats(&mut children);
            }
            Ok((children, mark))
        } else if node.is_link() {
            let link = node.linked_node();
            let linked_repository = repository.to_link_context(link.repository).await;
            let link_state = State::deserialize(linked_repository.clone(), link.revision).await?;
            Box::pin(link_state.node_children_map::<T, F, ()>(
                linked_repository.clone(),
                link.node,
                extract,
                drop_repeats,
            ))
            .await
            .map(|(children, ())| (children, M::none()))
        } else {
            Ok((vec![], M::none()))
        }
    }

    /// The children of a directory node in sibling order, or in node id order
    /// after a walk that a concurrent change may have made repeat.
    ///
    /// A link resolves to the children of the node it points at; a node that
    /// takes no children, such as a file, reports none.
    pub async fn node_children(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<Vec<NodeID>, StateError> {
        self.node_children_map(
            repository,
            node,
            |child_id, _, _| child_id,
            |children| {
                children.sort_unstable();
                children.dedup();
            },
        )
        .await
        .map(|(children, ())| children)
    }

    /// The first child of the marked directory carrying `name_hash` that its walk from the
    /// chain's head to the head `since` saw meets: what was prepended or reattached there
    /// since.
    ///
    /// A child is prepended, so everything prepended since lies ahead of the head
    /// the mark saw, and the walk stops there rather than covering children the
    /// caller already holds; a walk not meeting that head, which its detach causes,
    /// covers the whole chain. A miss answers [`AddedSince::Stale`] where the
    /// directory's count moved by [`REATTACH_CHANGES`], read at the walk's end, so a
    /// reattach during the walk counts: since the mark, for a walk stopping at the
    /// mark's head, as a reattach puts a node at the head, the mark's head among
    /// them, in front of children prepended since (H9); since the walk's start, for
    /// a walk covering the whole chain, where only a restart can pass over a node. A
    /// mark of another State's directory is stale, as is one of a node no longer a
    /// directory, and one taken before a sweep freed a slot (C15).
    pub async fn find_subnode_added_since(
        &self,
        repository: Arc<RepositoryContext>,
        since: &ChainMark,
        name_hash: u64,
    ) -> Result<AddedSince, StateError> {
        if since.reclaims != self.reclaims() {
            return Ok(AddedSince::Stale);
        }
        let parent_node = since.parent;
        let Some(block) = self
            .resident_block(NodeBlock::index(parent_node))
            .filter(|block| Arc::ptr_eq(block, &since.block))
        else {
            return Ok(AddedSince::Stale);
        };
        let (head, changes) = {
            let (parent, changes) = block.node_with_chain_changes(Node::index(parent_node));
            if !parent.is_directory() {
                return Ok(AddedSince::Stale);
            }
            (parent.child(), changes)
        };
        let walked = self
            .walk_children(
                repository,
                head,
                ChainWalk::new(parent_node, block, changes),
                since.head,
                |child_id, child| (child.name_hash == name_hash).then_some(child_id),
            )
            .await?;
        if let Some(found) = walked.answer {
            return Ok(AddedSince::Found(found));
        }
        // A walk that ran to the chain's end covered what the chain held at its start, read
        // with `changes`.
        let base = if walked.reached_end {
            changes
        } else {
            since.changes
        };
        let moved = since
            .block
            .chain_changes(Node::index(parent_node))
            .wrapping_sub(base);
        Ok(if moved >= REATTACH_CHANGES {
            AddedSince::Stale
        } else {
            AddedSince::Absent
        })
    }

    /// A find of `name_hash` below `parent`, for a retry whose earlier attempt found no other
    /// node of the name, with `since`, the head and count read from `parent_block` before that
    /// attempt's lookup: only what was prepended since is walked
    /// ([`Self::find_subnode_added_since`]), the whole chain where a move's weight landed
    /// meanwhile. Its callers box it, as only a retry takes it.
    #[lore_macro::test_pub]
    pub(in crate::state) async fn find_subnode_since(
        &self,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        parent_block: &Arc<NodeBlock>,
        (head, changes): (Option<NodeID>, u32),
        name_hash: u64,
    ) -> Result<NodeID, StateError> {
        let added = {
            // No sweep runs beside the retry, so the count it reads is the lookup's.
            let since = ChainMark {
                parent,
                head,
                block: parent_block.clone(),
                changes,
                reclaims: self.reclaims(),
            };
            self.find_subnode_added_since(repository.clone(), &since, name_hash)
                .await?
        };
        match added {
            AddedSince::Found(found) => Ok(found),
            AddedSince::Absent => Err(NodeNotFound.into()),
            AddedSince::Stale => self.find_subnode(repository, parent, name_hash).await,
        }
    }

    /// The dirty adds among the children of `parent_node`, each with its name hash. Empty where
    /// the state holds no `parent_node`, or it is not dirty, as a dirty child marks every
    /// ancestor but the root. After a walk that may have repeated, each comes once.
    pub(crate) async fn node_dirty_add_children(
        &self,
        repository: Arc<RepositoryContext>,
        parent_node: NodeID,
    ) -> Result<Vec<(NodeID, u64)>, StateError> {
        let mut dirty_adds = Vec::new();
        if !parent_node.is_valid_or_root_node_id() {
            return Ok(dirty_adds);
        }
        let Ok(block) = self
            .block(repository.clone(), NodeBlock::index(parent_node))
            .await
        else {
            return Ok(dirty_adds);
        };
        let (parent, changes) = block.node_with_chain_head(parent_node);
        if !parent.is_directory() || (parent_node != ROOT_NODE && !parent.is_dirty()) {
            return Ok(dirty_adds);
        }
        let walked = self
            .walk_children(
                repository,
                parent.child(),
                ChainWalk::new(parent_node, block, changes),
                None,
                |child_id, child| {
                    if child.is_dirty_add() {
                        dirty_adds.push((child_id, child.name_hash));
                    }
                    None::<()>
                },
            )
            .await?;
        if walked.may_repeat {
            dirty_adds.sort_unstable();
            dirty_adds.dedup();
        }
        Ok(dirty_adds)
    }

    /// The name hashes of the children of `parent_node` staged for delete.
    pub(crate) async fn node_staged_delete_name_hashes(
        &self,
        repository: Arc<RepositoryContext>,
        parent_node: NodeID,
    ) -> Result<HashSet<u64>, StateError> {
        let block = self
            .block(repository.clone(), NodeBlock::index(parent_node))
            .await?;
        let (parent, changes) = block.node_with_chain_head(parent_node);
        let mut name_hashes = HashSet::new();
        self.walk_children(
            repository,
            parent.child(),
            ChainWalk::new(parent_node, block, changes),
            None,
            |_, child| {
                if child.is_staged_delete() {
                    name_hashes.insert(child.name_hash);
                }
                None::<()>
            },
        )
        .await?;
        Ok(name_hashes)
    }

    /// The first child of `parent_node` carrying `name_hash` ahead of `stop_at` in its chain. A
    /// child is prepended, so one linked after `stop_at` lies ahead of it. `None` where
    /// `parent_node` is no directory.
    pub(crate) async fn find_subnode_ahead_of(
        &self,
        repository: Arc<RepositoryContext>,
        parent_node: NodeID,
        stop_at: NodeID,
        name_hash: u64,
    ) -> Result<Option<NodeID>, StateError> {
        let block = self
            .block(repository.clone(), NodeBlock::index(parent_node))
            .await?;
        let (parent, changes) = block.node_with_chain_head(parent_node);
        if !parent.is_directory() {
            return Ok(None);
        }
        let walked = self
            .walk_children(
                repository,
                parent.child(),
                ChainWalk::new(parent_node, block, changes),
                Some(stop_at),
                |child_id, child| (child.name_hash == name_hash).then_some(child_id),
            )
            .await?;
        Ok(walked.answer)
    }

    /// The children of a directory node in sibling order, each with the name
    /// hash the walk read for it. After a walk that may have repeated, each child
    /// comes once, at its last position in the walk and with the hash it carried
    /// there.
    pub async fn node_children_with_name_hash(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<Vec<(NodeID, u64)>, StateError> {
        let (children, ()) = self
            .node_children_map(repository, node, with_name_hash, keep_last_with_name_hash)
            .await?;
        Ok(without_walk_index(children))
    }

    /// [`Self::node_children_with_name_hash`], with the mark of the chain the listing began
    /// from, for [`Self::find_subnode_added_since`]; `None` for a link or a file.
    pub async fn node_children_with_name_hash_marked(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<(Vec<(NodeID, u64)>, Option<ChainMark>), StateError> {
        let (children, mark) = self
            .node_children_map(repository, node, with_name_hash, keep_last_with_name_hash)
            .await?;
        Ok((without_walk_index(children), mark))
    }

    pub async fn node_name_clone(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<String, StateError> {
        let block = self
            .block_with_nametable(repository.clone(), NodeBlock::index(node))
            .await?;
        block
            .read_node_and_name(Node::index(node), |_, stored| {
                #[cfg(feature = "state-hooks")]
                if let Ok(stored) = stored {
                    block.hook_named(node, stored);
                }
                stored.map(str::to_owned)
            })
            .forward::<StateError>("Node name")
    }

    /// Whether `node`'s stored name is `name`, compared where it is stored, under its block's
    /// read lock, and copied nowhere.
    pub async fn node_name_is(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
        name: &str,
    ) -> Result<bool, StateError> {
        let block = self
            .block_with_nametable(repository, NodeBlock::index(node))
            .await?;
        block
            .read_node_and_name(Node::index(node), |_, stored| {
                #[cfg(feature = "state-hooks")]
                if let Ok(stored) = stored {
                    block.hook_named(node, stored);
                }
                stored.map(|stored| stored == name)
            })
            .forward::<StateError>("Node name")
    }

    pub async fn node_name_ref(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<NodeNameLock, StateError> {
        let block = self
            .block_with_nametable(repository.clone(), NodeBlock::index(node))
            .await?;
        let name = block
            .node_name_ref(Node::index(node))
            .forward::<StateError>("Node name")?;
        #[cfg(feature = "state-hooks")]
        block.hook_named(node, &name);
        Ok(name)
    }

    /// [`Self::node_name_ref`], answering `None` for a node whose name the read path refuses,
    /// logged as the node being skipped; see [`NodeBlock::node_name_ref_or_skip`].
    ///
    /// The returned `NodeNameLock` holds a read lock on the node's block for its lifetime.
    /// Callers must drop it before calling `write()` on that block, or the write lock
    /// acquisition will deadlock. Nothing is held when the answer is `None`.
    pub async fn node_name_ref_or_skip(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<Option<NodeNameLock>, StateError> {
        let block = self
            .block_with_nametable(repository, NodeBlock::index(node))
            .await?;
        let name = block
            .node_name_ref_or_skip(Node::index(node), node)
            .forward::<StateError>("Node name")?;
        #[cfg(feature = "state-hooks")]
        if let Some(name) = &name {
            block.hook_named(node, name);
        }
        Ok(name)
    }

    /// The node `node` names and its name, read from one block, or `None` for a node whose name
    /// the read path refuses, logged as the node being skipped; see
    /// [`NodeBlock::node_name_ref_or_skip`].
    ///
    /// The name holds a read lock on the node's block for its lifetime, which callers drop before
    /// calling `write()` on that block. The node is copied out before that lock is taken: a second
    /// read lock on the block while the name holds one waits behind any writer queued between them.
    pub async fn node_and_name_or_skip(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<Option<(Node, NodeNameLock)>, StateError> {
        let block = self
            .block_with_nametable(repository, NodeBlock::index(node))
            .await?;
        let index = Node::index(node);
        let read = block.node(index);
        Ok(block
            .node_name_ref_or_skip(index, node)
            .forward::<StateError>("Node name")?
            .map(|name| (read, name)))
    }

    /// [`Self::node_name_clone`], answering `None` for a node whose name the read path refuses,
    /// logged as the node being skipped; see [`NodeBlock::node_name_ref_or_skip`].
    ///
    /// The name is copied out, so no lock on the block outlives the call.
    pub async fn node_name_clone_or_skip(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<Option<String>, StateError> {
        Ok(self
            .node_name_ref_or_skip(repository, node)
            .await?
            .map(|name| name.to_string()))
    }

    /// Whether a child of the directory `parent_node` carries the Staged flag, as
    /// [`Self::holds_child`] reads it.
    pub fn node_has_staged_children(
        &self,
        repository: Arc<RepositoryContext>,
        parent_node: NodeID,
    ) -> impl Future<Output = Result<bool, StateError>> + Send + '_ {
        self.holds_child(repository, parent_node, Node::is_staged)
    }

    /// Whether a child of the directory `parent_node` carries the Dirty flag, as
    /// [`Self::holds_child`] reads it.
    pub fn node_has_dirty_children(
        &self,
        repository: Arc<RepositoryContext>,
        parent_node: NodeID,
    ) -> impl Future<Output = Result<bool, StateError>> + Send + '_ {
        self.holds_child(repository, parent_node, Node::is_dirty)
    }

    /// Whether a child of the directory `parent_node` carries the Staged or the Dirty flag, as
    /// [`Self::holds_child`] reads it.
    pub fn node_has_staged_or_dirty_children(
        &self,
        repository: Arc<RepositoryContext>,
        parent_node: NodeID,
    ) -> impl Future<Output = Result<bool, StateError>> + Send + '_ {
        self.holds_child(repository, parent_node, |child| {
            child.is_staged() || child.is_dirty()
        })
    }

    /// Whether a child of the directory `parent_node` answers `carries`, read in one walk of its
    /// chain that stops at the first. A node that is no directory has no child here, a link
    /// included, whose children are another tree's.
    async fn holds_child(
        &self,
        repository: Arc<RepositoryContext>,
        parent_node: NodeID,
        carries: fn(&Node) -> bool,
    ) -> Result<bool, StateError> {
        let block = self
            .block(repository.clone(), NodeBlock::index(parent_node))
            .await?;
        let (node, changes) = block.node_with_chain_head(parent_node);
        if !node.is_directory() {
            return Ok(false);
        }
        let walked = self
            .walk_children(
                repository,
                node.child(),
                ChainWalk::new(parent_node, block, changes),
                None,
                |_, child| carries(child).then_some(()),
            )
            .await?;
        Ok(walked.answer.is_some())
    }

    /// Collect the repository-relative paths of all dirty nodes at or under
    /// `root_node`, the set to stage. `base_path` is `root_node`'s path and the
    /// prefix of every returned path. A link at `root_node` and each dirty link
    /// below it are followed into the linked state each pins, up to
    /// [`MAX_LINK_DEPTH`] links deep.
    pub async fn collect_dirty_paths(
        &self,
        repository: Arc<RepositoryContext>,
        root_node: NodeID,
        base_path: RelativePath,
    ) -> Result<Vec<RelativePath>, StateError> {
        if let Some(pin) = self.link_pin(repository.clone(), root_node).await? {
            return Self::collect_pinned_dirty_paths(pin, &repository, base_path, 0).await;
        }
        self.collect_dirty_paths_linked(repository, root_node, base_path, 0)
            .await
    }

    /// The pin of `node` where it is a link, and `None` for any other node.
    async fn link_pin(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<Option<NodeLink>, StateError> {
        let node = self.node(repository, node).await?;
        Ok(node.is_link().then(|| node.linked_node()))
    }

    /// The dirty paths at or under the node `pin` names, in the state it opens, for the link at
    /// `path` that lies below `link_depth` links.
    async fn collect_pinned_dirty_paths(
        pin: NodeLink,
        repository: &Arc<RepositoryContext>,
        path: RelativePath,
        link_depth: usize,
    ) -> Result<Vec<RelativePath>, StateError> {
        let (linked_repository, linked_state) = pin.open(repository).await?;
        Box::pin(linked_state.collect_dirty_paths_linked(
            linked_repository,
            pin.node,
            path,
            link_depth + 1,
        ))
        .await
    }

    async fn collect_dirty_paths_linked(
        &self,
        repository: Arc<RepositoryContext>,
        root_node: NodeID,
        base_path: RelativePath,
        link_depth: usize,
    ) -> Result<Vec<RelativePath>, StateError> {
        let mut result = Vec::new();
        let force = execution_context().globals().force();
        // Work in immutable `RelativePath` throughout: `push_into_buf` borrows
        // the parent (no per-sibling clone) and a single `freeze` per retained
        // child is reused as the stored stack/result value, so the filter check
        // borrows it without an extra allocation.
        let base_states = repository.filter.exclusion_states(&base_path);
        let mut stack: Vec<(NodeID, RelativePath, FilterStates)> =
            vec![(root_node, base_path, base_states)];

        while let Some((node_id, path, states)) = stack.pop() {
            let children = self
                .node_children(repository.clone(), node_id)
                .await
                .forward::<StateError>("Failed to get children for dirty path collection")?;

            if node_id == root_node && children.is_empty() && !path.is_empty() {
                let node = self.node(repository.clone(), node_id).await?;
                let tracked = node.is_dirty_add()
                    && node.is_directory()
                    && !force
                    && self
                        .tracks(
                            &repository,
                            node_id,
                            NodeFlags::from_bits_retain(node.flags),
                        )
                        .await?;
                if node.is_dirty_add()
                    && node.is_directory()
                    && (force
                        || repository.filter.should_descend(
                            states,
                            &path,
                            FilterMode::TRACKED.at(tracked),
                        ))
                {
                    result.push(path);
                }
                continue;
            }

            for &child_id in &children {
                let child = self.node(repository.clone(), child_id).await?;
                if !child.is_dirty() {
                    continue;
                }

                let Some(name) = self
                    .node_name_clone_or_skip(repository.clone(), child_id)
                    .await?
                else {
                    continue;
                };
                let child_path = path.push_into_buf(&name).freeze();

                // Don't carry forward dirty paths the filter excludes; they
                // can't be replayed against a checkout that never materializes
                // them. --force bypasses the filter. The ignore slot has no say
                // over a node the tree tracks, only over one it only marks, as
                // an add or a copy.
                let mode = if force {
                    FilterMode::TRACKED
                } else {
                    repository
                        .filter
                        .tracked_mode(
                            states,
                            &child_path,
                            !child.is_file(),
                            FilterMode::TRACKED,
                            async || {
                                self.tracks(
                                    &repository,
                                    child_id,
                                    NodeFlags::from_bits_retain(child.flags),
                                )
                                .await
                            },
                        )
                        .await?
                };
                let (child_states, excluded) = repository.filter.child_excludes_tree_unless_forced(
                    force,
                    states,
                    &child_path,
                    !child.is_file(),
                    mode,
                );
                if excluded {
                    continue;
                }

                if child.is_file() {
                    result.push(child_path);
                } else if child.is_link() {
                    if link_depth < MAX_LINK_DEPTH {
                        let linked_paths = Self::collect_pinned_dirty_paths(
                            child.linked_node(),
                            &repository,
                            child_path,
                            link_depth,
                        )
                        .await?;
                        result.extend(linked_paths);
                    }
                } else if child.is_directory() {
                    // A new (dirty-add) directory is itself a change to stage. One
                    // only marked, not staged, is that only where it is empty, with
                    // no child to carry it: one holding nodes is carried by them,
                    // and recorded itself it would stage everything the working
                    // tree holds below it, marked or not.
                    if child.is_dirty_add() && !marked_directory_carried_by_children(&child) {
                        result.push(child_path.clone());
                    }
                    // A dirty-delete directory is staged as the deletion of its
                    // whole subtree: stage_delete on a directory recurses and
                    // stages a delete for every descendant. Emit just this path
                    // and don't descend — collecting descendants here would only
                    // queue redundant deletes that stage_delete already covers.
                    if child.is_dirty_delete() {
                        result.push(child_path);
                    } else {
                        stack.push((child_id, child_path, child_states));
                    }
                }
            }
        }

        Ok(result)
    }

    /// Find a child of `parent_node` by name hash; a parent that cannot take children is
    /// [`NodeNotFound`].
    ///
    /// The head is read with the directory's chain change count, so a name held throughout the
    /// find is found however the chain changes meanwhile: a walk a node moved under starts
    /// again from the head, and one that reaches the end without the name after a reattach
    /// moved the count ([`REATTACH_CHANGES`]) walks the chain again, since the reattach may have
    /// put the name's node at the head behind it. Walks again at most `CHAIN_WALK_RESTARTS`
    /// times, then reports [`SlowDown`].
    pub fn find_subnode(
        &self,
        repository: Arc<RepositoryContext>,
        parent_node: NodeID,
        name_hash: u64,
    ) -> impl Future<Output = Result<NodeID, StateError>> + Send + '_ {
        self.find_subnode_in(repository, parent_node, name_hash)
    }

    /// The find of [`Self::find_subnode`]: the parent's block loaded, its head read with its
    /// chain change count, and its chain walked.
    async fn find_subnode_in(
        &self,
        repository: Arc<RepositoryContext>,
        parent_node: NodeID,
        name_hash: u64,
    ) -> Result<NodeID, StateError> {
        let mut block = self
            .block(repository.clone(), NodeBlock::index(parent_node))
            .await?;
        let mut search = {
            let (parent, changes) = block.node_with_chain_changes(Node::index(parent_node));
            if !parent.is_directory() {
                return Err(NodeNotFound.into());
            }
            NameSearch::new(parent_node, &parent, block.clone(), changes, name_hash)
        };
        let mut iblock = NodeBlock::index(parent_node);
        loop {
            let Some(node_id) = search.next(self)? else {
                return Err(NodeNotFound.into());
            };
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached(crate::state::node::hooks::Point::FindStep).await;
            if iblock != NodeBlock::index(node_id) {
                iblock = NodeBlock::index(node_id);
                block = self.block(repository.clone(), iblock).await?;
            }
            let node = *block.read().node(Node::index(node_id));
            if search.found(node_id, &node)? {
                return Ok(node_id);
            }
        }
    }

    /// [`Self::find_subnode`] through blocks in memory only, for a request that reads nothing
    /// from the store: `None` on the first block it needs that is not in memory.
    pub fn find_subnode_resident(
        &self,
        parent_node: NodeID,
        name_hash: u64,
    ) -> Result<Option<NodeID>, StateError> {
        let mut iblock = NodeBlock::index(parent_node);
        let Some(mut block) = self.resident_block(iblock) else {
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::log::arm(
                &self.hooks,
                crate::state::node::hooks::log::Arm::NeedLoad,
            );
            return Ok(None);
        };
        let mut search = {
            let (parent, changes) = block.node_with_chain_changes(Node::index(parent_node));
            if !parent.is_directory() {
                return Err(NodeNotFound.into());
            }
            NameSearch::new(parent_node, &parent, block.clone(), changes, name_hash)
        };
        loop {
            let Some(node_id) = search.next(self)? else {
                return Err(NodeNotFound.into());
            };
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::window(crate::state::node::hooks::Window::ResidentFind);
            if iblock != NodeBlock::index(node_id) {
                iblock = NodeBlock::index(node_id);
                let Some(next) = self.resident_block(iblock) else {
                    #[cfg(feature = "state-hooks")]
                    crate::state::node::hooks::log::arm(
                        &self.hooks,
                        crate::state::node::hooks::log::Arm::NeedLoad,
                    );
                    return Ok(None);
                };
                block = next;
            }
            let node = *block.read().node(Node::index(node_id));
            if search.found(node_id, &node)? {
                return Ok(Some(node_id));
            }
        }
    }

    /// Counts a refusal of the served chain rule under its locks.
    #[cold]
    pub(in crate::state) fn served_refused(&self) {
        self.served_refusals.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a settle's clear a change overtook.
    #[cold]
    pub(in crate::state) fn clear_overtaken(&self) {
        self.clears_overtaken.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts an unserved clear marking its directory again.
    #[cold]
    pub(in crate::state) fn clear_rechecked(&self) {
        self.clears_rechecked.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts an unserved clear's mark finding its directory detached.
    #[cold]
    pub(in crate::state) fn clear_found_gone(&self) {
        self.clears_found_gone.fetch_add(1, Ordering::Relaxed);
    }

    /// Resolves `path` below `root`, entering links on the way.
    ///
    /// A component naming the repository's own directory resolves to nothing: no node carries
    /// that name, and a revision holding one anyway is not reached through it, whichever case
    /// it is spelled in. Resolution is by name hash, which folds case, so this is the one check
    /// on the path that a listing's skip of such a node does not already cover.
    ///
    /// Answers [`Self::find_relative`]'s future itself, mapped to the link, so the caller's future
    /// holds no second copy of the arguments.
    pub fn find_relative_node_link<'a>(
        &'a self,
        repository: Arc<RepositoryContext>,
        root: NodeID,
        path: &'a str,
    ) -> impl Future<Output = Result<NodeLink, StateError>> + Send + 'a {
        futures::FutureExt::map(self.find_relative(repository, root, path), |found| {
            found.map(|(link, _)| link)
        })
    }

    /// [`Self::find_relative`], with the repository and state holding the node: the linked ones
    /// where a link was crossed to reach it, and `repository` and this state otherwise.
    ///
    /// Answers [`Self::find_relative`]'s future itself, mapped, so the caller's future holds no
    /// second copy of the arguments.
    pub(crate) fn find_relative_held<'a>(
        self: &'a Arc<Self>,
        repository: Arc<RepositoryContext>,
        root: NodeID,
        path: &'a str,
    ) -> impl Future<Output = Result<(NodeLink, LinkedState), StateError>> + Send + 'a {
        let own = (repository.clone(), self.clone());
        futures::FutureExt::map(self.find_relative(repository, root, path), move |found| {
            found.map(|(link, held)| (link, held.unwrap_or(own)))
        })
    }

    /// [`Self::find_relative_node_link`], with the linked state holding the node where a link was
    /// crossed to reach it, for a caller that reads the node.
    pub(crate) async fn find_relative(
        &self,
        repository: Arc<RepositoryContext>,
        root: NodeID,
        path: &str,
    ) -> Result<(NodeLink, Option<LinkedState>), StateError> {
        let stop = self.find_to_link(&repository, root, path, false).await?;
        self.find_relative_from(repository, path, stop).await
    }

    /// [`Self::find_relative`] resumed where [`Self::find_to_link`] stopped on `path`, for a
    /// caller that reads the stop itself first.
    ///
    /// Not an `async fn`: only a link crossed awaits, and its future holds only what crossing
    /// it needs rather than the whole stop.
    pub(crate) fn find_relative_from<'a>(
        &self,
        repository: Arc<RepositoryContext>,
        path: &'a str,
        stop: PathStop,
    ) -> impl Future<Output = Result<(NodeLink, Option<LinkedState>), StateError>> + Send + 'a {
        let link = match stop.at {
            Stop::End => {
                return futures::future::Either::Left(std::future::ready(Ok((
                    NodeLink {
                        node: stop.node,
                        repository: repository.id,
                        revision: self.revision(),
                    },
                    None,
                ))));
            }
            Stop::Missing => {
                return futures::future::Either::Left(std::future::ready(Err(NodeNotFound.into())));
            }
            Stop::Link(link) => link,
        };
        let rest = path.get(stop.length + 1..).unwrap_or_default();
        futures::future::Either::Right(async move {
            let repository = repository.to_link_context(link.repository).await;
            let link_state = State::deserialize(repository.clone(), link.revision).await?;
            let (found, held) =
                Box::pin(link_state.find_relative(repository.clone(), link.node, rest)).await?;
            Ok((found, held.or(Some((repository, link_state)))))
        })
    }

    /// Where a walk of `path` below `root` stops: at its end, at the first link it crosses, or
    /// at a link it ends at where `ends_at_links` says, or at the deepest node it reaches where
    /// the tree holds nothing at the next component or the component is a name no node may
    /// carry.
    pub(crate) async fn find_to_link(
        &self,
        repository: &Arc<RepositoryContext>,
        root: NodeID,
        path: &str,
        ends_at_links: bool,
    ) -> Result<PathStop, StateError> {
        let mut node = root;
        let mut length = 0;
        let mut rest = path;
        while !rest.is_empty() {
            let (name, tail) = rest.split_once('/').unwrap_or((rest, ""));
            if is_reserved_node_name(name) {
                return Ok(PathStop::missing(node, length));
            }
            let name_hash = hash::hash_string(name);
            // Under `state-hooks` the log borrows the find's answer; the block ends that borrow
            // before the awaits after it.
            let found = {
                let found = self.find_subnode(repository.clone(), node, name_hash).await;
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::log::hop(&self.hooks, (node, name_hash), &found);
                found
            };
            node = match found {
                Ok(found) => found,
                Err(err) if err.is_node_not_found() => return Ok(PathStop::missing(node, length)),
                Err(err) => return Err(err),
            };
            length = path.len() - rest.len() + name.len();
            rest = tail;
            if !rest.is_empty() || ends_at_links {
                let block = self
                    .block(repository.clone(), NodeBlock::index(node))
                    .await?;
                let found = block.node(Node::index(node));
                if found.is_link() {
                    return Ok(PathStop {
                        node,
                        length,
                        at: Stop::Link(found.linked_node()),
                    });
                }
            }
        }
        Ok(PathStop {
            node,
            length,
            at: Stop::End,
        })
    }

    pub async fn find_node_link(
        &self,
        repository: Arc<RepositoryContext>,
        path: &str,
    ) -> Result<NodeLink, StateError> {
        self.find_relative_node_link(repository, ROOT_NODE, path)
            .await
    }

    /// The kind this tree tracks `path` below `root` as ([`State::tracks`]): `Some` of whether a
    /// filter asks about it as a directory, or `None` where the tree does not track it. The node is
    /// read through any link the path crosses, and a path the tree cannot resolve is not tracked.
    pub async fn relative_path_tracked_as(
        self: &Arc<Self>,
        repository: Arc<RepositoryContext>,
        root: NodeID,
        path: &str,
    ) -> Result<Option<bool>, StateError> {
        let Some(link) = self
            .find_relative_node_link(repository.clone(), root, path)
            .await
            .ok()
            .filter(NodeLink::is_valid)
        else {
            return Ok(None);
        };
        let Ok((repository, state)) = link.resolve(repository, self.clone()).await else {
            return Ok(None);
        };
        let Ok(node) = state.node(repository.clone(), link.node).await else {
            return Ok(None);
        };
        let flags = NodeFlags::from_bits_retain(node.flags);
        Ok(state
            .tracks(&repository, link.node, flags)
            .await?
            .then(|| !flags.contains(NodeFlags::File)))
    }

    pub async fn find_link_parent_node(
        &self,
        repository: Arc<RepositoryContext>,
        path: &str,
        target_repository_id: RepositoryId,
    ) -> Result<NodeID, StateError> {
        let mut current_path = RelativePath::from_str(path).unwrap_or_default();

        while !current_path.is_empty() {
            current_path.pop();

            if current_path.is_empty() {
                break;
            }

            if let Ok(node_link) = self
                .find_node_link(repository.clone(), current_path.as_str())
                .await
                && node_link.is_valid()
                && let Ok(node) = self.node(repository.clone(), node_link.node).await
                && node.is_link()
                && node.address.context == target_repository_id.into()
            {
                return Ok(node_link.node);
            }
        }

        Err(NodeNotFound.into())
    }

    pub async fn find_node(
        &self,
        repository: Arc<RepositoryContext>,
        path: &str,
    ) -> Result<Node, StateError> {
        let (found, held) = self
            .find_relative(repository.clone(), ROOT_NODE, path)
            .await?;
        self.found_node(repository, &found, held).await
    }

    /// The node [`Self::find_relative`] found, read in the linked state it returned where it
    /// crossed a link to reach the node.
    pub(crate) async fn found_node(
        &self,
        repository: Arc<RepositoryContext>,
        found: &NodeLink,
        held: Option<LinkedState>,
    ) -> Result<Node, StateError> {
        match held {
            Some((repository, state)) => state.node(repository, found.node).await,
            None => self.node(repository, found.node).await,
        }
    }

    pub async fn node(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<Node, StateError> {
        if !node.is_valid_or_root_node_id() {
            return Err(StateError::internal("Invalid node"));
        }
        let iblock = NodeBlock::index(node);
        let inode = Node::index(node);
        let block = self.block(repository, iblock).await?;
        let block_reader = block.read();
        Ok(*block_reader.node(inode))
    }

    pub async fn try_node(&self, repository: Arc<RepositoryContext>, node: NodeID) -> Option<Node> {
        if !node.is_valid_or_root_node_id() {
            return None;
        }
        let iblock = NodeBlock::index(node);
        let inode = Node::index(node);
        if let Some(block) = self.try_block(repository, iblock).await {
            let block_reader = block.read();
            Some(*block_reader.node(inode))
        } else {
            None
        }
    }

    /// The path from `ancestor` down to `node`, or `None` where the parent chain leaves the tree
    /// without reaching `ancestor`. `ancestor` itself is the empty path.
    async fn path_from_ancestor(
        &self,
        repository: Arc<RepositoryContext>,
        mut node: NodeID,
        ancestor: NodeID,
    ) -> Result<Option<RelativePathBuf>, StateError> {
        let mut nodes = vec![];
        while node != ancestor {
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached_reader(crate::state::node::hooks::Point::NodePath)
                .await;
            if !node.is_valid_node_id() {
                return Ok(None);
            }
            nodes.push(node);

            let block = self
                .block(repository.clone(), NodeBlock::index(node))
                .await?;
            node = block.node_parent(node);
        }

        let mut path = RelativePathBuf::new();
        for node in nodes.iter().rev() {
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached_reader(crate::state::node::hooks::Point::NodePath)
                .await;
            let name = self
                .node_name_ref(repository.clone(), *node)
                .await
                .forward::<StateError>("Node name")?;
            path.push(name);
        }

        Ok(Some(path))
    }

    /// The path `node` sits at in this state's own tree.
    ///
    /// This is the tree's spelling, which is the working-tree path only for a state the working
    /// tree materializes from its root. A walk carries the working-tree path instead; this is for
    /// the few places that need what a repository itself calls a node, such as resolving
    /// configuration that names one.
    pub async fn node_path(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<String, StateError> {
        Ok(self
            .path_from_ancestor(repository, node, ROOT_NODE)
            .await?
            .map_or_else(String::new, |path| path.to_string()))
    }

    /// The path of `node` below `ancestor`, or `None` where `ancestor` does not hold it.
    ///
    /// A working tree materializes the subtree an ancestor roots, so this is what to spell from
    /// the path that subtree is materialized at. `ancestor` itself is the empty path.
    pub async fn node_path_below(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
        ancestor: NodeID,
    ) -> Result<Option<RelativePath>, StateError> {
        Ok(self
            .path_from_ancestor(repository, node, ancestor)
            .await?
            .map(RelativePathBuf::freeze))
    }

    /// The children of `parent` in walk order, those staged for deletion where `include_deleted`;
    /// for a link, where `include_links`, the children of the node it mounts in its linked State.
    /// A child the walk visits twice is kept once, at its last place, and one it filters at its
    /// last visit, as a delete beside it stages it, as the earlier visit read it
    /// (`keep_last_in_walk_order`). Not an `async fn`, which would hold its arguments twice.
    pub fn collect_children_unsorted(
        self: &Arc<Self>,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        include_deleted: bool,
        include_links: bool,
    ) -> impl Future<Output = Result<StateChildrenNodes, StateError>> + Send + '_ {
        self.collect_children_unsorted_where(
            repository,
            parent,
            include_deleted,
            include_links,
            |_| true,
        )
    }

    /// [`Self::collect_children_unsorted`] of the children whose record `keep` keeps, read as the
    /// walk passes them, so the filter costs no read.
    #[lore_macro::test_pub]
    async fn collect_children_unsorted_where(
        self: &Arc<Self>,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        include_deleted: bool,
        include_links: bool,
        keep: impl Fn(&Node) -> bool + Copy + Send + Sync,
    ) -> Result<StateChildrenNodes, StateError> {
        let mut children = vec![];
        if !parent.is_valid_or_root_node_id() {
            return Ok(StateChildrenNodes {
                repository,
                state: self.clone(),
                children,
            });
        }

        let node = self.node(repository.clone(), parent).await?;
        if node.is_file() {
            return Ok(StateChildrenNodes {
                repository,
                state: self.clone(),
                children,
            });
        }

        if node.is_link() {
            if !include_links {
                return Ok(StateChildrenNodes {
                    repository,
                    state: self.clone(),
                    children,
                });
            }

            let link = node.linked_node();
            let linked_repository_id = link.repository;
            let signature = link.revision;
            let link_node = link.node;
            let linked_repository = repository.to_link_context(linked_repository_id).await;
            let link_state = State::deserialize(linked_repository.clone(), signature)
                .await
                .forward::<StateError>("Link error")?;

            let result = Box::pin(link_state.collect_children_unsorted_where(
                linked_repository.clone(),
                link_node,
                include_deleted,
                include_links,
                keep,
            ))
            .await?;

            return Ok(result);
        }

        let mut iter =
            StateNodeChildrenIterator::new(self.clone(), repository.clone(), parent).await?;
        while let Some((child_id, child_node)) = iter.next().await? {
            if (include_deleted || !child_node.is_staged_delete()) && keep(&child_node) {
                let walk_index = children.len() as u32;
                push_named_child(
                    &mut children,
                    StateNamedNode::new(
                        child_id,
                        walk_index,
                        child_node.name_hash,
                        NodeFlags::from_bits_retain(child_node.flags),
                    ),
                );
            }
        }
        if iter.may_repeat() {
            keep_last_in_walk_order(&mut children, |child| (child.node, child.walk_index()));
        }

        Ok(StateChildrenNodes {
            repository,
            state: self.clone(),
            children,
        })
    }

    pub async fn collect_named_children_unsorted(
        self: &Arc<Self>,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        include_deleted: bool,
        include_links: bool,
    ) -> Result<StateNamedChildrenNodes, StateError> {
        let mut children = vec![];
        if !parent.is_valid_or_root_node_id() {
            return Ok(StateNamedChildrenNodes {
                repository,
                state: self.clone(),
                children,
            });
        }

        let node = self.node(repository.clone(), parent).await?;
        if node.is_file() {
            return Ok(StateNamedChildrenNodes {
                repository,
                state: self.clone(),
                children,
            });
        }

        if node.is_link() {
            if !include_links {
                return Ok(StateNamedChildrenNodes {
                    repository,
                    state: self.clone(),
                    children,
                });
            }

            let link = node.linked_node();
            let linked_repository_id = link.repository;
            let signature = link.revision;
            let link_node = link.node;
            let linked_repository = repository.to_link_context(linked_repository_id).await;
            let link_state = State::deserialize(linked_repository.clone(), signature)
                .await
                .forward::<StateError>("Link error")?;

            let result = Box::pin(link_state.collect_named_children_unsorted(
                linked_repository.clone(),
                link_node,
                include_deleted,
                include_links,
            ))
            .await?;

            return Ok(result);
        }

        let mut iter =
            StateNodeChildrenWithNameIterator::new(self.clone(), repository.clone(), parent)
                .await?;
        while let Some((child_id, child_node, name_lock)) = iter.next().await? {
            if include_deleted || !child_node.is_staged_delete() {
                children.push(StateNamedStringNode {
                    node: child_id,
                    walk_index: children.len() as u32,
                    name: child_node.name_hash,
                    name_string: name_lock.freeze(),
                });
            }
        }
        if iter.may_repeat() {
            keep_last_in_walk_order(&mut children, |child| (child.node, child.walk_index));
        }

        Ok(StateNamedChildrenNodes {
            repository,
            state: self.clone(),
            children,
        })
    }

    pub fn tree_readonly(&self) -> Result<Tree, StateError> {
        {
            let lock = self.runtime_read();
            if let Some(tree) = lock.tree.as_ref() {
                return Ok(*tree);
            }
        }
        Err(StateError::internal("Tree not loaded"))
    }

    /// The loaded revision's tree header, installed on first use.
    ///
    /// Several callers can miss the cached value before any of them takes the
    /// write lock, so whichever one installs first wins and the rest adopt its
    /// tree. A second install would push another placeholder onto the block
    /// vector and make [`Self::block_count`] report a block the tree does not
    /// have.
    ///
    /// Only the read is boxed. Inline, it would make every future awaiting the tree as large as
    /// the read, whether or not the tree is loaded.
    pub async fn tree(&self, repository: Arc<RepositoryContext>) -> Result<Tree, StateError> {
        {
            let lock = self.runtime_read();
            if let Some(tree) = lock.tree.as_ref() {
                return Ok(*tree);
            }
        }

        let hash_tree = { self.data_read().hash_tree };

        let tree = {
            if hash_tree.is_zero() {
                let mut tree = Tree::new_zeroed();
                tree.magic = TREE_MAGIC;
                tree.format = TreeFormat::Initial as u32;
                tree.block_count = 1;
                {
                    let mut lock = self.runtime_write();
                    if let Some(current_tree) = &lock.tree {
                        tree = *current_tree;
                    } else {
                        lock.tree = Some(tree);
                        lock.block.push(Weak::new());
                    }
                }
                tree
            } else {
                let tree_address = Address::zero_context_hash(hash_tree);
                let options = read_options_from_repository(&repository);
                #[cfg(feature = "state-hooks")]
                let waiting =
                    crate::state::node::hooks::waiting(crate::state::node::hooks::Wait::BlockLoad);
                let mut tree =
                    Box::pin(Tree::read_from_immutable(repository, tree_address, options))
                        .await
                        .forward::<StateError>("Failed to deserialize tree")?;
                #[cfg(feature = "state-hooks")]
                drop(waiting);
                if tree.magic != TREE_MAGIC {
                    return Err(StateError::internal("Tree corrupt header"));
                } else if tree.format == 0 || tree.format > TreeFormat::Initial as u32 {
                    return Err(StateError::internal(format!(
                        "Tree invalid format: {}",
                        tree.format
                    )));
                } else if tree.block_count > MAX_TREE_BLOCK_COUNT {
                    return Err(StateError::from(Oversized {
                        context: format!(
                            "tree block count {} exceeds limit {}",
                            tree.block_count, MAX_TREE_BLOCK_COUNT
                        ),
                    }));
                }
                {
                    let mut lock = self.runtime_write();
                    if let Some(current_tree) = &lock.tree {
                        tree = *current_tree;
                    } else {
                        lock.tree = Some(tree);
                    }
                }
                tree
            }
        };

        Ok(tree)
    }

    /// Fetches every node block of the state and every block's name table in bulk requests and
    /// caches them. On demand each block costs two round trips under 256 permits, which leaves
    /// the walk of a tree of millions of files waiting on the network for hours. It fetches the
    /// whole tree whatever the view, so it is for syncs that walk most of it, and runs only when
    /// the instance sets `[sync] prefetch_tree`. Fetch failures leave the walk to fetch on demand.
    pub async fn prefetch_tree(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<(), StateError> {
        let tree = self.tree(repository.clone()).await?;
        let started = std::time::Instant::now();
        if tree.hash_node.is_zero() {
            return Ok(());
        }
        // The walk reads no block past the tree's block count, so neither does the prefetch
        let block_count = tree.block_count as usize;
        let addresses = self
            .node_block_addresses(&repository, tree.hash_node, block_count)
            .await?;
        let block_address: Vec<Address> = addresses
            .as_type_slice::<Hash>()
            .iter()
            .take(block_count)
            .filter(|hash| !hash.is_zero())
            .map(|&hash| Address::zero_context_hash(hash))
            .collect();
        let requested = block_address.len();
        let fetched = immutable::cache(repository.clone(), block_address, false)
            .await
            .unwrap_or(0);
        lore_debug!(
            "Prefetched {fetched} of {requested} tree blocks in {:.1}s",
            started.elapsed().as_secs_f64()
        );

        // Each block names its children through a name table fragment of its own, read on
        // demand when the walk enters the block. The blocks are local by now, so read them and
        // fetch every name table in bulk as well.
        let started = std::time::Instant::now();
        use futures::StreamExt as _;
        let name_address: Vec<Address> =
            futures::stream::iter(0..block_count)
                .map(|block_index| {
                    let repository = repository.clone();
                    async move {
                        self.block(repository, block_index).await.ok() as Option<Arc<NodeBlock>>
                    }
                })
                .buffer_unordered(256)
                .filter_map(|block| async move {
                    let hash = block?.name_table_hash();
                    (!hash.is_zero()).then(|| Address::zero_context_hash(hash))
                })
                .collect()
                .await;
        let requested = name_address.len();
        let fetched = immutable::cache(repository.clone(), name_address, false)
            .await
            .unwrap_or(0);
        lore_debug!(
            "Prefetched {fetched} of {requested} name tables in {:.1}s",
            started.elapsed().as_secs_f64()
        );
        Ok(())
    }

    /// Caches the state's own records in the local store, so the state reads without the remote,
    /// and with `file_metadata` every file metadata block, so staging and committing, which write
    /// to them, do too. The custom metadata a block names is not cached.
    ///
    /// Without `file_metadata` caching is best effort and its failures are disregarded. With it, a
    /// fragment left uncached fails the call, since an offline commit can need any block.
    pub async fn cache_fragments(
        &self,
        repository: Arc<RepositoryContext>,
        file_metadata: bool,
    ) -> Result<(), StateError> {
        let tree = self.tree(repository.clone()).await?;
        let blocks = if file_metadata {
            self.file_metadata_addresses(
                &repository,
                tree.hash_file_metadata,
                tree.block_count as usize,
            )
            .await?
        } else {
            Bytes::default()
        };
        let blocks = blocks.as_type_slice::<Hash>();

        let mut address = Vec::with_capacity(7 + blocks.len());

        // A remote can return the state and tree records without the local cache priority
        // flag, so a read does not keep them. Cache them to keep the state readable offline.
        address.push(Address::zero_context_hash(self.revision()));
        address.push(Address::zero_context_hash(tree.hash_node));

        {
            let data = self.data_read();
            address.push(Address::zero_context_hash(data.hash_tree));
            address.push(Address::zero_context_hash(data.hash_metadata));
            address.push(Address::zero_context_hash(data.hash_link));
        }

        address.push(Address::zero_context_hash(tree.hash_delta));
        address.push(Address::zero_context_hash(tree.hash_file_metadata));
        address.extend(
            blocks
                .iter()
                .filter(|hash| !hash.is_zero())
                .map(|&hash| Address::zero_context_hash(hash)),
        );

        let cached = immutable::cache(repository.clone(), address, true).await;
        if file_metadata {
            cached.forward::<StateError>(
                "Failed to cache the state records and file metadata blocks",
            )?;
        }

        /* Avoid caching all the blocks, generally it's better to fetch these on demand
           as it parallelizes better with other i/o

        // Cache the node blocks
        let buffer = immutable::read(
            repository.clone(),
            Address::zero_context_hash(tree.hash_node),
            None, /* Read the full array of block hashes */
            immutable::read_options_from_repository(&repository).with_cache(),
        )
        .await
        .forward::<StateError>("Failed to deserialize node block list")?;

        let block_hash = buffer.as_type_slice::<Hash>();
        let block_address_count = block_hash.len();

        let address: Vec<Address> = block_hash[..block_address_count]
            .iter()
            .map(|&hash| Address::zero_context_hash(hash))
            .collect();

        let _ = immutable::cache(repository.clone(), address, false).await;
        */

        Ok(())
    }

    pub async fn revision_metadata(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<RevisionMetadata, StateError> {
        let metadata = Metadata::deserialize(repository, self.metadata_hash())
            .await
            .forward::<StateError>("Failed to deserialize metadata")?;

        Ok(RevisionMetadata::from_metadata(metadata))
    }

    pub fn link_merge_hash(&self) -> Hash {
        self.data_read().hash_link_merge
    }

    /// Points the state at the link list `hash`, so that a test can name one the store doesn't
    /// hold.
    #[cfg(feature = "test-util")]
    pub fn set_link_list_hash(&self, hash: Hash) {
        self.data_write().hash_link = hash;
    }

    pub fn set_link_merge_hash(&self, hash: Hash) {
        let mut data = self.data_write();
        data.hash_link_merge = hash;
        data.flags |= StateFlags::Dirty;
    }

    pub fn clear_link_merge_state(&self) {
        let mut data = self.data_write();
        if !data.hash_link_merge.is_zero() {
            data.hash_link_merge = Hash::default();
            data.flags |= StateFlags::Dirty;
        }
    }

    pub async fn serialize_link_merge_state(
        &self,
        repository: Arc<RepositoryContext>,
        entries: &[LinkMergeEntry],
    ) -> Result<Hash, StateError> {
        let header = LinkMergeState {
            count: entries.len() as u32,
            flags: 0,
        };

        let mut bytes =
            Vec::with_capacity(size_of::<LinkMergeState>() + std::mem::size_of_val(entries));
        bytes.extend_from_slice(header.as_bytes());
        for entry in entries {
            bytes.extend_from_slice(entry.as_bytes());
        }

        let address = immutable::write(
            repository.clone(),
            Context::default(),
            Bytes::from(bytes),
            immutable::write_options_from_repository(repository)
                .with_local_cache_priority()
                .with_max_size_chunk(),
        )
        .await
        .forward::<StateError>("Failed to serialize link merge state")?;

        self.set_link_merge_hash(address.hash);
        Ok(address.hash)
    }

    pub async fn deserialize_link_merge_state(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<Vec<LinkMergeEntry>, StateError> {
        let hash = self.link_merge_hash();
        if hash.is_zero() {
            return Ok(vec![]);
        }

        let options = read_options_from_repository(&repository);
        let data = immutable::read(
            repository.clone(),
            Address::zero_context_hash(hash),
            None,
            options,
        )
        .await
        .forward::<StateError>("Failed to read link merge state")?;

        let raw = data.as_ref();
        let header_size = std::mem::size_of::<LinkMergeState>();
        if raw.len() < header_size {
            return Ok(vec![]);
        }

        let Ok(header) = LinkMergeState::read_from_bytes(&raw[..header_size]) else {
            return Ok(vec![]);
        };

        let mut entries = Vec::with_capacity(header.count as usize);
        let entry_bytes = &raw[header_size..];
        for chunk in entry_bytes
            .as_chunks::<{ size_of::<LinkMergeEntry>() }>()
            .0
            .iter()
            .take(header.count as usize)
        {
            let Ok(entry) = LinkMergeEntry::read_from_bytes(chunk) else {
                break;
            };
            entries.push(entry);
        }

        Ok(entries)
    }

    /// The link registry as the state was last serialized with, ignoring the runtime copy.
    async fn serialized_link_list(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<Vec<LinkReference>, StateError> {
        let list_hash = { self.data_read().hash_link };
        if list_hash.is_zero() {
            return Ok(vec![]);
        }

        let data = immutable::read(
            repository.clone(),
            Address::zero_context_hash(list_hash),
            None,
            immutable::read_options_from_repository(&repository)
                .with_cache()
                .with_priority(),
        )
        .await
        .forward::<StateError>("Failed to read state data")?
        .to_aligned::<LinkReference>();

        Ok(data.as_type_slice::<LinkReference>().to_vec())
    }

    /// The link registry: the runtime copy once anything has edited it, and what the state was
    /// serialized with until then.
    ///
    /// Callers get a copy. The runtime copy stays where it is: it is the working set the mutators
    /// below edit in place, and [`State::serialize`] writes a new link list only while it is
    /// present, so it has to outlive every reader for the edits to reach the serialized state.
    pub async fn link_list(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<Vec<LinkReference>, StateError> {
        {
            let runtime = self.runtime_read();
            if let Some(link_list) = runtime.link_list.as_ref() {
                return Ok(link_list.clone());
            }
        }

        self.serialized_link_list(repository).await
    }

    /// Applies `edit` to the runtime link registry, holding the lock across the whole
    /// read-modify-write.
    ///
    /// A commit edits one link per task against one shared state, so an edit that released the
    /// lock between finding its entry and storing the result would lose whatever another edit
    /// stored in the meantime. The serialized list is loaded before the lock is taken, because
    /// reading it awaits; an edit that then finds the registry already populated discards what it
    /// loaded, since the populated copy is the one carrying edits.
    async fn edit_link_list(
        &self,
        repository: Arc<RepositoryContext>,
        edit: impl FnOnce(&mut Vec<LinkReference>) -> Result<(), StateError>,
    ) -> Result<(), StateError> {
        let populated = self.runtime_read().link_list.is_some();
        let loaded = if populated {
            vec![]
        } else {
            self.serialized_link_list(repository).await?
        };

        let mut runtime = self.runtime_write();
        edit(runtime.link_list.get_or_insert(loaded))
    }

    pub async fn link_find(
        &self,
        repository: Arc<RepositoryContext>,
        link_id: RepositoryId,
        local_node: NodeID,
    ) -> Result<LinkReference, StateError> {
        let link_list = {
            let runtime = self.runtime_read();
            runtime.link_list.clone()
        };

        let link_list = if let Some(link_list) = link_list {
            link_list
        } else {
            let list_hash = { self.data_read().hash_link };
            if !list_hash.is_zero() {
                let data = immutable::read(
                    repository.clone(),
                    Address::zero_context_hash(list_hash),
                    None,
                    immutable::read_options_from_repository(&repository)
                        .with_cache()
                        .with_priority(),
                )
                .await
                .forward::<StateError>("Failed to read state data")?
                .to_aligned::<LinkReference>();
                data.as_type_slice::<LinkReference>().to_vec()
            } else {
                vec![]
            }
        };

        for link in link_list.iter() {
            if link.repository == link_id && link.local_node == local_node {
                return Ok(*link);
            }
        }

        Err(LinkNotFound.into())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn link_add(
        &self,
        repository: Arc<RepositoryContext>,
        link_id: RepositoryId,
        branch: BranchId,
        signature: Hash,
        local_node: NodeID,
        link_flags: LinkFlags,
    ) -> Result<(), StateError> {
        self.edit_link_list(repository, |link_list| {
            // Ensure link is not referenced by other revision anywhere
            for link in link_list.iter_mut() {
                if link.repository == link_id && link.signature != signature {
                    // TODO(vri): Link revision divergence
                    return Err(StateError::internal("Link divergence"));
                }
                if link.repository == link_id && link.local_node == local_node {
                    link.signature = signature;
                    return Ok(());
                }
            }

            link_list.push(LinkReference {
                repository: link_id,
                branch,
                signature,
                local_node,
                flags: link_flags.into(),
                ..Default::default()
            });

            Ok(())
        })
        .await
    }

    pub async fn link_update(
        &self,
        repository: Arc<RepositoryContext>,
        link_id: RepositoryId,
        branch: BranchId,
        signature: Hash,
        local_node: NodeID,
    ) -> Result<(), StateError> {
        lore_debug!(
            "Update link with ID {link_id}, local node {local_node}, new signature {signature}, new branch {branch}"
        );

        self.edit_link_list(repository, |link_list| {
            for link in link_list.iter_mut() {
                if link.repository == link_id && link.local_node == local_node {
                    link.branch = branch;
                    link.signature = signature;
                    return Ok(());
                }
            }

            Err(LinkNotFound.into())
        })
        .await
    }

    pub async fn link_remove(
        &self,
        repository: Arc<RepositoryContext>,
        link_id: RepositoryId,
        local_node: NodeID,
    ) -> Result<(), StateError> {
        lore_debug!("Remove link with ID {link_id}, local node {local_node}");

        self.edit_link_list(repository, |link_list| {
            if let Some(index) = link_list
                .iter()
                .position(|link| link.repository == link_id && link.local_node == local_node)
            {
                link_list.remove(index);
                return Ok(());
            }

            Err(LinkNotFound.into())
        })
        .await
    }

    /// [`Self::link_remove`] for a link the registry may not hold.
    pub(crate) async fn link_remove_if_present(
        &self,
        repository: Arc<RepositoryContext>,
        link_id: RepositoryId,
        local_node: NodeID,
    ) -> Result<(), StateError> {
        match self.link_remove(repository, link_id, local_node).await {
            Err(err) if err.is_link_not_found() => Ok(()),
            result => result,
        }
    }

    pub fn force_rehash_names(&self) {
        self.runtime_write().rehash_node_names = true;
    }

    /// The deprecated name table, loaded unless it is resident.
    ///
    /// Only the load is boxed, as in [`Self::block`]. Inline, it would make every node block load
    /// larger, though only a version 0 block with external names reads the name table.
    pub async fn nametable(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<Arc<NameTable>, StateError> {
        {
            let runtime = self.runtime_read();
            if let Some(name_table) = runtime.name_table_deprecated.as_ref() {
                return Ok(name_table.clone());
            }
        }

        Box::pin(self.nametable_load(repository)).await
    }

    /// Loads the deprecated name table for [`Self::nametable`] when it is not resident.
    #[lore_macro::test_pub]
    async fn nametable_load(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<Arc<NameTable>, StateError> {
        let _permit = self
            .deserialize
            .acquire()
            .await
            .internal("Failed to deserialize name table")?;

        let hash_nametable = self
            .tree(repository.clone())
            .await?
            .hash_nametable_deprecated;

        let name_table = {
            Arc::new(if !hash_nametable.is_zero() {
                NameTable::deserialize(repository, hash_nametable)
                    .await
                    .forward::<StateError>("Failed to deserialize name table")?
            } else {
                NameTable::default()
            })
        };

        {
            let mut runtime = self.runtime_write();
            if let Some(prev_name_table) = runtime.name_table_deprecated.as_ref() {
                return Ok(prev_name_table.clone());
            }

            runtime.name_table_deprecated = Some(name_table.clone());
        }

        Ok(name_table)
    }
}

/// Rebase the staged anchor onto a new current revision.
///
/// Callers (sync, branch switch, and similar operations that advance the
/// current revision pointer) invoke this after `store_current_anchor` to
/// keep the staged anchor consistent with the new current. The staged
/// anchor's contract is "current plus uncommitted modifications"; once
/// current moves, the anchor must either point at the new current (no
/// uncommitted work) or carry forward the uncommitted dirty paths.
///
/// Behavior:
/// - No staged anchor on disk: nothing to do.
/// - Anchor already equals `new_current_signature`: nothing to do.
/// - Anchor's tree has no dirty descendants: drop the anchor so the next
///   load falls back to the new current.
/// - Anchor's tree has dirty descendants: drop the anchor, then re-apply
///   each dirty path against the new current via [`crate::file::dirty::dirty`],
///   comparing files where `realized`. Only dirty nodes carry over; the prior
///   staged merkle tree is discarded.
///
/// Wraps [`rebase_staged_state_in`] with the instance anchor I/O.
pub async fn rebase_staged_anchor(
    repository: Arc<RepositoryContext>,
    operation: &Arc<InstanceOperationImpl>,
    new_current_signature: Hash,
    force: bool,
    realized: bool,
) -> Result<(), StateError> {
    let Some(old_staged_signature) = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
    else {
        return Ok(());
    };

    if old_staged_signature == new_current_signature {
        return Ok(());
    }

    let _ = crate::instance::delete_staged_anchor(&repository).await;

    let Some(rebased_signature) = rebase_staged_state_in(
        operation,
        repository.clone(),
        old_staged_signature,
        new_current_signature,
        force,
        realized,
        None,
    )
    .await?
    else {
        return Ok(());
    };

    crate::instance::store_staged_anchor(&repository, rebased_signature)
        .await
        .forward::<StateError>("Failed to serialize staged anchor")?;

    Ok(())
}

/// Where a layer's tree stands in the working tree: its names below `source` are held below
/// `mount`.
pub(crate) struct LayerPaths<'a> {
    pub source: &'a RelativePath,
    pub mount: &'a RelativePath,
}

/// Rebase a staged state onto a new current revision, touching no anchors, reading the working
/// tree through a filesystem operation of its own.
///
/// Returns the signature of the rebased state, leaving persistence to the
/// caller, or `None` when nothing needs staging on top of the new current.
///
/// `force` carries forward dirty paths `repository`'s filter excludes, for a
/// caller whose filter is the one those paths were recorded under. An operation
/// that changes the view is not such a caller: its filter is the one the working
/// tree is left materialized under, so a path it excludes names a file the tree
/// no longer holds. A status asks the same filter and so reports nothing of a
/// flag carried there, until the view widens again and it surfaces as a local
/// change to a file nothing touched.
#[cfg(feature = "test-util")]
pub async fn rebase_staged_state(
    repository: Arc<RepositoryContext>,
    old_staged_signature: Hash,
    new_current_signature: Hash,
    force: bool,
) -> Result<Option<Hash>, StateError> {
    crate::fs::filesystem_provider::with_operation(repository.file_system(), async |operation| {
        rebase_staged_state_in(
            &operation,
            repository.clone(),
            old_staged_signature,
            new_current_signature,
            force,
            true,
            None,
        )
        .await
    })
    .await
}

/// [`rebase_staged_state`] reading the working tree through `operation`, for the tree of the
/// layer `layer` places, or the repository's own where `None`.
///
/// `realized` states that the working tree holds the new current revision and the modified times
/// recorded on the way are stored. A dirty path the working tree then agrees with is dropped:
/// its marker was left by a change the sync or switch overwrote.
pub(crate) async fn rebase_staged_state_in(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    old_staged_signature: Hash,
    new_current_signature: Hash,
    force: bool,
    realized: bool,
    layer: Option<LayerPaths<'_>>,
) -> Result<Option<Hash>, StateError> {
    let old_staged_state = State::deserialize(repository.clone(), old_staged_signature).await?;
    let has_dirty = old_staged_state
        .node_has_dirty_children(repository.clone(), crate::node::ROOT_NODE)
        .await?;

    if !has_dirty {
        return Ok(None);
    }

    let mut dirty_paths: Vec<RelativePath> = Vec::new();
    collect_dirty_paths_inner(
        old_staged_state,
        repository.clone(),
        crate::node::ROOT_NODE,
        &mut RelativePathBuf::new(),
        &mut dirty_paths,
        DirtyWalkOptions {
            skip_staged: false,
            force,
        },
    )
    .await?;

    if dirty_paths.is_empty() {
        return Ok(None);
    }

    let root = RelativePath::default();
    let (source, paths, mask) = match layer {
        None => {
            let paths = dirty_paths
                .into_iter()
                .map(|path| (path.clone(), path))
                .collect::<Vec<_>>();
            let layers = crate::layer::list(repository.clone())
                .await
                .forward::<StateError>("Failed to list layers")?;
            let mask = (!layers.is_empty()).then(|| Arc::new(crate::layer::mount_paths(&layers)));
            (&root, paths, mask)
        }
        // A path outside the source is one the layer does not draw into the working tree.
        Some(layer) => {
            let paths = dirty_paths
                .iter()
                .filter_map(|path| {
                    let below = path.below_ignore_case(layer.source)?;
                    Some((
                        RelativePath::new_from_clean_parts(below, ""),
                        layer.mount.join(below),
                    ))
                })
                .collect::<Vec<_>>();
            (layer.source, paths, None)
        }
    };

    let state_current = State::deserialize(repository.clone(), new_current_signature).await?;
    let signature = crate::file::dirty::dirty_drawn_paths_in(
        operation,
        repository,
        state_current.clone(),
        state_current,
        source,
        &paths,
        mask,
        realized,
    )
    .await
    .forward::<StateError>("Failed to apply dirty paths during staged rebase")?;

    Ok((signature != new_current_signature).then_some(signature))
}

/// Walk a staged state and collect paths of nodes carrying an explicit dirty
/// action (`DirtyAdd`/`DirtyModify`/`DirtyDelete`/`DirtyMove`/`DirtyCopy`).
///
/// Propagated-only `Dirty` parents are walked but not recorded — only the
/// leaves with concrete actions need re-application. A dirty link is followed
/// into the linked state it pins, recording its paths below the mount. Descent
/// stops at `DirtyDelete`/`DirtyMove` subtrees the walk records (the parent
/// action carries the whole subtree when re-applied).
pub(crate) async fn collect_dirty_paths(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node: NodeID,
    mut parent_path: RelativePathBuf,
    paths: &mut Vec<RelativePath>,
) -> Result<(), StateError> {
    collect_dirty_paths_inner(
        state,
        repository,
        parent_node,
        &mut parent_path,
        paths,
        DirtyWalkOptions::from_context(false),
    )
    .await
}

/// Like [`collect_dirty_paths`] but skips nodes that are also staged.
///
/// Used by the commit pipeline to capture only paths that should be
/// re-applied as a new staged anchor on top of the freshly committed
/// revision — staged paths are already part of the new commit and would be
/// incorrectly re-marked as `DirtyModify` by `file::dirty::dirty()` if
/// included. A staged `DirtyDelete`/`DirtyMove` directory records no path, so
/// the walk descends it, a staged subtree whole, for the unstaged nodes below.
#[lore_macro::test_pub]
pub(crate) async fn collect_dirty_only_paths(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node: NodeID,
    mut parent_path: RelativePathBuf,
    paths: &mut impl DirtyPathSink,
) -> Result<(), StateError> {
    collect_dirty_paths_inner(
        state,
        repository,
        parent_node,
        &mut parent_path,
        paths,
        DirtyWalkOptions::from_context(true),
    )
    .await
}

/// What [`State::node_return_unstaged_move`] did with a node marked dirty as moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Return {
    /// Nothing: it stands where the base revision holds it, or that revision holds no node of
    /// its kind and context at its ID.
    Stays,
    /// It went back.
    Back,
    /// It waits under its committed parent, by a name of its own, for the name there to come free.
    Parked,
    /// It cannot go back: a staged node holds its name, or its committed parent is gone.
    Displaced,
    /// It is no longer in the tree.
    Gone,
}

/// The name a node waits under while another node holds its own, which its ID spells, so no other
/// waiting node takes it. A node the tree holds by that name already keeps it beside the waiting
/// one, which no lookup of another name meets; a move whose committed name it is waits while this
/// one does. Spelled in place, as it is only compared and stored.
struct ParkedName {
    spelled: [u8; PARKED_NAME_CAPACITY],
    len: u8,
}

/// Whether `name` is the name `node` waits under ([`ParkedName`]).
pub(crate) fn names_parked(node: NodeID, name: &str) -> bool {
    ParkedName::of(node).as_str() == name
}

/// The prefix of a [`ParkedName`], which the node's ID follows.
const PARKED_NAME_PREFIX: &str = ".lore-returning-";

/// The bytes a [`ParkedName`] holds at most: its prefix and the digits of the largest ID.
const PARKED_NAME_CAPACITY: usize = PARKED_NAME_PREFIX.len() + NodeID::MAX.ilog10() as usize + 1;

impl ParkedName {
    fn of(node: NodeID) -> Self {
        let mut spelled = [0; PARKED_NAME_CAPACITY];
        let free = {
            let mut free = &mut spelled[..];
            let fits = write!(free, "{PARKED_NAME_PREFIX}{node}").is_ok();
            debug_assert!(fits, "the name fits its capacity");
            free.len()
        };
        Self {
            spelled,
            len: (PARKED_NAME_CAPACITY - free) as u8,
        }
    }

    fn as_str(&self) -> &str {
        str::from_utf8(&self.spelled[..usize::from(self.len)]).unwrap_or_default()
    }
}

/// What a dirty walk does with the nodes it meets.
///
/// A struct rather than two parameters, so a caller cannot transpose them.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Default)]
struct DirtyWalkOptions {
    /// Record no path for a node that is also staged.
    skip_staged: bool,
    /// Record paths the view/ignore filter excludes.
    force: bool,
}

impl DirtyWalkOptions {
    /// Options for a walk driven from the command line, taking `force` from the
    /// execution context.
    ///
    /// Read where the walk is started rather than inside it, so the context is
    /// read once per walk. Panics if no execution context is set, which makes
    /// the caller's task, not the future's poller, the one that has to have one.
    fn from_context(skip_staged: bool) -> Self {
        Self {
            skip_staged,
            force: execution_context().globals().force(),
        }
    }
}

/// Whether a dirty node carries an action of its own to record.
///
/// `skip_staged` drops nodes that are also staged: their action belongs to the
/// commit being written, and re-applying it would mark the committed content
/// dirty again.
///
/// A new directory only marked and holding nodes is carried by them; see
/// [`marked_directory_carried_by_children`].
fn dirty_path_contributes(child: &Node, skip_staged: bool) -> bool {
    child.action_bits() != 0
        && !(skip_staged && child.is_staged())
        && !marked_directory_carried_by_children(child)
}

/// Whether `node` is a new directory only marked dirty, not staged, whose children carry it.
/// Recorded itself, it would be staged or re-marked whole, taking in everything the working tree
/// holds below it, marked or not. A staged one is still recorded: its path is what stages the
/// removal of what staging put below it.
fn marked_directory_carried_by_children(node: &Node) -> bool {
    node.is_dirty_add() && !node.is_staged() && node.is_directory() && node.child().is_some()
}

/// Whether a dirty node's children must be walked, given whether it records a path of its own
/// (`contributes`).
///
/// A `DirtyDelete` or `DirtyMove` directory recording its path carries its whole subtree when
/// it is re-applied, so its descendants are covered by its own path, a moved one's unless the
/// walk goes `through_moves`. A directory that records no path of its own is still walked, a
/// moved or deleted one too: staging a file stages the directories above it, so a staged
/// directory is where a dirty-only file lives.
fn dirty_path_descends(child: &Node, contributes: bool, through_moves: bool) -> bool {
    child.is_directory()
        && (!contributes || !child.is_dirty_delete() && (through_moves || !child.is_dirty_move()))
}

/// Where a dirty walk records the nodes it finds carrying an action of their own.
#[lore_macro::test_pub]
pub(crate) trait DirtyPathSink: Send {
    /// Whether the walk records the nodes below a directory marked dirty as moved that it records,
    /// rather than leave the directory's path to stand for them ([`dirty_path_descends`]).
    const THROUGH_MOVES: bool = false;

    /// Records `node`, `id`, found at `path`, in a tree the walk reached through a link where
    /// `linked` says so.
    fn record(&mut self, path: &RelativePathBuf, id: NodeID, node: &Node, linked: bool);
}

impl DirtyPathSink for Vec<RelativePath> {
    fn record(&mut self, path: &RelativePathBuf, _: NodeID, _: &Node, _: bool) {
        self.push(path.clone().freeze());
    }
}

/// The node block a sibling chain is being read from.
///
/// A chain is walked one node at a time, and [`State::node`] resolves each from
/// its block: a lock on the block table, a bounds check and a weak upgrade, for
/// what is usually the block just read. Nodes are allocated as a tree is built,
/// so siblings and their children land together and a chain rarely leaves one
/// block. Holding it makes the common step an index comparison.
///
/// A held block cannot be evicted, so [`State::block`] keeps returning the one
/// held here and a node read through the cursor is the node [`State::node`]
/// would return.
struct BlockCursor {
    index: usize,
    block: Arc<NodeBlock>,
}

impl BlockCursor {
    /// Opens a cursor on the block holding `node`.
    async fn open(
        state: &State,
        repository: &Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<Self, StateError> {
        if !node.is_valid_or_root_node_id() {
            return Err(StateError::internal("Invalid node"));
        }
        let index = NodeBlock::index(node);
        Ok(Self {
            index,
            block: state.block(repository.clone(), index).await?,
        })
    }

    /// Reads `node`, fetching the block holding it unless that is the one held.
    ///
    /// Selecting the block and reading from it are one step, so a node is only
    /// ever read out of its own block. Split apart they would let a read take a
    /// node index against whichever block the cursor happened to hold, which
    /// names an unrelated node rather than failing.
    async fn node(
        &mut self,
        state: &State,
        repository: &Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<Node, StateError> {
        if !node.is_valid_or_root_node_id() {
            return Err(StateError::internal("Invalid node"));
        }
        let index = NodeBlock::index(node);
        if index != self.index {
            self.block = state.block(repository.clone(), index).await?;
            self.index = index;
        }
        Ok(*self.block.read().node(Node::index(node)))
    }
}

/// Appends the name of `child_id` to `path`, reporting whether anything was added, or
/// `None` for a node whose name the read path refuses, which appends nothing and is
/// the caller's to pass over: a dirty flag can predate the rule that refuses the name.
///
/// The name is a read lock on the block holding it, released as this returns and
/// so before the caller descends. A walk that took a second shared lock on that
/// block would deadlock behind a queued writer.
///
/// An empty name appends nothing, which [`RelativePathBuf::push`] already does and
/// which the caller must not undo: a pop would take the parent's own last
/// component off instead.
async fn push_dirty_child_name(
    state: &State,
    repository: Arc<RepositoryContext>,
    path: &mut RelativePathBuf,
    child_id: NodeID,
) -> Result<Option<bool>, StateError> {
    let Some(child_name) = state.node_name_ref_or_skip(repository, child_id).await? else {
        return Ok(None);
    };
    let named = !child_name.is_empty();
    path.push(child_name);
    Ok(Some(named))
}

/// One directory on the way down, and where the walk left off in its children.
///
/// `appended` says whether this level's own name is on the path buffer, so the
/// buffer is restored to its parent's path when the level is done. A level opened
/// for a node with an empty name appended nothing and must take nothing off.
///
/// Each level carries its own [`BlockCursor`], because a level resumes its chain
/// after its children are walked and the levels below will have moved their own
/// cursors elsewhere.
struct DirtyWalkLevel {
    node: NodeID,
    next_child: Option<NodeID>,
    cycle: SiblingCycleGuard,
    cursor: BlockCursor,
    appended: bool,
    /// The filter's verdict for the directory this level walks, which each of
    /// its children steps from rather than folding its whole path.
    states: FilterStates,
}

/// Tree depth a dirty walk's stack is sized for. A deeper tree grows it.
const DIRTY_WALK_LEVELS: usize = 32;

/// The level walking `node_id`'s children, or nothing where there are none.
///
/// A file holds no children, and a link's children live in another repository's
/// state, so neither is descended. [`Node::child`] means nothing on either.
async fn dirty_walk_level(
    state: &State,
    repository: &Arc<RepositoryContext>,
    node_id: NodeID,
    appended: bool,
    states: FilterStates,
) -> Result<Option<DirtyWalkLevel>, StateError> {
    let mut cursor = BlockCursor::open(state, repository, node_id).await?;
    let node = cursor.node(state, repository, node_id).await?;
    if node.is_link() || !node.is_directory() {
        return Ok(None);
    }
    Ok(Some(DirtyWalkLevel {
        node: node_id,
        next_child: node.child(),
        cycle: SiblingCycleGuard::new(node_id),
        cursor,
        appended,
        states,
    }))
}

/// Walks the children of `parent_node`, recording dirty paths under `parent_path`.
///
/// A child is named only once [`dirty_path_contributes`] or [`dirty_path_descends`]
/// says it is wanted. The ignore filter matches every one of its lines against a
/// name, and under `skip_staged` most of a tree is wanted for neither: a commit
/// stages what it writes.
///
/// `parent_node` is walked only if it is a directory, as [`Node::child`] means
/// nothing on a file. A dirty link is walked in the linked state it pins, up to
/// [`MAX_LINK_DEPTH`] links deep, its paths named below the mount.
///
/// `parent_path` is the path of `parent_node` and the buffer every descendant is
/// named into, so naming costs no allocation and a path is allocated only where one
/// is recorded. A walk that records nothing allocates nothing. A name is taken off
/// where the child that appended it is finished with: at the end of the loop body
/// for a child that is only recorded, and when its level is exhausted for one that
/// is descended. An error abandons the walk and the buffer with it, so it needs no
/// unwinding.
///
/// Descent is an explicit stack of [`DirtyWalkLevel`], so the walk runs at a fixed
/// call depth however deep the tree is. Depth-first order in the sibling chain is
/// preserved by resuming a level where it left off rather than queueing subtrees: a
/// directory's own path is recorded before its level is pushed, and its next
/// sibling is visited once that level is exhausted.
#[lore_macro::test_pub]
async fn collect_dirty_paths_inner(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node: NodeID,
    parent_path: &mut RelativePathBuf,
    paths: &mut impl DirtyPathSink,
    options: DirtyWalkOptions,
) -> Result<(), StateError> {
    // The parent is folded once; every level below carries the verdict its own
    // children step from.
    let parent_states = repository.filter.exclusion_states(&*parent_path);
    collect_dirty_paths_linked(
        state,
        repository,
        parent_node,
        parent_path,
        parent_states,
        paths,
        options,
        0,
    )
    .await
}

/// [`collect_dirty_paths_inner`] in a state reached through `link_depth` links,
/// from the filter's verdict for `parent_path`.
#[allow(clippy::too_many_arguments)]
async fn collect_dirty_paths_linked<S: DirtyPathSink>(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node: NodeID,
    parent_path: &mut RelativePathBuf,
    parent_states: FilterStates,
    paths: &mut S,
    options: DirtyWalkOptions,
    link_depth: usize,
) -> Result<(), StateError> {
    let mut levels: Vec<DirtyWalkLevel> = Vec::with_capacity(DIRTY_WALK_LEVELS);
    levels.extend(dirty_walk_level(&state, &repository, parent_node, false, parent_states).await?);

    while let Some(level) = levels.last_mut() {
        let Some(child_id) = level.next_child else {
            if levels.pop().is_some_and(|done| done.appended) {
                parent_path.pop();
            }
            continue;
        };

        let level_states = level.states;
        let child = level.cursor.node(&state, &repository, child_id).await?;
        child.walk_step(child_id, level.node, &mut level.cycle)?;
        level.next_child = child.sibling();

        if !child.is_dirty() {
            continue;
        }

        let contributes = dirty_path_contributes(&child, options.skip_staged);
        let descends = dirty_path_descends(&child, contributes, S::THROUGH_MOVES);
        let crosses = child.is_link() && link_depth < MAX_LINK_DEPTH;
        if !contributes && !descends && !crosses {
            continue;
        }

        let Some(appended) =
            push_dirty_child_name(&state, repository.clone(), parent_path, child_id).await?
        else {
            continue;
        };

        // Don't carry forward dirty paths that the view/ignore filter
        // excludes — they cannot be re-applied against a checkout that
        // never materializes them. --force bypasses the filter. The ignore
        // slot has no say over a node the tree tracks, only over one it only
        // marks, as in `State::collect_dirty_paths`.
        let mode = if options.force {
            FilterMode::TRACKED
        } else {
            repository
                .filter
                .tracked_mode(
                    level_states,
                    &*parent_path,
                    !child.is_file(),
                    FilterMode::TRACKED,
                    async || {
                        state
                            .tracks(
                                &repository,
                                child_id,
                                NodeFlags::from_bits_retain(child.flags),
                            )
                            .await
                    },
                )
                .await?
        };
        let (child_states, excluded) = repository.filter.child_excludes_tree_unless_forced(
            options.force,
            level_states,
            &*parent_path,
            !child.is_file(),
            mode,
        );

        let mut descended = false;
        if !excluded {
            if contributes {
                paths.record(parent_path, child_id, &child, link_depth > 0);
            }

            if descends {
                let opened =
                    dirty_walk_level(&state, &repository, child_id, appended, child_states).await?;
                descended = opened.is_some();
                levels.extend(opened);
            } else if crosses {
                let pin = child.linked_node();
                let (linked_repository, linked_state) = pin.open(&repository).await?;
                Box::pin(collect_dirty_paths_linked(
                    linked_state,
                    linked_repository,
                    pin.node,
                    parent_path,
                    child_states,
                    paths,
                    options,
                    link_depth + 1,
                ))
                .await?;
            }
        }

        if appended && !descended {
            parent_path.pop();
        }
    }

    Ok(())
}

pub struct TreePath {
    pub path: RelativePath,
    pub address: Option<Address>,
    pub flags: NodeFlags,
    pub size: u64,
    pub mode: u64,
    /// True when a link node tracks its parent's branch; false for pinned
    /// links and all non-link nodes.
    pub tracking: bool,
    /// The revision that last changed this entry, as an index from 1 into
    /// [`TreeWalk::revisions`]; 0 when the walk does not attribute, the entry has no history, or a
    /// read for its attribution failed.
    pub last_revision: u32,
}

/// A revision a walk attributes entries to, and the context of the repository it is in: for an
/// entry under a link, the context the walk read the linked repository with.
#[derive(Clone)]
pub struct AttributedRevision {
    pub repository: Arc<RepositoryContext>,
    pub revision: Hash,
}

/// Attribution reads that failed, each leaving entries unattributed, and the first of them.
#[derive(Default)]
pub struct AttributionFailures {
    pub count: u32,
    pub error: Option<StateError>,
}

impl AttributionFailures {
    /// Counts a read that failed, keeping the first error.
    pub fn failed(&mut self, err: StateError) {
        self.count = self.count.saturating_add(1);
        if self.error.is_none() {
            self.error = Some(err);
        }
    }

    /// Counts the attribution read that failed with `err`, unless it was refused as `SlowDown`,
    /// which it returns, to fail the walk.
    fn failed_unless_refused(&mut self, err: StateError) -> Result<(), StateError> {
        if err.is_slow_down() {
            return Err(err);
        }
        self.failed(err);
        Ok(())
    }
}

/// Where the entries a walk lists from one repository begin: from entry `first` of
/// [`TreeWalk::paths`] on, up to the next span, the entries are in `repository`.
pub struct RepositorySpan {
    pub first: usize,
    pub repository: RepositoryId,
}

/// The entries of a tree walk, the repositories they are in, and with attribution the revisions
/// they name and what attribution could not read.
#[derive(Default)]
pub struct TreeWalk {
    pub paths: Vec<TreePath>,
    /// Where the walk enters and leaves each linked repository, in entry order. The entries before
    /// the first span are in the repository the walk was asked for.
    pub repository_spans: Vec<RepositorySpan>,
    /// The revisions entries were attributed to, each once. An entry the walked revision changed, or
    /// a failed read, can leave one named by no entry.
    pub revisions: Vec<AttributedRevision>,
    pub attribution_failures: AttributionFailures,
    /// The index of each revision in `revisions`, by repository and revision, while the walk runs.
    indices: HashMap<(RepositoryId, Hash), u32>,
}

impl TreeWalk {
    /// Notes that the entries listed from here on are in `repository`.
    fn enter_repository(&mut self, repository: RepositoryId) {
        self.repository_spans.push(RepositorySpan {
            first: self.paths.len(),
            repository,
        });
    }

    /// The index of `revision` of `repository` in `revisions`, from 1, added on first sight. 0,
    /// leaving the entry unattributed, once the indices reach `u32::MAX`.
    fn intern(&mut self, repository: &Arc<RepositoryContext>, revision: Hash) -> u32 {
        let key = (repository.id, revision);
        if let Some(&index) = self.indices.get(&key) {
            return index;
        }
        let Some(index) = u32::try_from(self.revisions.len() + 1)
            .ok()
            .filter(|index| *index < u32::MAX)
        else {
            return 0;
        };
        self.revisions.push(AttributedRevision {
            repository: repository.clone(),
            revision,
        });
        self.indices.insert(key, index);
        index
    }
}

/// Attributes the entries a walk lists from one state against that state. A linked subtree is
/// attributed by one of its own.
struct TreeAttribution {
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    /// The file-metadata block read last, and its index.
    block: Option<(usize, Arc<NodeFileMetadataBlock>)>,
    /// The file-metadata blocks that failed to read, a bit per block index.
    failed_blocks: Vec<u64>,
    /// Each entry listed from the state: its index in [`TreeWalk::paths`], and its node.
    listed: Vec<(u32, NodeID)>,
}

impl TreeAttribution {
    fn new(state: Arc<State>, repository: Arc<RepositoryContext>) -> Self {
        Self {
            state,
            repository,
            block: None,
            failed_blocks: Vec::new(),
            listed: Vec::new(),
        }
    }

    /// Attributes `node`, listed as entry `entry` of `walk`, to the revision its file history
    /// records as its last change; [`Self::finish`] attributes the entries the walked revision
    /// changed to it. A read that fails leaves the entry unattributed, as does an index past
    /// `u32::MAX`. A read refused as `SlowDown` fails the walk.
    async fn attribute(
        &mut self,
        node: NodeID,
        entry: usize,
        walk: &mut TreeWalk,
    ) -> Result<u32, StateError> {
        let Ok(entry) = u32::try_from(entry) else {
            return Ok(0);
        };
        self.listed.push((entry, node));
        match self.recorded_revision(node).await {
            Ok(revision) if revision.is_zero() => Ok(0),
            Ok(revision) => Ok(walk.intern(&self.repository, revision)),
            Err(err) => walk
                .attribution_failures
                .failed_unless_refused(err)
                .map(|()| 0),
        }
    }

    /// The revision `node`'s file history records as its last change, as the first entry `lore
    /// file history` reports after the walked revision: slot 0, or slot 1 where slot 0 is zero.
    /// Zero for a node with no history.
    ///
    /// Reads the metadata with [`State::try_block_file_metadata_existing`], which answers `None`
    /// for a block the tree does not store, and keeps each block it reads from the store resident
    /// in the State, up to `MAX_CHECKED_FILE_METADATA_BLOCKS` of them, about 4 MiB: the bound of
    /// what attribution holds per walked State. A block that failed to read is noted, so the other
    /// entries in it answer zero without a read.
    async fn recorded_revision(&mut self, node: NodeID) -> Result<Hash, StateError> {
        let metadata_node = node_to_file_metadata(node);
        let block_index = NodeFileMetadataBlock::index(metadata_node);
        let block = match &self.block {
            Some((read, block)) if *read == block_index => block,
            _ => {
                if block_bit(&self.failed_blocks, block_index) {
                    return Ok(Hash::default());
                }
                match self
                    .state
                    .try_block_file_metadata_existing(self.repository.clone(), block_index)
                    .await
                {
                    Ok(None) => return Ok(Hash::default()),
                    Ok(Some(block)) => &self.block.insert((block_index, block)).1,
                    Err(err) => {
                        set_block_bit(&mut self.failed_blocks, block_index);
                        return Err(err);
                    }
                }
            }
        };
        let reader = block.read();
        let record = reader.node(NodeFileMetadata::index(metadata_node));
        Ok(record.revision[record.history_slot()])
    }

    /// Attributes each listed entry the walked revision changed to it, merges included, so a merge
    /// stands for what it merged in. Matches the walked revision's delta against a set built from
    /// the smaller of the two: the listed entries, or the delta. A delta that fails to read leaves
    /// every listed entry unattributed, as which of them it changed is unknown. A read refused as
    /// `SlowDown` fails the walk.
    async fn finish(self, walk: &mut TreeWalk) -> Result<(), StateError> {
        if self.listed.is_empty() {
            return Ok(());
        }
        let changes = match self.state.tree(self.repository.clone()).await {
            Ok(tree) => tree.delta_count as usize,
            Err(err) => {
                self.clear(walk);
                return walk.attribution_failures.failed_unless_refused(err);
            }
        };
        if changes == 0 {
            return Ok(());
        }
        let delta = match self.state.delta_block(self.repository.clone()).await {
            Ok(delta) => delta.to_aligned::<NodeDelta>(),
            Err(err) => {
                self.clear(walk);
                return walk.attribution_failures.failed_unless_refused(err);
            }
        };
        let deltas = delta.as_type_slice::<NodeDelta>();
        let walked = self.state.revision();
        let mut index = 0;
        let mut attribute = |walk: &mut TreeWalk, entry: u32| {
            if index == 0 {
                index = walk.intern(&self.repository, walked);
            }
            if let Some(path) = walk.paths.get_mut(entry as usize) {
                path.last_revision = index;
            }
        };
        if self.listed.len() <= changes {
            let listed: HashMap<NodeID, u32> = self
                .listed
                .iter()
                .map(|&(entry, node)| (node, entry))
                .collect();
            for delta in deltas {
                if let Some(&entry) = listed.get(&delta.node) {
                    attribute(walk, entry);
                }
            }
        } else {
            let changed: HashSet<NodeID> = deltas.iter().map(|delta| delta.node).collect();
            for &(entry, node) in &self.listed {
                if changed.contains(&node) {
                    attribute(walk, entry);
                }
            }
        }
        Ok(())
    }

    /// Leaves every listed entry unattributed.
    fn clear(&self, walk: &mut TreeWalk) {
        for &(entry, _) in &self.listed {
            if let Some(path) = walk.paths.get_mut(entry as usize) {
                path.last_revision = 0;
            }
        }
    }
}

pub type CanReadRepository = Arc<dyn Fn(RepositoryId) -> bool + Send + Sync>;

pub fn allow_all_repositories() -> CanReadRepository {
    Arc::new(|_| true)
}

/// Maximum number of link hops a tree walk follows before giving up. Bounds
/// both unbounded link chains and link cycles so a walk terminates.
pub const MAX_LINK_DEPTH: usize = 8;

/// The entries of the tree of `state` below `path`, `max_depth` levels deep or all of them for 0,
/// descending the links `can_read` admits. With `include_last_revision`, each entry is attributed
/// to the revision that last changed it.
pub async fn gather_tree_paths(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    path: RelativePath,
    max_depth: usize,
    can_read: CanReadRepository,
    include_last_revision: bool,
) -> Result<TreeWalk, StateError> {
    let (walk_state, walk_repository, parent_node_id, starts_linked) = if path.is_empty() {
        (state, repository, ROOT_NODE, false)
    } else {
        let node_link = state
            .find_node_link(repository.clone(), path.as_str())
            .await?;
        if !node_link.is_valid() {
            return Err(NodeNotFound.into());
        }
        if !can_read(node_link.repository) {
            lore_debug!(
                "Path resolution stops at unauthorized repository {}",
                node_link.repository,
            );
            return Err(NodeNotFound.into());
        }
        if node_link.revision == state.revision() {
            (state, repository, node_link.node, false)
        } else {
            let starts_linked = node_link.repository != repository.id;
            let linked_repo = repository.to_link_context(node_link.repository).await;
            let linked_state = State::deserialize(linked_repo.clone(), node_link.revision).await?;
            (linked_state, linked_repo, node_link.node, starts_linked)
        }
    };

    let mut walk = TreeWalk::default();
    if starts_linked {
        walk.enter_repository(walk_repository.id);
    }
    enumerate_children(
        walk_state,
        walk_repository,
        parent_node_id,
        path,
        0,
        max_depth,
        0,
        can_read,
        &mut walk,
        include_last_revision,
    )
    .await?;
    walk.indices = HashMap::new();
    Ok(walk)
}

/// Walks the children of directory `parent_node_id` of `state` into `walk`. With
/// `include_last_revision`, attributes each entry against `state`, as a linked subtree is
/// attributed against its own. A walk that fails part-way leaves the entries it listed from
/// `state` unattributed.
///
/// The attribution state and its pass over the walked revision's delta are a cold path, run once
/// per walked state and only when attribution is asked for, so they are boxed. Inline, they would
/// grow the future of every entry by their own size, attribution or not, since an entry that is a
/// link walks its subtree through this function.
#[lore_macro::test_pub]
#[allow(clippy::too_many_arguments)]
async fn enumerate_children(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node_id: NodeID,
    parent_path: RelativePath,
    depth: usize,
    max_depth: usize,
    link_depth: usize,
    can_read: CanReadRepository,
    walk: &mut TreeWalk,
    include_last_revision: bool,
) -> Result<(), StateError> {
    let block_index = NodeBlock::index(parent_node_id);
    let node_index = Node::index(parent_node_id);
    let block = state.block(repository.clone(), block_index).await?;
    let parent = block.node(node_index);
    if !parent.is_directory() {
        return Err(InvalidPath {
            path: parent_path.to_string(),
        }
        .into());
    }
    let mut attribution = include_last_revision
        .then(|| Box::new(TreeAttribution::new(state.clone(), repository.clone())));
    let mut cycle = SiblingCycleGuard::new(parent_node_id);
    let listed = gather_tree_paths_node_recurse(
        state,
        repository,
        parent.child(),
        parent_node_id,
        parent_path,
        depth,
        max_depth,
        link_depth,
        can_read,
        walk,
        &mut cycle,
        attribution.as_deref_mut(),
    )
    .await;
    let Some(attribution) = attribution else {
        return listed;
    };
    if let Err(err) = listed {
        attribution.clear(walk);
        return Err(err);
    }
    Box::pin((*attribution).finish(walk)).await
}

/// Skips the linked subtree at `revision` of `repository` that failed with `err`: logs the failure,
/// at debug level for what is not found, and counts it in `failures` where given. A read refused
/// as `SlowDown` is returned instead, to fail the walk.
#[lore_macro::test_pub]
fn skip_linked_subtree(
    prefix: &str,
    repository: RepositoryId,
    revision: Hash,
    err: StateError,
    failures: Option<&mut AttributionFailures>,
) -> Result<(), StateError> {
    match err {
        StateErrors::SlowDown(_) => return Err(err),
        StateErrors::NotFound(_)
        | StateErrors::NodeNotFound(_)
        | StateErrors::LinkNotFound(_)
        | StateErrors::RevisionNotFound(_)
        | StateErrors::AddressNotFound(_) => {
            lore_debug!("{prefix} at {repository} @ {revision}: {err}");
        }
        _ => {
            lore_warn!("{prefix} at {repository} @ {revision}: {err}");
        }
    }
    if let Some(failures) = failures {
        failures.failed(err);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn gather_tree_paths_node(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    expected_parent: NodeID,
    parent_path: RelativePath,
    depth: usize,
    max_depth: usize,
    link_depth: usize,
    can_read: CanReadRepository,
    walk: &mut TreeWalk,
    cycle: &mut SiblingCycleGuard,
    mut attribution: Option<&mut TreeAttribution>,
) -> Result<Option<NodeID>, StateError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = state
        .block_with_nametable(repository.clone(), block_index)
        .await?;
    let node = block.node(node_index);
    node.walk_step(node_id, expected_parent, cycle)?;

    let node_path = {
        let Some(node_name) = block
            .node_name_ref_or_skip(node_index, node_id)
            .forward::<StateError>("Node name")?
        else {
            return Ok(node.sibling());
        };
        if parent_path.is_empty() {
            RelativePath::new_from_initial_path(&*node_name).unwrap_or_default()
        } else {
            parent_path.push_into_buf(&node_name).freeze()
        }
    };
    let address = if node.is_directory() {
        None
    } else {
        Some(node.address)
    };
    let flags = if node.is_file() {
        NodeFlags::File
    } else if node.is_link() {
        NodeFlags::Link
    } else {
        NodeFlags::NoFlags
    };
    // An unresolvable link reference falls back to pinned.
    let tracking = if node.is_link() {
        let link = node.linked_node();
        match state
            .link_find(repository.clone(), link.repository, node_id)
            .await
        {
            Ok(link_ref) => link_ref.is_tracking(),
            Err(err) if err.is_slow_down() => return Err(err),
            Err(_) => false,
        }
    } else {
        false
    };
    let last_revision = match attribution.as_deref_mut() {
        Some(attribution) => {
            attribution
                .attribute(node_id, walk.paths.len(), walk)
                .await?
        }
        None => 0,
    };
    walk.paths.push(TreePath {
        path: node_path.clone(),
        address,
        flags,
        size: node.size,
        mode: node.mode as u64,
        tracking,
        last_revision,
    });

    let depth_remaining = max_depth == 0 || depth + 1 < max_depth;
    if node.is_directory() && depth_remaining {
        let mut child_cycle = SiblingCycleGuard::new(node_id);
        gather_tree_paths_node_recurse(
            state.clone(),
            repository.clone(),
            node.child(),
            node_id,
            node_path,
            depth + 1,
            max_depth,
            link_depth,
            can_read,
            walk,
            &mut child_cycle,
            attribution,
        )
        .await?;
    } else if node.is_link() && depth_remaining && link_depth < MAX_LINK_DEPTH {
        let link = node.linked_node();
        if !can_read(link.repository) {
            lore_debug!(
                "Skipping linked subtree: caller not authorized for repository {}",
                link.repository,
            );
        } else {
            let linked_repo = repository.to_link_context(link.repository).await;
            match State::deserialize(linked_repo.clone(), link.revision).await {
                Ok(linked_state) => {
                    walk.enter_repository(link.repository);
                    let listed = enumerate_children(
                        linked_state,
                        linked_repo,
                        link.node,
                        node_path,
                        depth + 1,
                        max_depth,
                        link_depth + 1,
                        can_read,
                        walk,
                        attribution.is_some(),
                    )
                    .await;
                    walk.enter_repository(repository.id);
                    if let Err(err) = listed {
                        skip_linked_subtree(
                            "Aborting linked subtree",
                            link.repository,
                            link.revision,
                            err,
                            attribution
                                .is_some()
                                .then_some(&mut walk.attribution_failures),
                        )?;
                    }
                }
                Err(err) => skip_linked_subtree(
                    "Skipping linked subtree: cannot load state",
                    link.repository,
                    link.revision,
                    err,
                    None,
                )?,
            }
        }
    }

    Ok(node.sibling())
}

#[allow(clippy::too_many_arguments)]
fn gather_tree_paths_node_recurse<'a>(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    first_child: Option<NodeID>,
    expected_parent: NodeID,
    parent_path: RelativePath,
    depth: usize,
    max_depth: usize,
    link_depth: usize,
    can_read: CanReadRepository,
    walk: &'a mut TreeWalk,
    cycle: &'a mut SiblingCycleGuard,
    mut attribution: Option<&'a mut TreeAttribution>,
) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send + 'a>> {
    Box::pin(async move {
        let mut next = first_child;
        while let Some(node_id) = next {
            next = gather_tree_paths_node(
                state.clone(),
                repository.clone(),
                node_id,
                expected_parent,
                parent_path.clone(),
                depth,
                max_depth,
                link_depth,
                can_read.clone(),
                walk,
                cycle,
                attribution.as_deref_mut(),
            )
            .await?;
        }
        Ok(())
    })
}

#[lore_macro::test_pub]
#[repr(C)]
#[derive(Copy, Clone, IntoBytes, FromBytes, Immutable)]
pub struct Tree {
    /// Magic identifier
    pub magic: u32,
    /// Format version
    pub format: u32,
    /// Tree flags
    pub flags: u32,
    /// Node and file metadata block count
    pub block_count: u32,
    /// Delta count
    pub delta_count: u32,
    /// First block with unused node slots
    block_unused_first: u32,
    /// Size of the full tree in bytes
    pub size: u64,
    /// Root hash
    pub hash_root: Hash,
    /// Node blocks fragment
    pub hash_node: Hash,
    /// Nametable fragment
    pub hash_nametable_deprecated: Hash,
    /// File metadata blocks fragment
    pub hash_file_metadata: Hash,
    /// Delta fragment
    pub hash_delta: Hash,
    /// Reserved for future extension
    hash_reserved: [Hash; 3],
}

impl Default for Tree {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

const TREE_MAGIC: u32 = 0x3C71BF05u32;

/// Maximum number of blocks in a tree. Guards against malicious or corrupt
/// tree headers triggering unbounded allocations when blocks are accessed.
pub const MAX_TREE_BLOCK_COUNT: u32 = 1_000_000;

/// Tree format version identifiers
#[repr(u32)]
pub enum TreeFormat {
    /// Initial version
    Initial = 1,
}

fn named_node_sort(node: &mut [StateNamedNode]) {
    node.sort_unstable_by_key(|lhs| lhs.name);
}

/// The flags `node` carries into a change: what it is staged for, and what a merge left on it.
///
/// What a walk measured is the walk's to state, not the node's, and is joined by the caller.
pub fn compute_change_flags(node: &Node) -> change::Flags {
    let mut flags = change::Flags::None;

    if node.is_staged() {
        flags |= change::Flags::Staged;
    }
    if node.is_dirty() {
        flags |= change::Flags::Dirty;
    }
    if node.is_staged_merge() {
        flags |= change::Flags::Merge;
    }
    if node.is_staged_merge_conflict() {
        flags |= change::Flags::Conflict;
    }
    if node.is_staged_merge_resolved() {
        flags |= change::Flags::ConflictResolved;
    }
    if node.is_staged_merge_mine() {
        flags |= change::Flags::ConflictMine;
    }
    if node.is_staged_merge_theirs() {
        flags |= change::Flags::ConflictTheirs;
    }

    flags
}

/// Indicates which node source to use for loading node data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeSource {
    /// Use the 'from' node (typically for Delete actions)
    From,
    /// Use the 'to' node (typically for Add/Keep actions)
    To,
    /// No valid node available (filesystem-only paths)
    Invalid,
}

/// [`add_change`] for an action that emits and stops.
///
/// Only [`FileAction::Delete`] and [`FileAction::Add`] walk the hierarchy under
/// the change, so only they read a verdict. A caller emitting anything else owes
/// none, and is spared the step it would take to produce one.
///
/// Returns [`add_change`]'s future itself: a future of its own would hold the
/// arguments again beside it.
fn emit_change(
    from: NodeChangeState,
    to: NodeChangeState,
    action: change::FileAction,
    measured: change::Flags,
    changes: &ChangeSender,
    filter_mode: FilterMode,
) -> impl Future<Output = Result<(), StateError>> {
    debug_assert!(
        !matches!(action, FileAction::Delete | FileAction::Add),
        "{action:?} walks the hierarchy and needs a verdict"
    );
    add_change(
        from,
        to,
        action,
        measured,
        changes,
        filter_mode,
        FilterStates::ROOT,
    )
}

/// [`add_change_where`] walking every child of a directory added or deleted as a whole.
#[allow(clippy::too_many_arguments)]
fn add_change(
    from: NodeChangeState,
    to: NodeChangeState,
    action: change::FileAction,
    measured: change::Flags,
    changes: &ChangeSender,
    filter_mode: FilterMode,
    states: FilterStates,
) -> impl Future<Output = Result<(), StateError>> + Send + '_ {
    add_change_where(
        from,
        to,
        action,
        measured,
        changes,
        filter_mode,
        states,
        false,
    )
}

/// `states` is the filter's verdict for `path`, which the hierarchy walk below
/// steps its children from rather than folding each whole path, and `versioned_only`
/// whether the walk takes only the children a working tree versions
/// ([`diff::versioned_on_a_working_tree`]), as it does in a filesystem State.
///
/// Only a directory added or deleted as a whole is walked: a caller emitting a
/// modification walks the directory itself. The walk is boxed, one allocation per
/// such directory: inline, it would make this future and every future above it
/// larger.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn add_change_where(
    from: NodeChangeState,
    to: NodeChangeState,
    action: change::FileAction,
    measured: change::Flags,
    changes: &ChangeSender,
    filter_mode: FilterMode,
    states: FilterStates,
    versioned_only: bool,
) -> impl Future<Output = Result<(), StateError>> + Send + '_ {
    async move {
        // Avoid adding repository root node in case it was to/from an empty repository
        if from.mapping.node != ROOT_NODE || to.mapping.node != ROOT_NODE {
            // Determine which node to use and load it
            let source = match (
                from.mapping.node.is_valid_node_id(),
                to.mapping.node.is_valid_node_id(),
            ) {
                (_, true) => NodeSource::To,
                (true, false) => NodeSource::From,
                (false, false) => NodeSource::Invalid,
            };
            // Determine if a different node should be used for early checking recursion.
            let recursion_source = if action == change::FileAction::Delete {
                Some(NodeSource::From)
            } else {
                None
            };

            // Only add (file system path not in merkle tree) should end up here for Invalid source
            debug_assert!(source != NodeSource::Invalid || action == FileAction::Add);

            let Some(node) = load_node_for_change(source, &from, &to).await else {
                return Ok(());
            };
            let recursion_node_storage = if let Some(recursion_source) = recursion_source {
                load_node_for_change(recursion_source, &from, &to).await
            } else {
                None
            };
            let recursion_node = recursion_node_storage.as_ref().unwrap_or(&node);

            // Compute flags and create change record
            let flags = compute_change_flags(&node) | measured;

            emit(changes, || NodeChange {
                action,
                flags,
                from: from.clone(),
                to: to.clone(),
            })
            .await?;

            if !recursion_node.is_directory() {
                return Ok(());
            }
        }

        if !matches!(action, FileAction::Add | FileAction::Delete) {
            return Ok(());
        }

        Box::pin(add_change_hierarchy(
            from,
            to,
            action,
            changes,
            filter_mode,
            states,
            versioned_only,
        ))
        .await
    }
}

/// Coalesce the add/delete pairs that name one file into moves.
///
/// A file is identified by the context in its node address, so an add and a delete sharing a
/// non-zero context are the two halves of a move. A change offering no identity, which
/// `NodeChange::move_identity` reports as a zero context for a directory or a link, is left as
/// it is.
///
/// The changes waiting for a partner are kept by identity, in a map sized to the candidates, so
/// the pairing is linear in the changes. Where several adds, or several deletes, share an
/// identity, they pair in the order they stand.
///
/// The vector is modified in place: the add becomes the move, taking the delete's `from` as its
/// source, and the delete is dropped. Changes that are not coalesced keep their order.
pub fn detect_and_coalesce_moves(changes: &mut Vec<NodeChange>) {
    let is_candidate = |change: &NodeChange| {
        matches!(change.action, FileAction::Add | FileAction::Delete)
            && !change.move_identity().is_zero()
    };
    let candidates = changes.iter().filter(|change| is_candidate(change)).count();
    if candidates < 2 {
        return;
    }

    // The first and the last change waiting on each identity, all adds or all deletes. Where
    // more than one waits, `queued` links each to the next.
    let mut waiting: HashMap<Context, (usize, usize)> = HashMap::with_capacity(candidates);
    let mut queued: HashMap<usize, usize> = HashMap::new();
    let mut coalesced: Vec<usize> = Vec::new();
    for index in 0..changes.len() {
        if !is_candidate(&changes[index]) {
            continue;
        }
        let partner = match waiting.entry(changes[index].move_identity()) {
            Entry::Vacant(entry) => {
                entry.insert((index, index));
                continue;
            }
            Entry::Occupied(mut entry) => {
                let (first, last) = *entry.get();
                if changes[first].action == changes[index].action {
                    queued.insert(last, index);
                    entry.get_mut().1 = index;
                    continue;
                }
                match queued.remove(&first) {
                    Some(next) => entry.get_mut().0 = next,
                    None => {
                        entry.remove();
                    }
                }
                first
            }
        };
        let (add, delete) = if changes[index].action == FileAction::Add {
            (index, partner)
        } else {
            (partner, index)
        };

        let from_state = changes[delete].from.clone();
        lore_trace!(
            "Detected move: {} -> {}",
            from_state.mapping.path.as_str(),
            changes[add].path().as_str()
        );
        let moved = &mut changes[add];
        moved.action = FileAction::Move;
        moved.from = from_state;
        if moved.from.differs_from(&moved.to) {
            moved.flags |= change::Flags::Modify;
        }
        coalesced.push(delete);
    }

    if coalesced.is_empty() {
        return;
    }
    coalesced.sort_unstable();
    let mut position = 0;
    changes.retain(|_| {
        let keep = coalesced.binary_search(&position).is_err();
        position += 1;
        keep
    });
}

/// The side of `change`, an add or a delete of a walk, that holds the node, and the side the walk
/// found nothing on.
fn walked_sides(change: &NodeChange) -> (&NodeChangeState, &NodeChangeState) {
    match change.action {
        FileAction::Delete => (&change.from, &change.to),
        _ => (&change.to, &change.from),
    }
}

/// Whether `change`, an add or a delete of a walk, adds or deletes a file that has an identity, and
/// the side the walk found nothing on is `state` itself rather than a linked repository's tree.
pub fn changes_own_file(state: &Arc<State>, change: &NodeChange) -> bool {
    let (side, absent) = walked_sides(change);
    Arc::ptr_eq(&absent.mapping.state, state)
        && side.flags.contains(NodeFlags::File)
        && !side.address.context.is_zero()
}

/// The move of a node from where `from` stands to where `to` stands.
fn moved(from: NodeChangeState, to: NodeChangeState) -> NodeChange {
    NodeChange {
        action: FileAction::Move,
        flags: if from.differs_from(&to) {
            change::Flags::Modify
        } else {
            change::Flags::None
        },
        from,
        to,
    }
}

/// The move `change`, an add or a delete [`changes_own_file`] answers for, is half of: where the
/// tree the walk found nothing in holds the node with its ID, a file of the same identity, the move
/// between the path it stands at there and the change's path. `None` where that tree holds no such
/// file.
pub async fn whole_move(change: &NodeChange) -> Result<Option<NodeChange>, StateError> {
    let (side, absent) = walked_sides(change);
    let (repository, state, id) = (
        &absent.mapping.repository,
        &absent.mapping.state,
        side.mapping.node,
    );
    let Some(node) = state.try_node(repository.clone(), id).await else {
        return Ok(None);
    };
    if !node.is_file()
        || node.is_discarded()
        || node.is_staged_delete()
        || node.address.context != side.address.context
    {
        return Ok(None);
    }
    let Some(path) = state
        .node_path_below(repository.clone(), id, ROOT_NODE)
        .await?
    else {
        return Ok(None);
    };
    let other = absent.from_child(id, &node, path);
    Ok(Some(match change.action {
        FileAction::Delete => moved(change.from.clone(), other),
        _ => moved(other, change.to.clone()),
    }))
}

/// Where a walk of a path stops, as [`State::find_to_link`] answers it: at `node`, which the
/// leading `length` bytes of the path name.
#[derive(Clone, Copy)]
pub(crate) struct PathStop {
    pub(crate) node: NodeID,
    pub(crate) length: usize,
    pub(crate) at: Stop,
}

/// What a walk of a path stops at.
#[derive(Clone, Copy)]
pub(crate) enum Stop {
    /// The end of the path.
    End,
    /// A link, which mounts what it names.
    Link(NodeLink),
    /// A component the tree lacks, `node` being the deepest it holds on the path.
    Missing,
}

impl PathStop {
    /// No node, where the tree holds none to walk from.
    const NONE: PathStop = PathStop::missing(INVALID_NODE, 0);

    const fn missing(node: NodeID, length: usize) -> Self {
        PathStop {
            node,
            length,
            at: Stop::Missing,
        }
    }

    /// The node the path names, a link not entered, or an invalid node where the tree holds
    /// none.
    fn named(&self) -> NodeID {
        match self.at {
            Stop::End | Stop::Link(_) => self.node,
            Stop::Missing => INVALID_NODE,
        }
    }

    /// The length of the mount and what the link mounts, where the walk stopped at a link.
    fn link(&self) -> Option<(usize, NodeLink)> {
        match self.at {
            Stop::Link(link) => Some((self.length, link)),
            _ => None,
        }
    }
}

/// One side of a diff at a path: the tree, and the node it holds there, which is invalid where
/// it holds none.
#[derive(Clone)]
pub(crate) struct DiffSide {
    pub(crate) repository: Arc<RepositoryContext>,
    pub(crate) state: Arc<State>,
    pub(crate) node: NodeID,
}

impl DiffSide {
    /// Where `path` below this side's node stops.
    async fn stop(&self, path: &str) -> Result<PathStop, StateError> {
        if !self.node.is_valid_or_root_node_id() {
            return Ok(PathStop::NONE);
        }
        self.state
            .find_to_link(&self.repository, self.node, path, true)
            .await
    }

    /// This side entering a link at `linked`, the node of the linked tree it mounts.
    async fn enter(self, linked: &NodeLink) -> Result<DiffSide, StateError> {
        let (repository, state) = linked.resolve(self.repository, self.state).await?;
        Ok(DiffSide {
            repository,
            state,
            node: linked.node,
        })
    }
}

/// The link mounts the walk over the whole tree holds on reaching a path, as it replaces them on
/// crossing each link.
#[derive(Clone, Copy)]
enum WalkMounts {
    /// Those of the links the two trees hold, collected at the root.
    Root,
    /// Those of the links below the last link the from tree holds that the walk crossed, in the
    /// linked trees of both sides.
    Linked,
    /// None, the last link crossed being one only the current tree holds.
    Empty,
}

/// The sides a walk compares below a path, each entering the link it holds there, which
/// `from_link` and `current_link` name. Where only the from side holds one, both compare its
/// linked tree, so that what the linked tree tracks is not found new.
pub(crate) async fn cross_links(
    from: DiffSide,
    current: DiffSide,
    from_link: Option<&NodeLink>,
    current_link: Option<&NodeLink>,
) -> Result<(DiffSide, DiffSide), StateError> {
    let from = match from_link {
        Some(link) => from.enter(link).await?,
        None => from,
    };
    let current = match current_link {
        Some(link) => current.enter(link).await?,
        None if from_link.is_some() => from.clone(),
        None => current,
    };
    Ok((from, current))
}

/// The link mounts below `path` in the trees two sides crossed into, at `from_node` and
/// `current_node`: what the walk below them steps around. A tree both sides pin to one revision
/// at one node is read once, as each side holds its own copy of it.
pub(crate) async fn collect_crossed_mounts(
    from: &DiffSide,
    from_node: NodeID,
    current: &DiffSide,
    current_node: NodeID,
    path: &RelativePath,
) -> Result<Vec<LinkMountInfo>, StateError> {
    let mut trees = Vec::with_capacity(2);
    if from_node.is_valid_or_root_node_id() {
        trees.push((&from.state, &from.repository, from_node));
    }
    let same = current_node == from_node
        && current.repository.id == from.repository.id
        && current.state.revision() == from.state.revision();
    if current_node.is_valid_or_root_node_id() && !same {
        trees.push((&current.state, &current.repository, current_node));
    }
    collect_link_mounts(&trees, path).await
}

/// What a diff scoped to a path compares, as [`scoped_roots`] answers it.
struct ScopedRoots {
    from: NodeMapping,
    current: NodeMapping,
    link_mounts: Vec<LinkMountInfo>,
    chain_root: ChainRoot,
}

/// What the working tree holds at a link mount.
enum MountOnDisk {
    Directory,
    File,
    /// Nothing, or nothing a walk descends into or compares as a file.
    Missing,
}

/// What the working tree holds at `mount`, as the walk finds it: a listing leaves out a symbolic
/// link above the path's end, where the walk reads a listing, but not at it.
async fn mount_on_disk(
    repository: &RepositoryContext,
    mount: &str,
    at_path: bool,
) -> Result<MountOnDisk, StateError> {
    let Ok(mount) = RelativePath::from_str(mount);
    let absolute = mount
        .to_absolute_path(repository.require_path()?)
        .forward::<StateError>("placing the mount below the root")?;
    let driver = lore_io::IoDriver::global();
    let stat = if at_path {
        driver.stat(absolute).await
    } else {
        driver.symlink_stat(absolute).await
    };
    Ok(match stat {
        Ok(stat) if stat.kind == lore_io::StatKind::Directory => MountOnDisk::Directory,
        Ok(stat) if stat.kind == lore_io::StatKind::File => MountOnDisk::File,
        _ => MountOnDisk::Missing,
    })
}

/// What a diff scoped to `path` compares, as the walk over the whole tree compares it on reaching
/// `path`: the roots on each side, the link mounts below them, and where a marking walk adds the
/// directories leading to a path the from tree lacks. `None` where that walk reports nothing under
/// `path`.
///
/// The path is resolved one link at a time, deciding each link it crosses or ends at as the walk
/// does on meeting it ([`cross_links`]), and what the working tree holds at its mount:
///
/// - A mount of a link the from tree holds that the working tree does not hold as a directory
///   keeps its link, decided before the linked tree is read, since a mount stays absent while its
///   linked repository cannot be read. The path at a mount the working tree holds as a file names
///   the link itself.
/// - Where the from tree holds nothing at a link only the current tree holds, a directory the
///   working tree holds there belongs to the linked repository if the walk's mounts name the
///   link, and is new to both sides otherwise, as a file there is.
/// - Where the from tree holds a node at a link only the current tree holds and the working tree
///   no directory, the from tree's node is compared alone, or against the link where the path
///   ends at a file the working tree holds there.
/// - Where the from tree holds a file at the path and the working tree a directory, the file is
///   replaced as the walk replaces a node whose type changed. Where it holds one above the path,
///   a marking walk stays out of the directory and a reporting one finds it new.
///
/// Boxed by its caller: it runs once per named path, and inline it would widen the future of
/// every diff and status scan by its own.
async fn scoped_roots(
    from: FilesystemDiffTree,
    current: FilesystemDiffTree,
    path: &RelativePath,
    intent: FilesystemDiffIntent,
) -> Result<Option<ScopedRoots>, StateError> {
    let mut from_side = DiffSide {
        repository: from.repository.clone(),
        state: from.state.clone(),
        node: ROOT_NODE,
    };
    let mut current_side = DiffSide {
        repository: current.repository.clone(),
        state: current.state.clone(),
        node: ROOT_NODE,
    };
    let mut mounts = WalkMounts::Root;
    let mut offset = 0;
    let (from_end, current_end) = loop {
        let rest = path.as_str().get(offset..).unwrap_or_default();
        let current_end = current_side.stop(rest).await?;
        let limit = current_end.link().map_or(rest.len(), |(length, _)| length);
        let from_end = from_side.stop(&rest[..limit]).await?;
        let (length, from_link, current_link) = match (from_end.link(), current_end.link()) {
            (Some((length, from_link)), current_link) => (
                length,
                Some(from_link),
                current_link
                    .filter(|(mount, _)| *mount == length)
                    .map(|(_, link)| link),
            ),
            (None, Some((length, current_link))) => (length, None, Some(current_link)),
            (None, None) => break (from_end, current_end),
        };
        let mount = &path.as_str()[..offset + length];
        let at_path = offset + length == path.len();
        if from_link.is_none() {
            if matches!(from_end.at, Stop::Missing) {
                let listed = !matches!(mounts, WalkMounts::Empty);
                if listed
                    && matches!(
                        mount_on_disk(&from.repository, mount, at_path).await?,
                        MountOnDisk::Directory
                    )
                {
                    return Ok(None);
                }
                break (from_end, PathStop::NONE);
            }
            from_side.node = from_end.node;
            let directory = from_side
                .state
                .node(from_side.repository.clone(), from_side.node)
                .await?
                .is_directory();
            match mount_on_disk(&from.repository, mount, at_path).await? {
                MountOnDisk::Directory if directory => {}
                MountOnDisk::Directory if at_path => break (from_end, PathStop::NONE),
                MountOnDisk::Directory if intent.marks_dirty() => return Ok(None),
                MountOnDisk::Directory => break (PathStop::NONE, PathStop::NONE),
                MountOnDisk::File if at_path => break (from_end, current_end),
                MountOnDisk::File | MountOnDisk::Missing => {
                    current_side.node = INVALID_NODE;
                    offset += length + 1;
                    continue;
                }
            }
        } else {
            match mount_on_disk(&from.repository, mount, at_path).await? {
                MountOnDisk::Directory => {}
                MountOnDisk::File if at_path => break (from_end, current_end),
                MountOnDisk::File | MountOnDisk::Missing => return Ok(None),
            }
        }
        (from_side, current_side) = cross_links(
            from_side,
            current_side,
            from_link.as_ref(),
            current_link.as_ref(),
        )
        .await?;
        mounts = if from_link.is_some() {
            WalkMounts::Linked
        } else {
            WalkMounts::Empty
        };
        offset += length + 1;
    };

    if intent.marks_dirty()
        && matches!(from_end.at, Stop::Missing)
        && from_end.node.is_valid_or_root_node_id()
        && !from_side
            .state
            .node(from_side.repository.clone(), from_end.node)
            .await?
            .is_directory()
    {
        return Ok(None);
    }
    let from_node = from_end.named();
    let current_node = current_end.named();
    let link_mounts = match mounts {
        WalkMounts::Root => {
            collect_link_mounts(
                &[
                    (&from.state, &from.repository, ROOT_NODE),
                    (&current.state, &current.repository, ROOT_NODE),
                ],
                &RelativePath::new(),
            )
            .await?
        }
        WalkMounts::Linked => {
            collect_crossed_mounts(&from_side, from_node, &current_side, current_node, path).await?
        }
        WalkMounts::Empty => Vec::new(),
    };
    let skip = u32::try_from(offset.min(path.len())).internal("measuring the scoped path")?;
    Ok(Some(ScopedRoots {
        chain_root: ChainRoot {
            node: from_side.node,
            skip,
        },
        from: NodeMapping {
            repository: from_side.repository,
            state: from_side.state,
            path: path.clone(),
            node: from_node,
        },
        current: NodeMapping {
            repository: current_side.repository,
            state: current_side.state,
            path: path.clone(),
            node: current_node,
        },
        link_mounts,
    }))
}

/// Calculate the set of changes between two revision states and emit them
/// into `changes`. Streams raw `Add` / `Delete` / `Keep` records as discovered
/// — does **not** run the post-walk move-coalescing or path-sort fixup that
/// the legacy `Vec`-returning version applied. Callers that want the
/// historical buffered-and-coalesced shape use `diff_collect` instead.
///
/// Answers with what the walk did rather than what it found, for a caller
/// measuring it; the changes are the `changes` channel's. `diff_collect`
/// discards it.
#[allow(clippy::too_many_arguments)]
pub async fn diff(
    repository_from: Arc<RepositoryContext>,
    state_from: Arc<State>,
    repository_to: Arc<RepositoryContext>,
    state_to: Arc<State>,
    path: Option<RelativePath>,
    graft: Option<Arc<GraftOracle>>,
    changes: &ChangeSender,
    filter_mode: FilterMode,
) -> Result<DiffWalkStats, StateError> {
    if let Some(path) = path {
        let from_link = state_from
            .find_node_link(repository_from.clone(), path.as_str())
            .await
            .unwrap_or(NodeLink::invalid());
        let to_link = state_to
            .find_node_link(repository_to.clone(), path.as_str())
            .await
            .unwrap_or(NodeLink::invalid());

        let mut repository_from = repository_from;
        let state_from =
            if !from_link.repository.is_zero() && from_link.repository != repository_from.id {
                repository_from = repository_from.to_link_context(from_link.repository).await;
                State::deserialize(repository_from.clone(), from_link.revision).await?
            } else {
                state_from
            };

        let mut repository_to = repository_to;
        let state_to = if !to_link.repository.is_zero() && to_link.repository != repository_to.id {
            repository_to = repository_to.to_link_context(to_link.repository).await;
            State::deserialize(repository_to.clone(), to_link.revision).await?
        } else {
            state_to
        };

        let from =
            node_change_state(&repository_from, &state_from, from_link.node, path.clone()).await;
        let to = node_change_state(&repository_to, &state_to, to_link.node, path.clone()).await;

        diff::diff_subtree(from, to, path, graft, changes, filter_mode).await
    } else {
        diff::diff_subtree(
            NodeChangeState {
                mapping: NodeMapping {
                    repository: repository_from,
                    state: state_from,
                    path: RelativePath::new(),
                    node: ROOT_NODE,
                },
                observed: None,
                flags: NodeFlags::NoFlags,
                address: Address::default(),
                mode: 0,
            },
            NodeChangeState {
                mapping: NodeMapping {
                    repository: repository_to,
                    state: state_to,
                    path: RelativePath::new(),
                    node: ROOT_NODE,
                },
                observed: None,
                flags: NodeFlags::NoFlags,
                address: Address::default(),
                mode: 0,
            },
            RelativePath::new(),
            graft,
            changes,
            filter_mode,
        )
        .await
    }
}

/// The node `node_id` of `state` at `path`, as one side of a change, carrying the flags and
/// address it holds. A node the state does not hold is one side of an add or a delete, and
/// carries none.
pub(crate) async fn node_change_state(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    node_id: NodeID,
    path: RelativePath,
) -> NodeChangeState {
    let (address, mode, flags) = if let Ok(node) = state.node(repository.clone(), node_id).await {
        (
            node.address,
            node.mode,
            NodeFlags::from_bits_retain(node.flags),
        )
    } else {
        (Address::default(), 0, NodeFlags::NoFlags)
    };
    NodeChangeState {
        mapping: NodeMapping {
            repository: repository.clone(),
            state: state.clone(),
            path,
            node: node_id,
        },
        observed: None,
        flags,
        address,
        mode,
    }
}

/// The changes between the subtree `from` names and the subtree `to` names, spelled from `path`.
///
/// A subtree a working copy materializes somewhere other than where the repository holding it
/// spells it from — what a link mounts — is diffed by naming its node in each revision and
/// reporting at the path it is materialized at, so no change carries the other spelling.
pub async fn diff_collect_subtree(
    from: NodeChangeState,
    to: NodeChangeState,
    path: RelativePath,
    filter_mode: FilterMode,
) -> Result<Vec<NodeChange>, StateError> {
    let mut changes = ChangeStream::spawn(async move |changes| {
        diff::diff_subtree(from, to, path, None, &changes, filter_mode).await
    })
    .collect()
    .await?;
    detect_and_coalesce_moves(&mut changes);
    crate::change::sort_by_path(&mut changes);
    Ok(changes)
}

/// Collect the set of changes between two revision states into a `Vec`,
/// preserving the post-walk move-coalescing and path-sort that the legacy
/// `state::diff` performed. Callers that want a `Vec<NodeChange>` use this
/// wrapper; callers that want streaming use `diff` directly.
pub async fn diff_collect(
    repository_from: Arc<RepositoryContext>,
    state_from: Arc<State>,
    repository_to: Arc<RepositoryContext>,
    state_to: Arc<State>,
    path: Option<RelativePath>,
    filter_mode: FilterMode,
) -> Result<Vec<NodeChange>, StateError> {
    let mut changes = ChangeStream::spawn(async move |changes| {
        diff(
            repository_from,
            state_from,
            repository_to,
            state_to,
            path,
            None,
            &changes,
            filter_mode,
        )
        .await
    })
    .collect()
    .await?;
    detect_and_coalesce_moves(&mut changes);
    crate::change::sort_by_path(&mut changes);
    Ok(changes)
}

/// A state's node mapped to its path in the repository instance's file system.
///
/// `path` is as seen from the top-level repository instance root, which every repository context
/// shares, while `node` names the same subtree in `state`'s own repository tree. The two part
/// wherever a link or layer mount draws its subtree from a path other than the one it is mounted
/// at. An invalid `node` is a path the state holds nothing at, which is where a file the file
/// system alone holds stands.
///
/// As an example, a link mounting a linked repository at `mount`, for the file that repository
/// holds at `dir/file.txt`:
///
/// - `repository`: a context whose root is the top-level repository instance root and whose
///   repository ID is the linked repository's
/// - `state`: the linked repository's revision state
/// - `path`: `mount/dir/file.txt`
/// - `node`: the node ID for `dir/file.txt` in `state`, loaded from `repository`
#[derive(Clone, Debug)]
pub struct NodeMapping {
    /// The repository `node` and its content exist in.
    pub repository: Arc<RepositoryContext>,
    /// The revision holding `node`.
    pub state: Arc<State>,
    /// The path of `node` as a relative path from the top-level repository instance root.
    pub path: RelativePath,
    /// The node in `state`'s own tree.
    pub node: NodeID,
}

impl NodeMapping {
    /// A state's own root, for a caller whose paths are spelled from the top-level repository
    /// instance root.
    pub fn root(repository: Arc<RepositoryContext>, state: Arc<State>) -> Self {
        NodeMapping {
            repository,
            state,
            path: RelativePath::new(),
            node: ROOT_NODE,
        }
    }

    /// The node this mapping holds at `path`, or an invalid link where it holds none.
    ///
    /// `path` is spelled from the top-level repository instance root, as every change is, and the
    /// names below this mapping's own path are walked from `node`. Both are the tree's own
    /// spelling: a path taken from the file system answers for nothing where the two name a
    /// component with different case.
    pub async fn node_at(&self, path: &RelativePath) -> NodeLink {
        let Some(below) = path.below(&self.path) else {
            return NodeLink::invalid();
        };
        self.state
            .find_relative_node_link(self.repository.clone(), self.node, below)
            .await
            .unwrap_or_default()
    }
}

#[derive(Default)]
pub struct FilesystemDiffStats {
    pub file_add: AtomicU64,
    pub file_delete: AtomicU64,
    pub file_retain: AtomicU64,
    pub file_replace: AtomicU64,
    /// Files the answer required reading, including any that could not be read.
    pub file_hash: AtomicU64,
    /// Files a recorded modified time answered for, sparing them a hash check.
    pub file_mtime_match: AtomicU64,
    /// Adds the tree no longer holds, discarded from the tree diffed against it.
    pub discarded: AtomicU64,
    /// Where the provider diff met its filesystem State changed since it listed a directory, as
    /// the walk's `DiffWalkStats::moved_under_walk` counts it. For tests.
    #[doc(hidden)]
    pub moved_under_walk: AtomicU64,
    /// Of those, the finds made for a delete since the directory's chain changed, as
    /// `DiffWalkStats::finds_after_change` counts them. For tests.
    #[doc(hidden)]
    pub finds_after_change: AtomicU64,
}

impl FilesystemDiffStats {
    fn append(&mut self, stats: FilesystemDiffStats) {
        self.file_add
            .fetch_add(stats.file_add.load(Ordering::Relaxed), Ordering::Relaxed);
        self.file_delete
            .fetch_add(stats.file_delete.load(Ordering::Relaxed), Ordering::Relaxed);
        self.file_retain
            .fetch_add(stats.file_retain.load(Ordering::Relaxed), Ordering::Relaxed);
        self.file_replace.fetch_add(
            stats.file_replace.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.file_hash
            .fetch_add(stats.file_hash.load(Ordering::Relaxed), Ordering::Relaxed);
        self.file_mtime_match.fetch_add(
            stats.file_mtime_match.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.discarded
            .fetch_add(stats.discarded.load(Ordering::Relaxed), Ordering::Relaxed);
        self.moved_under_walk.fetch_add(
            stats.moved_under_walk.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.finds_after_change.fetch_add(
            stats.finds_after_change.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }

    /// Record what settled a file's comparison. A size mismatch counts as neither.
    pub fn classify(&self, modification: &FileModification) {
        match modification.answered_by() {
            ComparisonAnswer::Mtime => {
                self.file_mtime_match.fetch_add(1, Ordering::Relaxed);
            }
            ComparisonAnswer::Hash => {
                self.file_hash.fetch_add(1, Ordering::Relaxed);
            }
            ComparisonAnswer::Size => {}
        }
    }
}

/// Information about a configured layer mount used by `diff_filesystem` to
/// switch comparison context when the filesystem walk crosses into a layer.
///
/// When the parent's filesystem walker encounters a directory whose
/// parent-relative path equals `mount_path`, it stops treating the directory
/// as a "new" entry and instead recurses into it using `repository` and
/// `state` — i.e., compares the on-disk content against the layer's tree
/// rather than the parent's.
#[derive(Clone, Debug)]
pub struct LayerMountInfo {
    /// Parent-relative mount path (e.g. `"external/lib"`).
    pub mount_path: String,
    /// The layer's repository context — node lookups during the recursion
    /// resolve in this repo.
    pub repository: Arc<RepositoryContext>,
    /// State to diff against. Typically the layer's staged state (falling
    /// back to current when no staging has happened).
    pub state: Arc<State>,
    /// Node ID in `state` corresponding to the layer's `source_path`.
    pub source_node: NodeID,
}

/// Information about a link mount, collected once per diff so the per-directory pass
/// can detect "this filesystem directory is a link, not a fresh add" with a single
/// linear `find` instead of an async block-walk per directory.
///
/// Only `mount_path` is needed: the walk tells a mount from a directory only the working tree
/// holds by its path, and crosses into a link it holds through the link node itself.
pub struct LinkMountInfo {
    /// Parent-relative mount path of the link node (e.g. `"libs/shared"`).
    pub mount_path: RelativePath,
}

impl LinkMountInfo {
    /// Whether any of `mounts` sits at or under `path`, which the repository root does.
    ///
    /// Case-insensitive, because node lookup is: a path a caller named resolves to the node a
    /// mount hangs below however either is spelled.
    pub(crate) fn any_under(mounts: &[LinkMountInfo], path: &RelativePath) -> bool {
        if path.is_empty() {
            return !mounts.is_empty();
        }
        mounts
            .iter()
            .any(|mount| path.covers_ignore_case(&mount.mount_path))
    }
}

/// Mount paths of the links `state` holds below `source_node`, spelled from `mount_path` the
/// way the walk that crossed into it spells paths.
pub(crate) async fn collect_link_mounts_below(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    source_node: NodeID,
    mount_path: &RelativePath,
) -> Result<Vec<LinkMountInfo>, StateError> {
    let mut mounts = Vec::new();
    for link_ref in state.link_list(repository.clone()).await?.iter() {
        // The list is a hint a walk consults, so an entry it cannot place is dropped.
        let below = match state
            .node_path_below(
                repository.clone(),
                link_ref.local_node as NodeID,
                source_node,
            )
            .await
        {
            Ok(below) => below,
            Err(err) => {
                lore_debug!(
                    "Link {} node {} resolves no mount path: {err}",
                    link_ref.repository,
                    link_ref.local_node
                );
                continue;
            }
        };
        if let Some(below) = below.filter(|below| !below.is_empty()) {
            mounts.push(LinkMountInfo {
                mount_path: mount_path.join(below.as_str()),
            });
        }
    }
    Ok(mounts)
}

/// Mount paths of the links the trees hold below the node named for each, spelled from
/// `mount_path`, with one entry per path. The result rides in `FilesystemDiffContext` by
/// reference so the per-directory walk avoids an O(depth) block-walk per new directory on
/// fresh checkouts.
///
/// Every tree is read because a staged tree carries links no commit has recorded yet, and a
/// link the walk crosses can be pinned to a different revision on each side.
pub(crate) async fn collect_link_mounts(
    trees: &[(&Arc<State>, &Arc<RepositoryContext>, NodeID)],
    mount_path: &RelativePath,
) -> Result<Vec<LinkMountInfo>, StateError> {
    let mut mounts: Vec<LinkMountInfo> = Vec::new();
    for (state, repository, source_node) in trees {
        for mount in collect_link_mounts_below(state, repository, *source_node, mount_path).await? {
            if !mounts
                .iter()
                .any(|seen| seen.mount_path.as_str() == mount.mount_path.as_str())
            {
                mounts.push(mount);
            }
        }
    }
    Ok(mounts)
}

/// The filter slots a walk consults. A forced walk consults none: it drops nothing, so it
/// reports nothing as excluded and descends everywhere, which is what an empty mode is. The
/// verdicts it threads are then folded but never read, so no site below needs to know.
///
/// Only a staging walk is forced. `status --scan` answers for the view whether or not the
/// command that asked was forced.
fn walk_filter_mode(filter_mode: FilterMode, intent: FilesystemDiffIntent) -> FilterMode {
    if intent.stage().is_some() && execution_context().globals().force() {
        return FilterMode::empty();
    }
    filter_mode
}

/// Resolves `path` on both sides and diffs the filesystem under it through
/// `operation`, appending a change per difference to `changes`.
///
/// Resolution reads the trees, and the working tree only at each link mount `path` reaches, so
/// a filesystem implementation is handed roots it never has to follow a link to find. The filter
/// is asked about `path` as what the working tree holds there, else as the node `from` holds, a
/// link as a directory, and as a directory where neither holds anything.
///
/// `layer_mounts` is consulted only by the parent's walk to switch context when crossing
/// into a configured layer. Pass an empty Arc for non-layer-aware callers; the
/// layer-internal recursion always passes empty, there being no nested layer mounts under
/// non-overlapping layers.
///
/// A marking walk discards what it finds stale once it has drained. A caller running several
/// walks over one tree uses `diff_filesystem_queuing` instead.
#[allow(clippy::too_many_arguments)]
pub fn diff_filesystem<'a>(
    operation: &'a Arc<InstanceOperationImpl>,
    from: FilesystemDiffTree,
    current: FilesystemDiffTree,
    path: Option<RelativePath>,
    filter_mode: FilterMode,
    intent: FilesystemDiffIntent,
    layer_mounts: Arc<Vec<LayerMountInfo>>,
) -> impl Future<Output = Result<ChangeStream<FilesystemDiffStats>, StateError>> + Send + 'a {
    diff_filesystem_queuing(
        operation,
        from,
        current,
        path,
        filter_mode,
        intent,
        layer_mounts,
        None,
    )
}

/// [`diff_filesystem`], holding what a marking walk finds stale on `discards` where given, for
/// the caller to discard once every walk over the tree has drained, rather than discarding it
/// once this walk has drained.
#[lore_macro::test_pub]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn diff_filesystem_queuing(
    operation: &Arc<InstanceOperationImpl>,
    from: FilesystemDiffTree,
    current: FilesystemDiffTree,
    path: Option<RelativePath>,
    filter_mode: FilterMode,
    intent: FilesystemDiffIntent,
    layer_mounts: Arc<Vec<LayerMountInfo>>,
    discards: Option<Arc<WalkDiscards>>,
) -> Result<ChangeStream<FilesystemDiffStats>, StateError> {
    let FilesystemDiffTree {
        repository: repository_from,
        state: state_from,
    } = from;
    let FilesystemDiffTree {
        repository: repository_current,
        state: state_current,
    } = current;
    let filter_mode = walk_filter_mode(filter_mode, intent);
    let states = match path.as_ref() {
        Some(path) => {
            let filter = &repository_from.filter;
            let states = filter.parent_exclusion_states(path);
            // The tree is searched only for a path the filter would drop whatever it holds, since
            // a path the tree tracks is past the ignore slot.
            if filter.excludes_any_kind(states, path, filter_mode) {
                let tracked = state_from
                    .relative_path_tracked_as(repository_from.clone(), ROOT_NODE, path.as_str())
                    .await?
                    .is_some();
                let mode = (filter_mode | FilterMode::KeepTracked).at(tracked);
                if filter.emit_excludes_any_kind(states, path, mode) {
                    return Ok(ChangeStream::nothing());
                }
            }
            states
        }
        None => FilterStates::ROOT,
    };

    let Some(path) = path else {
        let link_mounts = collect_link_mounts(
            &[
                (&state_from, &repository_from, ROOT_NODE),
                (&state_current, &repository_current, ROOT_NODE),
            ],
            &RelativePath::new(),
        )
        .await?;
        return Ok(diff_with(
            operation,
            FilesystemDiffContext {
                operation: operation.clone(),
                from: NodeMapping {
                    repository: repository_from,
                    state: state_from,
                    path: RelativePath::new(),
                    node: ROOT_NODE,
                },
                current: NodeMapping {
                    repository: repository_current,
                    state: state_current,
                    path: RelativePath::new(),
                    node: ROOT_NODE,
                },
                filesystem_path: RelativePath::new(),
                states,
                from_states: states,
                named_path: None,
                filter_mode,
                intent,
                layer_mounts,
                link_mounts: Arc::new(link_mounts),
                discards,
            },
        ));
    };

    let Some(roots) = Box::pin(scoped_roots(
        FilesystemDiffTree {
            repository: repository_from,
            state: state_from,
        },
        FilesystemDiffTree {
            repository: repository_current,
            state: state_current,
        },
        &path,
        intent,
    ))
    .await?
    else {
        return Ok(ChangeStream::nothing());
    };

    Ok(diff_with(
        operation,
        FilesystemDiffContext {
            operation: operation.clone(),
            from: roots.from,
            current: roots.current,
            filesystem_path: path,
            states,
            from_states: states,
            named_path: Some(roots.chain_root),
            filter_mode,
            intent,
            layer_mounts,
            link_mounts: Arc::new(roots.link_mounts),
            discards,
        },
    ))
}

/// Whether `node` is staged only because something below it is: the bare `Staged` bit
/// [`State::node_mark`] carries up to the ancestors of the node it marks. An action bit can be the
/// node's own staged action or its dirty one, so a node carrying one, a merge flag, or a link is
/// taken to be staged in its own right.
fn is_staged_for_descendants_only(node: &Node) -> bool {
    node.is_staged()
        && !node.is_link()
        && node.flags & (NodeFlags::ActionBits.bits() | NodeFlags::MergeBits.bits()) == 0
}

/// Whether `node` is marked dirty for a change of its own, an action bit with no staged change to
/// claim it, as a directory marked dirty as added or moved is, rather than by the bare `Dirty` bit
/// a mark below carries up.
fn is_dirty_in_own_right(node: &Node) -> bool {
    node.is_dirty() && !node.is_staged() && node.flags & NodeFlags::ActionBits.bits() != 0
}

/// Clear the `Dirty` and `Staged` bits that marking a node carried up to `ancestor` and the nodes
/// above it, for a node that has since left the tree, in one walk up the chain. `Dirty` is cleared
/// up to the first node that still has a dirty child or is [dirty in its own
/// right](is_dirty_in_own_right), as a directory marked dirty as added is however little it holds,
/// `Staged` up to the first that still has a staged child or is not [staged for descendants
/// only](is_staged_for_descendants_only).
///
/// Must not run while a walk reads or marks the tree. Marking stops at the first ancestor already
/// carrying the bit, so a node marked between this checking an ancestor's children and clearing its
/// bit is left below an ancestor without it.
pub(crate) async fn clear_propagation(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    mut ancestor: NodeID,
) -> Result<(), StateError> {
    let (mut dirty, mut staged) = (true, true);
    while ancestor.is_valid_or_root_node_id() {
        let block_index = NodeBlock::index(ancestor);
        let node_index = Node::index(ancestor);
        let block = state.block(repository.clone(), block_index).await?;
        let (parent, mut children) = {
            let (node, children) = StateNodeChildrenIterator::with_parent_in(
                state.clone(),
                repository.clone(),
                ancestor,
                block.clone(),
            );
            staged &= is_staged_for_descendants_only(&node);
            dirty &= !is_dirty_in_own_right(&node);
            (node.parent, children)
        };
        while (dirty || staged)
            && let Some((_, child)) = children.next().await?
        {
            dirty &= !child.is_dirty();
            staged &= !child.is_staged();
        }
        if !dirty && !staged {
            break;
        }
        let dirtied = {
            #[cfg(feature = "state-hooks")]
            let _label = crate::state::node::hooks::log::label(
                crate::state::node::hooks::log::Section::ClearPropagation,
            );
            let mut block_writer = block.write();
            let writer_node = block_writer.node(node_index);
            if dirty {
                writer_node.clear_dirty_flags();
            }
            if staged {
                writer_node.clear_staged_flags();
            }
            block_writer.mark_dirty()
        };
        if dirtied {
            state.block_modified(block, block_index);
        }
        if ancestor == ROOT_NODE {
            break;
        }
        ancestor = parent;
    }
    Ok(())
}

/// Diffs the filesystem under `node_path` against roots the caller has already
/// resolved, through `operation`.
///
/// [`diff_filesystem`] is the entry for a caller holding a path rather than roots.
#[allow(clippy::too_many_arguments)]
pub async fn diff_filesystem_subtree(
    operation: &Arc<InstanceOperationImpl>,
    from: NodeMapping,
    current: NodeMapping,
    filesystem_path: RelativePath,
    filter_mode: FilterMode,
    intent: FilesystemDiffIntent,
    layer_mounts: Arc<Vec<LayerMountInfo>>,
) -> Result<ChangeStream<FilesystemDiffStats>, StateError> {
    let filter_mode = walk_filter_mode(filter_mode, intent);
    let link_mounts = Arc::new(
        collect_link_mounts(
            &[
                (&from.state, &from.repository, ROOT_NODE),
                (&current.state, &current.repository, ROOT_NODE),
            ],
            &RelativePath::new(),
        )
        .await?,
    );
    let states = from.repository.filter.exclusion_states(&filesystem_path);
    Ok(diff_with(
        operation,
        FilesystemDiffContext {
            operation: operation.clone(),
            from,
            current,
            filesystem_path,
            states,
            from_states: states,
            named_path: None,
            filter_mode,
            intent,
            layer_mounts,
            link_mounts,
            discards: None,
        },
    ))
}

/// Hands one diff to `operation`, which answers with the walk behind a stream.
fn diff_with(
    operation: &Arc<InstanceOperationImpl>,
    context: FilesystemDiffContext,
) -> ChangeStream<FilesystemDiffStats> {
    operation.changes_from_filesystem_to_state(context)
}

/// Result of comparing a single file from filesystem against state.
/// This captures the common logic for single-file node comparison.
#[derive(Debug)]
pub enum SingleFileCompareResult {
    /// File is unmodified (content matches state)
    Unmodified,
    /// File is modified (content differs from state)
    Modified,
    /// File is new (not present in state)
    NewFile,
    /// Type changed from directory/link to file
    TypeChangedToFile,
    /// Type changed from file to directory
    TypeChangedToDirectory,
}

/// Context for creating file diff changes.
/// Encapsulates all the state needed to create `NodeChangeState` instances.
struct FileDiffContext {
    repository_from: Arc<RepositoryContext>,
    state_from: Arc<State>,
    from_node_id: NodeID,
    from_node: Option<Node>,
    /// Parent directory node for a newly added leaf. `Some` from the directory
    /// walk (the directory being walked), which is correct even across a
    /// link/layer boundary, from a single-file path whose walk found or added
    /// the directories leading to it, and from a named directory that replaces a
    /// file, the file's parent; `None` otherwise, resolved by path.
    parent_node_id: Option<NodeID>,
    /// When true, set Dirty on modified files and clear Dirty on retained files inline.
    intent: FilesystemDiffIntent,
    /// The filter's verdict for the path this context describes, carried into
    /// the changes it emits so a hierarchy walk below one does not fold again.
    states: FilterStates,
    /// What the walk measured at the path, which a node it creates records rather than
    /// reading the path a second time.
    observed: FileInfo,
    /// Settle the node whatever the comparison says. Force asks for it, and a merge needs
    /// it to carry its own flags onto a node the working copy already holds.
    insists: bool,
}

impl FileDiffContext {
    /// The 'from' side of a change, at `path`.
    fn create_from_change_state(&self, path: RelativePath) -> NodeChangeState {
        NodeChangeState {
            mapping: NodeMapping {
                repository: self.repository_from.clone(),
                state: self.state_from.clone(),
                path,
                node: self.from_node_id,
            },
            observed: None,
            flags: self
                .from_node
                .map_or(NodeFlags::NoFlags, |n| NodeFlags::from_bits_retain(n.flags)),
            address: self.from_node.map_or_else(Address::default, |n| n.address),
            mode: self.from_node.map_or(0, |n| n.mode),
        }
    }

    /// The side a change does not have, standing at `path` and holding no node there.
    fn invalid_change_state(&self, path: RelativePath) -> NodeChangeState {
        NodeChangeState {
            mapping: NodeMapping {
                repository: self.repository_from.clone(),
                state: self.state_from.clone(),
                path,
                node: INVALID_NODE,
            },
            observed: None,
            flags: NodeFlags::NoFlags,
            address: Address::default(),
            mode: 0,
        }
    }

    /// A new file at `path`, which the file system holds and the state does not.
    fn new_file_change_state(&self, path: RelativePath) -> NodeChangeState {
        NodeChangeState {
            mapping: NodeMapping {
                repository: self.repository_from.clone(),
                state: self.state_from.clone(),
                path,
                node: INVALID_NODE,
            },
            observed: Some(self.observed),
            flags: NodeFlags::File,
            address: Address::default(),
            mode: 0,
        }
    }

    /// A new directory at `path`, which the file system holds and the state does not.
    fn new_directory_change_state(&self, path: RelativePath) -> NodeChangeState {
        NodeChangeState {
            mapping: NodeMapping {
                repository: self.repository_from.clone(),
                state: self.state_from.clone(),
                path,
                node: INVALID_NODE,
            },
            observed: Some(self.observed),
            flags: NodeFlags::NoFlags,
            address: Address::default(),
            mode: 0,
        }
    }

    /// The `to` side of a change for a node this context added, carrying the flags it
    /// settled with rather than the ones it was created with.
    async fn settled_change_state(
        &self,
        node_id: NodeID,
        path: RelativePath,
    ) -> Result<NodeChangeState, StateError> {
        let node = self
            .state_from
            .node(self.repository_from.clone(), node_id)
            .await?;
        Ok(NodeChangeState {
            mapping: NodeMapping {
                repository: self.repository_from.clone(),
                state: self.state_from.clone(),
                path,
                node: node_id,
            },
            observed: None,
            flags: NodeFlags::from_bits_retain(node.flags),
            address: node.address,
            mode: node.mode,
        })
    }

    /// The node a child is added under: the directory the walk supplied, which is right
    /// across a link or layer boundary too, and the path's own parent otherwise, which
    /// discovery created before reaching here.
    async fn parent_node(&self, file_path: &RelativePath) -> Result<NodeID, StateError> {
        if let Some(parent) = self.parent_node_id {
            return Ok(parent);
        }
        match file_path.parent() {
            Some(parent) if !parent.is_empty() => Ok(self
                .state_from
                .find_node_link(self.repository_from.clone(), parent)
                .await
                .forward::<StateError>("scan add: parent directory node missing for nested add")?
                .node),
            _ => Ok(ROOT_NODE),
        }
    }

    /// Add the node a path new to the tree takes and settle it as an add, carrying what the
    /// walk measured where the path is a file.
    ///
    /// The parent takes the base dirty bit alone: the add belongs to the child.
    async fn add_new_node(
        &self,
        file_path: &RelativePath,
        is_directory: bool,
    ) -> Result<NodeID, StateError> {
        let parent_node_id = self.parent_node(file_path).await?;
        let mut flags = NodeFlags::DirtyAdd;
        if !is_directory {
            flags |= NodeFlags::File;
        }
        let mut node = Node {
            flags: flags.bits(),
            name_hash: crate::hash::hash_string(file_path.name()),
            ..Default::default()
        };
        let staging = self.intent.stage();
        if let Some(stage) = staging
            && !is_directory
        {
            node.mode = self.observed.mode(0);
            node.size = self.observed.size();
            node.address.context = stage.file_id.unwrap_or_else(|| uuid::Uuid::now_v7().into());
        }

        let node_id = self
            .state_from
            .node_add(
                self.repository_from.clone(),
                parent_node_id,
                node,
                file_path.name(),
            )
            .await
            .unwrap_or(INVALID_NODE);
        if staging.is_some() {
            mark_settled(
                &self.state_from,
                &self.repository_from,
                node_id,
                SettledAction::Add,
                self.intent,
            )
            .await?;
        }

        let _ = self
            .state_from
            .node_mark(
                &self.repository_from,
                parent_node_id,
                Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
            )
            .await;
        Ok(node_id)
    }
}

/// What the walk settled on for a node, which it records as a dirty action and, where it
/// stages, as the staged action standing for the same change.
#[lore_macro::test_pub]
#[derive(Debug, Clone, Copy)]
enum SettledAction {
    Add,
    Modify,
    Move,
    Delete,
}

impl SettledAction {
    /// The dirty action every marking walk records.
    #[lore_macro::test_pub]
    fn dirty(self) -> NodeFlags {
        match self {
            SettledAction::Add => NodeFlags::DirtyAdd,
            SettledAction::Modify => NodeFlags::DirtyModify,
            SettledAction::Move => NodeFlags::DirtyMove,
            SettledAction::Delete => NodeFlags::DirtyDelete,
        }
    }

    /// The staged action a staging walk records beside it.
    #[lore_macro::test_pub]
    fn staged(self) -> NodeFlags {
        match self {
            SettledAction::Add => NodeFlags::StagedAdd,
            SettledAction::Modify => NodeFlags::StagedModify,
            SettledAction::Move => NodeFlags::StagedMove,
            SettledAction::Delete => NodeFlags::StagedDelete,
        }
    }
}

/// Record `action` for `node_id`, propagating to its ancestors.
///
/// A caller that skips this because the dirty action is already recorded has to make an
/// exception for a staging intent, which records the staged action the node does not yet
/// carry.
///
/// A staging intent records the staged action in the mark's staged half, which keeps it in
/// the staged bits; the dirty half masks to the dirty bits and would drop it. The mark is the
/// one a stage records ([`crate::stage::staged_change`]).
///
/// Not an `async fn`, which would hold its arguments beside the mark's future.
fn mark_settled<'a>(
    state: &'a Arc<State>,
    repository: &'a Arc<RepositoryContext>,
    node_id: NodeID,
    action: SettledAction,
    intent: FilesystemDiffIntent,
) -> impl Future<Output = Result<(), StateError>> + Send + 'a {
    let mark = match intent.stage() {
        None => Mark::dirty(action.dirty(), Ext::Set(NodeExtFlags::NoFlags), true),
        Some(stage) => {
            crate::stage::staged_change(action.staged(), action.dirty(), stage.node_flags)
        }
    };
    state.node_mark(repository, node_id, mark)
}

/// Returns whether the child `name` of the directory `parent` addresses holds
/// its own `.lore/`, making it a nested working copy. Such a working copy
/// bounds the parent's filesystem walk: its contents belong to it, not to the
/// parent, the way a nested `.git` bounds git. A legacy `.urc/` working copy is
/// not a boundary — the format predates nesting support and no client that
/// creates one is still in use.
///
/// `parent` holds the absolute path of the directory being walked and is
/// restored before returning, so building a candidate costs no allocation once
/// the buffer has grown. The driver copies the path it is handed, so passing a
/// borrow leaves one allocation per candidate — handing over an owned path
/// built per candidate instead costs the same copy plus the build.
pub(crate) async fn is_nested_repository_root(parent: &mut std::path::PathBuf, name: &str) -> bool {
    parent.push(name);
    parent.push(DOT_LORE);
    let nested = lore_io::IoDriver::global()
        .stat(parent.as_path())
        .await
        .is_ok_and(|stat| stat.kind == lore_io::StatKind::Directory);
    parent.pop();
    parent.pop();
    nested
}

/// What a staging walk does with a node the tree holds and the file system still has at its
/// path, decided before the content is compared.
#[derive(Debug, Clone, Copy, PartialEq)]
enum StagedEntry {
    /// Compare the content and settle what that says, as a marking walk does. `insisted`
    /// settles a modification even where the content matches, which is what force asks for
    /// and what carries a merge's own flags onto a node the working copy already holds.
    Compare { insisted: bool },
    /// Nothing to settle: the node already carries a staged action and neither force nor a
    /// merge asks for it again.
    Settled,
    /// The node was staged for delete and the file system still holds the path, so the
    /// delete is taken back and the node settled as a modification, reported as the add it
    /// is from the tree's side.
    Undeleted,
}

/// Budget for subtree tasks live at once. Process-wide because every directory in the walk
/// owns its own [`JoinSet`].
static DIFF_FILESYSTEM_TASK_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// The files flagged staged in a subtree, as far as [`count_staged_files`] read it.
#[derive(Clone, Copy, Debug)]
pub struct StagedFileCount {
    /// The staged files counted.
    pub count: u64,
    /// Whether every directory of the subtree was read: where one wasn't, `count` leaves out the
    /// staged files it holds.
    pub complete: bool,
}

/// Count the files flagged staged in the subtree rooted at `node_id`.
///
/// An unreadable subtree is reported and counted as zero rather than raised:
/// callers use this to describe staged work, and failing the whole operation
/// because one directory would not load is worse than under-reporting it. The
/// count is then not `complete`, for a caller that must not take it as the
/// whole subtree's.
pub async fn count_staged_files(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
) -> StagedFileCount {
    let mut iter =
        match StateNodeChildrenIterator::new(state.clone(), repository.clone(), node_id).await {
            Ok(iter) => iter,
            Err(err) => {
                lore_warn!("Failed to iterate children for staged file count: {err}");
                return StagedFileCount {
                    count: 0,
                    complete: false,
                };
            }
        };

    let mut counted = StagedFileCount {
        count: 0,
        complete: true,
    };
    loop {
        let (child_id, child_node) = match iter.next().await {
            Ok(Some(child)) => child,
            Ok(None) => break,
            Err(_) => {
                counted.complete = false;
                break;
            }
        };
        if !child_node.is_staged() {
            continue;
        }
        if child_node.is_file() {
            counted.count += 1;
        } else if child_node.is_directory() {
            let below = Box::pin(count_staged_files(
                repository.clone(),
                state.clone(),
                child_id,
            ))
            .await;
            counted.count += below.count;
            counted.complete &= below.complete;
        }
    }

    counted
}

// TODO(UCS-13059): Extend with file mode check
/// Outcome of comparing a file on disk to the content a node addresses.
#[derive(Debug)]
pub enum NodeComparison {
    /// The file holds the node's content.
    Matches,
    /// The file differs from the node, or the stored object could not be described or walked
    /// well enough to tell. Nothing was established in the latter case, and treating it as a
    /// match would let a real local change be overwritten.
    Differs,
    /// The file could not be read, which a scan running alongside a branch switch sees
    /// whenever one is deleted under it.
    Unreadable,
}

/// Whether the working tree holds the content `node` addresses.
///
/// A size of its own settles it without asking anything further, as does anything a previous
/// comparison against the same file already established. What neither answers is asked of
/// `operation`, and how it reaches the answer is its own business.
///
/// `established` is what those previous comparisons settled. A caller measuring one file against
/// several nodes passes one across them and the file is read no more than the answers require; a
/// caller asking once passes a fresh one.
///
/// Reads no recorded modification time and records none: a recorded time speaks for the current
/// revision's node, and this answers about any node. Reads the operation's clock floor
/// ([`InstanceOperationImpl::clock_floor`]) before reading the file, so a time recorded from what
/// the read establishes is compared against a clock read before it.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn file_matches_node<'a>(
    repository: Arc<RepositoryContext>,
    node: &'a Node,
    file_size: u64,
    file_path: &'a impl WalkPath,
    operation: &'a InstanceOperationImpl,
    established: &'a lore_storage::ContentHashes,
) -> impl Future<Output = Result<NodeComparison, StateError>> + 'a {
    async move {
        if file_size != node.size {
            lore_trace!("File {file_path} size differs from node, differs");
            return Ok(NodeComparison::Differs);
        }

        if let Some(settled) = established.decides(node.address, Some(node.size), file_size) {
            lore_trace!("File {file_path} settled by an earlier comparison: {settled:?}");
            return Ok(node_comparison(settled));
        }

        operation.clock_floor(&repository).await;
        let comparison = operation
            .file_holds_content(
                repository,
                &file_path.to_path(),
                node.address,
                node.size,
                established,
            )
            .await
            .forward_any::<StateError>("Failed to compare the file to the node")?;
        lore_trace!("File {file_path} compared to stored content: {comparison:?}");
        Ok(comparison)
    }
}

/// How a storage comparison reads as a node comparison.
///
/// A comparison that settled nothing reads as differing: treating it as a match would let a real
/// local change be overwritten.
pub fn node_comparison(matched: lore_storage::FileMatch) -> NodeComparison {
    match matched {
        lore_storage::FileMatch::Match => NodeComparison::Matches,
        lore_storage::FileMatch::Differs | lore_storage::FileMatch::Indeterminate => {
            NodeComparison::Differs
        }
        lore_storage::FileMatch::Unreadable => NodeComparison::Unreadable,
    }
}

/// How a file compared to the node it was measured against, and what answered it.
pub enum FileModification {
    /// The recorded modified time vouched for the file, which was never read.
    UnmodifiedByMtime,
    /// A hash check established that the file holds the node's content.
    UnmodifiedByHash,
    /// The file could not be read, which a scan running alongside a branch switch sees
    /// whenever one is deleted under it. Reported unmodified so a routine deletion does not
    /// look like a local change, and never recorded: nothing was established.
    Unreadable,
    /// The size differs from the node's, which settles it without a hash check.
    ModifiedBySize,
    /// A hash check established that the file differs from the node.
    ModifiedByHash,
}

/// What settled a file comparison, which is what the size and hash counters report.
pub enum ComparisonAnswer {
    /// The size differed from the node's, settling it without reading either the recorded
    /// time or the file.
    Size,
    /// A recorded modified time vouched for the file, which was never read.
    Mtime,
    /// The file had to be read to answer, whether or not the read succeeded.
    Hash,
}

impl FileModification {
    /// Whether the file differs from the node.
    pub fn is_modified(&self) -> bool {
        matches!(
            self,
            FileModification::ModifiedBySize | FileModification::ModifiedByHash
        )
    }

    /// What settled the comparison.
    pub fn answered_by(&self) -> ComparisonAnswer {
        match self {
            FileModification::ModifiedBySize => ComparisonAnswer::Size,
            FileModification::UnmodifiedByMtime => ComparisonAnswer::Mtime,
            FileModification::UnmodifiedByHash
            | FileModification::ModifiedByHash
            | FileModification::Unreadable => ComparisonAnswer::Hash,
        }
    }
}

/// How the file on disk compares to the content `node` addresses.
///
/// Size and the recorded modification time answer first where they can, and comparing the
/// content answers the rest. A recorded time speaks for the node the current revision holds
/// and no other, so a caller asking about a different node sets `force_check_hash`, or else
/// knows that no time can have been recorded for the path.
///
/// Records nothing. Whether an observed match is worth recording depends on which node was
/// asked about, which only the caller knows.
#[allow(clippy::too_many_arguments)]
pub async fn file_modification(
    repository: Arc<RepositoryContext>,
    node: &Node,
    file_mtime: u64,
    file_size: u64,
    file_path: &impl WalkPath,
    force_check_hash: bool,
    operation: &InstanceOperationImpl,
    established: &lore_storage::ContentHashes,
) -> Result<FileModification, StateError> {
    // Assume files are identical if size and timestamp match
    let node_size = node.size;
    if file_size != node_size {
        lore_trace!("File {file_path} size changed, modified");
        return Ok(FileModification::ModifiedBySize);
    }

    let node_mtime = if !force_check_hash {
        file_modified_time(repository.clone(), file_path).await
    } else {
        0
    };
    if file_mtime == node_mtime {
        lore_trace!("File {file_path} unmodified, size {file_size} and mtime {file_mtime} match");
        return Ok(FileModification::UnmodifiedByMtime);
    }

    lore_trace!(
        "Hash check file {file_path} - file size {file_size} node size {node_size}, file mtime {file_mtime}, node mtime {node_mtime}, force {force_check_hash}"
    );

    Ok(
        match file_matches_node(
            repository,
            node,
            file_size,
            file_path,
            operation,
            established,
        )
        .await?
        {
            NodeComparison::Matches => FileModification::UnmodifiedByHash,
            NodeComparison::Differs => FileModification::ModifiedByHash,
            NodeComparison::Unreadable => FileModification::Unreadable,
        },
    )
}

/// How the file on disk compares to `node`, recording the modified time when a hash check is
/// what established the match.
///
/// `node_is_current` states that `node` is the one the current revision holds at this path,
/// and gates both halves: a recorded time speaks for that node alone, so it can neither
/// answer for any other node nor be written from a match against one. Recording here is what
/// spares the next scan the hash check this one just paid for. A time not older than the
/// filesystem clock the operation read before reading the file is not recorded: a write after
/// the read may share it ([`InstanceOperationImpl::clock_floor`]).
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
pub fn file_modified_against_node<'a>(
    repository: Arc<RepositoryContext>,
    node: &'a Node,
    file_mtime: u64,
    file_size: u64,
    file_path: &'a impl WalkPath,
    node_is_current: bool,
    operation: &'a InstanceOperationImpl,
    established: &'a lore_storage::ContentHashes,
) -> impl Future<Output = Result<FileModification, StateError>> + 'a {
    async move {
        let modification = file_modification(
            repository.clone(),
            node,
            file_mtime,
            file_size,
            file_path,
            !node_is_current,
            operation,
            established,
        )
        .await?;

        if node_is_current
            && matches!(modification, FileModification::UnmodifiedByHash)
            && file_mtime < operation.clock_floor(&repository).await
        {
            file_modified_time_store(repository, file_path, file_mtime).await;
        }

        Ok(modification)
    }
}

/// Modified times collected by an operation, to be recorded once it has completed.
///
/// An entry states that a path held the current revision's content at that time. Recording
/// as each file is handled would publish that before it is true and, worse, would vouch for
/// a time the next write can still share, so entries are collected and written at the end by
/// [`store`](Self::store). An operation that leaves the current revision elsewhere calls
/// [`discard`](Self::discard).
///
/// A collector made by [`below`](Self::below) collects no time it cannot tell apart from a later
/// write: none not older than the filesystem clock read before reading any file whose time it
/// collects.
#[must_use]
#[lore_macro::test_pub]
pub struct RecordedModifiedTimes {
    /// Times for the instance's records.
    records: Box<crossbeam_queue::SegQueue<Record>>,
    /// The mutable store keys of paths in `records` whose store records an older client may
    /// have written, to be zeroed.
    replaced: Box<crossbeam_queue::SegQueue<Hash>>,
    /// Times for the mutable store, for an instance that keeps its records there.
    store: Box<crossbeam_queue::SegQueue<(Hash, u64)>>,
    /// Times at or after this are not collected.
    floor: u64,
}

impl Default for RecordedModifiedTimes {
    fn default() -> Self {
        Self::below(u64::MAX)
    }
}

impl RecordedModifiedTimes {
    /// A collector of the times older than `floor`, the filesystem clock read before reading any
    /// file whose time it collects ([`read_clock_floor`]).
    pub fn below(floor: u64) -> Self {
        Self {
            records: Box::default(),
            replaced: Box::default(),
            store: Box::default(),
            floor,
        }
    }

    /// Collects the entry recording that `path` held `repository`'s current content at
    /// `mtime`, where the time is older than the collector's floor.
    pub fn record(&self, repository: &RepositoryContext, path: &RelativePath, mtime: u64) {
        if mtime < self.floor {
            self.push(file_modified_time_entry(repository, path, mtime));
        }
    }

    /// Writes the collected times into the instance's records, or into `repository`'s mutable
    /// store where the instance keeps them there.
    ///
    /// The records are set in one pass per shard and written back with the context's flush.
    /// Alongside, the store's record of each path that replaces one an older client may have
    /// written is zeroed where it holds a time, so that no client reads a time the store kept
    /// for content the path no longer holds. The store work runs one task per store group: the
    /// store takes its write lock within the group a key's first byte selects, so splitting the
    /// keys by that byte lets every task run without ever contending with another.
    ///
    /// Without a floor, waits for the filesystem to stamp later than every time written, so that
    /// none of them can be shared by a write that follows — bounded, so a filesystem that does not
    /// appear to move on is left with times a following write can still share rather than being
    /// waited on forever. See [`wait_until_settled`]. A collector with a floor holds only times
    /// older than a stamp the filesystem has already given, so it has nothing to wait for.
    pub async fn store(&self, repository: Arc<RepositoryContext>) {
        let store = match repository.try_mutable_store_arc() {
            Some(store) if !execution_context().globals().dry_run() => store,
            _ => {
                self.discard();
                return;
            }
        };

        let mut mtime_max = 0;
        let mut tasks = JoinSet::new();
        if !self.store.is_empty() {
            let mut groups = vec![Vec::new(); lore_storage::local::mutable_store::GROUP_COUNT];
            while let Some((key, mtime)) = self.store.pop() {
                mtime_max = mtime_max.max(mtime);
                groups[key.data()[0] as usize].push((key, mtime));
            }
            for items in groups {
                if items.is_empty() {
                    continue;
                }
                // Each task holds the context, and with it the command's claims on the store, so
                // that none outlives them when this is dropped part way and its tasks run on to
                // their next await.
                lore_spawn!(tasks, {
                    let store = store.clone();
                    let repository = repository.clone();
                    async move {
                        file_modified_time_store_group(store, repository.root_id(), items).await;
                    }
                });
            }
        }
        if !self.replaced.is_empty() {
            let mut groups = vec![Vec::new(); lore_storage::local::mutable_store::GROUP_COUNT];
            while let Some(key) = self.replaced.pop() {
                groups[key.data()[0] as usize].push(key);
            }
            for keys in groups {
                if !keys.is_empty() {
                    lore_spawn!(
                        tasks,
                        zero_store_modified_time_group(repository.clone(), keys)
                    );
                }
            }
        }
        drop(store);

        if let Some(records) = repository.instance_records()
            && !self.records.is_empty()
        {
            let mut batch = RecordBatch::with_capacity(self.records.len());
            while let Some(record) = self.records.pop() {
                mtime_max = mtime_max.max(record.mtime());
                batch.push(record);
            }
            if let Err(err) = records.set_all(batch).await {
                lore_warn!("Failed to record modified times: {err}");
            }
        }
        while let Some(joined) = tasks.join_next().await {
            if let Err(err) = joined {
                lore_warn!("Failed to store modified times: {err}");
            }
        }

        if self.floor == u64::MAX && mtime_max != 0 {
            wait_until_settled(&repository, mtime_max).await;
        }
    }

    /// Collects an entry built by [`file_modified_time_entry`], for a caller that computed
    /// the key where it had the path rather than where it records, where its time is older than
    /// the collector's floor.
    pub fn push(&self, entry: ModifiedTimeEntry) {
        match entry {
            ModifiedTimeEntry::Record(record) if record.mtime() < self.floor => {
                self.records.push(record);
            }
            ModifiedTimeEntry::Replace(record, key) if record.mtime() < self.floor => {
                self.records.push(record);
                self.replaced.push(key);
            }
            ModifiedTimeEntry::Store(key, mtime) if mtime < self.floor => {
                self.store.push((key, mtime));
            }
            _ => {}
        }
    }

    /// Moves the times collected by `other` into this collector.
    pub fn absorb(&self, other: Self) {
        self.move_from(&other);
    }

    /// Removes the times collected so far, leaving the collector empty.
    pub fn take(&self) -> Self {
        let taken = Self::default();
        taken.move_from(self);
        taken
    }

    /// Drops the times, for an operation that leaves the current revision elsewhere.
    pub fn discard(&self) {
        while self.records.pop().is_some() {}
        while self.replaced.pop().is_some() {}
        while self.store.pop().is_some() {}
    }

    /// Moves the times `other` collected into this collector, where they are older than its
    /// floor.
    fn move_from(&self, other: &Self) {
        while let Some(record) = other.records.pop() {
            if record.mtime() < self.floor {
                self.records.push(record);
            }
        }
        while let Some(key) = other.replaced.pop() {
            self.replaced.push(key);
        }
        while let Some((key, mtime)) = other.store.pop() {
            if mtime < self.floor {
                self.store.push((key, mtime));
            }
        }
    }
}

/// Name of the file stamped to read the working copy's own clock.
const MODIFIED_TIME_PROBE: &str = "mtime-probe";

/// The whole time [`wait_until_settled`] may spend on the filesystem's clock.
///
/// A filesystem whose stamps do not advance — one keeping a clock coarser than the tick the
/// wait assumes, one handing out a single time for the whole run, or one failing every probe —
/// would otherwise hold the operation open indefinitely. Giving up leaves the recorded times
/// unable to tell a following write apart, the same position a working copy that cannot be
/// stamped at all is in.
const MODIFIED_TIME_SETTLE_LIMIT: std::time::Duration = std::time::Duration::from_millis(10);

/// The path of the file stamped to read the working copy's clock, `None` when the working copy
/// has no path to stamp.
#[lore_macro::test_pub]
fn modified_time_probe_path(repository: &RepositoryContext) -> Option<std::path::PathBuf> {
    Some(repository.dot_dir_path().ok()?.join(MODIFIED_TIME_PROBE))
}

/// Stamps the probe at `path` and reads back the time the filesystem gave it.
///
/// The metadata a write returns is taken while the file is still open, which on a filesystem
/// that settles the stamp when the handle closes is not the time the file ends up carrying.
/// The write closes the file before it completes, so reading the time back in a call of its
/// own asks about the state a later scan will see.
///
/// `None` when either call fails: a probe that could not be written or read says nothing about
/// the filesystem's clock, which is the same answer as a working copy that cannot be stamped.
async fn stamp_probe(path: &std::path::Path) -> Option<u64> {
    let driver = lore_io::IoDriver::global();
    driver
        .write_file_bytes(path, bytes::Bytes::from_static(&[0u8]), false)
        .await
        .ok()?;
    let modified = driver.stat(path).await.ok()?.modified;
    Some(crate::util::fs::modified_millis(modified))
}

/// [`filesystem_stamp_now`], as the floor below which an operation records times: a file
/// written from now on carries a time no older. 0 where the working copy cannot be stamped, and
/// in a dry run, which records nothing: no time is older, so none is recorded.
#[lore_macro::test_pub]
pub(crate) async fn read_clock_floor(repository: &RepositoryContext) -> u64 {
    if crate::runtime::try_execution_context().is_some_and(|context| context.globals().dry_run()) {
        return 0;
    }
    filesystem_stamp_now(repository).await.unwrap_or(0)
}

/// The time the working copy's filesystem is currently stamping writes with.
///
/// A file's modified time comes from a clock the filesystem advances on its own schedule and
/// at its own resolution, which the process clock runs ahead of. Comparing a recorded time
/// against [`std::time::SystemTime::now`] therefore always finds it older, however recently
/// the file was written. Stamping a file and reading it back asks the filesystem instead, so
/// the answer is on the scale the comparison needs.
///
/// The probe is removed once read, so a stamp file is not left behind in the dot directory.
///
/// `None` when the working copy cannot be stamped, which leaves a caller no way to tell a
/// settled time from one a further write can still share.
pub async fn filesystem_stamp_now(repository: &RepositoryContext) -> Option<u64> {
    stamp_and_remove_probe(&modified_time_probe_path(repository)?).await
}

/// [`stamp_probe`], then removes the probe at `path`.
async fn stamp_and_remove_probe(path: &std::path::Path) -> Option<u64> {
    let stamp = stamp_probe(path).await;
    let _ = lore_io::IoDriver::global().remove_file(path).await;
    stamp
}

/// A time older than the modified time of any file written from now on to the filesystem that
/// holds `dir`: one millisecond before the stamp a probe written in `dir` gets. The probe is
/// removed once read.
///
/// The process clock where `dir` is `None` or cannot be stamped. A file written within the
/// filesystem's clock tick can then carry the time returned.
#[lore_macro::test_pub]
pub(crate) async fn time_before_next_write(dir: Option<&std::path::Path>) -> u64 {
    let stamp = match dir {
        Some(dir) => stamp_and_remove_probe(&dir.join(MODIFIED_TIME_PROBE)).await,
        None => None,
    };
    match stamp {
        Some(stamp) => stamp.saturating_sub(1),
        None => crate::util::fs::modified_millis(None),
    }
}

/// Waits until the working copy's filesystem stamps later than `mtime_max`.
///
/// A file carrying the stamp the filesystem is still handing out can be written again without
/// that stamp changing, so a time recorded for it cannot tell the two states apart. Holding
/// the operation open until the filesystem has moved past every time it recorded leaves all
/// of them able to, at the cost of at most the tick the filesystem is currently in — and never
/// more than [`MODIFIED_TIME_SETTLE_LIMIT`], which is the ceiling on the whole wait rather than
/// on the sleeps alone, so a probe that hangs cannot hold the operation either.
///
/// A probe that failed says nothing about the clock — least of all that it has moved on — so
/// the wait asks again rather than reading the failure as settled, and the limit is what ends a
/// wait that cannot get an answer.
///
/// The probe is removed on the way out however the wait ended, so a stamp file is not left
/// behind in the dot directory.
///
/// Returns without waiting when the working copy cannot be stamped, which leaves no way to
/// tell whether the times have settled.
pub async fn wait_until_settled(repository: &RepositoryContext, mtime_max: u64) {
    let Some(path) = modified_time_probe_path(repository) else {
        return;
    };
    let _ = tokio::time::timeout(MODIFIED_TIME_SETTLE_LIMIT, async {
        while stamp_probe(&path)
            .await
            .is_none_or(|stamp| mtime_max >= stamp)
        {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await;
    let _ = lore_io::IoDriver::global().remove_file(&path).await;
}

/// The key a file's modification time is stored under in the mutable store.
///
/// The store is case-insensitive over paths, and [`RelativePath`] already carries
/// the fold, so the key is taken from it rather than folded again. Folding here
/// would allocate a `String` per file, half a million of them in one scan of a
/// large tree.
pub fn file_modified_time_key(salt: &[u8], instance: InstanceId, path: &impl FilterPath) -> Hash {
    hash::hash_function_args_slice(
        salt,
        FILE_MTIME,
        instance.data(),
        path.as_lowercase_str().as_bytes(),
    )
}

/// The key a file's modification time is filed under in the instance's records: a hash of the
/// repository's salt and the lowercased path, and not the instance, so the records in a copy of
/// the dot directory describe the copy's files.
pub fn file_modified_time_record_key(salt: &[u8], path: &impl FilterPath) -> RecordKey {
    InstanceRecords::key(&hash::hash_function_args_slice(
        salt,
        FILE_MTIME,
        &[],
        path.as_lowercase_str().as_bytes(),
    ))
}

/// A time recorded for a path, keyed for where the instance keeps its records.
#[derive(Debug, Clone, Copy)]
pub enum ModifiedTimeEntry {
    /// For the instance's records.
    Record(Record),
    /// For the instance's records, with the key of the path's record in the mutable store, which
    /// an older client may have written and which is zeroed.
    Replace(Record, Hash),
    /// For the mutable store: the key and the time.
    Store(Hash, u64),
}

/// The entry recording that `path` held the current revision's content at `mtime`, for a
/// caller collecting entries to write in one batch rather than one at a time.
pub fn file_modified_time_entry(
    repository: &RepositoryContext,
    path: &RelativePath,
    mtime: u64,
) -> ModifiedTimeEntry {
    let Some(records) = repository.instance_records() else {
        return ModifiedTimeEntry::Store(
            file_modified_time_key(repository.salt(), repository.instance_id, path),
            mtime,
        );
    };
    let record = Record::new(
        file_modified_time_record_key(repository.salt(), path),
        mtime,
    );
    if records.has_store_records() {
        ModifiedTimeEntry::Replace(
            record,
            file_modified_time_key(repository.salt(), repository.instance_id, path),
        )
    } else {
        ModifiedTimeEntry::Record(record)
    }
}

/// The time the mutable store holds for `path`, 0 where it holds none.
async fn store_modified_time(repository: &RepositoryContext, path: &impl WalkPath) -> u64 {
    let key = file_modified_time_key(repository.salt(), repository.instance_id, path);
    stored_modified_time(repository, key).await
}

/// The modification time stored under `key` in `repository`'s mutable store, 0 for none.
pub(crate) async fn stored_modified_time(repository: &RepositoryContext, key: Hash) -> u64 {
    match repository
        .read_mutable_store()
        .load(repository.root_id(), key, KeyType::Untyped)
        .await
    {
        Ok(value) => u64::from_ne_bytes(
            value.data()[..size_of::<u64>()]
                .try_into()
                .unwrap_or_default(),
        ),
        Err(_) => 0,
    }
}

/// The time the instance's records hold for `path`. Where the store can hold an older client's
/// records of the instance, a path with no record falls back to the time the store holds, which
/// is copied into the instance's records and left in the store, since it still describes the
/// path's content.
async fn instance_modified_time(
    repository: &RepositoryContext,
    records: &InstanceRecords,
    path: &impl WalkPath,
) -> u64 {
    let key = file_modified_time_record_key(repository.salt(), path);
    match records.lookup(&key).await {
        Lookup::Recorded(mtime) => mtime,
        Lookup::Missing(generation) if records.has_store_records() => {
            adopt_store_modified_time(repository, records, path, key, generation).await
        }
        Lookup::Missing(_) => 0,
    }
}

/// The time the mutable store holds for `path`, copied into the instance's records under `key`
/// unless they have changed since the lookup that returned `generation`.
async fn adopt_store_modified_time(
    repository: &RepositoryContext,
    records: &InstanceRecords,
    path: &impl WalkPath,
    key: RecordKey,
    generation: u64,
) -> u64 {
    let mtime = store_modified_time(repository, path).await;
    if mtime != 0
        && !crate::runtime::try_execution_context()
            .is_some_and(|context| context.globals().dry_run())
    {
        records.adopt(key, mtime, generation).await;
    }
    mtime
}

pub async fn file_modified_time(repository: Arc<RepositoryContext>, path: &impl WalkPath) -> u64 {
    let mtime = match repository.instance_records() {
        Some(records) => instance_modified_time(&repository, records, path).await,
        None => store_modified_time(&repository, path).await,
    };
    lore_trace!("Load mtime {mtime} for {path}");
    mtime
}

/// Zeroes the mutable store's record under `key` where it holds one, for an instance that keeps
/// its own records. A store record left beside a newer one would describe content the path no
/// longer holds to a client that reads only the store.
async fn zero_store_modified_time(
    store: &crate::store::handles::WriteHandle<'_>,
    partition: RepositoryId,
    key: Hash,
) {
    if store
        .load(partition, key, KeyType::Untyped)
        .await
        .is_ok_and(|value| !value.is_zero())
    {
        let _ = store
            .store(partition, key, Hash::default(), KeyType::Untyped)
            .await;
    }
}

/// Records that `path` held the current revision's content at `mtime`.
///
/// A dry run records nothing: it leaves the current revision where it was, so no time it
/// takes describes the revision the working copy is on.
pub async fn file_modified_time_store(
    repository: Arc<RepositoryContext>,
    path: &impl WalkPath,
    mtime: u64,
) {
    lore_trace!("Store mtime {mtime} for {path}");
    if execution_context().globals().dry_run() {
        return;
    }
    let Some(handle) = repository.try_write_mutable_store() else {
        return;
    };
    let Some(records) = repository.instance_records() else {
        let key = file_modified_time_key(repository.salt(), repository.instance_id, path);
        let _ = handle
            .store(
                repository.root_id(),
                key,
                Hash::from_u64(mtime),
                KeyType::Untyped,
            )
            .await;
        return;
    };
    records
        .set(
            file_modified_time_record_key(repository.salt(), path),
            mtime,
        )
        .await;
    if records.has_store_records() {
        let key = file_modified_time_key(repository.salt(), repository.instance_id, path);
        zero_store_modified_time(&handle, repository.root_id(), key).await;
    }
}

/// Writes one store group's worth of pre-computed `(mtime_key, mtime)` pairs.
///
/// Every entry belongs to the same group, so the calls contend with no other group's task.
async fn file_modified_time_store_group(
    store: Arc<dyn crate::store::MutableStore>,
    partition: RepositoryId,
    items: Vec<(Hash, u64)>,
) {
    for (key, mtime) in items {
        let _ = store
            .clone()
            .store(partition, key, Hash::from_u64(mtime), KeyType::Untyped)
            .await;
    }
}

/// Zeroes one store group's worth of the store records named by `keys` that hold a time, as
/// [`zero_store_modified_time`] does.
async fn zero_store_modified_time_group(repository: Arc<RepositoryContext>, keys: Vec<Hash>) {
    let Some(handle) = repository.try_write_mutable_store() else {
        return;
    };
    for key in keys {
        zero_store_modified_time(&handle, repository.root_id(), key).await;
    }
}

/// Removes the time recorded for `path`, from the instance's records and, where it can hold
/// one, from the mutable store.
pub async fn file_modified_time_clear(repository: Arc<RepositoryContext>, path: &RelativePath) {
    lore_trace!("Clear mtime for {path}");
    let Some(handle) = repository.try_write_mutable_store() else {
        return;
    };
    let Some(records) = repository.instance_records() else {
        let key = file_modified_time_key(repository.salt(), repository.instance_id, path);
        let _ = handle
            .store(repository.root_id(), key, Hash::default(), KeyType::Untyped)
            .await;
        return;
    };
    records
        .remove(&file_modified_time_record_key(repository.salt(), path))
        .await;
    if records.has_store_records() {
        let key = file_modified_time_key(repository.salt(), repository.instance_id, path);
        zero_store_modified_time(&handle, repository.root_id(), key).await;
    }
}

pub async fn verify_node_name_case(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node: NodeID,
) -> Result<(), StateError> {
    verify_node_name_case_impl(repository, state, node).await
}

fn verify_node_name_case_recurse(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node: NodeID,
) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send>> {
    Box::pin(verify_node_name_case_impl(repository, state, node))
}

async fn verify_node_name_case_impl(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node: NodeID,
) -> Result<(), StateError> {
    let nodes = state
        .collect_children_unsorted(
            repository.clone(),
            node,
            false, /* Do not include deleted nodes */
            true,  /* Traverse links */
        )
        .await?;
    if nodes.children.is_empty() {
        return Ok(());
    }

    for index in 0..(nodes.children.len() - 1) {
        let current_named_node = &nodes.children[index];
        let current_block = nodes
            .state
            .block_with_nametable(
                nodes.repository.clone(),
                NodeBlock::index(current_named_node.node),
            )
            .await?;
        let current_node = current_block.node(Node::index(current_named_node.node));
        if current_node.is_staged_delete() || current_node.is_discarded() {
            continue;
        }

        let first_path = nodes
            .state
            .node_path(nodes.repository.clone(), current_named_node.node)
            .await?;

        lore_trace!(
            "Check node name case for siblings of {} {}",
            first_path,
            current_named_node.node
        );

        for next_named_node in nodes.children.iter().skip(index + 1) {
            let next_block = nodes
                .state
                .block_with_nametable(
                    nodes.repository.clone(),
                    NodeBlock::index(next_named_node.node),
                )
                .await?;
            let next_node = next_block.node(Node::index(next_named_node.node));
            if next_node.is_staged_delete() || next_node.is_discarded() {
                continue;
            }

            //let next_name = state.node_name_direct(&next_node, &next_nametable);
            //let next_hash = hash_string_lowercase(next_name);

            if current_node.name_hash != next_node.name_hash {
                continue;
            }

            let second_path = nodes
                .state
                .node_path(nodes.repository.clone(), next_named_node.node)
                .await?;

            // TODO(mjansson): User input should be behind an --interactive command line/API option
            //                 or a flag to select automatic resolve behaviour. Revisit this once
            //                 structured output is in place. Potentially remove this healing code
            //                 path once name case variations cannot be created anymore.
            let selection = if current_named_node.name != next_named_node.name {
                println!(
                    "Node differ only by case:\n1) {} (node {})\n2) {} (node {})",
                    first_path, current_named_node.node, second_path, next_named_node.node
                );
                print!("Select which name to use (1 or 2, or anything else to abort)> ");
                let _ = std::io::stdout().flush();
                let mut input = String::new();
                let _ = std::io::stdin().read_line(&mut input);
                input.trim().to_string()
            } else {
                let option = if current_named_node.node < next_named_node.node {
                    "1".to_string()
                } else {
                    "2".to_string()
                };
                lore_warn!(
                    "Multiple nodes have identical name, unifying by selecting option {option}:\n1) {} (node {})\n2) {} (node {})",
                    first_path,
                    current_named_node.node,
                    second_path,
                    next_named_node.node
                );
                option
            };

            let mut keep_node = current_named_node;
            let mut delete_node = next_named_node;
            match selection.as_str() {
                "1" => {
                    // Use the already setup node combo
                }
                "2" => {
                    keep_node = next_named_node;
                    delete_node = current_named_node;
                }
                _ => {
                    println!("No option selected, aborting");
                    return Err(StateError::internal("Name case clash"));
                }
            }

            lore_trace!(
                "Keep {} node {:?}, delete node {:?}",
                second_path,
                keep_node,
                delete_node
            );

            let delete_path = if delete_node.node == next_named_node.node {
                &second_path
            } else {
                &first_path
            };
            stage_delete(
                nodes.repository.clone(),
                nodes.state.clone(),
                RelativePath::new_from_initial_path(delete_path).unwrap_or_default(),
                delete_node.node,
                NodeFlags::NoFlags,
                Arc::default(),
                None, // No link tracking in state verification
            )
            .await
            .forward::<StateError>("Verify delete")?;

            if delete_node.node == current_named_node.node {
                break;
            }
        }
    }

    for named_node in nodes.children.iter() {
        let node = nodes
            .state
            .node(nodes.repository.clone(), named_node.node)
            .await?;
        if node.is_directory() && !node.is_staged_delete() {
            lore_trace!(
                "Recurse check node name case for children of {}",
                named_node.node
            );
            verify_node_name_case_recurse(
                nodes.repository.clone(),
                nodes.state.clone(),
                named_node.node,
            )
            .await?;
        }
    }

    Ok(())
}

async fn collect_state_fragments(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
) -> Result<Vec<Address>, StateError> {
    let mut addresses = Vec::with_capacity(32);

    {
        let data = state.data_read();
        addresses.push(Address::zero_context_hash(data.hash_link));
        addresses.push(Address::zero_context_hash(data.hash_metadata));
        addresses.push(Address::zero_context_hash(data.hash_tree));
    }

    let tree = state.tree(repository.clone()).await?;
    addresses.push(Address::zero_context_hash(tree.hash_node));
    addresses.push(Address::zero_context_hash(tree.hash_file_metadata));
    addresses.push(Address::zero_context_hash(tree.hash_delta));

    if !tree.hash_nametable_deprecated.is_zero() {
        addresses.push(Address::zero_context_hash(tree.hash_nametable_deprecated));
    }

    addresses.push(Address::zero_context_hash(state.revision()));

    Ok(addresses)
}

async fn collect_node_blocks(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
) -> Result<Vec<Address>, StateError> {
    let mut addresses = Vec::with_capacity(32);

    let hash_node = state.tree(repository.clone()).await?.hash_node;
    if !hash_node.is_zero() {
        let block_address = Address::zero_context_hash(hash_node);
        let buffer = immutable::read(
            repository.clone(),
            block_address,
            None, /* Read the full array of block hashes */
            immutable::read_options_from_repository(&repository)
                .with_cache()
                .with_priority(),
        )
        .await
        .forward::<StateError>("Failed to deserialize node block list")?;

        let hash_slice = buffer.as_type_slice::<Hash>();
        addresses.reserve(hash_slice.len());
        for hash in hash_slice.iter() {
            addresses.push(Address::zero_context_hash(*hash));
        }
    }

    Ok(addresses)
}

async fn collect_file_metadata_blocks(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
) -> Result<Vec<Address>, StateError> {
    let mut addresses = Vec::with_capacity(32);

    let hash_file_metadata = state.tree(repository.clone()).await?.hash_file_metadata;
    if !hash_file_metadata.is_zero() {
        let block_address = Address::zero_context_hash(hash_file_metadata);
        let buffer = immutable::read(
            repository.clone(),
            block_address,
            None, /* Read the full array of block hashes */
            immutable::read_options_from_repository(&repository)
                .with_cache()
                .with_priority(),
        )
        .await
        .forward::<StateError>("Failed to deserialize node block list")?;

        let hash_slice = buffer.as_type_slice::<Hash>();
        addresses.reserve(hash_slice.len());
        for hash in hash_slice.iter() {
            addresses.push(Address::zero_context_hash(*hash));
        }
    }

    Ok(addresses)
}

async fn collect_name_fragments(
    repository: Arc<RepositoryContext>,
    blocks: Vec<Address>,
) -> Result<Vec<Address>, StateError> {
    let mut tasks = JoinSet::new();
    for address in blocks {
        lore_spawn!(tasks, {
            let repository = repository.clone();
            async move {
                if let Ok(block_data) =
                    NodeBlockData::read_box_from_immutable(repository.clone(), address, true).await
                {
                    Ok(block_data.name_table)
                } else {
                    // Make sure block can be read even though it has no local name table
                    let _block_data =
                        NodeBlockDataV0::read_box_from_immutable(repository.clone(), address, true)
                            .await
                            .forward::<StateError>("Failed to deserialize node block")?;
                    Ok(Hash::default())
                }
            }
        });
    }
    let mut name = Vec::with_capacity(tasks.len());
    let mut failure = None;
    while let Some(result) = tasks.join_next().await {
        match result
            .internal("Task failure")
            .map_err(StateError::from)
            .flatten()
        {
            Ok(hash) => {
                if !hash.is_zero() {
                    name.push(Address::zero_context_hash(hash));
                }
            }
            Err(err) => {
                failure = failure.or(Some(err));
            }
        }
    }

    if let Some(err) = failure {
        return Err(err);
    }

    Ok(name)
}

fn collect_diff_addresses(from: Vec<Address>, to: Vec<Address>) -> Vec<Address> {
    let mut new = Vec::with_capacity(to.len());
    let mut ifrom = 0;
    let mut ito = 0;
    while ifrom < from.len() && ito < to.len() {
        match from[ifrom].cmp(&to[ito]) {
            std::cmp::Ordering::Less => {
                ifrom += 1;
            }
            std::cmp::Ordering::Greater => {
                new.push(to[ito]);
                ito += 1;
            }
            std::cmp::Ordering::Equal => {
                ifrom += 1;
                ito += 1;
            }
        }
    }
    new.extend_from_slice(&to[ito..]);
    new
}

/// Which addresses a collection of the fragments a state adds keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collection {
    /// Every address, durable or not, as for the state a diff starts from.
    All,
    /// The addresses the local store holds no durable full match for, which a push sends.
    New,
    /// As [`Collection::New`], and an address the local store holds no entry for, as after
    /// eviction removed it, is asked of the remote, and when the remote holds it as a list the
    /// fragments it names are collected too. For a push the remote refused for a missing
    /// fragment: one that stopped part way can leave a list on the remote without every fragment
    /// it names.
    Recovery,
}

/// As [`collect_fragments`], keeping the addresses the local store holds no durable full match
/// for with `ignore_durably_stored`, and every address without it.
///
/// Not an `async fn`, which would hold the collection's future in a state of its own.
pub fn collect_new_fragments(
    repository: Arc<RepositoryContext>,
    state_from: Arc<State>,
    state_to: Arc<State>,
    ignore_durably_stored: bool,
    tracker: Arc<StoreRequestTracker>,
) -> impl Future<Output = Result<Vec<Address>, StateError>> {
    let collection = if ignore_durably_stored {
        Collection::New
    } else {
        Collection::All
    };
    collect_fragments(repository, state_from, state_to, collection, tracker)
}

/// The addresses of the fragments `state_to` holds that `state_from` does not, that
/// `collection` keeps, sorted and deduplicated.
///
/// `tracker` counts store reads made by the per-address tasks that the collection spawns to look up
/// each candidate fragment. It also counts reads of metadata blobs named by changed file metadata
/// blocks.
pub async fn collect_fragments(
    repository: Arc<RepositoryContext>,
    state_from: Arc<State>,
    state_to: Arc<State>,
    collection: Collection,
    tracker: Arc<StoreRequestTracker>,
) -> Result<Vec<Address>, StateError> {
    let from_state_address = lore_spawn!({
        let repository = repository.clone();
        let state = state_from.clone();
        let tracker = tracker.clone();
        async move {
            let addresses = collect_state_fragments(repository.clone(), state).await?;
            // Collect all from block addresses, even uploaded, as we want to diff against these
            let mut addresses =
                collect_new_addresses(repository, &addresses, Collection::All, tracker).await?;
            addresses.sort_unstable();
            Ok(addresses)
        }
    });

    let to_new_state_address = lore_spawn!({
        let repository = repository.clone();
        let state = state_to.clone();
        let tracker = tracker.clone();
        async move {
            let addresses = collect_state_fragments(repository.clone(), state).await?;
            let mut addresses =
                collect_new_addresses(repository, &addresses, collection, tracker).await?;
            addresses.sort_unstable();
            Ok(addresses)
        }
    });

    let from_block_address = lore_spawn!({
        let repository = repository.clone();
        let state = state_from.clone();
        async move {
            // Blocks are never fragmented, safe to not call collect_new_addresses
            let mut addresses = collect_node_blocks(repository, state).await?;
            addresses.sort_unstable();
            Ok(addresses)
        }
    });

    let to_block_address = lore_spawn!({
        let repository = repository.clone();
        let state = state_to.clone();
        async move {
            // Blocks are never fragmented, safe to not call collect_new_addresses
            let mut addresses = collect_node_blocks(repository, state).await?;
            addresses.sort_unstable();
            Ok(addresses)
        }
    });

    let from_file_metadata_block_address = lore_spawn!({
        let repository = repository.clone();
        let state = state_from.clone();
        async move {
            // Collect all from metadata block addresses, even uploaded, as we want to diff against these
            let addresses = collect_file_metadata_blocks(repository, state).await?;
            Ok(addresses)
        }
    });

    let to_file_metadata_block_address = lore_spawn!({
        let repository = repository.clone();
        let state = state_to.clone();
        async move {
            // Collect all to metadata block addresses, even uploaded, as we want to inspect and load these
            let addresses = collect_file_metadata_blocks(repository, state).await?;
            Ok(addresses)
        }
    });

    let new_file_address = lore_spawn!({
        let repository = repository.clone();
        let state_from = state_from.clone();
        let state_to = state_to.clone();
        let tracker = tracker.clone();
        async move {
            // Safe to filter these directly to only contain not uploaded fragments, we don't
            // use it as input to any other collection
            collect_new_file_fragments(
                repository, state_from, state_to, ROOT_NODE, ROOT_NODE, collection, tracker,
            )
            .await
        }
    });

    let new_revision_metadata_address = lore_spawn!({
        let repository = repository.clone();
        let metadata_hash = state_to.metadata_hash();
        let tracker = tracker.clone();
        async move {
            collect_new_revision_metadata_fragments(repository, metadata_hash, collection, tracker)
                .await
        }
    });

    let mut failure = None;

    // Get the diff of the node blocks addresses
    let from_block_address = from_block_address.await;
    let to_block_address = to_block_address.await;
    let from_block_address = match from_block_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    let to_block_address = match to_block_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    let from_len = from_block_address.len();
    let to_len = to_block_address.len();
    let diff_block_address = if failure.is_none() {
        collect_diff_addresses(from_block_address, to_block_address)
    } else {
        vec![]
    };
    let diff_block_address_len = diff_block_address.len();
    lore_debug!(
        "Collecting fragments, from blocks {from_len}, to blocks {to_len} -> {diff_block_address_len} diff",
    );

    // Get the new name tables for the new node blocks
    let new_name_address = lore_spawn!({
        let repository = repository.clone();
        let blocks = diff_block_address.clone();
        let tracker = tracker.clone();
        async move {
            let addresses = collect_name_fragments(repository.clone(), blocks).await?;
            let mut addresses =
                collect_new_addresses(repository, &addresses, collection, tracker).await?;
            addresses.sort_unstable();
            Ok(addresses)
        }
    });

    // Get the actual new block addresses, ignore already uploaded now that we have
    // collected the name block addresses from the diff list
    let new_block_address = lore_spawn!({
        let repository = repository.clone();
        let tracker = tracker.clone();
        async move {
            let mut addresses =
                collect_new_addresses(repository, &diff_block_address, collection, tracker).await?;
            addresses.sort_unstable();
            Ok(addresses)
        }
    });

    let from_state_address = from_state_address.await;
    let to_new_state_address = to_new_state_address.await;
    let from_state_address = match from_state_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    let to_new_state_address = match to_new_state_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    let from_len = from_state_address.len();
    let to_len = to_new_state_address.len();
    let mut new_state_address = if failure.is_none() {
        collect_diff_addresses(from_state_address, to_new_state_address)
    } else {
        vec![]
    };
    lore_debug!(
        "Collecting fragments, from state {from_len}, to state new {to_len} -> {} new",
        new_state_address.len()
    );

    let from_file_metadata_block_address = from_file_metadata_block_address.await;
    let to_file_metadata_block_address = to_file_metadata_block_address.await;
    let from_file_metadata_block_address = match from_file_metadata_block_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    let to_file_metadata_block_address = match to_file_metadata_block_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };

    let from_file_metadata_block_len = from_file_metadata_block_address.len();
    let to_file_metadata_block_len = to_file_metadata_block_address.len();
    let (mut new_file_metadata_block_address, mut new_file_metadata_blob_address_tasks) = if failure
        .is_none()
    {
        let mut new_file_metadata_address = vec![];
        let mut tasks = JoinSet::new();
        for (block_index, to_block_address) in to_file_metadata_block_address.iter().enumerate() {
            if to_block_address.hash.is_zero() {
                continue;
            }

            let from_block_address = from_file_metadata_block_address.get(block_index).cloned();
            if from_block_address != Some(*to_block_address) {
                new_file_metadata_address.push(*to_block_address);

                // Check which metadata is new
                let repository = repository.clone();
                let state_to = state_to.clone();
                lore_spawn!(
                    tasks,
                    collect_new_node_metadata_fragments(
                        repository,
                        state_to,
                        from_block_address,
                        *to_block_address,
                        block_index,
                        collection,
                        tracker.clone(),
                    )
                );
            }
        }
        (new_file_metadata_address, tasks)
    } else {
        (vec![], JoinSet::new())
    };
    lore_debug!(
        "Collecting fragments, from file metadata {from_file_metadata_block_len} blocks, to file metadata new {to_file_metadata_block_len} blocks -> {} new blocks",
        new_file_metadata_block_address.len()
    );

    let new_block_address = new_block_address.await;
    let new_name_address = new_name_address.await;

    let mut new_block_address = match new_block_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    lore_debug!(
        "Collected node block from {} diff -> {} new",
        diff_block_address_len,
        new_block_address.len(),
    );
    let mut new_name_address = match new_name_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    lore_debug!(
        "Collected name block from {} diff -> {} new",
        diff_block_address_len,
        new_name_address.len(),
    );

    let mut new_file_metadata_blob_address = vec![];
    while let Some(result) = new_file_metadata_blob_address_tasks.join_next().await {
        match result
            .internal("Task failure")
            .map_err(StateError::from)
            .flatten()
        {
            Ok(mut address) => new_file_metadata_blob_address.append(&mut address),
            Err(err) => failure = failure.or(Some(err)),
        }
    }
    lore_debug!(
        "Collected file metadata blobs from {} blocks -> {} new",
        to_file_metadata_block_len,
        new_file_metadata_blob_address.len(),
    );

    let mut fragments = Vec::with_capacity(
        new_state_address.len()
            + new_block_address.len()
            + new_name_address.len()
            + new_file_metadata_block_address.len()
            + new_file_metadata_blob_address.len(),
    );

    fragments.append(&mut new_state_address);
    fragments.append(&mut new_block_address);
    fragments.append(&mut new_name_address);
    fragments.append(&mut new_file_metadata_block_address);
    fragments.append(&mut new_file_metadata_blob_address);

    lore_debug!("Collected {} new addresses from state", fragments.len());

    // Add branch metadata
    if let Ok((_current_revision, current_branch)) =
        crate::instance::load_current_anchor(&repository).await
        && let Ok(metadata_hash) = branch::metadata_hash(repository.clone(), current_branch).await
    {
        let metadata_fragments = collect_new_addresses(
            repository.clone(),
            &[Address::zero_context_hash(metadata_hash)],
            collection,
            tracker,
        )
        .await;
        if let Ok(mut metadata_fragments) = metadata_fragments {
            lore_debug!(
                "Collected {} new addresses from branch metadata",
                metadata_fragments.len()
            );
            fragments.append(&mut metadata_fragments);
        } else {
            failure = failure.or(metadata_fragments.err());
        }
    }

    // Collect new file node fragments
    let mut new_file_address = match new_file_address
        .await
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    lore_debug!(
        "Collected {} new addresses from files",
        new_file_address.len()
    );

    // Collect the fragments the revision metadata names
    let mut new_revision_metadata_address = match new_revision_metadata_address
        .await
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    lore_debug!(
        "Collected {} new addresses from revision metadata",
        new_revision_metadata_address.len()
    );

    if let Some(err) = failure {
        return Err(err);
    }

    fragments.reserve(new_file_address.len() + new_revision_metadata_address.len());
    fragments.append(&mut new_file_address);
    fragments.append(&mut new_revision_metadata_address);

    fragments.sort_unstable();
    fragments.dedup();
    lore_debug!("Collected {} new addresses in total", fragments.len());

    Ok(fragments)
}

/// The file content under `node_to` in `state_to` that `node_from` in `state_from` does not hold at
/// the same path.
///
/// Both child lists are sorted by name hash, so each child of `node_to` is paired with the child of
/// `node_from` carrying its name in one pass over the two, the pairing [`diff()`] makes.
async fn collect_new_file_fragments(
    repository: Arc<RepositoryContext>,
    state_from: Arc<State>,
    state_to: Arc<State>,
    node_from: NodeID,
    node_to: NodeID,
    collection: Collection,
    tracker: Arc<StoreRequestTracker>,
) -> Result<Vec<Address>, StateError> {
    let (from, to) = join!(
        state_from.collect_children_unsorted(
            repository.clone(),
            node_from,
            false, /* No deleted */
            false, /* No links, pushed separately */
        ),
        state_to.collect_children_unsorted(
            repository.clone(),
            node_to,
            false, /* No deleted */
            false, /* No links, pushed separately */
        )
    );
    let mut from = from?;
    let mut to = to?;
    named_node_sort(&mut from.children);
    named_node_sort(&mut to.children);

    let mut tasks = JoinSet::new();
    let mut failure = None;
    let mut from_index = 0;
    for to_named_node in to.children {
        let to_node_id = to_named_node.node;
        let to_node = to.state.node(to.repository.clone(), to_node_id).await;
        let Ok(to_node) = to_node else {
            failure = failure.or(to_node.err());
            break;
        };

        while from_index < from.children.len()
            && from.children[from_index].name < to_named_node.name
        {
            from_index += 1;
        }

        let mut from_node_id = INVALID_NODE;
        let mut modified = false;
        if let Some(from_named_node) = from.children.get(from_index)
            && from_named_node.name == to_named_node.name
        {
            from_index += 1;
            let from_node = from
                .state
                .node(from.repository.clone(), from_named_node.node)
                .await?;
            from_node_id = from_named_node.node;
            modified = to_node.address != from_node.address;
        }

        if !from_node_id.is_valid_node_id() || modified {
            if to_node.is_file() {
                let repository = to.repository.clone();
                let address = [to_node.address];
                let tracker = tracker.clone();
                lore_spawn!(tasks, async move {
                    collect_new_addresses(repository, &address, collection, tracker).await
                });
            } else {
                let repository = to.repository.clone();
                let state_from = from.state.clone();
                let state_to = to.state.clone();
                let tracker = tracker.clone();
                lore_spawn!(tasks, async move {
                    collect_new_file_fragments_recurse(
                        repository,
                        state_from,
                        state_to,
                        from_node_id,
                        to_node_id,
                        collection,
                        tracker,
                    )
                    .await
                });
            }
        }
    }

    let mut new_addresses = vec![];
    while let Some(result) = tasks.join_next().await {
        match result
            .internal("Task failure")
            .map_err(StateError::from)
            .flatten()
        {
            Ok(mut address) => {
                new_addresses.append(&mut address);
            }
            Err(err) => {
                failure = failure.or(Some(err));
            }
        }
    }

    if let Some(err) = failure {
        return Err(err);
    }

    Ok(new_addresses)
}

fn collect_new_file_fragments_recurse(
    repository: Arc<RepositoryContext>,
    state_from: Arc<State>,
    state_to: Arc<State>,
    node_from: NodeID,
    node_to: NodeID,
    collection: Collection,
    tracker: Arc<StoreRequestTracker>,
) -> Pin<Box<dyn Future<Output = Result<Vec<Address>, StateError>> + Send + 'static>> {
    Box::pin(collect_new_file_fragments(
        repository, state_from, state_to, node_from, node_to, collection, tracker,
    ))
}

/// Appends the addresses the [`MetadataType::Address`] values in `metadata` name.
///
/// A value of that type holds its payload in a fragment of its own, so the
/// payload is part of what carries the metadata and has to travel with it. A
/// value naming the zero address names no payload and is skipped. A value the
/// address decoder rejects fails the walk, since a payload that cannot be named
/// cannot be sent.
fn collect_metadata_address_refs(
    metadata: &Metadata,
    refs: &mut Vec<Address>,
) -> Result<(), StateError> {
    let mut invalid = 0;
    metadata.walk(
        |_key_slice: &[u8], value_slice: &[u8], value_type: MetadataType| {
            if value_type != MetadataType::Address {
                return;
            }
            match Metadata::to_address(value_slice) {
                Ok(address) if !address.hash.is_zero() => refs.push(address),
                Ok(_) => (),
                Err(_) => invalid += 1,
            }
        },
    );

    if invalid != 0 {
        return Err(StateError::internal("Invalid metadata address"));
    }

    Ok(())
}

async fn collect_new_node_metadata_fragments(
    repository: Arc<RepositoryContext>,
    state_to: Arc<State>,
    block_address_from: Option<Address>,
    block_address_to: Address,
    block_index: usize,
    collection: Collection,
    tracker: Arc<StoreRequestTracker>,
) -> Result<Vec<Address>, StateError> {
    let metadata_block_from = if let Some(address) = block_address_from {
        NodeFileMetadataBlockData::read_box_from_immutable_compat(repository.clone(), address, true)
            .await
            .forward::<StateError>("Failed to deserialize metadata")?
    } else {
        NodeFileMetadataBlockData::new_from_heap_zeroed()
    };

    let metadata_block_to = NodeFileMetadataBlockData::read_box_from_immutable_compat(
        repository.clone(),
        block_address_to,
        true,
    )
    .await
    .forward::<StateError>("Failed to deserialize metadata")?;

    let mut metadata_blobs = vec![];
    {
        let node_block_to = state_to.block(repository.clone(), block_index).await?;
        let node_block_to = node_block_to.read();

        for node_index in 0..metadata_block_to.node.len() {
            let metadata_hash = metadata_block_to.node[node_index].metadata;
            if metadata_block_from.node[node_index].metadata == metadata_hash
                || metadata_hash.is_zero()
            {
                continue;
            }

            // We need to check if the node is actually in use or old stale data
            if node_block_to.is_node_in_use(node_index) {
                metadata_blobs.push(Address::zero_context_hash(metadata_hash));
            }
        }
    }

    let mut metadata_refs = vec![];
    for metadata_blob in metadata_blobs.iter() {
        let metadata = {
            let _read = tracker.track_metadata_read();
            Metadata::deserialize(repository.clone(), metadata_blob.hash)
                .await
                .forward::<StateError>("Failed to deserialize metadata")?
        };

        collect_metadata_address_refs(&metadata, &mut metadata_refs)?;
    }

    let mut addresses = collect_new_addresses(
        repository.clone(),
        &metadata_blobs,
        collection,
        tracker.clone(),
    )
    .await?;
    let mut more_addresses =
        collect_new_addresses(repository, &metadata_refs, collection, tracker).await?;
    addresses.append(&mut more_addresses);

    addresses.sort_unstable();
    addresses.dedup();

    Ok(addresses)
}

/// The fragments a revision's own metadata names.
///
/// The blob holding the metadata travels with the rest of the state. This is
/// what that blob points at, which the state does not cover.
async fn collect_new_revision_metadata_fragments(
    repository: Arc<RepositoryContext>,
    metadata_hash: Hash,
    collection: Collection,
    tracker: Arc<StoreRequestTracker>,
) -> Result<Vec<Address>, StateError> {
    if metadata_hash.is_zero() {
        return Ok(vec![]);
    }

    let metadata = Metadata::deserialize(repository.clone(), metadata_hash)
        .await
        .forward::<StateError>("Failed to deserialize revision metadata")?;

    let mut metadata_refs = vec![];
    collect_metadata_address_refs(&metadata, &mut metadata_refs)?;

    let mut addresses =
        collect_new_addresses(repository, &metadata_refs, collection, tracker).await?;

    addresses.sort_unstable();
    addresses.dedup();

    Ok(addresses)
}

/// The addresses among `addresses`, and among the fragments the lists of them name, that
/// `collection` keeps. An address the local store holds no entry for is collected alone, unless
/// [`Collection::Recovery`] finds the remote holds it as a list.
async fn collect_new_addresses(
    repository: Arc<RepositoryContext>,
    addresses: &[Address],
    collection: Collection,
    tracker: Arc<StoreRequestTracker>,
) -> Result<Vec<Address>, StateError> {
    let mut new_addresses = Vec::with_capacity(addresses.len());

    const MAX_TASKS: usize = 1000;
    let mut task = JoinSet::new();
    for address in addresses {
        if address.hash.is_zero() {
            continue;
        }

        let address = *address;
        let repository = repository.clone();
        let tracker = tracker.clone();
        lore_spawn!(task, {
            async move {
                let query = {
                    let _request = tracker.track_lookup();
                    repository
                        .immutable_store()
                        .get_metadata(repository.id, address)
                        .await
                };
                let query = query
                    .ok()
                    .filter(|query| query.match_made != StoreMatch::MatchNone);
                let (fragmented, new) = match &query {
                    Some(query) => (
                        query.fragment.flags & FragmentFlags::PayloadFragmented != 0,
                        collection == Collection::All
                            || query.match_made != StoreMatch::MatchFull
                            || (query.fragment.flags & FragmentFlags::PayloadStoredDurable) == 0,
                    ),
                    None => (
                        collection == Collection::Recovery
                            && remote_lists_fragments(&repository, address, &tracker).await,
                        true,
                    ),
                };
                let mut addresses = vec![];
                if fragmented
                    && let Ok((_fragment, buffer)) = {
                        let _request = tracker.track_lookup();
                        immutable::load_raw(
                            repository.clone(),
                            address,
                            immutable::read_options_from_repository(&repository),
                        )
                        .await
                    }
                {
                    let buffer = buffer.to_aligned::<FragmentReference>();
                    let mut subaddress = Vec::with_capacity(buffer.count::<FragmentReference>());
                    for reference in buffer.as_type_slice::<FragmentReference>().iter() {
                        subaddress.push(Address {
                            context: address.context,
                            hash: reference.hash,
                        });
                    }
                    if let Ok(mut subaddress) = collect_new_addresses_recurse(
                        repository.clone(),
                        subaddress.as_slice(),
                        collection,
                        tracker.clone(),
                    )
                    .await
                    {
                        addresses.append(&mut subaddress);
                    }
                }
                if new {
                    addresses.push(address);
                }
                (!addresses.is_empty()).then_some(addresses)
            }
        });

        while task.len() > MAX_TASKS {
            if let Some(result) = task.join_next().await
                && let Some(mut address) = result.internal("Task failure")?
            {
                new_addresses.append(&mut address);
            }
        }
    }

    while let Some(result) = task.join_next().await {
        if let Some(mut address) = result.internal("Task failure")? {
            new_addresses.append(&mut address);
        }
    }

    Ok(new_addresses)
}

/// Whether the remote holds `address` as a list of fragments. `false` without a remote, or when
/// the remote cannot answer. The remote read counts as a fragment lookup of `tracker`.
async fn remote_lists_fragments(
    repository: &Arc<RepositoryContext>,
    address: Address,
    tracker: &StoreRequestTracker,
) -> bool {
    let Some(session) = immutable::resolve_session(repository) else {
        return false;
    };
    let _request = tracker.track_lookup();
    session
        .get_metadata(&address)
        .await
        .is_ok_and(|fragment| fragment.flags & FragmentFlags::PayloadFragmented != 0)
}

fn collect_new_addresses_recurse(
    repository: Arc<RepositoryContext>,
    addresses: &[Address],
    collection: Collection,
    tracker: Arc<StoreRequestTracker>,
) -> Pin<Box<dyn Future<Output = Result<Vec<Address>, StateError>> + Send + '_>> {
    Box::pin(collect_new_addresses(
        repository, addresses, collection, tracker,
    ))
}

/// Applies a set of node-level changes to a state tree without touching the filesystem.
///
/// This is used for server-side merge operations where there is no working directory.
/// Changes are applied purely at the state tree level: nodes are added, modified, or
/// deleted in the target state based on the diff result.
///
/// The `target_state` is the state being modified (e.g., the current branch head).
/// Each `NodeChange` carries source node data in its `to` field, which is copied into
/// the target state tree with the mode `to` states.
///
/// A move deletes the node at its source and stages the moved node at its destination. The
/// destination of a move that only changes case resolves to the node at its source, so that node
/// is renamed in place instead, keeping its children. A renamed directory keeps the rest of the
/// target's node, since its address derives from its children, which the changes below it reach.
/// A renamed file or link takes the moved node's content only where the move modifies it, and
/// otherwise keeps the target's: the 3-way diff folds a change the target made at the source into
/// a move that leaves the content as it was.
///
/// Preconditions:
/// - The changes must be conflict-free (no entries with `Flags::Conflict`)
/// - All fragment data referenced by the changes must already exist in the immutable store
#[lore_macro::test_pub]
pub(crate) async fn apply_tree_changes(
    repository: Arc<RepositoryContext>,
    target_state: Arc<State>,
    changes: &[NodeChange],
) -> Result<(), StateError> {
    let stats = Arc::new(crate::stage::StageStats::default());

    // Process deletes first, in reverse path order (deepest paths first) so that
    // children are deleted before parent directories
    let mut delete_changes: Vec<&NodeChange> = changes
        .iter()
        .filter(|c| c.action == FileAction::Delete)
        .collect();
    delete_changes.sort_by_key(|b| std::cmp::Reverse(b.path().as_str().len()));

    for change in &delete_changes {
        let node_link = match target_state
            .find_node_link(repository.clone(), change.path().as_str())
            .await
        {
            Ok(node_link) => node_link,
            Err(e) if e.is_node_not_found() => continue,
            Err(err) => return Err(err),
        };

        if node_link.is_valid() {
            crate::stage::stage_delete(
                repository.clone(),
                target_state.clone(),
                change.path().clone(),
                node_link.node,
                NodeFlags::StagedMerge,
                stats.clone(),
                None,
            )
            .await
            .forward::<StateError>("Node not found")?;
        }
    }

    // Process add/modify/move changes
    for change in changes {
        if change.action == FileAction::Delete {
            continue;
        }

        if change.action == FileAction::Move
            && let Some(from_path) = change.move_source()
        {
            let node_link = match target_state
                .find_node_link(repository.clone(), from_path.as_str())
                .await
            {
                Ok(node_link) => node_link,
                Err(e) if e.is_node_not_found() => NodeLink::invalid(),
                Err(err) => return Err(err),
            };

            if node_link.is_valid() {
                if change.is_case_only_move() {
                    let renamed_directory = target_state
                        .node(repository.clone(), node_link.node)
                        .await?
                        .is_directory();
                    target_state
                        .node_rename(
                            repository.clone(),
                            node_link.node,
                            NameChange::Rename(change.path().name()),
                        )
                        .await?;
                    if renamed_directory || !change.flags.contains(change::Flags::Modify) {
                        target_state
                            .node_mark(
                                &repository,
                                node_link.node,
                                Mark::staged(NodeFlags::Staged, true),
                            )
                            .await?;
                        continue;
                    }
                } else {
                    crate::stage::stage_delete(
                        repository.clone(),
                        target_state.clone(),
                        from_path.clone(),
                        node_link.node,
                        NodeFlags::StagedMerge,
                        stats.clone(),
                        None,
                    )
                    .await
                    .forward::<StateError>("Node not found")?;
                }
            }
        }

        // Get the source node data from the change
        let source_state = &change.to.mapping.state;
        let source_node_id = change.to.mapping.node;
        if !source_node_id.is_valid_node_id() {
            continue;
        }

        let mut node = source_state
            .node(change.to.mapping.repository.clone(), source_node_id)
            .await?;
        node.mode = change.to.mode;

        // Stage the node into the target state at the change path
        crate::stage::stage_single_node(
            repository.clone(),
            target_state.clone(),
            change.path().clone(),
            node,
            stats.clone(),
            None,
            crate::filter::FilterMode::Full,
        )
        .await
        .forward::<StateError>("Node not found")?;
    }

    Ok(())
}

/// Load a node based on the determined source.
async fn load_node_for_change(
    source: NodeSource,
    from: &NodeChangeState,
    to: &NodeChangeState,
) -> Option<Node> {
    match source {
        NodeSource::From => {
            let block_index = NodeBlock::index(from.mapping.node);
            let node_index = Node::index(from.mapping.node);
            from.mapping
                .state
                .block(from.mapping.repository.clone(), block_index)
                .await
                .ok()
                .map(|block| block.node(node_index))
        }
        NodeSource::To => {
            let block_index = NodeBlock::index(to.mapping.node);
            let node_index = Node::index(to.mapping.node);
            to.mapping
                .state
                .block(to.mapping.repository.clone(), block_index)
                .await
                .ok()
                .map(|block| block.node(node_index))
        }
        NodeSource::Invalid => Some(Node::default()),
    }
}

/// A directory the hierarchy walk has entered: the children it has yet to emit, whether its name
/// is on the walk's path buffer, and the filter's verdict for the directory, which each child
/// steps from.
///
/// `appended` is false for the walk's top directory, whose path the buffer starts as, and for a
/// directory with an empty name, which appends nothing and so must take nothing off.
struct HierarchyLevel {
    children: LevelChildren,
    appended: bool,
    states: FilterStates,
}

/// The children a [`HierarchyLevel`] has yet to emit.
enum LevelChildren {
    /// On a State no VFS provider serves: the directory's chain, walked as the level goes.
    Streamed(StateNodeChildrenWithNameIterator),
    /// On a served State, which may change under the walk: the children collected when the level
    /// opened.
    Collected(CollectedChildren),
}

/// A served directory's children in walk order, each once however often a chain walk that may
/// repeat met it.
struct CollectedChildren {
    /// The directory the children were collected from.
    node: NodeID,
    children: std::vec::IntoIter<StateNamedNode>,
    /// The block the last child was read from, which consecutive children mostly share.
    block: Option<(usize, Arc<NodeBlock>)>,
}

impl HierarchyLevel {
    /// The level walking the children of `node`, in the tree `side` names; on a served State, only
    /// those a working tree versions where `versioned_only`.
    async fn open(
        side: &NodeChangeState,
        node: NodeID,
        appended: bool,
        states: FilterStates,
        versioned_only: bool,
    ) -> Result<Self, StateError> {
        let NodeMapping {
            repository, state, ..
        } = &side.mapping;
        let children = if state.is_served() {
            // Boxed: only a served State collects, and the collection would otherwise set the
            // size of every walk's future.
            let collected = if versioned_only {
                Box::pin(state.collect_children_unsorted_where(
                    repository.clone(),
                    node,
                    true,
                    false,
                    diff::versioned_on_a_working_tree,
                ))
                .await?
            } else {
                Box::pin(state.collect_children_unsorted(repository.clone(), node, true, false))
                    .await?
            };
            LevelChildren::Collected(CollectedChildren {
                node,
                children: collected.children.into_iter(),
                block: None,
            })
        } else {
            LevelChildren::Streamed(
                StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node)
                    .await?,
            )
        };
        Ok(Self {
            children,
            appended,
            states,
        })
    }

    /// The next child to emit, with its record and name, or `None` once the level is done.
    fn next_child<'a>(
        &'a mut self,
        side: &'a NodeChangeState,
    ) -> impl Future<Output = Result<Option<(NodeID, Node, NodeNameLock)>, StateError>> + Send + 'a
    {
        match &mut self.children {
            LevelChildren::Streamed(children) => Either::Left(children.next()),
            LevelChildren::Collected(children) => Either::Right(children.next(side)),
        }
    }
}

impl CollectedChildren {
    /// The next child, read again from its block, that is still at its place under the name it
    /// was collected with: one discarded, moved to another directory or renamed to another name
    /// hash since is left out, as a children walk may omit a node moved during it. A child whose
    /// name the read path refuses is skipped with a warning, as a listing skips it.
    async fn next(
        &mut self,
        side: &NodeChangeState,
    ) -> Result<Option<(NodeID, Node, NodeNameLock)>, StateError> {
        loop {
            let Some(StateNamedNode {
                node: child,
                name: name_hash,
                ..
            }) = self.children.next()
            else {
                return Ok(None);
            };
            let index = NodeBlock::index(child);
            let block = match self.block.take() {
                Some((held, block)) if held == index => block,
                _ => {
                    side.mapping
                        .state
                        .block_with_nametable(side.mapping.repository.clone(), index)
                        .await?
                }
            };
            let (node, name) = self
                .block
                .insert((index, block))
                .1
                .node_and_name_ref(Node::index(child));
            if node.is_discarded() || node.parent != self.node || node.name_hash != name_hash {
                side.mapping
                    .state
                    .walk_moves
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }
            match name {
                Ok(name) => return Ok(Some((child, node, name))),
                Err(err) => lore_warn!("Skipping node {child} with invalid name: {err}"),
            }
        }
    }
}

/// Emits a change for every node under a directory added or deleted as a whole.
///
/// A delete enumerates the side [`delete_hierarchy_side`] answers with, and emits nothing, with a
/// warning, where that node cannot be read; an add enumerates `to`, and fails where it cannot be
/// read. Neither descends a file or a link: a link's children are another repository's, and its
/// mount path stands for them. `states` is the filter's verdict for the directory, which each child
/// steps from, and the walk takes only the children a working tree versions where
/// `versioned_only`.
///
/// Descent is an explicit stack of [`HierarchyLevel`] rather than a box per node, so the walk runs
/// at a fixed call depth. A child directory is handed to a task of its own while
/// [`hierarchy_task_semaphore`] allows, so a hierarchy of millions of nodes is walked on every core
/// rather than one; once the budget is spent, the levels stack in this task. A directory's change
/// precedes everything below it, but siblings walked by different tasks interleave. On a State no
/// VFS provider serves, no child of a directory moves while the walk lists it, so each level walks
/// its directory's chain as the walk goes, reading each child once, as a listing does: a child
/// renamed in place is emitted at the name it holds when the walk reaches it, and one detached
/// before then is left out. A served State may change under the walk, so there each level collects
/// its children first and reads each again as it emits it ([`CollectedChildren`]): no path is
/// emitted twice and none at a name its node has left. A child whose name the read path refuses is
/// skipped with a warning, as a listing skips it.
///
/// Children are named on one path buffer, which holds the path of the directory being walked and
/// keeps [`RelativePath::COMPONENT_ROOM`] free beyond it. A child's name is a read lock on its node
/// block, released once the name is pushed, so the lock covers copying the name into that room;
/// only a longer name grows the buffer under it. The child's own path is allocated from the buffer
/// after that, and only for a child the filter keeps. A name is taken off as soon as its child is
/// done with, or for a directory once its level is done.
///
/// Not an `async fn`, which would hold `from` and `to` beside the sides taken from them.
fn add_change_hierarchy(
    from: NodeChangeState,
    to: NodeChangeState,
    action: change::FileAction,
    changes: &ChangeSender,
    filter_mode: FilterMode,
    states: FilterStates,
    versioned_only: bool,
) -> impl Future<Output = Result<(), StateError>> + Send + '_ {
    let sides = match action {
        FileAction::Delete => delete_hierarchy_side(from, &to).map(|side| (side, to)),
        FileAction::Add => Some((to, from)),
        _ => None,
    };
    async move {
        let Some((side, other)) = &sides else {
            return Ok(());
        };
        if !side.mapping.node.is_valid_or_root_node_id() {
            return Ok(());
        }
        match side.get_node().await {
            Ok(node) if !node.is_directory() => return Ok(()),
            Ok(_) => {}
            Err(err) if action == FileAction::Delete => {
                lore_warn!(
                    "Skipping deletes below {}: node {} could not be read: {err}",
                    side.mapping.path,
                    side.mapping.node
                );
                return Ok(());
            }
            Err(err) => return Err(err),
        }

        // Walked in a helper so an early `?` still joins the tasks it spawned.
        let mut tasks = JoinSet::new();
        let walked = walk_hierarchy(
            side,
            other,
            action,
            changes,
            filter_mode,
            states,
            versioned_only,
            &mut tasks,
        )
        .await;
        let joined = lore_base::lore_drain_tasks!(tasks, StateError::internal("Task failure"));
        walked.and(joined)
    }
}

/// Budget for hierarchy walks live at once beyond the one each caller awaits: one per worker thread.
///
/// Its own budget rather than the diff walk's. A hierarchy walk holds the node blocks of every
/// level it stands in, and walks beyond the workers only park holding them: deleting a 5M-entry
/// tree took 73 s on one task and 12.9 s on 16, with the same peak, while 32 tasks
/// were no faster and held 0.7 GB more, and the diff walk's 1000 held 2.3 GB more.
static HIERARCHY_TASK_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn hierarchy_task_semaphore() -> &'static Arc<Semaphore> {
    HIERARCHY_TASK_SEMAPHORE.get_or_init(|| {
        Arc::new(Semaphore::new(
            lore_base::runtime::default_worker_threads().max(1),
        ))
    })
}

/// [`add_change_hierarchy`] for the directory `from` or `to` names, in a task of its own holding
/// `permit` from [`hierarchy_task_semaphore`].
///
/// Boxed, since the walk it runs spawns more of these.
#[allow(clippy::too_many_arguments)]
fn spawned_hierarchy(
    from: NodeChangeState,
    to: NodeChangeState,
    action: change::FileAction,
    changes: ChangeSender,
    filter_mode: FilterMode,
    states: FilterStates,
    versioned_only: bool,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send>> {
    Box::pin(async move {
        let result = add_change_hierarchy(
            from,
            to,
            action,
            &changes,
            filter_mode,
            states,
            versioned_only,
        )
        .await;
        drop(permit);
        result
    })
}

/// The walk of [`add_change_hierarchy`] below `side`, spawning into `tasks`.
#[allow(clippy::too_many_arguments)]
async fn walk_hierarchy(
    side: &NodeChangeState,
    other: &NodeChangeState,
    action: change::FileAction,
    changes: &ChangeSender,
    filter_mode: FilterMode,
    states: FilterStates,
    versioned_only: bool,
    tasks: &mut JoinSet<Result<(), StateError>>,
) -> Result<(), StateError> {
    let mut path = side
        .mapping
        .path
        .to_buf_with_capacity(RelativePath::COMPONENT_ROOM);
    let mut levels =
        vec![HierarchyLevel::open(side, side.mapping.node, false, states, versioned_only).await?];
    while let Some(level) = levels.last_mut() {
        let Some((child_id, child_node, child_name)) = level.next_child(side).await? else {
            if levels.pop().is_some_and(|done| done.appended) {
                path.pop();
            }
            continue;
        };
        // On every level: a streamed one meets each child here first, and a served one collected
        // only what a working tree versions but reads each child again here, which may have
        // become a symbolic link since.
        if versioned_only && !diff::versioned_on_a_working_tree(&child_node) {
            continue;
        }
        let appended = !child_name.is_empty();
        path.push(child_name);
        let filter = &side.mapping.repository.filter;
        let mode = filter
            .tracked_mode(
                level.states,
                &path,
                child_node.is_directory(),
                filter_mode.at_staged(child_node.is_staged()),
                async || {
                    side.mapping
                        .state
                        .holds_tracked_change(
                            &side.mapping.repository,
                            child_id,
                            NodeFlags::from_bits_retain(child_node.flags),
                        )
                        .await
                },
            )
            .await?;
        let (child_states, excluded) =
            filter.child_emit_excludes(level.states, &path, child_node.is_directory(), mode);

        if !excluded {
            let child_path = path.clone().freeze();
            emit(changes, || {
                let absent = other.invalid(child_path.clone());
                let child = side.from_child(child_id, &child_node, child_path);
                let (from, to) = match action {
                    FileAction::Delete => (child, absent),
                    _ => (absent, child),
                };
                NodeChange {
                    action,
                    flags: compute_change_flags(&child_node),
                    from,
                    to,
                }
            })
            .await?;
        }

        // TODO(UCS-11623): Check if the target link repository has no local changes - if so,
        // do not iterate and show each link file as added. Otherwise, recurse in and compare
        // against file system and/or staged state in link
        if !excluded && child_node.is_directory() {
            if let Ok(permit) = hierarchy_task_semaphore().clone().try_acquire_owned() {
                let child = side.from_child(child_id, &child_node, path.clone().freeze());
                let absent = other.invalid(child.mapping.path.clone());
                let (from, to) = match action {
                    FileAction::Delete => (child, absent),
                    _ => (absent, child),
                };
                let changes = changes.clone();
                lore_spawn!(
                    tasks,
                    crate::subtask!(spawned_hierarchy(
                        from,
                        to,
                        action,
                        changes,
                        filter_mode,
                        child_states,
                        versioned_only,
                        permit
                    ))
                );
                if appended {
                    path.pop();
                }
            } else {
                levels.push(
                    HierarchyLevel::open(side, child_id, appended, child_states, versioned_only)
                        .await?,
                );
                path.reserve(1 + RelativePath::COMPONENT_ROOM);
            }
        } else if appended {
            path.pop();
        }
        while let Some(joined) = tasks.try_join_next() {
            joined
                .internal("Task failure")
                .map_err(StateError::from)
                .flatten()?;
        }
    }
    Ok(())
}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TreeFlags: u32 {
        /// Tree is dirty
        const Dirty = 0b1;
    }
}
bitflagsops!(TreeFlags, u32);

/// The side a delete hierarchy enumerates, which is the side its caller drew the filter verdict
/// from.
///
/// `from` where the walk found the deletion between two trees, `to` where a stage recorded it and
/// there is no from node at all. The tree and the verdict travel together: a verdict names lines
/// by index into one filter, and the same indices read different rules in another.
fn delete_hierarchy_side(from: NodeChangeState, to: &NodeChangeState) -> Option<NodeChangeState> {
    if from.mapping.node.is_valid_or_root_node_id() {
        Some(from)
    } else if to.mapping.node.is_valid_or_root_node_id() {
        Some(to.clone())
    } else {
        None
    }
}

/// The accessors, wrappers and constructors a `test-util` build adds to this module's types, for
/// tests.
#[cfg(feature = "test-util")]
pub mod test_util {
    use super::*;

    impl State {
        /// The hash of the link list the State was serialized with. For tests.
        pub fn link_list_hash(&self) -> Hash {
            self.data_read().hash_link
        }

        /// Drops the State's own hold on the blocks it keeps in memory for reuse: its block cache
        /// and the file metadata blocks kept for checks and attaches. A block nothing else holds
        /// leaves memory, to be loaded again when next asked for. Dirty blocks stay registered for
        /// the next save, and a holder of a block's `Arc` keeps it. Takes the runtime write lock
        /// and no block lock; the blocks drop after it is released. For tests and the stress
        /// suite's evictor.
        pub fn evict_blocks_for_tests(&self) {
            let held = {
                let mut runtime = self.runtime_write();
                (
                    std::mem::take(&mut runtime.block_cache),
                    std::mem::take(&mut runtime.block_file_metadata_checked),
                    runtime.attach_file_metadata.take(),
                )
            };
            drop(held);
        }

        /// [`Self::block_modified`], for tests that write a block's records or names directly, or
        /// register a block no write changed.
        pub fn block_modified_for_tests(&self, block: Arc<NodeBlock>, index: usize) {
            self.block_modified(block, index);
        }

        /// How many times a find by name ([`Self::find_subnode`], [`Self::find_subnode_resident`])
        /// walked its directory's chain again after a reattach, over this state's life.
        pub fn find_restarts(&self) -> u64 {
            self.find_restarts.load(Ordering::Relaxed)
        }

        /// How many times an attach ([`Self::node_attach`]) found its publish refused and looked
        /// its name up again, over this state's life. For tests.
        pub fn attach_retries(&self) -> u64 {
            self.attach_retries.load(Ordering::Relaxed)
        }

        /// How many times the served chain rule refused a change under its locks, after the
        /// change's pre-test passed: an attach's publish, a detach's or a reattach's section, or a
        /// level of a served mark's descent, over this state's life. For tests.
        pub fn served_refusals(&self) -> u64 {
            self.served_refusals.load(Ordering::Relaxed)
        }

        /// How many settles' clears found their directory's children equal to the committed ones,
        /// then a change below, or a served move of the directory itself, landed before the clear
        /// (the pending clear dropped, or the chain count moved), over this state's life. For
        /// tests.
        pub fn clears_overtaken(&self) -> u64 {
            self.clears_overtaken.load(Ordering::Relaxed)
        }

        /// How many unserved clears found a directory's child dirty, or its chain changed, after
        /// clearing it, and marked it again, over this state's life. For tests.
        pub fn clears_rechecked(&self) -> u64 {
            self.clears_rechecked.load(Ordering::Relaxed)
        }

        /// How many unserved clears marking a directory again found it, or a directory above it,
        /// detached, which they answer as success, over this state's life. For tests.
        pub fn clears_found_gone(&self) -> u64 {
            self.clears_found_gone.load(Ordering::Relaxed)
        }

        /// How many children the walk of a directory added or deleted as a whole found moved
        /// since it collected them, over this state's life. For tests.
        pub fn walk_moves(&self) -> u64 {
            self.walk_moves.load(Ordering::Relaxed)
        }

        /// Whether node block `index` is recorded changed ([`Self::block_modified`]), so the next
        /// save writes it.
        pub fn block_recorded_changed(&self, index: usize) -> bool {
            self.runtime_read()
                .block_dirty
                .iter()
                .any(|(_, at)| *at == index)
        }

        /// Whether node block `index` is noted for the next sweep ([`StateRuntime::unswept`]),
        /// which scans a noted block in memory whose flag is set (C15). For tests.
        pub fn block_noted_unswept(&self, index: usize) -> bool {
            super::block_bit(&self.runtime_read().unswept, index)
        }

        /// How many times node block `index` is registered for the next save, at most once.
        pub fn block_registrations(&self, index: usize) -> usize {
            self.runtime_read()
                .block_dirty
                .iter()
                .filter(|(_, at)| *at == index)
                .count()
        }

        /// How many times file metadata block `index` is registered for the next save, at most
        /// once.
        pub fn block_file_metadata_registrations(&self, index: usize) -> usize {
            self.runtime_read()
                .block_file_metadata_dirty
                .iter()
                .filter(|(_, at)| *at == index)
                .count()
        }

        /// Where the registered bits disagree with the registrations, at rest: a block registered
        /// twice, a registered block whose bit is clear, a resident block whose bit is set that no
        /// registration holds, node and file metadata blocks alike. For tests.
        pub fn registration_faults(&self) -> Vec<String> {
            fn faults<B>(
                kind: &str,
                registered: &[(Arc<B>, usize)],
                resident: &[Weak<B>],
                is_registered: impl Fn(&B) -> bool,
            ) -> Vec<String> {
                let mut found = Vec::new();
                let mut listed = std::collections::BTreeSet::new();
                for (block, index) in registered {
                    if !listed.insert(*index) {
                        found.push(format!("{kind} block {index} is registered twice"));
                    }
                    if !is_registered(block) {
                        found.push(format!(
                            "{kind} block {index} is registered with its bit clear"
                        ));
                    }
                }
                for (index, block) in resident.iter().enumerate() {
                    if let Some(block) = block.upgrade()
                        && is_registered(&block)
                        && !listed.contains(&index)
                    {
                        found.push(format!("{kind} block {index} has its bit set unregistered"));
                    }
                }
                found
            }
            let runtime = self.runtime_read();
            let mut found = faults(
                "node",
                &runtime.block_dirty,
                &runtime.block,
                NodeBlock::is_registered,
            );
            found.append(&mut faults(
                "file metadata",
                &runtime.block_file_metadata_dirty,
                &runtime.block_file_metadata,
                NodeFileMetadataBlock::is_registered,
            ));
            found
        }
    }
}
