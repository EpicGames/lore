// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Holding a target revision on a mount ([`crate::fs::hold`]): record times, apply the changes
//! to the filesystem State in place, republish it, then invalidate what the kernel cached of the
//! old revision. Nodes the changes leave alone keep their ids, and so their inodes and listings.
//! The first publish onto the empty revision a mount starts on records nothing and diffs
//! nothing: it installs the target's State whole. Notifications go out with no table lock held,
//! because the kernel may call back into the filesystem while handling one.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use lore_base::types::Address;
use lore_error_set::prelude::*;
use lore_fuse::transport::Notifier;

use crate::change::FileAction;
use crate::change::NodeChange;
use crate::change::NodeChangeState;
use crate::filter::FilterMode;
use crate::fs::filesystem_provider::FsError;
use crate::fs::filesystem_provider::HoldOptions;
use crate::fs::filesystem_provider::InstanceOperation as _;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::fuse::filesystem::FuseFilesystem;
use crate::fs::fuse::inode::InodeKind;
use crate::fs::fuse::inode::InodeTable;
use crate::fs::fuse::inode::Name;
use crate::fs::fuse::inode::ROOT_INO;
use crate::fs::fuse::listing_cache::ListingCache;
use crate::fs::fuse::notifications::Invalidations;
use crate::fs::fuse::notifications::NotificationQueue;
use crate::fs::fuse::notifications::Pending;
use crate::fs::fuse::operation::StoredContent;
use crate::fs::fuse::served::Served;
use crate::fs::fuse::spelling::SpellingRecord;
use crate::fs::fuse::spelling::Spellings;
use crate::fs::fuse::view;
use crate::fs::fuse::view::filter_states_to_u8;
use crate::fs::fuse::view::same_view;
use crate::fs::hold;
use crate::fs::hold::HoldHost;
use crate::fs::hold::is_file;
use crate::hash::hash_string;
use crate::lore_trace;
use crate::lore_warn;
use crate::node::Node;
use crate::node::NodeFileMode;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::state::NodeComparison;
use crate::state::State;
use crate::state::Tree;
use crate::util::path::RelativePath;

/// Where a swap's invalidations go: the kernel through a [`Notifier`], or a test's recorder.
/// Called from a notification thread ([`NotificationQueue`]), never from a runtime worker.
pub trait NotificationSink: Send + Sync {
    fn inval_inode(&self, ino: u64);
    fn inval_entry(&self, parent: u64, name: &[u8]);
}

impl NotificationSink for Notifier {
    fn inval_inode(&self, ino: u64) {
        // Offset 0 and length 0 drop every cached page along with the attributes.
        if let Err(error) = Notifier::inval_inode(self, ino, 0, 0) {
            lore_trace!("FUSE inval_inode({ino}) not delivered: {error}");
        }
    }

    fn inval_entry(&self, parent: u64, name: &[u8]) {
        // The kernel answers ENOENT for a dentry it no longer holds, which is not a failure.
        if let Err(error) = Notifier::inval_entry(self, parent, name, false) {
            lore_trace!("FUSE inval_entry({parent}) not delivered: {error}");
        }
    }
}

/// What one swap invalidates and orphans. Kept on the filesystem between swaps so its
/// buffers are reused.
#[derive(Default)]
pub struct SwapPlan {
    pub inode_invalidations: Vec<u64>,
    pub entry_invalidations: Vec<(u64, Name)>,
    pub orphaned: Vec<u64>,
    /// Kernel-known directories whose entry set changed; their inode takes the swap time.
    pub touched_directories: Vec<u64>,
    /// Live file inodes whose content the swap changed, with what they now hold.
    pub rewritten: Vec<(u64, NodeChangeState)>,
    spellings: Vec<(u64, Spellings)>,
    live: Vec<u64>,
    /// The paths applies that failed or were cancelled since the last one that completed
    /// changed, each true where the next swap should add back what the target holds there.
    unsettled: BTreeMap<String, bool>,
    /// The State reads the last apply made.
    reads: u64,
}

impl SwapPlan {
    /// The paths marked unsettled, sorted.
    pub(crate) fn unsettled_paths(&self) -> Vec<String> {
        self.unsettled.keys().cloned().collect()
    }

    /// The State reads the last apply made.
    pub fn reads(&self) -> u64 {
        self.reads
    }

