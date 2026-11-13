use std::sync::Arc;

use lore_base::types::Hash;
use lore_error_set::ForwardStrict;
use lore_error_set::WrapInternal;
use parking_lot::RwLock;

use crate::filter::Filter;
use crate::filter::FilterMode;
use crate::fs::swfs::api_interface::swfs_api::SWFSNodeModFlag_Deleted;
use crate::fs::swfs::api_interface::swfs_api::SWFSNodeModFlag_NewNode;
use crate::fs::swfs::api_interface::swfs_api::SWFSNodeModFlag_UpdateName;
use crate::fs::swfs::frozen_files::FrozenFile;
use crate::fs::swfs::frozen_files::moves_in;
use crate::fs::swfs::paths::SwfsPath;
use crate::fs::swfs::served_tree::ServedTree;
use crate::lore_warn;
use crate::node::Node;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::ROOT_NODE;
use crate::repository::DOT_LOREIGNORE;
use crate::repository::RepositoryContext;
use crate::state::Ext;
use crate::state::Mark;
use crate::state::State;
use crate::state::StateError;
use crate::state::collect_dirty_paths;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;

/// The files an SWFS instance holds as modified from its current revision, as an in-memory
/// [`State`] carrying only `Dirty*` flags, the ones `lore status --scan` sets.
///
/// Kept apart from the staged `State`, so it cannot disagree with a command's staged write, and in
/// memory alone: a mount starts with it untrusted, and the first operation rebuilds it from SWFS's
/// own list of what it holds modified, which SWFS keeps across mounts ([`rebuild`](Self::rebuild)).
/// No command reads or writes it. It may report a file that is not modified, such as one written
/// back to its original content, which costs the diff a comparison. It must not omit one.
#[derive(Debug)]
pub struct ModifiedTree {
    repository: Arc<RepositoryContext>,
    state: RwLock<Arc<State>>,
    /// Whether the record is known to include every modification. A callback SWFS cannot be
    /// answered for, or a mount whose past changes are unknown, leaves it false.
    trust: parking_lot::Mutex<Trust>,
    /// Paths it excludes are never recorded.
    ignore: RwLock<Arc<Filter>>,
    /// The revision the record is measured against, once one has been seen.
    revision: RwLock<Option<Hash>>,
    /// What the revision tracks, which the ignore rules leave alone.
    served: RwLock<Option<Arc<ServedTree>>>,
    /// Held while the record is changed. SWFS calls its callbacks from threads of its own, and a
    /// directory two of them find missing would be added twice.
    updating: tokio::sync::Mutex<()>,
}

/// Whether the record is known to hold every modification, and how often it has been found not
/// to.
#[derive(Debug, Default)]
struct Trust {
    trusted: bool,
    distrusted: u64,
}

/// A child of a directory as the record holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedChild {
    /// The name as the record first saw it spelled.
    pub name: String,
    /// What the record says of the child itself. A child held only as the way to something that
    /// changed below it is `Unchanged`.
    pub coverage: Coverage,
}

/// What the record says of a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// The record holds nothing of the path, so it is as the revision has it.
    Unchanged,
    /// The path was added or modified, or lies below a directory that was added.
    Changed,
    /// The path was deleted, or lies below a directory that was.
    Deleted,
}

/// A change SWFS reported through a `notify_*` callback, by the node path it names.
pub enum Change {
    Written(String),
    Created {
        path: String,
        directory: bool,
    },
    Moved {
        from: String,
        to: String,
        directory: bool,
    },
    Deleted {
        path: String,
        directory: bool,
    },
}

impl Change {
    fn touches(&self, name: &str) -> bool {
        match self {
            Change::Written(path) | Change::Created { path, .. } | Change::Deleted { path, .. } => {
                path == name
            }
            Change::Moved { from, to, .. } => from == name || to == name,
        }
    }
}

impl ModifiedTree {
    pub fn new(repository: Arc<RepositoryContext>) -> Self {
        Self {
            repository,
            state: RwLock::new(State::new()),
            trust: parking_lot::Mutex::new(Trust::default()),
            ignore: RwLock::new(Arc::default()),
            revision: RwLock::new(None),
            served: RwLock::new(None),
            updating: tokio::sync::Mutex::new(()),
        }
    }

    pub fn state(&self) -> Arc<State> {
        self.state.read().clone()
    }

    pub fn repository(&self) -> Arc<RepositoryContext> {
        self.repository.clone()
    }

    /// The revision the record holds the differences from, which it last observed, if any.
    pub fn revision(&self) -> Option<Hash> {
        *self.revision.read()
    }

    pub fn is_trusted(&self) -> bool {
        self.trust.lock().trusted
    }

    pub fn set_trusted(&self, trusted: bool) {
        self.trust.lock().trusted = trusted;
    }

