// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The host files of a writable [`FuseFilesystem`] and its write family. Local content lives at
//! its repository-relative path under `write/`, a real directory whose file system owns its
//! bytes, sizes and times; each host file is opened once per inode and kept in the descriptor
//! cache for the kernel's zero-message opens. A name changes in the filesystem State
//! (`node_attach`, `node_detach`, `node_reattach`) together with its host entry; UNLINK and RMDIR
//! of a name the revision may hold, and RENAME away from one, append it to the deletion list.
//!
//! Each request of the write family takes a `fetch` flag. Without it, as on the FUSE worker, a
//! directory with no cached listing, an emptiness check whose chain is not in memory, or a
//! copy-up that keeps any tracked content answers [`ReadFault::WouldFetch`] before any change; an
//! attach whose walk for the name or served mark meets a block not in memory answers it too,
//! attaching nothing, with the directories its mark reached left marked.
//! A stale cached listing answers a name from the State instead, walking the blocks the listing
//! holds and the dirty blocks of nodes attached since, as the attach itself does; a copy-up that
//! keeps no content, as a truncate to zero, reads nothing and runs on the worker. The deferred
//! fetch builds the listings, loads the chain, and stages copy-up content ([`StagedCopy`]), and
//! the request then runs again with `fetch`, resolving everything afresh.

// The provider owns its host and staging files and writes them outside any repository write
// token.
#![allow(clippy::disallowed_methods)]

use std::ffi::CStr;
use std::ffi::CString;
use std::fs::File;
use std::os::fd::AsRawFd as _;
use std::os::fd::FromRawFd as _;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::FileExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use lore_base::text::fold_eq;
use lore_base::types::Address;
use lore_error_set::prelude::*;
use lore_fuse::abi::Attr;
use lore_fuse::abi::Kstatfs;
use lore_fuse::abi::SetattrIn;
use lore_fuse::abi::entry::setattr_valid;
use lore_fuse::reply::AttrReply;
use lore_fuse::reply::CreateReply;
use lore_fuse::reply::EntryReply;
use lore_fuse::reply::Errno;
use lore_fuse::reply::OpenReply;
use lore_fuse::reply::ReadFault;
use smallvec::SmallVec;

use crate::filter::FilterStates;
use crate::fs::fuse::FuseProviderError;
use crate::fs::fuse::attr::host_attr;
use crate::fs::fuse::filesystem::FuseFilesystem;
use crate::fs::fuse::filesystem::HostKind;
use crate::fs::fuse::filesystem::LookupCase;
use crate::fs::fuse::filesystem::MAX_DEPTH;
use crate::fs::fuse::filesystem::OnStale;
use crate::fs::fuse::filesystem::SettleOwed;
use crate::fs::fuse::filesystem::WriteStep;
use crate::fs::fuse::filesystem::holds_name;
use crate::fs::fuse::filesystem::scoped_in;
use crate::fs::fuse::filesystem::unnamed;
use crate::fs::fuse::inode::InodeEntry;
use crate::fs::fuse::inode::InodeKind;
use crate::fs::fuse::inode::InodeTable;
use crate::fs::fuse::inode::Name;
use crate::fs::fuse::inode::NewChild;
use crate::fs::fuse::inode::ROOT_INO;
use crate::fs::fuse::listing::Hit;
use crate::fs::fuse::listing::Listing;
use crate::fs::fuse::listing::ListingEntry;
use crate::fs::fuse::listing::entry_flags;
use crate::fs::fuse::listing::is_representable;
use crate::fs::fuse::local_tree::LocalKind;
use crate::fs::fuse::operation::ServedContent;
use crate::fs::fuse::operation::ServedFile;
use crate::fs::fuse::operation::StoredContent;
use crate::fs::fuse::served::Served;
use crate::fs::fuse::spelling::DEFAULT_TIMEOUTS;
use crate::fs::fuse::view::filter_states_to_u8;
use crate::fs::fuse::view::view_line;
use crate::fs::hold::SwapPhase;
use crate::hash::hash_string;
use crate::lore_warn;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeExtFlags;
use crate::node::NodeFileMode;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::node::validate_node_name_for_store;
use crate::repository::RepositoryContext;
use crate::state::Absent;
use crate::state::AttachCheck;
use crate::state::AttachOutcome;
use crate::state::CHAIN_WALK_RESTARTS;
use crate::state::ClearAgainst;
use crate::state::ClearOutcome;
use crate::state::ClearScratch;
use crate::state::Detach;
use crate::state::IfUnchanged;
use crate::state::NodeContent;
use crate::state::REATTACH_CHANGES;
use crate::state::ReattachCheck;
use crate::state::Reattached;
use crate::state::ServedClear;
use crate::state::State;
use crate::state::StateError;
use crate::state::StateNodeChildrenWithNameIterator;
use crate::util::path::RelativePath;

/// Where copy-ups materialize before they move under `write/`, beside it; emptied at mount.
pub const STAGING_DIRECTORY: &str = "write-staging";

/// Host files held open before unused ones are closed. The kernel's own FORGETs close most.
pub const CACHED_FILES: usize = 16384;

/// Most attempts a host write to a local file makes when a commit's flip turns the file
/// store-served, or replaces its descriptor, between the copy-up check and the write.
const HOST_WRITE_ATTEMPTS: u32 = 8;

/// The yields a host entry made below a directory being renamed waits, at most, for the rename
/// to end before the entry moves into the directory's new host path.
const RENAME_WAITS: u32 = 1024;

/// Releases of a name's claims [`FuseFilesystem::creates_finished`] waits for at most: the
/// claims of a create and the brief ones of the requests racing it, with room for more, while
/// claims churning without end cannot hold a write.
const CREATE_WAITS: u32 = 16;

/// What a move of a local file's host file answers where a commit's flip has turned the file
/// store-served since the request resolved it, so the request resolves the file again.
const FLIPPED: Errno = Errno(libc::ESTALE);

const PATH_BYTES: usize = libc::PATH_MAX as usize;

// RMDIR counts its checks in a byte.
const _: () = assert!(CHAIN_WALK_RESTARTS < u8::MAX as u32);

/// Most attempts [`FuseFilesystem::descriptor`] makes where the file it opens by an inode's path
/// is another file's: a RENAME of the file or of a directory above it, and a CREATE at the old
/// path, came between the path's build and the open. One such pair takes one attempt more.
const DESCRIPTOR_ATTEMPTS: u32 = 8;

/// What a host file opened by an inode's path is to the inode, read under its entry guard.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HostMatch {
    /// The inode's own host file.
    Own,
    /// The inode is not a local file: it has no host file, as one a commit turned store-served.
    NotLocal,
    /// The inode changed since the path was built: another incarnation holds the number, or the
    /// file turned store-served and local again.
    Changed,
    /// Another file's host file.
    Other,
    /// A local file whose host file nothing has identified yet.
    Unidentified,
}

impl HostMatch {
    /// What host file `opened` is to `entry`, which read `began` as its incarnation and host epoch
    /// when the path was built.
    fn of(entry: &InodeEntry, began: (u64, u64), opened: u64) -> Self {
        if !entry.kind.is_local_file() {
            Self::NotLocal
        } else if (entry.incarnation, entry.host_epoch) != began {
            Self::Changed
        } else if entry.host_file == opened {
            Self::Own
        } else if entry.host_file != 0 {
            Self::Other
        } else {
            Self::Unidentified
        }
    }
}

/// Shards of the copy-up gates. Each is given its table when the provider opens, so a RENAME over
/// a file, which takes a gate, never makes the first table of the shard its inode lands in.
const COPY_UP_GATE_SHARDS: usize = 16;
/// Gates a shard holds before its table grows: one per request taking one there at a time.
const COPY_UP_GATES_PER_SHARD: usize = 3;

#[lore_macro::test_pub]
struct CachedFile {
    file: Arc<File>,
    /// The inode's path no longer reaches this file, so it cannot be reopened.
    pinned: bool,
}

/// The host side of a writable provider: `write/`, held open, the staging directory beside it,
/// and the descriptor cache of the local files the kernel holds.
#[lore_macro::test_pub]
pub(crate) struct LocalFiles {
    root_path: PathBuf,
    root: OwnedFd,
    staging: PathBuf,
    files: DashMap<u64, CachedFile>,
    copy_ups: DashMap<u64, Arc<tokio::sync::Mutex<()>>>,
    staged: AtomicU64,
    /// Content fetched ahead of a deferred copy-up, by inode.
    copies: DashMap<u64, StagedCopy>,
    /// At least the number of entries in `copies`, so a write finding it zero skips the map.
    copies_held: AtomicUsize,
    /// Names where a node of any kind may stand while the inode the kernel reaches it through
    /// is not indexed there yet: a CREATE's, MKNOD's, MKDIR's or SYMLINK's, whose node is
    /// attached before its inode is made, and a RENAME's target, whose node is moved before its
    /// inode. The request claims the name before it changes the State and releases it once the
    /// inode stands at the name or the request failed. A LOOKUP or READDIRPLUS that finds the
    /// name missing, or held by an inode not of the node, while it is claimed waits for one
    /// release or defers ([`FuseFilesystem::child_matching`]); a commit's flip of a file no inode
    /// holds leaves a claimed file alone.
    pub(crate) arriving: PlaceClaims,
    /// Names whose host file a commit's flip of a file no inode holds has taken aside while it
    /// settles the file, claimed one at a time before the host file goes and released once it is
    /// gone or back. A request that finds a local file's host file absent waits while its own
    /// name is claimed and starts again where it was released since the request began
    /// ([`FuseFilesystem::settled_at`]).
    pub(crate) settling: PlaceClaims,
    /// The names settled, recorded before their claims are released.
    settle_log: SettleLog,
    /// Told after each settling claim is released.
    settled: tokio::sync::Notify,
    /// Requests that waited for a settling claim, requests that started again for one, and
    /// waits begun.
    settle_waits: [AtomicU64; 3],
}

/// Slots of [`PlaceClaims`]; claims past them are counted instead.
const CLAIM_SLOTS: usize = 64;

/// Settles of files no inode held a commit runs between yields.
const SETTLES_BETWEEN_YIELDS: usize = 64;

/// Settles [`SettleLog`] remembers.
const SETTLE_RING: usize = 1024;

/// Low bits of a [`SettleLog`] entry holding the settle's number; the rest hold the name's key.
const SETTLE_NUMBER_BITS: u64 = 0xFF_FFFF;

/// The latest settles of a commit's flips and of the directories whose host directories its
/// settle removes, by number, each with the name it settled. One commit writes it at a time,
/// serialized with swaps, each entry before the count that covers it; requests read it.
/// Lock-free and allocating nothing.
#[lore_macro::test_pub]
pub(crate) struct SettleLog {
    /// Settles recorded so far.
    settled: AtomicU64,
    /// By settle number modulo the ring size, the settled name's key over the settle's number,
    /// in one word ([`SETTLE_NUMBER_BITS`]).
    ring: Box<[AtomicU64]>,
}

impl SettleLog {
    fn new() -> Self {
        Self {
            settled: AtomicU64::new(0),
            ring: (0..SETTLE_RING).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    /// Settles recorded so far, which [`Self::settled_since`] compares.
    pub(crate) fn epoch(&self) -> u64 {
        self.settled.load(Ordering::SeqCst)
    }

    /// Records a settle of the name at `place`, while its claim is still held, or before its host
    /// directory goes.
    fn record(&self, place: (NodeID, u64)) {
        let number = self.settled.load(Ordering::SeqCst) + 1;
        self.ring[number as usize % SETTLE_RING].store(
            (PlaceClaims::key(place) & !SETTLE_NUMBER_BITS) | (number & SETTLE_NUMBER_BITS),
            Ordering::SeqCst,
        );
        self.settled.store(number, Ordering::SeqCst);
    }

    /// Whether the name at `place` may have been settled since [`Self::epoch`] answered `epoch`:
    /// the ring holds such a settle, or no longer holds every settle since.
    fn settled_since(&self, place: (NodeID, u64), epoch: u64) -> bool {
        let now = self.epoch();
        if now == epoch {
            return false;
        }
        if now - epoch > SETTLE_RING as u64 {
            return true;
        }
        let key = PlaceClaims::key(place) & !SETTLE_NUMBER_BITS;
        ((epoch + 1)..=now).any(|number| {
            let entry = self.ring[number as usize % SETTLE_RING].load(Ordering::SeqCst);
            entry & SETTLE_NUMBER_BITS != number & SETTLE_NUMBER_BITS
                || entry & !SETTLE_NUMBER_BITS == key
        })
    }
}

/// Names claimed, by directory node and name hash. Fixed slots, lock-free and allocating
/// nothing; a claim finding none free counts in `overflow`, which reads as every name claimed.
/// A slot's word is a name's key above a generation its releases count up: a claim takes a free
/// word, its key bits zero, by compare-and-swap, and its release stores the next free word.
///
/// A claim's release follows what the claimer did under it, so a reader that sees the release
/// sees that work: the store that frees a slot and the read-modify-writes that lower `held` or
/// `overflow` are each the release or in its release sequence, and a slot is retaken only by
/// compare-and-swap. A plain store to `held` or `overflow`, or to a slot by any but its
/// claimer, would break this.
#[lore_macro::test_pub]
pub(crate) struct PlaceClaims {
    slots: Box<[AtomicU64]>,
    /// Claims held, counted before a claim takes its slot and after it gives it up, so a zero
    /// answers [`Self::holds`] in one load.
    held: AtomicUsize,
    /// What only a busy set reads, boxed so the set stays small inline in a provider.
    wake: Box<ClaimWake>,
}

/// The low bits of a [`PlaceClaims`] slot word: its generation.
const CLAIM_GENERATION: u64 = (1 << 16) - 1;

/// What only a busy [`PlaceClaims`] reads: the claims past its slots and the waits for a
/// release.
#[lore_macro::test_pub]
struct ClaimWake {
    overflow: AtomicUsize,
    /// The claims past the slots that were released, each counted after `overflow` drops.
    overflow_releases: AtomicU64,
    /// Requests waiting in [`PlaceClaims::released`]; a release tells `released` only while
    /// nonzero.
    waiters: AtomicUsize,
    released: tokio::sync::Notify,
}

/// Where [`PlaceClaims::arrival`] found a name claimed, to wait for one release there.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug)]
pub(crate) enum Arrival {
    /// In slot `.0`, whose word, the claim's key and generation, read `.1`.
    Slot(usize, u64),
    /// Past the slots, whose release count read `.0` before the overflow did.
    Overflow(u64),
}

impl PlaceClaims {
    #[lore_macro::test_pub]
    fn new() -> Self {
        Self {
            slots: (0..CLAIM_SLOTS).map(|_| AtomicU64::new(0)).collect(),
            held: AtomicUsize::new(0),
            wake: Box::new(ClaimWake {
                overflow: AtomicUsize::new(0),
                overflow_releases: AtomicU64::new(0),
                waiters: AtomicUsize::new(0),
                released: tokio::sync::Notify::new(),
            }),
        }
    }

    /// Whether any name may be claimed.
    fn busy(&self) -> bool {
        self.held.load(Ordering::SeqCst) > 0
    }

    /// The nonzero key of the name hashed `name_hash` in directory node `dir`.
    fn key((dir, name_hash): (NodeID, u64)) -> u64 {
        (name_hash ^ u64::from(dir).rotate_left(32)) | 1
    }

    /// The key bits of a slot word claiming the name at `place`: above the generation, the
    /// lowest of them set so a taken word is never free.
    fn slot_key(place: (NodeID, u64)) -> u64 {
        (Self::key(place) & !CLAIM_GENERATION) | (CLAIM_GENERATION + 1)
    }

    /// Claims the name at `place` until the claim is dropped.
    #[lore_macro::test_pub]
    fn claim(&self, place: (NodeID, u64)) -> PlaceClaim<'_> {
        self.held.fetch_add(1, Ordering::SeqCst);
        let key = Self::slot_key(place);
        let slot = self.slots.iter().position(|slot| {
            let free = slot.load(Ordering::SeqCst);
            free & !CLAIM_GENERATION == 0
                && slot
                    .compare_exchange(free, key | free, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok()
        });
        if slot.is_none() {
            self.wake.overflow.fetch_add(1, Ordering::SeqCst);
        }
        PlaceClaim { set: self, slot }
    }

    /// Whether the name at `place` may be claimed.
    #[lore_macro::test_pub]
    pub(crate) fn holds(&self, place: (NodeID, u64)) -> bool {
        if !self.busy() {
            return false;
        }
        let key = Self::slot_key(place);
        self.wake.overflow.load(Ordering::SeqCst) > 0
            || self
                .slots
                .iter()
                .any(|slot| slot.load(Ordering::SeqCst) & !CLAIM_GENERATION == key)
    }

    /// Where the name at `place` may be claimed: a slot holding it, with the slot's word, else
    /// the overflow while any claim is past the slots; `None` where it is not. A slot's word
    /// changes only with the release of the claim it shows, which moves its generation, so a
    /// release of another name's claim from the same slot cannot end the wait. The overflow's
    /// release count is read before the overflow, so a release after this call moves it. One
    /// load where no claim is held.
    #[lore_macro::test_pub]
    pub(crate) fn arrival(&self, place: (NodeID, u64)) -> Option<Arrival> {
        self.arrival_scanning(place, |_| {})
    }

    /// [`Self::arrival`], running `scanned` with each slot's index once its word is read.
    #[lore_macro::test_pub]
    fn arrival_scanning(
        &self,
        place: (NodeID, u64),
        mut scanned: impl FnMut(usize),
    ) -> Option<Arrival> {
        if !self.busy() {
            return None;
        }
        let key = Self::slot_key(place);
        for (index, slot) in self.slots.iter().enumerate() {
            let word = slot.load(Ordering::SeqCst);
            scanned(index);
            if word & !CLAIM_GENERATION == key {
                return Some(Arrival::Slot(index, word));
            }
        }
        let seen = self.wake.overflow_releases.load(Ordering::SeqCst);
        (self.wake.overflow.load(Ordering::SeqCst) > 0).then_some(Arrival::Overflow(seen))
    }

    /// Waits for one release where `arrival` found the name claimed: the slot's claim, or any
    /// claim past the slots. Woken through `released`, created before each check so a release
    /// between them still wakes it, and counted in `waiters` until it returns or is dropped.
    /// While any waiter is counted, each release wakes every waiter, which checks its own slot:
    /// a herd only where waits pile up, as under overflow.
    #[lore_macro::test_pub]
    pub(crate) async fn released(&self, arrival: Arrival) {
        /// Counts a waiter down however the wait ends.
        struct Waiting<'a>(&'a AtomicUsize);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let wake = &self.wake;
        wake.waiters.fetch_add(1, Ordering::SeqCst);
        let _waiting = Waiting(&wake.waiters);
        loop {
            let notified = wake.released.notified();
            let moved = match arrival {
                Arrival::Slot(index, word) => self.slots[index].load(Ordering::SeqCst) != word,
                Arrival::Overflow(seen) => wake.overflow_releases.load(Ordering::SeqCst) != seen,
            };
            if moved {
                return;
            }
            notified.await;
        }
    }

    /// Requests waiting in [`Self::released`]. For tests.
    #[cfg(feature = "test-util")]
    pub fn waiters(&self) -> usize {
        self.wake.waiters.load(Ordering::SeqCst)
    }
}

/// A claim on a name ([`PlaceClaims::claim`]), released when dropped.
#[lore_macro::test_pub]
struct PlaceClaim<'a> {
    set: &'a PlaceClaims,
    /// Its slot, or `None` past the slots.
    slot: Option<usize>,
}

impl Drop for PlaceClaim<'_> {
    fn drop(&mut self) {
        let set = self.set;
        if let Some(index) = self.slot {
            let slot = &set.slots[index];
            // Its own word, as only the claimer writes a taken slot: freed a generation on.
            let word = slot.load(Ordering::Relaxed);
            slot.store(word.wrapping_add(1) & CLAIM_GENERATION, Ordering::SeqCst);
        } else {
            set.wake.overflow.fetch_sub(1, Ordering::SeqCst);
            set.wake.overflow_releases.fetch_add(1, Ordering::SeqCst);
        }
        set.held.fetch_sub(1, Ordering::SeqCst);
        // One load where nobody waits.
        if set.wake.waiters.load(Ordering::SeqCst) > 0 {
            set.wake.released.notify_waiters();
        }
    }
}

/// A claim on a name taken by a test ([`FuseFilesystem::claim_arriving`]), released when
/// dropped.
pub struct ArrivingClaim<'a> {
    _claim: PlaceClaim<'a>,
}

/// A settle's claim on its name ([`LocalFiles::settling`]): dropped, even by a panic, it releases
/// the claim and then wakes the requests waiting on it.
#[lore_macro::test_pub]
struct Settling<'a> {
    claim: Option<PlaceClaim<'a>>,
    settled: &'a tokio::sync::Notify,
}

impl Drop for Settling<'_> {
    fn drop(&mut self) {
        drop(self.claim.take());
        self.settled.notify_waiters();
    }
}

/// A directory RENAME counted under way on its inode
/// ([`InodeEntry::renames`](crate::fs::fuse::inode::InodeEntry::renames)) until dropped.
struct RenameUnderWay<'a> {
    inodes: &'a InodeTable,
    ino: u64,
    incarnation: u64,
}

impl<'a> RenameUnderWay<'a> {
    fn begin(inodes: &'a InodeTable, ino: u64) -> Option<Self> {
        let entry = inodes.get(ino)?;
        entry.renames.fetch_add(1, Ordering::SeqCst);
        entry.rename_changes.fetch_add(1, Ordering::SeqCst);
        Some(Self {
            inodes,
            ino,
            incarnation: entry.incarnation,
        })
    }
}

impl Drop for RenameUnderWay<'_> {
    fn drop(&mut self) {
        if let Some(entry) = self.inodes.get(self.ino)
            && entry.incarnation == self.incarnation
        {
            entry.rename_changes.fetch_add(1, Ordering::SeqCst);
            entry.renames.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// The renames of a directory and every directory above it, as one walk read them.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct RenameWatch {
    /// The change counters added: each only grows, so the sum moves where any one does.
    changes: u64,
    /// Whether a rename of any of them was under way.
    under_way: bool,
}

impl RenameWatch {
    fn add(&mut self, entry: &InodeEntry) {
        let changes = entry.rename_changes.load(Ordering::SeqCst);
        self.changes = self.changes.wrapping_add(changes);
        self.under_way |= entry.renames.load(Ordering::SeqCst) != 0;
    }
}

/// A host entry made under a path built while a rename of a directory above was under way, or
/// came between the build and the entry ([`FuseFilesystem::settle_host_entry`]).
struct RacedEntry {
    /// The path the entry was made at.
    path: String,
    /// How many of the deepest directories of `path` making the entry created.
    made_dirs: usize,
}

/// A local file's host file taken aside into the staging directory, its directory held open
/// so the file can go back into it wherever the directory has moved meanwhile. Dropped before it
/// is settled, as by a panic, it puts the file back.
#[lore_macro::test_pub]
struct TakenAside<'a> {
    local: &'a LocalFiles,
    /// The directory; `None` for `write/` itself.
    dir: Option<OwnedFd>,
    name: CString,
    staged: PathBuf,
    settled: bool,
}

impl TakenAside<'_> {
    /// Removes the staged file where the settle flipped the file, else puts it back.
    fn settle(mut self, flipped: bool) -> Result<(), Errno> {
        self.settled = true;
        if flipped {
            std::fs::remove_file(&self.staged).map_err(|error| io_errno(&error))
        } else {
            self.local.put_back(&self)
        }
    }
}

impl Drop for TakenAside<'_> {
    fn drop(&mut self) {
        if !self.settled
            && let Err(errno) = self.local.put_back(self)
        {
            lore_warn!(
                "FUSE could not put {} back under write/: {errno:?}",
                self.staged.display()
            );
        }
    }
}

