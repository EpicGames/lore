use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use bytes::Bytes;
use bytes::BytesMut;
use dashmap::DashMap;
use dashmap::DashSet;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_error_set::ForwardAny;
use lore_error_set::WrapInternal;
use lore_error_set::error_set;
use lore_storage::WindowRead;

use crate::change::NodeChange;
use crate::filter::Filter;
use crate::filter::FilterMode;
use crate::fs::filesystem_provider::CloneAnswer;
use crate::fs::filesystem_provider::DirectoryEntry;
use crate::fs::filesystem_provider::DirectoryListing;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::FilesystemDiffContext;
use crate::fs::filesystem_provider::FilesystemProvider;
use crate::fs::filesystem_provider::FsError;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::StaticDispatchInstanceOperation;
use crate::fs::os::OsFilesystem;
use crate::fs::os::OsOperation;
use crate::fs::swfs::api_interface::SwfsInterface;
use crate::fs::swfs::api_interface::swfs_api::SWFSNodeModFlag_Deleted;
use crate::fs::swfs::callback_counters::CallbackMark;
use crate::fs::swfs::frozen_files::FrozenSnapshot;
use crate::fs::swfs::frozen_files::SwfsFrozenReader;
use crate::fs::swfs::modified_tree::Change;
use crate::fs::swfs::modified_tree::Coverage;
use crate::fs::swfs::modified_tree::ModifiedTree;
use crate::fs::swfs::mount_io::MountIo;
use crate::fs::swfs::mount_resources::SwfsExecutionToken;
use crate::fs::swfs::operation_lock::OperationGuard;
use crate::fs::swfs::paths::RelativePathKey;
use crate::fs::swfs::paths::SwfsPath;
use crate::fs::swfs::paths::at_or_below;
use crate::fs::swfs::paths::rest_below_exactly;
use crate::fs::swfs::record_diff;
use crate::fs::swfs::renamed_at_risk::give_renamed_files_at_risk_bytes_of_their_own;
use crate::fs::swfs::served_tree::MountChange;
use crate::fs::swfs::served_tree::Served;
use crate::fs::swfs::served_tree::ServedMount;
use crate::fs::swfs::served_tree::ServedTree;
use crate::fs::swfs::served_tree::mount_changes;
use crate::fs::swfs::state::ListedDeletions;
use crate::fs::swfs::state::ThawRemovals;
use crate::fs::swfs::state::name_committed_removals;
use crate::fs::swfs::state::tracked_file_info;
use crate::fs::swfs::view::ContentAnswer;
use crate::fs::swfs::view::HoldsAnswer;
use crate::fs::swfs::view::ListingAnswer;
use crate::fs::swfs::view::ReadSource;
use crate::fs::swfs::view::SwfsView;
use crate::fs::swfs::view::compare_content;
use crate::fs::swfs::view::file_not_found;
use crate::immutable;
use crate::immutable::stored_content;
use crate::lore_debug;
use crate::lore_warn;
use crate::node::INVALID_NODE;
use crate::node::Node;
use crate::repository::DOT_LOREIGNORE;
use crate::repository::RepositoryContext;
use crate::state::ChangeStream;
use crate::state::FilesystemDiffStats;
use crate::state::NodeComparison;
use crate::state::State;
use crate::state::StateError;
use crate::state::diff_subtree_paths;
use crate::state::node_change_state;
use crate::util::path::RelativePath;

#[error_set]
pub enum SwfsFilesystemError {}

/// A file an operation wrote from `repository`'s store.
#[derive(Debug)]
struct WrittenNode {
    repository: Arc<RepositoryContext>,
    node: Node,
}

/// SWFS-backed filesystem provider.
#[derive(Debug)]
pub struct SwfsFilesystem {
    mount: Arc<SwfsInterface>,
    os: OsFilesystem,
}

impl SwfsFilesystem {
    /// Create a new SWFS-backed filesystem provider.
    pub fn new(repo_path: impl AsRef<Path>, mount: Arc<SwfsInterface>) -> Self {
        Self {
            mount,
            os: OsFilesystem::new(repo_path),
        }
    }

    #[allow(clippy::unused_async)]
    pub async fn begin_operation(&self) -> Result<SwfsOperation, FsError> {
        // Held until the operation has finalized, so no other command freezes the mount, or thaws
        // it, while this one reads it (ADR-00026).
        let operation = self.mount.lock_operation().await?;
        // Before the freeze, so what it rewrites SWFS holds as its own by the time the operation
        // reads it, and while this operation holds the mount, so no other moves the source the
        // files are read from meanwhile.
        give_renamed_files_at_risk_bytes_of_their_own(
            self.mount.resources().renamed(),
            self.mount.resources().mount_path(),
            MountIo::global(),
        )
        .await;
        let callbacks_at_start = self.mount.resources().callbacks().mark();
        let moves_kept_before = self.mount.resources().renamed().mark();
        let operation = self
            .mount
            .freeze(SwfsExecutionToken {}, operation)
            .await
            .forward_any::<FsError>("Freezing SWFS file system")?;
        match self.read_frozen().await {
            Ok(snapshot) => {
                // A move SWFS no longer lists as renamed, as after a thaw named its destination or
                // a directory above it, is read from the source at the destination since, before
                // the operation reads it. Moves kept since the freeze began are left to the next.
                self.mount
                    .resources()
                    .renamed()
                    .prune_unlisted(moves_kept_before, |destination| {
                        snapshot.lists_renamed(destination)
                    });
                Ok(SwfsOperation::new(
                    self.mount.clone(),
                    snapshot,
                    callbacks_at_start,
                    Some(operation),
                    OsFilesystem::begin_operation(&self.os),
                    MountIo::global(),
                ))
            }
            // Thawed before the failure is returned, which otherwise leaves the mount frozen with
            // no operation to thaw it.
            Err(error) => {
                if let Err(thaw) = self
                    .mount
                    .thaw(
                        SwfsExecutionToken {},
                        false,
                        &DashSet::new(),
                        &ThawRemovals::default(),
                        operation,
                    )
                    .await
                {
                    lore_warn!("Unable to thaw SWFS after an operation failed to begin: {thaw}");
                }
                Err(error)
            }
        }
    }

    /// What an operation reads once the mount is frozen: what the mount serves, loaded again, and
    /// what SWFS froze, with the modified-file record rebuilt where it is untrusted.
    async fn read_frozen(&self) -> Result<FrozenSnapshot, FsError> {
        // What the mount serves is loaded again, for a command stores its staged state after the
        // operation it ran has thawed, so the last thaw loaded it as it was before.
        self.mount
            .resources()
            .update_state(SwfsExecutionToken {})
            .await
            .internal("Loading what the mount serves")?;
        // What SWFS froze is listed once, as the freeze left it.
        let snapshot = FrozenSnapshot::capture(Arc::new(SwfsFrozenReader(self.mount.clone())))
            .forward_any::<FsError>("Listing the frozen SWFS files")?;
        lore_debug!(
            "SWFS operation begins with these frozen, starting: {:x?}",
            snapshot.sample(20)
        );
        // The user's own `.loreignore`, where SWFS holds it changed, is the instance's ignore rules,
        // as a plain instance reads it from disk. The rules are set before the record is rebuilt,
        // which they decide what it skips of.
        let resources = self.mount.resources();
        let users_ignore = users_ignore_rules(&snapshot, resources.modified()).await;
        let read_from_snapshot = users_ignore.is_some();
        resources
            .use_users_ignore_rules(SwfsExecutionToken {}, users_ignore)
            .await;
        // An untrusted record, such as one a revision change emptied, is rebuilt from SWFS's own
        // list of the files it holds modified, so the view answers from the store and the frozen
        // snapshot rather than reading the mount: reads through the mount can starve the syscall
        // pool that serving them needs (ADR-00035).
        rebuild_untrusted_record(&self.mount).await?;
        // SWFS freezes no deletion, so a `.loreignore` the user deleted is told by the record only
        // once it is rebuilt, and the record is then rebuilt again under no rules.
        if !read_from_snapshot && users_deleted_ignore_rules(resources.modified()).await {
            resources
                .use_users_ignore_rules(SwfsExecutionToken {}, Some(Bytes::new()))
                .await;
            rebuild_untrusted_record(&self.mount).await?;
        }
        Ok(snapshot)
    }
}

#[async_trait]
impl FilesystemProvider for SwfsFilesystem {
    fn supports_dependency_sync(&self) -> bool {
        false
    }

    async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
        let holds_state = self.holds_state();
        Ok(Arc::new(InstanceOperationImpl::new(
            StaticDispatchInstanceOperation::Swfs(Box::new(self.begin_operation().await?)),
            holds_state,
        )))
    }

    async fn clone_answer(
        &self,
        _source: &Path,
        _dot_path: &Path,
        _destination: &Path,
    ) -> Result<CloneAnswer, crate::fs::filesystem_provider::InstanceCloneError> {
        Ok(CloneAnswer::Unsupported {
            reason: "this virtual filesystem provider can't clone instances".to_string(),
        })
    }
}

/// How much of a file [`SwfsOperation::copy_file`] holds at once: the window content is hashed
/// and chunked in.
const COPY_WINDOW: usize = 2 * lore_storage::FRAGMENT_SIZE_THRESHOLD;

/// SWFS-backed filesystem operation context.
pub struct SwfsOperation {
    /// Where each read of this operation is answered from.
    view: SwfsView,
    /// The callbacks SWFS had made when the operation began, to report what came during it.
    callbacks_at_start: CallbackMark,
    mount: Arc<SwfsInterface>,
    /// The mount's operation, held from before the freeze until the operation has finalized, and
    /// `None` for an operation that froze nothing.
    operation: parking_lot::Mutex<Option<OperationGuard>>,
    /// Paths this operation has written or removed, for thaw to describe to SWFS. Whether a path
    /// was added or deleted makes no difference to that, so they share the set.
    touched_nodes: DashSet<RelativePathKey>,
    /// The files this operation wrote to the mount itself: bytes it holds, such as a merge's
    /// result, and copies it made there. SWFS keeps these in its write cache, so they are left out
    /// of `touched_nodes`, which thaw describes from the repository's state. The mount holds them
    /// and nothing else yet, so the operation asks the mount about them.
    written_to_disk: DashSet<RelativePathKey>,
    /// The directories holding a path in `written_to_disk`, each with how many, so a listing can
    /// tell whether the mount holds something the operation wrote in it.
    disk_directories: DashMap<RelativePathKey, usize>,
    /// The directories at any depth above a path in `written_to_disk`, each with how many, so
    /// whether the operation wrote to the mount below a path is one lookup.
    disk_ancestors: DashMap<RelativePathKey, usize>,
    /// The files this operation wrote from the store with `write_node`, which SWFS is only told
    /// of when the operation thaws, so nothing holds them yet but the store. A merge writes the
    /// base and theirs copies this way and reads them straight back.
    written_nodes: DashMap<RelativePathKey, WrittenNode>,
    /// What the operation means each path it wrote from a store, created or removed to hold when
    /// it ends. One the state thaw describes from does not hold is realized on the mount when the
    /// operation finalizes (ADR-00033).
    intents: DashMap<RelativePathKey, Intent>,
    /// The directories the operation means to create or write something in, from `intents`.
    intended_children: IntendedChildren,
    /// The directories the operation means to remove something in, from `intents`.
    removed_children: RemovedChildren,
    /// What the mount served when the operation began, against which a change of revision, link
    /// or layer is diffed to keep what the operation leaves unwritten (ADR-00034).
    served_at_begin: Arc<ServedTree>,
    /// Every path the operation wrote, created, removed, renamed or copied to, by whatever means:
    /// what [`Self::keep_unwritten_changes`] leaves to the operation.
    operated: DashSet<RelativePathKey>,
    /// The paths the operation removed with what they held, whose contents it removed with them.
    removed_trees: DashSet<RelativePathKey>,
    /// Whether the operation may leave changes of the revision it moves the instance to unwritten,
    /// as a sync restricted to a dependency set does, or a failed operation: finalizing then diffs
    /// the revisions to keep them ([`Self::keep_unwritten_changes`]).
    leaves_changes_unwritten: AtomicBool,
    /// How many times the operation diffed the revisions it moved the instance between.
    revision_diffs: AtomicUsize,
    /// The served trees the operation removed whole, user files included, that the thaw describes
    /// as holding nothing: a link or layer mount path, as `link remove` and `layer remove --purge`
    /// remove (ADR-00037), or a tree the new revision no longer holds. What SWFS holds of the
    /// user's below them is deleted once the thaw has dropped the rest.
    removed_subtrees: parking_lot::Mutex<Vec<RelativePath>>,
    /// The paths [`InstanceOperation::remove`] removed as intents: each alone, not with what lies
    /// below it, as it would have from disk.
    removed_singly: DashSet<RelativePathKey>,
    /// The paths a commit told the operation it stored, which the thaw names once the commit is
    /// known to have succeeded.
    committed: DashSet<RelativePathKey>,
    /// The files a commit told the operation it deleted. Once the commit is known to have
    /// succeeded, the thaw names each SWFS lists as deleted, or the deleted directory it lay in,
    /// unless SWFS froze something there since.
    committed_removed: DashSet<RelativePathKey>,
    /// The directories a commit told the operation it deleted with all they held, of which the
    /// thaw names what SWFS lists as deleted at and below each.
    committed_removed_trees: DashSet<RelativePathKey>,
    /// The files SWFS froze, unmoved, that a comparison found holding the content each names, and
    /// its size: the thaw ending a successful operation names each where the state it describes
    /// from holds that content, so SWFS and the record drop a file the user changed back
    /// ([`Self::name_unchanged_files`]).
    unchanged: DashMap<RelativePathKey, (Address, u64)>,
    /// The directories this operation removed from the mount itself, which the thaw does not name: an
    /// entry naming one reads to SWFS as something that exists.
    removed_on_mount: DashSet<RelativePathKey>,
    /// The removed link and layer mount paths the operation means a directory at, made once the
    /// thaw has dropped what they held.
    recreated_mounts: parking_lot::Mutex<Vec<RelativePath>>,
    /// What the operation reads and writes on the mount itself, each syscall with a permit from
    /// `mount_io` (ADR-00035).
    os: OsOperation,
    mount_io: &'static MountIo,
}