    fn clear(&mut self) {
        self.inode_invalidations.clear();
        self.entry_invalidations.clear();
        self.orphaned.clear();
        self.touched_directories.clear();
        self.rewritten.clear();
        self.spellings.clear();
        self.live.clear();
    }

    fn dedup(&mut self) {
        for list in [
            &mut self.inode_invalidations,
            &mut self.orphaned,
            &mut self.touched_directories,
        ] {
            list.sort_unstable();
            list.dedup();
        }
        self.entry_invalidations.sort_unstable();
        self.entry_invalidations.dedup();
    }
}

/// The plan for `changes` against the inodes and spellings the kernel holds now. Takes the
/// spellings it invalidates out of `spellings`.
pub fn plan_swap(
    inodes: &InodeTable,
    spellings: &SpellingRecord,
    changes: &[NodeChange],
) -> SwapPlan {
    let mut plan = SwapPlan::default();
    plan_swap_into(&mut plan, inodes, spellings, changes, &[]);
    plan
}

/// [`plan_swap`] into `plan`, leaving alone the inodes of the directory nodes in `kept`,
/// sorted, which the State still holds after the apply.
fn plan_swap_into(
    plan: &mut SwapPlan,
    inodes: &InodeTable,
    spellings: &SpellingRecord,
    changes: &[NodeChange],
    kept: &[NodeID],
) {
    for change in changes {
        let entry_changed = changes_entry_set(change);
        if let Some(source) = change.move_source() {
            plan_path(plan, inodes, spellings, source, (None, kept), true, false);
        }
        let after = (change.action != FileAction::Delete).then_some(&change.to);
        let rewrites = change.move_source().is_none()
            && is_file(&change.from)
            && is_file(&change.to)
            && content_differs(&change.from, &change.to);
        plan_path(
            plan,
            inodes,
            spellings,
            change.path(),
            (after, kept),
            entry_changed,
            rewrites,
        );
    }
    plan.dedup();
}

/// Plans the name at `path`, which holds `after` once the swap is published (nothing when
/// `None`). A file whose content changes keeps its inode, which takes the new content: where
/// it holds other content than `after`, or where `rewrites`, the change's own content differs,
/// whatever the inode holds now. A directory whose node is in `kept` stays as it is.
fn plan_path(
    plan: &mut SwapPlan,
    inodes: &InodeTable,
    spellings: &SpellingRecord,
    path: &RelativePath,
    (after, kept): (Option<&NodeChangeState>, &[NodeID]),
    entry_changed: bool,
    rewrites: bool,
) {
    if path.is_empty() {
        return;
    }
    // A directory the kernel never looked up holds nothing the kernel cached.
    let Some(parent) = resolve_directory(inodes, &path.parent_path()) else {
        return;
    };
    let name = path.name();
    let name_hash = hash_string(name);
    if let Some(taken) = spellings.take(parent, name_hash) {
        plan.entry_invalidations
            .extend(taken.seen.into_iter().map(|seen| (parent, seen)));
    }
    if entry_changed {
        plan.inode_invalidations.push(parent);
        plan.touched_directories.push(parent);
    }
    let Some(ino) = inodes.find_child(parent, name_hash, name) else {
        return;
    };
    let Some(entry) = inodes.get(ino) else {
        return;
    };
    let (stale, content_changed) = match (&entry.kind, after) {
        // Local content wins over whatever the target revision holds here.
        (InodeKind::File { local: true, .. } | InodeKind::Symlink, _) => (false, false),
        (InodeKind::Dir { node, .. }, _) if kept.binary_search(node).is_ok() => (false, false),
        (_, None) => (true, false),
        (InodeKind::Dir { .. }, Some(after)) => (is_file(after), false),
        (
            InodeKind::File {
                address,
                executable,
                ..
            },
            Some(after),
        ) => (
            !is_file(after),
            is_file(after)
                && (rewrites
                    || address.hash != after.address.hash
                    || *executable != is_executable(after)),
        ),
    };
    drop(entry);
    if stale {
        plan.orphaned.push(ino);
    }
    if content_changed && let Some(after) = after {
        plan.rewritten.push((ino, after.clone()));
    }
    if content_changed || (stale && after.is_none()) {
        plan.inode_invalidations.push(ino);
    }
}

