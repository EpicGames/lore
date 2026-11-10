// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A directory's children, packed as one entry array plus one contiguous name
//! arena: a listing is read on every `readdirplus` and `lookup`, so its shape
//! favors a cheap read over a cheap build. A lookup reads the changes since the
//! build from the directory's chain log ([`Listing::lookup`]); anything else
//! rebuilds a listing its directory changed since.

use std::sync::Arc;

use lore_base::text::fold_eq;
use lore_base::types::Address;
use lore_error_set::prelude::*;
use smallvec::SmallVec;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

use crate::BoxFuture;
use crate::errors::NotSupported;
use crate::filter::FilterStates;
use crate::fs::fuse::FuseProviderError;
use crate::fs::fuse::filesystem::ListingHook;
use crate::fs::fuse::served::Served;
use crate::fs::fuse::view::child_filter;
use crate::fs::fuse::view::filter_states_to_u8;
use crate::fs::fuse::view::view_line;
use crate::hash::hash_string;
use crate::node::ChainLog;
use crate::node::Newest;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeExtFlags;
use crate::node::NodeID;
use crate::node::PatchKind;
use crate::state::Absent;
use crate::state::StateNodeChildrenWithNameIterator;
use crate::state::drop_repeated_nodes;
use crate::util::path::RelativePath;

/// The directory of a listing no State directory backs, which never changes.
pub const NO_DIR: NodeID = NodeID::MAX;

/// Bits packed into [`ListingEntry::flags`].
pub mod entry_flags {
    /// The child is a directory rather than a file.
    pub const IS_DIR: u8 = 1;
    /// The child is excluded by the mounted view (task 32); still listed so a
    /// lookup can answer `ENOENT` for it without a separate filter pass.
    pub const VIEW_EXCLUDED: u8 = 2;
    /// The child's content lives under `write/` (`NodeExtFlags::LocalContent`): the host file
    /// answers its READ, size and times.
    pub const LOCAL: u8 = 4;
    /// The child is a symbolic link under `write/`.
    pub const SYMLINK: u8 = 8;
}

/// One child of a directory.
///
/// 80 bytes so a directory of any size holds its entries in one contiguous
/// allocation with no per-entry heap type; names live in the listing's
/// separate byte arena, sliced by `name_offset`/`name_len`. Holds the full
/// `Address` (hash plus context), not just the content hash, since
/// `immutable::read_into` and address comparisons both need the context.
#[repr(C)]
#[derive(Debug, Clone, Copy, IntoBytes, FromBytes, Immutable)]
pub struct ListingEntry {
    pub name_hash: u64,
    pub size: u64,
    pub address: Address,
    pub name_offset: u32,
    pub name_len: u16,
    pub mode: u16,
    pub flags: u8,
    pub filter_state: u8,
    /// The line the view slot's verdict was decided at ([`view_line`]).
    pub view_line: u16,
    /// The index in the listing's held blocks of the block holding the child.
    pub block: u32,
}

const _: () = assert!(std::mem::size_of::<ListingEntry>() == 80);

/// One child gathered by [`Listing::build`] before it is sorted and packed:
/// the node id and node read from `State`, its name copied off the
/// `NodeNameLock`, its packed view `filter_state` with the line that decided
/// it, its `VIEW_EXCLUDED` bit, and its position in the walk.
type CollectedChild = (NodeID, Node, SmallVec<[u8; 256]>, (u8, u16), bool, u32);