/// What an operation means a path to hold when it ends.
#[derive(Clone)]
enum Intent {
    /// The content `address` names in `repository`'s store.
    Content {
        repository: Arc<RepositoryContext>,
        address: Address,
    },
    Directory,
    Removed,
}

impl Intent {
    /// Whether the intent makes something at its path: content or a directory, not a removal.
    fn makes(&self) -> bool {
        !matches!(self, Intent::Removed)
    }
}

/// The changes between what `old` and `new` serve: each link or layer whose repository, revision
/// or source changed, added or removed, reported at the paths it is mounted at, except one the
/// `view` leaves out, and with `revisions`, the parent's revisions outside every mount. Counts in
/// `revision_diffs` each diff of the parent's revisions.
async fn served_changes(
    old: &ServedTree,
    new: &ServedTree,
    view: &Arc<Filter>,
    revisions: bool,
    revision_diffs: &AtomicUsize,
) -> Result<Vec<NodeChange>, FsError> {
    let mut changes = Vec::new();
    let old_revision = old.current().map_or_default(|state| state.revision());
    let new_revision = new.current().map_or_default(|state| state.revision());
    if revisions && old_revision != new_revision {
        revision_diffs.fetch_add(1, Ordering::Relaxed);
        // Walked with the view, so a subtree it leaves out, whose links may never have been
        // fetched, is not entered.
        let parent_repository = Arc::new(new.repository().to_filter_context(view.clone()));
        let old_current = old.current().cloned().unwrap_or_else(State::new);
        let new_current = new.current().cloned().unwrap_or_else(State::new);
        let parent = ChangeStream::spawn(async move |changes| {
            crate::state::diff(
                parent_repository.clone(),
                old_current,
                parent_repository,
                new_current,
                None,
                None,
                &changes,
                FilterMode::View,
            )
            .await
        })
        .collect()
        .await
        .internal("Diffing the revisions the operation moved between")?;
        // What a link or layer serves is diffed from the mounts below, whose revisions the
        // parent's own may not hold.
        changes.extend(
            parent
                .into_iter()
                .filter(|change| !old.is_mounted(change.path()) && !new.is_mounted(change.path())),
        );
    }
    for (path, _) in mount_changes(old, new) {
        let path = &path;
        // A mount the view leaves out is not in the working tree, and what it holds may never have
        // been fetched, so there is nothing of it to diff or to keep.
        if view.excludes_tree(path, true, FilterMode::View) {
            continue;
        }
        let (from, to) = (old.mount_at(path), new.mount_at(path));
        // The side with no mount holds nothing there, spelled with the other side's state.
        let side = async |mount: Option<&ServedMount>, other: Option<&ServedMount>| {
            let (spelled_by, source) = match (mount, other) {
                (Some(mount), _) => (mount, mount.source),
                (None, Some(other)) => (other, INVALID_NODE),
                (None, None) => unreachable!("a mount path comes from one side or the other"),
            };
            node_change_state(
                &spelled_by.repository,
                &spelled_by.state,
                source,
                path.clone(),
            )
            .await
        };
        let from_side = side(from, to).await;
        let to_side = side(to, from).await;
        changes.extend(
            diff_subtree_paths(from_side, to_side, path.clone(), FilterMode::empty())
                .await
                .internal("Diffing a link or layer the operation moved")?,
        );
    }
    Ok(changes)
}

/// The content of the user's own `.loreignore`, where SWFS holds it changed: what `snapshot`
/// froze of it, or nothing where `record`, trusted, holds it deleted. `None` where SWFS holds the
/// revision's.
async fn users_ignore_rules(snapshot: &FrozenSnapshot, record: &ModifiedTree) -> Option<Bytes> {
    let path = RelativePath::new_from_initial_path(DOT_LOREIGNORE).ok()?;
    match snapshot.content(&path) {
        Ok(Some(source)) => match source.read_all().await {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                lore_warn!("Unable to read the frozen {DOT_LOREIGNORE}: {error}");
                None
            }
        },
        Ok(None) => {
            (record.is_trusted() && users_deleted_ignore_rules(record).await).then(Bytes::new)
        }
        Err(error) => {
            lore_warn!("Unable to read the frozen {DOT_LOREIGNORE}: {error}");
            None
        }
    }
}

/// Whether `record` holds the user's `.loreignore` deleted.
async fn users_deleted_ignore_rules(record: &ModifiedTree) -> bool {
    record
        .coverage(DOT_LOREIGNORE)
        .await
        .is_ok_and(|coverage| coverage == Coverage::Deleted)
}

/// How many times an operation rebuilds an untrusted record before it gives up: a rebuild leaves
/// the record untrusted only where a callback could not record a change while it ran.
const RECORD_REBUILDS: usize = 3;

/// Rebuilds `mount`'s modified-file record, where it is untrusted, from SWFS's own list of what it
/// holds modified, measured against what the mount now serves, so the operation's reads are
/// answered from the store and the frozen snapshot rather than through the mount (ADR-00035).
/// Called while the mount is frozen, so the list cannot change under it. Fails where SWFS cannot
/// list them, or where the record is still untrusted after [`RECORD_REBUILDS`] rebuilds.
async fn rebuild_untrusted_record(mount: &SwfsInterface) -> Result<(), FsError> {
    let resources = mount.resources();
    let record = resources.modified();
    for _ in 0..RECORD_REBUILDS {
        if record.is_trusted() {
            return Ok(());
        }
        lore_debug!(
            "SWFS rebuilds the modified-file record from its list of what it holds modified"
        );
        let listed = mount
            .modified_files()
            .forward_any::<FsError>("Listing the modified SWFS files")?;
        record
            .rebuild(&listed, Some(&resources.served_tree()))
            .await
            .forward_any::<FsError>("Rebuilding the modified-file record")?;
    }
    if record.is_trusted() {
        return Ok(());
    }
    Err(FsError::internal(
        "the modified-file record is still untrusted after it was rebuilt",
    ))
}

/// What an operation made at a path: the content `address` names in `repository`'s store, or a
/// directory.
enum Made {
    Content {
        repository: Arc<RepositoryContext>,
        address: Address,
    },
    Directory,
}

/// What an operation reads of a path below a directory it moves through the mount.
enum Carried {
    Directory,
    /// A directory the operation means to make, which the mount does not hold yet.
    MadeDirectory,
    /// What the operation removed, which the mount still holds until it finalizes.
    Removed,
    /// What the mount holds: written there by the operation, SWFS's own bytes, or a file whose
    /// origin serves no file.
    Mounted,
    /// The file `Node` of the repository's store, written by the operation.
    Node(Arc<RepositoryContext>, Node),
    /// The file `Node` of the repository's store, which SWFS holds as the revision holds it at the
    /// path it was moved from, with no bytes of its own.
    Moved(Arc<RepositoryContext>, Node),
    /// Content the operation took from the repository's store without its node.
    Content(Arc<RepositoryContext>, Address),
}

/// Whether the thaw still describes a path once the operation's intent for it is realized.
enum Description {
    /// Left in what the thaw describes.
    Kept,
    /// Taken out of what the thaw describes, as realized on the mount or named by what lies below.
    Dropped,
}

/// The changed mount `path` lies at or below.
fn mount_holding<'a>(
    changes: &'a [(RelativePath, MountChange)],
    path: &RelativePath,
) -> Option<&'a (RelativePath, MountChange)> {
    changes.iter().find(|(mount, _)| at_or_below(path, mount))
}

/// For each directory, the names of its children through which an operation's intents to make
/// something lie, each with how many: the directories an operation means to create, which the
/// mount does not hold until the operation finalizes, and what it means to create in each.
/// Kept beside the intents rather than found by scanning them, which a clone holds one of for
/// every file it writes.
#[derive(Default)]
struct IntendedChildren(DashMap<RelativePathKey, HashMap<String, usize>>);

impl IntendedChildren {
    /// Counts an intent to make something at `path` in every directory above it.
    // The entry's shard lock is held for one count, with no await while it is.
    #[allow(clippy::disallowed_methods)]
    fn add(&self, path: &RelativePath) {
        let mut directory = RelativePath::default();
        for name in path.as_str().split('/') {
            *self
                .0
                .entry(RelativePathKey(directory.clone()))
                .or_default()
                .entry(name.to_owned())
                .or_default() += 1;
            directory = directory.join(name);
        }
    }

    /// Takes back what [`Self::add`] counted for `path`.
    fn remove(&self, path: &RelativePath) {
        let mut directory = RelativePath::default();
        for name in path.as_str().split('/') {
            let key = RelativePathKey(directory.clone());
            if let Some(mut children) = self.0.get_mut(&key)
                && let Some(count) = children.get_mut(name)
            {
                *count -= 1;
                if *count == 0 {
                    children.remove(name);
                }
            }
            self.0.remove_if(&key, |_, children| children.is_empty());
            directory = directory.join(name);
        }
    }

    /// Whether the operation means to make something below `path`, spelled as it is.
    fn makes_below(&self, path: &RelativePath) -> bool {
        self.0.contains_key(&RelativePathKey(path.clone()))
    }

    /// The names of the children of the directory at `path` through which the operation means
    /// to make something.
    fn children(&self, path: &RelativePath) -> Vec<String> {
        self.0
            .get(&RelativePathKey(path.clone()))
            .map_or_default(|children| children.keys().cloned().collect())
    }

    fn clear(&self) {
        self.0.clear();
    }
}

/// For each directory, how many of its children an operation means gone: kept beside the intents
/// rather than found by scanning them, so a listing of a directory the operation removes nothing
/// in costs nothing in proportion to how much it removes elsewhere.
#[derive(Default)]
struct RemovedChildren(DashMap<RelativePathKey, usize>);

impl RemovedChildren {
    /// Counts an intent to remove `path` in the directory holding it.
    // The entry's shard lock is held for one count, with no await while it is.
    #[allow(clippy::disallowed_methods)]
    fn add(&self, path: &RelativePath) {
        *self
            .0
            .entry(RelativePathKey(path.parent_path()))
            .or_default() += 1;
    }

    /// Takes back what [`Self::add`] counted for `path`.
    fn remove(&self, path: &RelativePath) {
        let directory = RelativePathKey(path.parent_path());
        if let Some(mut count) = self.0.get_mut(&directory) {
            *count -= 1;
        }
        self.0.remove_if(&directory, |_, count| *count == 0);
    }

    /// Whether the operation means a child of the directory at `path` gone.
    fn removes_in(&self, path: &RelativePath) -> bool {
        self.0.contains_key(&RelativePathKey(path.clone()))
    }

    fn clear(&self) {
        self.0.clear();
    }
}

impl SwfsOperation {
    /// An operation over `mount` that has done nothing yet, holding the mount's `operation` where it
    /// froze the mount, reading what SWFS froze through `snapshot` and the mount's own files
    /// through `os`, each syscall there with a permit from `mount_io`.
    fn new(
        mount: Arc<SwfsInterface>,
        snapshot: FrozenSnapshot,
        callbacks_at_start: CallbackMark,
        operation: Option<OperationGuard>,
        os: OsOperation,
        mount_io: &'static MountIo,
    ) -> Self {
        let resources = mount.resources();
        // An operation that froze the mount rebuilt the record as it began, and fails where it
        // could not, so a callback that distrusts it since does not make the view distrust it.
        let trusted = operation.is_some() || resources.modified().is_trusted();
        let view = SwfsView::with_trust(
            resources.modified().clone(),
            snapshot,
            resources.served_tree(),
            trusted,
        )
        .with_kept_moves(resources.renamed().clone());
        let served_at_begin = resources.served_tree();
        Self {
            view,
            callbacks_at_start,
            mount,
            operation: parking_lot::Mutex::new(operation),
            touched_nodes: Default::default(),
            written_to_disk: Default::default(),
            disk_directories: Default::default(),
            disk_ancestors: Default::default(),
            written_nodes: Default::default(),
            intents: Default::default(),
            intended_children: Default::default(),
            removed_children: Default::default(),
            served_at_begin,
            operated: Default::default(),
            removed_trees: Default::default(),
            leaves_changes_unwritten: AtomicBool::new(false),
            revision_diffs: AtomicUsize::new(0),
            removed_subtrees: Default::default(),
            removed_singly: Default::default(),
            committed: Default::default(),
            committed_removed: Default::default(),
            committed_removed_trees: Default::default(),
            unchanged: Default::default(),
            removed_on_mount: Default::default(),
            recreated_mounts: Default::default(),
            os,
            mount_io,
        }
    }

    /// Runs `io`, a syscall on the mount, with a permit from the bound on them (ADR-00035), held
    /// for that call alone. Built once the permit is given, so a closure borrowing what the call needs
    /// holds no copy of it while it waits; a call taking values by move takes the permit inline.
    async fn on_mount<T, F: Future<Output = T>>(&self, io: impl FnOnce() -> F) -> T {
        let _permit = self.mount_io.permit().await;
        io().await
    }

