// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The provider's `lore_fuse::Filesystem`: every request reads the filesystem State through
//! the current [`Served`], its node-keyed listings, and the inode table. Hot requests on warm
//! state allocate nothing; inode creation and listing builds may.

use std::any::Any;
use std::fmt::Write as _;
use std::future::Future;
use std::mem::ManuallyDrop;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use dashmap::DashMap;
use dashmap::DashSet;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::text::fold_eq;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_error_set::prelude::*;
use lore_fuse::abi::Attr;
use lore_fuse::abi::DirentType;
use lore_fuse::abi::EntryOut;
use lore_fuse::abi::Kstatfs;
use lore_fuse::abi::SetattrIn;
use lore_fuse::abi::file::open_flags;
use lore_fuse::codec::dirent::DirentWriter;
use lore_fuse::filesystem::Fetch;
use lore_fuse::filesystem::Filesystem;
use lore_fuse::filesystem::RequestInfo;
use lore_fuse::reply::AttrReply;
use lore_fuse::reply::CreateReply;
use lore_fuse::reply::EntryReply;
use lore_fuse::reply::Errno;
use lore_fuse::reply::OpenReply;
use lore_fuse::reply::ReadFault;
use lore_fuse::reply::ReplyOutcome;
use lore_fuse::reply::Timeouts;
use lore_fuse::transport::Notifier;
use lore_storage::ContentSource;
use lore_storage::local::store_lock::StoreHold;
use smallvec::SmallVec;

use crate::filter::FilterStates;
use crate::fs::fuse::FuseProviderError;
use crate::fs::fuse::access_recorder::AccessRecorder;
use crate::fs::fuse::attr;
use crate::fs::fuse::attr::AttrContext;
use crate::fs::fuse::attr::RecordedTimes;
use crate::fs::fuse::attr::attr_for;
use crate::fs::fuse::deletions::DELETION_LIST;
use crate::fs::fuse::deletions::DeletionList;
use crate::fs::fuse::fragment_cache::DEFAULT_FRAGMENT_CACHE;
use crate::fs::fuse::fragment_cache::FragmentCache;
use crate::fs::fuse::inode::ChildLookup;
use crate::fs::fuse::inode::InodeEntry;
use crate::fs::fuse::inode::InodeKind;
use crate::fs::fuse::inode::InodeTable;
use crate::fs::fuse::inode::Name;
use crate::fs::fuse::inode::NewChild;
use crate::fs::fuse::inode::ROOT_INO;
use crate::fs::fuse::inode::claim_once;
use crate::fs::fuse::inode_number::Identity;
use crate::fs::fuse::inode_number::IdentityKind;
use crate::fs::fuse::inode_number::derive_inode_number;
use crate::fs::fuse::listing::Hit;
use crate::fs::fuse::listing::Listing;
use crate::fs::fuse::listing::ListingEntry;
use crate::fs::fuse::listing::NO_DIR;
use crate::fs::fuse::listing::entry_flags;
use crate::fs::fuse::listing::holds_local;
use crate::fs::fuse::listing::is_representable;
use crate::fs::fuse::listing_cache::LISTING_BLOCK_BYTES;
use crate::fs::fuse::listing_cache::LISTING_CACHE_BYTES;
use crate::fs::fuse::listing_cache::ListingCache;
use crate::fs::fuse::local_tree::LocalEntry;
use crate::fs::fuse::local_tree::LocalKind;
use crate::fs::fuse::local_tree::LocalTree;
use crate::fs::fuse::notifications::NotificationQueue;
use crate::fs::fuse::prefetch::PrefetchTracker;
use crate::fs::fuse::read::FETCH_DEADLINE;
use crate::fs::fuse::read::RecentMisses;
use crate::fs::fuse::read::note_read;
use crate::fs::fuse::read::read_file;
use crate::fs::fuse::read::readahead_wanted;
use crate::fs::fuse::read::start_readahead;
use crate::fs::fuse::served::Served;
use crate::fs::fuse::served::ServedCell;
use crate::fs::fuse::spelling::DEFAULT_TIMEOUTS;
use crate::fs::fuse::spelling::SpellingRecord;
use crate::fs::fuse::swap::NotificationSink;
use crate::fs::fuse::swap::SwapPlan;
use crate::fs::fuse::view::child_filter_in;
use crate::fs::fuse::view::filter_states_from_packed;
use crate::fs::fuse::view::filter_states_to_u8;
use crate::fs::fuse::view::view_line;
use crate::fs::fuse::write::LocalFiles;
use crate::fs::fuse::write::would_fetch;
use crate::fs::hold::SwapHook;
use crate::global::external_dir::WRITE_DIR;
use crate::hash::hash_string;
use crate::lore_warn;
use crate::node::BLOCK_NODE_COUNT;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeExtFlags;
use crate::node::NodeFileMode;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeNameError;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::repository::SharedRemote;
use crate::state::Content;
use crate::state::Detach;
use crate::state::Ext;
use crate::state::Mark;
use crate::state::NameChange;
use crate::state::Rewrite;
use crate::state::State;
use crate::state::StateError;
use crate::state::StateNodeChildrenIterator;
use crate::state::Tree;
use crate::util::path::RelativePath;

/// Deeper chains than this are treated as a corrupt inode table rather than walked.
pub(crate) const MAX_DEPTH: usize = 4096;

const DIR_MODE: u32 = libc::S_IFDIR | 0o555;

/// Most times a request starts again that found a local file's host file absent while a
/// commit's flip held it aside ([`FuseFilesystem::settled_at`]).
const SETTLE_ATTEMPTS: u32 = 8;

/// How often one request rebuilds a listing a burst of changes keeps making stale.
pub(crate) const STALE_REBUILDS: u32 = 4;

/// One OPENDIR stream. Its first READDIR (and any rewind to offset 0) pins the current
/// listing, and later offsets index that pin, so neither a swap nor a write shifts a stream's
/// offsets.
struct DirHandle {
    ino: u64,
    view: Option<StreamView>,
}

/// A directory stream's view: the listing its first READDIR read, with the swap count and the
/// settle log's epoch read before it. Positions past `.` and `..` index the listing.
#[derive(Clone)]
struct StreamView {
    listing: Arc<Listing>,
    swaps: u64,
    settles: u64,
}

/// What a request read before it resolved a name: the swap count, and the settle log's epoch
/// ([`FuseFilesystem::settle_epoch`]). An inode made from what it read takes content only while
/// no swap began since and no commit settled the name since.
#[derive(Clone, Copy)]
pub(crate) struct Pinned {
    pub swaps: u64,
    pub settles: u64,
}

/// What one position of a [`StreamView`] holds.
enum Slot {
    /// The listing's entry at this index.
    Listed(usize),
    /// A view-excluded entry, which READDIR does not report.
    Hidden,
    End,
}

impl StreamView {
    fn slot(&self, position: u64) -> Slot {
        match position
            .checked_sub(2)
            .and_then(|index| usize::try_from(index).ok())
        {
            Some(index) if index < self.listing.len() => {
                if self.listing.entry(index).flags & entry_flags::VIEW_EXCLUDED != 0 {
                    Slot::Hidden
                } else {
                    Slot::Listed(index)
                }
            }
            _ => Slot::End,
        }
    }
}

/// How OPEN is answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// `ENOSYS`: the kernel stops sending OPEN for the connection and keeps its page cache.
    ZeroMessage,
    /// `FOPEN_KEEP_CACHE | FOPEN_NOFLUSH` with handle 0.
    KeepCache,
}

pub struct FuseFilesystem {
    served: ServedCell,
    /// The claims on the instance's stores, held for as long as this provider serves them and
    /// shared with every context it serves through ([`provider_context`]); `None` where the stores
    /// have nothing on disk to claim.
    stores: Option<Arc<StoreHold>>,
    /// The remote every context this provider serves through shares, connected on first use
    /// whatever the command that built or last published it was told
    /// ([`RepositoryContext::serving_remote`]).
    remote: SharedRemote,
    inodes: InodeTable,
    spellings: SpellingRecord,
    recorded: RecordedTimes,
    attr: AttrContext,
    notifier: OnceLock<Notifier>,
    /// The thread swaps and instance operations send their invalidations from.
    notifications: OnceLock<NotificationQueue>,
    /// Set at mount construction when `LORE_FUSE_RECORD_DIR` is set.
    access_recorder: OnceLock<AccessRecorder>,
    read_deadline_nanos: AtomicU64,
    recent_misses: RecentMisses,
    /// Whether READDIRPLUS lists a directory as READDIR would until one of its files is looked
    /// up; off by default.
    plain_first: AtomicBool,
    /// Directories a file of which was looked up while `plain_first` was on.
    advised: DashSet<u64>,
    statfs: parking_lot::RwLock<Kstatfs>,
    dir_handles: DashMap<u64, DirHandle>,
    next_dir_handle: AtomicU64,
    open_mode: OpenMode,
    /// The execution context current at construction. Listing builds emit filter events and
    /// reads resolve a storage session, both of which need one; requests arriving on tasks
    /// without it run under this.
    context: Option<Arc<dyn Any + Send + Sync>>,
    readaheads: AtomicU64,
    fragment_cache: Arc<FragmentCache>,
    /// Serializes swaps and holds the scratch each one reuses.
    swap_plan: tokio::sync::Mutex<SwapPlan>,
    /// The host files under `write/`; `None` on a read-only provider, which refuses writes
    /// with `EROFS`.
    local: Option<LocalFiles>,
    /// The deletion list of a writable provider, kept open for appending.
    deletions: Option<DeletionList>,
    /// Background warm-ups a clone started.
    pub(super) prefetch: PrefetchTracker,
    /// Listing builds in flight by directory node.
    listing_builds: DashMap<NodeID, Arc<ListingBuild>>,
    listings_built: AtomicU64,
    /// Whether a swap has published a revision; until one does, a swap onto an empty mount
    /// installs its State whole.
    published: AtomicBool,
    /// Twice the number of swaps begun, plus one while a swap applies: an inode made from what a
    /// request read while it moved takes no content, and a READDIRPLUS stream read across it
    /// lists plain.
    swaps: AtomicU64,
    /// A test's hook into every swap's apply.
    swap_hook: OnceLock<SwapHook>,
    /// A test's hook into the State step of every copy-up and rename.
    write_hook: OnceLock<WriteHook>,
    /// A test's hook run after every listing build, before the listing is cached.
    listing_hook: OnceLock<ListingHook>,
    /// A test's hook run in every listing build once its log mark is read, before its walk.
    walk_hook: OnceLock<ListingHook>,
    /// A test's hook run once a copy-up's content is staged, before the copy is kept.
    staging_hook: OnceLock<StagingHook>,
    /// A test's hook run before an emptiness check reads a child's record.
    emptiness_hook: OnceLock<ListingHook>,
    /// A test's hook run once a commit has read the files it stores, before they are flipped.
    commit_hook: OnceLock<CommitHook>,
    /// A test's hook into a LOOKUP of a node attached since its listing.
    lookup_hook: OnceLock<LookupHook>,
    /// Files commits flipped to the revision, and files they left dirty.
    commit_outcomes: [AtomicU64; 2],
    /// Operations that have served a file's content and not yet ended
    /// ([`FuseFilesystem::commit_reading`]).
    commits_reading: AtomicUsize,
    /// New inodes refused because their name was settled since the request read it, refused
    /// because a swap began since, and dropped because a local file's host file was absent
    /// ([`FuseFilesystem::lookup_refusals`]).
    lookup_refusals: [AtomicU64; 3],
}

/// A step of a write request where a test's [`WriteHook`] runs: before a copy-up's or a
/// rename's State change and after its host change, and inside the host operation of a write to
/// a local file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteStep {
    CopyUp,
    Rename,
    /// A WRITE, SETATTR or FALLOCATE of a local file that has found the file local and taken
    /// its descriptor, before the bracket around the host operation.
    BeforeHostWrite,
    /// The host operation of a WRITE, SETATTR or FALLOCATE of a local file, with the inode's
    /// write generation as it reads while the operation runs.
    HostWrite {
        generation: u64,
    },
    /// A CREATE, MKNOD, MKDIR or SYMLINK whose node is attached and whose inode is not made yet.
    /// The hook's answer is not read: the step cannot fail.
    Created,
    /// A RENAME whose node has moved to the target in directory inode `dir` and whose inode has
    /// not, of a regular file where `file`. The hook's answer is not read.
    NodeMoved {
        dir: u64,
        file: bool,
    },
    /// A RENAME over an inode the kernel held at the target in directory inode `dir`, once that
    /// inode has left the name. The hook's answer is not read.
    TargetUnindexed {
        dir: u64,
    },
    /// A RENAME onto the name of inode `ino`, about to wait for the inode's copy-up gate. The
    /// hook's answer is not read.
    TargetGate {
        ino: u64,
    },
    /// A commit's flip of file `node`, which no inode held at its read, its name claimed: before
    /// its host file is taken aside, and with it aside (`aside`). The hook's answer is not read.
    Settle {
        node: NodeID,
        aside: bool,
    },
    /// A commit about to reconcile the State with the revision it stored; `false` leaves the
    /// State unreconciled, as a commit whose service stops first does.
    Reconcile,
    /// A commit's flip once it has classified the files it stored, before it settles those no
    /// inode held. The hook's answer is not read.
    Classified,
    /// A request about to make a new inode of node `node`, from what it read before. The hook's
    /// answer is not read.
    Admitting {
        node: NodeID,
    },
    /// The host file of inode `ino` opened by its path for its descriptor, before it is cached.
    /// The hook's answer is not read.
    DescriptorOpened {
        ino: u64,
    },
    /// Inode `ino` gone from the table at a FORGET, before its fragment cache state goes and its
    /// host file closes. The hook's answer is not read.
    Forgotten {
        ino: u64,
    },
    /// An instance write that found nothing at the name it writes in directory inode `parent`,
    /// before it creates the file. The hook's answer is not read.
    InstanceFoundNothing {
        parent: u64,
    },
    /// A create's host path in directory inode `parent` built from the inode names, before its
    /// host entry is made. The hook's answer is not read.
    HostPathBuilt {
        parent: u64,
    },
    /// A create's host entry in directory inode `parent` made, before the renames above it are
    /// read again. The hook's answer is not read.
    HostEntryMade {
        parent: u64,
    },
    /// An instance write whose create of a directory in directory inode `parent` found the name
    /// taken while a create or rename holds the name's claim, before it waits for one release of
    /// it. The hook's answer is not read.
    AwaitingCreate {
        parent: u64,
    },
    /// A directory RENAME that has respelled the inode and gathered what stood under the old host
    /// path, before it stops counting as under way. The hook's answer is not read.
    Gathered,
    /// The host path of directory inode `dir` built from the inode names to make the directory
    /// where missing, before the mkdir. The hook's answer is not read.
    HostDirBuilt {
        dir: u64,
    },
    /// A RENAME's host path at its target in directory inode `dir` built from the inode names,
    /// before the host rename. The hook's answer is not read.
    RenameTargetBuilt {
        dir: u64,
    },
    /// A copy-up's host path in directory inode `parent` built from the inode names, before the
    /// staged file moves to it. The hook's answer is not read.
    CopyUpPathBuilt {
        parent: u64,
    },
    /// A CREATE, MKNOD, MKDIR or SYMLINK that has marked directory node `dir` dirty, before it
    /// attaches its node. The hook's answer is not read.
    Attaching {
        dir: NodeID,
    },
    /// An MKDIR that has read the swap count and built the empty listing of directory node
    /// `dir`, which it made, before it caches the listing. The hook's answer is not read.
    EmptyListingBuilt {
        dir: NodeID,
    },
    /// A commit settling directory node `node`, found with no dirty child, before it sets the
    /// directory clean. The hook's answer is not read.
    DirectorySettling {
        node: NodeID,
    },
    /// A commit's settle that found directory `node` afresh at the path it settles, the directory
    /// its last try found there having moved, before it tries `node`. The hook's answer is not
    /// read.
    SettleFound {
        node: NodeID,
    },
    /// A commit that has settled its directories, before it rewrites the deletion list and
    /// removes the settled directories' host directories; `false` stops the reconcile there, as
    /// a commit cancelled there does.
    DirectoriesSettled,
    /// An operation about to serve the content of a file, as a commit reads it. The hook's
    /// answer is not read.
    ContentServing,
    /// An UNLINK or RMDIR about to detach node `node` from the State. The hook's answer is not
    /// read.
    Detaching {
        node: NodeID,
    },
    /// An UNLINK or RMDIR that has detached node `node`, before it decides whether to record the
    /// deletion. The hook's answer is not read.
    Detached {
        node: NodeID,
    },
    /// The host path of inode `ino` built for its descriptor from the inode entries, before the
    /// file at it is opened. The hook's answer is not read.
    PathResolved {
        ino: u64,
    },
    /// A commit's flip of file `node`, which no inode held at its read, past its check of the
    /// writes forgotten since, before it looks for the inode now at the name. The hook's answer
    /// is not read.
    FlipChecked {
        node: NodeID,
    },
}