/// A directory's children as its chain held them when its chain log stood at `log_seq`.
///
/// Entries are sorted by `(name_hash, name)`, which is what lets
/// [`Listing::find`] (task 33) binary-search the hash and fold-compare only
/// the run that ties.
pub struct Listing {
    pub entries: Box<[ListingEntry]>,
    pub names: Box<[u8]>,
    pub node_ids: Box<[NodeID]>,
    /// The directory listed.
    pub dir: NodeID,
    /// The directory's log position when the build began: the patches from it on are the
    /// changes the listing does not hold.
    pub log_seq: u32,
    /// The log's detach count when the build began.
    pub detaches: u32,
    /// The log's respell count when the build began.
    pub respells: u32,
    pub skipped_unrepresentable: u32,
    /// Children the build left out as staged deletes.
    pub skipped_staged: u32,
    /// Children the build left out because their block could not give back their name.
    pub skipped_invalid: u32,
    /// The directory's block, held so a check reads it with no State lock; `None` for a
    /// listing no directory backs.
    dir_block: Option<Arc<NodeBlock>>,
    /// The blocks holding the listed nodes, held so a lookup reads a child's record with no
    /// State lock. Each is shared with the State and with every listing of a directory with
    /// children in it, and stays in memory while one holds it.
    blocks: Box<[Arc<NodeBlock>]>,
}

/// What [`Listing::lookup`] found for a name.
pub enum Hit {
    /// The listed entry at this index, confirmed live under the listed spelling, with its record
    /// as it stands ([`Listing::current_entry`]).
    Listed(usize, ListingEntry),
    /// A node attached since the build, or a listed node now spelled otherwise, with the block
    /// holding its record.
    Added(NodeID, Arc<NodeBlock>),
    Miss,
    /// The listing cannot answer: its directory has no log, the log no longer holds the
    /// patches since the build, a child was respelled since, the block a patch names left
    /// memory, or a listed child's name cannot be read. It is to be rebuilt.
    Stale,
}

/// What [`Listing::lookup`] read of a listed candidate under its block's read lock.
enum Seen {
    /// The node left the entry's name: discarded, moved or renamed.
    Left,
    /// The node holds the name under the listed spelling, with its record as it stands.
    Listed(ListingEntry),
    /// The node holds the name under another spelling.
    Respelled,
    /// The node's name cannot be read from its block.
    Unreadable,
}

impl std::fmt::Debug for Hit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Listed(index, _) => write!(f, "Listed({index})"),
            Self::Added(node, _) => write!(f, "Added({node})"),
            Self::Miss => f.write_str("Miss"),
            Self::Stale => f.write_str("Stale"),
        }
    }
}

impl Listing {
    /// A listing of the given entries that no State directory backs.
    pub fn from_parts(
        entries: Box<[ListingEntry]>,
        names: Box<[u8]>,
        node_ids: Box<[NodeID]>,
    ) -> Self {
        Self {
            entries,
            names,
            node_ids,
            dir: NO_DIR,
            log_seq: 0,
            detaches: 0,
            respells: 0,
            skipped_unrepresentable: 0,
            skipped_staged: 0,
            skipped_invalid: 0,
            dir_block: None,
            blocks: Box::new([]),
        }
    }

    /// The listing of directory `dir`, held in `dir_block`, while its chain is empty, as a
    /// directory MKDIR just attached holds it; `None` once a child is attached or the directory
    /// is gone. Installs the directory's log first, so a child attached later is found through
    /// it, as for any listing.
    pub fn empty(dir: NodeID, dir_block: Arc<NodeBlock>) -> Option<Arc<Listing>> {
        let (mark, node) = dir_block.watch_chain(Node::index(dir), None);
        if node.is_discarded() || node.child().is_some() {
            return None;
        }
        Some(Arc::new(Self {
            entries: Box::new([]),
            names: Box::new([]),
            node_ids: Box::new([]),
            dir,
            log_seq: mark.seq,
            detaches: mark.detaches,
            respells: mark.respells,
            skipped_unrepresentable: 0,
            skipped_staged: 0,
            skipped_invalid: 0,
            dir_block: Some(dir_block),
            blocks: Box::new([]),
        }))
    }

    /// This listing holding `blocks`, as a build leaves it holding the blocks of its entries.
    #[cfg(feature = "test-util")]
    pub fn holding(mut self, blocks: Vec<Arc<NodeBlock>>) -> Self {
        self.blocks = blocks.into_boxed_slice();
        self
    }