    /// What the operation wrote to the mount at `path`, read through the mount a range at a time
    /// within the bound.
    async fn mounted_content(
        &self,
        path: &RelativePath,
    ) -> Result<lore_storage::ContentSource<'static>, FsError> {
        Ok(self.mount_io.content(self.os.absolute(path)?).await?)
    }

    /// The listing of the directory the mount holds at `path`, read whole within the bound, so no
    /// read of it is left for later outside it.
    async fn mounted_directory(&self, path: &RelativePath) -> Result<DirectoryListing, FsError> {
        let _permit = self.mount_io.permit().await;
        let mut listing = self.os.read_directory(path).await?;
        let mut entries = Vec::new();
        while let Some(entry) = listing.next().await {
            entries.push(entry?);
        }
        Ok(DirectoryListing::from_entries(entries))
    }

    /// Whether the file the operation wrote to the mount at `path` holds the content `previous`
    /// addresses, read within the bound.
    async fn mounted_holds_content(
        &self,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        previous: Address,
        previous_size: u64,
        established: &lore_storage::ContentHashes,
    ) -> Result<NodeComparison, FsError> {
        let source = match self.mounted_content(path).await {
            Ok(source) => source,
            Err(_) => return Ok(NodeComparison::Unreadable),
        };
        compare_content(repository, previous, previous_size, &source, established).await
    }

    /// Notes what the operation means `path` to hold when it ends.
    fn intend(&self, path: &RelativePath, intent: Intent) {
        if path.is_empty() {
            return;
        }
        self.operated.insert(RelativePathKey(path.clone()));
        let makes = intent.makes();
        let previous = self
            .intents
            .insert(RelativePathKey(path.clone()), intent)
            .map(|previous| previous.makes());
        match (previous, makes) {
            (None | Some(false), true) => self.intended_children.add(path),
            (Some(true), false) => self.intended_children.remove(path),
            _ => {}
        }
        match (previous, makes) {
            (None | Some(true), false) => self.removed_children.add(path),
            (Some(false), true) => self.removed_children.remove(path),
            _ => {}
        }
    }

    /// Whether the operation means `path`, spelled as it is, to be a directory when it ends: it
    /// created the directory, or means to make something below it. The mount holds neither until
    /// the operation finalizes, so what the operation reads back must.
    fn creates_directory(&self, path: &RelativePath) -> bool {
        if path.is_empty() {
            return false;
        }
        match self.intents.get(&RelativePathKey(path.clone())).as_deref() {
            Some(Intent::Directory) => true,
            Some(Intent::Content { .. }) => false,
            // Removed and something made below it since, or never named itself.
            Some(Intent::Removed) | None => self.intended_children.makes_below(path),
        }
    }

    /// Fails where a file stands at or above `path` that a directory cannot be made over, as making
    /// the directory, or a file below it, fails on disk there: a thaw describing it would replace a
    /// file the user holds.
    fn refuse_a_directory_over_a_file(&self, path: &RelativePath) -> Result<(), FsError> {
        if let Some(file) = self.file_blocking_a_directory_at(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("a file stands at {file}, where a directory would be made"),
            )
            .into());
        }
        Ok(())
    }

    /// The path at or above `path` where a file stands that a directory cannot be made over: one
    /// SWFS froze, which is a file the user created or changed, or one this operation wrote to
    /// the mount, unless the operation removes it. A file the revision serves and the user did not
    /// change is the operation's to replace.
    fn file_blocking_a_directory_at(&self, path: &RelativePath) -> Option<RelativePath> {
        let mut at = path.clone();
        while !at.is_empty() {
            if (self.view.froze_file_at(&at) || self.wrote_to_disk(&at))
                && !self.removed_by_intent(&at)
            {
                return Some(at);
            }
            at = at.parent_path();
        }
        None
    }

    /// Whether the operation means `path` gone when it ends: it removed the path, or a directory
    /// above it, and has written nothing at or below the path since. The removal reaches the
    /// mount only when the operation finalizes, so until then the mount still holds the path, and
    /// what the operation reads back must not.
    fn removed_by_intent(&self, path: &RelativePath) -> bool {
        !self.wrote_to_disk(path) && self.removal_reaches(path) && !self.writes_below(path)
    }

    /// Whether the intent nearest at or above `path` is a removal, whatever the operation wrote
    /// below `path` since.
    fn removal_reaches(&self, path: &RelativePath) -> bool {
        if self.intents.is_empty() {
            return false;
        }
        let mut ancestor = path.clone();
        loop {
            match self
                .intents
                .get(&RelativePathKey(ancestor.clone()))
                .as_deref()
            {
                Some(Intent::Removed) => return true,
                // Created or written after anything above it was removed.
                Some(_) => return false,
                None if ancestor.is_empty() => return false,
                None => ancestor = ancestor.parent_path(),
            }
        }
    }

    /// Whether the operation wrote, or means to create or write, anything below `path`.
    fn writes_below(&self, path: &RelativePath) -> bool {
        self.intended_children.makes_below(path) || self.wrote_to_disk_below(path)
    }

    /// Whether this operation wrote something to the mount at any depth below `path`.
    fn wrote_to_disk_below(&self, path: &RelativePath) -> bool {
        self.disk_ancestors
            .contains_key(&RelativePathKey(path.clone()))
    }

    /// Whether the operation wrote `path` to the mount, or means to make something there.
    fn makes_at(&self, path: &RelativePath) -> bool {
        self.wrote_to_disk(path)
            || self
                .intents
                .get(&RelativePathKey(path.clone()))
                .is_some_and(|intent| intent.makes())
    }

    /// Whether the operation means the entry `name` of the directory at `path` gone, as
    /// [`Self::removed_by_intent`] answers for it, for a listing that asks it of every entry:
    /// `reaches_directory`, what [`Self::removal_reaches`] answers for `path`, is found once.
    fn removed_entry(&self, path: &RelativePath, name: &str, reaches_directory: bool) -> bool {
        let child = path.join(name);
        let reached = match self.intents.get(&RelativePathKey(child.clone())).as_deref() {
            Some(Intent::Removed) => true,
            Some(_) => false,
            None => reaches_directory,
        };
        reached && !self.wrote_to_disk(&child) && !self.writes_below(&child)
    }

    /// Whether the operation reads nothing below `path`: a file, or a directory it has emptied.
    async fn holds_nothing_below(&self, path: &RelativePath) -> Result<bool, FsError> {
        if !InstanceOperation::file_info(self, path).await?.is_dir() {
            return Ok(true);
        }
        let mut listing = InstanceOperation::read_directory(self, path).await?;
        Ok(listing.next().await.is_none())
    }

    /// Whether SWFS holds a change at `path`: it froze the path, or the record holds one there or
    /// above. A file SWFS froze can be one the record has not heard of yet, as SWFS reports a
    /// change after the thaw that follows it.
    async fn swfs_holds_changed(&self, path: &RelativePath) -> Result<bool, FsError> {
        if self.view.frozen_info(path).is_some() {
            return Ok(true);
        }
        let coverage = self
            .mount
            .resources()
            .modified()
            .coverage(path.as_str())
            .await
            .internal("Asking the modified-file record")?;
        Ok(coverage != Coverage::Unchanged)
    }

    /// Whether SWFS holds a change below the directory at `path`: it froze something directly in
    /// it, or the record holds a child there, whether changed or only the way to a change below.
    async fn holds_changes_below(&self, path: &RelativePath) -> Result<bool, FsError> {
        if self.view.froze_in(path) {
            return Ok(true);
        }
        let children = self
            .mount
            .resources()
            .modified()
            .children(path.as_str())
            .await
            .internal("Asking the modified-file record")?;
        Ok(!children.is_empty())
    }

    /// Removes from the mount the files below `path` that SWFS did not freeze, as it holds no change
    /// to them, and each directory left empty, `path` included.
    async fn remove_unmodified_tree(&self, path: &RelativePath) {
        let mut pending = vec![path.clone()];
        let mut directories = Vec::new();
        while let Some(directory) = pending.pop() {
            let Ok(entries) = self.mounted_entries(&directory).await else {
                continue;
            };
            directories.push(directory.clone());
            for (name, is_directory) in entries {
                let Ok(child) =
                    RelativePath::new_from_initial_path(format!("{}/{name}", directory.as_str()))
                else {
                    continue;
                };
                if is_directory {
                    pending.push(child);
                } else if !self.view.froze_file_at(&child)
                    && let Err(error) = self.on_mount(|| self.os.remove(&child)).await
                {
                    lore_debug!("SWFS: {child} left by a removed mount: {error}");
                }
            }
        }
        for directory in directories.into_iter().rev() {
            if let Err(error) = self
                .on_mount(|| self.os.remove_empty_directory(&directory))
                .await
            {
                lore_debug!("SWFS: {directory} left by a removed mount: {error}");
            }
        }
    }

    /// The names the mount holds in the directory at `path`, each with whether it is a directory,
    /// read whole within the bound, or the error reading it. A symbolic link or junction,
    /// whatever it leads to, is no directory. A name that is not text is left out.
    async fn mounted_entries(&self, path: &RelativePath) -> Result<Vec<(String, bool)>, FsError> {
        let absolute = self.os.absolute(path)?;
        let _permit = self.mount_io.permit().await;
        let mut entries = lore_io::IoDriver::global().read_dir(absolute).await?;
        let mut names = Vec::new();
        loop {
            let Some(entry) = entries.next().await else {
                break;
            };
            let entry = entry?;
            let Some(name) = entry.file_name.to_str() else {
                continue;
            };
            let is_directory = !entry.is_symlink
                && entry
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.kind == lore_io::StatKind::Directory);
            names.push((name.to_owned(), is_directory));
        }
        Ok(names)
    }

    /// Whether the mount itself holds something at `path`.
    async fn mount_holds(&self, path: &RelativePath) -> bool {
        let Ok(absolute) = self.os.absolute(path) else {
            return false;
        };
        self.on_mount(|| lore_io::IoDriver::global().metadata(&absolute))
            .await
            .is_ok()
    }

    /// Writes the content `address` names in `repository`'s store to the mount at `path`, making
    /// the directories leading to it.
    async fn write_stored_to_mount(
        &self,
        repository: Arc<RepositoryContext>,
        address: Address,
        path: &RelativePath,
    ) -> Result<(), FsError> {
        if let Some(parent) = self.os.absolute(path)?.parent() {
            self.on_mount(|| lore_io::IoDriver::global().create_dir_all(parent))
                .await?;
        }
        // The store is read, and may be fetched from the remote, within the permit too: the write
        // takes the content as it arrives, in writes of its own that the one permit covers.
        {
            let _permit = self.mount_io.permit().await;
            self.os
                .set_file_to_immutable_store_contents(repository, address, path)
                .await?;
        }
        self.sync_to_mount(path).await
    }

    /// Makes what the operation wrote to the mount at `path` durable, so it reaches SWFS within
    /// this operation: left in the OS file cache, it reaches SWFS when the cache is next flushed,
    /// which can be after a later operation's thaw replaced the file, and SWFS then keeps it over
    /// what that thaw described.
    async fn sync_to_mount(&self, path: &RelativePath) -> Result<(), FsError> {
        let absolute = self.os.absolute(path)?;
        let _permit = self.mount_io.permit().await;
        lore_io::IoDriver::global()
            .open(absolute, &lore_io::OpenOptions::new().write(true))
            .await?
            .sync_data()
            .await?;
        Ok(())
    }

    /// Forgets what the operation meant `path` to hold, which it has since written on the mount
    /// itself.
    fn forget_intent(&self, path: &RelativePath) {
        match self.intents.remove(&RelativePathKey(path.clone())) {
            Some((_, intent)) if intent.makes() => self.intended_children.remove(path),
            Some(_) => self.removed_children.remove(path),
            None => {}
        }
    }

    /// Notes that the operation wrote the file `node` of `repository`'s store at `path`, which it
    /// reads from the store: thaw describes it, or finalizing realizes it on the mount
    /// (ADR-00033).
    fn note_written_node(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
    ) {
        self.record_written(path);
        self.forget_written_to_disk(path);
        self.intend(
            path,
            Intent::Content {
                repository: repository.clone(),
                address: node.address,
            },
        );
        self.written_nodes.insert(
            RelativePathKey(path.clone()),
            WrittenNode {
                repository,
                node: *node,
            },
        );
    }

    /// What lies below `from`, a directory the operation is about to move through the mount, each
    /// path below it with what the operation reads there, or nothing where `from` is no
    /// directory. A file SWFS holds no bytes of is fetched through the mount first
    /// ([`Self::fetch_through_mount`]). Boxed by its caller, as a path that may be a directory is
    /// rarely moved through the mount.
    async fn what_lies_below(
        &self,
        from: &RelativePath,
    ) -> Result<Vec<(String, Carried)>, FsError> {
        let mut carried = Vec::new();
        if !InstanceOperation::file_info(self, from).await?.is_dir() {
            return Ok(carried);
        }
        let below = |path: &RelativePath| path.as_str()[from.as_str().len() + 1..].to_owned();
        let mut fetch_buffer = None;
        // Listed through the mount, which is what the rename moves: the view lists no more of a
        // directory SWFS holds moved than SWFS froze of it.
        let mut pending = vec![from.clone()];
        while let Some(directory) = pending.pop() {
            for (name, is_directory) in self.mounted_entries(&directory).await? {
                let child = directory.join(&name);
                // What the operation removed is gone, as moved on disk it would be.
                if self.removed_by_intent(&child) {
                    carried.push((below(&child), Carried::Removed));
                    continue;
                }
                // What the operation wrote or means at the path decides, not what the mount lists.
                if let Some(made) = self.carried_made(&child) {
                    if is_directory && matches!(made, Carried::MadeDirectory) {
                        pending.push(child.clone());
                    }
                    carried.push((below(&child), made));
                    continue;
                }
                if is_directory {
                    carried.push((below(&child), Carried::Directory));
                    pending.push(child);
                    continue;
                }
                let file = self.carried_file(&child).await?;
                if matches!(file, Carried::Moved(..)) {
                    fetch_buffer = Some(self.fetch_through_mount(&child, fetch_buffer).await?);
                }
                carried.push((below(&child), file));
            }
        }
        // What the operation means to make below it that the mount does not hold yet.
        let made_below = self.made_below(from);
        if made_below.is_empty() {
            return Ok(carried);
        }
        let listed: HashSet<String> = carried
            .iter()
            .map(|(rest, _)| rest.to_ascii_lowercase())
            .collect();
        for made in made_below {
            let rest = below(&made);
            if !listed.contains(&rest.to_ascii_lowercase())
                && let Some(carried_made) = self.carried_made(&made)
            {
                carried.push((rest, carried_made));
            }
        }
        Ok(carried)
    }

    /// What the operation wrote or means at `path`, below a directory it is about to move through
    /// the mount: a node written from a store, content intended, or a directory it means to make.
    fn carried_made(&self, path: &RelativePath) -> Option<Carried> {
        if let Some((repository, node)) = self.written_node(path) {
            return Some(Carried::Node(repository, node));
        }
        match self.intents.get(&RelativePathKey(path.clone())).as_deref() {
            Some(Intent::Content {
                repository,
                address,
            }) => Some(Carried::Content(repository.clone(), *address)),
            Some(Intent::Directory) => Some(Carried::MadeDirectory),
            _ => None,
        }
    }

    /// What the operation reads of the file at `child`, below a directory it is about to move
    /// through the mount, where it wrote and means nothing there itself ([`Self::carried_made`]):
    /// what it wrote to the mount, or what SWFS holds there, either of its own bytes or as the
    /// revision holds it at the path it was moved from.
    async fn carried_file(&self, child: &RelativePath) -> Result<Carried, FsError> {
        if self.wrote_to_disk(child) {
            return Ok(Carried::Mounted);
        }
        let origin = if self.view.froze_file_at(child) {
            // A file SWFS froze changed only in name or attributes holds no bytes of its own.
            if self.view.froze_bytes_at(child)? {
                return Ok(Carried::Mounted);
            }
            self.view.frozen_origin(child)
        } else {
            self.view.moved_from(child)
        };
        let Some(origin) = origin else {
            return Ok(Carried::Mounted);
        };
        Ok(
            match self
                .served_at_begin
                .resolve(&origin)
                .await
                .internal("Finding the origin of a moved file")?
            {
                Some(Served::Node {
                    repository, node, ..
                }) if !node.is_directory() => Carried::Moved(repository, node),
                _ => Carried::Mounted,
            },
        )
    }

    /// Reads the file at `path` through the mount, whole, a window at a time within the bound
    /// (ADR-00035), so SWFS fetches the bytes it holds none of while it still reads them from the
    /// path the file was moved from. Moved on again through the mount, a file SWFS never fetched
    /// is read from the backing store at the path its directory had in between, which nothing
    /// holds, and reads as zeros.
    /// `buffer`, where a fetch before left one, is read into, and handed back for the next.
    async fn fetch_through_mount(
        &self,
        path: &RelativePath,
        buffer: Option<BytesMut>,
    ) -> Result<BytesMut, FsError> {
        let (content, size) = self.mounted_content(path).await?.open_once().await?;
        let window = size.min(COPY_WINDOW as u64) as usize;
        // SAFETY: each read fills the first `want` bytes of the buffer, and nothing reads them.
        let mut buffer = match buffer {
            Some(buffer) if buffer.len() >= window => buffer,
            // Grown at least twofold, so a walk regrows it only a few times.
            Some(buffer) => unsafe {
                lore_io::uninit_buffer(window.max(2 * buffer.len()).min(COPY_WINDOW))
            },
            None => unsafe { lore_io::uninit_buffer(window) },
        };
        let mut offset = 0;
        while offset < size {
            let want = (size - offset).min(window as u64) as usize;
            buffer = content
                .read_window(WindowRead::new(buffer, 0, want), offset)
                .await?;
            offset += want as u64;
        }
        Ok(buffer)
    }

    /// Moves the operation's account of what lay below `from`, which it has moved to `to` through
    /// the mount, to `to`: what it wrote or made there, and what SWFS held there, as `carried`
    /// says it reads it. A file SWFS held as the revision holds it at the path it was moved from
    /// is noted as written from the store, so the operation reads it from there, and thaw names
    /// it, or finalizing writes it to the mount.
    ///
    /// Answers the paths at `to` the operation removed, which the move brought there.
    #[must_use]
    fn carry_below(
        &self,
        from: &RelativePath,
        to: &RelativePath,
        carried: Vec<(String, Carried)>,
    ) -> Vec<RelativePath> {
        let mut removed = Vec::new();
        self.forget_made_below(from);
        for written in self.written_to_disk_at_or_below(from) {
            self.forget_written_to_disk(&written);
        }
        for (rest, carried) in carried {
            let at = to.join(&rest);
            self.forget_intent(&from.join(&rest));
            match carried {
                Carried::Directory | Carried::Mounted => self.record_written_to_disk(&at),
                Carried::MadeDirectory => {
                    self.record_written(&at);
                    self.intend(&at, Intent::Directory);
                }
                Carried::Removed => {
                    let written = self.intend_removal(&at);
                    debug_assert!(written.is_empty(), "nothing was recorded at {at} yet");
                    removed.push(at);
                }
                Carried::Node(repository, node) | Carried::Moved(repository, node) => {
                    self.note_written_node(repository, &node, &at);
                }
                Carried::Content(repository, address) => {
                    self.record_written(&at);
                    self.intend(
                        &at,
                        Intent::Content {
                            repository,
                            address,
                        },
                    );
                }
            }
        }
        removed
    }

    /// Removes from the mount each of `removed`, paths the operation removed below a directory it
    /// then moved through the mount, which the revision at the start does not serve: realizing
    /// the removal takes one it serves off the mount, but nothing else takes one away that the
    /// move brought where the snapshot names nothing of the user's. Best effort, as the move has
    /// been made: a path left is logged. Boxed by its caller, as such a removal is rare.
    async fn remove_carried_removals(&self, removed: Vec<RelativePath>) {
        for path in &removed {
            let served = match self.served_at_begin.resolve(path).await {
                Ok(served) => served.is_some(),
                Err(error) => {
                    lore_debug!("SWFS: leaving {path}, removed before its move: {error}");
                    continue;
                }
            };
            if !served
                && self.mount_holds(path).await
                && let Err(error) = self.on_mount(|| self.os.remove_recursive(path)).await
            {
                lore_debug!("SWFS: leaving {path}, removed before its move: {error}");
            }
        }
    }

    /// Forgets everything the operation wrote or meant at `path`: the node it wrote there from a
    /// store, what it wrote to the mount, and its intent.
    fn forget_all_written_at(&self, path: &RelativePath) {
        self.forget_written(path);
        self.forget_written_to_disk(path);
        self.forget_intent(path);
    }

    /// Means `path` gone, with what lies below it, when the operation ends: realized when it
    /// finalizes, or left to the thaw. Returns the files the operation wrote to the mount itself
    /// at or below `path`, which are its caller's to remove there.
    #[must_use]
    fn intend_removal(&self, path: &RelativePath) -> Vec<RelativePath> {
        self.forget_written(path);
        // The root is never removed as an intent, so what was made or written below it stays.
        let written_to_disk = if path.is_empty() {
            Vec::new()
        } else {
            self.forget_made_below(path);
            self.written_to_disk_at_or_below(path)
        };
        self.record_written(path);
        self.intend(path, Intent::Removed);
        self.removed_trees.insert(RelativePathKey(path.clone()));
        written_to_disk
    }

    /// Removes `path` as [`InstanceOperation::remove`] does, where `held_and_emptied` says the
    /// caller has found the revision holding `path` with nothing left below it; otherwise this
    /// asks.
    async fn remove_found(
        &self,
        path: &RelativePath,
        held_and_emptied: bool,
    ) -> Result<(), FsError> {
        if !self.wrote_to_disk(path) {
            // What the mount serves, rather than holds of the operation's own making, is removed
            // as an intent, as `remove_recursive` removes, and so realized when the operation
            // finalizes or left to the description of a link or layer it lies under (ADR-00037).
            // Removed through the mount now, during the freeze, it is SWFS's own change to a name
            // touched while frozen, which no description in the thaw ending this freeze undoes,
            // hiding a link that comes to serve the name until a later operation's thaw. A
            // directory goes that way only once nothing is left in it, as a removal from disk
            // refuses one that holds something.
            if self.written_node(path).is_none()
                && (held_and_emptied
                    || (self.view.revision_holds(path).await?
                        && self.holds_nothing_below(path).await?))
            {
                // Nothing is left below it, of what the operation wrote to the mount either.
                let written = self.intend_removal(path);
                debug_assert!(written.is_empty());
                self.removed_singly.insert(RelativePathKey(path.clone()));
                return Ok(());
            }
            // A file the user holds is removed when the operation finalizes: by naming it in the
            // thaw where no revision holds it, or through the mount where one does, and not
            // before, where a directory the revision holds there replaces it in that thaw.
            // Removed through the mount now it would be the user's own deletion, which the thaw
            // keeps.
            if self.view.froze_file_at(path) {
                // A file, which the operation did not write to the mount itself.
                let written = self.intend_removal(path);
                debug_assert!(written.is_empty());
                return Ok(());
            }
        }
        self.on_mount(|| self.os.remove(path)).await?;
        self.operated.insert(RelativePathKey(path.clone()));
        self.removed_trees.insert(RelativePathKey(path.clone()));
        self.forget_all_written_at(path);
        // Thaw describes a removed path as holding nothing, which hides what the revision holds
        // there. A path it does not hold has nothing to hide, and SWFS turns the description into
        // an empty file where it had one of its own, such as a copy a merge made and removed.
        if self.view.revision_holds(path).await? {
            self.record_written(path);
        }
        Ok(())
    }

    /// Realizes on the mount each path whose intent the state thaw describes from does not hold,
    /// and leaves it out of what thaw describes (ADR-00033). Thaw describes a path from the
    /// current revision, so a result the revision does not hold, such as the other side of a
    /// resolved conflict, would otherwise be described, and served, as the revision holds it.
    async fn realize_what_the_revision_does_not_hold(&self) -> Result<(), FsError> {
        let resources = self.mount.resources();
        resources
            .update_state(SwfsExecutionToken {})
            .await
            .internal("Loading the state thaw describes from")?;
        // What follows asks the record whether SWFS holds a path changed: the frozen snapshot
        // cannot say for a deletion, which SWFS does not freeze, so an untrusted record is rebuilt
        // first. It then holds what SWFS holds before the thaw, and drops what the thaw names, as
        // SWFS does.
        // The operation's work is done by now, a commit's stored, so a record that cannot be
        // rebuilt is left for the next operation to rebuild or fail on, rather than failing this.
        if let Err(error) = rebuild_untrusted_record(&self.mount).await {
            lore_warn!("Unable to rebuild the modified-file record before the thaw: {error}");
        }
        let served = resources.served_tree();
        let state = resources.current_read_state();
        if state.is_some() {
            self.name_unchanged_files(&served).await?;
        }
        // The links and layers that changed since the mount last described them are described by
        // their mount paths, which refreshes or drops all below them in one entry and keeps the
        // user's changes there (ADR-00037).
        let mut changed_mounts = resources
            .described_tree()
            .map_or_default(|described| mount_changes(&described, &served));
        // A file the user holds where a removed mount stood is theirs: naming the path would delete
        // it.
        changed_mounts.retain(|(mount, change)| {
            !(matches!(change, MountChange::Removed) && self.view.froze_file_at(mount))
        });
        // Collected first, so no shard guard is held across an await.
        let mut intents: Vec<(RelativePath, Intent)> = self
            .intents
            .iter()
            .map(|entry| (entry.key().0.clone(), entry.value().clone()))
            .collect();
        // Removals last, deepest first, so a directory removed alone is empty by then.
        intents.sort_by_key(|(path, intent)| match intent {
            Intent::Removed => (1, std::cmp::Reverse(path.as_str().len())),
            _ => (0, std::cmp::Reverse(0)),
        });
        for (path, intent) in intents {
            let description = match intent {
                Intent::Removed => {
                    self.realize_removal(&path, &changed_mounts, &served, state.is_some())
                        .await?
                }
                Intent::Content {
                    repository,
                    address,
                } => {
                    self.realize_made(
                        &path,
                        Made::Content {
                            repository,
                            address,
                        },
                        &changed_mounts,
                        &served,
                        state.is_some(),
                    )
                    .await?
                }
                Intent::Directory => {
                    self.realize_made(
                        &path,
                        Made::Directory,
                        &changed_mounts,
                        &served,
                        state.is_some(),
                    )
                    .await?
                }
            };
            if matches!(description, Description::Dropped) {
                self.touched_nodes.remove(&RelativePathKey(path));
            }
        }
        if state.is_some() {
            self.keep_unwritten_changes().await?;
        }
        if !changed_mounts.is_empty() {
            self.describe_changed_mounts(&changed_mounts).await?;
        }
        Ok(())
    }

    /// Realizes the operation's removal of `path` on the mount, or leaves it to the thaw, against
    /// `served`, the tree the thaw describes from, with `changed_mounts` the links and layers
    /// described by their mount paths, and `has_revision` whether there is a current revision to
    /// describe from.
    async fn realize_removal(
        &self,
        path: &RelativePath,
        changed_mounts: &[(RelativePath, MountChange)],
        served: &ServedTree,
        has_revision: bool,
    ) -> Result<Description, FsError> {
        // A removal at or below a changed mount path is described rather than made through the
        // mount, where SWFS would keep it as the user's own deletion until a later thaw names it
        // and its directory again: the one ending this freeze cannot. At or below a removed mount
        // path the mount path's description drops it; below an added or repinned one, the path's
        // own (see `describe_changed_mounts`).
        match mount_holding(changed_mounts, path) {
            Some((mount, MountChange::Removed)) => {
                if mount.as_str().eq_ignore_ascii_case(path.as_str()) {
                    self.removed_subtrees.lock().push(mount.clone());
                }
                return Ok(Description::Kept);
            }
            // What SWFS holds of the user's or an earlier operation's writes below it stays through
            // the refresh of the mount path, so each such file is named, which deletes it.
            Some(_) => {
                self.name_users_files_below(path).await;
                self.mount
                    .resources()
                    .note_removed_below_mount(path.clone());
                return Ok(Description::Kept);
            }
            None => {}
        }
        let now = served
            .resolve(path)
            .await
            .internal("Finding a removed path's node")?;
        // A path `remove` took, rather than `remove_recursive`, is described as holding nothing
        // where the new tree no longer serves it, as a sync or branch switch removes what its
        // target lacks, and the description drops it: made through the mount it would be SWFS's
        // own deletion, of whatever SWFS still lists below it too. A file SWFS holds of the user's
        // there goes the same way, since naming it deletes it. A directory the user made stays,
        // which no thaw removes. Where the new tree still serves it, as removing a tracked file or
        // narrowing a view leaves the revision holding it, only a removal on the mount takes it,
        // made alone as from disk: the working tree's own change.
        if self.removed_singly.contains(&RelativePathKey(path.clone())) {
            if now.is_some()
                && self.mount_holds(path).await
                && let Err(error) = self.on_mount(|| self.os.remove(path)).await
            {
                lore_debug!("SWFS: removing {path} from the mount: {error}");
            }
            return Ok(Description::Kept);
        }
        // A tree `remove_recursive` took that the mount served, and that the new tree no longer
        // serves, is described as holding nothing, which drops it in one entry; what SWFS holds of
        // the user's below it is named too, each file on its own, so the same thaw deletes it.
        // Removed through the mount, every directory in it would be listed through the mount to
        // find what to delete. Where the new tree still serves it, the removal is the working
        // tree's own change, which only a removal on the mount makes.
        if now.is_none()
            && self
                .served_at_begin
                .resolve(path)
                .await
                .internal("Finding a removed path's node")?
                .is_some()
        {
            self.removed_subtrees.lock().push(path.clone());
            return Ok(Description::Kept);
        }
        // Thaw describes from the served tree, which below a link or layer is the repository
        // mounted there. With no revision it describes nothing. What the mount holds of its own,
        // such as a directory the user made, stays there whatever thaw describes, so what the
        // served tree holds never makes a removal the thaw's.
        match now.filter(|_| has_revision) {
            // A path the new tree does not serve, which holds only what the user made: each file
            // of the user's is named, and the thaw deletes it. The path itself stays named only
            // where it is such a file, since naming a directory the user made does nothing. Where
            // the new tree serves it, the removal is the working tree's own change and goes to the
            // mount.
            None => {
                self.name_users_files_below(path).await;
                if self.view.froze_file_at(path) {
                    return Ok(Description::Kept);
                }
            }
            Some(held) => {
                // A directory the new tree serves where the user holds a file is described to the
                // thaw, which replaces the file.
                if self.view.froze_file_at(path) && held.is_directory() {
                    return Ok(Description::Kept);
                }
                if self.mount_holds(path).await {
                    if self.writes_below(path) {
                        Box::pin(self.remove_tree_keeping_what_was_made(path)).await?;
                    } else {
                        self.on_mount(|| self.os.remove_recursive(path)).await?;
                    }
                }
            }
        }
        Ok(Description::Dropped)
    }

    /// Removes from the mount what the directory at `path` holds, and the directory, but what the
    /// operation wrote or made below it after removing it, such as a merge's copy of a conflicted
    /// file, and the directories leading there: removed whole, the directory would take them
    /// with it. Each entry is removed on its own, as [`Self::remove_unmodified_tree`] removes, and
    /// what the operation wrote to the mount, a directory moved there included, is kept whole.
    /// Names are matched in the spelling the operation wrote them in. Boxed by its caller, as a
    /// directory is rarely written into after it was removed.
    async fn remove_tree_keeping_what_was_made(&self, path: &RelativePath) -> Result<(), FsError> {
        let mut pending = vec![path.clone()];
        let mut directories = Vec::new();
        // The directories holding something kept, which stay.
        let mut keeping = HashSet::new();
        let mut keep = |directory: &RelativePath| {
            let mut at = directory.clone();
            while keeping.insert(at.as_str().to_owned()) && at != *path {
                at = at.parent_path();
            }
        };
        while let Some(directory) = pending.pop() {
            let entries = self.mounted_entries(&directory).await?;
            directories.push(directory.clone());
            let mut keeps_something = false;
            for (name, is_directory) in &entries {
                let child = directory.join(name);
                // What is listed as no directory may be a symbolic link or junction the
                // operation wrote through.
                let kept = if *is_directory {
                    self.wrote_to_disk(&child)
                } else {
                    self.makes_at(&child) || self.wrote_to_disk_below(&child)
                };
                if kept {
                    keeps_something = true;
                } else if *is_directory {
                    pending.push(child);
                } else {
                    // A symbolic link or junction is removed itself, not what it leads to.
                    self.on_mount(|| self.os.remove(&child)).await?;
                }
            }
            if keeps_something {
                keep(&directory);
            }
        }
        for directory in directories.into_iter().rev() {
            if !keeping.contains(directory.as_str())
                && !self.makes_at(&directory)
                && !self.intended_children.makes_below(&directory)
            {
                self.on_mount(|| self.os.remove_empty_directory(&directory))
                    .await?;
            }
        }
        Ok(())
    }

    /// Realizes on the mount what the operation `made` at `path`, unless `served`, the tree the
    /// thaw describes from, holds it, with `changed_mounts` the links and layers described by
    /// their mount paths, and `has_revision` whether there is a current revision to describe from.
    async fn realize_made(
        &self,
        path: &RelativePath,
        made: Made,
        changed_mounts: &[(RelativePath, MountChange)],
        served: &ServedTree,
        has_revision: bool,
    ) -> Result<Description, FsError> {
        let resources = self.mount.resources();
        // A directory the operation means at a removed mount path, as removing a link staged for
        // add leaves one, is made once the thaw has dropped the rest.
        let directory = matches!(made, Made::Directory);
        if directory
            && let Some((mount, MountChange::Removed)) = mount_holding(changed_mounts, path)
            && mount.as_str().eq_ignore_ascii_case(path.as_str())
        {
            self.recreated_mounts.lock().push(mount.clone());
            return Ok(Description::Kept);
        }
        // Thaw describes from the served tree, which below a link or layer is the repository
        // mounted there. With no revision it describes nothing.
        let held = if has_revision {
            served
                .resolve(path)
                .await
                .internal("Finding a written path's node")?
        } else {
            None
        };
        let holds = match (&made, &held) {
            (Made::Content { address, .. }, Some(Served::Node { node, .. })) => {
                !node.is_directory() && node.address.hash == address.hash
            }
            (Made::Directory, Some(served)) => served.is_directory(),
            _ => false,
        };
        // What is served leaves out what the working tree does not hold, as the view it was
        // loaded with sees it; a widening view is written only after the sync that applies it, so
        // what it brings in goes to the mount (ADR-00034).
        let holds = holds && !resources.hides(path, directory);
        // What the served tree holds is left to the thaw, a path the user deleted on the mount
        // included: the thaw names the path and every directory leading to it, which brings it
        // back from the source. Written through the mount, it would be a copy SWFS holds as the
        // user's own change. A deletion made while frozen is the only one a thaw cannot undo, and
        // only the one ending that freeze: a later operation's restores it.
        if holds {
            // The record keeps the user's deletion, which SWFS reports no change to undo: noted
            // as made again, as a file SWFS reports created is, the next diff compares what the
            // thaw brings back with the revision rather than taking it as deleted.
            let modified = resources.modified();
            if modified
                .coverage(path.as_str())
                .await
                .internal("Asking the modified-file record")?
                == Coverage::Deleted
            {
                modified
                    .record(Change::Created {
                        path: path.as_str().to_owned(),
                        directory,
                    })
                    .await;
            }
            return Ok(Description::Kept);
        }
        match made {
            Made::Content {
                repository,
                address,
            } => {
                self.write_stored_to_mount(repository, address, path)
                    .await?;
            }
            Made::Directory => self.on_mount(|| self.os.create_dir_all(path)).await?,
        }
        Ok(Description::Dropped)
    }

    /// Names in the thaw each file [`Self::unchanged`] holds where `served`, the tree the thaw
    /// describes from, holds the content the file was found to hold, as a reset of the file names
    /// it: the thaw takes it off SWFS's list, and the record drops it, so a later scan no longer
    /// compares it. What the thaw names it as is what the user holds, and SWFS keeps a write made
    /// since the freeze over it. A path the operation acted on is left to what it did there.
    async fn name_unchanged_files(&self, served: &ServedTree) -> Result<(), FsError> {
        let unchanged: Vec<(RelativePath, Address, u64)> = self
            .unchanged
            .iter()
            .map(|entry| (entry.key().0.clone(), entry.value().0, entry.value().1))
            .collect();
        self.unchanged.clear();
        let resources = self.mount.resources();
        for (path, address, size) in unchanged {
            if self.operated.contains(&RelativePathKey(path.clone()))
                || resources.hides(&path, false)
            {
                continue;
            }
            let holds = matches!(
                served
                    .resolve(&path)
                    .await
                    .internal("Finding an unchanged file's node")?,
                Some(Served::Node { node, .. })
                    if !node.is_directory() && node.address.hash == address.hash && node.size == size
            );
            if holds
                && resources
                    .modified()
                    .coverage(path.as_str())
                    .await
                    .internal("Asking the modified-file record")?
                    == Coverage::Changed
            {
                self.touched_nodes.insert(RelativePathKey(path));
            }
        }
        Ok(())
    }

    /// Describes each changed mount path in place of the paths below it that SWFS holds no change
    /// to: naming a directory relists it and every level SWFS listed below it, refreshing what it
    /// holds unmodified, dropping what the source no longer holds and bringing back what it holds
    /// again. A path below it stays described on its own where SWFS holds it changed, since the
    /// refresh leaves a changed file as it is until a thaw names the file itself (`swfsThaw` in
    /// `swfs.h`), as an operation restoring it means it to be.
    async fn describe_changed_mounts(
        &self,
        changed_mounts: &[(RelativePath, MountChange)],
    ) -> Result<(), FsError> {
        let below: Vec<RelativePath> = self
            .touched_nodes
            .iter()
            .filter(|path| mount_holding(changed_mounts, &path.0).is_some())
            .map(|path| path.0.clone())
            .collect();
        for path in below {
            if !self.swfs_holds_changed(&path).await? {
                self.touched_nodes.remove(&RelativePathKey(path));
            }
        }
        for (mount, _) in changed_mounts {
            self.touched_nodes.insert(RelativePathKey(mount.clone()));
        }
        Ok(())
    }

    /// Names, so the thaw deletes it, each file SWFS holds of the user's at and below `path` that
    /// the new tree does not serve. Naming a directory leaves what the user holds below it, and no
    /// thaw removes a directory the user made, so the files are named one by one from what SWFS
    /// froze, and a directory the user made stays. What the new tree serves at `path`, such as the
    /// folder a branch holds where another holds a link, is the working tree's, not the user's, and
    /// stays too.
    async fn name_users_files_below(&self, path: &RelativePath) {
        let paths = match self.mount.resources().modified().paths().await {
            Ok(paths) => paths,
            Err(error) => {
                lore_warn!("Unable to list the user's files below {path}: {error}");
                return;
            }
        };
        let served = self.mount.resources().served_tree();
        // The record may not hold a file SWFS froze, such as one written before the record was
        // rebuilt for a new revision.
        let mut candidates: Vec<RelativePath> = paths
            .into_iter()
            .filter(|below| at_or_below(below, path))
            .collect();
        candidates.extend(self.view.frozen_files_below(path));
        candidates.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        candidates.dedup_by(|left, right| left.as_str() == right.as_str());
        for below in candidates {
            if !self.view.froze_file_at(&below) {
                continue;
            }
            let serves = served
                .resolve(&below)
                .await
                .is_ok_and(|served| served.is_some());
            if !serves {
                self.record_written(&below);
            }
        }
    }

    /// Where the operation moved a link or layer to another revision, or, having said it leaves
    /// changes unwritten ([`Self::leaves_changes_unwritten`]), the instance, keeps what it did not
    /// write as the working tree held it (ADR-00034, ADR-00037). On a plain filesystem a file an
    /// operation does not write stays as it is; an SWFS mount serves what the instance now
    /// stores, so each path the two served trees differ at that the operation left alone, that the
    /// view includes and that SWFS holds no change to, is kept: a path the old tree served gets
    /// that content written to the mount, from the repository that served it, and one only the
    /// new tree serves is recorded as absent and no longer served. Any other operation that moves
    /// the instance writes every change of the revision, so the revisions are not diffed.
    async fn keep_unwritten_changes(&self) -> Result<(), FsError> {
        let resources = self.mount.resources();
        let old = self.served_at_begin.clone();
        let new = resources.served_tree();
        let written: Vec<RelativePath> = self
            .operated
            .iter()
            .map(|entry| entry.key().0.clone())
            .collect();
        let changes = served_changes(
            &old,
            &new,
            &resources.served_filter(),
            self.leaves_changes_unwritten.load(Ordering::Acquire),
            &self.revision_diffs,
        )
        .await?;
        if changes.is_empty() {
            resources.update_absent(&[], &written);
            return Ok(());
        }

        // Left to the operation: a path it acted on, a directory holding one, and anything under a
        // tree it removed. What lies under a directory it only created is not. Looked up as sets
        // of lowercase paths, since a commit of many files names each of them.
        let mut written_at_or_above = std::collections::HashSet::new();
        for path in &written {
            let mut ancestor = path.clone();
            while written_at_or_above.insert(ancestor.as_str().to_ascii_lowercase())
                && !ancestor.is_empty()
            {
                ancestor = ancestor.parent_path();
            }
        }
        let removed: std::collections::HashSet<String> = self
            .removed_trees
            .iter()
            .map(|entry| entry.key().0.as_str().to_ascii_lowercase())
            .collect();
        let is_written = |path: &RelativePath| {
            if written_at_or_above.contains(&path.as_str().to_ascii_lowercase()) {
                return true;
            }
            let mut ancestor = path.clone();
            while !ancestor.is_empty() {
                ancestor = ancestor.parent_path();
                if removed.contains(&ancestor.as_str().to_ascii_lowercase()) {
                    return true;
                }
            }
            false
        };
        let mut absent = Vec::new();
        for change in changes {
            let path = change.path().clone();
            if path.is_empty() || is_written(&path) {
                continue;
            }
            let held = old
                .resolve(&path)
                .await
                .internal("Finding a path in the tree the operation left")?;
            let is_directory = held.as_ref().map_or(
                !change
                    .resolved_side()
                    .flags
                    .contains(crate::node::NodeFlags::File),
                Served::is_directory,
            );
            if resources.hides(&path, is_directory) || self.swfs_holds_changed(&path).await? {
                continue;
            }
            // The mount still serves what the old tree held until the thaw, so a path it holds nothing
            // at was removed from it: by the user, whose deletion SWFS keeps, or by this operation,
            // such as the origin of a move. Writing it back would undo the removal.
            if held.is_some() && !self.mount_holds(&path).await {
                continue;
            }
            match held {
                None => absent.push(path.clone()),
                Some(Served::Node {
                    repository, node, ..
                }) if !node.is_directory() => {
                    self.write_stored_to_mount(repository, node.address, &path)
                        .await?;
                }
                Some(_) => self.on_mount(|| self.os.create_dir_all(&path)).await?,
            }
            self.touched_nodes.remove(&RelativePathKey(path));
        }
        resources.update_absent(&absent, &written);
        Ok(())
    }

    /// Notes that this operation wrote or removed `path`, so thaw describes it to SWFS. The root is
    /// never written as such.
    fn record_written(&self, path: &RelativePath) {
        if !path.is_empty() {
            self.operated.insert(RelativePathKey(path.clone()));
            self.touched_nodes.insert(RelativePathKey(path.clone()));
        }
    }

    /// Notes that the mount at `path` is to be described again on finalizing, so that the state it
    /// is loaded from is read as it is then.
    pub(crate) fn mount_changed(&self, path: &RelativePath) {
        if !path.is_empty() {
            self.touched_nodes.insert(RelativePathKey(path.clone()));
        }
    }

    /// Notes that the operation may leave changes of the revision it moves the instance to
    /// unwritten, which finalizing then keeps as the working tree showed them (ADR-00034).
    #[lore_macro::test_pub]
    pub(crate) fn leaves_changes_unwritten(&self) {
        self.leaves_changes_unwritten.store(true, Ordering::Release);
    }

    /// Notes that a commit stored `path`, which the thaw then describes as the revision it
    /// committed holds it.
    pub(crate) fn committed_path(&self, path: &RelativePath) {
        if !path.is_empty() {
            self.committed.insert(RelativePathKey(path.clone()));
        }
    }

    /// Notes that a commit deleted the file at `path`. The thaw then names it as holding nothing
    /// where SWFS lists it as deleted, or names the deleted directory it lay in, where the
    /// revision it committed holds nothing there and SWFS froze nothing there since: what it
    /// froze is the user's.
    pub(crate) fn committed_removal(&self, path: &RelativePath) {
        if !path.is_empty() {
            self.committed_removed.insert(RelativePathKey(path.clone()));
        }
    }

    /// Notes that a commit deleted the directory at `path` and all it held. The thaw names what SWFS
    /// lists as deleted there, the directory and each path below it, and nothing where SWFS lists no
    /// deletion: an entry naming a path SWFS does not hold deleted makes an empty file of it.
    pub(crate) fn committed_tree_removal(&self, path: &RelativePath) {
        if !path.is_empty() {
            self.committed_removed_trees
                .insert(RelativePathKey(path.clone()));
        }
    }

    /// The file this operation wrote at `path` from a store, and the repository it is in.
    fn written_node(&self, path: &RelativePath) -> Option<(Arc<RepositoryContext>, Node)> {
        self.written_nodes
            .get(&RelativePathKey(path.clone()))
            .map(|written| (written.repository.clone(), written.node))
    }

    /// Forgets what this operation wrote at `path`, which it has since removed or moved.
    fn forget_written(&self, path: &RelativePath) {
        self.written_nodes.remove(&RelativePathKey(path.clone()));
    }

    /// Notes that this operation wrote `path` to the mount, which holds the only copy of what it
    /// wrote until the next operation, and that it is no longer a node written from the store.
    // The entry's shard lock is held for one count, with no await while it is.
    #[allow(clippy::disallowed_methods)]
    fn record_written_to_disk(&self, path: &RelativePath) {
        self.forget_written(path);
        self.forget_intent(path);
        if path.is_empty() {
            return;
        }
        self.operated.insert(RelativePathKey(path.clone()));
        if self.written_to_disk.insert(RelativePathKey(path.clone())) {
            *self
                .disk_directories
                .entry(RelativePathKey(path.parent_path()))
                .or_default() += 1;
            self.count_disk_ancestors(path, true);
        }
    }

    /// Counts `path`, written to the mount, in each directory above it, or takes the count back
    /// where `added` is false.
    // The entry's shard lock is held for one count, with no await while it is.
    #[allow(clippy::disallowed_methods)]
    fn count_disk_ancestors(&self, path: &RelativePath, added: bool) {
        let mut ancestor = path.clone();
        while !ancestor.is_empty() {
            ancestor = ancestor.parent_path();
            let key = RelativePathKey(ancestor.clone());
            if added {
                *self.disk_ancestors.entry(key).or_default() += 1;
                continue;
            }
            if let Some(mut count) = self.disk_ancestors.get_mut(&key) {
                *count -= 1;
            }
            self.disk_ancestors.remove_if(&key, |_, count| *count == 0);
        }
    }

    /// Forgets that this operation wrote `path` to the mount, which it has since removed or moved
    /// there.
    fn forget_written_to_disk(&self, path: &RelativePath) {
        if self
            .written_to_disk
            .remove(&RelativePathKey(path.clone()))
            .is_some()
        {
            let directory = RelativePathKey(path.parent_path());
            if let Some(mut count) = self.disk_directories.get_mut(&directory) {
                *count -= 1;
            }
            self.disk_directories
                .remove_if(&directory, |_, count| *count == 0);
            self.count_disk_ancestors(path, false);
        }
    }

    /// Removes from the mount each of `written`, what the operation wrote there itself and has
    /// since removed, with what lies below it, deepest first, and forgets each it removed: the
    /// thaw describes nothing of them, so where no revision holds their paths, nothing else takes
    /// them off the mount. One that cannot be removed is still the operation's, so removing it
    /// again tries again; the first failure is returned once every one was tried. Boxed by its
    /// caller, as a path is rarely removed after the operation wrote to the mount below it.
    async fn remove_on_mount(&self, mut written: Vec<RelativePath>) -> Result<(), FsError> {
        written.sort_by_key(|path| std::cmp::Reverse(path.as_str().len()));
        let mut failed = None;
        for path in &written {
            match self.on_mount(|| self.os.remove_recursive(path)).await {
                Ok(()) => self.forget_written_to_disk(path),
                Err(error) => {
                    failed.get_or_insert(error);
                }
            }
        }
        failed.map_or(Ok(()), Err)
    }

    /// Forgets what the operation meant to make below `path`, and the files it wrote there from a
    /// store, which it has since removed with `path`: what it makes there later counts again.
    /// Found through [`Self::intended_children`], so it costs what the operation made below
    /// `path`, not what it made elsewhere.
    fn forget_made_below(&self, path: &RelativePath) {
        for child in self.made_below(path) {
            self.forget_written(&child);
            let makes = self
                .intents
                .get(&RelativePathKey(child.clone()))
                .is_some_and(|intent| intent.makes());
            if makes {
                self.forget_intent(&child);
            }
        }
    }

    /// Each path below `path` through which the operation means to make something, found through
    /// [`Self::intended_children`], so it costs what the operation made below `path`, not what it
    /// made elsewhere.
    fn made_below(&self, path: &RelativePath) -> Vec<RelativePath> {
        let mut made = Vec::new();
        if !self.intended_children.makes_below(path) {
            return made;
        }
        let mut pending = vec![path.clone()];
        while let Some(directory) = pending.pop() {
            if !self.intended_children.makes_below(&directory) {
                continue;
            }
            for name in self.intended_children.children(&directory) {
                let child = directory.join(&name);
                made.push(child.clone());
                pending.push(child);
            }
        }
        made
    }

    /// What this operation wrote to the mount at or below `path`, looked through only where it
    /// wrote something there.
    fn written_to_disk_at_or_below(&self, path: &RelativePath) -> Vec<RelativePath> {
        if !self.wrote_to_disk(path) && !self.wrote_to_disk_below(path) {
            return Vec::new();
        }
        self.written_to_disk
            .iter()
            .filter(|entry| rest_below_exactly(entry.0.as_str(), path.as_str()).is_some())
            .map(|entry| entry.0.clone())
            .collect()
    }

    /// Whether this operation wrote `path` to the mount.
    fn wrote_to_disk(&self, path: &RelativePath) -> bool {
        self.written_to_disk
            .contains(&RelativePathKey(path.clone()))
    }

    /// Whether this operation wrote something to the mount directly in the directory at `path`.
    fn wrote_to_disk_in(&self, path: &RelativePath) -> bool {
        self.disk_directories
            .contains_key(&RelativePathKey(path.clone()))
    }

    /// Whether a link or layer stands at another state than the mount last described, as loaded
    /// again now.
    async fn mounts_moved(&self) -> bool {
        let resources = self.mount.resources();
        let described = resources.described_tree();
        let has_mounts = self.served_at_begin.has_mounts()
            || described.as_ref().is_some_and(|tree| tree.has_mounts());
        if !has_mounts || resources.update_state(SwfsExecutionToken {}).await.is_err() {
            return false;
        }
        described.is_some_and(|described| {
            !mount_changes(&described, &resources.served_tree()).is_empty()
        })
    }

    /// The paths SWFS lists as deleted (`swfsGetModifiedFileList`), as a thaw entry naming one
    /// commits it. `None` where SWFS cannot list them: nothing the commit removed is then named,
    /// as an entry naming a path SWFS does not list as deleted makes an empty file of it. The
    /// thaw that follows lists them again to describe what the operation wrote, and fails where
    /// it cannot either.
    fn deletions_swfs_lists(&self) -> Option<ListedDeletions> {
        let listed = match self.mount.modified_files() {
            Ok(listed) => listed,
            Err(error) => {
                lore_warn!(
                    "Unable to list the deletions SWFS holds, so the thaw names nothing the \
                     commit removed: {error}"
                );
                return None;
            }
        };
        Some(
            listed
                .iter()
                .filter(|file| file.mod_flags & SWFSNodeModFlag_Deleted != 0)
                .filter_map(|file| SwfsPath(&file.path).node_path().ok())
                .map(|path| (path.0.as_lowercase_str().to_owned(), path.0))
                .collect(),
        )
    }

    /// Notes in `removals.held_deleted` each path the thaw is to describe that SWFS holds deleted,
    /// other than one a commit removed, which the thaw names: a path
    /// SWFS holds deleted, as a directory it keeps listed after the user deleted it, is not named
    /// in the thaw, whose entry would make it something that exists. Asks the record as the
    /// operation left it: called before realizing, which may rebuild an untrusted record and
    /// notes as created a deleted path it leaves the thaw to bring back. Loading the new revision
    /// moves the record rather than emptying it, as [`ModifiedTree::begin_thaw`] has begun the
    /// thaw.
    async fn note_held_deleted(&self, removals: &mut ThawRemovals) {
        let paths: Vec<RelativePath> = self
            .touched_nodes
            .iter()
            .map(|path| path.0.clone())
            .collect();
        let record = self.mount.resources().modified().clone();
        for path in paths {
            let key = RelativePathKey(path.clone());
            if removals.held_deleted.contains(&key) || removals.committed.contains(&key) {
                continue;
            }
            if record
                .coverage(path.as_str())
                .await
                .is_ok_and(|coverage| coverage == Coverage::Deleted)
            {
                removals.held_deleted.insert(key);
            }
        }
    }

    /// What an operation that made changes describes once it has realized what the revision does
    /// not hold, `realized` saying whether that succeeded: the user's files below the removed
    /// subtrees named, where it did, and what the view leaves out dropped, whether or not it did.
    /// Returns how many paths the thaw describes, before the directories leading to them.
    async fn settle_description(&self, realized: bool) -> usize {
        // Named in the thaw that ends this freeze, which deletes them, rather than deleted through
        // the mount, where each would be the user's own change.
        if realized {
            let subtrees = self.removed_subtrees.lock().clone();
            for subtree in &subtrees {
                self.name_users_files_below(subtree).await;
            }
        }
        // What the view leaves out is not on the mount, so there is nothing there to describe, and
        // describing it would resolve it in a tree whose links under it were never fetched. A
        // directory the view only leaves out as a file, as one it reaches into for what it
        // re-includes, stays.
        let resources = self.mount.resources();
        let served = resources.served_tree();
        let touched: Vec<RelativePath> = self
            .touched_nodes
            .iter()
            .map(|path| path.0.clone())
            .collect();
        for path in touched {
            if !resources.view_hides(&path, false) {
                continue;
            }
            let directory = !resources.view_hides(&path, true)
                && served
                    .resolve(&path)
                    .await
                    .is_ok_and(|served| served.is_some_and(|served| served.is_directory()));
            if !directory {
                self.touched_nodes.remove(&RelativePathKey(path));
            }
        }
        // Before the directories leading to them, which the thaw adds.
        let described = self.touched_nodes.len();
        lore_debug!(
            "SWFS operation describes {described} paths, starting {:?}",
            self.touched_nodes
                .iter()
                .take(20)
                .map(|path| path.0.as_str().to_owned())
                .collect::<Vec<_>>()
        );
        described
    }

    /// Realizes what the revision does not hold of the operation's results, then thaws the frozen
    /// filesystem, clearing the write cache where the operation wrote.
    ///
    /// Always attempts the thaw, whatever failed before it, as every caller that begins an
    /// operation finalizes it, [`with_operation`] among them: an operation's freeze is ended here
    /// or, as a last resort where SWFS couldn't finish the thaw, by the next command's freeze.
    /// Where the files the operation wrote can't be described, the thaw applies none and the
    /// failure is returned.
    ///
    /// [`with_operation`]: crate::fs::filesystem_provider::with_operation
    #[lore_macro::test_pub]
    pub(crate) async fn finalize(
        &self,
        changes_made: bool,
        succeeded: bool,
    ) -> Result<(), FsError> {
        // Handed to the thaw, which hands it back once SWFS has finished, so the work on the mount
        // after the thaw, a second thaw among it, is this operation's too.
        let operation = self.operation.lock().take();
        // What a failed operation found unchanged is not named: it named nothing else either. One
        // that moved the instance, as a clone stores its revision whether or not it wrote it all,
        // may have stopped short of writing its changes.
        if !succeeded {
            self.unchanged.clear();
            self.leaves_changes_unwritten();
        }
        // From here the state is loaded again, at the revision the operation leaves, which moves
        // the modified-file record there for the thaw to bring up to date rather than emptying it.
        self.mount.resources().modified().begin_thaw();
        // A link or layer another repository now stands at, which a command stores its state for
        // after the operation realizing it ran, is described again whether or not this operation
        // wrote anything.
        let changes_made = changes_made || self.mounts_moved().await || !self.unchanged.is_empty();
        // What a commit stored, named in the thaw only if the commit went through: SWFS takes a path a
        // thaw names off its list of what is modified, and a failed commit changed no revision, so
        // the file is still the user's edit.
        let committed: Vec<RelativePath> =
            self.committed.iter().map(|path| path.0.clone()).collect();
        self.committed.clear();
        let committed_removed: Vec<RelativePath> = self
            .committed_removed
            .iter()
            .map(|path| path.0.clone())
            .collect();
        self.committed_removed.clear();
        let committed_removed_trees: Vec<RelativePath> = self
            .committed_removed_trees
            .iter()
            .map(|path| path.0.clone())
            .collect();
        self.committed_removed_trees.clear();
        // What the operation removed from the mount itself is no more a path to name than one SWFS
        // holds deleted: an entry naming it reads to SWFS as something that exists.
        let mut removals = ThawRemovals::default();
        for removed in self.removed_on_mount.iter() {
            removals.held_deleted.insert(removed.clone());
        }
        if succeeded {
            // A file the commit took in renamed leaves SWFS listing where it came from as deleted,
            // which pairs with its next rename as its origin, a path the revision may no longer
            // hold: SWFS then reads its bytes from a source that holds none. Named, the origin
            // comes off the list. An origin the user made something at again is not listed, as
            // SWFS drops the deletion then.
            let origins = self.view.origins_of_renamed_files(&committed);
            // Only a deletion SWFS lists is committed by an entry naming it. Any other, as one an
            // earlier operation's thaw made, or the merge resolve that removed a file the merge
            // wrote, has nothing for the entry to clear, and SWFS makes it an empty file. What
            // SWFS froze at a path the commit deleted, as a file the user made there since the
            // deletion was staged, is the user's, and naming the path would drop it.
            let anything_removed = !committed_removed.is_empty()
                || !origins.is_empty()
                || !committed_removed_trees.is_empty();
            if anything_removed && let Some(listed) = self.deletions_swfs_lists() {
                name_committed_removals(
                    &mut removals,
                    &listed,
                    committed_removed.into_iter().chain(origins),
                    &committed_removed_trees,
                    |path| self.view.frozen_info(path).is_some(),
                );
                for named in &removals.committed {
                    self.record_written(&named.0);
                }
            }
            for path in committed {
                self.record_written(&path);
            }
        }
        // Thawed whether or not realizing succeeds, so a failure never leaves the mount frozen.
        let realized = if changes_made {
            self.note_held_deleted(&mut removals).await;
            self.realize_what_the_revision_does_not_hold().await
        } else {
            Ok(())
        };
        let described = if changes_made {
            self.settle_description(realized.is_ok()).await
        } else {
            0
        };
        let token = SwfsExecutionToken {};
        let Some(operation) = operation else {
            self.mount.resources().modified().end_thaw(None).await;
            return Err(FsError::internal("SWFS operation already thawed"));
        };
        // Held from here until this returns, on every path.
        let (result, _operation) = match self
            .mount
            .thaw(
                token,
                changes_made,
                &self.touched_nodes,
                &removals,
                operation,
            )
            .await
        {
            Ok(operation) => (Ok(()), Some(operation)),
            Err(error) => (Err(error), None),
        };
        // Whatever the outcome, none of these writes are pending any longer.
        self.touched_nodes.clear();
        self.intents.clear();
        self.intended_children.clear();
        self.removed_children.clear();
        let removed_subtrees = std::mem::take(&mut *self.removed_subtrees.lock());
        let recreated_mounts = std::mem::take(&mut *self.recreated_mounts.lock());
        realized?;
        if result.is_ok() {
            // What the thaw could not remove below a removed mount, as the directories the mount made
            // itself, a removed link's mount path recreated earlier, goes with the mount, unless it
            // is the user's: a file SWFS froze, and the directory holding it, stay.
            for subtree in &removed_subtrees {
                if recreated_mounts.contains(subtree) || !self.mount_holds(subtree).await {
                    continue;
                }
                self.remove_unmodified_tree(subtree).await;
                // The removal is the mount's own, which the record hears of from SWFS after this thaw
                // and which a later thaw describing the path again undoes, so the record is rebuilt.
                self.mount.resources().modified().distrust();
            }
            for mount in &recreated_mounts {
                if let Err(error) = self.on_mount(|| self.os.create_dir_all(mount)).await {
                    lore_warn!(
                        "Unable to make the directory {mount} a removed link leaves: {error}"
                    );
                }
            }
        }
        // Only a test makes a read fall back to the mount, so only a test build counts them.
        #[cfg(feature = "test-util")]
        let fell_back = format!(", {} fell back to the mount", self.view.fallbacks());
        #[cfg(not(feature = "test-util"))]
        let fell_back = "";
        lore_debug!(
            "SWFS operation: {}; {} reads answered without the mount{fell_back}; \
             {} files frozen; {} paths described; {} revision diffs",
            self.mount
                .resources()
                .callbacks()
                .since(self.callbacks_at_start),
            self.view.answered(),
            self.view.frozen_file_count(),
            described,
            self.revision_diffs.load(Ordering::Relaxed)
        );
        result.forward_any::<FsError>("Thawing SWFS file system")?;
        Ok(())
    }
}