/// Plans `path`, a local file a forced sync overwrote, or `detached`: an inode the kernel holds
/// there drops what it cached, and one at a detached file goes. The overwrite turned the inode it
/// found store-served; a local one found now was made local again by a write since, which wins.
fn plan_overwritten(plan: &mut SwapPlan, fs: &FuseFilesystem, path: &RelativePath, detached: bool) {
    let inodes = fs.inodes();
    let Some(parent) = resolve_directory(inodes, &path.parent_path()) else {
        return;
    };
    let name = path.name();
    let Some(ino) = inodes.find_child(parent, hash_string(name), name) else {
        return;
    };
    plan.inode_invalidations.push(ino);
    if detached {
        plan.orphaned.push(ino);
        plan.inode_invalidations.push(parent);
        plan.touched_directories.push(parent);
    }
}

/// On a view change every cached answer may be wrong: every directory's entries, every
/// dentry, and every file whose path the new view excludes.
fn plan_view_change(plan: &mut SwapPlan, fs: &FuseFilesystem, repository: &RepositoryContext) {
    let inodes = fs.inodes();
    let published = fs.served().repository.filter.clone();
    inodes.live(&mut plan.live);
    for &ino in &plan.live {
        let is_dir = match inodes.get(ino) {
            Some(entry)
                if matches!(
                    entry.kind,
                    InodeKind::File { local: true, .. } | InodeKind::Symlink
                ) =>
            {
                continue;
            }
            Some(entry) => matches!(entry.kind, InodeKind::Dir { .. }),
            None => continue,
        };
        if is_dir {
            // The directory's verdict, packed with the line that decided it, becomes the view's
            // the mount serves now: the target's, or the old one where the apply failed and the
            // served repository stayed. A request racing this reads a line of either view, which
            // the view it reads under clamps to its own (`view::child_filter`).
            if let Ok((path, _depth)) = fs.path_of(ino) {
                let states = published.exclusion_states(&path);
                if let Some(mut entry) = inodes.get_mut(ino)
                    && let InodeKind::Dir {
                        filter_state,
                        view_line,
                        ..
                    } = &mut entry.kind
                {
                    *filter_state = filter_states_to_u8(states);
                    *view_line = view::view_line(states);
                }
            }
            plan.inode_invalidations.push(ino);
            continue;
        }
        let Ok((path, _depth)) = fs.path_of(ino) else {
            continue;
        };
        if repository
            .filter
            .excludes_tree(&path, false, FilterMode::View)
        {
            plan.orphaned.push(ino);
            plan.inode_invalidations.push(ino);
        }
    }
    fs.spellings().take_all(&mut plan.spellings);
    for (parent, spellings) in plan.spellings.drain(..) {
        plan.entry_invalidations
            .extend(spellings.seen.into_iter().map(|seen| (parent, seen)));
    }
    plan.dedup();
}

/// The inode of the directory at `path`, walking the child index from the root.
fn resolve_directory(inodes: &InodeTable, path: &RelativePath) -> Option<u64> {
    let mut ino = ROOT_INO;
    for name in path.as_str().split('/').filter(|name| !name.is_empty()) {
        ino = inodes.find_child(ino, hash_string(name), name)?;
    }
    Some(ino)
}

fn is_executable(side: &NodeChangeState) -> bool {
    side.mode & NodeFileMode::Executable == NodeFileMode::Executable
}

/// Whether two file sides hold different content or a different executable bit.
fn content_differs(a: &NodeChangeState, b: &NodeChangeState) -> bool {
    a.address.hash != b.address.hash || is_executable(a) != is_executable(b)
}

/// Whether the change adds or removes a name, or replaces a file with a directory or back.
fn changes_entry_set(change: &NodeChange) -> bool {
    match change.action {
        FileAction::Keep => is_file(&change.from) != is_file(&change.to),
        _ => true,
    }
}

/// Whether the change leaves a file whose content or mode the target revision sets anew.
fn writes_file(change: &NodeChange) -> bool {
    change.action != FileAction::Delete
        && is_file(&change.to)
        && (change.action != FileAction::Keep || change.from.differs_from(&change.to))
}

/// What [`swap`] and [`swap_mounted`] do to local changes: keep each, as a sync forwarding local
/// changes does.
const KEEP_LOCAL: HoldOptions = HoldOptions {
    overwrite_local: false,
    keep_local: true,
    dry_run: false,
};