impl LocalFiles {
    /// Opens `write`, creating it on first use, and empties the staging directory beside it.
    /// Blocking.
    #[lore_macro::test_pub]
    pub(crate) fn open(write: &Path) -> Result<Self, FuseProviderError> {
        let root_path = write.to_path_buf();
        let staging = write.with_file_name(STAGING_DIRECTORY);
        std::fs::create_dir_all(&root_path)
            .map_err(|error| open_failed("creating", &root_path, &error))?;
        if let Err(error) = std::fs::remove_dir_all(&staging)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(open_failed("emptying", &staging, &error));
        }
        std::fs::create_dir_all(&staging)
            .map_err(|error| open_failed("creating", &staging, &error))?;
        let root = File::open(&root_path)
            .map_err(|error| open_failed("opening", &root_path, &error))?
            .into();
        Ok(Self {
            root_path,
            root,
            staging,
            files: DashMap::new(),
            copy_ups: DashMap::with_capacity_and_shard_amount(
                COPY_UP_GATE_SHARDS * COPY_UP_GATES_PER_SHARD,
                COPY_UP_GATE_SHARDS,
            ),
            staged: AtomicU64::new(0),
            copies: DashMap::new(),
            copies_held: AtomicUsize::new(0),
            arriving: PlaceClaims::new(),
            settling: PlaceClaims::new(),
            settle_log: SettleLog::new(),
            settled: tokio::sync::Notify::new(),
            settle_waits: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
        })
    }

    /// `write/` itself.
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// The staging directory beside `write/`, on `write/`'s file system.
    pub fn staging_dir(&self) -> &Path {
        &self.staging
    }

    /// The cached host file of inode `ino`.
    pub fn file(&self, ino: u64) -> Option<Arc<File>> {
        self.files.get(&ino).map(|cached| cached.file.clone())
    }

    /// Caches `file` as inode `ino`'s host file, closing unused ones past [`CACHED_FILES`]. A
    /// file pinned for the inode stays: its path no longer reaches it.
    pub fn cache_file(&self, ino: u64, file: Arc<File>) {
        self.insert_file(ino, file);
        self.evict_past_budget();
    }

    /// Caches `file` as inode `ino`'s host file, closing none. A file pinned for the inode stays:
    /// its path no longer reaches it. A caller holding an inode guard runs
    /// [`Self::evict_past_budget`] once the guard is released.
    pub fn insert_file(&self, ino: u64, file: Arc<File>) {
        // One shard write lock, inside an inode shard guard at most, and nothing taken within.
        #[allow(clippy::disallowed_methods)]
        match self.files.entry(ino) {
            dashmap::Entry::Occupied(held) if held.get().pinned => {}
            dashmap::Entry::Occupied(mut held) => {
                held.insert(CachedFile {
                    file,
                    pinned: false,
                });
            }
            dashmap::Entry::Vacant(vacant) => {
                vacant.insert(CachedFile {
                    file,
                    pinned: false,
                });
            }
        }
    }

    /// Closes unused cached files once more than [`CACHED_FILES`] are held.
    pub fn evict_past_budget(&self) {
        if self.files.len() > CACHED_FILES {
            self.evict();
        }
    }

    /// Keeps `file` as inode `ino`'s host file until the inode is forgotten, for an inode whose
    /// path is about to stop reaching it.
    pub fn pin_file(&self, ino: u64, file: Arc<File>) {
        self.files.insert(ino, CachedFile { file, pinned: true });
    }

    /// Lets inode `ino`'s host file close with the unused ones again, its path reaching it.
    #[lore_macro::test_pub]
    fn unpin(&self, ino: u64) {
        if let Some(mut cached) = self.files.get_mut(&ino) {
            cached.pinned = false;
        }
    }

    /// Closes inode `ino`'s host file, and drops content staged for it, once the kernel holds
    /// the inode no more.
    pub fn forget(&self, ino: u64) {
        drop(self.take_forgotten(ino));
    }

    /// Inode `ino`'s cached host file and the content staged for it, taken out as the inode
    /// leaves the table; dropped, they close the file and remove the staged copy.
    pub(crate) fn take_forgotten(&self, ino: u64) -> (Option<Arc<File>>, Option<StagedCopy>) {
        let file = self.files.remove(&ino).map(|(_, cached)| cached.file);
        (file, self.take_staged(ino))
    }

    /// Keeps `copy`, content staged for inode `ino`'s copy-up; a copy it replaces is removed.
    fn put_staged(&self, ino: u64, copy: StagedCopy) {
        self.copies_held.fetch_add(1, Ordering::AcqRel);
        if self.copies.insert(ino, copy).is_some() {
            self.copies_held.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// How many copy-ups hold staged content.
    pub fn staged_copies(&self) -> usize {
        self.copies_held.load(Ordering::Acquire)
    }

    /// The content staged for inode `ino`'s copy-up, taken.
    fn take_staged(&self, ino: u64) -> Option<StagedCopy> {
        if self.copies_held.load(Ordering::Acquire) == 0 {
            return None;
        }
        let (_, copy) = self.copies.remove(&ino)?;
        self.copies_held.fetch_sub(1, Ordering::AcqRel);
        Some(copy)
    }

    /// Closes inode `ino`'s cached host file if it is open read-only and its path still reaches
    /// it, so that the next use opens it read-write.
    pub fn drop_read_only(&self, ino: u64) {
        self.files.remove_if(&ino, |_, cached| {
            !cached.pinned && opened_read_only(&cached.file)
        });
    }

    /// Serializes copy-ups of inode `ino`.
    #[lore_macro::test_pub]
    fn copy_up_gate(&self, ino: u64) -> Arc<tokio::sync::Mutex<()>> {
        // One shard write lock, and nothing else is locked while it is held.
        self.copy_ups.entry(ino).or_default().clone()
    }

    /// Lets inode `ino`'s copy-up gate go once no other copy-up holds or waits on it: the map
    /// and the caller hold the only references.
    #[lore_macro::test_pub]
    fn release_copy_up_gate(&self, ino: u64) {
        self.copy_ups
            .remove_if(&ino, |_, gate| Arc::strong_count(gate) <= 2);
    }

    /// A fresh path in the staging directory, on `write/`'s file system.
    fn staging_path(&self) -> PathBuf {
        let n = self.staged.fetch_add(1, Ordering::Relaxed);
        self.staging.join(n.to_string())
    }

    /// Opens the host file at `path`, read-write unless its mode refuses writing.
    fn open_existing(&self, path: &HostPath) -> Result<File, Errno> {
        match self.open_at(path, libc::O_RDWR, 0) {
            Err(Errno::EACCES) => self.open_at(path, libc::O_RDONLY, 0),
            opened => opened,
        }
    }

    /// Creates the host file at `path`, which must not exist, open read-write.
    fn create_file(&self, path: &HostPath, mode: u32) -> Result<File, Errno> {
        let file = self.open_at(path, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, mode)?;
        // The service's umask would otherwise narrow the mode the kernel already masked.
        set_file_mode(&file, mode)?;
        Ok(file)
    }

    fn open_at(&self, path: &HostPath, flags: i32, mode: u32) -> Result<File, Errno> {
        let path = path.as_cstr()?;
        // SAFETY: `root` is an open directory and `path` is NUL-terminated.
        let fd = unsafe {
            libc::openat(
                self.root.as_raw_fd(),
                path.as_ptr(),
                flags | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(last_errno());
        }
        // SAFETY: `fd` was just opened and nothing else owns it.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn mkdir(&self, path: &HostPath, mode: u32) -> Result<(), Errno> {
        let path = path.as_cstr()?;
        // SAFETY: `root` is an open directory and `path` is NUL-terminated.
        check(unsafe { libc::mkdirat(self.root.as_raw_fd(), path.as_ptr(), mode) })?;
        self.chmod(path, mode)
    }

    fn symlink(&self, target: &[u8], path: &HostPath) -> Result<(), Errno> {
        let target = CString::new(target).map_err(|_nul| Errno::EINVAL)?;
        let path = path.as_cstr()?;
        // SAFETY: `root` is an open directory and both strings are NUL-terminated.
        check(unsafe { libc::symlinkat(target.as_ptr(), self.root.as_raw_fd(), path.as_ptr()) })
    }

    fn remove_path(&self, path: &HostPath, dir: bool) -> Result<(), Errno> {
        let path = path.as_cstr()?;
        let flags = if dir { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: `root` is an open directory and `path` is NUL-terminated.
        check(unsafe { libc::unlinkat(self.root.as_raw_fd(), path.as_ptr(), flags) })
    }

    fn rename(&self, from: &HostPath, to: &HostPath) -> Result<(), Errno> {
        let (from, to) = (from.as_cstr()?, to.as_cstr()?);
        let root = self.root.as_raw_fd();
        // SAFETY: `root` is an open directory and both paths are NUL-terminated.
        check(unsafe { libc::renameat(root, from.as_ptr(), root, to.as_ptr()) })
    }

    /// [`Self::rename`] that replaces nothing: `EEXIST` where `to` exists.
    fn rename_no_replace(&self, from: &HostPath, to: &HostPath) -> Result<(), Errno> {
        let (from, to) = (from.as_cstr()?, to.as_cstr()?);
        let root = self.root.as_raw_fd();
        // SAFETY: `root` is an open directory and both paths are NUL-terminated.
        check(unsafe {
            libc::renameat2(
                root,
                from.as_ptr(),
                root,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        })
    }

    /// Moves `staged`, a path from [`Self::staging_path`], to `to`.
    fn rename_staged(&self, staged: &Path, to: &HostPath) -> Result<(), Errno> {
        let staged = CString::new(staged.as_os_str().as_bytes()).map_err(|_nul| Errno::EINVAL)?;
        let to = to.as_cstr()?;
        // SAFETY: `staged` is absolute, `root` an open directory, both paths NUL-terminated.
        check(unsafe {
            libc::renameat(
                libc::AT_FDCWD,
                staged.as_ptr(),
                self.root.as_raw_fd(),
                to.as_ptr(),
            )
        })
    }

    /// Takes the host file of local file `path` aside into the staging directory, its directory
    /// held open ([`TakenAside`]).
    #[lore_macro::test_pub]
    fn take_aside(&self, path: &RelativePath) -> Result<TakenAside<'_>, Errno> {
        let (dir, name) = match path.as_str().rsplit_once('/') {
            Some((parent, name)) => {
                let parent = relative_host_path(&path.parent_path())
                    .filter(|_| !parent.is_empty())
                    .ok_or(Errno::ENOENT)?;
                let parent = parent.as_cstr()?;
                // SAFETY: `root` is an open directory, `parent` NUL-terminated.
                let fd = unsafe {
                    libc::openat(
                        self.root.as_raw_fd(),
                        parent.as_ptr(),
                        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(last_errno());
                }
                // SAFETY: `fd` was just opened and is owned here alone.
                (Some(unsafe { OwnedFd::from_raw_fd(fd) }), name)
            }
            None => (None, path.as_str()),
        };
        let name = CString::new(name).map_err(|_nul| Errno::EINVAL)?;
        let staged = self.staging_path();
        let to = CString::new(staged.as_os_str().as_bytes()).map_err(|_nul| Errno::EINVAL)?;
        let at = dir
            .as_ref()
            .map_or(self.root.as_raw_fd(), |dir| dir.as_raw_fd());
        // SAFETY: `at` is an open directory, `to` absolute, both paths NUL-terminated.
        check(unsafe { libc::renameat(at, name.as_ptr(), libc::AT_FDCWD, to.as_ptr()) })?;
        Ok(TakenAside {
            local: self,
            dir,
            name,
            staged,
            settled: false,
        })
    }

    /// Puts a host file [`Self::take_aside`] took back into its directory, unless something
    /// took its name meanwhile.
    fn put_back(&self, aside: &TakenAside<'_>) -> Result<(), Errno> {
        let staged =
            CString::new(aside.staged.as_os_str().as_bytes()).map_err(|_nul| Errno::EINVAL)?;
        let at = aside
            .dir
            .as_ref()
            .map_or(self.root.as_raw_fd(), |dir| dir.as_raw_fd());
        // SAFETY: `staged` is absolute, `at` an open directory, both paths NUL-terminated.
        check(unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                staged.as_ptr(),
                at,
                aside.name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        })
    }

    /// Moves what `path` names to a fresh staging path, which it answers.
    fn move_to_staging(&self, path: &HostPath) -> Result<PathBuf, Errno> {
        let staged = self.staging_path();
        let to = CString::new(staged.as_os_str().as_bytes()).map_err(|_nul| Errno::EINVAL)?;
        let from = path.as_cstr()?;
        // SAFETY: `root` is an open directory, `to` absolute, both paths NUL-terminated.
        check(unsafe {
            libc::renameat(
                self.root.as_raw_fd(),
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
            )
        })?;
        Ok(staged)
    }

    /// `stat` of what `path` names, not following a symlink.
    fn stat(&self, path: &HostPath) -> Result<libc::stat, Errno> {
        let path = path.as_cstr()?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `root` is an open directory, `path` NUL-terminated, `stat` a valid out-pointer.
        check(unsafe {
            libc::fstatat(
                self.root.as_raw_fd(),
                path.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })?;
        // SAFETY: `fstatat` succeeded, so it filled `stat`.
        Ok(unsafe { stat.assume_init() })
    }

    /// Writes the target of the symlink at `path` into `out`; answers its length.
    fn readlink(&self, path: &HostPath, out: &mut [u8]) -> Result<usize, Errno> {
        let path = path.as_cstr()?;
        // SAFETY: `root` is an open directory, `path` NUL-terminated, `out` writable for its
        // length.
        let n = unsafe {
            libc::readlinkat(
                self.root.as_raw_fd(),
                path.as_ptr(),
                out.as_mut_ptr().cast(),
                out.len(),
            )
        };
        usize::try_from(n).map_err(|_negative| last_errno())
    }

    fn set_mode(&self, path: &HostPath, mode: u32) -> Result<(), Errno> {
        self.chmod(path.as_cstr()?, mode)
    }

    fn chmod(&self, path: &CStr, mode: u32) -> Result<(), Errno> {
        // SAFETY: `root` is an open directory and `path` is NUL-terminated.
        check(unsafe { libc::fchmodat(self.root.as_raw_fd(), path.as_ptr(), mode & 0o7777, 0) })
    }

    /// Sets the access and modification times of what `path` names, not following a symlink.
    fn set_times(&self, path: &HostPath, times: &[libc::timespec; 2]) -> Result<(), Errno> {
        let path = path.as_cstr()?;
        // SAFETY: `root` is an open directory, `path` NUL-terminated, `times` two timespecs.
        check(unsafe {
            libc::utimensat(
                self.root.as_raw_fd(),
                path.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })
    }

    /// `statvfs` of `write/`'s file system.
    fn statvfs(&self) -> Result<libc::statvfs, Errno> {
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: `root` is an open fd and `stat` a valid out-pointer.
        check(unsafe { libc::fstatvfs(self.root.as_raw_fd(), stat.as_mut_ptr()) })?;
        // SAFETY: `fstatvfs` succeeded, so it filled `stat`.
        Ok(unsafe { stat.assume_init() })
    }

    /// Closes cached files no request holds, not pinned, until a quarter of the budget is free.
    fn evict(&self) {
        let mut excess = self.files.len().saturating_sub(CACHED_FILES * 3 / 4);
        self.files.retain(|_, cached| {
            if excess == 0 || cached.pinned || Arc::strong_count(&cached.file) > 1 {
                return true;
            }
            excess -= 1;
            false
        });
    }
}

/// A tracked file's content at `address`, or its first `kept` bytes, fetched into the staging
/// directory ahead of its copy-up. Dropped, it removes its file, so a fetch that is interrupted
/// or overtaken leaves nothing behind.
#[lore_macro::test_pub]
pub(crate) struct StagedCopy {
    address: Address,
    /// How many of the content's first bytes the file holds.
    kept: u64,
    path: Option<PathBuf>,
}

impl StagedCopy {
    fn new(address: Address, kept: u64, path: PathBuf) -> Self {
        Self {
            address,
            kept,
            path: Some(path),
        }
    }

    fn path(&self) -> &Path {
        self.path.as_deref().unwrap_or(Path::new(""))
    }

    /// Gives up removing the staged file, once it has moved into place.
    fn moved(mut self) {
        self.path = None;
    }
}

impl Drop for StagedCopy {
    fn drop(&mut self) {
        if let Some(path) = self.path.take()
            && let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            lore_warn!("FUSE could not remove '{}': {error}", path.display());
        }
    }
}

/// A path below `write/`, built back to front in a fixed buffer and NUL-terminated, so building
/// one allocates nothing.
pub struct HostPath {
    bytes: [u8; PATH_BYTES],
    start: usize,
}

impl Default for HostPath {
    fn default() -> Self {
        Self::new()
    }
}

impl HostPath {
    /// `write/` itself.
    pub fn new() -> Self {
        Self {
            bytes: [0; PATH_BYTES],
            start: PATH_BYTES - 1,
        }
    }

    /// Puts `name` in front, as the outermost component so far.
    pub fn prepend(&mut self, name: &[u8]) -> Result<(), Errno> {
        let separator = usize::from(!self.is_root());
        if name.len() + separator > self.start {
            return Err(Errno::ENAMETOOLONG);
        }
        if separator == 1 {
            self.start -= 1;
            self.bytes[self.start] = b'/';
        }
        self.start -= name.len();
        self.bytes[self.start..self.start + name.len()].copy_from_slice(name);
        Ok(())
    }

    pub fn is_root(&self) -> bool {
        self.start == PATH_BYTES - 1
    }

    /// The path for the `*at` calls: `.` for `write/` itself.
    pub fn as_cstr(&self) -> Result<&CStr, Errno> {
        if self.is_root() {
            return Ok(c".");
        }
        CStr::from_bytes_with_nul(&self.bytes[self.start..]).map_err(|_nul| Errno::EINVAL)
    }

    /// The repository-relative path; empty for `write/` itself.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[self.start..PATH_BYTES - 1]).unwrap_or_default()
    }
}

/// What a name holds in its directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Found {
    Nothing,
    /// A file; `local` when its bytes live under `write/`.
    File {
        local: bool,
    },
    /// A directory; `local` when it was added under `write/`, so everything below it is local.
    Dir {
        local: bool,
    },
    Symlink,
}

impl Found {
    fn of_entry(entry: &ListingEntry) -> Self {
        let local = entry.flags & entry_flags::LOCAL != 0;
        if entry.flags & entry_flags::IS_DIR != 0 {
            Self::Dir { local }
        } else if entry.flags & entry_flags::SYMLINK != 0 {
            Self::Symlink
        } else {
            Self::File { local }
        }
    }

    fn is_dir(self) -> bool {
        matches!(self, Self::Dir { .. })
    }
}

/// A name resolved in its directory.
pub(crate) struct Resolved {
    pub found: Found,
    /// The spelling the State holds the name under; the asked one where it holds none.
    pub spelled: Name,
    /// The node holding the name.
    pub node: Option<NodeID>,
    /// For a name nothing holds, its absence as the listing and the directory's log showed it,
    /// where they could and it was asked for ([`FuseFilesystem::resolve_to_attach`]).
    pub absent: Option<Absent>,
}

impl Resolved {
    fn nothing(name: &str) -> Self {
        Self {
            found: Found::Nothing,
            spelled: Name::from_slice(name.as_bytes()),
            node: None,
            absent: None,
        }
    }
}

/// What `name` holds as `node_id`, read from `block` under one borrowed read guard: `None`
/// where the node no longer holds the name in `dir`, which the State answers by name instead.
/// A name the block cannot give back is `EIO`.
fn resolved_from_block(
    block: &NodeBlock,
    dir: NodeID,
    node_id: NodeID,
    name: &str,
) -> Result<Option<Resolved>, Errno> {
    block
        .read_node_and_name(Node::index(node_id), |node, held| {
            if !holds_name(node, dir, hash_string(name)) {
                return Ok(None);
            }
            let held = held?;
            if !fold_eq(held, name) {
                return Ok(Some(Resolved::nothing(name)));
            }
            Ok(Some(Resolved {
                found: Found::of_entry(&Listing::entry_of(node, 0)),
                spelled: Name::from_slice(held.as_bytes()),
                node: Some(node_id),
                absent: None,
            }))
        })
        .map_err(|error| unnamed(name, &error))
}

/// One side of a rename: the directory inode and its node, and the name.
#[derive(Clone, Copy)]
struct Side<'a> {
    dir: u64,
    node: NodeID,
    name: &'a str,
}

impl FuseFilesystem {
    /// A fresh path under `write-staging/` for content an instance write stages before it
    /// writes it through the request handlers; `None` on a read-only provider.
    pub(crate) fn instance_staging_path(&self) -> Option<PathBuf> {
        self.local_files().map(LocalFiles::staging_path)
    }

    fn writable(&self) -> Result<&LocalFiles, Errno> {
        self.local_files().ok_or(Errno::EROFS)
    }

    /// The settle log's epoch ([`SettleLog::epoch`]), read before a request reaches a local
    /// file's host file; 0 on a read-only provider.
    pub(crate) fn settle_epoch(&self) -> u64 {
        self.local_files()
            .map_or(0, |local| local.settle_log.epoch())
    }

    /// Whether a request that found the host file of the name at `place` absent starts again: it
    /// waits, for a notification, while a commit's flip holds that name's host file aside
    /// ([`LocalFiles::settling`]), and answers whether it did, or the name was settled since
    /// [`Self::settle_epoch`] answered `epoch`. A name no settle touched costs no wait.
    pub(crate) async fn settled_at(&self, place: (NodeID, u64), epoch: u64) -> bool {
        let Some(local) = self.local_files() else {
            return false;
        };
        let mut waited = false;
        loop {
            let notified = local.settled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !local.settling.holds(place) {
                break;
            }
            waited = true;
            local.settle_waits[2].fetch_add(1, Ordering::SeqCst);
            notified.await;
        }
        let again = waited || local.settle_log.settled_since(place, epoch);
        if waited {
            local.settle_waits[0].fetch_add(1, Ordering::Relaxed);
        }
        if again {
            local.settle_waits[1].fetch_add(1, Ordering::Relaxed);
        }
        again
    }

    /// [`Self::settled_at`] for `name` in directory inode `parent`.
    pub(crate) async fn settled_at_name(&self, parent: u64, name: &[u8], epoch: u64) -> bool {
        let (Ok(name), Ok((dir, _))) = (std::str::from_utf8(name), self.dir_node(parent)) else {
            return false;
        };
        self.settled_at((dir, hash_string(name)), epoch).await
    }

    /// [`Self::settled_at`] for the name of inode `ino`.
    pub(crate) async fn settled_at_inode(&self, ino: u64, epoch: u64) -> bool {
        let Some((parent, name_hash)) = self
            .inodes()
            .get(ino)
            .map(|entry| (entry.parent, entry.name_hash))
        else {
            return false;
        };
        let Ok((dir, _)) = self.dir_node(parent) else {
            return false;
        };
        self.settled_at((dir, name_hash), epoch).await
    }

    /// Where a request moving a node to the name at `place` may be under way, to wait for one
    /// release there ([`PlaceClaims::arrival`]); `None` where none is. One load where no claim is
    /// held.
    pub(crate) fn arrival_at(&self, place: (NodeID, u64)) -> Option<Arrival> {
        self.local_files()?.arriving.arrival(place)
    }

    /// Waits for one release where [`Self::arrival_at`] answered `arrival`.
    pub(crate) async fn arrived(&self, arrival: Arrival) {
        if let Some(local) = self.local_files() {
            local.arriving.released(arrival).await;
        }
    }

    /// Waits until no create or rename holds the claim on `name` in directory inode `parent`,
    /// looking again after each release, at most [`CREATE_WAITS`] releases; at once where none
    /// does. A create holds the claim from before its host entry until its inode is made
    /// ([`Self::create_node`]), and others racing it to the name hold theirs briefly too, so one
    /// release may be theirs.
    pub(crate) async fn creates_finished(&self, parent: u64, name: &str) {
        let Ok((dir, _)) = self.dir_node(parent) else {
            return;
        };
        let place = (dir, hash_string(name));
        for _ in 0..CREATE_WAITS {
            let Some(arrival) = self.arrival_at(place) else {
                return;
            };
            self.write_step_allowed(WriteStep::AwaitingCreate { parent });
            self.arrived(arrival).await;
        }
    }

    /// Claims `name` in directory inode `parent` as a request moving a node there does, until
    /// the claim is dropped. For tests.
    pub fn claim_arriving(&self, parent: u64, name: &str) -> Option<ArrivingClaim<'_>> {
        let local = self.local_files()?;
        let (dir, _) = self.dir_node(parent).ok()?;
        Some(ArrivingClaim {
            _claim: local.arriving.claim((dir, hash_string(name))),
        })
    }

    /// Whether a commit is settling `name` in directory inode `parent` now. One load where no
    /// settle runs.
    pub(crate) fn settling_name(&self, parent: u64, name: &str) -> bool {
        let Some(local) = self.local_files() else {
            return false;
        };
        if !local.settling.busy() {
            return false;
        }
        self.dir_node(parent)
            .is_ok_and(|(dir, _)| local.settling.holds((dir, hash_string(name))))
    }

    /// Whether a commit is settling the name at `place`, or settled it since
    /// [`Self::settle_epoch`] answered `epoch`: a new inode of what a request read before then
    /// takes no content.
    pub(crate) fn settle_touched(&self, place: (NodeID, u64), epoch: u64) -> bool {
        self.local_files().is_some_and(|local| {
            local.settling.holds(place) || local.settle_log.settled_since(place, epoch)
        })
    }

    /// Waits for a commit's settle of the name a request reached begun so far. For tests.
    pub fn settle_waits_begun(&self) -> u64 {
        self.local_files()
            .map_or(0, |local| local.settle_waits[2].load(Ordering::SeqCst))
    }

    /// Requests that waited for a commit's settle of the name they reached, and requests that
    /// started again for one (`Self::settled_at`). For tests.
    pub fn settle_waits(&self) -> (u64, u64) {
        self.local_files().map_or((0, 0), |local| {
            (
                local.settle_waits[0].load(Ordering::Relaxed),
                local.settle_waits[1].load(Ordering::Relaxed),
            )
        })
    }

    /// Builds the path of `ino` below `write/`, then `child` below it, into `out`, from the
    /// spellings the inodes hold.
    pub(crate) fn host_path(
        &self,
        mut ino: u64,
        child: Option<&[u8]>,
        out: &mut HostPath,
    ) -> Result<(), Errno> {
        if let Some(child) = child {
            out.prepend(child)?;
        }
        let mut depth = 0;
        while ino != ROOT_INO {
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(Errno::EIO);
            }
            let entry = self.inodes().get(ino).ok_or(Errno::ENOENT)?;
            if entry.orphaned {
                return Err(Errno::ENOENT);
            }
            out.prepend(&entry.name)?;
            ino = entry.parent;
        }
        Ok(())
    }

    /// [`Self::host_path`] through orphaned inodes too, which keep the name and parent they were
    /// orphaned with: the path a request built before a directory on it was removed.
    fn host_path_through_orphans(
        &self,
        mut ino: u64,
        child: Option<&[u8]>,
        out: &mut HostPath,
    ) -> Result<(), Errno> {
        if let Some(child) = child {
            out.prepend(child)?;
        }
        let mut depth = 0;
        while ino != ROOT_INO {
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(Errno::EIO);
            }
            let entry = self.inodes().get(ino).ok_or(Errno::ENOENT)?;
            out.prepend(&entry.name)?;
            ino = entry.parent;
        }
        Ok(())
    }

    /// Removes the host entry a create made as `name` in directory inode `parent`, node `dir`,
    /// once its attach failed, a directory where `is_dir`. Where the directory went meanwhile, its
    /// node detached as a resident block shows (an RMDIR between its removal of the host
    /// directory and its detach, or a rename replacing it), the create's host step may have made
    /// the host directory again, and other creates their entries in it: the entry goes at the path
    /// the inodes spell, orphaned ones included, and then the directory's host directory, while
    /// left empty, so the next mount takes no directory back (H11); the last create to go removes
    /// it. That host directory stays unless the State finds no node at the place: any node there,
    /// as a directory renamed over the gone one, any error finding it, or a create's or rename's
    /// claim on the name keeps it. The State is read before any host path is built, so no await
    /// holds a path's buffer; without `fetch` only in memory, where a block it needs that is not
    /// keeps the host directory too.
    async fn remove_failed_entry(
        &self,
        served: &Served,
        (parent, dir): (u64, NodeID),
        (name, is_dir): (&str, bool),
        fetch: bool,
    ) {
        let Some(local) = self.local_files() else {
            return;
        };
        let gone = served
            .state
            .resident_block(NodeBlock::index(dir))
            .is_some_and(|block| block.node(Node::index(dir)).is_discarded());
        if !gone {
            let _ = self.at_host_path(parent, Some(name.as_bytes()), |path| {
                local.remove_path(path, is_dir)
            });
            return;
        }
        // The place the directory stood, where no live node holds it now.
        let free = match self
            .inodes()
            .get(parent)
            .map(|entry| (entry.parent, entry.name_hash))
            .map(|(above, name_hash)| (self.dir_node(above), name_hash))
        {
            Some((Ok((above, _)), name_hash)) => {
                let held = if fetch {
                    served
                        .state
                        .find_subnode(served.repository.clone(), above, name_hash)
                        .await
                        .map(Some)
                } else {
                    served.state.find_subnode_resident(above, name_hash)
                };
                match held {
                    Err(error) if error.is_node_not_found() => Some((above, name_hash)),
                    Ok(_) | Err(_) => None,
                }
            }
            _ => None,
        };
        self.remove_orphaned_entry(local, (parent, name), is_dir, free);
    }

    /// Removes the host entry `name` in directory inode `parent`, a directory where `is_dir`, at
    /// the path the inodes spell, orphaned ones included; then, given `free`, the place the
    /// directory stood with no live node holding it, the directory's host directory, while left
    /// empty, unless a create or rename claims the place.
    fn remove_orphaned_entry(
        &self,
        local: &LocalFiles,
        (parent, name): (u64, &str),
        is_dir: bool,
        free: Option<(NodeID, u64)>,
    ) {
        let mut path = HostPath::new();
        if self
            .host_path_through_orphans(parent, Some(name.as_bytes()), &mut path)
            .is_err()
        {
            return;
        }
        let _ = local.remove_path(&path, is_dir);
        let Some(place) = free else {
            return;
        };
        let mut path = HostPath::new();
        if local.arriving.holds(place)
            || self
                .host_path_through_orphans(parent, None, &mut path)
                .is_err()
        {
            return;
        }
        let _ = local.remove_path(&path, true);
    }

    /// [`Self::host_path`], with the renames of `ino` and every directory above it as the walk
    /// read them.
    fn host_path_watched(
        &self,
        mut ino: u64,
        child: Option<&[u8]>,
        out: &mut HostPath,
    ) -> Result<RenameWatch, Errno> {
        if let Some(child) = child {
            out.prepend(child)?;
        }
        let mut watch = RenameWatch::default();
        let mut depth = 0;
        while ino != ROOT_INO {
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(Errno::EIO);
            }
            let entry = self.inodes().get(ino).ok_or(Errno::ENOENT)?;
            if entry.orphaned {
                return Err(Errno::ENOENT);
            }
            watch.add(&entry);
            out.prepend(&entry.name)?;
            ino = entry.parent;
        }
        Ok(watch)
    }

    /// The renames of directory `ino` and every directory above it.
    fn rename_watch(&self, mut ino: u64) -> Result<RenameWatch, Errno> {
        let mut watch = RenameWatch::default();
        let mut depth = 0;
        while ino != ROOT_INO {
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(Errno::EIO);
            }
            let entry = self.inodes().get(ino).ok_or(Errno::ENOENT)?;
            watch.add(&entry);
            ino = entry.parent;
        }
        Ok(watch)
    }

    /// Runs `host`, which makes a host entry at the path of directory `ino` then `child`, adding
    /// to its count the host directories it made ([`Self::with_host_dirs_made`]), and answers
    /// too with the entry where a rename of a directory above was under way or came between the
    /// path's build and the entry: the entry may stand under a name the rename moved away from
    /// ([`Self::settle_host_entry`]). A path built before a rename respelled a directory above
    /// names a host directory gone once the rename has gathered, so `ENOENT` after such a rename
    /// builds the path again, at most [`HOST_WRITE_ATTEMPTS`] times. The path lives in this call
    /// alone, and only the rare answer allocates.
    fn at_host_path_watched<T>(
        &self,
        ino: u64,
        child: Option<&[u8]>,
        mut host: impl FnMut(&HostPath, &mut usize) -> Result<T, Errno>,
    ) -> Result<(T, Option<RacedEntry>), Errno> {
        let mut attempts = 0;
        loop {
            let mut path = HostPath::new();
            let before = self.host_path_watched(ino, child, &mut path)?;
            let mut made_dirs = 0;
            let made = host(&path, &mut made_dirs);
            let raced =
                before.under_way || !self.rename_watch(ino).is_ok_and(|after| after == before);
            match made {
                Ok(made) => {
                    let raced = raced.then(|| RacedEntry {
                        path: path.as_str().to_owned(),
                        made_dirs,
                    });
                    return Ok((made, raced));
                }
                Err(Errno::ENOENT) if raced && attempts < HOST_WRITE_ATTEMPTS => attempts += 1,
                Err(errno) => return Err(errno),
            }
        }
    }

    /// Moves the host entry `raced` made at the host path of directory `parent` then `name` as
    /// a rename of a directory above found it to the path they spell once no rename above is
    /// under way, waiting at most [`RENAME_WAITS`] yields for that, and removes the directories
    /// of the old path its making created, where left empty: a directory made there otherwise,
    /// as by the user under the old name, stays. An entry a rename's gather moved already is
    /// left as it is.
    async fn settle_host_entry(
        &self,
        local: &LocalFiles,
        parent: u64,
        name: &str,
        raced: &RacedEntry,
    ) {
        let old = raced.path.as_str();
        for _ in 0..RENAME_WAITS {
            match self.rename_watch(parent) {
                Ok(watch) if !watch.under_way => break,
                Ok(_) => tokio::task::yield_now().await,
                Err(_) => return,
            }
        }
        let mut current = HostPath::new();
        if self
            .host_path(parent, Some(name.as_bytes()), &mut current)
            .is_err()
            || current.as_str() == old
        {
            return;
        }
        let Some(from) = host_path_of(old) else {
            return;
        };
        match self.with_host_dirs(parent, || local.rename_no_replace(&from, &current)) {
            Ok(()) | Err(Errno::ENOENT) => {}
            Err(errno) => {
                lore_warn!(
                    "FUSE could not move '{old}' to '{}' after a rename above it: {errno:?}",
                    current.as_str()
                );
                return;
            }
        }
        let mut dir = old;
        for _ in 0..raced.made_dirs {
            let Some((above, _)) = dir.rsplit_once('/') else {
                break;
            };
            dir = above;
            let kept = current
                .as_str()
                .strip_prefix(dir)
                .is_some_and(|rest| rest.starts_with('/'));
            if kept {
                break;
            }
            let removed =
                host_path_of(dir).is_some_and(|host| local.remove_path(&host, true).is_ok());
            if !removed {
                break;
            }
        }
    }

    /// Runs `host` on the path of `ino` below `write/`, then `child` below it. The path lives
    /// in this call alone, so no future holds its buffer.
    fn at_host_path<T>(
        &self,
        ino: u64,
        child: Option<&[u8]>,
        host: impl FnOnce(&HostPath) -> Result<T, Errno>,
    ) -> Result<T, Errno> {
        let mut path = HostPath::new();
        self.host_path(ino, child, &mut path)?;
        host(&path)
    }

    /// Inode `ino`'s host file, a local file's: cached, else opened by its path and cached, where
    /// under the inode's entry guard the same incarnation still holds the number, the file has
    /// not turned store-served or local since, and what opened is the file's own host file
    /// ([`InodeEntry::host_file`]). Anything else is closed, never served: the path is built
    /// again, at most `DESCRIPTOR_ATTEMPTS` times before `EIO`, and a file no longer local
    /// answers `ENOENT`, as its absent host file does.
    pub fn descriptor(&self, ino: u64) -> Result<Arc<File>, Errno> {
        let local = self.writable()?;
        if let Some(file) = local.file(ino) {
            return Ok(file);
        }
        for _ in 0..DESCRIPTOR_ATTEMPTS {
            if let Some(file) = self.open_descriptor(local, ino)? {
                return Ok(file);
            }
        }
        lore_warn!(
            "FUSE inode {ino}: its path reached another file's host file {DESCRIPTOR_ATTEMPTS} times"
        );
        Err(Errno::EIO)
    }

    /// One attempt of [`Self::descriptor`]: the host file at inode `ino`'s path, cached where it
    /// is the inode's; `None`, closed, where it is another file's or the inode changed since the
    /// attempt began; `ENOENT` where the inode is no longer a local file.
    fn open_descriptor(&self, local: &LocalFiles, ino: u64) -> Result<Option<Arc<File>>, Errno> {
        let began = {
            let entry = self.inodes().get(ino).ok_or(Errno::ENOENT)?;
            (entry.incarnation, entry.host_epoch)
        };
        let mut path = HostPath::new();
        self.host_path(ino, None, &mut path)?;
        self.write_step_allowed(WriteStep::PathResolved { ino });
        let file = Arc::new(local.open_existing(&path)?);
        self.write_step_allowed(WriteStep::DescriptorOpened { ino });
        let opened = file_stat(&file)?.st_ino;
        // After a FORGET since the open, the number may stand for a new inode whose host file
        // this no longer is; after a flip or a copy-up, a copy-up's file may stand at the path;
        // after a RENAME of the file or a directory above it and a CREATE, another file may.
        let mut found = self.inodes().get(ino).map_or(HostMatch::NotLocal, |entry| {
            let found = HostMatch::of(&entry, began, opened);
            if found == HostMatch::Own {
                local.insert_file(ino, file.clone());
            }
            found
        });
        if found == HostMatch::Unidentified {
            found = self.identify_host_file(local, ino, began, (&path, opened), &file)?;
        }
        Ok(match found {
            HostMatch::Own => {
                // The entry guard released, closing unused files holds up nothing on the inode's
                // shard.
                local.evict_past_budget();
                Some(file)
            }
            HostMatch::NotLocal => return Err(Errno::ENOENT),
            HostMatch::Changed | HostMatch::Other | HostMatch::Unidentified => None,
        })
    }

    /// [`Self::open_descriptor`] for a local file whose host file nothing has identified, as a
    /// LOOKUP's: `file`, opened at `path`, is the file's own where its path built again is still
    /// `path`, so no RENAME of it or of a directory above it came between the build and the open,
    /// and the file at that path is `file`, so none went away and back across it either.
    /// Recorded and cached under the entry's write guard.
    fn identify_host_file(
        &self,
        local: &LocalFiles,
        ino: u64,
        began: (u64, u64),
        (path, opened): (&HostPath, u64),
        file: &Arc<File>,
    ) -> Result<HostMatch, Errno> {
        let mut again = HostPath::new();
        self.host_path(ino, None, &mut again)?;
        if again.as_str() != path.as_str()
            || !local.stat(&again).is_ok_and(|at| at.st_ino == opened)
        {
            return Ok(HostMatch::Other);
        }
        Ok(self
            .inodes()
            .get_mut(ino)
            .map_or(HostMatch::NotLocal, |mut entry| {
                let mut found = HostMatch::of(&entry, began, opened);
                if found == HostMatch::Unidentified {
                    entry.host_file = opened;
                    found = HostMatch::Own;
                }
                if found == HostMatch::Own {
                    local.insert_file(ino, file.clone());
                }
                found
            }))
    }

    /// Attributes of local inode `ino`, of `kind`, from the host: a file's by `fstat` of its
    /// descriptor, one descriptor-cache lookup when it is open, a symlink's or directory's by
    /// `lstat` of its path. Takes no inode lock across the host call.
    pub(crate) fn local_attr(&self, ino: u64, kind: HostKind) -> Result<Attr, Errno> {
        let stat = match kind {
            HostKind::File => file_stat(&*self.descriptor(ino)?)?,
            HostKind::Dir | HostKind::Symlink => {
                let local = self.writable()?;
                self.at_host_path(ino, None, |path| local.stat(path))?
            }
        };
        Ok(host_attr(ino, &stat, self.attr_context()))
    }

    /// READ of local file `ino` from its host file.
    pub(crate) fn read_local(&self, ino: u64, offset: u64, out: &mut [u8]) -> Result<usize, Errno> {
        let file = self.descriptor(ino)?;
        let mut done = 0;
        while done < out.len() {
            match file.read_at(&mut out[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(read) => done += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(io_errno(&error)),
            }
        }
        Ok(done)
    }

    /// What `name` holds in directory `parent` under `served`: the lookup LOOKUP makes, through
    /// the listing and the log of changes since. Without `fetch`, a directory with no cached
    /// listing answers [`ReadFault::WouldFetch`]. A stale listing is never rebuilt here: the
    /// State answers the one name ([`Self::resolved_by_name`]), and the build is left to the
    /// next LOOKUP or READDIR.
    pub(crate) async fn resolve(
        &self,
        served: &Served,
        parent: u64,
        name: &str,
        fetch: bool,
    ) -> Result<Resolved, ReadFault> {
        self.resolve_name(served, parent, name, fetch, false).await
    }

    /// [`Self::resolve`] of a name a request is about to attach: where nothing holds it and the
    /// listing and the directory's log show that, the answer carries the absence
    /// ([`Resolved::absent`]), so the attach checks the log under its write locks in place of
    /// walking the directory's chain.
    pub(crate) async fn resolve_to_attach(
        &self,
        served: &Served,
        parent: u64,
        name: &str,
        fetch: bool,
    ) -> Result<Resolved, ReadFault> {
        self.resolve_name(served, parent, name, fetch, true).await
    }

    /// [`Self::resolve`]; with `to_attach`, [`Self::resolve_to_attach`].
    async fn resolve_name(
        &self,
        served: &Served,
        parent: u64,
        name: &str,
        fetch: bool,
        to_attach: bool,
    ) -> Result<Resolved, ReadFault> {
        let Some(listing) = self.lookup_listing(served, parent, fetch).await? else {
            return Err(would_fetch(fetch));
        };
        let dir = self.dir_node(parent)?.0;
        let (hit, absent) = if to_attach {
            listing.lookup_to_attach(&served.state, name)
        } else {
            (listing.lookup(name), None)
        };
        let resolved = match hit {
            Hit::Listed(index, entry) => Some(Resolved {
                found: Found::of_entry(&entry),
                spelled: Name::from_slice(listing.name(index)),
                node: Some(listing.node_ids[index]),
                absent: None,
            }),
            Hit::Miss => Some(Resolved {
                absent,
                ..Resolved::nothing(name)
            }),
            Hit::Added(node_id, block) => resolved_from_block(&block, dir, node_id, name)?,
            Hit::Stale => None,
        };
        drop(listing);
        match resolved {
            Some(resolved) => Ok(resolved),
            None => self.resolved_by_name(served, dir, name).await,
        }
    }

    /// What `name` holds in directory `dir`, found by name in the State.
    async fn resolved_by_name(
        &self,
        served: &Served,
        dir: NodeID,
        name: &str,
    ) -> Result<Resolved, ReadFault> {
        let failed = |error: StateError| {
            lore_warn!("FUSE lookup of '{name}' failed: {error}");
            ReadFault::from(Errno::EIO)
        };
        let child = match served
            .state
            .find_subnode(served.repository.clone(), dir, hash_string(name))
            .await
        {
            Ok(child) => child,
            Err(error) if error.is_node_not_found() => return Ok(Resolved::nothing(name)),
            Err(error) => return Err(failed(error)),
        };
        let block = served
            .state
            .block_with_nametable(served.repository.clone(), NodeBlock::index(child))
            .await
            .map_err(failed)?;
        Ok(resolved_from_block(&block, dir, child, name)?
            .unwrap_or_else(|| Resolved::nothing(name)))
    }

    /// CREATE, MKNOD, MKDIR and SYMLINK: `name` in `parent` as `kind`, with `mode`, a symlink
    /// pointing at `target`: the host entry under `write/`, then a dirty, zero-address
    /// local-content node attached to the parent's. Without `fetch`, a directory whose listing
    /// needs a build answers [`ReadFault::WouldFetch`] before any change.
    pub(crate) async fn create_node(
        &self,
        parent: u64,
        name: &[u8],
        kind: LocalKind,
        (mode, target): (u32, &[u8]),
        fetch: bool,
    ) -> Result<EntryReply, ReadFault> {
        let local = self.writable()?;
        let name = new_name(name)?;
        let (dir_node, parent_filter) = self.dir_node(parent)?;
        let served = self.served();
        let absent = match self.resolve_to_attach(&served, parent, name, fetch).await? {
            Resolved {
                found: Found::Nothing,
                absent,
                ..
            } => absent,
            _ => return Err(Errno::EEXIST.into()),
        };
        let name_hash = hash_string(name);
        // Every kind: a lookup finding the node before its inode waits for this.
        let _arriving = local.arriving.claim((dir_node, name_hash));
        let (file, raced) =
            self.at_host_path_watched(parent, Some(name.as_bytes()), |path, made_dirs| {
                self.write_step_allowed(WriteStep::HostPathBuilt { parent });
                let made = self.with_host_dirs_made(parent, made_dirs, || match kind {
                    LocalKind::File => local.create_file(path, mode).map(Some),
                    LocalKind::Dir => local.mkdir(path, mode).map(|()| None),
                    LocalKind::Symlink => local.symlink(target, path).map(|()| None),
                });
                if made.is_ok() {
                    self.write_step_allowed(WriteStep::HostEntryMade { parent });
                }
                made
            })?;
        if let Some(raced) = raced {
            self.settle_host_entry(local, parent, name, &raced).await;
        }
        // Unidentified where the `fstat` fails: the first descriptor opened identifies it.
        let host_file = file
            .as_ref()
            .and_then(|file| file_stat(file).ok())
            .map_or(0, |stat| stat.st_ino);
        let executable = kind == LocalKind::File && mode & 0o111 != 0;
        let (flags, ext) = match kind {
            LocalKind::Dir => (NodeFlags::NoFlags, NodeExtFlags::LocalContent),
            LocalKind::File => (NodeFlags::File, NodeExtFlags::LocalContent),
            LocalKind::Symlink => (
                NodeFlags::File,
                NodeExtFlags::LocalContent | NodeExtFlags::Symlink,
            ),
        };
        let node = Node {
            flags: (flags | NodeFlags::DirtyAdd).bits(),
            ext_flags: ext.bits(),
            mode: if executable {
                NodeFileMode::Executable.bits()
            } else {
                0
            },
            size: if kind == LocalKind::Symlink {
                target.len() as u64
            } else {
                0
            },
            ..Node::default()
        };
        let node_id = match self
            .attach_local(&served, dir_node, node, name, (fetch, absent.as_ref()))
            .await
        {
            Ok(node_id) => node_id,
            Err(fault) => {
                let entry = (name, kind == LocalKind::Dir);
                self.remove_failed_entry(&served, (parent, dir_node), entry, fetch)
                    .await;
                return Err(fault);
            }
        };
        if kind == LocalKind::Dir {
            self.cache_empty_listing(&served, node_id);
        }
        self.write_step_allowed(WriteStep::Created);
        self.invalidate_spellings(parent, name_hash, name.as_bytes());
        let inode_kind = match kind {
            LocalKind::File => InodeKind::File {
                address: Address::default(),
                size: 0,
                executable,
                local: true,
            },
            LocalKind::Symlink => InodeKind::Symlink,
            LocalKind::Dir => {
                let parent_path = self.path_of(parent)?.0;
                let (state, _excluded) = self.view_filter(
                    &served.repository,
                    parent_filter,
                    &parent_path.join(name),
                    true,
                );
                InodeKind::Dir {
                    filter_state: filter_states_to_u8(state),
                    view_line: view_line(state),
                    node: node_id,
                    local: true,
                }
            }
        };
        if let Some(stale) = self.inodes().find_child(parent, name_hash, name) {
            self.inodes().orphan(stale);
        }
        let (ino, ()) = self.inodes().insert_child(
            NewChild {
                parent,
                name,
                name_hash,
                kind: inode_kind,
                mtime: 0,
                node: node_id,
            },
            |_| (),
        );
        if let Some(file) = file {
            if let Some(mut entry) = self.inodes().get_mut(ino) {
                entry.host_file = host_file;
            }
            local.cache_file(ino, Arc::new(file));
        }
        let attr = match self.attr_of(ino) {
            Ok(attr) => attr,
            Err(errno) => {
                self.drop_lookups(ino, 1);
                return Err(errno.into());
            }
        };
        self.touch_dir(parent);
        let timeouts = self.timeouts_for(&served, parent, name_hash, name.as_bytes());
        Ok(EntryReply {
            ino,
            generation: 0,
            attr,
            timeouts,
        })
    }

    /// Attaches `node`, whole with its dirty and extension flags, named `name`, below directory
    /// node `dir` under the served chain rule, so the directory and its ancestors are dirty
    /// before anything below it holds local content; `EEXIST` where a live sibling holds the
    /// name, checked against the directory's log where `absent` is given. Without `fetch`, the
    /// attach reads nothing from the store and answers [`ReadFault::WouldFetch`], attaching
    /// nothing, where what it needs is not in memory.
    async fn attach_local(
        &self,
        served: &Served,
        dir: NodeID,
        node: Node,
        name: &str,
        (fetch, absent): (bool, Option<&Absent>),
    ) -> Result<NodeID, ReadFault> {
        let failed = |error: StateError| state_errno("create", name, &error);
        self.write_step_allowed(WriteStep::Attaching { dir });
        debug_assert!(
            served.state.attach_slot_resident(),
            "a served State takes new nodes from fresh blocks"
        );
        let check = match (fetch, absent) {
            (true, Some(absent)) => AttachCheck::Absent(absent),
            (true, None) => AttachCheck::Walk,
            (false, absent) => AttachCheck::Resident(absent),
        };
        let attached = served
            .state
            .served()
            .node_attach(served.repository.clone(), dir, node, name, check)
            .await;
        match attached.map_err(failed)? {
            Some(AttachOutcome::Attached(node_id)) => Ok(node_id),
            Some(AttachOutcome::Exists(_)) => Err(Errno::EEXIST.into()),
            None => Err(ReadFault::WouldFetch),
        }
    }

    /// CREATE: a regular file, as [`Self::create_node`], opened with no handle, since every
    /// descriptor of the file shares the inode's host file.
    pub(crate) async fn create_file_node(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        fetch: bool,
    ) -> Result<CreateReply, ReadFault> {
        let entry = self
            .create_node(parent, name, LocalKind::File, (mode, &[]), fetch)
            .await?;
        Ok(CreateReply {
            entry,
            open: OpenReply { fh: 0, flags: 0 },
        })
    }

    /// MKNOD, of regular files only, as [`Self::create_node`].
    pub(crate) async fn mknod_node(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        fetch: bool,
    ) -> Result<EntryReply, ReadFault> {
        self.writable()?;
        let format = mode & libc::S_IFMT;
        if format != 0 && format != libc::S_IFREG {
            return Err(Errno::EPERM.into());
        }
        self.create_node(parent, name, LocalKind::File, (mode, &[]), fetch)
            .await
    }

    /// WRITE of `data` at `offset` into inode `ino`, a tracked file copied up first. Without
    /// `fetch`, a tracked file answers [`ReadFault::WouldFetch`] before any change.
    pub(crate) async fn write_node(
        &self,
        ino: u64,
        offset: u64,
        data: &[u8],
        fetch: bool,
    ) -> Result<usize, ReadFault> {
        self.writable()?;
        self.write_local_file(ino, None, fetch, |file| {
            file.write_all_at(data, offset)
                .map_err(|error| io_errno(&error))
        })
        .await?;
        Ok(data.len())
    }

    /// Runs `host` on local file `ino`'s descriptor inside the write-generation bracket
    /// ([`Self::host_write`]), a tracked file copied up first, sized `truncate_to` where given.
    /// Where the bracket finds the inode no longer local, or holding another descriptor, as
    /// after a commit's flip, it goes back through the copy-up, at most [`HOST_WRITE_ATTEMPTS`]
    /// times, then answers `EAGAIN`.
    async fn write_local_file<T>(
        &self,
        ino: u64,
        truncate_to: Option<u64>,
        fetch: bool,
        host: impl Fn(&File) -> Result<T, Errno>,
    ) -> Result<T, ReadFault> {
        for _ in 0..HOST_WRITE_ATTEMPTS {
            let Some(file) = self.local_file_for_write(ino, truncate_to, fetch).await? else {
                continue;
            };
            if let Some(done) = self.host_write(ino, &file, || host(&file))? {
                return Ok(done);
            }
        }
        Err(Errno::EAGAIN.into())
    }

    /// Local file `ino`'s descriptor for a host write, a tracked file copied up first, sized
    /// `truncate_to` where given; `None` where a commit's flip turned the file store-served
    /// before its descriptor was opened.
    async fn local_file_for_write(
        &self,
        ino: u64,
        truncate_to: Option<u64>,
        fetch: bool,
    ) -> Result<Option<Arc<File>>, ReadFault> {
        let epoch = self.settle_epoch();
        self.ensure_local_file(ino, truncate_to, fetch).await?;
        let file = match self.descriptor(ino) {
            Ok(file) => file,
            Err(_) if self.is_tracked_file(ino) => return Ok(None),
            Err(Errno::ENOENT) if self.settled_at_inode(ino, epoch).await => return Ok(None),
            Err(errno) => return Err(errno.into()),
        };
        if !self.write_step_allowed(WriteStep::BeforeHostWrite) {
            return Err(Errno::EIO.into());
        }
        Ok(Some(file))
    }

    /// Runs `host`, the host operation of a WRITE, SETATTR or FALLOCATE of local file `ino`
    /// through `file`, with the inode's write generation moved by one before it and one after
    /// it, both under the inode's read guard ([`InodeEntry::write_generation`]); `None`, running
    /// nothing, where under that guard the inode is no longer local or `file` is no longer its
    /// cached descriptor. `host` must not read the inode table.
    fn host_write<T>(
        &self,
        ino: u64,
        file: &Arc<File>,
        host: impl FnOnce() -> Result<T, Errno>,
    ) -> Result<Option<T>, Errno> {
        let local = self.writable()?;
        let entry = self.inodes().get(ino).ok_or(Errno::ENOENT)?;
        let current = matches!(entry.kind, InodeKind::File { local: true, .. })
            && local
                .file(ino)
                .is_some_and(|cached| Arc::ptr_eq(&cached, file));
        if !current {
            return Ok(None);
        }
        let generation = entry.write_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let done = if self.write_step_allowed(WriteStep::HostWrite { generation }) {
            host()
        } else {
            Err(Errno::EIO)
        };
        entry.write_generation.fetch_add(1, Ordering::Release);
        done.map(Some)
    }

    /// Makes `ino` a local file, copying a tracked file up first, from the content staged for
    /// it where that is still its content; a copy sized `truncate_to` holds only what that size
    /// keeps. Without `fetch`, a copy-up that keeps any of the tracked content answers
    /// [`ReadFault::WouldFetch`]; one that keeps none, as a truncate to zero, reads nothing and
    /// runs here.
    async fn ensure_local_file(
        &self,
        ino: u64,
        truncate_to: Option<u64>,
        fetch: bool,
    ) -> Result<(), ReadFault> {
        let local = self.writable()?;
        let kept = match self.inodes().get(ino).ok_or(Errno::ENOENT)?.kind {
            InodeKind::File { local: true, .. } => None,
            InodeKind::File { size, .. } => Some(truncate_to.map_or(size, |len| len.min(size))),
            InodeKind::Dir { .. } => return Err(Errno::EISDIR.into()),
            InodeKind::Symlink => return Err(Errno::EINVAL.into()),
        };
        let Some(kept) = kept else {
            drop(local.take_staged(ino));
            return Ok(());
        };
        if !fetch && kept > 0 {
            return Err(ReadFault::WouldFetch);
        }
        let staged = local.take_staged(ino);
        // Boxed: the decode is a large future, and only a copy-up awaits it.
        Box::pin(self.copy_up(ino, truncate_to, staged, fetch)).await
    }

    /// Materializes tracked file `ino` at its path under `write/`, makes its node a dirty,
    /// zero-address local-content node, and serves it from the host file from then on, under
    /// the inode the kernel holds. `staged` is taken where it holds the file's content. Without
    /// `fetch`, a copy-up of the inode already under way, or a listing to build, answers
    /// [`ReadFault::WouldFetch`].
    async fn copy_up(
        &self,
        ino: u64,
        truncate_to: Option<u64>,
        staged: Option<StagedCopy>,
        fetch: bool,
    ) -> Result<(), ReadFault> {
        let local = self.writable()?;
        let gate = local.copy_up_gate(ino);
        let held = if fetch {
            Some(gate.lock().await)
        } else {
            gate.try_lock().ok()
        };
        let Some(held) = held else {
            return Err(ReadFault::WouldFetch);
        };
        let copied = self.copy_up_held(ino, truncate_to, staged, fetch).await;
        drop(held);
        local.release_copy_up_gate(ino);
        copied
    }

    async fn copy_up_held(
        &self,
        ino: u64,
        truncate_to: Option<u64>,
        staged: Option<StagedCopy>,
        fetch: bool,
    ) -> Result<(), ReadFault> {
        let local = self.writable()?;
        let (address, size, executable, parent, name) = {
            let entry = self.inodes().get(ino).ok_or(Errno::ENOENT)?;
            match entry.kind {
                InodeKind::File { local: true, .. } => return Ok(()),
                // Its path is gone, and a copy-up has nowhere to go.
                InodeKind::File { .. } if entry.orphaned => return Err(Errno::EROFS.into()),
                InodeKind::File {
                    address,
                    size,
                    executable,
                    ..
                } => (address, size, executable, entry.parent, entry.name.clone()),
                InodeKind::Dir { .. } | InodeKind::Symlink => return Err(Errno::EISDIR.into()),
            }
        };
        let name = std::str::from_utf8(&name).map_err(|_invalid| Errno::EIO)?;
        let served = self.served();
        let resolved = self.resolve(&served, parent, name, fetch).await?;
        let node_id = match resolved.found {
            Found::File { local: false } => resolved.node.ok_or(Errno::ENOENT)?,
            Found::File { local: true } => {
                // No descriptor is open here: the first one opened identifies the host file.
                self.flip_to_local(ino, size, resolved.node, 0);
                return Ok(());
            }
            _ => return Err(Errno::ENOENT.into()),
        };
        let kept = truncate_to.map_or(size, |len| len.min(size));
        let staged = staged.filter(|copy| copy.address == address && copy.kept >= kept);
        let (staged, content) = if let Some(copy) = staged {
            let content = copy.kept;
            (copy, content)
        } else {
            let copy = StagedCopy::new(address, kept, local.staging_path());
            self.fetch_content(&served, ino, (address, size, kept), copy.path())
                .await?;
            (copy, kept)
        };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(staged.path())
            .map_err(|error| io_errno(&error))?;
        let len = truncate_to.unwrap_or(size);
        if len != content {
            file.set_len(len).map_err(|error| io_errno(&error))?;
        }
        set_file_mode(&file, if executable { 0o755 } else { 0o644 })?;
        let ((), raced) =
            self.at_host_path_watched(parent, Some(name.as_bytes()), |path, made_dirs| {
                self.write_step_allowed(WriteStep::CopyUpPathBuilt { parent });
                self.with_host_dirs_made(parent, made_dirs, || {
                    local.rename_staged(staged.path(), path)
                })
            })?;
        if let Some(raced) = raced {
            self.settle_host_entry(local, parent, name, &raced).await;
        }
        staged.moved();
        let mode = if executable {
            NodeFileMode::Executable.bits()
        } else {
            0
        };
        let (dir, _) = self.dir_node(parent)?;
        let marked = self
            .mark_local_file(&served, dir, node_id, (mode, len))
            .await;
        if let Err(errno) = marked {
            let _ = self.at_host_path(parent, Some(name.as_bytes()), |host| {
                lore_warn!("FUSE copy-up of '{}' failed", host.as_str());
                local.remove_path(host, false)
            });
            return Err(errno.into());
        }
        // The staged file, renamed into place, is the host file.
        let host_file = file_stat(&file).map_or(0, |stat| stat.st_ino);
        local.cache_file(ino, Arc::new(file));
        self.flip_to_local(ino, len, Some(node_id), host_file);
        Ok(())
    }

    /// Makes file node `node_id` of directory node `dir` a dirty, zero-address local-content
    /// node of `mode` and `size` in one block write, under the served chain rule: where the
    /// directory the node stands below refuses it, the directory is marked dirty with its
    /// ancestors and the write tried again.
    async fn mark_local_file(
        &self,
        served: &Served,
        dir: NodeID,
        node_id: NodeID,
        (mode, size): (u16, u64),
    ) -> Result<(), Errno> {
        let failed = |error: StateError| state_errno("copy-up", &node_id.to_string(), &error);
        if !self.write_step_allowed(WriteStep::CopyUp) {
            return Err(Errno::EIO);
        }
        served
            .state
            .served()
            .node_copy_up(
                &served.repository,
                node_id,
                dir,
                (mode, size),
                (NodeFlags::DirtyModify, NodeExtFlags::LocalContent),
            )
            .await
            .map_err(failed)
    }

    /// Serves file inode `ino`, `size` bytes, from its host file from now on, standing for
    /// `node` where the copy-up found it; `host_file` identifies the host file, 0 where nothing
    /// has ([`InodeEntry::host_file`]).
    fn flip_to_local(&self, ino: u64, len: u64, node: Option<NodeID>, host_file: u64) {
        if let Some(mut entry) = self.inodes().get_mut(ino)
            && matches!(entry.kind, InodeKind::File { .. })
        {
            if let Some(node) = node {
                entry.node = node;
            }
            if let InodeKind::File {
                address,
                size,
                local,
                ..
            } = &mut entry.kind
            {
                *address = Address::default();
                *size = len;
                *local = true;
            }
            entry.host_epoch += 1;
            entry.host_file = host_file;
        }
        self.fragment_cache().forget(ino);
    }

    /// Reads the first `kept` bytes of the `size`-byte content at `address`, inode `ino`'s, into
    /// `path`, fetching what the local store lacks.
    async fn fetch_content(
        &self,
        served: &Served,
        ino: u64,
        (address, size, kept): (Address, u64, u64),
        path: &Path,
    ) -> Result<(), Errno> {
        read_content(
            self.execution_context(),
            served.repository.clone(),
            ino,
            (address, size, kept),
            path,
        )
        .await
    }

    /// Fetches tracked file `ino`'s content, its first `len` bytes where given, into the staging
    /// directory ahead of its copy-up, which takes it while it is still the file's content and
    /// covers what the copy-up keeps, after resolving the file as the copy-up does. A file no
    /// longer tracked, or whose path is gone, stages nothing. The staging runs in a task that
    /// owns the staged file: abandoned, as by INTERRUPT, the task runs to its end and removes the
    /// file only after every host write into it has finished.
    pub(crate) async fn stage_copy(&self, ino: u64, len: Option<u64>) -> Result<(), Errno> {
        let local = self.writable()?;
        let (address, size, parent, name) = {
            let entry = self.inodes().get(ino).ok_or(Errno::ENOENT)?;
            match entry.kind {
                InodeKind::File {
                    local: false,
                    address,
                    size,
                    ..
                } if !entry.orphaned => (address, size, entry.parent, entry.name.clone()),
                _ => return Ok(()),
            }
        };
        let name = std::str::from_utf8(&name).map_err(|_invalid| Errno::EIO)?;
        let served = self.served();
        self.resolve(&served, parent, name, true).await?;
        let kept = len.map_or(size, |len| len.min(size));
        let copy = StagedCopy::new(address, kept, local.staging_path());
        let context = self.execution_context();
        let repository = served.repository.clone();
        let hook = self.staging_hook();
        let staging = lore_base::lore_spawn!(async move {
            read_content(context, repository, ino, (address, size, kept), copy.path()).await?;
            if let Some(hook) = hook {
                hook(copy.path().to_path_buf()).await;
            }
            Ok::<_, Errno>(copy)
        });
        let copy = staging.await.map_err(|_panicked| Errno::EIO)??;
        local.put_staged(ino, copy);
        Ok(())
    }

    /// SETATTR of inode `ino`: size, mode and times; owner and group only as they are. A
    /// tracked file is copied up first, and a tracked directory takes a new time only.
    pub(crate) async fn setattr_node(
        &self,
        ino: u64,
        attr: &SetattrIn,
        fetch: bool,
    ) -> Result<AttrReply, ReadFault> {
        let local = self.writable()?;
        let valid = attr.valid;
        let context = self.attr_context();
        if (valid & setattr_valid::UID != 0 && attr.uid != context.uid)
            || (valid & setattr_valid::GID != 0 && attr.gid != context.gid)
        {
            return Err(Errno::EPERM.into());
        }
        let changes = setattr_valid::SIZE
            | setattr_valid::MODE
            | setattr_valid::ATIME
            | setattr_valid::MTIME
            | setattr_valid::ATIME_NOW
            | setattr_valid::MTIME_NOW;
        let target = match self.inodes().get(ino).ok_or(Errno::ENOENT)?.kind {
            InodeKind::File { local, .. } => SetattrTarget::File { local },
            InodeKind::Dir { local, .. } => SetattrTarget::Dir { local },
            InodeKind::Symlink => SetattrTarget::Symlink,
        };
        let is_file = match target {
            SetattrTarget::Dir { local: false } => return Ok(self.setattr_tracked_dir(ino, attr)?),
            _ if valid & changes == 0 => return Ok(self.attr_reply(ino)?),
            SetattrTarget::File { .. } => true,
            SetattrTarget::Dir { .. } | SetattrTarget::Symlink => false,
        };
        let times = setattr_times(attr);
        let set_times = valid
            & (setattr_valid::ATIME
                | setattr_valid::MTIME
                | setattr_valid::ATIME_NOW
                | setattr_valid::MTIME_NOW)
            != 0;
        if is_file {
            let truncate_to = (valid & setattr_valid::SIZE != 0).then_some(attr.size);
            if valid & setattr_valid::MODE != 0 {
                self.write_local_file(ino, truncate_to, fetch, |file| {
                    set_file_mode(file, attr.mode)
                })
                .await?;
                if attr.mode & libc::S_IWUSR != 0 {
                    local.drop_read_only(ino);
                }
            }
            if valid & setattr_valid::SIZE != 0 || set_times {
                self.write_local_file(ino, truncate_to, fetch, |file| {
                    if valid & setattr_valid::SIZE != 0 {
                        file.set_len(attr.size).map_err(|error| io_errno(&error))?;
                    }
                    if set_times {
                        set_file_times(file, &times)?;
                    }
                    Ok(())
                })
                .await?;
            }
        } else {
            if valid & setattr_valid::SIZE != 0 {
                return Err(Errno::EISDIR.into());
            }
            let mut path = HostPath::new();
            self.host_path(ino, None, &mut path)?;
            if valid & setattr_valid::MODE != 0 && target != SetattrTarget::Symlink {
                local.set_mode(&path, attr.mode)?;
            }
            if set_times {
                local.set_times(&path, &times)?;
            }
        }
        Ok(self.attr_reply(ino)?)
    }

    /// SETATTR of tracked directory `ino`, which records a modification time and keeps its mode.
    fn setattr_tracked_dir(&self, ino: u64, attr: &SetattrIn) -> Result<AttrReply, Errno> {
        let valid = attr.valid;
        if valid & setattr_valid::SIZE != 0 {
            return Err(Errno::EISDIR);
        }
        if valid & setattr_valid::MODE != 0
            && attr.mode & 0o7777 != self.attr_of(ino)?.mode & 0o7777
        {
            return Err(Errno::EPERM);
        }
        let mtime = if valid & setattr_valid::MTIME_NOW != 0 {
            Some(crate::util::time::timestamp())
        } else if valid & setattr_valid::MTIME != 0 {
            Some(
                attr.mtime
                    .saturating_mul(1000)
                    .saturating_add(u64::from(attr.mtimensec) / 1_000_000),
            )
        } else {
            None
        };
        if let Some(mtime) = mtime
            && let Some(mut entry) = self.inodes().get_mut(ino)
        {
            entry.mtime = mtime;
        }
        self.attr_reply(ino)
    }

    fn attr_reply(&self, ino: u64) -> Result<AttrReply, Errno> {
        Ok(AttrReply {
            attr: self.attr_of(ino)?,
            timeout: DEFAULT_TIMEOUTS.attr,
        })
    }

    /// UNLINK, or RMDIR when `dir`: removes the host entry, if any, and detaches the node; a
    /// name the revision may hold is appended to the deletion list. The directory's listing
    /// stays cached: the detach counts toward its prefix bound, and a lookup skips the entry.
    /// Without `fetch`, a listing that needs a build answers [`ReadFault::WouldFetch`] before
    /// any change. One that found the host file absent while a commit's flip held it aside
    /// starts again ([`Self::settled_at`]), at most [`HOST_WRITE_ATTEMPTS`] times.
    pub(crate) async fn remove_node(
        &self,
        parent: u64,
        name: &[u8],
        dir: bool,
        fetch: bool,
    ) -> Result<(), ReadFault> {
        let mut removed = Err(Errno::ENOENT.into());
        for _ in 0..HOST_WRITE_ATTEMPTS {
            let epoch = self.settle_epoch();
            removed = self.remove_resolving(parent, name, dir, fetch).await;
            if !matches!(removed, Err(ReadFault::Errno(Errno::ENOENT)))
                || !self.settled_at_name(parent, name, epoch).await
            {
                break;
            }
        }
        removed
    }

    /// [`Self::remove_node`] but for starting again where a commit's flip held the host file
    /// aside.
    async fn remove_resolving(
        &self,
        parent: u64,
        name: &[u8],
        dir: bool,
        fetch: bool,
    ) -> Result<(), ReadFault> {
        self.writable()?;
        let name = std::str::from_utf8(name).map_err(|_invalid| Errno::ENOENT)?;
        let served = self.served();
        let resolved = self.resolve(&served, parent, name, fetch).await?;
        match (resolved.found, dir) {
            (Found::Nothing, _) => return Err(Errno::ENOENT.into()),
            (found, true) if !found.is_dir() => return Err(Errno::ENOTDIR.into()),
            (found, false) if found.is_dir() => return Err(Errno::EISDIR.into()),
            _ => {}
        }
        let node_id = resolved.node.ok_or(Errno::ENOENT)?;
        let (ino, counted) = self.held_child(&served, parent, name, fetch).await?;
        let removed = self
            .remove_held(&served, (parent, ino), node_id, &resolved, fetch)
            .await;
        if counted {
            self.drop_lookups(ino, 1);
        }
        removed
    }

    async fn remove_held(
        &self,
        served: &Served,
        (parent, ino): (u64, u64),
        node_id: NodeID,
        resolved: &Resolved,
        fetch: bool,
    ) -> Result<(), ReadFault> {
        let local = self.writable()?;
        let spelled = std::str::from_utf8(&resolved.spelled).map_err(|_invalid| Errno::EIO)?;
        let dir = resolved.found.is_dir();
        let mut checks: u8 = 0;
        loop {
            // A directory goes only while its chain holds what its emptiness check walked, so an
            // entry a create or a swap attached since keeps it (H11).
            let unchanged = if dir {
                let Some(unchanged) = self.dir_is_empty(served, ino, fetch, false).await? else {
                    return Err(Errno::ENOTEMPTY.into());
                };
                Some(unchanged)
            } else {
                None
            };
            // The check reads the inode's directory: one the name no longer resolves to would
            // refuse every detach.
            if unchanged
                .as_ref()
                .is_some_and(|unchanged| unchanged.directory() != node_id)
            {
                return Err(Errno::EAGAIN.into());
            }
            // A chain that moved during the check is checked again before the host is touched.
            if unchanged
                .as_ref()
                .is_none_or(|unchanged| unchanged.changes_since() == 0)
            {
                let path = self.at_host_path(parent, Some(spelled.as_bytes()), |path| {
                    Ok(path.as_str().to_owned())
                })?;
                let aside = self.set_aside(local, parent, spelled, resolved.found, ino)?;
                match self
                    .detach_node(served, node_id, &path, unchanged.as_ref())
                    .await
                {
                    Ok(true) => {
                        finish_aside(aside);
                        self.inodes().orphan(ino);
                        self.invalidate_spellings(parent, hash_string(spelled), spelled.as_bytes());
                        self.touch_dir(parent);
                        return Ok(());
                    }
                    Ok(false) => self.restore_aside(local, parent, spelled, aside),
                    Err(errno) => {
                        self.restore_aside(local, parent, spelled, aside);
                        return Err(errno.into());
                    }
                }
            }
            checks += 1;
            if u32::from(checks) > CHAIN_WALK_RESTARTS {
                return Err(Errno::EAGAIN.into());
            }
        }
    }

    /// Takes the host entry of `found`, spelled `name` in directory `parent`, off its path: a
    /// local file or symlink moves to the staging directory, the file's descriptor pinned for
    /// inode `ino`, and a directory's host directory goes. A file is local where its inode is
    /// when it moves, as a copy-up that finished since leaves it and a commit's flip does not.
    fn set_aside(
        &self,
        local: &LocalFiles,
        parent: u64,
        name: &str,
        found: Found,
        ino: u64,
    ) -> Result<Aside, Errno> {
        let found = match found {
            Found::File { .. } => Found::File {
                local: self.is_local_file(ino),
            },
            found => found,
        };
        self.at_host_path(parent, Some(name.as_bytes()), |path| match found {
            Found::File { local: true } => {
                if let Err(errno) = self.pin_local_file(ino) {
                    return if self.is_local_file(ino) {
                        Err(errno)
                    } else {
                        Ok(Aside::Nothing)
                    };
                }
                match self.while_local_file(ino, || local.move_to_staging(path)) {
                    Ok(staged) => Ok(Aside::Moved {
                        staged,
                        pinned: Some(ino),
                    }),
                    Err(FLIPPED) => {
                        local.unpin(ino);
                        Ok(Aside::Nothing)
                    }
                    Err(errno) => {
                        local.unpin(ino);
                        Err(errno)
                    }
                }
            }
            Found::Symlink => local.move_to_staging(path).map(|staged| Aside::Moved {
                staged,
                pinned: None,
            }),
            Found::Dir { .. } => {
                let stat = match local.stat(path) {
                    Ok(stat) => stat,
                    Err(Errno::ENOENT) => return Ok(Aside::Nothing),
                    Err(errno) => return Err(errno),
                };
                local.remove_path(path, true)?;
                Ok(Aside::RemovedDir {
                    mode: stat.st_mode & 0o7777,
                    times: [
                        libc::timespec {
                            tv_sec: stat.st_atime,
                            tv_nsec: stat.st_atime_nsec,
                        },
                        libc::timespec {
                            tv_sec: stat.st_mtime,
                            tv_nsec: stat.st_mtime_nsec,
                        },
                    ],
                })
            }
            Found::File { local: false } | Found::Nothing => Ok(Aside::Nothing),
        })
    }

    /// Puts what [`Self::set_aside`] took back at `name` in directory `parent`.
    fn restore_aside(&self, local: &LocalFiles, parent: u64, name: &str, aside: Aside) {
        let restored = self.at_host_path(parent, Some(name.as_bytes()), |path| match &aside {
            Aside::Moved { staged, pinned } => {
                local.rename_staged(staged, path)?;
                if let Some(ino) = pinned {
                    local.unpin(*ino);
                }
                Ok(())
            }
            Aside::RemovedDir { mode, times } => match local.mkdir(path, *mode) {
                Ok(()) => local.set_times(path, times),
                // Made again meanwhile, as by a CREATE in the directory, which an RMDIR then
                // kept: it takes the directory's mode back, its times being the CREATE's.
                Err(Errno::EEXIST) => local.set_mode(path, *mode),
                Err(errno) => Err(errno),
            },
            Aside::Nothing => Ok(()),
        });
        if let Err(errno) = restored {
            lore_warn!("FUSE could not restore the host entry of '{name}': {errno:?}");
        }
    }

    /// Detaches `node_id`, at `path`, and appends the path to the deletion list unless the
    /// node was added since the revision and no commit was reading or owed its settle
    /// ([`Self::commit_reading`]), neither before the detach nor after it: a settle completing in
    /// between cannot hide the removal.
    /// A path the revision does not hold is dropped from the list by the next commit and skipped
    /// at mount. With `unchanged`, a directory's chain as its emptiness check read it, the detach
    /// goes ahead only while the chain holds it, and answers false, detaching and recording
    /// nothing, where it does not.
    async fn detach_node(
        &self,
        served: &Served,
        node_id: NodeID,
        path: &str,
        unchanged: Option<&IfUnchanged>,
    ) -> Result<bool, Errno> {
        let failed = |error: StateError| {
            lore_warn!("FUSE removal of '{path}' failed: {error}");
            Errno::EIO
        };
        let sampled = self.commit_reading();
        self.write_step_allowed(WriteStep::Detaching { node: node_id });
        let when = unchanged.map_or(Detach::Always, Detach::IfUnchanged);
        if !served
            .state
            .served()
            .node_detach(served.repository.clone(), node_id, when)
            .await
            .map_err(failed)?
        {
            return Ok(false);
        }
        // The detached record keeps the flags it was detached with, which a commit storing the
        // node changes under the same block lock.
        let node = served
            .state
            .node(served.repository.clone(), node_id)
            .await
            .map_err(failed)?;
        self.write_step_allowed(WriteStep::Detached { node: node_id });
        if !node.is_dirty_add() || sampled || self.commit_reading() {
            self.record_deletion(path);
        }
        Ok(true)
    }

    /// Appends `path`, detached, to the deletion list. A deletion that is not recorded is lost
    /// at the next mount, as the last ones are on a crash, and the request still succeeds.
    /// Answers false where the append failed.
    fn record_deletion(&self, path: &str) -> bool {
        let Some(deletions) = self.deletions() else {
            return true;
        };
        let appended = RelativePath::new_from_initial_path(path)
            .map_err(|error| error.to_string())
            .and_then(|relative| {
                deletions
                    .append(&relative)
                    .map_err(|error| error.to_string())
            });
        if let Err(error) = appended {
            lore_warn!("FUSE deletion of '{path}' was not recorded: {error}");
            return false;
        }
        true
    }

    /// RENAME and RENAME2: `RENAME_NOREPLACE` is honoured, other flags are `EINVAL`. A tracked
    /// file is copied up first; a directory the revision holds is `EXDEV`, which tools answer
    /// by copying. Without `fetch`, a listing that needs a build or a tracked file to copy up
    /// answers [`ReadFault::WouldFetch`] before any change. A source a commit's flip turned
    /// store-served after it was resolved local, or whose host file it held aside, is resolved
    /// again, at most [`HOST_WRITE_ATTEMPTS`] times.
    pub(crate) async fn rename_node(
        &self,
        (parent, name): (u64, &[u8]),
        (newparent, newname): (u64, &[u8]),
        flags: u32,
        fetch: bool,
    ) -> Result<(), ReadFault> {
        let local = self.writable()?;
        let source = (local.staged_copies() > 0)
            .then(|| std::str::from_utf8(name).ok())
            .flatten()
            .and_then(|name| self.inodes().find_child(parent, hash_string(name), name));
        let mut renamed = Err(FLIPPED.into());
        for _ in 0..HOST_WRITE_ATTEMPTS {
            let epoch = self.settle_epoch();
            renamed = self
                .rename_resolving((parent, name), (newparent, newname), flags, fetch)
                .await;
            let again = match renamed {
                Err(ReadFault::Errno(FLIPPED)) => true,
                Err(ReadFault::Errno(Errno::ENOENT)) => {
                    let source = self.settled_at_name(parent, name, epoch).await;
                    let target = self.settled_at_name(newparent, newname, epoch).await;
                    source || target
                }
                _ => false,
            };
            if !again {
                break;
            }
        }
        if let Some(source) = source
            && !matches!(renamed, Err(ReadFault::WouldFetch))
        {
            drop(local.take_staged(source));
        }
        renamed
    }

    /// [`Self::rename_node`] but for dropping the source's staged copy.
    async fn rename_resolving(
        &self,
        (parent, name): (u64, &[u8]),
        (newparent, newname): (u64, &[u8]),
        flags: u32,
        fetch: bool,
    ) -> Result<(), ReadFault> {
        self.writable()?;
        if flags & !libc::RENAME_NOREPLACE != 0 {
            return Err(Errno::EINVAL.into());
        }
        let name = std::str::from_utf8(name).map_err(|_invalid| Errno::ENOENT)?;
        let newname = new_name(newname)?;
        let served = self.served();
        let from = Side {
            dir: parent,
            node: self.dir_node(parent)?.0,
            name,
        };
        let to = Side {
            dir: newparent,
            node: self.dir_node(newparent)?.0,
            name: newname,
        };
        let source = self.resolve(&served, from.dir, name, fetch).await?;
        let same_name = from.dir == to.dir && fold_eq(name, newname);
        match source.found {
            Found::Nothing => return Err(Errno::ENOENT.into()),
            // A change of case alone respells the directory in place, whatever it holds.
            Found::Dir { .. } if same_name => {}
            Found::Dir { local: false } => return Err(Errno::EXDEV.into()),
            Found::Dir { local: true } => {
                let node = source.node.ok_or(Errno::ENOENT)?;
                if !self.subtree_is_local(&served, node).await? {
                    return Err(Errno::EXDEV.into());
                }
            }
            Found::File { .. } | Found::Symlink => {}
        }
        let (ino, counted) = self.held_child(&served, from.dir, name, fetch).await?;
        let renamed = self
            .rename_held(&served, (from, to), ino, &source, (flags, fetch))
            .await;
        if counted {
            self.drop_lookups(ino, 1);
        }
        renamed
    }

    async fn rename_held(
        &self,
        served: &Served,
        (from, to): (Side<'_>, Side<'_>),
        ino: u64,
        source: &Resolved,
        (flags, fetch): (u32, bool),
    ) -> Result<(), ReadFault> {
        let same_name = from.dir == to.dir && fold_eq(from.name, to.name);
        let target = if same_name {
            Resolved::nothing(to.name)
        } else {
            self.resolve_to_attach(served, to.dir, to.name, fetch)
                .await?
        };
        let mut replaced = None;
        let mut counted = false;
        if target.found != Found::Nothing {
            if flags & libc::RENAME_NOREPLACE != 0 {
                return Err(Errno::EEXIST.into());
            }
            match (source.found.is_dir(), target.found.is_dir()) {
                (true, false) => return Err(Errno::ENOTDIR.into()),
                (false, true) => return Err(Errno::EISDIR.into()),
                _ => {}
            }
            let (target_ino, target_counted) =
                self.held_child(served, to.dir, to.name, fetch).await?;
            replaced = Some(target_ino);
            counted = target_counted;
        }
        // The replaced inode takes no copy-up until it is orphaned, so none lands on the
        // path this rename fills.
        let local = self.writable()?;
        let gate = replaced.map(|ino| (ino, local.copy_up_gate(ino)));
        let held = match &gate {
            Some((ino, gate)) => {
                self.write_step_allowed(WriteStep::TargetGate { ino: *ino });
                Some(gate.lock().await)
            }
            None => None,
        };
        let renamed = self
            .rename_onto(served, (from, to), ino, (source, &target), replaced, fetch)
            .await;
        drop(held);
        if let Some((replaced, _)) = gate {
            local.release_copy_up_gate(replaced);
        }
        if counted && let Some(replaced) = replaced {
            self.drop_lookups(replaced, 1);
        }
        renamed
    }

    /// The rename once the target is resolved: `replaced`, the inode at the target, goes.
    async fn rename_onto(
        &self,
        served: &Served,
        (from, to): (Side<'_>, Side<'_>),
        ino: u64,
        (source, target): (&Resolved, &Resolved),
        replaced: Option<u64>,
        fetch: bool,
    ) -> Result<(), ReadFault> {
        let local = self.writable()?;
        let same_name = from.dir == to.dir && fold_eq(from.name, to.name);
        let node_id = source.node.ok_or(Errno::ENOENT)?;
        if let Some(target_ino) = replaced
            && target.found.is_dir()
            && self
                .dir_is_empty(served, target_ino, fetch, true)
                .await?
                .is_none()
        {
            return Err(Errno::ENOTEMPTY.into());
        }
        if source.found == (Found::File { local: false }) {
            self.ensure_local_file(ino, None, fetch).await?;
        }
        let spelled = std::str::from_utf8(&source.spelled).map_err(|_invalid| Errno::EIO)?;
        let target_spelled = std::str::from_utf8(&target.spelled).map_err(|_invalid| Errno::EIO)?;
        let aside = match replaced {
            Some(target_ino) => {
                self.set_aside(local, to.dir, target_spelled, target.found, target_ino)?
            }
            None => Aside::Nothing,
        };
        // With no other name resolved as the target, the host holds nothing at it either, so a
        // host entry the State does not know answers `EEXIST` rather than being replaced.
        let no_replace = replaced.is_none() && !same_name;
        let file = matches!(source.found, Found::File { .. });
        let to_hash = hash_string(to.name);
        // Every kind: a lookup finding the replaced inode once the node has moved waits for this.
        let arriving = local.arriving.claim((to.node, to_hash));
        // Under way until the rename has respelled the inode and gathered, for a create below it.
        let _renaming = if file {
            None
        } else {
            RenameUnderWay::begin(self.inodes(), ino)
        };
        // A directory's host path before, to gather what a create racing the rename made under it.
        let gathered_from = if file {
            None
        } else {
            self.at_host_path(from.dir, Some(spelled.as_bytes()), |old| {
                Ok(old.as_str().to_owned())
            })
            .ok()
        };
        // The target watched as a create's entry is: a rename above it moves the host directory.
        let moved = self.at_host_path(from.dir, Some(spelled.as_bytes()), |old| {
            self.at_host_path_watched(to.dir, Some(to.name.as_bytes()), |new, made_dirs| {
                self.write_step_allowed(WriteStep::RenameTargetBuilt { dir: to.dir });
                self.with_host_dirs_made(to.dir, made_dirs, || {
                    let rename = || {
                        if no_replace {
                            local.rename_no_replace(old, new)
                        } else {
                            match local.rename(old, new) {
                                // A directory respelled with no host directory yet gets one in
                                // the new spelling, which the next mount's State takes.
                                Err(Errno::ENOENT) if same_name && !file => {
                                    match local.mkdir(new, 0o755) {
                                        Err(Errno::EEXIST) => Ok(()),
                                        made => made,
                                    }
                                }
                                renamed => renamed,
                            }
                        }
                    };
                    if file {
                        self.while_local_file(ino, rename)
                    } else {
                        rename()
                    }
                })
            })
        });
        match moved {
            Ok(((), Some(raced))) => self.settle_host_entry(local, to.dir, to.name, &raced).await,
            Ok(((), None)) => {}
            Err(errno) => {
                self.restore_aside(local, to.dir, target_spelled, aside);
                return Err(errno.into());
            }
        }
        // Sampled before the State step, as a removal samples it.
        let sampled = self.commit_reading();
        let reattached = self
            .rename_in_state(served, node_id, to, (target.node, target.absent.as_ref()))
            .await;
        // Whether the node counted as added as it moved, read under the move's locks: a settle
        // that dropped the add action before shows here, and one after finds the node moved.
        let added = match reattached {
            Ok(added) => added,
            Err(errno) => {
                let back = self.at_host_path(to.dir, Some(to.name.as_bytes()), |new| {
                    self.at_host_path(from.dir, Some(spelled.as_bytes()), |old| {
                        local.rename(new, old)
                    })
                });
                if let Err(back) = back {
                    let _ = self.at_host_path(to.dir, Some(to.name.as_bytes()), |new| {
                        self.at_host_path(from.dir, Some(spelled.as_bytes()), |old| {
                            lore_warn!(
                                "FUSE could not move '{}' back to '{}': {back:?}",
                                new.as_str(),
                                old.as_str()
                            );
                            Ok(())
                        })
                    });
                }
                self.restore_aside(local, to.dir, target_spelled, aside);
                return Err(errno.into());
            }
        };
        finish_aside(aside);
        self.write_step_allowed(WriteStep::NodeMoved { dir: to.dir, file });
        // A commit may store an added node while content is served or its settle is owed: its
        // old path is recorded.
        if !same_name && (!added || sampled || self.commit_reading()) {
            let _ = self.at_host_path(from.dir, Some(spelled.as_bytes()), |old| {
                self.record_deletion(old.as_str());
                Ok(())
            });
        }
        let from_hash = hash_string(from.name);
        self.inodes()
            .move_child(ino, replaced, to.dir, to.name, to_hash);
        if replaced.is_some_and(|replaced| replaced != ino) {
            self.write_step_allowed(WriteStep::TargetUnindexed { dir: to.dir });
        }
        // The inode stands at the name: nothing after needs the claim.
        drop(arriving);
        if let Some(old) = gathered_from {
            let gathered = self.at_host_path(ino, None, |new| {
                let moved = new.as_str() != old;
                if moved {
                    Self::gather_after_rename(local, &old, new.as_str());
                }
                Ok(moved)
            });
            if gathered == Ok(true) {
                self.write_step_allowed(WriteStep::Gathered);
            }
        }
        self.invalidate_spellings(from.dir, from_hash, from.name.as_bytes());
        self.invalidate_spellings(to.dir, to_hash, to.name.as_bytes());
        // The kernel moves the source's dentry to the name it renamed to, so it caches that
        // spelling with no LOOKUP recording it: a swap removing the name drops it only where it
        // is recorded.
        self.spellings()
            .record(to.dir, to_hash, to.name.as_bytes(), true);
        self.touch_dir(from.dir);
        self.touch_dir(to.dir);
        Ok(())
    }

    /// Moves into the host directory `new`, a renamed directory's, what a create made under its
    /// old host path `old` before the rename respelled the inode, a create building its path
    /// from the inode names. One made later moves itself ([`Self::settle_host_entry`]).
    fn gather_after_rename(local: &LocalFiles, old: &str, new: &str) {
        let root = local.root_path();
        gather_host_dir(&root.join(old), &root.join(new));
    }

    /// Moves `node_id` to `to.name` below `to.node` under the served chain rule, replacing
    /// `holder`, the node the name held, in the same critical section
    /// ([`ReattachCheck::Replacing`]); onto a free name, checked against the directory's log
    /// where `absent` is given ([`ReattachCheck::Absent`]). Answers whether the node counted as
    /// added as it moved; a directory holding no local content, moved to another place, is
    /// `EXDEV` with nothing moved ([`crate::state::ServedNodes::node_reattach_local`]).
    async fn rename_in_state(
        &self,
        served: &Served,
        node_id: NodeID,
        to: Side<'_>,
        (holder, absent): (Option<NodeID>, Option<&Absent>),
    ) -> Result<bool, Errno> {
        if !self.write_step_allowed(WriteStep::Rename) {
            return Err(Errno::EIO);
        }
        let check = match (holder, absent) {
            (Some(holder), _) => ReattachCheck::Replacing(holder),
            (None, Some(absent)) => ReattachCheck::Absent(absent),
            (None, None) => ReattachCheck::Walk,
        };
        let moved = served
            .state
            .served()
            .node_reattach_local(served.repository.clone(), node_id, to.node, to.name, check)
            .await;
        match moved {
            Ok(Reattached::Moved { added }) => Ok(added),
            Ok(Reattached::Exists(_)) => Err(Errno::EEXIST),
            // A directory a commit made the revision's since the RENAME found it local: its
            // `write/` directory no longer carries what the commit flipped.
            Ok(Reattached::NotLocal) => Err(Errno::EXDEV),
            Err(error) => Err(state_errno("rename to", to.name, &error)),
        }
    }

    /// Whether every node below directory node `dir` holds local content at the zero address,
    /// so the whole subtree lives under `write/` and moves with a host rename.
    async fn subtree_is_local(&self, served: &Served, dir: NodeID) -> Result<bool, Errno> {
        let failed = |error: StateError| {
            lore_warn!("FUSE rename of a directory failed: {error}");
            Errno::EIO
        };
        let mut pending = vec![dir];
        while let Some(dir) = pending.pop() {
            for child in served
                .state
                .node_children(served.repository.clone(), dir)
                .await
                .map_err(failed)?
            {
                let node = served
                    .state
                    .node(served.repository.clone(), child)
                    .await
                    .map_err(failed)?;
                if !node.ext_flags().contains(NodeExtFlags::LocalContent)
                    || !node.address.hash.is_zero()
                {
                    return Ok(false);
                }
                if node.is_directory() {
                    pending.push(child);
                }
            }
        }
        Ok(true)
    }

    /// FSYNC on the worker: a tracked file has nothing to sync, and a local file's flush answers
    /// [`ReadFault::WouldFetch`], to run off the worker ([`Self::fsync_node`]). Without `write/`
    /// nothing is ever written, and `ENOSYS` makes the kernel treat every later fsync as a
    /// success.
    pub(crate) fn fsync_local_node(&self, ino: u64) -> Result<(), ReadFault> {
        if self.local_files().is_none() {
            return Err(Errno::ENOSYS.into());
        }
        let local = self
            .inodes()
            .get(ino)
            .ok_or(Errno::ENOENT)?
            .kind
            .is_local_file();
        if local {
            return Err(ReadFault::WouldFetch);
        }
        Ok(())
    }

    /// FSYNC: a local file's host file reaches stable storage; a tracked file has nothing to
    /// sync. Without `write/` it is `ENOSYS`, as [`Self::fsync_local_node`] answers.
    pub(crate) async fn fsync_node(&self, ino: u64, datasync: bool) -> Result<(), Errno> {
        match self.fsync_local_node(ino) {
            Ok(()) => return Ok(()),
            Err(ReadFault::Errno(errno)) => return Err(errno),
            Err(ReadFault::WouldFetch) => {}
        }
        // Safety: FUSE runs on Linux, whose backends require nothing of a wrapped file.
        let file = unsafe {
            lore_io::IoFile::from_std(lore_io::IoDriver::global(), self.descriptor(ino)?)
        }
        .map_err(|error| io_errno(&error))?;
        let synced = if datasync {
            file.sync_data().await
        } else {
            file.sync_all().await
        };
        synced.map_err(|error| io_errno(&error))
    }

    /// Fetches what a request on `name` in `parent` reads: the directory's listing and, for a
    /// directory the name holds, its chain. A request that attaches a node reads nothing more:
    /// the served State takes new nodes from fresh blocks ([`State::attach_slot_resident`]).
    pub(crate) async fn fetch_entry(&self, parent: u64, name: &[u8]) -> Result<(), Errno> {
        let Ok(name) = std::str::from_utf8(name) else {
            return Ok(());
        };
        let served = self.served();
        let resolved = self.resolve(&served, parent, name, true).await?;
        self.fetch_held_dir(&served, parent, name, resolved.found)
            .await
    }

    /// Fetches what a RENAME of `name` in `parent` to `newname` in `newparent`, with `flags`,
    /// reads: both directories' listings, the chain of a directory the target names, and a
    /// tracked source file's content ([`Self::stage_copy`]), unless the target is a directory
    /// or, under `RENAME_NOREPLACE`, exists, where the rename fails before any copy-up.
    pub(crate) async fn fetch_rename(
        &self,
        (parent, name): (u64, &[u8]),
        (newparent, newname): (u64, &[u8]),
        flags: u32,
    ) -> Result<(), Errno> {
        let (Ok(name), Ok(newname)) = (std::str::from_utf8(name), std::str::from_utf8(newname))
        else {
            return Ok(());
        };
        let served = self.served();
        let source = self.resolve(&served, parent, name, true).await?;
        let target = self.resolve(&served, newparent, newname, true).await?;
        self.fetch_held_dir(&served, newparent, newname, target.found)
            .await?;
        let fails_first = target.found.is_dir()
            || (target.found != Found::Nothing && flags & libc::RENAME_NOREPLACE != 0);
        if source.found == (Found::File { local: false })
            && !fails_first
            && let Some(ino) = self.inodes().find_child(parent, hash_string(name), name)
        {
            self.stage_copy(ino, None).await?;
        }
        Ok(())
    }

    /// Fetches the chain of the directory `found` in `parent` under `name`, which its emptiness
    /// check reads, where the kernel holds its inode, as it does for the directory an RMDIR or a
    /// RENAME over it names.
    async fn fetch_held_dir(
        &self,
        served: &Served,
        parent: u64,
        name: &str,
        found: Found,
    ) -> Result<(), Errno> {
        if !found.is_dir() {
            return Ok(());
        }
        let Some(ino) = self.inodes().find_child(parent, hash_string(name), name) else {
            return Ok(());
        };
        self.dir_is_empty(served, ino, true, true)
            .await
            .map(drop)
            .map_err(Errno::from)
    }

    /// FALLOCATE on the host file, a tracked one copied up first.
    pub(crate) async fn fallocate_node(
        &self,
        ino: u64,
        (offset, length, mode): (u64, u64, u32),
        fetch: bool,
    ) -> Result<(), ReadFault> {
        self.writable()?;
        self.write_local_file(ino, None, fetch, |file| {
            allocate(file, mode, offset, length)
        })
        .await
    }

    /// READLINK of a local symlink.
    pub(crate) fn readlink_node(&self, ino: u64, out: &mut [u8]) -> Result<usize, Errno> {
        let local = self.local_files().ok_or(Errno::EINVAL)?;
        if !matches!(
            self.inodes().get(ino).ok_or(Errno::ENOENT)?.kind,
            InodeKind::Symlink
        ) {
            return Err(Errno::EINVAL);
        }
        let mut path = HostPath::new();
        self.host_path(ino, None, &mut path)?;
        local.readlink(&path, out)
    }

    /// STATFS: `tree`, the State's figures, plus the size of `write/`'s file system, and its
    /// free space and inodes as what is free.
    pub(crate) fn statfs_with_local(&self, tree: Kstatfs) -> Kstatfs {
        let Some(host) = self.local_files().and_then(|local| local.statvfs().ok()) else {
            return tree;
        };
        let frsize = u64::from(tree.frsize.max(1));
        let blocks = |count: u64| count.saturating_mul(host.f_frsize) / frsize;
        Kstatfs {
            blocks: tree.blocks.saturating_add(blocks(host.f_blocks)),
            bfree: blocks(host.f_bfree),
            bavail: blocks(host.f_bavail),
            files: tree.files.saturating_add(host.f_files),
            ffree: host.f_ffree,
            ..tree
        }
    }

    /// Whether directory `ino` holds no child the view shows, read from its chain in the State:
    /// an emptied directory reads only its own block, which its parent's listing or the removals
    /// that emptied it keep resident. A walk whose directory's chain count moves by a reattach
    /// ([`REATTACH_CHANGES`]) may have missed a child moved to the head behind it: without
    /// `fetch` it answers [`ReadFault::WouldFetch`], as it does for a chain block or name table not
    /// in memory, a child moved to another directory, or a sibling cycle; with `fetch` it walks
    /// again where `again`, at most [`CHAIN_WALK_RESTARTS`] times, then answers `EAGAIN`. An empty
    /// directory answers its chain as read before the walk that found it empty, which a detach
    /// compares under the lock ([`Detach::IfUnchanged`]). Without `again`, a fetching walk answers
    /// so however the chain moved under it, for a caller that checks the chain itself, as RMDIR
    /// does, so one budget bounds its checks.
    async fn dir_is_empty(
        &self,
        served: &Served,
        ino: u64,
        fetch: bool,
        again: bool,
    ) -> Result<Option<IfUnchanged>, ReadFault> {
        let (node, filter) = self.dir_node(ino)?;
        if !fetch {
            return self
                .resident_dir_is_empty(served, (ino, node, filter))
                .await;
        }
        let failed = |error: StateError| {
            lore_warn!("FUSE emptiness check of directory {ino} failed: {error}");
            Errno::EIO
        };
        let dir_block = served
            .state
            .block(served.repository.clone(), NodeBlock::index(node))
            .await
            .map_err(failed)?;
        let mut dir_path = None;
        for _ in 0..=CHAIN_WALK_RESTARTS {
            // The head taken out, so no await holds the record.
            let (head, changes) = {
                let (dir, changes) = dir_block.node_with_chain_changes(Node::index(node));
                (dir.child(), changes)
            };
            let mut children = StateNodeChildrenWithNameIterator::new(
                served.state.clone(),
                served.repository.clone(),
                node,
            )
            .await
            .map_err(failed)?;
            if let Some(head) = head {
                self.emptiness_step(head).await;
            }
            while let Some((_, child, lock)) = children.next().await.map_err(failed)? {
                let name = Name::from_slice(lock.as_bytes());
                drop(lock);
                if child.is_staged_delete() {
                    continue;
                }
                let view = (ino, filter, &mut dir_path);
                if !self.hidden_child(served, view, &name, child.is_directory())? {
                    return Ok(None);
                }
            }
            let now = dir_block.chain_changes(Node::index(node));
            if !again || now.wrapping_sub(changes) < REATTACH_CHANGES {
                return Ok(Some(IfUnchanged::new(dir_block, node, changes)));
            }
        }
        Err(Errno::EAGAIN.into())
    }

    /// [`Self::dir_is_empty`] of directory `ino`, node `node` with view state `filter`, reading
    /// only blocks and name tables in memory. A child whose name cannot be read counts as
    /// present, as a child it cannot tell hidden.
    async fn resident_dir_is_empty(
        &self,
        served: &Served,
        (ino, node, filter): (u64, NodeID, FilterStates),
    ) -> Result<Option<IfUnchanged>, ReadFault> {
        let resident = |node: NodeID| {
            served
                .state
                .resident_block(NodeBlock::index(node))
                .ok_or(ReadFault::WouldFetch)
        };
        let dir_block = resident(node)?;
        let (mut next, changes) = {
            let (dir, changes) = dir_block.node_with_chain_changes(Node::index(node));
            (dir.child(), changes)
        };
        let mut cycles = SiblingCycleGuard::new(node);
        let mut current: Option<(usize, Arc<NodeBlock>)> = None;
        let mut dir_path = None;
        while let Some(child_id) = next {
            cycles
                .observe(child_id)
                .map_err(|_cycle| ReadFault::WouldFetch)?;
            let index = NodeBlock::index(child_id);
            if !matches!(&current, Some((held, _)) if *held == index) {
                let block = resident(child_id)?;
                if !block.is_nametable_deserialized() {
                    return Err(ReadFault::WouldFetch);
                }
                current = Some((index, block));
            }
            self.emptiness_step(child_id).await;
            let Some((_, block)) = &current else {
                return Err(ReadFault::WouldFetch);
            };
            let (child, name) = block.node_and_name_ref(Node::index(child_id));
            if child.parent != node {
                return Err(ReadFault::WouldFetch);
            }
            next = child.sibling();
            let name = name.map(|lock| Name::from_slice(lock.as_bytes())).ok();
            if child.is_discarded() || child.is_staged_delete() {
                continue;
            }
            let Some(name) = name else {
                return Ok(None);
            };
            let view = (ino, filter, &mut dir_path);
            if !self.hidden_child(served, view, &name, child.is_directory())? {
                return Ok(None);
            }
        }
        let now = dir_block.chain_changes(Node::index(node));
        if now.wrapping_sub(changes) >= REATTACH_CHANGES {
            return Err(ReadFault::WouldFetch);
        }
        Ok(Some(IfUnchanged::new(dir_block, node, changes)))
    }

    /// Whether the view hides the child named `name` of directory `ino` with view state
    /// `filter`; `dir_path` caches the directory's path across the children of one walk.
    fn hidden_child(
        &self,
        served: &Served,
        (ino, filter, dir_path): (u64, FilterStates, &mut Option<RelativePath>),
        name: &[u8],
        is_dir: bool,
    ) -> Result<bool, Errno> {
        let name = std::str::from_utf8(name).map_err(|_invalid| Errno::EIO)?;
        let dir_path = match dir_path {
            Some(path) => path,
            None => dir_path.insert(self.path_of(ino)?.0),
        };
        Ok(self
            .view_filter(&served.repository, filter, &dir_path.join(name), is_dir)
            .1)
    }

    /// The inode the kernel holds for `name` in `parent`, else one looked up now; `true` when
    /// this lookup counted one the caller drops. Without `fetch`, a lookup that needs a listing
    /// build answers [`ReadFault::WouldFetch`].
    async fn held_child(
        &self,
        served: &Served,
        parent: u64,
        name: &str,
        fetch: bool,
    ) -> Result<(u64, bool), ReadFault> {
        if let Some(ino) = self.inodes().find_child(parent, hash_string(name), name) {
            return Ok((ino, false));
        }
        // Any spelling, as the inode table above finds one.
        let reply = Box::pin(self.lookup_with(
            served,
            parent,
            name.as_bytes(),
            fetch,
            (OnStale::ByName, LookupCase::Folded),
        ))
        .await?;
        if reply.ino == 0 {
            return Err(Errno::ENOENT.into());
        }
        Ok((reply.ino, true))
    }

    /// Whether inode `ino` is a file whose bytes live under `write/`.
    fn is_local_file(&self, ino: u64) -> bool {
        self.inodes()
            .get(ino)
            .is_some_and(|entry| entry.kind.is_local_file())
    }

    /// Whether inode `ino` is a file served from the store.
    fn is_tracked_file(&self, ino: u64) -> bool {
        self.inodes()
            .get(ino)
            .is_some_and(|entry| matches!(entry.kind, InodeKind::File { local: false, .. }))
    }

    /// Runs `host`, a move of local file `ino`'s host file, under the inode's read guard, which a
    /// commit's flip takes for writing ([`Self::flip_to_tracked`]); [`FLIPPED`], running nothing,
    /// where the inode is no longer local. `host` must not read the inode table.
    fn while_local_file<T>(
        &self,
        ino: u64,
        host: impl FnOnce() -> Result<T, Errno>,
    ) -> Result<T, Errno> {
        let entry = self.inodes().get(ino).ok_or(Errno::ENOENT)?;
        if !entry.kind.is_local_file() {
            return Err(FLIPPED);
        }
        host()
    }

    /// Keeps local file `ino` open for the kernel's handles before its path stops reaching it.
    fn pin_local_file(&self, ino: u64) -> Result<(), Errno> {
        let file = self.descriptor(ino)?;
        self.writable()?.pin_file(ino, file);
        Ok(())
    }

    /// Drops the kernel's dentries for every other spelling of `name`'s key in `parent`, which
    /// the change being answered leaves stale. Queued on the mount's notification thread without
    /// waiting: the kernel holds `parent` locked until this request is answered.
    fn invalidate_spellings(&self, parent: u64, name_hash: u64, name: &[u8]) {
        let Some(taken) = self.spellings().take(parent, name_hash) else {
            return;
        };
        let Some(queue) = self.notifications() else {
            return;
        };
        let stale: SmallVec<[Name; 2]> = taken
            .seen
            .into_iter()
            .filter(|seen| seen.as_slice() != name)
            .collect();
        queue.post_entries(parent, stale);
    }

    /// Runs `host`, a host operation below directory `dir`, and again once the host directories
    /// of `dir` and its ancestors exist where it found one missing.
    fn with_host_dirs<T>(
        &self,
        dir: u64,
        host: impl FnMut() -> Result<T, Errno>,
    ) -> Result<T, Errno> {
        self.with_host_dirs_made(dir, &mut 0, host)
    }

    /// [`Self::with_host_dirs`], setting `made` to how many of the deepest host directories it
    /// made ([`Self::ensure_host_dirs`]).
    fn with_host_dirs_made<T>(
        &self,
        dir: u64,
        made: &mut usize,
        mut host: impl FnMut() -> Result<T, Errno>,
    ) -> Result<T, Errno> {
        match host() {
            Err(Errno::ENOENT) => {
                *made = self.ensure_host_dirs(dir)?;
                host()
            }
            done => done,
        }
    }

    /// Creates the missing host directories of directory `dir` and its ancestors under `write/`,
    /// answering how many it made counted up from `dir`'s own until one it found standing.
    fn ensure_host_dirs(&self, dir: u64) -> Result<usize, Errno> {
        let local = self.writable()?;
        let mut made = 0;
        let mut chain: SmallVec<[u64; 16]> = SmallVec::new();
        let mut ino = dir;
        while ino != ROOT_INO {
            if chain.len() >= MAX_DEPTH {
                return Err(Errno::EIO);
            }
            chain.push(ino);
            ino = self.inodes().get(ino).ok_or(Errno::ENOENT)?.parent;
        }
        for &ino in chain.iter().rev() {
            let mut path = HostPath::new();
            self.host_path(ino, None, &mut path)?;
            self.write_step_allowed(WriteStep::HostDirBuilt { dir: ino });
            match local.mkdir(&path, 0o755) {
                Ok(()) => made += 1,
                Err(Errno::EEXIST) => made = 0,
                Err(errno) => return Err(errno),
            }
        }
        Ok(made)
    }

    /// Stamps directory `ino` with the current time after its entries changed.
    fn touch_dir(&self, ino: u64) {
        if let Some(mut entry) = self.inodes().get_mut(ino)
            && matches!(entry.kind, InodeKind::Dir { .. })
        {
            entry.mtime = crate::util::time::timestamp();
        }
    }

    /// Reconciles the filesystem State with `committed`, the revision a commit stored from the
    /// files of `served`, which it read through one operation and whose stored content it handed
    /// over: each file nothing changed since its read is flipped to that content
    /// ([`Self::flip_to_tracked`]); a committed file left dirty no longer counts as added; each
    /// directory above a committed file takes its committed address, clean, where it has no
    /// dirty child, and otherwise no longer counts as added either; the deletion list
    /// keeps what the revision still holds; and the State stands at the revision. A file is found
    /// by the node, place and inode recorded at its read, never by its path. A file or directory
    /// that cannot be read stays dirty, which is where it stood. Serialized with swaps.
    ///
    /// `handed` are the directories the commit adds or moves, settled as the others are, empty
    /// ones included. The host directories of those cleared go once the deletion list is
    /// rewritten. Answers whether every step completed with no error, swallowed ones included,
    /// which alone ends the settle the commit owes ([`SettleOwed`]).
    #[lore_macro::test_pub]
    pub(crate) async fn committed(
        &self,
        repository: Arc<RepositoryContext>,
        committed: Arc<State>,
        served: &ServedContent,
        handed: Vec<RelativePath>,
    ) -> Result<bool, FuseProviderError> {
        if self.local_files().is_none() {
            return Ok(true);
        }
        let _swaps = self.swap_plan().lock().await;
        if let Some(hook) = self.commit_hook() {
            hook().await;
        }
        let current = self.served();
        current.state.set_revision(committed.revision());
        let revision = (&*committed, &repository);
        let mut clean = true;
        let mut dirs = handed;
        let mut kept = Vec::new();
        let mut unheld = Vec::new();
        for file in served.stored_files() {
            match self.flip_to_tracked(&current, &file).await {
                Ok(Flip::Flipped) => {
                    self.count_commit_outcome(true);
                    dirs.push(file.spelled.parent_path());
                }
                Ok(Flip::Kept) => kept.push(file),
                Ok(Flip::Unheld(held)) => unheld.push(*held),
                Err(error) => {
                    lore_warn!(
                        "FUSE flipping '{}' to a commit failed: {error}",
                        file.spelled
                    );
                    clean = false;
                    kept.push(file);
                }
            }
        }
        self.write_step_allowed(WriteStep::Classified);
        for (settled, file) in unheld.into_iter().enumerate() {
            if settled % SETTLES_BETWEEN_YIELDS == SETTLES_BETWEEN_YIELDS - 1 {
                tokio::task::yield_now().await;
            }
            if self.settle_unheld(&current, &file) {
                self.count_commit_outcome(true);
                dirs.push(file.served.spelled.parent_path());
            } else {
                kept.push(file.served);
            }
        }
        let mut gone = Vec::new();
        for file in kept {
            self.count_commit_outcome(false);
            match self.keep_committed(&current, &file).await {
                Ok(true) => dirs.push(file.spelled.parent_path()),
                Ok(false) => gone.push(file.spelled),
                Err(error) => {
                    lore_warn!(
                        "FUSE keeping '{}' after a commit failed: {error}",
                        file.spelled
                    );
                    clean = false;
                }
            }
        }
        // Unread, the list is left whole, and the settle stays owed.
        let listed = self.read_deletions().unwrap_or_else(|| {
            clean = false;
            (0, Vec::new())
        });
        let mut in_revision = ChildIndex::new(revision);
        let mut in_state = ChildIndex::new((&current.state, &current.repository));
        for path in &listed.1 {
            match in_revision.node(path).await {
                Ok(None) => dirs.push(path.parent_path()),
                Ok(Some(_)) => {}
                Err(_) => clean = false,
            }
        }
        // Boxed, once per commit: the settle's futures would otherwise sit in the commit's.
        let settled = Box::pin(self.settle_directories(
            &current,
            revision,
            (&mut in_revision, &mut in_state),
            dirs,
        ))
        .await;
        gone.extend(settled.gone);
        if !self.write_step_allowed(WriteStep::DirectoriesSettled) {
            return Err(FuseProviderError::internal(
                "the reconcile stopped after its settles",
            ));
        }
        self.rewrite_deletions((&mut in_revision, &mut in_state), listed, gone)
            .await?;
        self.remove_settled_host_dirs(&current, &settled.cleared);
        Ok(clean && settled.clean)
    }

    /// Arms the settle a commit owes before its revision may become current ([`SettleOwed`]),
    /// for `handed`, the directories the commit adds or moves. The count rises first; only then
    /// is each handed path looked up in the served State, so a removal racing the lookups either
    /// samples the count or left its path to the lookup. A path holding no directory, removed
    /// since the stage, has its deletion recorded now, as no settle reaches it; the holder's
    /// flags are left alone. A lookup or record that fails leaves the settle owed for good.
    #[lore_macro::test_pub]
    pub(crate) async fn owe_settle(self: &Arc<Self>, handed: Vec<RelativePath>) -> SettleOwed {
        let mut owed = SettleOwed::arm(self.clone(), handed);
        if self.local_files().is_none() || owed.handed.is_empty() {
            return owed;
        }
        let current = self.served();
        let mut in_state = ChildIndex::new((&current.state, &current.repository));
        let mut unrecorded = false;
        for path in &owed.handed {
            let recorded = match in_state.record(path).await {
                Ok(Some((_, node))) if !node.is_discarded() && node.is_directory() => true,
                Ok(_) => self.record_deletion(path.as_str()),
                Err(_) => false,
            };
            unrecorded |= !recorded;
        }
        if unrecorded {
            owed.stick();
        }
        owed
    }

    /// Flips the file `served` records, whose content a commit read and stored, to that content
    /// served from the store, clean, where nothing changed it since the read: its node still
    /// stands where it stood, and the inode the kernel held for it is still that incarnation with
    /// its write generation unmoved, or, where none was held, none is now or the one now holds no
    /// write, and no written inode standing for its node has been forgotten since the read. A
    /// file arriving at its name leaves it alone ([`LocalFiles::arriving`]). A local file leaves
    /// `write/`.
    /// One it did not flip stays dirty; one no inode holds is settled after the others
    /// ([`Self::settle_unheld`]).
    pub(crate) async fn flip_to_tracked(
        &self,
        current: &Served,
        served: &ServedFile,
    ) -> Result<Flip, FuseProviderError> {
        let Some(stored) = served.stored else {
            return Ok(Flip::Kept);
        };
        let local = self
            .writable()
            .map_err(|errno| flip_failed(&served.spelled, errno))?;
        let state = &current.state;
        let block = state
            .block(current.repository.clone(), NodeBlock::index(served.node))
            .await
            .forward_any::<FuseProviderError>("reading the node a commit stored")?;
        let record = *block.read().node(Node::index(served.node));
        if record.is_discarded()
            || (record.parent, record.name_hash) != served.place
            || !record.is_file()
            || record.ext_flags().contains(NodeExtFlags::Symlink)
        {
            return Ok(Flip::Kept);
        }
        let place = served.place;
        let flipped = if !record.ext_flags().contains(NodeExtFlags::LocalContent) {
            record.address.hash == stored.address.hash
                && state.node_change_in_place(&block, served.node, place, |held| {
                    held.address.hash == stored.address.hash && take_committed(held, &stored, false)
                })
        } else if let Some(ino) = served.ino {
            self.flip_held(
                &served.spelled,
                (ino, served.incarnation, served.generation),
                (state, &block, served.node, place),
                &stored,
                None,
            )?
        } else if local.arriving.holds(place)
            || self
                .inodes()
                .written_forgotten_since(served.node, served.incarnation)
        {
            false
        } else if let Some(ino) = {
            self.write_step_allowed(WriteStep::FlipChecked { node: served.node });
            self.live_inode(&served.spelled)
        } {
            match self.inode_incarnation(ino) {
                Some((incarnation, 0)) => self.flip_held(
                    &served.spelled,
                    (ino, incarnation, 0),
                    (state, &block, served.node, place),
                    &stored,
                    Some(served.incarnation),
                )?,
                _ => false,
            }
        } else {
            return Ok(self
                .unheld_file(&served.spelled)
                .map_or(Flip::Kept, |host| {
                    Flip::Unheld(Box::new(Unheld {
                        served: served.clone(),
                        block,
                        stored,
                        host,
                    }))
                }));
        };
        if !flipped {
            return Ok(Flip::Kept);
        }
        Ok(Flip::Flipped)
    }

    /// The `stat` of the host file of `path`, a local file no inode holds; `None` where none is
    /// there.
    fn unheld_file(&self, path: &RelativePath) -> Option<libc::stat> {
        let local = self.local_files()?;
        local.stat(&relative_host_path(path)?).ok()
    }

    /// [`Self::flip_to_tracked`] of local file inode `ino`: under the inode's write guard, which a
    /// WRITE, SETATTR or FALLOCATE holds for reading across its host operation and a RENAME or
    /// UNLINK across its move of the host file, the inode must still name `path`, stand for
    /// node `node_id`, be incarnation `incarnation` holding local content and its write
    /// generation `generation`, and `write/<path>` must be its host file; then the node takes
    /// `stored`, the inode turns
    /// store-served, and the host file leaves `write/`. Its descriptor stays cached until FORGET
    /// or a later copy-up, so a request that read the inode as local before the flip still reads
    /// the committed bytes through it. With `since`, an inode found after a read that no inode
    /// held, no write may be arriving at `place` and no written inode of the node forgotten since
    /// that read's count: the forget marks the node under the number's shard lock, which this
    /// inode's insert took after it, so the check under its guard sees the mark.
    fn flip_held(
        &self,
        path: &RelativePath,
        (ino, incarnation, generation): (u64, u64, u64),
        (state, block, node_id, place): (&State, &Arc<NodeBlock>, NodeID, (NodeID, u64)),
        stored: &StoredContent,
        since: Option<u64>,
    ) -> Result<bool, FuseProviderError> {
        let local = self.writable().map_err(|errno| flip_failed(path, errno))?;
        let mut host = HostPath::new();
        if self.host_path(ino, None, &mut host).is_err() || host.as_str() != path.as_str() {
            return Ok(false);
        }
        let (parent, name) = {
            let Some(entry) = self.inodes().get(ino) else {
                return Ok(false);
            };
            (entry.parent, entry.name.clone())
        };
        let file = self
            .descriptor(ino)
            .map_err(|errno| flip_failed(path, errno))?;
        let held = file_stat(&file).map_err(|errno| flip_failed(path, errno))?;
        let flipped = {
            let Some(mut entry) = self.inodes().get_mut(ino) else {
                return Ok(false);
            };
            // The inode must stand for the node flipped: after a swap of two directories
            // holding the same names, the read path holds the other node's inode (H25).
            let unchanged = !entry.orphaned
                && entry.node == node_id
                && entry.incarnation == incarnation
                && entry.parent == parent
                && entry.name == name
                && entry.kind.is_local_file()
                && entry.write_generation.load(Ordering::Acquire) == generation
                && generation.is_multiple_of(2)
                && local.stat(&host).is_ok_and(|at_path| {
                    at_path.st_dev == held.st_dev && at_path.st_ino == held.st_ino
                })
                && since.is_none_or(|since| {
                    !local.arriving.holds(place)
                        && !self.inodes().written_forgotten_since(node_id, since)
                });
            let taken = unchanged
                && state.node_change_in_place(block, node_id, place, |held| {
                    take_committed(held, stored, true)
                });
            if taken {
                entry.kind = committed_kind(stored);
                entry.host_epoch += 1;
                entry.host_file = 0;
                entry.mtime = stat_millis(&held);
                if let Err(errno) = local.remove_path(&host, false) {
                    lore_warn!(
                        "FUSE could not remove the committed '{path}' from write/: {errno:?}"
                    );
                }
            }
            taken
        };
        if flipped {
            self.fragment_cache().forget(ino);
        }
        Ok(flipped)
    }

    /// Flips `file`, a local file no inode held at the commit's read, as [`Self::flip_to_tracked`]
    /// flips one, its node never standing clean while its host file may go back. With its name
    /// claimed ([`LocalFiles::settling`]), its host file is taken aside, so no request reaches it
    /// and none moves it, and must be the one found before, its path still leading to its node;
    /// then [`Self::settle_unreached`] checks the file and changes its node; and the host file
    /// leaves `write/`. A file that fails
    /// has its host file put back into its directory, wherever that has moved meanwhile, and its
    /// node never changed. Answers whether the file flipped.
    fn settle_unheld(&self, current: &Served, file: &Unheld) -> bool {
        let Some(local) = self.local_files() else {
            return false;
        };
        let _settling = Settling {
            claim: Some(local.settling.claim(file.served.place)),
            settled: &local.settled,
        };
        let settled = self.settle_claimed(current, local, file);
        local.settle_log.record(file.served.place);
        settled
    }

    /// [`Self::settle_unheld`] with the name claimed.
    fn settle_claimed(&self, current: &Served, local: &LocalFiles, file: &Unheld) -> bool {
        let path = &file.served.spelled;
        self.write_step_allowed(WriteStep::Settle {
            node: file.served.node,
            aside: false,
        });
        let parent = || {
            if path.as_str().contains('/') {
                self.live_inode(&path.parent_path())
            } else {
                Some(ROOT_INO)
            }
        };
        let held = parent();
        // Checked before the host file goes aside too, so the common case of a path leading
        // elsewhere never hides another node's host file; checked again after, for the race.
        if !path_leads_to(&current.state, file.served.node, path) {
            return false;
        }
        let aside = match local.take_aside(path) {
            Ok(aside) => aside,
            Err(errno) => {
                if errno != Errno::ENOENT {
                    lore_warn!("FUSE could not take the committed '{path}' aside: {errno:?}");
                }
                return false;
            }
        };
        self.write_step_allowed(WriteStep::Settle {
            node: file.served.node,
            aside: true,
        });
        // With the host file aside, a lookup below a directory inode made since makes no local
        // inode of the file, so one found now covers every inode that could hold it.
        let dir = held.or_else(parent);
        // The host file at the read path may be another node's: after a swap of two directories
        // holding the same names, the path leads elsewhere (H25).
        let flipped = std::fs::symlink_metadata(&aside.staged)
            .is_ok_and(|taken| taken.dev() == file.host.st_dev && taken.ino() == file.host.st_ino)
            && path_leads_to(&current.state, file.served.node, path)
            && self.settle_unreached(current, local, file, dir);
        if let Err(errno) = aside.settle(flipped) {
            lore_warn!("FUSE could not settle '{path}' under write/: {errno:?}");
        }
        flipped
    }

    /// The checks and the change of [`Self::settle_unheld`] for `file`, its host file aside: no
    /// file is arriving at its name ([`LocalFiles::arriving`]) and no written inode standing for
    /// its node has been forgotten since the commit read it; the inode the kernel holds at the
    /// name, if any, stands for its node, is local and holds no write, and is held under its
    /// write guard throughout;
    /// then the node takes its committed content and the inode turns store-served. A lookup that
    /// read the node before cannot make an inode of what it read: with the name claimed, and once
    /// it is recorded settled, a new inode of it is refused ([`FuseFilesystem::settle_touched`]),
    /// and lookups of other names are never refused. The inode is found below `dir`: the directory inode held before the
    /// host file was taken, which a RENAME of a directory above does not move, or with none held
    /// then, one a lookup made since; with none at all, no inode below it holds the host file.
    /// Answers whether the node changed.
    fn settle_unreached(
        &self,
        current: &Served,
        local: &LocalFiles,
        file: &Unheld,
        dir: Option<u64>,
    ) -> bool {
        let served = &file.served;
        let unreached = || {
            !local.arriving.holds(served.place)
                && !self
                    .inodes()
                    .written_forgotten_since(served.node, served.incarnation)
        };
        let change = || {
            current
                .state
                .node_change_in_place(&file.block, served.node, served.place, |held| {
                    take_committed(held, &file.stored, true)
                })
        };
        let name = served.spelled.as_str().rsplit('/').next().unwrap_or("");
        let held = dir.and_then(|dir| self.inodes().find_child(dir, hash_string(name), name));
        let Some(mut entry) = held.and_then(|ino| self.inodes().get_mut(ino)) else {
            return unreached() && change();
        };
        let changed = entry.kind.is_local_file()
            && entry.node == served.node
            && entry.write_generation.load(Ordering::Acquire) == 0
            && unreached()
            && change();
        if changed {
            entry.kind = committed_kind(&file.stored);
            entry.host_epoch += 1;
            entry.host_file = 0;
            entry.mtime = stat_millis(&file.host);
            let ino = entry.ino;
            drop(entry);
            self.fragment_cache().forget(ino);
        }
        changed
    }

    /// Answers whether the State still holds a node at the path of `served`, a file the commit
    /// stored, and marks one marked as added as modified instead: the revision now holds the
    /// path, so a later removal of it is a deletion to record. The node recorded is checked where
    /// it stood, under the lock of the change; only where it moved is its directory asked for the
    /// name. A node that left the place, as a rename over it does, hands the place to its holder,
    /// at most [`CHAIN_WALK_RESTARTS`] times; past that the settle is not clean.
    async fn keep_committed(
        &self,
        current: &Served,
        served: &ServedFile,
    ) -> Result<bool, FuseProviderError> {
        let state = &current.state;
        let repository = &current.repository;
        let mut node = served.node;
        // A counter rather than a range: the range's iterator would sit in the future beside it.
        let mut tries = 0;
        loop {
            let block = state
                .block(repository.clone(), NodeBlock::index(node))
                .await
                .forward_any::<FuseProviderError>("reading a committed node")?;
            // Set only where the node stands at the place, under the lock of the change.
            let mut at_place = false;
            state.node_change_in_place(&block, node, served.place, |held| {
                at_place = true;
                no_longer_added(held)
            });
            drop(block);
            if at_place {
                return Ok(true);
            }
            tries += 1;
            if tries > CHAIN_WALK_RESTARTS {
                return Err(FuseProviderError::internal(
                    "a committed name changed hands on every try",
                ));
            }
            let (dir, name_hash) = served.place;
            node = match state.find_subnode(repository.clone(), dir, name_hash).await {
                Ok(holder) => holder,
                Err(error) if error.is_node_not_found() => return Ok(false),
                Err(error) => {
                    return Err(error).forward_any::<FuseProviderError>("finding a committed name");
                }
            };
        }
    }

    /// Settles each of `dirs`, and each of their ancestors, deepest first, where its children
    /// are, once the commit's flips are done, those of the directory `revision` holds at its path,
    /// none dirty and each holding the committed content: it takes the committed address, clean,
    /// a local one turning tracked, and its empty host directory goes. One the revision holds no
    /// longer counts as added or moved, settled or not. The clear is the served State's
    /// ([`crate::state::ServedNodes::node_clear`]), pending from before the children are read, so
    /// a change below meanwhile keeps the directory dirty. Directories are found through the
    /// indexes, the State's in `in_state`; one that moved away meanwhile, or that the index does
    /// not hold, is found again by name from the root. Answers the paths the State then holds no
    /// directory at, which the revision does, and the directories cleared, whose host
    /// directories the caller removes once the deletion list is rewritten.
    async fn settle_directories(
        &self,
        current: &Served,
        revision: (&State, &Arc<RepositoryContext>),
        (in_revision, in_state): (&mut ChildIndex<'_>, &mut ChildIndex<'_>),
        dirs: Vec<RelativePath>,
    ) -> Settled {
        let paths = with_ancestors(dirs);
        if paths.is_empty() {
            return Settled {
                gone: Vec::new(),
                cleared: Vec::new(),
                clean: true,
            };
        }
        let mut pass = Box::new(SettlePass {
            in_revision,
            in_state,
            scratch: ClearScratch::default(),
            settled: Settled {
                gone: Vec::new(),
                cleared: Vec::new(),
                clean: true,
            },
        });
        for (_, dir) in &paths {
            if let Err(error) = self
                .settle_directory(current, revision, &mut pass, dir)
                .await
            {
                lore_warn!("FUSE settling '{dir}' with a commit failed: {error}");
                pass.settled.clean = false;
            }
        }
        pass.settled
    }

    /// [`Self::settle_directories`] of one directory, `dir`, adding it to `settled.gone` where
    /// the State no longer holds a directory there and the revision does, and to
    /// `settled.cleared` where it cleared it.
    async fn settle_directory(
        &self,
        current: &Served,
        revision: StateIn<'_>,
        pass: &mut SettlePass<'_, '_, '_>,
        dir: &RelativePath,
    ) -> Result<(), FuseProviderError> {
        let failed = |error: StateError| {
            FuseProviderError::internal(format!("settling a committed directory: {error}"))
        };
        // What the settle reads of a record, copied out: no record is held across a lookup.
        let indexed = match pass.in_state.record(dir).await? {
            Some((node, record)) if record.is_directory() => Some((
                FoundDir::of(node, &record, (0, 0)),
                record.parent,
                record.name_hash,
            )),
            _ => None,
        };
        let indexed = match indexed {
            Some((found, parent, name_hash)) => {
                indexed_place(pass.in_state, dir, (parent, name_hash))
                    .await?
                    .map(|place| FoundDir { place, ..found })
            }
            None => None,
        };
        // The index's listings are cached: a path it does not hold is looked up afresh.
        let found = match indexed {
            Some(found) => Some(found),
            None => self.directory_afresh(current, dir).await?,
        };
        let Some(mut found) = found else {
            if pass.in_revision.node(dir).await?.is_some() {
                pass.settled.gone.push(dir.clone());
            }
            return Ok(());
        };
        if !found.dirty {
            return Ok(());
        }
        // The clear checks that it is a directory.
        let Some(committed) = pass.in_revision.node(dir).await? else {
            return Ok(());
        };
        let against = ClearAgainst {
            state: revision.0,
            repository: revision.1,
            node: committed,
        };
        let nodes = current.state.served();
        // A counter rather than a range: the range's iterator would sit in the future beside it.
        let mut tries = 0;
        while tries <= CHAIN_WALK_RESTARTS {
            tries += 1;
            let clear = ServedClear {
                place: found.place,
                against,
                spelled: against,
                scratch: &mut pass.scratch,
            };
            let node_id = found.node;
            let settling = async {
                self.write_step_allowed(WriteStep::DirectorySettling { node: node_id });
            };
            match nodes
                .node_clear(&current.repository, node_id, clear, settling)
                .await
                .map_err(failed)?
            {
                ClearOutcome::Kept => return Ok(()),
                ClearOutcome::Cleared => {
                    if node_id != ROOT_NODE {
                        self.settled_local_directory((node_id, dir), found.local);
                        pass.settled.cleared.push((node_id, dir.clone()));
                    }
                    return Ok(());
                }
                ClearOutcome::Moved => {
                    let Some(again) = self.directory_afresh(current, dir).await? else {
                        if pass.in_revision.node(dir).await?.is_some() {
                            pass.settled.gone.push(dir.clone());
                        }
                        return Ok(());
                    };
                    found = again;
                    self.write_step_allowed(WriteStep::SettleFound { node: found.node });
                }
            }
        }
        // The directory last found kept its action bits.
        lore_warn!("FUSE settling '{dir}' found a directory moving there on every try");
        pass.settled.clean = false;
        Ok(())
    }

    /// What follows a settle's clear of directory `node_id` at `dir`, `local` where the settle
    /// found it holding local content: its inode turns tracked, where the live inode at the path
    /// stands for it. A commit's settle removes its empty host directory after the deletion list
    /// is rewritten ([`Self::remove_settled_host_dirs`]).
    fn settled_local_directory(&self, (node_id, dir): (NodeID, &RelativePath), local: bool) {
        if local
            && let Some(ino) = self.live_inode(dir)
            && let Some(mut entry) = self.inodes().get_mut(ino)
            && let InodeKind::Dir { local, node, .. } = &mut entry.kind
            && *node == node_id
        {
            *local = false;
        }
    }

    /// Removes the empty host directories of `cleared`, the directories a settle cleared, once
    /// the deletion list is rewritten, so a commit cancelled before leaves each its `write/`
    /// entry. Each is checked first in O(depth), with no listing ([`Self::settled_place`]), and
    /// skipped where a create claims its name. One made again between the check and the removal,
    /// a live local directory there or a create claiming the name, has its host directory made
    /// again, with the default mode: the removal reads nothing of it first, which would cost every
    /// directory a stat for that rare race.
    fn remove_settled_host_dirs(&self, current: &Served, cleared: &[(NodeID, RelativePath)]) {
        let Some(local) = self.local_files() else {
            return;
        };
        for (node, dir) in cleared {
            let Some(place) = self.settled_place(current, *node, dir) else {
                continue;
            };
            let Some(host) = relative_host_path(dir) else {
                continue;
            };
            if local.arriving.holds(place) {
                continue;
            }
            // First, so a request resolved before that finds the host directory gone resolves
            // again.
            local.settle_log.record(place);
            match local.remove_path(&host, true) {
                Ok(()) => {}
                Err(Errno::ENOENT | Errno::ENOTEMPTY | Errno::EEXIST | Errno::ENOTDIR) => continue,
                Err(errno) => {
                    lore_warn!(
                        "FUSE could not remove the committed '{dir}' from write/: {errno:?}"
                    );
                    continue;
                }
            }
            let made_again = local.arriving.holds(place)
                || self
                    .live_inode(dir)
                    .and_then(|ino| self.inodes().get(ino))
                    .is_some_and(|entry| matches!(entry.kind, InodeKind::Dir { local: true, .. }));
            if made_again
                && let Err(errno) = local.mkdir(&host, 0o755)
                && errno != Errno::EEXIST
            {
                lore_warn!("FUSE could not make '{dir}' again under write/: {errno:?}");
            }
        }
    }

    /// The place of `node`, a directory a settle cleared at `dir`, where it still stands there
    /// live, clean and tracked, its parents leading to the root by the path's names, and the
    /// live inode at the path, if any, stands for it tracked. Served node IDs are never reused.
    fn settled_place(
        &self,
        current: &Served,
        node: NodeID,
        dir: &RelativePath,
    ) -> Option<(NodeID, u64)> {
        let block = current.state.resident_block(NodeBlock::index(node))?;
        let record = block.node(Node::index(node));
        if record.is_discarded()
            || !record.is_directory()
            || record.is_dirty()
            || record.ext_flags().contains(NodeExtFlags::LocalContent)
            || !path_leads_to(&current.state, node, dir)
        {
            return None;
        }
        if let Some(ino) = self.live_inode(dir) {
            let entry = self.inodes().get(ino)?;
            if entry.orphaned
                || entry.node != node
                || !matches!(entry.kind, InodeKind::Dir { local: false, .. })
            {
                return None;
            }
        }
        Some((record.parent, record.name_hash))
    }

    /// The directory the served State holds at `dir` now, found by name from the root past every
    /// index, with its place as that walk found it: the parent it was found below and the
    /// name's hash. `None` where nothing, or no directory, stands there.
    async fn directory_afresh(
        &self,
        current: &Served,
        dir: &RelativePath,
    ) -> Result<Option<FoundDir>, FuseProviderError> {
        let (state, repository) = (&current.state, &current.repository);
        let (mut node, mut place) = (ROOT_NODE, None);
        for name in dir.as_str().split('/').filter(|name| !name.is_empty()) {
            let name_hash = hash_string(name);
            place = Some((node, name_hash));
            node = match state
                .find_subnode(repository.clone(), node, name_hash)
                .await
            {
                Ok(found) => found,
                Err(error) if error.is_node_not_found() => return Ok(None),
                Err(error) => {
                    return Err(error)
                        .forward_any::<FuseProviderError>("finding a settled directory");
                }
            };
        }
        let record = state
            .node(repository.clone(), node)
            .await
            .forward_any::<FuseProviderError>("reading a settled directory")?;
        let place = place.unwrap_or((record.parent, record.name_hash));
        Ok((!record.is_discarded() && record.is_directory())
            .then(|| FoundDir::of(node, &record, place)))
    }

    /// Removes the host directory of `dir`, a directory settled clean, where it is empty: a write
    /// below it makes it again ([`Self::with_host_dirs`]).
    fn remove_host_dir(&self, dir: &RelativePath) {
        let Some(local) = self.local_files() else {
            return;
        };
        let Some(host) = relative_host_path(dir) else {
            return;
        };
        match local.remove_path(&host, true) {
            Ok(()) | Err(Errno::ENOENT | Errno::ENOTEMPTY | Errno::EEXIST) => {}
            Err(errno) => {
                lore_warn!("FUSE could not remove the committed '{dir}' from write/: {errno:?}");
            }
        }
    }

    /// Gives local file `node_id`, at `path` as the State spells it and held at `place`, the
    /// content `target` over its local content, as a forced sync does: its node takes `target`
    /// clean, the inode the kernel holds at the path turns store-served under its write guard,
    /// which a WRITE holds across its host operation, and the host file leaves `write/` under the
    /// same guard. The name is claimed throughout and recorded settled, as a commit's flip of a
    /// file no inode holds is ([`Self::settle_unheld`]), so a lookup that read the file local
    /// before makes no inode of it. Answers whether the node changed; a symbolic link, or a node
    /// moved meanwhile, is left as it stands.
    pub(crate) async fn overwrite_local_file(
        &self,
        current: &Served,
        path: &RelativePath,
        node_id: NodeID,
        place: (NodeID, u64),
        target: &StoredContent,
    ) -> Result<bool, FuseProviderError> {
        let Some(local) = self.local_files() else {
            return Ok(false);
        };
        let block = current
            .state
            .block(current.repository.clone(), NodeBlock::index(node_id))
            .await
            .forward_any::<FuseProviderError>("reading a node a sync overwrites")?;
        let take = |held: &mut NodeContent<'_>| {
            if !held.is_file() || held.ext_flags().contains(NodeExtFlags::Symlink) {
                return false;
            }
            held.set_content(target.address, target.size, target.mode);
            held.clear_dirty_flags()
        };
        let remove_host = || {
            if let Some(host) = relative_host_path(path) {
                match local.remove_path(&host, false) {
                    Ok(()) | Err(Errno::ENOENT) => {}
                    Err(errno) => {
                        lore_warn!(
                            "FUSE could not remove the overwritten '{path}' from write/: {errno:?}"
                        );
                    }
                }
            }
        };
        let _settling = Settling {
            claim: Some(local.settling.claim(place)),
            settled: &local.settled,
        };
        let held = self.live_inode(path);
        let changed = if let Some(mut entry) = held.and_then(|ino| self.inodes().get_mut(ino)) {
            let changed = current
                .state
                .node_change_in_place(&block, node_id, place, take);
            if changed {
                entry.kind = committed_kind(target);
                entry.host_epoch += 1;
                entry.host_file = 0;
                remove_host();
                let ino = entry.ino;
                drop(entry);
                self.fragment_cache().forget(ino);
            }
            changed
        } else {
            let changed = current
                .state
                .node_change_in_place(&block, node_id, place, take);
            if changed {
                remove_host();
            }
            changed
        };
        local.settle_log.record(place);
        Ok(changed)
    }

    /// Removes the host file of `path`, a local file a forced sync detached.
    pub(crate) fn remove_host_file(&self, path: &RelativePath) {
        let (Some(local), Some(host)) = (self.local_files(), relative_host_path(path)) else {
            return;
        };
        match local.remove_path(&host, false) {
            Ok(()) | Err(Errno::ENOENT) => {}
            Err(errno) => {
                lore_warn!("FUSE could not remove the overwritten '{path}' from write/: {errno:?}");
            }
        }
    }

    /// Removes the host directory of `dir`, a tracked directory a swap detached, with every
    /// directory below it. Nothing local stood below it, so it holds directories alone, which
    /// writes below the tracked directory made ([`Self::with_host_dirs`]); a file found below
    /// keeps its directories.
    pub(crate) fn remove_host_tree(&self, dir: &RelativePath) {
        let (Some(local), Some(root)) = (self.local_files(), self.host_file_path(dir)) else {
            return;
        };
        let mut found = vec![(root, dir.clone())];
        let mut at = 0;
        while at < found.len() {
            let (host, path) = found[at].clone();
            at += 1;
            let Ok(entries) = std::fs::read_dir(&host) else {
                continue;
            };
            for entry in entries.flatten() {
                let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
                if let (true, Some(name)) = (is_dir, entry.file_name().to_str()) {
                    found.push((entry.path(), path.join(name)));
                }
            }
        }
        for (_, path) in found.iter().rev() {
            let Some(host) = relative_host_path(path) else {
                continue;
            };
            match local.remove_path(&host, true) {
                Ok(()) | Err(Errno::ENOENT | Errno::ENOTEMPTY | Errno::EEXIST) => {}
                Err(errno) => {
                    lore_warn!(
                        "FUSE could not remove the detached '{path}' from write/: {errno:?}"
                    );
                }
            }
        }
    }

    /// Rewrites the deletion list against `target`, the revision a swap applied: a listed path
    /// stays where `target` holds it and the State does not serve it from `target`.
    pub(crate) async fn rewrite_deletions_for(
        &self,
        current: &Served,
        target: (&State, &Arc<RepositoryContext>),
    ) -> Result<(), FuseProviderError> {
        let listed = self.listed_deletions();
        let mut in_revision = ChildIndex::new(target);
        let mut in_state = ChildIndex::new((&current.state, &current.repository));
        self.rewrite_deletions((&mut in_revision, &mut in_state), listed, Vec::new())
            .await
    }

    /// Gives each of `dirs` and each of their ancestors, deepest first, the address `target`
    /// holds for it, clean, where its children are `target`'s: the same names, none dirty, each
    /// holding `target`'s content, and where it carries the name `before`, the revision served
    /// before the swap, gives its path, or `target` where `before` lacks it. One that differs
    /// keeps its address and flags, so a diff descends it. The clear is the served State's
    /// ([`crate::state::ServedNodes::node_clear`]).
    pub(crate) async fn settle_swapped(
        &self,
        current: &Served,
        (target, before): (StateIn<'_>, StateIn<'_>),
        dirs: Vec<RelativePath>,
    ) {
        let paths = with_ancestors(dirs);
        if paths.is_empty() {
            return;
        }
        let mut pass = Box::new(SwapPass {
            in_revision: ChildIndex::new(target),
            in_before: ChildIndex::new(before),
            in_state: ChildIndex::new((&current.state, &current.repository)),
            scratch: ClearScratch::default(),
        });
        for (_, dir) in &paths {
            if let Err(error) = self
                .settle_swapped_directory(current, target, before, &mut pass, dir)
                .await
            {
                lore_warn!("FUSE settling '{dir}' with a swap failed: {error}");
            }
        }
    }

    /// [`Self::settle_swapped`] of one directory, `dir`.
    async fn settle_swapped_directory(
        &self,
        current: &Served,
        target: StateIn<'_>,
        before: StateIn<'_>,
        pass: &mut SwapPass<'_, '_, '_>,
        dir: &RelativePath,
    ) -> Result<(), FuseProviderError> {
        // What the settle reads of each record, copied out: no record is held across a lookup.
        let (found, own, hash) = match pass.in_state.record(dir).await? {
            Some((node, record)) if record.is_directory() => (
                FoundDir::of(node, &record, (0, 0)),
                (record.parent, record.name_hash),
                record.address.hash,
            ),
            _ => return Ok(()),
        };
        let wanted_id = match pass.in_revision.record(dir).await? {
            Some((wanted_id, wanted))
                if wanted.is_directory() && (found.dirty || wanted.address.hash != hash) =>
            {
                wanted_id
            }
            _ => return Ok(()),
        };
        let Some(place) = indexed_place(&mut pass.in_state, dir, own).await? else {
            return Ok(());
        };
        let against = ClearAgainst {
            state: target.0,
            repository: target.1,
            node: wanted_id,
        };
        // A swap that does not move the revision spells against the target; a directory the
        // revision served before lacks, made locally, carries the target's spelling once it is
        // the target's.
        let spelled = if std::ptr::eq(before.0, target.0) {
            against
        } else {
            match pass.in_before.node(dir).await? {
                Some(node) => ClearAgainst {
                    state: before.0,
                    repository: before.1,
                    node,
                },
                None => against,
            }
        };
        self.settle_swapped_node(
            current,
            FoundDir { place, ..found },
            [against, spelled],
            dir,
            &mut pass.scratch,
        )
        .await
    }

    /// [`Self::settle_swapped_directory`] of directory `found`, against the first of `compared`
    /// and spelled as the second: in a future of its own, so the clear does not sit beside the
    /// record lookups' futures in the caller's.
    async fn settle_swapped_node(
        &self,
        current: &Served,
        mut found: FoundDir,
        compared: [ClearAgainst<'_>; 2],
        dir: &RelativePath,
        scratch: &mut ClearScratch,
    ) -> Result<(), FuseProviderError> {
        let failed = |error: StateError| {
            FuseProviderError::internal(format!("settling a swapped directory: {error}"))
        };
        let nodes = current.state.served();
        // A counter rather than a range: the range's iterator would sit in the future beside it.
        let mut tries = 0;
        while tries <= CHAIN_WALK_RESTARTS {
            tries += 1;
            let clear = ServedClear {
                place: found.place,
                against: compared[0],
                spelled: compared[1],
                scratch: &mut *scratch,
            };
            let node_id = found.node;
            let settling = async {
                if let Some(hook) = self.swap_hook() {
                    hook(SwapPhase::Settle).await;
                }
            };
            match nodes
                .node_clear(&current.repository, node_id, clear, settling)
                .await
                .map_err(failed)?
            {
                ClearOutcome::Kept => return Ok(()),
                ClearOutcome::Cleared => {
                    if node_id != ROOT_NODE && found.local {
                        self.settled_local_directory((node_id, dir), true);
                        if path_leads_to(&current.state, node_id, dir) {
                            self.remove_host_dir(dir);
                        }
                    }
                    return Ok(());
                }
                ClearOutcome::Moved => {
                    let Some(again) = self.directory_afresh(current, dir).await? else {
                        return Ok(());
                    };
                    found = again;
                }
            }
        }
        lore_warn!(
            "FUSE settling '{dir}' after a swap found a directory moving there on every try"
        );
        Ok(())
    }

    /// The deletion list's length, and the paths it holds then; nothing where it cannot be read.
    fn listed_deletions(&self) -> (u64, Vec<RelativePath>) {
        self.read_deletions().unwrap_or_default()
    }

    /// [`Self::listed_deletions`]; `None` where the list could not be read.
    fn read_deletions(&self) -> Option<(u64, Vec<RelativePath>)> {
        let Some(deletions) = self.deletions() else {
            return Some((0, Vec::new()));
        };
        let listed = deletions
            .length()
            .and_then(|covered| Ok((covered, deletions.read()?)));
        listed
            .inspect_err(|error| {
                lore_warn!("FUSE could not read the deletion list after a commit: {error}");
            })
            .ok()
    }

    /// Rewrites the first `covered` bytes of the deletion list, which held `listed`, to the paths
    /// still deleted against the committed revision `in_revision` indexes: those it holds and the
    /// State `in_state` indexes does not serve from it, and `gone`, paths it stored that the
    /// State no longer holds. Lines appended since stay.
    async fn rewrite_deletions(
        &self,
        (in_revision, in_state): (&mut ChildIndex<'_>, &mut ChildIndex<'_>),
        (covered, listed): (u64, Vec<RelativePath>),
        gone: Vec<RelativePath>,
    ) -> Result<(), FuseProviderError> {
        let Some(deletions) = self.deletions() else {
            return Ok(());
        };
        if listed.is_empty() && gone.is_empty() {
            return Ok(());
        }
        let mut kept = Vec::new();
        for path in listed {
            if in_revision.node(&path).await?.is_none() {
                continue;
            }
            let served = in_state.record(&path).await?;
            if served.is_none_or(|(_, node)| node.ext_flags().contains(NodeExtFlags::LocalContent))
            {
                kept.push(path);
            }
        }
        kept.extend(gone);
        kept.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        kept.dedup_by(|a, b| a.as_str() == b.as_str());
        deletions.rewrite(&kept, covered).map_err(|error| {
            FuseProviderError::internal(format!("rewriting the deletion list: {error}"))
        })
    }
}