impl InstanceOperation for SwfsOperation {
    async fn file_info(&self, path: &RelativePath) -> Result<FileInfo, FsError> {
        if let Some((_, node)) = self.written_node(path) {
            return Ok(FileInfo::from_node_and_mtime(&node, 0));
        }
        if self.wrote_to_disk(path) {
            return self.on_mount(|| self.os.file_info(path)).await;
        }
        if self.creates_directory(path) {
            return Ok(FileInfo::Directory);
        }
        if self.removed_by_intent(path) {
            return Ok(FileInfo::NotExist);
        }
        let resources = self.mount.resources();
        match self
            .view
            .classify(path)
            .await
            .internal("Unable to ask the modified-file record")?
        {
            ReadSource::Unmodified => {
                let info = tracked_file_info(&resources.served_tree(), path)
                    .await
                    .internal("Unable to find file node")?;
                if info != FileInfo::NotExist {
                    return Ok(info);
                }
                // The revision holds nothing here: a file SWFS froze that the record does not
                // hold, such as an ignored one, or none.
                if let Some(frozen) = self.view.frozen_info(path) {
                    return Ok(frozen);
                }
                // Below a directory SWFS lists as moved, the revision holds it at the origin.
                if let Some(origin) = self.view.moved_from(path) {
                    return Ok(tracked_file_info(&resources.served_tree(), &origin)
                        .await
                        .internal("Unable to find file node")?);
                }
                Ok(info)
            }
            ReadSource::Deleted => Ok(FileInfo::NotExist),
            ReadSource::Covered(file) => Ok(file.file_info()),
            #[cfg(feature = "test-util")]
            ReadSource::Fallback(_) => self.on_mount(|| self.os.file_info(path)).await,
        }
    }