    /// Leaves the record untrusted because it may lack a change, and counts it, so a seed
    /// already running does not trust the record again when it finishes.
    pub fn distrust(&self) {
        let mut trust = self.trust.lock();
        trust.trusted = false;
        trust.distrusted += 1;
    }

    /// What [`trust_unless_distrusted_since`](Self::trust_unless_distrusted_since) compares
    /// against, taken before the walk that seeds the record begins.
    pub fn distrust_mark(&self) -> u64 {
        self.trust.lock().distrusted
    }

    /// Trusts the record once a seed has filled it, unless it was distrusted while the seed ran:
    /// that change may be one the seed's walk had already passed.
    pub fn trust_unless_distrusted_since(&self, mark: u64) {
        let mut trust = self.trust.lock();
        if trust.distrusted == mark {
            trust.trusted = true;
        }
    }

    pub fn set_ignore(&self, filter: Arc<Filter>) {
        *self.ignore.write() = filter;
    }

    /// Records `change`, unless the path it names is ignored. A change that cannot be recorded
    /// leaves the record untrusted, since a callback has no caller to report the failure to.
    pub async fn record(&self, change: Change) {
        // The ignore rules were read from the revision, so a change to them leaves the record
        // unsure what to skip until it is rebuilt.
        if change.touches(DOT_LOREIGNORE) {
            self.distrust();
        }
        if let Err(error) = self.try_record(&change).await {
            lore_warn!(
                "SWFS change could not be recorded, the record is no longer trusted: {error}"
            );
            self.distrust();
        }
    }

    async fn try_record(&self, change: &Change) -> Result<(), StateError> {
        match change {
            Change::Written(path) => {
                if !self.is_ignored(path, false).await? {
                    self.mark_modified(path).await?;
                }
            }
            &Change::Created {
                ref path,
                directory,
            } => {
                if !self.is_ignored(path, directory).await? {
                    self.mark(path, NodeFlags::DirtyAdd, directory).await?;
                }
            }
            &Change::Deleted {
                ref path,
                directory,
            } => {
                if !self.is_ignored(path, directory).await? {
                    self.mark(path, NodeFlags::DirtyDelete, directory).await?;
                }
            }
            &Change::Moved {
                ref from,
                ref to,
                directory,
            } => {
                if !self.is_ignored(from, directory).await? {
                    self.mark(from, NodeFlags::DirtyDelete, directory).await?;
                }
                if !self.is_ignored(to, directory).await? {
                    self.mark(to, NodeFlags::DirtyAdd, directory).await?;
                }
            }
        }
        Ok(())
    }

    /// Whether the record leaves `path` out for the ignore rules, so that it holds no change to it
    /// whether there was one or not. The rules govern only what the revision does not track, so a
    /// path it holds is recorded whatever they match.
    pub async fn ignores(&self, path: &RelativePath) -> bool {
        self.ignored(path, false).await.unwrap_or(false)
    }

    async fn is_ignored(&self, path: &str, directory: bool) -> Result<bool, StateError> {
        let path = RelativePath::new_from_initial_path(path)
            .internal("SWFS path is not a relative path")?;
        self.ignored(&path, directory).await
    }

    async fn ignored(&self, path: &RelativePath, directory: bool) -> Result<bool, StateError> {
        // The ignore rules alone: a change to the view takes effect after the sync that makes it
        // has thawed, while SWFS reports what that sync wrote, and a diff applies the view itself.
        let excluded = self
            .ignore
            .read()
            .excludes(path, directory, FilterMode::Ignore);
        if !excluded {
            return Ok(false);
        }
        let served = self.served.read().clone();
        let Some(served) = served else {
            return Ok(true);
        };
        Ok(served.resolve(path).await?.is_none())
    }

    /// Sets the tree the record asks whether the revision tracks a path.
    pub fn set_served(&self, served: Arc<ServedTree>) {
        *self.served.write() = Some(served);
    }

    /// Notes the revision the instance now stands at. A different one than the record was built
    /// against empties it and leaves it untrusted, so the next diff rebuilds it: the record holds
    /// differences from a revision, and what Lore itself writes to the tree is not reported.
    pub async fn observe_revision(&self, revision: Hash) {
        let _updating = self.updating.lock().await;
        let previous = self.revision.write().replace(revision);
        if previous.is_some_and(|previous| previous != revision) {
            self.clear();
            self.distrust();
        }
    }