/// A State with its repository.
type StateIn<'a> = (&'a State, &'a Arc<RepositoryContext>);

/// A directory a settle found, copied out of its record: its node, whether it is dirty and
/// holds local content, and its place as the lookup found it.
#[derive(Clone, Copy)]
struct FoundDir {
    node: NodeID,
    dirty: bool,
    local: bool,
    place: (NodeID, u64),
}

impl FoundDir {
    fn of(node: NodeID, record: &Node, place: (NodeID, u64)) -> Self {
        Self {
            node,
            dirty: record.is_dirty(),
            local: record.ext_flags().contains(NodeExtFlags::LocalContent),
            place,
        }
    }
}

/// A commit settle pass's indexes, its comparison's buffers and what it settled, boxed once per
/// pass, so each directory's settle carries one pointer to them.
struct SettlePass<'p, 'r, 's> {
    in_revision: &'p mut ChildIndex<'r>,
    in_state: &'p mut ChildIndex<'s>,
    scratch: ClearScratch,
    settled: Settled,
}

/// A swap settle pass's indexes and its comparison's buffers, boxed once per pass.
struct SwapPass<'t, 'b, 's> {
    in_revision: ChildIndex<'t>,
    in_before: ChildIndex<'b>,
    in_state: ChildIndex<'s>,
    scratch: ClearScratch,
}