/// An operation's serving of file content, which a commit stores
/// ([`FuseFilesystem::begin_commit_read`]), counted until dropped.
pub(crate) struct CommitReading(Arc<FuseFilesystem>);

/// A commit's settle owed from before its revision may become current (H20): while it stands,
/// [`FuseFilesystem::commit_reading`] answers true, so a removal or rename away of an added node
/// records its deletion, the node possibly committed. Only a settle that completed with no error
/// releases it ([`Self::release`]); dropped otherwise, as by a cancelled or failed commit, it
/// leaves the count raised for the rest of the mount, so removals keep recording.
#[lore_macro::test_pub]
pub(crate) struct SettleOwed {
    filesystem: Arc<FuseFilesystem>,
    /// The directories the commit added or moved, which its settle visits.
    pub(crate) handed: Vec<RelativePath>,
    /// A pre-record failed: the settle is owed for good.
    sticky: bool,
}

impl SettleOwed {
    /// Raises the count, ahead of any lookup the arming makes, for `handed`.
    pub(crate) fn arm(filesystem: Arc<FuseFilesystem>, handed: Vec<RelativePath>) -> Self {
        filesystem.commits_reading.fetch_add(1, Ordering::SeqCst);
        Self {
            filesystem,
            handed,
            sticky: false,
        }
    }

    /// Leaves the count raised however the guard ends.
    pub(crate) fn stick(&mut self) {
        self.sticky = true;
    }

    /// Ends the debt: the commit's revision never became current, or its settle completed with
    /// no error. A sticky guard keeps its count.
    #[lore_macro::test_pub]
    pub(crate) fn release(self) {
        if !self.sticky {
            self.filesystem
                .commits_reading
                .fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Drop for CommitReading {
    fn drop(&mut self) {
        self.0.commits_reading.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A test's hook into a write request; `false` fails the step as a State error would.
pub type WriteHook = Arc<dyn Fn(WriteStep) -> bool + Send + Sync>;

/// A test's hook run after the build of a listing of the directory it is given, as a change
/// racing the build would.
pub type ListingHook = Arc<dyn Fn(NodeID) -> crate::BoxFuture<'static, ()> + Send + Sync>;

/// A test's hook run once a commit has read the files it stores and before it flips them, as a
/// request racing the commit would.
pub type CommitHook = Arc<dyn Fn() -> crate::BoxFuture<'static, ()> + Send + Sync>;

/// Where a LOOKUP of a node attached since its directory's listing runs a test's hook, as a
/// change racing the lookup would; no lock is held at either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookupStep {
    /// The listing named `node` for the name; its record is read next.
    Added { dir: NodeID, node: NodeID },
    /// The listing confirmed its entry `node` for the name from its record; its inode is found
    /// next.
    Listed { dir: NodeID, node: NodeID },
    /// No inode holds the name; the node's name is read again to make one.
    Missing { dir: NodeID, node: NodeID },
    /// The name has no inode, on a first miss, or its inode stands for another node or kind,
    /// while a request moving a node to the name is under way; the lookup waits for one release
    /// of a claim on the name, then looks again.
    Arriving { dir: NodeID, node: NodeID },
    /// A new inode of node `node` stands at the name, its insert checked; its host file is read
    /// next.
    Inserted { node: NodeID },
}

/// What a lookup does where a request moving a node to its name is under way
/// ([`FuseFilesystem::child_matching`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OnArriving {
    /// Waits for one release of a claim on the name, as a request that may fetch does.
    Wait,
    /// Answers [`ReadFault::WouldFetch`]: a worker never waits, and the request runs again in a
    /// deferred task.
    Defer,
    /// Makes no inode, so READDIRPLUS pushes the row plain.
    Plain,
}

impl OnArriving {
    /// What a lookup that did not wait answers: [`ReadFault::WouldFetch`] where it defers, else
    /// nothing.
    fn unwaited(self) -> Result<(), ReadFault> {
        match self {
            OnArriving::Defer => Err(ReadFault::WouldFetch),
            OnArriving::Wait | OnArriving::Plain => Ok(()),
        }
    }
}

/// What a lookup does with a listing that no longer describes its directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OnStale {
    /// Rebuilds it, as LOOKUP does.
    Rebuild,
    /// Answers the one name from the State, as a write request does, leaving the build to the
    /// next LOOKUP or READDIR.
    ByName,
}

/// Which spellings of a name a lookup answers. The State holds one spelling per folded name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LookupCase {
    /// Only the spelling the State holds, as the kernel's LOOKUP: another spelling answers
    /// absent, as on a case-sensitive filesystem. Were it to answer with the same inode, the
    /// kernel would take a rename between two spellings for a rename onto itself, a no-op it
    /// never sends.
    Exact,
    /// Any spelling that folds to it, as a request on lore's behalf resolves a path.
    Folded,
}

/// A test's hook into a LOOKUP ([`LookupStep`]).
pub type LookupHook = Arc<dyn Fn(LookupStep) -> crate::BoxFuture<'static, ()> + Send + Sync>;

/// A test's hook run once a copy-up's content is staged at the path it is given, as a host
/// write still landing there would.
pub type StagingHook =
    Arc<dyn Fn(std::path::PathBuf) -> crate::BoxFuture<'static, ()> + Send + Sync>;

/// What a writable provider holds beyond a read-only one.
struct Writable {
    local: LocalFiles,
    deletions: DeletionList,
    /// The filesystem State built from the revision, the deletion list and `write/`.
    state: Arc<State>,
}

/// A listing build that concurrent requests for one directory share: the listing, or the
/// build's error.
type ListingBuild = tokio::sync::OnceCell<Result<Arc<Listing>, Errno>>;

/// One caller's share of a listing build: the build in flight for its key, joined or started.
/// The build leaves the map as it completes, so only builds in flight are shared. Dropped,
/// whether the build completed or its caller was cancelled, it removes the build once no caller
/// holds it.
struct BuildShare<'a> {
    builds: &'a DashMap<NodeID, Arc<ListingBuild>>,
    key: NodeID,
    build: ManuallyDrop<Arc<ListingBuild>>,
}

impl<'a> BuildShare<'a> {
    fn join(builds: &'a DashMap<NodeID, Arc<ListingBuild>>, key: NodeID) -> Self {
        // The entry guard lives for this statement only, inserting at most an empty cell.
        #[allow(clippy::disallowed_methods)]
        let build = builds.entry(key).or_default().clone();
        Self {
            builds,
            key,
            build: ManuallyDrop::new(build),
        }
    }
}

impl Drop for BuildShare<'_> {
    fn drop(&mut self) {
        // This share's reference goes first, so the last share to leave finds the map's alone.
        // SAFETY: `build` is dropped here only, and never used after.
        unsafe { ManuallyDrop::drop(&mut self.build) };
        self.builds
            .remove_if(&self.key, |_, build| Arc::strong_count(build) == 1);
    }
}

/// The claims a provider holds on `repository`'s stores for as long as it serves them, `None`
/// where they have nothing on disk to claim. The kernel's requests arrive outside any command, so
/// without them each store operation would claim the store for itself, and the store would close
/// its packfiles and reload whenever the claims of the operations in flight ran out.
async fn claim_provider_stores(
    repository: &RepositoryContext,
) -> Result<Option<Arc<StoreHold>>, FuseProviderError> {
    let stores = repository
        .hold_stores()
        .await
        .forward_any::<FuseProviderError>("claiming the instance's stores")?;
    Ok((!stores.is_empty()).then(|| Arc::new(stores)))
}

/// The context every provider read goes through: without command locks, holding the provider's
/// claims on its stores, so work done through it keeps them until it ends, over the provider's
/// own remote, so content stored on the remote alone stays readable after a command run offline
/// built or published it, and caching whatever it fetches whatever `repository` says, so content
/// once served stays readable offline.
fn provider_context(
    repository: &RepositoryContext,
    stores: Option<&Arc<StoreHold>>,
    remote: &SharedRemote,
) -> Arc<RepositoryContext> {
    let provider = repository
        .without_command_locks()
        .with_shared_remote(remote)
        .with_shared_store_hold(stores.cloned());
    provider.set_disable_cache(false);
    Arc::new(provider)
}

/// The provider error for `action` on `path` failing with `error`.
fn mount_failed(action: &str, path: &Path, error: &std::io::Error) -> FuseProviderError {
    FuseProviderError::internal(format!("{action} {}: {error}", path.display()))
}

/// A State of `revision` that this provider alone changes: loaded afresh, unless it names no
/// stored revision, in which case it is served as it is.
async fn own_state(
    repository: &Arc<RepositoryContext>,
    revision: Arc<State>,
) -> Result<Arc<State>, FuseProviderError> {
    if revision.revision().is_zero() {
        return Ok(revision);
    }
    load_revision(repository, revision.revision()).await
}

/// A fresh State of `revision`, which nothing else changes; an empty one for the zero revision.
pub(crate) async fn load_revision(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
) -> Result<Arc<State>, FuseProviderError> {
    fresh_state(repository, revision)
        .await
        .forward_any::<FuseProviderError>("loading a revision")
}

/// The State of `revision`, an empty one for the zero revision, taking new nodes from fresh
/// blocks: it is never serialized, so the revision's free slots would only cost a stored block,
/// its name table and its file metadata to reuse, and an attach never reads the store.
async fn fresh_state(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
) -> Result<Arc<State>, StateError> {
    let state = if revision.is_zero() {
        State::new()
    } else {
        State::deserialize(repository.clone(), revision).await?
    };
    state
        .take_new_nodes_from_fresh_blocks(repository.clone())
        .await?;
    Ok(state)
}

/// What a writable provider reads from its external directory at mount: the deletion list,
/// kept open, the paths it holds, and what `write/` holds. Blocking.
fn read_local_changes(
    external_dir: &Path,
) -> Result<(DeletionList, Vec<RelativePath>, LocalTree), FuseProviderError> {
    std::fs::create_dir_all(external_dir)
        .map_err(|error| mount_failed("creating", external_dir, &error))?;
    let list = external_dir.join(DELETION_LIST);
    let deletions =
        DeletionList::open(&list).map_err(|error| mount_failed("opening", &list, &error))?;
    let deleted = deletions
        .read()
        .map_err(|error| mount_failed("reading", &list, &error))?;
    let write = external_dir.join(WRITE_DIR);
    let local = LocalTree::scan(&write).map_err(|error| mount_failed("reading", &write, &error))?;
    Ok((deletions, deleted, local))
}

/// `revision` loaded afresh under `repository`, with each path of `deleted` it holds detached
/// and every entry of `local` added, as [`FuseFilesystem::new_writable`] describes. Each
/// directory holding local entries has its children read once, into an index by name hash, so
/// a directory of `n` entries costs `O(n log n)`; nothing else is read, so a State with no
/// local changes loads no block.
async fn local_state(
    repository: &Arc<RepositoryContext>,
    revision: &State,
    deleted: Vec<RelativePath>,
    local: &LocalTree,
) -> Result<Arc<State>, StateError> {
    let state = fresh_state(repository, revision.revision()).await?;
    apply_deletions(&state, repository, deleted, local).await?;
    let mut held: Vec<(u64, NodeID, Node)> = Vec::new();
    let mut pending = vec![(local.top(), ROOT_NODE, false)];
    while let Some((entries, parent, added)) = pending.pop() {
        held.clear();
        if !added && entries.iter().any(LocalTree::is_read) {
            let mut children =
                StateNodeChildrenIterator::new(state.clone(), repository.clone(), parent).await?;
            while let Some((node_id, node)) = children.next().await? {
                held.push((node.name_hash, node_id, node));
            }
            held.sort_unstable_by_key(|(name_hash, _, _)| *name_hash);
        }
        for entry in entries.iter().filter(|entry| LocalTree::is_read(entry)) {
            let existing = held
                .binary_search_by_key(&entry.name_hash, |(name_hash, _, _)| *name_hash)
                .ok()
                .map(|index| (held[index].1, held[index].2));
            let name = local.name(entry);
            let (node_id, added) =
                add_local_node(&state, repository, parent, name, entry, existing).await?;
            if entry.kind == LocalKind::Dir {
                pending.push((local.children(entry), node_id, added));
            }
        }
    }
    Ok(state)
}

/// Detaches each path of `deleted` that `state` holds, but for one `local` recreates with the
/// same kind, a file or symlink over a file or a directory over a directory, which stays the
/// node it was, as an edited file does; the listed paths below it are still applied. A path
/// below a detached one is skipped.
async fn apply_deletions(
    state: &State,
    repository: &Arc<RepositoryContext>,
    mut deleted: Vec<RelativePath>,
    local: &LocalTree,
) -> Result<(), StateError> {
    deleted.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    deleted.dedup_by(|a, b| a.as_str() == b.as_str());
    let mut detached: Vec<&str> = Vec::new();
    for path in &deleted {
        let path = path.as_str();
        if has_ancestor_in(&detached, path) {
            continue;
        }
        let Some(node_id) = node_at_path(state, repository, path).await? else {
            continue;
        };
        let node = state.node(repository.clone(), node_id).await?;
        if local
            .find(path)
            .is_some_and(|entry| same_kind(entry.kind, &node))
        {
            continue;
        }
        state
            .node_detach(repository.clone(), node_id, Detach::Always)
            .await?;
        detached.push(path);
    }
    Ok(())
}

/// Whether a `write/` entry of `kind` stands for `node`: a file or symlink for a file, a
/// directory for a directory.
fn same_kind(kind: LocalKind, node: &Node) -> bool {
    match kind {
        LocalKind::File | LocalKind::Symlink => node.is_file(),
        LocalKind::Dir => node.is_directory(),
    }
}

/// Whether `sorted` holds an ancestor of `path`.
fn has_ancestor_in(sorted: &[&str], path: &str) -> bool {
    let mut path = path;
    while let Some((ancestor, _)) = path.rsplit_once('/') {
        if sorted.binary_search(&ancestor).is_ok() {
            return true;
        }
        path = ancestor;
    }
    false
}

/// The node at `path` in `state`; `None` where there is none, or where the path passes through
/// a file or a linked repository.
pub(crate) async fn node_at_path(
    state: &State,
    repository: &Arc<RepositoryContext>,
    path: &str,
) -> Result<Option<NodeID>, StateError> {
    let mut node_id = ROOT_NODE;
    for name in path.split('/') {
        match state
            .find_subnode(repository.clone(), node_id, hash_string(name))
            .await
        {
            Ok(child) => node_id = child,
            Err(error) if error.is_node_not_found() => return Ok(None),
            Err(error) => return Err(error),
        }
    }
    Ok(Some(node_id))
}

/// Renames `node_id` to `name` where its stored name differs from it byte-wise, as a case-only
/// rename does, keeping its place in its directory's chain, then marks it and the directories
/// above dirty: it stands at a path its revision does not hold. The node keeps its id, file id
/// and subtree. The name was checked against `local_state`'s index of the directory.
async fn respell(
    state: &State,
    repository: &Arc<RepositoryContext>,
    node_id: NodeID,
    name: &str,
) -> Result<(), StateError> {
    let differs = &*state.node_name_ref(repository.clone(), node_id).await? != name;
    if !differs {
        return Ok(());
    }
    state
        .node_rename(repository.clone(), node_id, NameChange::Rename(name))
        .await?;
    state
        .node_mark(
            repository,
            node_id,
            Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
        )
        .await
}

/// Adds `entry`, named `name`, under `parent` as a dirty, zero-address local-content node, and
/// answers its id and whether the node is new. `existing` is the node `parent` holds under the
/// folded name: a file takes the entry's content in place, a directory stays for a directory
/// entry, either taking the entry's spelling, and a node of another kind is replaced.
async fn add_local_node(
    state: &State,
    repository: &Arc<RepositoryContext>,
    parent: NodeID,
    name: &str,
    entry: &LocalEntry,
    existing: Option<(NodeID, Node)>,
) -> Result<(NodeID, bool), StateError> {
    let (flags, ext) = match entry.kind {
        LocalKind::Dir => (NodeFlags::NoFlags, NodeExtFlags::LocalContent),
        LocalKind::File => (NodeFlags::File, NodeExtFlags::LocalContent),
        LocalKind::Symlink => (
            NodeFlags::File,
            NodeExtFlags::LocalContent | NodeExtFlags::Symlink,
        ),
    };
    let mode = if entry.executable {
        NodeFileMode::Executable.bits()
    } else {
        0
    };
    if let Some((node_id, node)) = existing {
        if same_kind(entry.kind, &node) {
            respell(state, repository, node_id, name).await?;
        }
        match entry.kind {
            LocalKind::Dir if node.is_directory() => return Ok((node_id, false)),
            LocalKind::File | LocalKind::Symlink if node.is_file() => {
                // The content, the action and the extension flags in one write, then the
                // directories above: nothing else holds the State before it is served.
                state
                    .node_rewrite(
                        repository,
                        node_id,
                        &Content {
                            mode,
                            size: entry.size,
                            address: Address::default(),
                        },
                        Rewrite::Dirty(NodeFlags::DirtyModify, ext),
                    )
                    .await?;
                state
                    .node_mark(
                        repository,
                        parent,
                        Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
                    )
                    .await?;
                return Ok((node_id, false));
            }
            LocalKind::Dir | LocalKind::File | LocalKind::Symlink => {
                state
                    .node_detach(repository.clone(), node_id, Detach::Always)
                    .await?;
            }
        }
    }
    let node = Node {
        flags: flags.bits(),
        mode,
        size: entry.size,
        name_hash: entry.name_hash,
        ..Node::default()
    };
    let node_id = state
        .node_add(repository.clone(), parent, node, name)
        .await?;
    state
        .node_mark(
            repository,
            node_id,
            Mark::dirty(NodeFlags::DirtyAdd, Ext::Set(ext), false),
        )
        .await?;
    Ok((node_id, true))
}