    /// Rebuilds the record from SWFS's own list of the files it holds modified
    /// (`swfsGetModifiedFileList`), which SWFS keeps across thaws and remounts, rather than by
    /// walking the tree, and trusts it. A directory that only holds a change is left out.
    ///
    /// SWFS marks a path new against the source as it was when the path was created, not against
    /// `revision`, the state the record measures from, which may have taken it in since: so a path
    /// SWFS calls new is recorded as added only where `revision` lacks it, and otherwise as a
    /// modified file or, for a directory, not at all, since an added directory would cover what
    /// changed below it. A deletion is recorded where `revision` holds the path, whether or not
    /// SWFS calls it new; one it does not hold, created and deleted since, deleted nothing. A rename
    /// is recorded whatever SWFS calls it.
    ///
    /// SWFS lists a renamed file by its destination, and its origin as a deletion with the same
    /// `file_index`, which names the move ([`moves_in`]). The origin is not listed while another
    /// node holds its path, and a rename without it is recorded as an add and leaves the record
    /// untrusted, as a record that cannot say the origin is gone. Returns whether the record is
    /// trusted.
    pub async fn rebuild(
        &self,
        listed: &[FrozenFile],
        revision: Option<&ServedTree>,
    ) -> Result<bool, StateError> {
        let mark = self.distrust_mark();
        self.clear();
        let moves: std::collections::HashMap<String, String> = moves_in(listed)
            .into_iter()
            .map(|(from, to)| (to.to_lowercase(), from))
            .collect();
        let mut resolved = true;
        for file in listed {
            let path = SwfsPath(&file.path)
                .node_path()
                .internal("Parsing a modified file path")?
                .0;
            if path.is_empty() {
                continue;
            }
            let path = path.as_str().to_owned();
            let directory = file.is_directory;
            let deleted = file.mod_flags & SWFSNodeModFlag_Deleted != 0;
            let created = file.mod_flags & SWFSNodeModFlag_NewNode != 0;
            let renamed = file.mod_flags & SWFSNodeModFlag_UpdateName != 0;
            let change = if deleted {
                if !self.revision_holds(revision, &path).await? {
                    continue;
                }
                Change::Deleted { path, directory }
            } else if renamed {
                if let Some(from) = moves.get(&path.to_lowercase()).cloned() {
                    Change::Moved {
                        from,
                        to: path,
                        directory,
                    }
                } else {
                    resolved = false;
                    Change::Created { path, directory }
                }
            } else if created && !self.revision_holds(revision, &path).await? {
                Change::Created { path, directory }
            } else if directory {
                continue;
            } else {
                Change::Written(path)
            };
            self.try_record(&change).await?;
        }
        if resolved {
            self.trust_unless_distrusted_since(mark);
        }
        Ok(resolved && self.is_trusted())
    }

    /// Whether `revision` serves `path`: below a link or layer, whether the repository mounted
    /// there holds it.
    async fn revision_holds(
        &self,
        revision: Option<&ServedTree>,
        path: &str,
    ) -> Result<bool, StateError> {
        let Some(revision) = revision else {
            return Ok(false);
        };
        let path = RelativePath::new_from_initial_path(path)
            .forward::<StateError>("Parsing a modified path")?;
        Ok(revision.resolve(&path).await?.is_some())
    }

    /// Forgets every recorded path.
    pub fn clear(&self) {
        *self.state.write() = State::new();
    }

    pub async fn mark_added(&self, path: &str) -> Result<(), StateError> {
        self.mark(path, NodeFlags::DirtyAdd, false).await
    }

    pub async fn mark_modified(&self, path: &str) -> Result<(), StateError> {
        self.mark(path, NodeFlags::DirtyModify, false).await
    }

    pub async fn mark_deleted(&self, path: &str) -> Result<(), StateError> {
        self.mark(path, NodeFlags::DirtyDelete, false).await
    }

    /// SWFS does not track moves, so a move is a delete of `from` and an add of `to`.
    pub async fn mark_moved(&self, from: &str, to: &str) -> Result<(), StateError> {
        self.mark_deleted(from).await?;
        self.mark_added(to).await
    }