/// Swaps `fs` to `target` under `repository`, keeping local changes. The invalidations go to
/// `sink` from a thread of their own.
pub async fn swap(
    fs: &FuseFilesystem,
    operation: &InstanceOperationImpl,
    repository: Arc<RepositoryContext>,
    target: Arc<State>,
    sink: Arc<dyn NotificationSink>,
) -> Result<(), FsError> {
    swap_with(fs, operation, repository, target, sink, KEEP_LOCAL).await
}

/// [`swap`] doing to local changes what `options` asks ([`hold_mounted`]).
pub async fn swap_with(
    fs: &FuseFilesystem,
    operation: &InstanceOperationImpl,
    repository: Arc<RepositoryContext>,
    target: Arc<State>,
    sink: Arc<dyn NotificationSink>,
    options: HoldOptions,
) -> Result<(), FsError> {
    let queue = NotificationQueue::start(sink).internal("starting a notification thread")?;
    let swapped = swap_through(fs, operation, repository, target, Some(&queue), options).await;
    queue.stop().await;
    swapped
}

/// [`hold_mounted`], sending the invalidations through `notifications`, or nowhere where `None`.
async fn swap_through(
    fs: &FuseFilesystem,
    operation: &InstanceOperationImpl,
    repository: Arc<RepositoryContext>,
    target: Arc<State>,
    notifications: Option<&NotificationQueue>,
    options: HoldOptions,
) -> Result<(), FsError> {
    let mut plan = fs.swap_plan().lock().await;
    plan.clear();

    let served = fs.served();
    if is_unpublished_empty(fs, &served) {
        if options.dry_run {
            return Ok(());
        }
        let listings = Arc::new(served.listings.fresh());
        drop(served);
        let installed = fs
            .mount_state(&repository, &target)
            .await
            .forward_any::<FsError>("loading the target revision")?;
        let tree = installed
            .tree(fs.provider_repository(&repository))
            .await
            .forward_any::<FsError>("reading the target tree header")?;
        fs.publish(
            installed,
            fs.provider_repository(&repository),
            Some(listings),
            Some(&tree),
        );
        fs.mark_published();
        plan_view_change(&mut plan, fs, &repository);
        plan.live.clear();
        fs.inodes().live(&mut plan.live);
        for &ino in &plan.live {
            fs.inodes().orphan(ino);
        }
        let pending = pending(&mut plan, notifications);
        send(&mut plan, pending).await;
        return Ok(());
    }
    let same_view = same_view(&served.repository, &repository);
    let unsettled = plan.unsettled.clone();
    let held = (&served.repository, &served.state);
    let (applied, before) = fs
        .scoped(hold::changes(held, (&repository, &target), &unsettled))
        .await
        .forward_any::<FsError>("diffing the served revision against the target")?;
    let changes = hold::in_view(&repository, &applied);
    // A view change also carries what enters and leaves the view, which the unfiltered diff holds
    // nothing of: what is checked for local changes and reported is what each view shows.
    let shown: Cow<'_, [NodeChange]> = if same_view {
        Cow::Borrowed(&changes)
    } else {
        Cow::Owned(
            fs.scoped(hold::shown_changes(held, (&repository, &target), &before))
                .await
                .forward_any::<FsError>("diffing what the served and the target views show")?,
        )
    };
    let mut paths = hold::Resolver::new(held);
    let host = Host {
        fs,
        served: &served,
        operation,
        repository: fs.provider_repository(&repository),
    };
    let matching = fs
        .scoped(hold::check((&host, &mut paths), &shown, options))
        .await?;
    if options.dry_run {
        return fs
            .scoped(hold::report(&shown))
            .await
            .forward_any::<FsError>("reporting what holding the target changes");
    }

    // The time is recorded for content served from the store. A copy-up later stamps the
    // content's host file in the staging directory and moves it under `write/`, so the time is
    // probed there: an edit of the same size compares as unchanged where the host file's stamp
    // matches it. A read-only mount writes no host file, so its time is probed in the dot
    // directory.
    let probe_dir = match fs.local_files() {
        Some(local) => Some(local.staging_dir()),
        None => repository.dot_dir_path().ok(),
    };
    let now = crate::state::time_before_next_write(probe_dir).await;
    let mut directories: Vec<RelativePath> = Vec::new();
    for change in applied.iter().filter(|change| changes_entry_set(change)) {
        directories.push(change.path().parent_path());
        if let Some(source) = change.move_source() {
            directories.push(source.parent_path());
        }
    }
    directories.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    directories.dedup_by(|a, b| a.as_str() == b.as_str());
    let recorded = || {
        applied
            .iter()
            .filter(|change| writes_file(change))
            .map(NodeChange::path)
            .chain(directories.iter())
    };
    for path in recorded() {
        operation.record_modified_time(&repository, path, now);
    }
    fs.recorded_times()
        .store(&repository, recorded(), now)
        .await?;

    fs.mark_published();
    let SwapPlan {
        unsettled: recorded,
        reads,
        ..
    } = &mut *plan;
    let mut applying = Applying::begin(
        fs,
        served.clone(),
        fs.provider_repository(&repository),
        recorded,
    );
    if !same_view {
        applying.listings = Some(Arc::new(served.listings.fresh()));
        served.listings.clear();
    }
    let mut reconciled = Vec::new();
    let mut overwritten = Vec::new();
    let result = fs
        .scoped(Box::pin(hold::apply(
            (&host, paths),
            &applied,
            (options, &matching),
            &mut applying.changed,
            reads,
            (&unsettled, &mut reconciled, &mut overwritten),
            fs.swap_hook(),
        )))
        .await
        .forward_any::<FsError>("applying the target revision to the served State");
    let tree = match result {
        Ok(()) => target
            .tree(repository.clone())
            .await
            .forward_any::<FsError>("reading the target tree header"),
        Err(error) => Err(error),
    };
    let applied_ok = tree.is_ok();
    if let Ok(tree) = &tree {
        served.state.set_revision(target.revision());
        let mut settled: Vec<RelativePath> = Vec::with_capacity(applied.len());
        for change in applied.iter() {
            settled.push(change.path().parent_path());
            settled.extend(change.move_source().map(RelativePath::parent_path));
        }
        settled.extend(reconciled.iter().map(RelativePath::parent_path));
        settled.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        settled.dedup_by(|a, b| a.as_str() == b.as_str());
        let spelled_by = match &before {
            Some(before) => (&**before, &served.repository),
            None => (&*target, &repository),
        };
        fs.settle_swapped(&served, ((&target, &repository), spelled_by), settled)
            .await;
        if let Err(error) = fs
            .rewrite_deletions_for(&served, (&target, &repository))
            .await
        {
            lore_warn!("FUSE rewriting the deletion list after a swap failed: {error}");
        }
        applying.complete(tree);
    }
    drop(applying);
    let kept = kept_directories(fs, &served, &changes, &reconciled).await;
    drop(served);

    plan_swap_into(&mut plan, fs.inodes(), fs.spellings(), &changes, &kept);
    for (path, detached) in &overwritten {
        plan_overwritten(&mut plan, fs, path, *detached);
    }
    for path in &reconciled {
        plan_path(
            &mut plan,
            fs.inodes(),
            fs.spellings(),
            path,
            (None, &kept),
            true,
            false,
        );
    }
    plan.dedup();
    if !same_view {
        plan_view_change(&mut plan, fs, &repository);
    }
    if !applied_ok {
        let rewritten = std::mem::take(&mut plan.rewritten);
        plan.orphaned
            .extend(rewritten.into_iter().map(|(ino, _)| ino));
    }
    for &ino in &plan.orphaned {
        fs.inodes().orphan(ino);
    }
    let pending = pending(&mut plan, notifications);
    // Taken, and cleared after use, as a side's mapping holds the command's context and with it
    // the write token, which the next command waits for.
    let mut rewritten = std::mem::take(&mut plan.rewritten);
    for (ino, after) in &rewritten {
        let side = &after.mapping;
        let Ok(written) = side.state.node(side.repository.clone(), side.node).await else {
            fs.inodes().orphan(*ino);
            continue;
        };
        if let Some(mut entry) = fs.inodes().get_mut(*ino)
            && let InodeKind::File {
                address,
                size,
                executable,
                local: false,
            } = &mut entry.kind
        {
            *address = written.address;
            *size = written.size;
            *executable = is_executable(after);
        }
    }
    rewritten.clear();
    plan.rewritten = rewritten;
    for &ino in &plan.touched_directories {
        if let Some(mut entry) = fs.inodes().get_mut(ino) {
            entry.mtime = now;
        }
    }

    send(&mut plan, pending).await;
    if tree.is_ok()
        && let Err(error) = fs.scoped(hold::report(&shown)).await
    {
        lore_warn!("FUSE reporting what a swap changed failed: {error}");
    }
    tree.map(|_| ())
}