impl FuseFilesystem {
    /// A provider answering OPEN zero-message.
    pub async fn new(
        repository: Arc<RepositoryContext>,
        state: Arc<State>,
        attr: AttrContext,
    ) -> Result<Arc<Self>, FuseProviderError> {
        Self::new_with_open(repository, state, attr, OpenMode::ZeroMessage).await
    }

    /// A provider answering OPEN zero-message over the local changes in `external_dir`, the
    /// instance's external directory. Its filesystem State ([`Self::state`]) is `revision`
    /// loaded afresh, with each path in the deletion list (`deletions.rs`) that it holds
    /// detached, then a dirty, zero-address [`NodeExtFlags::LocalContent`] node for every file,
    /// symlink and directory under `write/` (`local_tree.rs`), so `write/` wins over a deletion
    /// of the same path. A tracked file under `write/` stays its node, modified, even where it
    /// is also listed as deleted; a tracked directory under `write/` keeps its node and tracked
    /// children. Writes change the State and `write/` (`write.rs`), and lore reads them through
    /// the diff of the State (`operation.rs`).
    pub async fn new_writable(
        repository: Arc<RepositoryContext>,
        revision: Arc<State>,
        attr: AttrContext,
        external_dir: &Path,
    ) -> Result<Arc<Self>, FuseProviderError> {
        let remote = repository.serving_remote();
        Self::new_writable_over((repository, remote), revision, attr, external_dir).await
    }

    /// [`Self::new_writable`] reaching the remote through `remote`, a
    /// [`RepositoryContext::serving_remote`] of `repository`.
    pub(crate) async fn new_writable_over(
        (repository, remote): (Arc<RepositoryContext>, SharedRemote),
        revision: Arc<State>,
        attr: AttrContext,
        external_dir: &Path,
    ) -> Result<Arc<Self>, FuseProviderError> {
        let stores = claim_provider_stores(&repository).await?;
        let external = external_dir.to_path_buf();
        let (deletions, deleted, local) =
            lore_base::lore_spawn_blocking!(move || read_local_changes(&external))
                .await
                .internal("reading the local changes")??;
        let state = local_state(
            &provider_context(&repository, stores.as_ref(), &remote),
            &revision,
            deleted,
            &local,
        )
        .await
        .forward_any::<FuseProviderError>("building the filesystem State")?;
        let write = external_dir.join(WRITE_DIR);
        let local = lore_base::lore_spawn_blocking!(move || LocalFiles::open(&write))
            .await
            .internal("opening the local files")??;
        let filesystem = Self::build(
            repository,
            stores,
            remote,
            revision,
            attr,
            OpenMode::ZeroMessage,
            Some(Writable {
                local,
                deletions,
                state,
            }),
        )
        .await?;
        Ok(filesystem)
    }

    pub async fn new_with_open(
        repository: Arc<RepositoryContext>,
        state: Arc<State>,
        attr: AttrContext,
        open_mode: OpenMode,
    ) -> Result<Arc<Self>, FuseProviderError> {
        let stores = claim_provider_stores(&repository).await?;
        let remote = repository.serving_remote();
        Self::build(repository, stores, remote, state, attr, open_mode, None).await
    }

    /// The provider over `stores`, the claims [`claim_provider_stores`] took on `repository`'s
    /// stores.
    async fn build(
        repository: Arc<RepositoryContext>,
        stores: Option<Arc<StoreHold>>,
        remote: SharedRemote,
        state: Arc<State>,
        attr: AttrContext,
        open_mode: OpenMode,
        writable: Option<Writable>,
    ) -> Result<Arc<Self>, FuseProviderError> {
        let repository = provider_context(&repository, stores.as_ref(), &remote);
        let (local, deletions, served_state) = match writable {
            Some(writable) => (
                Some(writable.local),
                Some(writable.deletions),
                writable.state,
            ),
            None => (None, None, own_state(&repository, state).await?),
        };
        served_state.mark_served();
        let recorded = RecordedTimes::load(&repository).await;
        let tree = served_state
            .tree(repository.clone())
            .await
            .forward_any::<FuseProviderError>("reading the mounted tree header")?;
        let statfs = attr::statfs(
            &attr,
            tree.size,
            u64::from(tree.block_count) * BLOCK_NODE_COUNT as u64,
        );
        let served = Served::new(
            served_state,
            repository,
            Arc::new(ListingCache::new(LISTING_CACHE_BYTES, LISTING_BLOCK_BYTES)),
        );
        Ok(Arc::new(Self {
            served: ServedCell::new(Arc::new(served)),
            stores,
            remote,
            inodes: InodeTable::new(),
            spellings: SpellingRecord::new(),
            recorded,
            attr,
            notifier: OnceLock::new(),
            notifications: OnceLock::new(),
            access_recorder: OnceLock::new(),
            read_deadline_nanos: AtomicU64::new(0),
            recent_misses: RecentMisses::default(),
            plain_first: AtomicBool::new(false),
            advised: DashSet::new(),
            statfs: parking_lot::RwLock::new(statfs),
            dir_handles: DashMap::new(),
            next_dir_handle: AtomicU64::new(1),
            open_mode,
            context: lore_base::runtime::try_lore_context(),
            readaheads: AtomicU64::new(0),
            fragment_cache: FragmentCache::new(DEFAULT_FRAGMENT_CACHE).into_shared(),
            swap_plan: tokio::sync::Mutex::new(SwapPlan::default()),
            local,
            deletions,
            prefetch: PrefetchTracker::default(),
            listing_builds: DashMap::new(),
            listings_built: AtomicU64::new(0),
            published: AtomicBool::new(false),
            swaps: AtomicU64::new(0),
            swap_hook: OnceLock::new(),
            write_hook: OnceLock::new(),
            listing_hook: OnceLock::new(),
            walk_hook: OnceLock::new(),
            staging_hook: OnceLock::new(),
            emptiness_hook: OnceLock::new(),
            commit_hook: OnceLock::new(),
            lookup_hook: OnceLock::new(),
            commit_outcomes: [AtomicU64::new(0), AtomicU64::new(0)],
            commits_reading: AtomicUsize::new(0),
            lookup_refusals: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
        }))
    }

    /// The host files under `write/`; `None` on a read-only provider.
    pub(crate) fn local_files(&self) -> Option<&LocalFiles> {
        self.local.as_ref()
    }

    /// The context this provider serves what it reads under `repository` through
    /// (`provider_context`).
    pub fn provider_repository(&self, repository: &RepositoryContext) -> Arc<RepositoryContext> {
        repository.repair_shared_remote(&self.remote);
        provider_context(repository, self.stores.as_ref(), &self.remote)
    }

    /// The remote this provider reaches, which every context it serves through shares.
    pub(crate) fn remote(&self) -> SharedRemote {
        self.remote.clone()
    }

    /// The filesystem State every request reads.
    pub fn state(&self) -> Arc<State> {
        self.served.load().state.clone()
    }

    /// The deletion list; `None` on a read-only provider.
    pub fn deletions(&self) -> Option<&DeletionList> {
        self.deletions.as_ref()
    }

    pub(crate) fn attr_context(&self) -> &AttrContext {
        &self.attr
    }

    /// Hands over the session's notifier, and starts the mount's notification thread on it
    /// unless [`Self::set_notifications`] started one. A mount whose thread does not start fails:
    /// it would serve with nothing ever invalidated.
    pub fn set_notifier(&self, notifier: Notifier) -> std::io::Result<()> {
        self.set_notifications(Arc::new(notifier.clone()))?;
        let _ = self.notifier.set(notifier);
        Ok(())
    }

    /// Starts the mount's notification thread, sending through `sink`; only the first call takes
    /// effect. A test calls it before mounting to watch what is sent.
    pub fn set_notifications(&self, sink: Arc<dyn NotificationSink>) -> std::io::Result<()> {
        if self.notifications.get().is_none() {
            let _ = self.notifications.set(NotificationQueue::start(sink)?);
        }
        Ok(())
    }

    /// The thread swaps and instance operations send their invalidations from; `None` before
    /// the session hands over its notifier.
    pub fn notifications(&self) -> Option<&NotificationQueue> {
        self.notifications.get()
    }

    /// Sends what is queued and ends the notification thread, once the session no longer answers.
    pub async fn stop_notifications(&self) {
        if let Some(queue) = self.notifications.get() {
            queue.stop().await;
        }
    }

    /// Starts access recording for this mount's session; only the first call takes effect.
    pub fn start_access_recording(&self, recorder: AccessRecorder) {
        let _ = self.access_recorder.set(recorder);
    }

    pub fn notifier(&self) -> Option<&Notifier> {
        self.notifier.get()
    }

    pub(crate) fn swap_plan(&self) -> &tokio::sync::Mutex<SwapPlan> {
        &self.swap_plan
    }

    /// The paths the next swap places from what the State holds, sorted. For tests.
    pub async fn unsettled_paths(&self) -> Vec<String> {
        self.swap_plan.lock().await.unsettled_paths()
    }

    /// Publishes `state` read under `repository`, marking what was served superseded, and
    /// reports `tree`'s figures in STATFS where given. The listings carry over when `listings`
    /// is `None`.
    pub fn publish(
        &self,
        state: Arc<State>,
        repository: Arc<RepositoryContext>,
        listings: Option<Arc<ListingCache>>,
        tree: Option<&Tree>,
    ) {
        state.mark_served();
        let listings = listings.unwrap_or_else(|| self.served.load().listings.clone());
        self.served
            .publish(Arc::new(Served::new(state, repository, listings)));
        if let Some(tree) = tree {
            *self.statfs.write() = attr::statfs(
                &self.attr,
                tree.size,
                u64::from(tree.block_count) * BLOCK_NODE_COUNT as u64,
            );
        }
    }

    /// The State a first publish of `revision` installs: loaded afresh under `repository`, with
    /// the local changes of a writable provider applied as [`Self::new_writable`] applies them.
    pub(crate) async fn mount_state(
        &self,
        repository: &Arc<RepositoryContext>,
        revision: &Arc<State>,
    ) -> Result<Arc<State>, FuseProviderError> {
        let repository = self.provider_repository(repository);
        let (Some(local), Some(deletions)) = (&self.local, &self.deletions) else {
            return own_state(&repository, revision.clone()).await;
        };
        let deleted = deletions
            .read()
            .map_err(|error| mount_failed("reading", Path::new(DELETION_LIST), &error))?;
        let write = local.root_path().to_path_buf();
        let local = lore_base::lore_spawn_blocking!(move || {
            LocalTree::scan(&write).map_err(|error| mount_failed("reading", &write, &error))
        })
        .await
        .internal("reading the local changes")??;
        if deleted.is_empty() && local.top().is_empty() {
            return load_revision(&repository, revision.revision()).await;
        }
        local_state(&repository, revision, deleted, &local)
            .await
            .forward_any::<FuseProviderError>("building the filesystem State")
    }

    /// Whether a swap has published a revision since construction.
    pub(crate) fn is_published(&self) -> bool {
        self.published.load(Ordering::Acquire)
    }

    pub(crate) fn mark_published(&self) {
        self.published.store(true, Ordering::Release);
    }

    /// Runs `hook` in every later swap's apply, between removing and placing; `false` fails
    /// the apply there. For tests; a second call panics.
    pub fn set_swap_hook(&self, hook: SwapHook) {
        assert!(
            self.swap_hook.set(hook).is_ok(),
            "a swap hook is already set"
        );
    }

    pub(crate) fn swap_hook(&self) -> Option<SwapHook> {
        self.swap_hook.get().cloned()
    }

    /// Runs `hook` at every later write request's [`WriteStep`]; `false` fails the step. For
    /// tests; a second call panics.
    pub fn set_write_hook(&self, hook: WriteHook) {
        assert!(
            self.write_hook.set(hook).is_ok(),
            "a write hook is already set"
        );
    }

    /// Whether a test's hook lets the write request go on at `step`.
    pub(crate) fn write_step_allowed(&self, step: WriteStep) -> bool {
        self.write_hook.get().is_none_or(|hook| hook(step))
    }

    /// The write generation of inode `ino` ([`InodeEntry::write_generation`]); `None` for an
    /// inode the table does not hold.
    pub fn write_generation(&self, ino: u64) -> Option<u64> {
        self.inodes
            .get(ino)
            .map(|entry| entry.write_generation.load(Ordering::Acquire))
    }

    /// Closes inode `ino`'s cached host file, as FORGET does.
    pub fn close_local_file(&self, ino: u64) {
        if let Some(local) = self.local_files() {
            local.forget(ino);
        }
    }

    /// Runs `hook` after every later listing build, before the listing is cached. For tests;
    /// a second call panics.
    pub fn set_listing_hook(&self, hook: ListingHook) {
        assert!(
            self.listing_hook.set(hook).is_ok(),
            "a listing hook is already set"
        );
    }

    /// Runs `hook` at each later [`LookupStep`]. For tests; a second call panics.
    pub fn set_lookup_hook(&self, hook: LookupHook) {
        assert!(
            self.lookup_hook.set(hook).is_ok(),
            "a lookup hook is already set"
        );
    }

    /// Runs `hook` in every later listing build once its log mark is read, before its walk. For
    /// tests; a second call panics.
    pub fn set_walk_hook(&self, hook: ListingHook) {
        assert!(
            self.walk_hook.set(hook).is_ok(),
            "a walk hook is already set"
        );
    }

    /// Runs `hook` once each later copy-up's content is staged. For tests; a second call
    /// panics.
    pub fn set_staging_hook(&self, hook: StagingHook) {
        assert!(
            self.staging_hook.set(hook).is_ok(),
            "a staging hook is already set"
        );
    }

    pub(crate) fn staging_hook(&self) -> Option<StagingHook> {
        self.staging_hook.get().cloned()
    }

    /// Runs `hook` with the child a later emptiness check is about to read, as a change racing
    /// the check would: before each child a walk without fetching reads, and before the first
    /// child of a fetching walk. For tests; a second call panics.
    pub fn set_emptiness_hook(&self, hook: ListingHook) {
        assert!(
            self.emptiness_hook.set(hook).is_ok(),
            "an emptiness hook is already set"
        );
    }

    /// Runs the test's emptiness hook, if any, for `child`.
    pub(crate) async fn emptiness_step(&self, child: NodeID) {
        if let Some(hook) = self.emptiness_hook.get() {
            hook(child).await;
        }
    }

    /// Runs `hook` in every later commit once it has read the files it stores, before they are
    /// flipped. For tests; a second call panics.
    pub fn set_commit_hook(&self, hook: CommitHook) {
        assert!(
            self.commit_hook.set(hook).is_ok(),
            "a commit hook is already set"
        );
    }

    pub(crate) fn commit_hook(&self) -> Option<CommitHook> {
        self.commit_hook.get().cloned()
    }

    /// How many files commits flipped to the committed revision, and how many they left dirty.
    pub fn commit_outcomes(&self) -> (u64, u64) {
        (
            self.commit_outcomes[0].load(Ordering::Relaxed),
            self.commit_outcomes[1].load(Ordering::Relaxed),
        )
    }

    /// Counts one file a commit flipped, or left dirty.
    pub(crate) fn count_commit_outcome(&self, flipped: bool) {
        self.commit_outcomes[usize::from(!flipped)].fetch_add(1, Ordering::Relaxed);
    }

    /// Counts an operation that served a file's content, as a commit's does, until the answer is
    /// dropped: from then a RENAME, UNLINK or RMDIR records the old path of an added node as
    /// deleted too, since a commit may store it ([`Self::commit_reading`]). Other operations that
    /// serve content count too, which only records more paths the next commit drops.
    pub(crate) fn begin_commit_read(self: &Arc<Self>) -> CommitReading {
        self.commits_reading.fetch_add(1, Ordering::SeqCst);
        CommitReading(self.clone())
    }