    /// Whether `path` itself carries a modification, rather than only holding one below it.
    pub async fn is_modified(&self, path: &str) -> Result<bool, StateError> {
        match self.state().find_node(self.repository.clone(), path).await {
            Ok(node) => Ok(has_action(&node)),
            Err(StateError::NodeNotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// What the record says of `path`: it carries a modification itself, or lies below a directory
    /// that does, which was added or deleted whole. The first modification met on the way down
    /// decides, since what a directory is said to be is true of what is under it.
    pub async fn coverage(&self, path: &str) -> Result<Coverage, StateError> {
        let state = self.state();
        let segments = path_segments(path);
        let mut node = ROOT_NODE;
        for segment in segments {
            let name_hash = crate::hash::hash_string(segment);
            let Ok(child) = state
                .find_subnode(self.repository.clone(), node, name_hash)
                .await
            else {
                return Ok(Coverage::Unchanged);
            };
            let found = state.node(self.repository.clone(), child).await?;
            match node_coverage(&found) {
                Coverage::Unchanged => node = child,
                decided => return Ok(decided),
            }
        }
        Ok(Coverage::Unchanged)
    }

    /// The children the record holds directly in the directory at `path`, none where it holds
    /// the directory itself not at all.
    pub async fn children(&self, path: &str) -> Result<Vec<RecordedChild>, StateError> {
        let state = self.state();
        let segments = path_segments(path);
        let mut node = ROOT_NODE;
        for segment in segments {
            let name_hash = crate::hash::hash_string(segment);
            let Ok(child) = state
                .find_subnode(self.repository.clone(), node, name_hash)
                .await
            else {
                return Ok(Vec::new());
            };
            node = child;
        }
        let mut children = Vec::new();
        for child in state.node_children(self.repository.clone(), node).await? {
            let Some(name) = state
                .node_name_clone_or_skip(self.repository.clone(), child)
                .await?
            else {
                continue;
            };
            let found = state.node(self.repository.clone(), child).await?;
            children.push(RecordedChild {
                name,
                coverage: node_coverage(&found),
            });
        }
        Ok(children)
    }

    /// Whether `path` is changed by what the record holds, however it is changed.
    pub async fn covers(&self, path: &str) -> Result<bool, StateError> {
        Ok(self.coverage(path).await? != Coverage::Unchanged)
    }

    /// The paths carrying a modification, as `collect_dirty_paths` reads them from the record. A
    /// deleted directory stands for what it held.
    ///
    /// A created directory that holds a node is left out, for the paths below it to stand for it,
    /// as they do wherever dirty paths are re-applied. Only an empty created directory is listed.
    /// A caller that must reach every changed path adds the directories leading to each path
    /// itself.
    pub async fn paths(&self) -> Result<Vec<RelativePath>, StateError> {
        let mut paths = Vec::new();
        collect_dirty_paths(
            self.state(),
            self.repository.clone(),
            ROOT_NODE,
            RelativePathBuf::new(),
            &mut paths,
        )
        .await?;
        Ok(paths)
    }

    async fn mark(&self, path: &str, flags: NodeFlags, directory: bool) -> Result<(), StateError> {
        let _updating = self.updating.lock().await;
        let state = self.state();
        let mut parent = ROOT_NODE;
        let segments = path_segments(path);
        for (index, segment) in segments.iter().copied().enumerate() {
            let is_leaf = index + 1 == segments.len();
            let name_hash = crate::hash::hash_string(segment);
            let existing = state
                .find_subnode(self.repository.clone(), parent, name_hash)
                .await
                .ok();
            if is_leaf {
                return self
                    .mark_leaf(&state, parent, existing, segment, flags, directory)
                    .await;
            }
            parent = if let Some(directory) = existing {
                directory
            } else {
                // A directory node carries no File or Link flag.
                let directory = Node {
                    name_hash,
                    ..Default::default()
                };
                let id = state
                    .node_add(self.repository.clone(), parent, directory, segment)
                    .await?;
                state
                    .node_mark(
                        &self.repository,
                        id,
                        Mark::dirty(NodeFlags::Dirty, Ext::Keep, true),
                    )
                    .await?;
                id
            };
        }
        Ok(())
    }

    async fn mark_leaf(
        &self,
        state: &Arc<State>,
        parent: NodeID,
        existing: Option<NodeID>,
        name: &str,
        flags: NodeFlags,
        directory: bool,
    ) -> Result<(), StateError> {
        let id = if let Some(id) = existing {
            // A write to a file this record knows is new leaves it new.
            let node = state.node(self.repository.clone(), id).await?;
            if flags == NodeFlags::DirtyModify && node.is_dirty_add() {
                return Ok(());
            }
            id
        } else {
            let node = Node {
                flags: if directory { 0 } else { NodeFlags::File.bits() },
                name_hash: crate::hash::hash_string(name),
                ..Default::default()
            };
            state
                .node_add(self.repository.clone(), parent, node, name)
                .await?
        };
        // Propagates up, so a walk that prunes clean directories still reaches this one.
        state
            .node_mark(&self.repository, id, Mark::dirty(flags, Ext::Keep, true))
            .await
    }
}

/// The segments of a `/`-separated path, collected: an iterator over it with a closure in it,
/// held across an await, leaves the future the compiler cannot show to be `Send`.
fn path_segments(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// What the record says of the node itself, apart from anything above it.
fn node_coverage(node: &Node) -> Coverage {
    // A path deleted and then created again, such as a directory a view narrowed away and a
    // widening brought back, is there.
    if node.is_dirty_delete() && !node.is_dirty_add() {
        Coverage::Deleted
    } else if has_action(node) {
        Coverage::Changed
    } else {
        Coverage::Unchanged
    }
}

fn has_action(node: &Node) -> bool {
    node.is_dirty_add()
        || node.is_dirty_modify()
        || node.is_dirty_delete()
        || node.is_dirty_move()
        || node.is_dirty_copy()
}