/// What [`FuseFilesystem::settle_directories`] made of the directories it visited.
struct Settled {
    /// The paths the State holds no directory at, which the revision does.
    gone: Vec<RelativePath>,
    /// The directories cleared, node and path, whose empty host directories go once the
    /// deletion list is rewritten.
    cleared: Vec<(NodeID, RelativePath)>,
    /// Whether every settle completed with no error and left no directory it found unvisited.
    clean: bool,
}

/// Whether `path` leads to `node` in `state`: the node and each ancestor, read where its block
/// is in memory, carry the path's name hashes, from the last name up to the root. O(depth), with
/// no walk of a chain; a block not in memory answers false, which keeps a file dirty.
fn path_leads_to(state: &State, node: NodeID, path: &RelativePath) -> bool {
    let mut at = node;
    for name in path.as_str().rsplit('/') {
        let Some(block) = state.resident_block(NodeBlock::index(at)) else {
            return false;
        };
        let record = block.node(Node::index(at));
        if record.is_discarded() || record.name_hash != hash_string(name) {
            return false;
        }
        at = record.parent;
    }
    at == ROOT_NODE
}

/// The place `in_state` resolved directory `dir` at: the parent the index found it below and its
/// name's hash, never the record's own fields, which a move since the index read the parent would
/// make another place's. The root's is `own`, its record's. `None` where the index holds no
/// parent, which a concurrent change can cause.
async fn indexed_place(
    in_state: &mut ChildIndex<'_>,
    dir: &RelativePath,
    own: (NodeID, u64),
) -> Result<Option<(NodeID, u64)>, FuseProviderError> {
    if dir.is_empty() {
        return Ok(Some(own));
    }
    let (parent, name) = dir.as_str().rsplit_once('/').unwrap_or(("", dir.as_str()));
    let Some(parent) = in_state.node_at(parent).await? else {
        return Ok(None);
    };
    Ok(Some((parent, hash_string(name))))
}