    async fn make_executable(&self, path: &RelativePath, _executable: bool) -> Result<(), FsError> {
        self.record_written(path);
        Ok(())
    }

    async fn create_dir_all(&self, path: &RelativePath) -> Result<(), FsError> {
        self.refuse_a_directory_over_a_file(path)?;
        self.record_written(path);
        self.intend(path, Intent::Directory);
        Ok(())
    }

    async fn write_file(&self, path: &RelativePath, contents: Bytes) -> Result<(), FsError> {
        // Bytes only this call holds, so they go to the mount, where SWFS keeps them, rather than
        // to the paths thaw describes from the repository's state, which would clear them.
        // Durable, so they reach SWFS within this operation: left in the OS file cache, they reach
        // it when the cache is next flushed, which can be after a later operation's thaw replaced
        // the file, and SWFS then keeps them over what that thaw described.
        let absolute = self.os.absolute(path)?;
        {
            let _permit = self.mount_io.permit().await;
            lore_io::IoDriver::global()
                .write_file_bytes(absolute, contents, true)
                .await?;
        }
        self.record_written_to_disk(path);
        Ok(())
    }

    async fn remove_recursive(&self, path: &RelativePath) -> Result<(), FsError> {
        let written_to_disk = self.intend_removal(path);
        if written_to_disk.is_empty() {
            return Ok(());
        }
        Box::pin(self.remove_on_mount(written_to_disk)).await
    }