    /// New inodes refused because a commit settled their name since the request read it, new
    /// inodes refused because a swap began since, and new inodes dropped because their local
    /// file's host file was absent. For tests.
    pub fn lookup_refusals(&self) -> (u64, u64, u64) {
        (
            self.lookup_refusals[0].load(Ordering::Relaxed),
            self.lookup_refusals[1].load(Ordering::Relaxed),
            self.lookup_refusals[2].load(Ordering::Relaxed),
        )
    }

    /// Whether an operation that served a file's content has not ended, so an added node a
    /// request moves or removes may be one a commit stores.
    pub(crate) fn commit_reading(&self) -> bool {
        self.commits_reading.load(Ordering::SeqCst) > 0
    }

    /// How many copy-ups hold staged content no request has taken.
    pub fn staged_copies(&self) -> usize {
        self.local.as_ref().map_or(0, LocalFiles::staged_copies)
    }

    /// How many State reads the last swap's apply made: node records read and children walked.
    pub async fn last_swap_reads(&self) -> u64 {
        self.swap_plan.lock().await.reads()
    }

    /// Whether a swap is applying to the State now.
    pub fn is_swapping(&self) -> bool {
        !self.swaps.load(Ordering::Acquire).is_multiple_of(2)
    }

    /// Marks a swap as applying, until [`Self::end_swap`].
    pub(crate) fn begin_swap(&self) {
        self.swaps.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn end_swap(&self) {
        self.swaps.fetch_add(1, Ordering::SeqCst);
    }

    /// The READ path's fragment cache, whose strategy tests and benchmarks may switch.
    pub fn fragment_cache(&self) -> &FragmentCache {
        &self.fragment_cache
    }

    /// What requests are answered from now.
    pub fn served(&self) -> Arc<Served> {
        self.served.load()
    }

    pub fn inodes(&self) -> &InodeTable {
        &self.inodes
    }

    pub fn spellings(&self) -> &SpellingRecord {
        &self.spellings
    }

    /// Switches READDIRPLUS to list a directory without creating inodes, as READDIR does, until
    /// a file in it is looked up: `find` then creates none for files, `ls -l` looks each one up.
    pub fn set_readdirplus_plain_first(&self, on: bool) {
        self.plain_first.store(on, Ordering::Relaxed);
    }

    /// Switches prefetching ahead of sequential READs of local content: a stream of READs each
    /// starting where the last ended has the leaves ahead of it loaded into its inode's state.
    /// Applies under the per-inode fragment-cache strategies only. Off by default.
    pub fn set_sequential_prefetch(&self, on: bool) {
        self.fragment_cache.set_sequential_prefetch(on);
    }

    pub fn recorded_times(&self) -> &RecordedTimes {
        &self.recorded
    }

    pub fn set_read_deadline(&self, deadline: Duration) {
        let nanos = u64::try_from(deadline.as_nanos()).unwrap_or(u64::MAX);
        self.read_deadline_nanos.store(nanos, Ordering::Relaxed);
    }

    fn read_deadline(&self) -> Duration {
        match self.read_deadline_nanos.load(Ordering::Relaxed) {
            0 => FETCH_DEADLINE,
            nanos => Duration::from_nanos(nanos),
        }
    }

    /// How many readaheads this provider has started.
    pub fn readaheads_started(&self) -> u64 {
        self.readaheads.load(Ordering::Relaxed)
    }

    /// How many listing builds are under way.
    pub fn listing_builds_in_flight(&self) -> usize {
        self.listing_builds.len()
    }

    /// How many listings this provider has built.
    pub fn listings_built(&self) -> u64 {
        self.listings_built.load(Ordering::Relaxed)
    }

    /// The heap bytes the chain logs of the served State hold ([`State::chain_log_bytes`]).
    #[doc(hidden)]
    pub fn chain_log_bytes(&self) -> usize {
        self.served.load().state.chain_log_bytes()
    }

    /// The time reported for paths no swap has recorded one for, in milliseconds.
    pub fn base_time(&self) -> u64 {
        self.attr.base_time
    }

    /// The execution context captured at construction, for work spawned off a request.
    pub(crate) fn execution_context(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.context.clone()
    }

    /// Runs `work` under the captured execution context unless one is already current.
    pub(crate) fn scoped<F: Future>(&self, work: F) -> impl Future<Output = F::Output> {
        scoped_in(self.context.as_ref(), work)
    }

    /// The view's verdict on `path` below a directory with view state `parent` under
    /// `repository` ([`child_filter_in`]), an exclusion's event sent under the execution context
    /// captured at construction where none is current.
    pub(crate) fn view_filter(
        &self,
        repository: &RepositoryContext,
        parent: FilterStates,
        path: &RelativePath,
        is_directory: bool,
    ) -> (FilterStates, bool) {
        child_filter_in(
            self.context.as_ref(),
            repository,
            parent,
            path,
            is_directory,
        )
    }

    /// The current listing of the root.
    pub async fn root_listing(&self, served: &Served) -> Result<Arc<Listing>, Errno> {
        self.current_listing(served, ROOT_NODE, FilterStates::ROOT, &RelativePath::new())
            .await
    }

    /// The current listing of directory `dir_ino`: the cached one where its directory is
    /// unchanged since the build, else a fresh build.
    pub async fn listing_for(&self, served: &Served, dir_ino: u64) -> Result<Arc<Listing>, Errno> {
        if let Some(listing) = self.cached_listing(served, dir_ino)? {
            return Ok(listing);
        }
        let (node, filter) = self.dir_node(dir_ino)?;
        let path = self.dir_path(dir_ino)?;
        let stale = served.listings.get(node);
        self.scoped(self.build_listing(served, node, filter, &path, stale.as_ref()))
            .await
    }

    /// The node and view state of directory inode `ino`; `ENOTDIR` for a file or symlink.
    pub(crate) fn dir_node(&self, ino: u64) -> Result<(NodeID, FilterStates), Errno> {
        if ino == ROOT_INO {
            return Ok((ROOT_NODE, FilterStates::ROOT));
        }
        let entry = self.inodes.get(ino).ok_or(Errno::ENOENT)?;
        match entry.kind {
            InodeKind::Dir {
                node,
                filter_state,
                view_line,
                ..
            } => Ok((node, filter_states_from_packed(filter_state, view_line))),
            InodeKind::File { .. } | InodeKind::Symlink => Err(Errno::ENOTDIR),
        }
    }

    /// The path of directory `ino`, which a listing build filters its children by.
    fn dir_path(&self, ino: u64) -> Result<RelativePath, Errno> {
        Ok(self.path_of(ino)?.0)
    }

    /// The listing of directory `dir_ino` if it needs no build: the cached one where its
    /// directory is unchanged since the build. `None` when a build is needed.
    fn cached_listing(&self, served: &Served, dir_ino: u64) -> Result<Option<Arc<Listing>>, Errno> {
        let (node, _) = self.dir_node(dir_ino)?;
        Ok(served
            .listings
            .get(node)
            .filter(|listing| listing.is_current()))
    }

    /// The listing LOOKUP reads for directory `dir_ino`: the cached one whatever was attached
    /// since, else a fresh build; `None` without `fetch` where a build is needed.
    pub(crate) async fn lookup_listing(
        &self,
        served: &Served,
        dir_ino: u64,
        fetch: bool,
    ) -> Result<Option<Arc<Listing>>, Errno> {
        let (node, filter) = self.dir_node(dir_ino)?;
        if let Some(listing) = served.listings.get(node) {
            return Ok(Some(listing));
        }
        if !fetch {
            return Ok(None);
        }
        let path = self.dir_path(dir_ino)?;
        self.scoped(self.build_listing(served, node, filter, &path, None))
            .await
            .map(Some)
    }

    /// The current listing of directory `node`, whose view state is `filter` at `path`.
    async fn current_listing(
        &self,
        served: &Served,
        node: NodeID,
        filter: FilterStates,
        path: &RelativePath,
    ) -> Result<Arc<Listing>, Errno> {
        let stale = served.listings.get(node);
        if let Some(listing) = &stale
            && listing.is_current()
        {
            return Ok(listing.clone());
        }
        self.scoped(self.build_listing(served, node, filter, path, stale.as_ref()))
            .await
    }

    /// The listing of the directory at `path`, `None` where `path` names nothing or a file.
    /// Names fold as LOOKUP folds them. Creates no inode.
    pub async fn listing_at_path(
        &self,
        served: &Served,
        path: &RelativePath,
    ) -> Result<Option<Arc<Listing>>, Errno> {
        Ok(self
            .spelled_listing_at_path(served, path, &|_| true)
            .await?
            .map(|(listing, _)| listing))
    }

    /// [`Self::listing_at_path`] with the directory's path as the State spells it, rebuilt
    /// until `valid` accepts it or [`STALE_REBUILDS`] rebuilds have run. Each name on the way
    /// is found as [`Self::named_child`] finds it.
    pub(crate) async fn spelled_listing_at_path(
        &self,
        served: &Served,
        path: &RelativePath,
        valid: &(dyn Fn(&Listing) -> bool + Sync),
    ) -> Result<Option<(Arc<Listing>, RelativePath)>, Errno> {
        // Boxed: the walk nests listing builds, and operation futures stay small without it.
        Box::pin(self.scoped(self.walk_path(served, path, valid))).await
    }

    /// The walk of [`Self::spelled_listing_at_path`], through current listings.
    async fn walk_path(
        &self,
        served: &Served,
        path: &RelativePath,
        valid: &(dyn Fn(&Listing) -> bool + Sync),
    ) -> Result<Option<(Arc<Listing>, RelativePath)>, Errno> {
        let Some((dir, walked)) = self.walk_to_directory(served, path).await? else {
            return Ok(None);
        };
        let listing = self.current_listing(served, dir.0, dir.1, &walked).await?;
        let listing = self
            .checked_listing(served, dir, &walked, listing, valid)
            .await?;
        Ok(Some((listing, walked)))
    }

    /// The directory at `path`: its node and view state, and its path as the State spells it.
    /// Each name on the way is found as [`Self::named_child`] finds it, in a current listing of
    /// its directory. `None` where `path` names nothing or a file.
    async fn walk_to_directory(
        &self,
        served: &Served,
        path: &RelativePath,
    ) -> Result<Option<Directory>, Errno> {
        let mut dir = (ROOT_NODE, FilterStates::ROOT);
        let mut walked = RelativePath::new();
        for name in path.as_str().split('/').filter(|name| !name.is_empty()) {
            let listing = self.current_listing(served, dir.0, dir.1, &walked).await?;
            let Some(child) = self
                .named_child(served, dir, &walked, listing, name)
                .await?
            else {
                return Ok(None);
            };
            if child.entry.flags & entry_flags::IS_DIR == 0 {
                return Ok(None);
            }
            let spelled = std::str::from_utf8(&child.spelled).map_err(|_invalid| Errno::EIO)?;
            walked = walked.join(spelled);
            dir = (
                child.node,
                filter_states_from_packed(child.entry.filter_state, child.entry.view_line),
            );
        }
        Ok(Some((dir, walked)))
    }

    /// The child spelled `name` under any case in directory `dir` at `dir_path`, as LOOKUP finds
    /// it in `listing` ([`Listing::lookup`]): among its entries and the nodes attached since.
    /// Where the listing is stale, as after a reattach in the directory since its build, the
    /// State answers by name instead ([`State::find_subnode`]), so one name never costs a
    /// build. `None` where the name holds nothing.
    async fn named_child(
        &self,
        served: &Served,
        (node, filter): (NodeID, FilterStates),
        dir_path: &RelativePath,
        listing: Arc<Listing>,
        name: &str,
    ) -> Result<Option<NamedChild>, Errno> {
        let failed = |error: StateError| {
            lore_warn!("FUSE lookup of '{name}' failed: {error}");
            Errno::EIO
        };
        let child = match listing.lookup(name) {
            Hit::Listed(index, entry) => {
                return Ok(Some(NamedChild {
                    entry,
                    node: listing.node_ids[index],
                    spelled: Name::from_slice(listing.name(index)),
                }));
            }
            Hit::Miss => return Ok(None),
            Hit::Added(child, block) => (child, Some(block)),
            Hit::Stale => {
                // Boxed: a cold path, kept out of every walk's future.
                let find: crate::BoxFuture<'_, _> = Box::pin(served.state.find_subnode(
                    served.repository.clone(),
                    node,
                    hash_string(name),
                ));
                match find.await {
                    Ok(child) => (child, None),
                    Err(error) if error.is_node_not_found() => return Ok(None),
                    Err(error) => return Err(failed(error)),
                }
            }
        };
        let described: crate::BoxFuture<'_, _> =
            Box::pin(self.child_of_node(served, (node, filter), dir_path, name, child));
        described.await
    }

    /// `child`, a node of directory `dir` with view state `filter` at `dir_path` found for
    /// `name`, as a listing would hold it, read from `block` where the log named it there. A
    /// link, and a child a listing would leave out, staged for deletion, spelled otherwise,
    /// unrepresentable or excluded by the view, answer `None`, as an absent name does. A node
    /// that left the name since the log named it is looked up again by name in the State.
    async fn child_of_node(
        &self,
        served: &Served,
        (dir, filter): (NodeID, FilterStates),
        dir_path: &RelativePath,
        name: &str,
        (child, block): (NodeID, Option<Arc<NodeBlock>>),
    ) -> Result<Option<NamedChild>, Errno> {
        let failed = |error: StateError| {
            lore_warn!("FUSE lookup of '{name}' failed: {error}");
            Errno::EIO
        };
        let name_hash = hash_string(name);
        let read = block.as_deref().and_then(|block| {
            block.read_node_and_name(Node::index(child), |node, held| {
                holds_name(node, dir, name_hash)
                    .then(|| (*node, held.map(|held| spelled_as(Some(held), name))))
            })
        });
        let (child, record, spelled) = if let Some((record, spelled)) = read {
            (
                child,
                record,
                spelled.map_err(|error| unnamed(name, &error))?,
            )
        } else {
            let child = if block.is_some() {
                match served
                    .state
                    .find_subnode(served.repository.clone(), dir, name_hash)
                    .await
                {
                    Ok(child) => child,
                    Err(error) if error.is_node_not_found() => return Ok(None),
                    Err(error) => return Err(failed(error)),
                }
            } else {
                child
            };
            let record = served
                .state
                .node(served.repository.clone(), child)
                .await
                .map_err(failed)?;
            let held = served
                .state
                .node_name_ref(served.repository.clone(), child)
                .await
                .map_err(failed)?;
            let spelled = spelled_as(Some(&held), name);
            drop(held);
            (child, record, spelled)
        };
        if record.is_link() || record.is_staged_delete() {
            return Ok(None);
        }
        let Some(spelled) = spelled else {
            return Ok(None);
        };
        let spelled_str = std::str::from_utf8(&spelled).map_err(|_invalid| Errno::EIO)?;
        let (state, excluded) = self.view_filter(
            &served.repository,
            filter,
            &dir_path.join(spelled_str),
            record.is_directory(),
        );
        if excluded && !holds_local(&record) {
            return Ok(None);
        }
        let mut entry = Listing::entry_of(&record, spelled.len());
        entry.filter_state = filter_states_to_u8(state);
        entry.view_line = view_line(state);
        Ok(Some(NamedChild {
            entry,
            node: child,
            spelled,
        }))
    }

    /// The child spelled `name` under any case in the directory at `dir_path`, as
    /// [`Self::named_child`] finds it: `None` where `dir_path` names no directory, `Some(None)`
    /// where the directory holds no such child.
    ///
    /// A directory holds at most one child per folded name, so one child answers for every
    /// spelling: `State::node_attach` answers `Exists` for a second, staging claims nodes by
    /// their folded name, and a swap copies from States built the same way.
    pub(crate) async fn child_in_directory(
        &self,
        served: &Served,
        dir_path: &RelativePath,
        name: &str,
    ) -> Result<Option<Option<NamedChild>>, Errno> {
        Ok(self
            .spelled_child_in_directory(served, dir_path, name)
            .await?
            .map(|(_, _, child)| child))
    }

    /// [`Self::child_in_directory`] with the directory's path as the State spells it.
    async fn spelled_child_in_directory(
        &self,
        served: &Served,
        dir_path: &RelativePath,
        name: &str,
    ) -> Result<Option<(NodeID, RelativePath, Option<NamedChild>)>, Errno> {
        // Boxed: the walk nests listing builds, and operation futures stay small without it.
        Box::pin(self.scoped(async {
            let Some((dir, walked)) = self.walk_to_directory(served, dir_path).await? else {
                return Ok(None);
            };
            // A cached listing serves the name while it is current, else the State does; only a
            // directory with no listing cached builds one, which later reads then share.
            let listing = match served.listings.get(dir.0) {
                Some(listing) => listing,
                None => self.current_listing(served, dir.0, dir.1, &walked).await?,
            };
            let child = self
                .named_child(served, dir, &walked, listing, name)
                .await?;
            Ok(Some((dir.0, walked, child)))
        }))
        .await
    }

    /// `listing`, the listing of directory `node` with view state `filter` at `path`, or a
    /// rebuild of it where `valid` refuses it. Past [`STALE_REBUILDS`] rebuilds, the last one,
    /// as a directory read may list an entry changed during it.
    async fn checked_listing(
        &self,
        served: &Served,
        (node, filter): (NodeID, FilterStates),
        path: &RelativePath,
        mut listing: Arc<Listing>,
        valid: &(dyn Fn(&Listing) -> bool + Sync),
    ) -> Result<Arc<Listing>, Errno> {
        for _ in 0..STALE_REBUILDS {
            if valid(&listing) {
                break;
            }
            listing = self
                .scoped(self.build_listing(served, node, filter, path, Some(&listing)))
                .await?;
        }
        Ok(listing)
    }

    /// The entry of the child `path` names; `None` for the root and for a path that names
    /// nothing.
    pub async fn entry_at_path(
        &self,
        served: &Served,
        path: &RelativePath,
    ) -> Result<Option<ListingEntry>, Errno> {
        Ok(self
            .spelled_entry_at_path(served, path)
            .await?
            .map(|found| found.entry))
    }

    /// The content of the file at `path` as this provider serves it, read from its host file
    /// under `write/` where it is local and from the store otherwise, never through the mount,
    /// which the service never reads. `None` where `path` names no file, or names a symlink, which
    /// a command reads as nothing, as an ordinary clone's walk takes none.
    pub(crate) async fn read_held_file(
        &self,
        path: &RelativePath,
    ) -> Result<Option<Bytes>, FuseProviderError> {
        let served = self.served();
        let Some(found) = self
            .spelled_entry_at_path(&served, path)
            .await
            .map_err(|errno| {
                FuseProviderError::internal(format!("looking up '{path}': {errno:?}"))
            })?
        else {
            return Ok(None);
        };
        if found.entry.flags & (entry_flags::IS_DIR | entry_flags::SYMLINK) != 0 {
            return Ok(None);
        }
        let source = if found.entry.flags & entry_flags::LOCAL != 0 {
            match self.host_file_path(&found.spelled) {
                Some(host) => ContentSource::owned_file(host),
                None => return Ok(None),
            }
        } else {
            ContentSource::virtual_immutable(found.entry.address, found.entry.size)
        };
        let repository = served.repository.clone();
        drop(served);
        crate::immutable::read_source(&repository, &source)
            .await
            .map(Some)
            .forward_any::<FuseProviderError>("reading a file the provider serves")
    }

    /// Reads the root's ignore file once, so the store keeps its content, which the provider's
    /// context caches, and a command run offline reads it ([`Self::read_held_file`],
    /// `repository::load_held_filter`). A failure is left for the next command to report.
    pub(crate) async fn warm_ignore_file(&self) {
        for name in [
            crate::repository::DOT_LOREIGNORE,
            crate::repository::DOT_URCIGNORE,
        ] {
            match self
                .read_held_file(&RelativePath::new_from_clean_parts(name, ""))
                .await
            {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(error) => {
                    lore_warn!("FUSE could not read {name} ahead of the next command: {error}");
                    return;
                }
            }
        }
    }

    /// [`Self::entry_at_path`] with the child's node, its directory's node and its path as the
    /// State spells it.
    pub(crate) async fn spelled_entry_at_path(
        &self,
        served: &Served,
        path: &RelativePath,
    ) -> Result<Option<SpelledEntry>, Errno> {
        if path.is_empty() {
            return Ok(None);
        }
        let Some((dir, parent, Some(child))) = self
            .spelled_child_in_directory(served, &path.parent_path(), path.name())
            .await?
        else {
            return Ok(None);
        };
        let spelled = std::str::from_utf8(&child.spelled).map_err(|_invalid| Errno::EIO)?;
        Ok(Some(SpelledEntry {
            entry: child.entry,
            node: child.node,
            dir,
            spelled: parent.join(spelled),
        }))
    }

    /// The incarnation and write generation of inode `ino` ([`InodeEntry::incarnation`],
    /// [`InodeEntry::write_generation`]); `None` for an inode the table does not hold.
    pub(crate) fn inode_incarnation(&self, ino: u64) -> Option<(u64, u64)> {
        self.inodes.get(ino).map(|entry| {
            (
                entry.incarnation,
                entry.write_generation.load(Ordering::Acquire),
            )
        })
    }

    /// The inode the kernel holds for `path`, found by name from the root without counting a
    /// lookup; `None` where one on the way is not held.
    pub(crate) fn live_inode(&self, path: &RelativePath) -> Option<u64> {
        let mut ino = ROOT_INO;
        for name in path.as_str().split('/').filter(|name| !name.is_empty()) {
            ino = self.inodes.find_child(ino, hash_string(name), name)?;
        }
        Some(ino)
    }

    /// Where the local content at `spelled`, a path as the State spells it, lives on the host;
    /// `None` on a read-only provider.
    pub(crate) fn host_file_path(&self, spelled: &RelativePath) -> Option<std::path::PathBuf> {
        let root = self.local.as_ref()?.root_path();
        Some(if spelled.is_empty() {
            root.to_path_buf()
        } else {
            root.join(spelled.as_str())
        })
    }

    /// Builds and caches the listing of directory `node`, whose view state is `filter` at
    /// `path`. Concurrent calls for one directory share the build in flight. A cached listing
    /// other than `stale` answers without a build. A caller the shared build hands `stale` back
    /// joins or starts one more build.
    async fn build_listing(
        &self,
        served: &Served,
        node: NodeID,
        filter: FilterStates,
        path: &RelativePath,
        stale: Option<&Arc<Listing>>,
    ) -> Result<Arc<Listing>, Errno> {
        // Boxed and type-erased: a build nests the State's walk, and inline it would grow the
        // future of every request that can build a listing, in size and in the type depth a
        // caller's `Send` check walks.
        let build: crate::BoxFuture<'_, _> =
            Box::pin(self.shared_build(served, node, filter, path, stale));
        let listing = build.await?;
        if stale.is_some_and(|stale| Arc::ptr_eq(stale, &listing)) {
            let again: crate::BoxFuture<'_, _> =
                Box::pin(self.shared_build(served, node, filter, path, stale));
            return again.await;
        }
        Ok(listing)
    }

    /// The build in flight for directory `node`, joined or started. The build leaves the map as
    /// it completes, so a caller arriving later builds afresh.
    async fn shared_build(
        &self,
        served: &Served,
        node: NodeID,
        filter: FilterStates,
        path: &RelativePath,
        stale: Option<&Arc<Listing>>,
    ) -> Result<Arc<Listing>, Errno> {
        let share = BuildShare::join(&self.listing_builds, node);
        // Compared by address: the share keeps the build alive while it is compared.
        let build = Arc::as_ptr(&share.build) as usize;
        let listing = share
            .build
            .get_or_init(|| async {
                let built = self
                    .build_listing_once(served, node, filter, path, stale)
                    .await;
                self.listing_builds
                    .remove_if(&node, |_, shared| Arc::as_ptr(shared) as usize == build);
                built
            })
            .await
            .clone()?;
        drop(share);
        Ok(listing)
    }

    /// The build [`Self::build_listing`] shares.
    async fn build_listing_once(
        &self,
        served: &Served,
        node: NodeID,
        filter: FilterStates,
        path: &RelativePath,
        stale: Option<&Arc<Listing>>,
    ) -> Result<Arc<Listing>, Errno> {
        if let Some(listing) = served.listings.get(node)
            && !stale.is_some_and(|stale| Arc::ptr_eq(stale, &listing))
        {
            return Ok(listing);
        }
        self.listings_built.fetch_add(1, Ordering::Relaxed);
        let listing = self
            .scoped(Listing::build_with(
                served,
                node,
                filter,
                path,
                self.walk_hook.get(),
            ))
            .await
            .map_err(|error| {
                lore_warn!("FUSE listing of '{path}' failed: {error}");
                Errno::EIO
            })?;
        if let Some(hook) = self.listing_hook.get() {
            hook(node).await;
        }
        // Cached even where a swap or a rename ran during the build: the listing's log mark
        // precedes the walk, so a lookup replays what landed since.
        served.listings.insert(node, listing.clone());
        Ok(listing)
    }

    /// Caches the empty listing of `dir`, a directory MKDIR just attached, so the first LOOKUP
    /// in it answers on the worker. Cached as [`Self::build_listing_once`] caches a built one,
    /// whatever swap runs: what lands in the directory since is read from its log. Nothing where
    /// its block is not in memory or a child is attached already.
    pub(crate) fn cache_empty_listing(&self, served: &Served, dir: NodeID) {
        let Some(listing) = served
            .state
            .resident_block(NodeBlock::index(dir))
            .and_then(|dir_block| Listing::empty(dir, dir_block))
        else {
            return;
        };
        self.write_step_allowed(WriteStep::EmptyListingBuilt { dir });
        served.listings.insert(dir, listing);
    }

    /// LOOKUP answered from `served`, which the caller loaded: the exact spelling only, as the
    /// kernel asks ([`LookupCase::Exact`]).
    pub async fn lookup_at(
        &self,
        served: &Served,
        parent: u64,
        name: &[u8],
    ) -> Result<EntryReply, Errno> {
        self.lookup_case_at(served, parent, name, LookupCase::Exact)
            .await
    }

    /// [`Self::lookup_at`], answering the spellings `case` names.
    pub(crate) async fn lookup_case_at(
        &self,
        served: &Served,
        parent: u64,
        name: &[u8],
        case: LookupCase,
    ) -> Result<EntryReply, Errno> {
        self.lookup_with(served, parent, name, true, (OnStale::Rebuild, case))
            .await
            .map_err(Errno::from)
    }

    /// [`Self::lookup_case_at`]; without `fetch`, [`ReadFault::WouldFetch`] where the listing
    /// needs a build. A stale listing is treated as `on_stale` says.
    pub(crate) async fn lookup_with(
        &self,
        served: &Served,
        parent: u64,
        name: &[u8],
        fetch: bool,
        (on_stale, case): (OnStale, LookupCase),
    ) -> Result<EntryReply, ReadFault> {
        lookup_name(name)?;
        let Some(listing) = self.lookup_listing(served, parent, fetch).await? else {
            return Err(would_fetch(fetch));
        };
        self.lookup_in(served, parent, name, listing, (fetch, on_stale, case))
            .await
    }

    /// LOOKUP of `name` in `listing`, the listing of `parent`, rebuilt where it no longer
    /// describes the directory. Without `fetch` a rebuild answers [`ReadFault::WouldFetch`]. A
    /// swap that begins while the lookup runs makes it start again, so no inode is created
    /// from what the swap replaced. A name held in another spelling answers absent under
    /// [`LookupCase::Exact`], before any inode is counted.
    async fn lookup_in(
        &self,
        served: &Served,
        parent: u64,
        name: &[u8],
        mut listing: Arc<Listing>,
        (fetch, on_stale, case): (bool, OnStale, LookupCase),
    ) -> Result<EntryReply, ReadFault> {
        let name_str = lookup_name(name)?;
        let on_arriving = if fetch {
            OnArriving::Wait
        } else {
            OnArriving::Defer
        };
        self.record_dir_access(parent, Some(&listing));
        let mut rebuilds = 0;
        let mut settles = 0;
        let mut retried = false;
        let mut by_name = false;
        let (ino, attr, child) = loop {
            let pinned = self.pin();
            let epoch = pinned.settles;
            let hit = if by_name {
                self.name_in_state(served, parent, name_str).await?
            } else {
                self.lookup_hit(
                    served,
                    parent,
                    &mut listing,
                    name_str,
                    (fetch, on_stale),
                    &mut rebuilds,
                )
                .await?
            };
            // A file a commit is settling takes no inode until it is settled.
            if !matches!(hit, NameHit::Miss)
                && settles < SETTLE_ATTEMPTS
                && self.settling_name(parent, name_str)
            {
                settles += 1;
                self.settled_at_name(parent, name, epoch).await;
                continue;
            }
            let found = match hit {
                NameHit::Listed(index, child) => {
                    if case == LookupCase::Exact && listing.name(index) != name {
                        return Ok(self.negative_entry(served, parent, name_str)?);
                    }
                    let spelled =
                        std::str::from_utf8(listing.name(index)).map_err(|_invalid| Errno::EIO)?;
                    let node = listing.node_ids[index];
                    if let Some(hook) = self.lookup_hook.get() {
                        hook(LookupStep::Listed {
                            dir: listing.dir,
                            node,
                        })
                        .await;
                    }
                    match self
                        .child_inode(
                            served,
                            parent,
                            &mut None,
                            (&child, spelled, (listing.dir, node), || {
                                listing.holds_entry(index)
                            }),
                            name_str,
                            (pinned, on_arriving),
                        )
                        .await
                    {
                        Err(ReadFault::Errno(Errno::ENOENT))
                            if settles < SETTLE_ATTEMPTS
                                && self.settled_at_name(parent, name, epoch).await =>
                        {
                            settles += 1;
                            continue;
                        }
                        found => found?.map(|(ino, attr)| (ino, attr, child)),
                    }
                }
                NameHit::Added(node, block) => {
                    let dir = listing.dir;
                    if let Some(hook) = self.lookup_hook.get() {
                        hook(LookupStep::Added { dir, node }).await;
                    }
                    match self
                        .added_child(
                            served,
                            parent,
                            (dir, node, &block),
                            (name_str, case),
                            pinned,
                            on_arriving,
                        )
                        .await
                    {
                        Ok(Added::Found(found)) => Some(found),
                        Ok(Added::Refused) => None,
                        Ok(Added::Hidden) => {
                            return Ok(self.negative_entry(served, parent, name_str)?);
                        }
                        // The log answers once more, through a patch newer than the change.
                        Ok(Added::Stale) if !retried => {
                            retried = true;
                            continue;
                        }
                        // A name changing under every pass rebuilds, counted, so the lookup ends.
                        Ok(Added::Stale) => {
                            count_rebuild(fetch, &mut rebuilds)?;
                            match on_stale {
                                OnStale::Rebuild => {
                                    listing =
                                        self.rebuilt_listing(served, parent, &listing).await?;
                                }
                                OnStale::ByName => by_name = true,
                            }
                            continue;
                        }
                        Err(ReadFault::Errno(Errno::ENOENT))
                            if settles < SETTLE_ATTEMPTS
                                && self.settled_at_name(parent, name, epoch).await =>
                        {
                            settles += 1;
                            continue;
                        }
                        Err(fault) => return Err(fault),
                    }
                }
                NameHit::Miss => return Ok(self.negative_entry(served, parent, name_str)?),
            };
            if let Some(found) = found {
                break found;
            }
            // Refused with no swap begun: the name was settled since, so it starts again.
            if self.is_current(pinned.swaps) && settles < SETTLE_ATTEMPTS {
                settles += 1;
                self.settled_at_name(parent, name, epoch).await;
                continue;
            }
            count_rebuild(fetch, &mut rebuilds)?;
        };
        if self.plain_first.load(Ordering::Relaxed)
            && child.flags & entry_flags::IS_DIR == 0
            && !self.advised.contains(&parent)
        {
            self.advised.insert(parent);
        }
        let timeouts = self.timeouts_for(served, parent, child.name_hash, name);
        Ok(EntryReply {
            ino,
            generation: 0,
            attr,
            timeouts,
        })
    }

    /// What `name` holds in `listing`, the listing of `parent`, rebuilt where it no longer
    /// describes the directory, each rebuild counted in `rebuilds`. Without `fetch` a rebuild
    /// answers [`ReadFault::WouldFetch`]. A directory that changes past [`STALE_REBUILDS`]
    /// rebuilds, or any stale listing under [`OnStale::ByName`], answers from the State by name
    /// ([`State::find_subnode`]), as a node attached since the listing, or a miss.
    pub(crate) async fn lookup_hit(
        &self,
        served: &Served,
        parent: u64,
        listing: &mut Arc<Listing>,
        name: &str,
        (fetch, on_stale): (bool, OnStale),
        rebuilds: &mut u32,
    ) -> Result<NameHit, ReadFault> {
        loop {
            match listing.lookup(name) {
                Hit::Listed(index, entry) => return Ok(NameHit::Listed(index, entry)),
                Hit::Added(node, block) => return Ok(NameHit::Added(node, block)),
                Hit::Miss => return Ok(NameHit::Miss),
                Hit::Stale => {}
            }
            if !fetch {
                return Err(ReadFault::WouldFetch);
            }
            *rebuilds += 1;
            if *rebuilds > STALE_REBUILDS || on_stale == OnStale::ByName {
                return self.name_in_state(served, parent, name).await;
            }
            *listing = self.rebuilt_listing(served, parent, listing).await?;
        }
    }

    /// What `name` holds in directory `parent`, found by name in the State
    /// ([`State::find_subnode`]), as a node attached since any listing, or a miss.
    pub(crate) async fn name_in_state(
        &self,
        served: &Served,
        parent: u64,
        name: &str,
    ) -> Result<NameHit, ReadFault> {
        let (node, _) = self.dir_node(parent)?;
        // Boxed: a cold path, kept out of every LOOKUP's future.
        let find: crate::BoxFuture<'_, _> = Box::pin(served.state.find_subnode(
            served.repository.clone(),
            node,
            hash_string(name),
        ));
        let failed = |error: StateError| {
            lore_warn!("FUSE lookup of '{name}' failed: {error}");
            ReadFault::from(Errno::EIO)
        };
        let child = match find.await {
            Ok(child) => child,
            Err(error) if error.is_node_not_found() => return Ok(NameHit::Miss),
            Err(error) => return Err(failed(error)),
        };
        let block = served
            .state
            .block_with_nametable(served.repository.clone(), NodeBlock::index(child))
            .await
            .map_err(failed)?;
        Ok(NameHit::Added(child, block))
    }

    /// A fresh listing of directory `parent` in place of `stale`, which no longer describes it.
    pub(crate) async fn rebuilt_listing(
        &self,
        served: &Served,
        parent: u64,
        stale: &Arc<Listing>,
    ) -> Result<Arc<Listing>, Errno> {
        let (node, filter) = self.dir_node(parent)?;
        let path = self.dir_path(parent)?;
        self.scoped(self.build_listing(served, node, filter, &path, Some(stale)))
            .await
    }

    /// Whether a new inode may take what was read with the swap count at `pinned`: the count has
    /// not moved since. A swap applying then plans after it ends, so it orphans or updates the
    /// inode this request creates; one that ended since may already have planned.
    fn is_current(&self, pinned: u64) -> bool {
        self.swaps.load(Ordering::SeqCst) == pinned
    }

    /// The swap count and the settle log's epoch, read before a request resolves a name.
    fn pin(&self) -> Pinned {
        Pinned {
            swaps: self.swaps.load(Ordering::Acquire),
            settles: self.settle_epoch(),
        }
    }

    /// Whether an existing inode may take what was read with the swap count at `pinned`: no
    /// swap was applying then and none began since. An inode the kernel holds keeps its content
    /// through an apply, whose plan invalidates it against what the kernel may have cached.
    fn is_settled(&self, pinned: u64) -> bool {
        pinned.is_multiple_of(2) && self.is_current(pinned)
    }

    /// The inode for `node`, attached to `parent`'s directory `dir` since its listing was built
    /// and held in `block`, with one more lookup counted, its attributes and its entry. The
    /// record and name are read from `block` under a borrowed read guard, no State lock. An
    /// inode the kernel holds for the name is found by its name and takes the node's content,
    /// but not its view state, while no swap began since `pinned`, so a repeated lookup
    /// allocates nothing. A node whose record no longer holds the name answers [`Added::Stale`];
    /// one spelled otherwise than `requested`, under [`LookupCase::Exact`], [`Added::Hidden`].
    async fn added_child(
        &self,
        served: &Served,
        parent: u64,
        (dir, node_id, block): (NodeID, NodeID, &NodeBlock),
        (requested, case): (&str, LookupCase),
        pinned: Pinned,
        on_arriving: OnArriving,
    ) -> Result<Added, ReadFault> {
        let index = Node::index(node_id);
        let name_hash = hash_string(requested);
        let read = block.read_node_and_name(index, |node, name| {
            if !holds_name(node, dir, name_hash) {
                return Read::Stale;
            }
            let name = match name {
                Ok(name) => name,
                Err(error) => return Read::Unnamed(error),
            };
            let matches = match case {
                LookupCase::Exact => name == requested,
                LookupCase::Folded => fold_eq(name, requested),
            };
            if node.is_link() || !matches || !is_representable(name.as_bytes()) {
                return Read::Hidden;
            }
            Read::Found(
                Listing::entry_of(node, requested.len()),
                node.is_directory(),
            )
        });
        let (mut child, is_directory) = match read {
            Read::Found(child, is_directory) => (child, is_directory),
            Read::Hidden => return Ok(Added::Hidden),
            Read::Stale => return Ok(Added::Stale),
            Read::Unnamed(error) => return Err(unnamed(requested, &error).into()),
        };
        let mut attr = Attr::default();
        let mut host = None;
        let found = self
            .child_matching(
                parent,
                (dir, node_id),
                (child.name_hash, requested),
                on_arriving,
                |entry, adopt| {
                    if let InodeKind::Dir {
                        filter_state,
                        view_line,
                        ..
                    } = entry.kind
                    {
                        child.filter_state = filter_state;
                        child.view_line = view_line;
                    }
                    let same = identity_refresh(
                        entry,
                        &child,
                        (node_id, adopt),
                        self.is_settled(pinned.swaps),
                        || holds_name(&block.node(index), dir, name_hash),
                    );
                    if same {
                        attr = self.entry_attr(entry);
                        host = host_kind(&entry.kind);
                    }
                    same
                },
            )
            .await;
        let Some(found) = found else {
            return on_arriving.unwaited().map(|()| Added::Refused);
        };
        match found {
            ChildLookup::Counted(ino) => {
                let attr = self.host_completed(ino, host, attr)?;
                return Ok(Added::Found((ino, attr, child)));
            }
            ChildLookup::Mismatch(stale) => self.inodes.orphan(stale),
            ChildLookup::Missing => {}
        }
        if let Some(hook) = self.lookup_hook.get() {
            hook(LookupStep::Missing { dir, node: node_id }).await;
        }
        // Read again to copy: a rename since the first read leaves the name asked for absent.
        let name = block.read_node_and_name(index, |node, name| match name {
            Ok(name) if holds_name(node, dir, name_hash) && fold_eq(name, requested) => {
                Ok(Some(Name::from_slice(name.as_bytes())))
            }
            Ok(_) => Ok(None),
            Err(error) => Err(error),
        });
        let Some(name) = name.map_err(|error| unnamed(requested, &error))? else {
            return Ok(Added::Stale);
        };
        let name = std::str::from_utf8(&name).map_err(|_invalid| Errno::EIO)?;
        let parent_filter = self.dir_node(parent)?.1;
        let (state, excluded) = self.view_filter(
            &served.repository,
            parent_filter,
            &self.dir_path(parent)?.join(name),
            is_directory,
        );
        if excluded && !holds_local(&block.node(index)) {
            return Ok(Added::Hidden);
        }
        child.filter_state = filter_states_to_u8(state);
        child.view_line = view_line(state);
        Ok(
            match self
                .insert_child(
                    served,
                    parent,
                    &mut None,
                    (&child, name, node_id, || {
                        holds_name(&block.node(index), dir, name_hash)
                    }),
                    pinned,
                )
                .await?
            {
                Some((ino, attr)) => Added::Found((ino, attr, child)),
                None => Added::Refused,
            },
        )
    }

    /// The negative reply for `name` in `parent`, which caches the absence in the kernel unless
    /// `served` is superseded or the name's spellings are past their bound.
    pub(crate) fn negative_entry(
        &self,
        served: &Served,
        parent: u64,
        name: &str,
    ) -> Result<EntryReply, Errno> {
        if served.superseded() {
            return Err(Errno::ENOENT);
        }
        let timeouts = self
            .spellings
            .record(parent, hash_string(name), name.as_bytes(), false);
        if timeouts.entry.is_zero() {
            return Err(Errno::ENOENT);
        }
        Ok(EntryReply {
            ino: 0,
            generation: 0,
            attr: Attr::default(),
            timeouts,
        })
    }

    /// The inode for `child`, node `node` spelled `name`, of `parent`, read with the swap
    /// count at `pinned`, with one more lookup counted, and its attributes. An existing inode
    /// that is not the child is orphaned and replaced. The child's content goes into an inode,
    /// existing or new, only while no swap began since `pinned`, decided under the inode's
    /// lock; `None` where a new inode was refused for that. `parent_path` caches `parent`'s path
    /// across the children of one reply.
    async fn child_inode<Holds: Fn() -> bool>(
        &self,
        served: &Served,
        parent: u64,
        parent_path: &mut Option<(RelativePath, u32)>,
        (child, name, (dir, node), holds): (&ListingEntry, &str, (NodeID, NodeID), Holds),
        requested: &str,
        (pinned, on_arriving): (Pinned, OnArriving),
    ) -> Result<Option<(u64, Attr)>, ReadFault> {
        let mut attr = Attr::default();
        let mut host = None;
        let found = self
            .child_matching(
                parent,
                (dir, node),
                (child.name_hash, requested),
                on_arriving,
                |entry, adopt| {
                    let same = identity_refresh(
                        entry,
                        child,
                        (node, adopt),
                        self.is_settled(pinned.swaps),
                        &holds,
                    );
                    if same {
                        attr = self.entry_attr(entry);
                        host = host_kind(&entry.kind);
                    }
                    same
                },
            )
            .await;
        let Some(found) = found else {
            return on_arriving.unwaited().map(|()| None);
        };
        match found {
            ChildLookup::Counted(ino) => {
                return Ok(Some((ino, self.host_completed(ino, host, attr)?)));
            }
            ChildLookup::Mismatch(stale) => self.inodes.orphan(stale),
            ChildLookup::Missing => {}
        }
        Ok(self
            .insert_child(
                served,
                parent,
                parent_path,
                (child, name, node, holds),
                pinned,
            )
            .await?)
    }

    /// [`InodeTable::lookup_child_matching`] for node `node` at `name_hash`, spelled `requested`,
    /// in directory `dir`, inode `parent`, where a request moving a node to the name may be under
    /// way: it holds the name's `arriving` claim from before it moves the node until its inode
    /// stands at the name, and a new or replacing inode made in between would leave the moved
    /// node a second one. A mismatch with the claim held, read under the refused inode's entry
    /// guard (the request orphans that inode only through the same guard), or a first miss with
    /// it held, waits for one release of a claim on the name and looks again; a first miss with
    /// it free looks again at once, as a release between the miss and the check followed the
    /// inode's move. One release wait at most, so no stream of claims starves the lookup. During
    /// overflow, where no slot holds the name, a first miss pays one release of any claim past
    /// the slots, deferred. The wait yields with no guard held; where `on_arriving` does not
    /// wait, `None` answers instead. `matches` is told whether a file of another node may take
    /// `node`: only as the pass's one candidate, or once a pass of several found none standing
    /// for it, so a stale file inode ahead of the right one is passed over.
    async fn child_matching(
        &self,
        parent: u64,
        (dir, node): (NodeID, NodeID),
        (name_hash, requested): (u64, &str),
        on_arriving: OnArriving,
        mut matches: impl FnMut(&mut InodeEntry, bool) -> bool,
    ) -> Option<ChildLookup> {
        let place = (dir, name_hash);
        let mut missed = false;
        let mut waited = false;
        let mut adopting = false;
        loop {
            let mut refused = false;
            let mut shared = false;
            let mut arrival = None;
            let found =
                self.inodes
                    .lookup_child_matching(parent, name_hash, requested, |entry, alone| {
                        shared |= !alone;
                        let same = matches(entry, alone || adopting);
                        // The first refused, which a mismatch answers.
                        if !same && !refused {
                            refused = true;
                            arrival = (!waited).then(|| self.arrival_at(place)).flatten();
                        }
                        same
                    });
            let arrival = match (found, arrival) {
                (ChildLookup::Mismatch(_), _) if shared && !adopting => {
                    adopting = true;
                    continue;
                }
                (ChildLookup::Mismatch(_), Some(arrival)) => arrival,
                (ChildLookup::Missing, _) if !missed => {
                    missed = true;
                    match (!waited).then(|| self.arrival_at(place)).flatten() {
                        Some(arrival) => arrival,
                        None => continue,
                    }
                }
                (found, _) => return Some(found),
            };
            if on_arriving != OnArriving::Wait {
                return None;
            }
            if let Some(hook) = self.lookup_hook.get() {
                hook(LookupStep::Arriving { dir, node }).await;
            }
            self.arrived(arrival).await;
            waited = true;
        }
    }

    /// `attr`, what inode `ino`, counted by this lookup, answered under its entry lock, where the
    /// inode table holds it; from the host where the inode held local content of kind `host`
    /// under that lock, read after the lock is released. A host that fails drops the lookup
    /// counted.
    fn host_completed(&self, ino: u64, host: Option<HostKind>, attr: Attr) -> Result<Attr, Errno> {
        let Some(kind) = host else {
            return Ok(attr);
        };
        self.host_or_held_attr(ino, kind)
            .inspect_err(|_| self.drop_lookups(ino, 1))
    }

    /// Cold path: a new inode, with the per-path time recorded at the last swap, unless a swap
    /// began since `pinned`, the child is a local file a commit is settling or has settled
    /// since ([`Self::settle_touched`]), or `holds`, the node's record read again, says the node
    /// left the name: checked before the inode is indexed by name and after, under the shard
    /// lock, so an inode made during the settle, or for a node a RENAME or UNLINK detached, is
    /// rolled back before any other lookup counts it. The path is built only where a recorded
    /// time needs it.
    async fn insert_child(
        &self,
        served: &Served,
        parent: u64,
        parent_path: &mut Option<(RelativePath, u32)>,
        (child, name, node, holds): (&ListingEntry, &str, NodeID, impl Fn() -> bool),
        pinned: Pinned,
    ) -> Result<Option<(u64, Attr)>, Errno> {
        let mut mtime = 0;
        if !self.recorded.none() {
            let (parent_path, _) = match parent_path {
                Some(known) => &*known,
                None => parent_path.insert(self.path_of(parent)?),
            };
            let path = parent_path.join(name);
            mtime = self
                .scoped(self.recorded.mtime_for(&served.repository, &path, 0))
                .await;
        }
        let kind = if child.flags & entry_flags::IS_DIR != 0 {
            InodeKind::Dir {
                filter_state: child.filter_state,
                view_line: child.view_line,
                node,
                local: child.flags & entry_flags::LOCAL != 0,
            }
        } else if child.flags & entry_flags::SYMLINK != 0 {
            InodeKind::Symlink
        } else {
            InodeKind::File {
                address: child.address,
                size: child.size,
                executable: is_executable(child),
                local: child.flags & entry_flags::LOCAL != 0,
            }
        };
        let settling = (child.flags & entry_flags::LOCAL != 0
            && child.flags & entry_flags::IS_DIR == 0)
            .then(|| self.dir_node(parent).ok())
            .flatten()
            .map(|(dir, _)| (dir, child.name_hash));
        self.write_step_allowed(WriteStep::Admitting { node });
        let inserted = self.inodes.insert_child_if(
            NewChild {
                parent,
                name,
                name_hash: child.name_hash,
                kind,
                mtime,
                node,
            },
            || {
                // Orders the name index written before this check against a swap's count and a
                // settle's claim.
                std::sync::atomic::fence(Ordering::SeqCst);
                self.is_current(pinned.swaps)
                    && settling.is_none_or(|place| !self.settle_touched(place, pinned.settles))
                    && holds()
            },
            |entry| (self.entry_attr(entry), host_kind(&entry.kind)),
        );
        let Some((ino, (attr, host))) = inserted else {
            if !self.is_current(pinned.swaps) {
                self.lookup_refusals[1].fetch_add(1, Ordering::Relaxed);
            } else if holds() {
                self.lookup_refusals[0].fetch_add(1, Ordering::Relaxed);
            }
            return Ok(None);
        };
        if let Some(hook) = self.lookup_hook.get() {
            hook(LookupStep::Inserted { node }).await;
        }
        let attr = self.host_completed(ino, host, attr).inspect_err(|_| {
            self.lookup_refusals[2].fetch_add(1, Ordering::Relaxed);
        })?;
        Ok(Some((ino, attr)))
    }

    /// The path of `ino` from the root, by the spellings its inodes were created under.
    pub(crate) fn path_of(&self, mut ino: u64) -> Result<(RelativePath, u32), Errno> {
        let mut names: SmallVec<[Name; 8]> = SmallVec::new();
        while ino != ROOT_INO {
            if names.len() >= MAX_DEPTH {
                return Err(Errno::EIO);
            }
            let entry = self.inodes.get(ino).ok_or(Errno::ENOENT)?;
            names.push(entry.name.clone());
            ino = entry.parent;
        }
        let mut path = RelativePath::new();
        for name in names.iter().rev() {
            path = path.join(std::str::from_utf8(name).map_err(|_invalid| Errno::EIO)?);
        }
        Ok((path, names.len() as u32))
    }

    /// The recorder and `ino`'s path, if access recording is on, the revision serves the inode
    /// and this is its first access. A path under `write/`, a file or directory the mount's
    /// writer added, is not recorded: the revision holds nothing a prefetch could fetch for it.
    /// The path is resolved from the inode table alone, the same in-memory parent/name walk
    /// [`Self::path_of`] does.
    fn claim_access(&self, ino: u64) -> Option<(&AccessRecorder, RelativePath)> {
        let recorder = self.access_recorder.get()?;
        let claimed = self.inodes.get(ino).is_some_and(|entry| {
            host_kind(&entry.kind).is_none() && claim_once(&entry.access_recorded)
        });
        if !claimed {
            return None;
        }
        let (path, _) = self.path_of(ino).ok()?;
        Some((recorder, path))
    }

    /// Records `ino` as a directory, the first time a listing answers LOOKUP or
    /// READDIR/READDIRPLUS from it: its path with a trailing `/`, the root as `/`, then, given its
    /// `listing`, a tab and the ascending index of every node block holding one of its children,
    /// which a prefetch fetches without walking the sibling chain.
    fn record_dir_access(&self, ino: u64, listing: Option<&Listing>) {
        let Some((recorder, path)) = self.claim_access(ino) else {
            return;
        };
        let mut line = if path.is_empty() {
            "/".to_string()
        } else {
            format!("{path}/")
        };
        if let Some(listing) = listing {
            let mut blocks: Vec<usize> = listing
                .node_ids
                .iter()
                .map(|&id| NodeBlock::index(id))
                .collect();
            blocks.sort_unstable();
            blocks.dedup();
            let mut separator = '\t';
            for block in blocks {
                let _ = write!(line, "{separator}{block}");
                separator = ' ';
            }
        }
        recorder.record(line);
    }

    /// Records `ino` as a file, on its first successful non-empty READ.
    fn record_file_access(&self, ino: u64) {
        if let Some((recorder, path)) = self.claim_access(ino) {
            recorder.record(path.to_string());
        }
    }

    /// Attributes of inode `ino`: a local one's from the host, read with no inode lock held,
    /// else what the inode holds.
    pub(crate) fn attr_of(&self, ino: u64) -> Result<Attr, Errno> {
        let entry = self.inodes.get(ino).ok_or(Errno::ENOENT)?;
        let Some(kind) = host_kind(&entry.kind) else {
            return Ok(self.entry_attr(&entry));
        };
        drop(entry);
        self.host_or_held_attr(ino, kind)
    }

    /// Attributes of inode `ino`, local content of `kind` when read, from the host. An orphaned
    /// directory or symlink has no host path, and a file a commit has flipped since is
    /// store-served; each answers what it holds.
    fn host_or_held_attr(&self, ino: u64, kind: HostKind) -> Result<Attr, Errno> {
        match self.local_attr(ino, kind) {
            Err(Errno::ENOENT) => {
                let entry = self.inodes.get(ino).ok_or(Errno::ENOENT)?;
                if kind == HostKind::File && entry.kind.is_local_file() {
                    return Err(Errno::ENOENT);
                }
                Ok(self.entry_attr(&entry))
            }
            answered => answered,
        }
    }

    /// Attributes of `entry` from what it holds, under its entry lock and with no host call,
    /// with the owner write bit on a writable provider.
    fn entry_attr(&self, entry: &InodeEntry) -> Attr {
        let mut attr = attr_for(entry, &self.attr);
        if self.local.is_some() {
            attr.mode |= libc::S_IWUSR;
        }
        attr
    }

    pub(crate) fn timeouts_for(
        &self,
        served: &Served,
        parent: u64,
        name_hash: u64,
        name: &[u8],
    ) -> Timeouts {
        let timeouts = self.spellings.record(parent, name_hash, name, true);
        if served.superseded() {
            Timeouts::ZERO
        } else {
            timeouts
        }
    }

    /// The view stream `fh` on `ino` reads at `offset`: its pin past offset 0, else the current
    /// listing, which becomes the pin.
    async fn stream_listing(
        &self,
        served: &Served,
        ino: u64,
        fh: u64,
        offset: u64,
    ) -> Result<StreamView, Errno> {
        if let Some(pinned) = self.stream_pin(ino, fh, offset) {
            return Ok(pinned);
        }
        let pinned = self.pin();
        let listing = self.listing_for(served, ino).await?;
        Ok(self.pin_stream(ino, fh, listing, pinned))
    }

    /// [`Self::stream_listing`] when it needs no listing build or block load; `None` when it
    /// does.
    fn cached_stream_listing(
        &self,
        served: &Served,
        ino: u64,
        fh: u64,
        offset: u64,
    ) -> Result<Option<StreamView>, Errno> {
        if let Some(pinned) = self.stream_pin(ino, fh, offset) {
            return Ok(Some(pinned));
        }
        let pinned = self.pin();
        Ok(self
            .cached_listing(served, ino)?
            .map(|listing| self.pin_stream(ino, fh, listing, pinned)))
    }

    /// Stream `fh`'s pinned view of `ino`, which answers every READDIR past offset 0.
    fn stream_pin(&self, ino: u64, fh: u64, offset: u64) -> Option<StreamView> {
        if offset == 0 {
            return None;
        }
        self.pinned_view(ino, fh)
    }

    /// Stream `fh`'s pinned view of `ino`, whatever the offset.
    fn pinned_view(&self, ino: u64, fh: u64) -> Option<StreamView> {
        self.dir_handles
            .get(&fh)
            .filter(|handle| handle.ino == ino)
            .and_then(|handle| handle.view.clone())
    }

    /// `listing` pinned as stream `fh`'s view of `ino`, `pinned` read before `listing` was.
    fn pin_stream(&self, ino: u64, fh: u64, listing: Arc<Listing>, pinned: Pinned) -> StreamView {
        self.record_dir_access(ino, Some(&listing));
        let view = StreamView {
            listing,
            swaps: pinned.swaps,
            settles: pinned.settles,
        };
        if let Some(mut handle) = self.dir_handles.get_mut(&fh)
            && handle.ino == ino
        {
            handle.view = Some(view.clone());
        }
        view
    }

    /// Whether `view` still describes its directory: no swap began since it was read, its
    /// listing is the one cached for the directory, and every row it lists is still a member
    /// under its spelling ([`Listing::rows_current`]), whatever was created since. A READDIRPLUS
    /// page from a view that does not answers its entries plain; one that does still decides
    /// each entry's content under its inode's lock ([`Self::child_inode`]).
    fn stream_current(&self, served: &Served, view: &StreamView) -> bool {
        view.swaps.is_multiple_of(2)
            && self.is_current(view.swaps)
            && (view.listing.dir == NO_DIR
                || served
                    .listings
                    .get(view.listing.dir)
                    .is_some_and(|cached| Arc::ptr_eq(&cached, &view.listing)))
            && view.listing.rows_current()
    }

    fn parent_of(&self, ino: u64) -> u64 {
        self.inodes.get(ino).map_or(ROOT_INO, |entry| entry.parent)
    }

    /// Drops `n` lookups on `ino`, and the fragment cache's state and host file for it once
    /// none remain.
    pub(crate) fn drop_lookups(&self, ino: u64, n: u64) {
        // The host file leaves the cache as the entry leaves the table, under its shard lock, so
        // an inode taking the number since never meets it. Inode shard, then files shard, as
        // `host_write` and `descriptor` take them.
        let local = self.local.as_ref();
        let Some(host) = self
            .inodes
            .forget_with(ino, n, || local.map(|local| local.take_forgotten(ino)))
        else {
            return;
        };
        self.write_step_allowed(WriteStep::Forgotten { ino });
        self.fragment_cache.forget(ino);
        self.advised.remove(&ino);
        // Closed, and the staged copy removed, with no inode lock held.
        drop(host);
    }

    /// Pushes the READDIR entries of `view`, the stream of directory `ino`, from `offset` until
    /// `out` is full.
    fn push_dirents(&self, ino: u64, offset: u64, view: &StreamView, out: &mut DirentWriter<'_>) {
        let parent = self.parent_of(ino);
        let mut position = offset;
        loop {
            let pushed = match position {
                0 => out.push(ino, 1, DirentType::Dir, b"."),
                1 => out.push(parent, 2, DirentType::Dir, b".."),
                _ => match view.slot(position) {
                    Slot::End => break,
                    Slot::Hidden => {
                        position += 1;
                        continue;
                    }
                    Slot::Listed(index) => {
                        let child = view.listing.entry(index);
                        let name = view.listing.name(index);
                        out.push(
                            readdir_ino(ino, child, name),
                            position + 1,
                            dirent_type(child),
                            name,
                        )
                    }
                },
            };
            if !pushed {
                break;
            }
            position += 1;
        }
    }

    /// Pushes the READDIRPLUS entries of `view`, the stream of directory `ino`, from `offset`
    /// until `out` is full, counting a lookup per entry pushed with a node id; returns how many
    /// were counted. A view that no longer describes the directory pushes its entries plain, and
    /// so does the rest of the page once a swap began. An entry refused an inode, whose node left
    /// its name or whose lookup fails goes plain alone. Each counted entry carries its inode as
    /// its node id, which is what a failed reply gives back.
    async fn push_dirents_plus(
        &self,
        served: &Served,
        ino: u64,
        offset: u64,
        view: &StreamView,
        out: &mut DirentWriter<'_>,
    ) -> usize {
        let listing = &view.listing;
        let parent = self.parent_of(ino);
        let mut parent_path = None;
        let mut plain = (self.plain_first.load(Ordering::Relaxed) && !self.advised.contains(&ino))
            || !self.stream_current(served, view);
        let mut plain_row = false;
        let mut emitted = 0usize;
        let mut position = offset;
        loop {
            match position {
                0 | 1 => {
                    let (name, dot_ino): (&[u8], u64) = if position == 0 {
                        (b".", ino)
                    } else {
                        (b"..", parent)
                    };
                    // Node id 0: the kernel takes no reference for `.` and `..`.
                    let entry = EntryOut {
                        attr: Attr {
                            ino: dot_ino,
                            mode: DIR_MODE,
                            ..Attr::default()
                        },
                        ..EntryOut::default()
                    };
                    if !out.push_plus(&entry, position + 1, DirentType::Dir, name) {
                        break;
                    }
                }
                _ => {
                    let index = match view.slot(position) {
                        Slot::End => break,
                        Slot::Hidden => {
                            position += 1;
                            continue;
                        }
                        Slot::Listed(index) => index,
                    };
                    let name = listing.name(index);
                    let Ok(name_str) = std::str::from_utf8(name) else {
                        position += 1;
                        continue;
                    };
                    // The rows were read before the swap, as a stream pinned before one was
                    // (`stream_current`).
                    plain = plain || !self.is_settled(view.swaps);
                    // A row whose node left its name goes plain.
                    let current = if plain || plain_row {
                        None
                    } else {
                        listing.current_entry(index)
                    };
                    let Some(child) = current else {
                        plain_row = false;
                        // Node id 0: the kernel creates no dentry and takes no reference. The kind
                        // a plain row reports never changes with a record.
                        let listed = listing.entry(index);
                        let entry = EntryOut {
                            attr: Attr {
                                ino: readdir_ino(ino, listed, name),
                                ..Attr::default()
                            },
                            ..EntryOut::default()
                        };
                        if !out.push_plus(&entry, position + 1, dirent_type(listed), name) {
                            break;
                        }
                        position += 1;
                        continue;
                    };
                    let counted = self
                        .child_inode(
                            served,
                            ino,
                            &mut parent_path,
                            (
                                &child,
                                name_str,
                                (listing.dir, listing.node_ids[index]),
                                || listing.holds_entry(index),
                            ),
                            name_str,
                            (
                                Pinned {
                                    swaps: view.swaps,
                                    settles: view.settles,
                                },
                                // A row waits for no claim on its name: a plain row is a whole
                                // answer, and the kernel's LOOKUP of the name waits instead. A
                                // page cannot defer once it has counted rows.
                                OnArriving::Plain,
                            ),
                        )
                        .await;
                    // Refused an inode, or a lookup that failed, counted nothing: the row is
                    // listed as READDIR lists it, and a LOOKUP of the name meets the error.
                    let Ok(Some((child_ino, attr))) = counted else {
                        plain_row = true;
                        continue;
                    };
                    let timeouts = self.timeouts_for(served, ino, child.name_hash, name);
                    let entry = entry_out(child_ino, attr, timeouts);
                    // The inode's kind: one answered as it stands may differ from the record's.
                    if !out.push_plus(&entry, position + 1, attr_dirent_type(&attr), name) {
                        self.drop_lookups(child_ino, 1);
                        break;
                    }
                    emitted += 1;
                }
            }
            position += 1;
        }
        emitted
    }
}