/// The children of the directories asked about in one State, by name hash, each directory's
/// read in one walk the first time a path below it is asked for, and each directory found among
/// its parent's, so every directory on the way is walked once.
struct ChildIndex<'a> {
    state: (&'a State, &'a Arc<RepositoryContext>),
    /// By directory path, its children sorted by name hash; `None` for a path the State does not
    /// hold.
    dirs: std::collections::BTreeMap<String, Option<Vec<(u64, NodeID)>>>,
}

impl<'a> ChildIndex<'a> {
    fn new(state: (&'a State, &'a Arc<RepositoryContext>)) -> Self {
        Self {
            state,
            dirs: std::collections::BTreeMap::new(),
        }
    }

    /// The node at `path`, the root for the empty path, found among its directory's children by
    /// name hash, as a path walk finds it.
    async fn node(&mut self, path: &RelativePath) -> Result<Option<NodeID>, FuseProviderError> {
        self.node_at(path.as_str()).await
    }

    /// [`Self::node`] with the node's record, read now.
    async fn record(
        &mut self,
        path: &RelativePath,
    ) -> Result<Option<(NodeID, Node)>, FuseProviderError> {
        let Some(node_id) = self.node(path).await? else {
            return Ok(None);
        };
        let (state, repository) = self.state;
        let node = state
            .node(repository.clone(), node_id)
            .await
            .forward_any::<FuseProviderError>("reading an indexed node")?;
        Ok(Some((node_id, node)))
    }