    /// The live child spelled `name` under any case-fold-equivalent spelling: a listed entry
    /// still holding its node, read with its spelling from its record, else what the newest
    /// patch for the name since the build says, from the directory's chain log. A listed hit
    /// reads no log, so no change since the build makes it stale. Takes one block read lock at a
    /// time, no State lock, and allocates nothing.
    pub fn lookup(&self, name: &str) -> Hit {
        let Some(dir_block) = &self.dir_block else {
            return self
                .find(name.as_bytes())
                .map_or(Hit::Miss, |index| Hit::Listed(index, self.entries[index]));
        };
        for index in self.folding_to(name) {
            let listed = &self.entries[index];
            let block = &self.blocks[listed.block as usize];
            let node = self.node_ids[index];
            let seen = block.read_node_and_name(Node::index(node), |record, held| {
                if !describes(listed, record, self.dir) {
                    return Seen::Left;
                }
                match held {
                    Ok(held) if held.as_bytes() == self.name(index) => {
                        Seen::Listed(refreshed(listed, record))
                    }
                    Ok(_) => Seen::Respelled,
                    Err(_) => Seen::Unreadable,
                }
            });
            match seen {
                // A node that left the name falls through to the log, which logged its leaving.
                Seen::Left => {}
                Seen::Listed(entry) => return self.confirmed(dir_block, index, entry),
                Seen::Respelled => return Hit::Added(node, block.clone()),
                Seen::Unreadable => return Hit::Stale,
            }
        }
        self.patched(dir_block, hash_string(name))
    }

    /// A live listed hit, unless the directory is gone.
    fn confirmed(&self, dir_block: &NodeBlock, index: usize, entry: ListingEntry) -> Hit {
        if dir_block.read().node(Node::index(self.dir)).is_discarded() {
            return Hit::Miss;
        }
        Hit::Listed(index, entry)
    }

    /// What the newest patch for `name_hash` since the build says, read under the directory
    /// block's read lock. The child is not read here: two block locks are never held at once.
    fn patched(&self, dir_block: &NodeBlock, name_hash: u64) -> Hit {
        let reader = dir_block.read();
        let at = Node::index(self.dir);
        if reader.node(at).is_discarded() {
            return Hit::Miss;
        }
        let Some(log) = reader.chain_log(at) else {
            return Hit::Stale;
        };
        if log.respells() != self.respells {
            return Hit::Stale;
        }
        self.newest_patch(log, name_hash).unwrap_or(Hit::Miss)
    }

    /// What the newest patch for `name_hash` in `log` since the build says, `None` where there
    /// is none: stale where the log no longer holds every patch since the build.
    fn newest_patch(&self, log: &ChainLog, name_hash: u64) -> Option<Hit> {
        match log.newest(name_hash, self.log_seq) {
            Newest::Uncovered => Some(Hit::Stale),
            Newest::Nothing => None,
            Newest::Patch(patch) => Some(match patch.kind {
                PatchKind::Detached => Hit::Miss,
                PatchKind::Attached => patch
                    .block
                    .upgrade()
                    .map_or(Hit::Stale, |block| Hit::Added(patch.node, block)),
            }),
        }
    }