impl Filesystem for FuseFilesystem {
    async fn lookup(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
    ) -> Result<EntryReply, Errno> {
        let served = self.served.load();
        self.lookup_at(&served, parent, name).await
    }

    async fn lookup_local(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
    ) -> Result<EntryReply, ReadFault> {
        let served = self.served.load();
        let Some(listing) = self.lookup_listing(&served, parent, false).await? else {
            return Err(ReadFault::WouldFetch);
        };
        self.lookup_in(
            &served,
            parent,
            name,
            listing,
            (false, OnStale::Rebuild, LookupCase::Exact),
        )
        .await
    }

    fn forget(&self, ino: u64, nlookup: u64) {
        self.drop_lookups(ino, nlookup);
    }

    async fn getattr(&self, _req: &RequestInfo, ino: u64) -> Result<AttrReply, Errno> {
        let mut attempts = 0;
        let attr = loop {
            let epoch = self.settle_epoch();
            match self.attr_of(ino) {
                // The host file a commit's flip holds aside answers once it is settled.
                Err(Errno::ENOENT)
                    if attempts < SETTLE_ATTEMPTS && self.settled_at_inode(ino, epoch).await =>
                {
                    attempts += 1;
                }
                attr => break attr?,
            }
        };
        Ok(AttrReply {
            attr,
            timeout: DEFAULT_TIMEOUTS.attr,
        })
    }