    /// [`Self::node`] of a path as a string.
    async fn node_at(&mut self, path: &str) -> Result<Option<NodeID>, FuseProviderError> {
        if path.is_empty() {
            return Ok(Some(ROOT_NODE));
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        self.index(parent).await?;
        Ok(self.child(parent, name))
    }

    /// Indexes directory `dir` and each directory above it not indexed yet, from the root down,
    /// each found among its parent's children.
    async fn index(&mut self, dir: &str) -> Result<(), FuseProviderError> {
        if self.dirs.contains_key(dir) {
            return Ok(());
        }
        let mut prefixes = vec![""];
        if !dir.is_empty() {
            prefixes.extend(dir.match_indices('/').map(|(at, _)| &dir[..at]));
            prefixes.push(dir);
        }
        for prefix in prefixes {
            if self.dirs.contains_key(prefix) {
                continue;
            }
            let node = if prefix.is_empty() {
                Some(ROOT_NODE)
            } else {
                let (above, name) = prefix.rsplit_once('/').unwrap_or(("", prefix));
                self.child(above, name)
            };
            let children = match node {
                Some(node) => Some(self.children(node).await?),
                None => None,
            };
            self.dirs.insert(prefix.to_string(), children);
        }
        Ok(())
    }

    /// The child named `name` of indexed directory `parent`.
    fn child(&self, parent: &str, name: &str) -> Option<NodeID> {
        let children = self.dirs.get(parent)?.as_ref()?;
        let name_hash = crate::hash::hash_string(name);
        let at = children.partition_point(|&(hash, _)| hash < name_hash);
        children
            .get(at)
            .filter(|&&(hash, _)| hash == name_hash)
            .map(|&(_, node)| node)
    }

    /// The children of directory node `dir`, sorted by name hash, in one walk.
    async fn children(&self, dir: NodeID) -> Result<Vec<(u64, NodeID)>, FuseProviderError> {
        let (state, repository) = self.state;
        let mut children: Vec<(u64, NodeID)> = state
            .node_children_with_name_hash(repository.clone(), dir)
            .await
            .forward_any::<FuseProviderError>("listing an indexed directory")?
            .into_iter()
            .map(|(node, name_hash)| (name_hash, node))
            .collect();
        children.sort_unstable();
        Ok(children)
    }
}

/// What [`FuseFilesystem::flip_to_tracked`] made of a file a commit stored.
pub(crate) enum Flip {
    Flipped,
    /// Left dirty.
    Kept,
    /// A local file no inode holds, settled after the others ([`FuseFilesystem::settle_unheld`]).
    Unheld(Box<Unheld>),
}

/// A local file no inode holds as a commit flips it: what its read recorded, its node's block,
/// the content the commit stored, and its host file's `stat` before the flip.
pub(crate) struct Unheld {
    served: ServedFile,
    block: Arc<NodeBlock>,
    stored: StoredContent,
    host: libc::stat,
}

/// What [`FuseFilesystem::set_aside`] took off a path.
enum Aside {
    Nothing,
    /// A local file or symlink, moved to `staged`; a file's descriptor pinned for `pinned`.
    Moved {
        staged: PathBuf,
        pinned: Option<u64>,
    },
    /// A host directory of `mode` and access and modification `times`, removed.
    RemovedDir {
        mode: u32,
        times: [libc::timespec; 2],
    },
}

/// The host path of `path`, relative to `write/`; `None` where it does not fit.
fn host_path_of(path: &str) -> Option<HostPath> {
    let mut host = HostPath::new();
    for name in path.rsplit('/') {
        host.prepend(name.as_bytes()).ok()?;
    }
    Some(host)
}

/// Moves the entries of host directory `from` into `to`, merging a directory both hold, and
/// removes `from` once empty. A name `to` already holds otherwise stays in `from`. Blocking.
fn gather_host_dir(from: &Path, to: &Path) {
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };
    for entry in entries.flatten() {
        let source = entry.path();
        let target = to.join(entry.file_name());
        match std::fs::symlink_metadata(&target) {
            Ok(held) if held.is_dir() && entry.file_type().is_ok_and(|kind| kind.is_dir()) => {
                gather_host_dir(&source, &target);
            }
            Ok(_) => lore_warn!(
                "FUSE left '{}' where a rename moved its directory: '{}' is taken",
                source.display(),
                target.display()
            ),
            Err(_) => {
                if let Err(error) = std::fs::rename(&source, &target) {
                    lore_warn!(
                        "FUSE could not move '{}' to '{}' after a rename: {error}",
                        source.display(),
                        target.display()
                    );
                }
            }
        }
    }
    let _ = std::fs::remove_dir(from);
}