/// A swap applying to the served State. However it ends, completed, failed or dropped mid-apply,
/// its end republishes the State and ends
/// the swap, so the swap count never stays odd; one that did not complete leaves the paths it
/// changed among the unsettled ones the next swap re-places, and the served repository in
/// place, so a view change it began is made whole by the next swap.
struct Applying<'a> {
    fs: &'a FuseFilesystem,
    served: Arc<Served>,
    repository: Arc<RepositoryContext>,
    unsettled: &'a mut BTreeMap<String, bool>,
    /// The paths this apply changed, each true where the next swap should add it back.
    changed: BTreeMap<String, bool>,
    tree: Option<Tree>,
    /// The fresh listing cache a view change publishes in place of the served one.
    listings: Option<Arc<ListingCache>>,
}

impl<'a> Applying<'a> {
    fn begin(
        fs: &'a FuseFilesystem,
        served: Arc<Served>,
        repository: Arc<RepositoryContext>,
        unsettled: &'a mut BTreeMap<String, bool>,
    ) -> Self {
        fs.begin_swap();
        Self {
            fs,
            served,
            repository,
            unsettled,
            changed: BTreeMap::new(),
            tree: None,
            listings: None,
        }
    }

    /// Marks the apply complete, with the target's tree header to report in STATFS.
    fn complete(&mut self, tree: &Tree) {
        self.tree = Some(*tree);
    }