    fn statfs(&self, _req: &RequestInfo) -> Result<Kstatfs, Errno> {
        Ok(self.statfs_with_local(*self.statfs.read()))
    }

    fn opendir(&self, _req: &RequestInfo, ino: u64) -> Result<OpenReply, Errno> {
        match self.inodes.get(ino).ok_or(Errno::ENOENT)?.kind {
            InodeKind::Dir { .. } => {}
            InodeKind::File { .. } | InodeKind::Symlink => return Err(Errno::ENOTDIR),
        }
        let fh = self.next_dir_handle.fetch_add(1, Ordering::Relaxed);
        self.dir_handles.insert(fh, DirHandle { ino, view: None });
        Ok(OpenReply {
            fh,
            flags: open_flags::CACHE_DIR | open_flags::KEEP_CACHE,
        })
    }

    async fn readdir(
        &self,
        _req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        out: &mut DirentWriter<'_>,
    ) -> Result<(), Errno> {
        let served = self.served.load();
        let view = self.stream_listing(&served, ino, fh, offset).await?;
        self.push_dirents(ino, offset, &view, out);
        Ok(())
    }

    async fn readdir_local(
        &self,
        _req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        out: &mut DirentWriter<'_>,
    ) -> Result<(), ReadFault> {
        let served = self.served.load();
        let view = self
            .cached_stream_listing(&served, ino, fh, offset)?
            .ok_or(ReadFault::WouldFetch)?;
        self.push_dirents(ino, offset, &view, out);
        Ok(())
    }