/// Drops what [`FuseFilesystem::set_aside`] took, once the change it made room for is made.
fn finish_aside(aside: Aside) {
    if let Aside::Moved { staged, .. } = aside
        && let Err(error) = std::fs::remove_file(&staged)
    {
        lore_warn!("FUSE could not remove '{}': {error}", staged.display());
    }
}

/// The `utimensat` times SETATTR asks for; a time it leaves alone is `UTIME_OMIT`.
fn setattr_times(attr: &SetattrIn) -> [libc::timespec; 2] {
    let time = |now: u32, set: u32, seconds: u64, nanos: u32| {
        if attr.valid & now != 0 {
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_NOW,
            }
        } else if attr.valid & set != 0 {
            libc::timespec {
                tv_sec: seconds as libc::time_t,
                tv_nsec: libc::c_long::from(nanos),
            }
        } else {
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            }
        }
    };
    [
        time(
            setattr_valid::ATIME_NOW,
            setattr_valid::ATIME,
            attr.atime,
            attr.atimensec,
        ),
        time(
            setattr_valid::MTIME_NOW,
            setattr_valid::MTIME,
            attr.mtime,
            attr.mtimensec,
        ),
    ]
}

/// What a SETATTR applies to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SetattrTarget {
    File { local: bool },
    Dir { local: bool },
    Symlink,
}