    fn remove(&self, path: &RelativePath) -> impl Future<Output = Result<(), FsError>> + Send {
        self.remove_found(path, false)
    }

    async fn remove_empty_directory(&self, path: &RelativePath) -> Result<(), FsError> {
        // The files below a directory the revision holds are removed when the operation finalizes, so
        // the mount still holds them now: the directory goes with them once the operation sees
        // nothing left in it, as `remove` takes one.
        if self.view.revision_holds(path).await? && self.holds_nothing_below(path).await? {
            return self.remove_found(path, true).await;
        }
        self.on_mount(|| self.os.remove_empty_directory(path))
            .await?;
        self.removed_on_mount.insert(RelativePathKey(path.clone()));
        self.record_written(path);
        Ok(())
    }

    async fn rename(&self, from: &RelativePath, to: &RelativePath) -> Result<(), FsError> {
        let tracked = self
            .served_at_begin
            .resolve(from)
            .await
            .internal("Finding the path to move")?
            .is_some();
        if self.wrote_to_disk(from) || !tracked {
            // A scratch path the tree does not serve, such as a merge's `~mine` copy, and what this
            // operation wrote to the mount itself, are renamed through the mount, which then holds
            // what is at the destination, so it is read from there.
            // A file the operation wrote from a store holds nothing below it.
            let carried = if self.written_node(from).is_some() {
                Vec::new()
            } else {
                Box::pin(self.what_lies_below(from)).await?
            };
            self.on_mount(|| self.os.rename(from, to)).await?;
            self.forget_all_written_at(from);
            self.record_written(from);
            self.record_written(to);
            self.record_written_to_disk(to);
            let removed = self.carry_below(from, to, carried);
            if !removed.is_empty() {
                Box::pin(self.remove_carried_removals(removed)).await;
            }
            return Ok(());
        }
        // A tracked path is never renamed through the mount: that would make Lore's move the
        // user's own, which SWFS then keeps over what a thaw describes.
        if self.swfs_holds_changed(from).await? || self.holds_changes_below(from).await? {
            // The path holds bytes of the user's that only the mount has, and moving them is a write
            // through the mount. Nothing is renamed and nothing is named: the thaw of a commit that
            // stores the path names it with the committed state and spelling.
            return Ok(());
        }
        // The source serves it unchanged, so both ends are described to the thaw.
        self.refuse_a_directory_over_a_file(&to.parent_path())?;
        self.forget_written(from);
        self.forget_written(to);
        self.forget_intent(from);
        self.forget_intent(to);
        // A revision that does not hold the destination, as a merge left staged holds only the
        // source, is realized by the operation: the file is written at the destination and removed
        // from the origin, which the thaw leaves be where the revision does hold them.
        if !from.as_str().eq_ignore_ascii_case(to.as_str())
            && let Some(Served::Node {
                repository, node, ..
            }) = self
                .served_at_begin
                .resolve(from)
                .await
                .internal("Finding the path to move")?
            && !node.is_directory()
        {
            self.intend(
                to,
                Intent::Content {
                    repository,
                    address: node.address,
                },
            );
            self.intend(from, Intent::Removed);
        }
        self.record_written(to);
        // The same node under another spelling is named once, in the spelling it is left in.
        if !from.as_str().eq_ignore_ascii_case(to.as_str()) {
            self.record_written(from);
        }
        Ok(())
    }