    /// Orphans the live file inodes at the paths an apply that did not complete changed, so no
    /// lookup finds one by a name whose node that apply replaced or detached. The next swap
    /// invalidates the names.
    fn orphan_changed_files(&self) {
        let inodes = self.fs.inodes();
        for path in self.changed.keys() {
            let path = RelativePath::new_from_clean_parts(path, "");
            let Some(parent) = resolve_directory(inodes, &path.parent_path()) else {
                continue;
            };
            let name = path.name();
            let Some(ino) = inodes.find_child(parent, hash_string(name), name) else {
                continue;
            };
            if inodes
                .get(ino)
                .is_some_and(|entry| matches!(entry.kind, InodeKind::File { local: false, .. }))
            {
                inodes.orphan(ino);
            }
        }
    }
}

impl Drop for Applying<'_> {
    fn drop(&mut self) {
        let repository = if self.tree.is_some() {
            self.unsettled.clear();
            self.repository.clone()
        } else {
            self.orphan_changed_files();
            self.unsettled.append(&mut self.changed);
            self.served.repository.clone()
        };
        self.fs.publish(
            self.served.state.clone(),
            repository,
            self.listings.take(),
            self.tree.as_ref(),
        );
        self.fs.end_swap();
    }
}

/// The directory nodes, sorted, of the inodes the kernel holds at the paths `changes` delete,
/// move away or turn into files, and at `reconciled`, which the State still holds after the
/// apply, as where they hold local content. Each parent directory is resolved once.
async fn kept_directories(
    fs: &FuseFilesystem,
    served: &Served,
    changes: &[NodeChange],
    reconciled: &[RelativePath],
) -> Vec<NodeID> {
    let inodes = fs.inodes();
    let mut kept = Vec::new();
    let mut paths: Vec<&RelativePath> = Vec::with_capacity(changes.len() + reconciled.len());
    for change in changes {
        paths.extend(change.move_source());
        if change.action == FileAction::Delete || is_file(&change.to) {
            paths.push(change.path());
        }
    }
    paths.extend(reconciled);
    fn parent_of(path: &RelativePath) -> &str {
        path.parent().unwrap_or("")
    }
    paths.sort_unstable_by(|a, b| parent_of(a).cmp(parent_of(b)));
    let mut last_parent: Option<(&str, Option<u64>)> = None;
    for path in paths {
        let parent = match last_parent {
            Some((known, parent)) if known == parent_of(path) => parent,
            _ => {
                let parent = resolve_directory(inodes, &path.parent_path());
                last_parent = Some((parent_of(path), parent));
                parent
            }
        };
        let Some(parent) = parent else {
            continue;
        };
        let name = path.name();
        let Some(ino) = inodes.find_child(parent, hash_string(name), name) else {
            continue;
        };
        let node = match inodes.get(ino).map(|entry| match entry.kind {
            InodeKind::Dir { node, .. } => Some(node),
            InodeKind::File { .. } | InodeKind::Symlink => None,
        }) {
            Some(Some(node)) => node,
            _ => continue,
        };
        if let Ok(record) = served.state.node(served.repository.clone(), node).await
            && !record.is_discarded()
            && record.is_directory()
        {
            kept.push(node);
        }
    }
    kept.sort_unstable();
    kept.dedup();
    kept
}