    /// [`Self::lookup`] of a name a request is about to attach, with, on a miss, the name's
    /// absence: no child of the directory holds its hash, under any spelling or view state, as of
    /// the log position the absence carries, for the attach to check the patches since under its
    /// write locks. The entries under the hash are read before the log, so a node seen gone and
    /// attached again before the log read left its patch there, and the log is read once. No
    /// absence where the build left a child out or an entry under the hash still holds its node,
    /// as a walk by name would find either. Takes one block read lock at a time and allocates
    /// nothing.
    pub fn lookup_to_attach(
        &self,
        state: &crate::state::State,
        name: &str,
    ) -> (Hit, Option<Absent>) {
        let name_hash = hash_string(name);
        let left_out = self.skipped_unrepresentable != 0
            || self.skipped_staged != 0
            || self.skipped_invalid != 0;
        let dir_block = match &self.dir_block {
            Some(dir_block) if !left_out && !self.holds(name_hash) => dir_block,
            _ => return (self.lookup(name), None),
        };
        // No entry under the hash holds its node, so the log alone answers, as in `lookup`.
        let reader = dir_block.read();
        let at = Node::index(self.dir);
        if reader.node(at).is_discarded() {
            return (Hit::Miss, None);
        }
        let Some(log) = reader.chain_log(at) else {
            return (Hit::Stale, None);
        };
        if log.respells() != self.respells {
            return (Hit::Stale, None);
        }
        match self.newest_patch(log, name_hash) {
            None | Some(Hit::Miss) => {
                let absent = Absent::new(state, log.seq(), dir_block.clone(), self.dir, name_hash);
                (Hit::Miss, Some(absent))
            }
            Some(hit) => (hit, None),
        }
    }

    /// Whether a listed entry under `name_hash`, of any spelling or view state, still holds its
    /// node.
    fn holds(&self, name_hash: u64) -> bool {
        let start = self
            .entries
            .partition_point(|entry| entry.name_hash < name_hash);
        (start..self.entries.len())
            .take_while(|&index| self.entries[index].name_hash == name_hash)
            .any(|index| self.current_entry(index).is_some())
    }

    /// The entry at `index` with its node's record as it stands: the record, read from the block
    /// the listing holds under that block's read lock alone, answers the address, size, mode,
    /// kind and local content, and the listing the name, view state and block. `None` where the
    /// node left the entry's name: discarded, moved to another directory or renamed. A listing no
    /// directory backs answers its entry. Copies by value and allocates nothing.
    pub fn current_entry(&self, index: usize) -> Option<ListingEntry> {
        let listed = &self.entries[index];
        if self.dir_block.is_none() {
            return Some(*listed);
        }
        let node = self.blocks[listed.block as usize].node(Node::index(self.node_ids[index]));
        describes(listed, &node, self.dir).then(|| refreshed(listed, &node))
    }

    /// Whether the node listed at `index` holds the entry's name, read from its record as it
    /// stands under its block's read lock alone. A listing no directory backs answers true.
    pub fn holds_entry(&self, index: usize) -> bool {
        let listed = &self.entries[index];
        self.dir_block.is_none()
            || describes(
                listed,
                &self.blocks[listed.block as usize].node(Node::index(self.node_ids[index])),
                self.dir,
            )
    }

    /// The blocks this listing holds in memory, each once.
    pub fn held_blocks(&self) -> impl Iterator<Item = &Arc<NodeBlock>> {
        let dir = self.dir_block.as_ref().filter(|dir_block| {
            !self
                .blocks
                .iter()
                .any(|block| Arc::ptr_eq(block, dir_block))
        });
        self.blocks.iter().chain(dir)
    }

    /// Whether the listing is the whole directory: it is live and its log has taken no patch
    /// and no respelling since the build. False for a directory with no log.
    pub fn is_current(&self) -> bool {
        self.log_reads(|log| log.seq() == self.log_seq && log.respells() == self.respells)
    }

    /// Whether every listed row is still a member under its spelling: the directory is live and
    /// its log has taken no detach and no respelling since the build. False for a directory with
    /// no log.
    pub fn rows_current(&self) -> bool {
        self.log_reads(|log| log.detaches() == self.detaches && log.respells() == self.respells)
    }

    /// `check` on the directory's log under its block's read lock, where the directory is live;
    /// true for a listing no directory backs.
    fn log_reads(&self, check: impl FnOnce(&ChainLog) -> bool) -> bool {
        let Some(dir_block) = &self.dir_block else {
            return true;
        };
        let reader = dir_block.read();
        let at = Node::index(self.dir);
        !reader.node(at).is_discarded() && reader.chain_log(at).is_some_and(check)
    }