    /// Both resolve to one node of the served State, which finds a child by any spelling of its
    /// name, compared by its whole link. A path the State holds no node at names nothing, as does
    /// every path before the mount serves a State.
    async fn names_one_entry(
        &self,
        from: &RelativePath,
        to: &RelativePath,
    ) -> Result<Option<bool>, FsError> {
        let resources = self.mount.resources();
        let Some(state) = resources.current_read_state() else {
            return Ok(None);
        };
        let repository = resources.repository();
        let node_at = async |path: &RelativePath| match state
            .locate_node(repository.clone(), path.as_str())
            .await
        {
            Ok(found) if found.is_valid() => {
                let link = found.identity();
                Ok(Some((link.repository, link.node, link.revision)))
            }
            Ok(_) | Err(StateError::NodeNotFound(_)) => Ok(None),
            Err(err) => Err(FsError::internal_with_context(
                err,
                "finding the node a path names",
            )),
        };
        let Some(from) = node_at(from).await? else {
            return Ok(None);
        };
        Ok(Some(node_at(to).await? == Some(from)))
    }

    async fn copy_file(
        &self,
        source_path: &RelativePath,
        destination_path: &RelativePath,
    ) -> Result<(), FsError> {
        // Read where the operation reads the source: from the store for a file nothing changed,
        // from the frozen snapshot for one the user changed, or as this operation wrote it, such as
        // a file it wrote from a store, which the mount does not hold until the operation
        // finalizes. Copied through the mount, a file SWFS has not fetched yet is fetched into its
        // cache to be read.
        let source = InstanceOperation::content_source(self, source_path).await?;
        // The copy is bytes only this call holds, so it goes to the mount, as `write_file` writes,
        // a window at a time, so a file of any size holds no more memory than one window.
        let (content, size) = source.open_once().await?;
        let absolute = self.os.absolute(destination_path)?;
        let options = lore_io::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true);
        let destination = self
            .on_mount(|| lore_io::IoDriver::global().open(absolute, &options))
            .await?;
        let window = size.min(COPY_WINDOW as u64) as usize;
        // SAFETY: each read fills the first `want` bytes of the buffer before any is read, and
        // only those are written.
        let mut buffer = unsafe { lore_io::uninit_buffer(window) };
        let mut offset = 0;
        while offset < size {
            let want = (size - offset).min(window as u64) as usize;
            let mut filled = content
                .read_window(WindowRead::new(buffer, 0, want), offset)
                .await?;
            filled.truncate(want);
            let written = {
                let _permit = self.mount_io.permit().await;
                destination.write_all_at(filled.freeze(), offset).await?
            };
            offset += want as u64;
            buffer = written.try_into_mut().unwrap_or_else(|_| {
                // SAFETY: as above.
                unsafe { lore_io::uninit_buffer(window) }
            });
        }
        // Durable, so the copy reaches SWFS within this operation, as `write_file` makes it.
        self.on_mount(|| destination.sync_data()).await?;
        self.record_written_to_disk(destination_path);
        Ok(())
    }

    async fn write_node(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
    ) -> Result<FileInfo, FsError> {
        if node.is_directory() {
            self.refuse_a_directory_over_a_file(path)?;
        } else {
            self.refuse_a_directory_over_a_file(&path.parent_path())?;
        }
        let options = immutable::read_options_from_repository(&repository);
        immutable::read(repository.clone(), node.address, None, options)
            .await
            .internal("Failed to cache file contents")?;

        if node.is_directory() {
            self.record_written(path);
            self.forget_written_to_disk(path);
            self.forget_written(path);
            self.intend(path, Intent::Directory);
            return Ok(FileInfo::Directory);
        }
        self.note_written_node(repository, node, path);
        Ok(FileInfo::from_node_and_mtime(node, 0))
    }

    async fn set_file_to_immutable_store_contents(
        &self,
        repository: Arc<RepositoryContext>,
        address: Address,
        path: &RelativePath,
    ) -> Result<(Fragment, Option<FileInfo>), FsError> {
        self.refuse_a_directory_over_a_file(&path.parent_path())?;
        let options = immutable::read_options_from_repository(&repository);
        self.intend(
            path,
            Intent::Content {
                repository: repository.clone(),
                address,
            },
        );
        let content = immutable::read(repository, address, None, options)
            .await
            .internal("Failed to cache file contents")?;

        self.record_written(path);
        // The content was fetched whole into the store, which is what a clone reports
        // transferring (ADR-00033 writes it to the mount, or SWFS fetches it, from there).
        Ok((
            Fragment {
                size_content: content.len() as u64,
                ..Fragment::default()
            },
            None,
        ))
    }

    fn changes_from_filesystem_to_state(
        &self,
        diff: FilesystemDiffContext,
    ) -> ChangeStream<FilesystemDiffStats> {
        if record_diff::answers(&diff) {
            return record_diff::changes_from_record(
                self.mount.resources().modified().clone(),
                diff,
            );
        }
        self.os.changes_from_filesystem_to_state(diff)
    }

    /// A file the revision does not track, such as a merge sidecar or a nested repository's
    /// `.lore`, is one SWFS holds of its own, which it froze with the rest of what it holds
    /// changed, ignored files included: answered from the snapshot rather than the mount. What
    /// this operation wrote, the snapshot holds nothing of yet.
    async fn untracked_file_info(&self, path: &RelativePath) -> Result<FileInfo, FsError> {
        if self.wrote_to_disk(path) || !self.view.snapshot_captured() {
            return self.on_mount(|| self.os.file_info(path)).await;
        }
        if self.removed_by_intent(path) {
            return Ok(FileInfo::NotExist);
        }
        if let Some(frozen) = self.view.frozen_info(path) {
            return Ok(frozen);
        }
        if self.view.froze_in(path) || self.wrote_to_disk_in(path) {
            return Ok(FileInfo::Directory);
        }
        // None of SWFS's own: whatever the served tree holds there, if anything.
        InstanceOperation::file_info(self, path).await
    }

    async fn holds_name_exactly(&self, path: &RelativePath) -> Option<bool> {
        if self.written_node(path).is_some() || self.creates_directory(path) {
            return Some(true);
        }
        if self.wrote_to_disk(path) {
            return self.on_mount(|| self.os.holds_name_exactly(path)).await;
        }
        match self.view.holds_name_exactly(path).await {
            Ok(Some(held)) => Some(held),
            // A test forced the view to leave every read to the mount.
            Ok(None) => self.on_mount(|| self.os.holds_name_exactly(path)).await,
            // Declining sends the caller to read the directory, which fails the same way and
            // reports it, rather than asking the mount about a name the view could not place.
            Err(error) => {
                lore_warn!("SWFS cannot say whether {path} holds its name: {error}");
                None
            }
        }
    }

    async fn read_directory(&self, path: &RelativePath) -> Result<DirectoryListing, FsError> {
        // `removed_by_intent`, with the removal reaching the directory kept for its entries.
        let reaches_directory = self.removal_reaches(path);
        if reaches_directory && !self.wrote_to_disk(path) && !self.writes_below(path) {
            return Ok(DirectoryListing::from_entries(Vec::new()));
        }
        let created = self.creates_directory(path);
        let listed = if self.wrote_to_disk_in(path) {
            self.mounted_directory(path).await
        } else {
            match self.view.read_directory(path).await {
                Ok(ListingAnswer::Listing(listing)) => Ok(*listing),
                #[cfg(feature = "test-util")]
                Ok(ListingAnswer::Fallback(_)) => self.mounted_directory(path).await,
                Err(error) => Err(error),
            }
        };
        // A directory the operation created need be nowhere else yet.
        let mut listing = match listed {
            Err(_) if created => DirectoryListing::from_entries(Vec::new()),
            listed => listed?,
        };
        let mut made = self.intended_children.children(path);
        if made.is_empty() && !reaches_directory && !self.removed_children.removes_in(path) {
            return Ok(listing);
        }
        // The mount still holds what the operation removed in the directory, and nothing yet of
        // what it means to make there.
        let mut entries = Vec::new();
        while let Some(entry) = listing.next().await {
            entries.push(entry?);
        }
        entries.retain(|entry| !self.removed_entry(path, &entry.name, reaches_directory));
        if !made.is_empty() {
            let listed: HashSet<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
            made.retain(|name| !listed.contains(name.as_str()));
        }
        for name in made {
            let child = path.join(&name);
            let info = if self.creates_directory(&child) {
                FileInfo::Directory
            } else if let Some((_, node)) = self.written_node(&child) {
                FileInfo::from_node_and_mtime(&node, 0)
            } else {
                // Content taken from a store without its node, which the listing leaves to the
                // revision and the mount.
                continue;
            };
            entries.push(DirectoryEntry::new(name, info));
        }
        Ok(DirectoryListing::from_entries(entries))
    }

    async fn content_source(
        &self,
        path: &RelativePath,
    ) -> Result<lore_storage::ContentSource<'static>, FsError> {
        if let Some((repository, node)) = self.written_node(path) {
            return Ok(stored_content(&repository, &node));
        }
        if self.wrote_to_disk(path) {
            return self.mounted_content(path).await;
        }
        if self.removed_by_intent(path) {
            return Err(file_not_found(path));
        }
        match self.view.content_source(path).await? {
            ContentAnswer::Source(source) => Ok(source),
            #[cfg(feature = "test-util")]
            ContentAnswer::Fallback(_) => self.mounted_content(path).await,
        }
    }

    async fn file_holds_content(
        &self,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        _mtime: u64,
        previous: Address,
        previous_size: u64,
        established: &lore_storage::ContentHashes,
    ) -> Result<NodeComparison, FsError> {
        if let Some((written_in, node)) = self.written_node(path) {
            if node.address.hash == previous.hash && node.size == previous_size {
                return Ok(NodeComparison::Matches);
            }
            return compare_content(
                repository,
                previous,
                previous_size,
                &stored_content(&written_in, &node),
                established,
            )
            .await;
        }
        if self.wrote_to_disk(path) {
            return self
                .mounted_holds_content(repository, path, previous, previous_size, established)
                .await;
        }
        match self
            .view
            .file_holds_content(
                repository.clone(),
                path,
                previous,
                previous_size,
                established,
            )
            .await?
        {
            HoldsAnswer::Compared(comparison) => {
                if matches!(comparison, NodeComparison::Matches)
                    && self.view.froze_unmoved_file_at(path)
                {
                    self.unchanged
                        .insert(RelativePathKey(path.clone()), (previous, previous_size));
                }
                Ok(comparison)
            }
            #[cfg(feature = "test-util")]
            HoldsAnswer::Fallback(_) => {
                self.mounted_holds_content(repository, path, previous, previous_size, established)
                    .await
            }
        }
    }
}