/// Reads the first `kept` bytes of the `size`-byte content at `address`, inode `ino`'s, into
/// `path` under `context`, fetching what the local store lacks.
async fn read_content(
    context: Option<Arc<dyn std::any::Any + Send + Sync>>,
    repository: Arc<crate::repository::RepositoryContext>,
    ino: u64,
    (address, size, kept): (Address, u64, u64),
    path: &Path,
) -> Result<(), Errno> {
    if kept == 0 {
        return Ok(());
    }
    let options = crate::immutable::read_options_from_repository(&repository);
    let range = if kept < size {
        Some(0..usize::try_from(kept).map_err(|_range| Errno::EFBIG)?)
    } else {
        None
    };
    scoped_in(
        context.as_ref(),
        crate::immutable::read_into_file(repository, address, path, range, options),
    )
    .await
    .map(drop)
    .map_err(|error| {
        lore_warn!("FUSE copy-up of inode {ino} failed: {error}");
        Errno::EIO
    })
}

/// What a step answers where it needs a listing it did not find: [`ReadFault::WouldFetch`]
/// without `fetch`, else `EIO`, as a build that returned nothing.
pub(crate) fn would_fetch(fetch: bool) -> ReadFault {
    if fetch {
        Errno::EIO.into()
    } else {
        ReadFault::WouldFetch
    }
}

/// The errno a write request's State step answers for `error`: `ENOENT` where a node on the
/// way was detached meanwhile, as by a sync, else `EIO`, logged.
fn state_errno(request: &str, name: &str, error: &StateError) -> Errno {
    if error.is_node_not_found() {
        return Errno::ENOENT;
    }
    lore_warn!("FUSE {request} '{name}' failed: {error}");
    Errno::EIO
}

/// `name` as a new entry's name: at most 255 bytes, UTF-8, representable in a listing, and
/// storable in the State (`validate_node_name_for_store`), so a refused name reaches nothing.
fn new_name(name: &[u8]) -> Result<&str, Errno> {
    if name.len() > 255 {
        return Err(Errno::ENAMETOOLONG);
    }
    if !is_representable(name) {
        return Err(Errno::EINVAL);
    }
    let name = std::str::from_utf8(name).map_err(|_invalid| Errno::EINVAL)?;
    validate_node_name_for_store(name).map_err(|_invalid| Errno::EINVAL)?;
    Ok(name)
}

/// `fstat` of an open host file.
pub fn file_stat(file: &File) -> Result<libc::stat, Errno> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `file` is open and `stat` a valid out-pointer.
    check(unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) })?;
    // SAFETY: `fstat` succeeded, so it filled `stat`.
    Ok(unsafe { stat.assume_init() })
}

fn opened_read_only(file: &File) -> bool {
    // SAFETY: `file` is open.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    flags >= 0 && flags & libc::O_ACCMODE == libc::O_RDONLY
}

fn set_file_mode(file: &File, mode: u32) -> Result<(), Errno> {
    // SAFETY: `file` is open.
    check(unsafe { libc::fchmod(file.as_raw_fd(), mode & 0o7777) })
}

fn set_file_times(file: &File, times: &[libc::timespec; 2]) -> Result<(), Errno> {
    // SAFETY: `file` is open and `times` holds two timespecs.
    check(unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) })
}

fn allocate(file: &File, mode: u32, offset: u64, length: u64) -> Result<(), Errno> {
    let offset = libc::off_t::try_from(offset).map_err(|_range| Errno::EFBIG)?;
    let length = libc::off_t::try_from(length).map_err(|_range| Errno::EFBIG)?;
    // SAFETY: `file` is open.
    check(unsafe { libc::fallocate(file.as_raw_fd(), mode as i32, offset, length) })
}

fn io_errno(error: &std::io::Error) -> Errno {
    Errno(error.raw_os_error().unwrap_or(libc::EIO))
}

fn last_errno() -> Errno {
    io_errno(&std::io::Error::last_os_error())
}

fn check(result: libc::c_int) -> Result<(), Errno> {
    if result < 0 {
        Err(last_errno())
    } else {
        Ok(())
    }
}

fn open_failed(action: &str, path: &Path, error: &std::io::Error) -> FuseProviderError {
    FuseProviderError::internal(format!("{action} {}: {error}", path.display()))
}

/// The kind of a store-served inode of the file a commit stored as `stored`.
fn committed_kind(stored: &StoredContent) -> InodeKind {
    InodeKind::File {
        address: stored.address,
        size: stored.size,
        executable: stored.mode & NodeFileMode::Executable == NodeFileMode::Executable,
        local: false,
    }
}

/// Gives file node `held`, holding local content where `local`, the content a commit stored as
/// `stored`, clean; false, changing nothing, for a node of another kind or a symbolic link.
fn take_committed(held: &mut NodeContent<'_>, stored: &StoredContent, local: bool) -> bool {
    let ext = held.ext_flags();
    if !held.is_file()
        || ext.contains(NodeExtFlags::Symlink)
        || ext.contains(NodeExtFlags::LocalContent) != local
    {
        return false;
    }
    held.set_content(stored.address, stored.size, stored.mode);
    held.clear_dirty_flags()
}

/// Drops `held`'s add action where it holds one, a file turning modified: the committed revision
/// holds its path. Run under its block's lock; whether it changed the record.
fn no_longer_added(held: &mut NodeContent<'_>) -> bool {
    if !held.is_dirty_add() {
        return false;
    }
    let action = if held.is_file() {
        NodeFlags::DirtyModify
    } else {
        NodeFlags::NoFlags
    };
    held.replace_dirty_action(action)
}

/// Each of `dirs` and each of their ancestors up to the root, once each, deepest first, with its
/// depth. Views of the paths handed, so an ancestor costs no allocation. The climb and the sort
/// run over names borrowed from the paths handed, each output view cut from its path after.
#[lore_macro::test_pub]
fn with_ancestors(dirs: Vec<RelativePath>) -> Vec<(usize, RelativePath)> {
    // Each path taken: its depth, its name, the path handed it is cut from, and the names to cut.
    let mut keys: Vec<(usize, &str, u32, u32)> = Vec::with_capacity(dirs.len() + 1);
    // The last path taken at each depth, where a climb stops: the paths below a directory mostly
    // follow one another, and the repeats left are deduplicated after the sort.
    let mut taken: Vec<Option<&str>> = Vec::new();
    for (at, dir) in dirs.iter().enumerate() {
        let mut path = dir.as_str();
        // Clean paths: a separator between each two names.
        let mut depth =
            path.bytes().filter(|&byte| byte == b'/').count() + usize::from(!path.is_empty());
        let mut cut = 0;
        loop {
            if taken.len() <= depth {
                taken.resize(depth + 1, None);
            }
            if taken[depth] == Some(path) {
                break;
            }
            taken[depth] = Some(path);
            keys.push((depth, path, at as u32, cut));
            if depth == 0 {
                break;
            }
            path = path.rsplit_once('/').map_or("", |(parent, _)| parent);
            depth -= 1;
            cut += 1;
        }
    }
    keys.sort_unstable_by(|(a_depth, a, _, _), (b_depth, b, _, _)| {
        b_depth.cmp(a_depth).then_with(|| a.cmp(b))
    });
    keys.dedup_by(|(_, a, _, _), (_, b, _, _)| a == b);
    keys.into_iter()
        .map(|(depth, _, at, cut)| {
            let mut view = dirs[at as usize].clone();
            for _ in 0..cut {
                view.pop_name();
            }
            (depth, view)
        })
        .collect()
}

/// `path` as a path below `write/`; `None` where it is too long for one.
fn relative_host_path(path: &RelativePath) -> Option<HostPath> {
    let mut host = HostPath::new();
    path.as_str()
        .rsplit('/')
        .try_for_each(|name| host.prepend(name.as_bytes()))
        .ok()?;
    Some(host)
}

/// The modification time `stat` holds, in milliseconds.
fn stat_millis(stat: &libc::stat) -> u64 {
    u64::try_from(stat.st_mtime)
        .unwrap_or_default()
        .saturating_mul(1000)
        .saturating_add(u64::try_from(stat.st_mtime_nsec).unwrap_or_default() / 1_000_000)
}

fn flip_failed(path: &RelativePath, errno: Errno) -> FuseProviderError {
    FuseProviderError::internal(format!(
        "flipping '{path}' to the committed revision: {}",
        std::io::Error::from_raw_os_error(errno.0)
    ))
}