    async fn readdirplus(
        &self,
        _req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        out: &mut DirentWriter<'_>,
    ) -> Result<usize, Errno> {
        let served = self.served.load();
        let view = self.stream_listing(&served, ino, fh, offset).await?;
        Ok(self
            .push_dirents_plus(&served, ino, offset, &view, out)
            .await)
    }

    async fn readdirplus_local(
        &self,
        _req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        out: &mut DirentWriter<'_>,
    ) -> Result<usize, ReadFault> {
        let served = self.served.load();
        let view = self
            .cached_stream_listing(&served, ino, fh, offset)?
            .ok_or(ReadFault::WouldFetch)?;
        Ok(self
            .push_dirents_plus(&served, ino, offset, &view, out)
            .await)
    }

    fn releasedir(&self, _ino: u64, fh: u64) {
        self.dir_handles.remove(&fh);
    }

    fn open(&self, _req: &RequestInfo, ino: u64, _flags: u32) -> Result<OpenReply, Errno> {
        if self.open_mode == OpenMode::ZeroMessage {
            return Err(Errno::ENOSYS);
        }
        match self.inodes.get(ino).ok_or(Errno::ENOENT)?.kind {
            // The inode pins its content, so the handle carries nothing.
            InodeKind::File { .. } => Ok(OpenReply {
                fh: 0,
                flags: open_flags::KEEP_CACHE | open_flags::NOFLUSH,
            }),
            InodeKind::Dir { .. } => Err(Errno::EISDIR),
            InodeKind::Symlink => Err(Errno::ELOOP),
        }
    }