    /// The name of the child at `index`, sliced from the shared arena.
    pub fn name(&self, index: usize) -> &[u8] {
        let entry = &self.entries[index];
        let start = entry.name_offset as usize;
        let end = start + entry.name_len as usize;
        &self.names[start..end]
    }

    pub fn entry(&self, index: usize) -> &ListingEntry {
        &self.entries[index]
    }

    /// The index of the child spelled `name` under any case-fold-equivalent
    /// spelling, or `None` if there is none (including a name view-excluded
    /// from this mounted view, which answers the same as absent).
    ///
    /// `partition_point` narrows to the run of entries sharing `name`'s
    /// [`hash_string`], then each candidate in that run is compared by
    /// [`fold_eq`].
    pub fn find(&self, name: &[u8]) -> Option<usize> {
        self.folding_to(std::str::from_utf8(name).ok()?).next()
    }

    /// The indices of every child spelled as `name` under any case-fold-equivalent spelling,
    /// skipping view-excluded ones: the run sharing `name`'s folded hash, fold-compared.
    pub fn folding_to<'a>(&'a self, name: &'a str) -> impl Iterator<Item = usize> + 'a {
        let target_hash = hash_string(name);
        let start = self
            .entries
            .partition_point(|entry| entry.name_hash < target_hash);
        (start..self.entries.len())
            .take_while(move |&index| self.entries[index].name_hash == target_hash)
            .filter(move |&index| {
                // Every name in the arena was validated as UTF-8 before it was stored
                // (`validate_node_name_for_store` takes `&str`), so a failed conversion is a
                // corrupt arena.
                self.entries[index].flags & entry_flags::VIEW_EXCLUDED == 0
                    && std::str::from_utf8(self.name(index))
                        .is_ok_and(|candidate| fold_eq(candidate, name))
            })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Approximate heap footprint, for the listing cache's byte budget.
    pub fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + std::mem::size_of_val(&*self.entries)
            + std::mem::size_of_val(&*self.names)
            + std::mem::size_of_val(&*self.node_ids)
            + std::mem::size_of_val(&*self.blocks)
    }

    /// The entry of `node`, whose name is `name_len` long, with no view state: the caller
    /// sets it where it needs it.
    pub fn entry_of(node: &Node, name_len: usize) -> ListingEntry {
        pack_entry(node, 0, name_len, (0, 0), false)
    }

    /// Builds a listing from `parent`'s live children.
    ///
    /// `parent_filter` is the mounted view's state at `parent`, and
    /// `parent_path` its path; both feed [`child_filter`] to decide each
    /// child's own `VIEW_EXCLUDED` bit and packed `filter_state`.
    ///
    /// `NodeNameLock` discipline: everything that reads the lock's borrowed
    /// `&str` (the view-filter match, then the copy into a stack-allocated
    /// buffer) happens before the lock is dropped, and it is dropped before
    /// the iterator's next `.await`, so no two block locks are ever held at
    /// once.
    pub async fn build(
        served: &Served,
        parent: NodeID,
        parent_filter: FilterStates,
        parent_path: &RelativePath,
    ) -> Result<Arc<Listing>, FuseProviderError> {
        Self::build_with(served, parent, parent_filter, parent_path, None).await
    }