/// Whether `served` is the empty revision the mount started on, which no swap has published
/// over. A swap from it has no kernel state to diff against and no time to record, since every
/// path reads `base_time`.
fn is_unpublished_empty(fs: &FuseFilesystem, served: &Served) -> bool {
    !fs.is_published() && served.revision().is_zero()
}

/// `plan`'s invalidations, taken to be sent from the notification thread. Dropped unsent, as a
/// swap dropped after publish drops them, they are queued without waiting.
fn pending<'a>(plan: &mut SwapPlan, notifications: Option<&'a NotificationQueue>) -> Pending<'a> {
    Pending::new(
        notifications,
        Invalidations {
            entries: std::mem::take(&mut plan.entry_invalidations),
            inodes: std::mem::take(&mut plan.inode_invalidations),
        },
    )
}

/// Sends `pending`, entries before inodes, and waits for it: after publish, under the swap
/// plan's lock, before the swap returns. A swap dropped while it waits has still handed them
/// over, so they go out ahead of any later batch.
async fn send(plan: &mut SwapPlan, pending: Pending<'_>) {
    // Handed back so the next swap reuses the buffers.
    let sent = pending.send().await;
    plan.entry_invalidations = sent.entries;
    plan.inode_invalidations = sent.inodes;
}

/// The host side of a hold on a mount: the files under `write/` beside the served State, compared
/// through `operation` under `repository`, the provider's context.
struct Host<'a> {
    fs: &'a FuseFilesystem,
    served: &'a Served,
    operation: &'a InstanceOperationImpl,
    repository: Arc<RepositoryContext>,
}

impl HoldHost for Host<'_> {
    fn remove_tree(&self, path: &RelativePath) {
        self.fs.remove_host_tree(path);
    }

    fn remove_file(&self, path: &RelativePath) {
        self.fs.remove_host_file(path);
    }

    async fn overwrite_local(
        &self,
        path: &RelativePath,
        node: NodeID,
        place: (NodeID, u64),
        wanted: &Node,
    ) -> Result<bool, FsError> {
        let target = StoredContent {
            address: wanted.address,
            size: wanted.size,
            mode: wanted.mode,
        };
        self.fs
            .overwrite_local_file(self.served, path, node, place, &target)
            .await
            .forward_any::<FsError>("overwriting a local file")
    }

    async fn holds_content(
        &self,
        path: &RelativePath,
        address: Address,
        size: u64,
        established: &lore_storage::ContentHashes,
    ) -> Result<bool, FsError> {
        let compared = self
            .operation
            .file_holds_content(self.repository.clone(), path, address, size, established)
            .await?;
        Ok(matches!(compared, NodeComparison::Matches))
    }
}

/// Holds `target` under `repository` on `fs`, through the mount's notification thread, as
/// [`InstanceOperationImpl::hold_state`] asks. The served State takes the unfiltered diff from
/// the served revision ([`hold::changes`]); after an apply that failed or was cancelled, the
/// State may stand anywhere between the served revision and the targets of those applies, so
/// the next swap also re-places each path they changed. The kernel is told of the changes the
/// target's view shows. Forwards `swap_through`'s future.
#[inline]
pub fn hold_mounted(
    fs: &FuseFilesystem,
    operation: &InstanceOperationImpl,
    repository: Arc<RepositoryContext>,
    target: Arc<State>,
    options: HoldOptions,
) -> impl Future<Output = Result<(), FsError>> {
    swap_through(
        fs,
        operation,
        repository,
        target,
        fs.notifications(),
        options,
    )
}

/// [`swap`] through the mount's notification thread.
pub async fn swap_mounted(
    fs: &FuseFilesystem,
    operation: &InstanceOperationImpl,
    repository: Arc<RepositoryContext>,
    target: Arc<State>,
) -> Result<(), FsError> {
    hold_mounted(fs, operation, repository, target, KEEP_LOCAL).await
}