#[cfg(feature = "test-util")]
pub mod test_util {
    use super::*;

    impl SwfsOperation {
        /// Realizes what the state thaw describes from does not hold, as finalizing does.
        pub async fn realize_for_test(&self) -> Result<(), FsError> {
            self.realize_what_the_revision_does_not_hold().await
        }

        /// How many times the operation diffed the revisions it moved the instance between.
        pub fn revision_diffs_for_test(&self) -> usize {
            self.revision_diffs.load(Ordering::Relaxed)
        }

        /// Whether the mount leaves `path` out, as a file.
        pub fn hides_for_test(&self, path: &RelativePath) -> bool {
            self.mount.resources().hides(path, false)
        }

        /// The link and layer mount paths the operation removed whole, as finalizing would delete
        /// the user's files below.
        pub fn removed_subtrees_for_test(&self) -> Vec<String> {
            self.removed_subtrees
                .lock()
                .iter()
                .map(|path| path.as_str().to_owned())
                .collect()
        }

        /// The removed link and layer mount paths finalizing would make a directory at.
        pub fn recreated_mounts_for_test(&self) -> Vec<String> {
            self.recreated_mounts
                .lock()
                .iter()
                .map(|path| path.as_str().to_owned())
                .collect()
        }

        /// The modified-file record of the mount the operation is on.
        pub fn record_for_test(&self) -> Arc<crate::fs::swfs::modified_tree::ModifiedTree> {
            self.mount.resources().modified().clone()
        }

        /// The paths thaw would describe, sorted.
        pub fn described_for_test(&self) -> Vec<String> {
            let mut described: Vec<String> = self
                .touched_nodes
                .iter()
                .map(|path| path.0.as_str().to_owned())
                .collect();
            described.sort();
            described
        }

        /// An operation over `mount`, which need have mounted nothing, at `repo_path`, begun
        /// without a freeze: SWFS has frozen nothing and holds nothing modified.
        pub fn for_test(mount: Arc<SwfsInterface>, repo_path: &Path) -> Self {
            Self::for_test_frozen(mount, repo_path, FrozenSnapshot::empty())
        }

        /// [`Self::for_test`], begun with `snapshot` as what SWFS froze.
        pub fn for_test_frozen(
            mount: Arc<SwfsInterface>,
            repo_path: &Path,
            snapshot: FrozenSnapshot,
        ) -> Self {
            Self::for_test_bounded(mount, repo_path, snapshot, MountIo::global())
        }

        /// [`Self::for_test_frozen`], with each syscall on the mount bounded by `mount_io`.
        pub fn for_test_bounded(
            mount: Arc<SwfsInterface>,
            repo_path: &Path,
            snapshot: FrozenSnapshot,
            mount_io: &'static MountIo,
        ) -> Self {
            let callbacks_at_start = mount.resources().callbacks().mark();
            Self::new(
                mount,
                snapshot,
                callbacks_at_start,
                None,
                OsFilesystem::new(repo_path).begin_operation(),
                mount_io,
            )
        }
    }
}