    /// [`Self::build`], running `walk_hook`, a test's, once the directory's log mark is read and
    /// before the walk, with no lock held.
    pub async fn build_with(
        served: &Served,
        parent: NodeID,
        parent_filter: FilterStates,
        parent_path: &RelativePath,
        walk_hook: Option<&ListingHook>,
    ) -> Result<Arc<Listing>, FuseProviderError> {
        let mut collected: Vec<CollectedChild> = Vec::new();
        let mut skipped_unrepresentable: u32 = 0;
        let mut skipped_staged: u32 = 0;
        let dir_block = served
            .state
            .block(served.repository.clone(), NodeBlock::index(parent))
            .await
            .forward_any::<FuseProviderError>("reading the directory")?;
        // The mark is read before the walk, so a patch landing during it is replayed.
        let (mark, _) = dir_block.watch_chain(Node::index(parent), None);
        if let Some(hook) = walk_hook {
            hook(parent).await;
        }

        // Boxed and type-erased: the load holds a children iterator of its own, and inline it
        // would grow the future of every request that can build a listing, in size and in the
        // type depth a caller's `Send` check walks.
        let load: BoxFuture<'_, _> = Box::pin(
            served
                .state
                .load_children_with_nametables(served.repository.clone(), parent),
        );
        let resident = load
            .await
            .forward_any::<FuseProviderError>("loading directory children")?;
        let mut iter = StateNodeChildrenWithNameIterator::new(
            served.state.clone(),
            served.repository.clone(),
            parent,
        )
        .await
        .forward_any::<FuseProviderError>("opening directory children")?;

        loop {
            let next = iter
                .next()
                .await
                .forward_any::<FuseProviderError>("reading next directory child")?;
            let Some((node_id, node, name_lock)) = next else {
                break;
            };

            // Staged-delete children are defensively excluded: a commit or
            // scan that has not yet swept them should not surface them.
            if node.is_staged_delete() {
                skipped_staged += 1;
                continue; // drops name_lock before the loop's next .await
            }
            if node.is_link() {
                return Err(FuseProviderError::from(NotSupported {
                    operation: "a FUSE mount of a revision holding a link".to_string(),
                }));
            }

            // The view match reads the lock's &str directly: it is a pure,
            // synchronous glob match, never an await point or a State call,
            // so doing it before the copy-and-drop below holds no lock across
            // anything it should not.
            let child_path = parent_path.join(&*name_lock);
            let (child_state, view_excluded) = child_filter(
                &served.repository,
                parent_filter,
                &child_path,
                node.is_directory(),
            );
            // A local change stays listed outside the view, as an ordinary working tree keeps a
            // local file a view leaves out: a sync narrowing the view past one refuses unless it
            // forwards local changes, which keeps it.
            let view_excluded = view_excluded && !holds_local(&node);
            let packed_filter_state = (filter_states_to_u8(child_state), view_line(child_state));

            let name: SmallVec<[u8; 256]> = SmallVec::from_slice(name_lock.as_bytes());
            drop(name_lock);

            if !is_representable(&name) {
                skipped_unrepresentable += 1;
                continue;
            }
            let walk_index = collected.len() as u32;
            collected.push((
                node_id,
                node,
                name,
                packed_filter_state,
                view_excluded,
                walk_index,
            ));
        }
        let skipped_invalid = iter.skipped_invalid();
        if iter.may_repeat() {
            drop_repeated_nodes(&mut collected, |child| (child.0, child.5));
        }

        collected.sort_by(|a, b| {
            a.1.name_hash
                .cmp(&b.1.name_hash)
                .then_with(|| a.2.cmp(&b.2))
        });

        let mut block_indices: Vec<usize> = collected
            .iter()
            .map(|child| NodeBlock::index(child.0))
            .collect();
        block_indices.sort_unstable();
        block_indices.dedup();
        let mut blocks = Vec::with_capacity(block_indices.len());
        for &index in &block_indices {
            blocks.push(
                served
                    .state
                    .block(served.repository.clone(), index)
                    .await
                    .forward_any::<FuseProviderError>("reading a directory child")?,
            );
        }
        // The blocks of children left out stay held too: a walk by name reads them.
        for block in resident {
            if !blocks.iter().any(|held| Arc::ptr_eq(held, &block)) {
                blocks.push(block);
            }
        }

        let mut entries = Vec::with_capacity(collected.len());
        let mut node_ids = Vec::with_capacity(collected.len());
        let mut names: Vec<u8> = Vec::with_capacity(collected.iter().map(|c| c.2.len()).sum());