    async fn read(
        &self,
        _req: &RequestInfo,
        ino: u64,
        _fh: u64,
        offset: u64,
        out: &mut [u8],
        fetch: bool,
    ) -> Result<usize, ReadFault> {
        let mut attempts = 0;
        let (address, size) = loop {
            let epoch = self.settle_epoch();
            let tracked = match self.inodes.get(ino).ok_or(Errno::ENOENT)?.kind {
                InodeKind::File { local: true, .. } => None,
                InodeKind::File { address, size, .. } => Some((address, size)),
                InodeKind::Dir { .. } => return Err(ReadFault::Errno(Errno::EISDIR)),
                InodeKind::Symlink => return Err(ReadFault::Errno(Errno::EINVAL)),
            };
            match tracked {
                Some(tracked) => break tracked,
                // A file a commit flipped since it was read as local is store-served; one whose
                // host file a commit's flip holds aside answers once it is settled.
                None => match self.read_local(ino, offset, out) {
                    Err(Errno::ENOENT) => {
                        let flipped = match self.inodes.get(ino).ok_or(Errno::ENOENT)?.kind {
                            InodeKind::File {
                                local: false,
                                address,
                                size,
                                ..
                            } => Some((address, size)),
                            _ => None,
                        };
                        if let Some(flipped) = flipped {
                            break flipped;
                        }
                        if attempts >= SETTLE_ATTEMPTS || !self.settled_at_inode(ino, epoch).await {
                            return Err(Errno::ENOENT.into());
                        }
                        attempts += 1;
                    }
                    read => return Ok(read?),
                },
            }
        };
        let served = self.served.load();
        let deadline = self.read_deadline();
        self.scoped(async {
            let read = read_file(
                &served,
                &self.fragment_cache,
                &self.recent_misses,
                ino,
                address,
                size,
                offset,
                out,
                deadline,
                fetch,
            )
            .await?;
            if read > 0 {
                self.record_file_access(ino);
                if readahead_wanted(size)
                    && self
                        .inodes
                        .get(ino)
                        .is_some_and(|entry| note_read(&entry, offset, read))
                {
                    self.readaheads.fetch_add(1, Ordering::Relaxed);
                    start_readahead(served.repository.clone(), address);
                }
            }
            Ok(read)
        })
        .await
    }

    fn release(&self, _ino: u64, _fh: u64) {}

    async fn setattr(
        &self,
        _req: &RequestInfo,
        ino: u64,
        attr: &SetattrIn,
    ) -> Result<AttrReply, Errno> {
        self.setattr_node(ino, attr, true)
            .await
            .map_err(Errno::from)
    }

    async fn mknod(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        _rdev: u32,
        _umask: u32,
    ) -> Result<EntryReply, Errno> {
        self.mknod_node(parent, name, mode, true)
            .await
            .map_err(Errno::from)
    }

    async fn mkdir(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        _umask: u32,
    ) -> Result<EntryReply, Errno> {
        self.create_node(parent, name, LocalKind::Dir, (mode, &[]), true)
            .await
            .map_err(Errno::from)
    }

    async fn readlink(&self, _req: &RequestInfo, ino: u64, out: &mut [u8]) -> Result<usize, Errno> {
        self.readlink_node(ino, out)
    }

    async fn symlink(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        target: &[u8],
    ) -> Result<EntryReply, Errno> {
        self.create_node(parent, name, LocalKind::Symlink, (0o777, target), true)
            .await
            .map_err(Errno::from)
    }

    async fn rename(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
    ) -> Result<(), Errno> {
        self.rename_node((parent, name), (newparent, newname), flags, true)
            .await
            .map_err(Errno::from)
    }

    /// Hard links are refused; tools that make them fall back to copies.
    async fn link(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _newparent: u64,
        _newname: &[u8],
    ) -> Result<EntryReply, Errno> {
        Err(if self.local.is_some() {
            Errno::EPERM
        } else {
            Errno::EROFS
        })
    }

    async fn unlink(&self, _req: &RequestInfo, parent: u64, name: &[u8]) -> Result<(), Errno> {
        self.remove_node(parent, name, false, true)
            .await
            .map_err(Errno::from)
    }

    async fn rmdir(&self, _req: &RequestInfo, parent: u64, name: &[u8]) -> Result<(), Errno> {
        self.remove_node(parent, name, true, true)
            .await
            .map_err(Errno::from)
    }

    async fn create(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        _umask: u32,
        _flags: u32,
    ) -> Result<CreateReply, Errno> {
        self.create_file_node(parent, name, mode, true)
            .await
            .map_err(Errno::from)
    }

    async fn write(
        &self,
        _req: &RequestInfo,
        ino: u64,
        _fh: u64,
        offset: u64,
        data: &[u8],
        _write_flags: u32,
    ) -> Result<usize, Errno> {
        self.write_node(ino, offset, data, true)
            .await
            .map_err(Errno::from)
    }

    async fn fallocate(
        &self,
        _req: &RequestInfo,
        ino: u64,
        _fh: u64,
        offset: u64,
        length: u64,
        mode: u32,
    ) -> Result<(), Errno> {
        self.fallocate_node(ino, (offset, length, mode), true)
            .await
            .map_err(Errno::from)
    }

    async fn setattr_local(
        &self,
        _req: &RequestInfo,
        ino: u64,
        attr: &SetattrIn,
    ) -> Result<AttrReply, ReadFault> {
        self.setattr_node(ino, attr, false).await
    }

    async fn fallocate_local(
        &self,
        _req: &RequestInfo,
        ino: u64,
        _fh: u64,
        offset: u64,
        length: u64,
        mode: u32,
    ) -> Result<(), ReadFault> {
        self.fallocate_node(ino, (offset, length, mode), false)
            .await
    }

    async fn create_local(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        _umask: u32,
        _flags: u32,
    ) -> Result<CreateReply, ReadFault> {
        self.create_file_node(parent, name, mode, false).await
    }

    async fn mknod_local(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        _rdev: u32,
        _umask: u32,
    ) -> Result<EntryReply, ReadFault> {
        self.mknod_node(parent, name, mode, false).await
    }

    async fn mkdir_local(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        _umask: u32,
    ) -> Result<EntryReply, ReadFault> {
        self.create_node(parent, name, LocalKind::Dir, (mode, &[]), false)
            .await
    }

    async fn symlink_local(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        target: &[u8],
    ) -> Result<EntryReply, ReadFault> {
        self.create_node(parent, name, LocalKind::Symlink, (0o777, target), false)
            .await
    }

    async fn unlink_local(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
    ) -> Result<(), ReadFault> {
        self.remove_node(parent, name, false, false).await
    }

    async fn rmdir_local(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
    ) -> Result<(), ReadFault> {
        self.remove_node(parent, name, true, false).await
    }

    async fn rename_local(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
    ) -> Result<(), ReadFault> {
        self.rename_node((parent, name), (newparent, newname), flags, false)
            .await
    }

    async fn write_local(
        &self,
        _req: &RequestInfo,
        ino: u64,
        _fh: u64,
        offset: u64,
        data: &[u8],
        _write_flags: u32,
    ) -> Result<usize, ReadFault> {
        self.write_node(ino, offset, data, false).await
    }

    async fn fsync_local(
        &self,
        _req: &RequestInfo,
        ino: u64,
        _fh: u64,
        _datasync: bool,
    ) -> Result<(), ReadFault> {
        self.fsync_local_node(ino)
    }

    async fn fetch(&self, _req: &RequestInfo, what: Fetch<'_>) -> Result<(), Errno> {
        match what {
            Fetch::Entry { parent, name, .. } => self.fetch_entry(parent, name).await,
            Fetch::Rename {
                parent,
                name,
                newparent,
                newname,
                flags,
            } => {
                self.fetch_rename((parent, name), (newparent, newname), flags)
                    .await
            }
            Fetch::Content { ino, len } => self.stage_copy(ino, len).await,
            Fetch::Flush { ino, datasync } => self.fsync_node(ino, datasync).await,
        }
    }

    fn reply_failed(&self, _req: &RequestInfo, outcome: &ReplyOutcome) {
        match *outcome {
            ReplyOutcome::Entry { ino } => {
                if ino != 0 {
                    self.drop_lookups(ino, 1);
                }
            }
            ReplyOutcome::None => {}
        }
    }
}

/// What [`FuseFilesystem::added_child`] answered.
enum Added {
    /// The inode, its attributes and its entry.
    Found((u64, Attr, ListingEntry)),
    /// The name answers absent: the node's name does not fold to the one asked for, the view
    /// excludes it, it is a linked repository, or its name cannot be an entry.
    Hidden,
    /// A swap began since the node was read; the lookup starts again.
    Refused,
    /// The node's record no longer holds the name the listing named it for: it moved since.
    Stale,
}

/// What [`FuseFilesystem::added_child`] read under the node's block lock.
enum Read {
    /// The node's entry, and whether it is a directory.
    Found(ListingEntry, bool),
    Hidden,
    Stale,
    /// The node's name could not be read from its block.
    Unnamed(NodeNameError),
}

/// The error for a LOOKUP of `requested` whose node's name could not be read.
pub(crate) fn unnamed(requested: &str, error: &NodeNameError) -> Errno {
    lore_warn!("FUSE lookup of '{requested}' failed: {error}");
    Errno::EIO
}

/// Whether `node` is a live child of `dir` named under `name_hash`.
/// `held`, a node's name, where it folds to `name` and can be an entry.
fn spelled_as(held: Option<&str>, name: &str) -> Option<Name> {
    held.filter(|held| fold_eq(held, name) && is_representable(held.as_bytes()))
        .map(|held| Name::from_slice(held.as_bytes()))
}

pub(crate) fn holds_name(node: &Node, dir: NodeID, name_hash: u64) -> bool {
    !node.is_discarded() && node.parent == dir && node.name_hash == name_hash
}

fn attr_dirent_type(attr: &Attr) -> DirentType {
    match attr.mode & libc::S_IFMT {
        libc::S_IFDIR => DirentType::Dir,
        libc::S_IFLNK => DirentType::Lnk,
        _ => DirentType::Reg,
    }
}

fn dirent_type(child: &ListingEntry) -> DirentType {
    if child.flags & entry_flags::IS_DIR != 0 {
        DirentType::Dir
    } else if child.flags & entry_flags::SYMLINK != 0 {
        DirentType::Lnk
    } else {
        DirentType::Reg
    }
}

fn is_executable(child: &ListingEntry) -> bool {
    child.mode & NodeFileMode::Executable == NodeFileMode::Executable
}

/// The number a plain READDIR reports for a child without creating its inode.
fn readdir_ino(parent: u64, child: &ListingEntry, name: &[u8]) -> u64 {
    let Ok(name) = std::str::from_utf8(name) else {
        return 0;
    };
    let kind = if child.flags & entry_flags::IS_DIR != 0 {
        IdentityKind::Dir
    } else if child.flags & entry_flags::SYMLINK != 0 {
        IdentityKind::Symlink
    } else {
        IdentityKind::File
    };
    derive_inode_number(&Identity {
        parent_ino: parent,
        name,
        name_hash: child.name_hash,
        kind,
    })
}

fn entry_out(ino: u64, attr: Attr, timeouts: Timeouts) -> EntryOut {
    EntryOut {
        nodeid: ino,
        generation: 0,
        entry_valid: timeouts.entry.as_secs(),
        attr_valid: timeouts.attr.as_secs(),
        entry_valid_nsec: timeouts.entry.subsec_nanos(),
        attr_valid_nsec: timeouts.attr.subsec_nanos(),
        attr,
    }
}

/// Whether `entry` is still the inode of `child`, node `node`, refreshing it where it is:
/// - a directory of `node` takes the child's view state when `current`;
/// - a symlink matches a symlink;
/// - a file of `node`, or of any node while not `current`, takes a tracked child's content into
///   a tracked inode when `current`.
///
/// A directory of another node, a file of another node while `current`, and an inode of another
/// kind are checked against `holds`, `node`'s record read again. Where `node` still holds the
/// name, the file takes `node` where `adopt`, else is refused, and the rest are mismatches.
/// Where it no longer does, a RENAME over the name since the record was read moved this inode
/// there, and every arm answers it as it stands, taking nothing. Where the content lives never
/// changes here: only the request that changes a node flips its inode.
fn identity_refresh(
    entry: &mut InodeEntry,
    child: &ListingEntry,
    (node, adopt): (NodeID, bool),
    current: bool,
    holds: impl Fn() -> bool,
) -> bool {
    if child.flags & entry_flags::IS_DIR != 0 {
        return match &mut entry.kind {
            InodeKind::Dir {
                filter_state,
                view_line,
                node: held,
                ..
            } => {
                if *held != node {
                    return !holds();
                }
                if current {
                    *filter_state = child.filter_state;
                    *view_line = child.view_line;
                }
                true
            }
            InodeKind::File { .. } | InodeKind::Symlink => !holds(),
        };
    }
    if child.flags & entry_flags::SYMLINK != 0 {
        return matches!(entry.kind, InodeKind::Symlink) || !holds();
    }
    if current && matches!(entry.kind, InodeKind::File { .. }) && entry.node != node {
        if !holds() {
            return true;
        }
        if !adopt {
            return false;
        }
        entry.node = node;
    }
    match &mut entry.kind {
        InodeKind::File {
            address,
            size,
            executable,
            local,
        } => {
            if current && !*local && child.flags & entry_flags::LOCAL == 0 {
                *address = child.address;
                *size = child.size;
                *executable = is_executable(child);
            }
            true
        }
        InodeKind::Dir { .. } | InodeKind::Symlink => !holds(),
    }
}

/// Runs `work` under `context` unless one is already current.
pub(crate) async fn scoped_in<T>(
    context: Option<&Arc<dyn Any + Send + Sync>>,
    work: impl Future<Output = T>,
) -> T {
    match context {
        Some(context) if LORE_CONTEXT.try_with(|_| ()).is_err() => {
            LORE_CONTEXT.scope(context.clone(), work).await
        }
        _ => work.await,
    }
}

/// A directory a walk reached: its node and view state, and its path as the State spells it.
type Directory = ((NodeID, FilterStates), RelativePath);

/// A child found by its path ([`FuseFilesystem::spelled_entry_at_path`]).
pub(crate) struct SpelledEntry {
    pub entry: ListingEntry,
    pub node: NodeID,
    /// The node of the directory holding it.
    pub dir: NodeID,
    /// Its path as the State spells it.
    pub spelled: RelativePath,
}

/// A child a walk found by name: its entry, its node, and its name as the State spells it.
pub(crate) struct NamedChild {
    pub(crate) entry: ListingEntry,
    pub(crate) node: NodeID,
    pub(crate) spelled: Name,
}

/// What [`FuseFilesystem::lookup_hit`] found for a name in a listing that describes its
/// directory.
pub(crate) enum NameHit {
    /// The listed entry at this index, with its record as it stands.
    Listed(usize, ListingEntry),
    /// A node attached since the listing, with the block holding its record.
    Added(NodeID, Arc<NodeBlock>),
    Miss,
}

impl std::fmt::Debug for NameHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Listed(index, _) => write!(f, "Listed({index})"),
            Self::Added(node, _) => write!(f, "Added({node})"),
            Self::Miss => f.write_str("Miss"),
        }
    }
}

/// What a local inode is on the host, which answers its attributes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostKind {
    File,
    Dir,
    Symlink,
}

/// Where an inode of `kind` answers its attributes from the host: a local file or directory, or a
/// symlink.
fn host_kind(kind: &InodeKind) -> Option<HostKind> {
    match kind {
        InodeKind::File { local: true, .. } => Some(HostKind::File),
        InodeKind::Dir { local: true, .. } => Some(HostKind::Dir),
        InodeKind::Symlink => Some(HostKind::Symlink),
        InodeKind::File { .. } | InodeKind::Dir { .. } => None,
    }
}

/// Counts one more rebuild of a listing in `rebuilds`: without `fetch` it answers
/// [`ReadFault::WouldFetch`], and past [`STALE_REBUILDS`] `EAGAIN`.
pub(crate) fn count_rebuild(fetch: bool, rebuilds: &mut u32) -> Result<(), ReadFault> {
    if !fetch {
        return Err(ReadFault::WouldFetch);
    }
    *rebuilds += 1;
    if *rebuilds > STALE_REBUILDS {
        return Err(ReadFault::Errno(Errno::EAGAIN));
    }
    Ok(())
}

/// `name` as LOOKUP takes it: over 255 bytes is `ENAMETOOLONG`, and a name that is not UTF-8
/// names nothing.
fn lookup_name(name: &[u8]) -> Result<&str, Errno> {
    if name.len() > 255 {
        return Err(Errno::ENAMETOOLONG);
    }
    std::str::from_utf8(name).map_err(|_invalid| Errno::ENOENT)
}