        for (node_id, node, name, packed_filter_state, view_excluded, _) in &collected {
            let name_offset = names.len();
            names.extend_from_slice(name);
            let mut entry = pack_entry(
                node,
                name_offset,
                name.len(),
                *packed_filter_state,
                *view_excluded,
            );
            entry.block = block_indices
                .binary_search(&NodeBlock::index(*node_id))
                .unwrap_or_default() as u32;
            entries.push(entry);
            node_ids.push(*node_id);
        }

        Ok(Arc::new(Listing {
            entries: entries.into_boxed_slice(),
            names: names.into_boxed_slice(),
            node_ids: node_ids.into_boxed_slice(),
            dir: parent,
            log_seq: mark.seq,
            detaches: mark.detaches,
            respells: mark.respells,
            skipped_unrepresentable,
            skipped_staged,
            skipped_invalid,
            dir_block: Some(dir_block),
            blocks: blocks.into_boxed_slice(),
        }))
    }
}

/// The entry of `node`, its name at `name_offset` in the arena and `name_len` long, its view
/// verdict packed with the line that decided it.
fn pack_entry(
    node: &Node,
    name_offset: usize,
    name_len: usize,
    (filter_state, view_line): (u8, u16),
    view_excluded: bool,
) -> ListingEntry {
    let mut flags = 0u8;
    if node.is_directory() {
        flags |= entry_flags::IS_DIR;
    }
    if view_excluded {
        flags |= entry_flags::VIEW_EXCLUDED;
    }
    if is_local(node) {
        flags |= entry_flags::LOCAL;
        if node.ext_flags().contains(NodeExtFlags::Symlink) {
            flags |= entry_flags::SYMLINK;
        }
    }
    ListingEntry {
        name_hash: node.name_hash,
        size: node.size,
        address: node.address,
        name_offset: name_offset as u32,
        name_len: name_len as u16,
        mode: node.mode,
        flags,
        filter_state,
        view_line,
        block: 0,
    }
}

/// Whether `node` is still the child `entry` lists: live, in `dir`, under the entry's name hash.
fn describes(entry: &ListingEntry, node: &Node, dir: NodeID) -> bool {
    !node.is_discarded() && node.parent == dir && node.name_hash == entry.name_hash
}

/// `listed` with the fields its node's record decides taken from `node`.
fn refreshed(listed: &ListingEntry, node: &Node) -> ListingEntry {
    let mut entry = Listing::entry_of(node, listed.name_len as usize);
    entry.name_offset = listed.name_offset;
    entry.flags |= listed.flags & entry_flags::VIEW_EXCLUDED;
    entry.filter_state = listed.filter_state;
    entry.view_line = listed.view_line;
    entry.block = listed.block;
    entry
}

/// Whether `node`'s content lives under `write/`.
fn is_local(node: &Node) -> bool {
    node.ext_flags().contains(NodeExtFlags::LocalContent)
}

/// Whether `node` is local content, or a directory holding a local change, which marks each
/// directory above it dirty: what a view leaves listed outside it.
pub(crate) fn holds_local(node: &Node) -> bool {
    is_local(node) || (node.is_directory() && node.is_dirty() && !node.is_dirty_delete())
}

/// Whether `name` can be represented as a directory entry: no `/` or NUL
/// (both reserved by the path syntax a lookup parses), not exactly `.` or
/// `..` (the synthetic self/parent entries `readdirplus` emits itself), and
/// no more than 255 bytes (`NAME_MAX` on the filesystems FUSE mounts onto).
///
/// A `State` never yields the first four: it refuses a name with `/` or NUL,
/// and `.` and `..`, on both the write and the read path. It does hold a name
/// of up to `MAX_NODE_NAME_LEN` bytes, so the length bound is the case this
/// function's skip path sees in practice, such as a non-ASCII name a Windows
/// client committed. The full predicate stays as the definition of an entry
/// FUSE can carry.
pub fn is_representable(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    if name == b"." || name == b".." {
        return false;
    }
    !name.iter().any(|&b| b == b'/' || b == 0)
}
