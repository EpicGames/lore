// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;

use bitflags::bitflags;
use dashmap::DashMap;
use lore_base::types::BranchPoint;
use lore_error_set::prelude::*;
use lore_transport::Connection;
use serde::Deserialize;
use serde::Serialize;

use crate::bitflagsops;
use crate::branch;
use crate::change::NodeChange;
use crate::errors::*;
use crate::event::EventError;
use crate::filter::FilterMode;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::interface::LoreError;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_info;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::revision;
use crate::revision::sync;
use crate::revision::sync::SyncOptions;
use crate::revision::sync::SyncVerifyArgs;
use crate::revision::sync::sync_verify_filesystem;
use crate::stage;
use crate::state;
use crate::state::LinkReference;
use crate::state::State;
use crate::state::StateError;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;

pub mod add;
pub mod list;
pub mod remove;
pub(crate) mod reset;
pub mod update;

#[error_set]
pub enum LinkError {
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    RevisionNotFound,
    BranchNotFound,
    WriteRequired,
    Oversized,
    InvalidPath,
    InvalidNodeHierarchy,
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    NothingStaged,
    BranchAdvanced,
    Conflict,
    InvalidArguments,
    AlreadyLinked,
    LayerNotFound,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NoRemote,
    NotSupported,
    LinkPathNotFound,
    NotALink,
    NotALayer,
    BranchAlreadyExists,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    IdenticalMetadata,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NotConnected,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for LinkError {
    fn translated(&self) -> LoreError {
        match self {
            LinkError::Disconnected(_) => LoreError::Connection,
            LinkError::SlowDown(_) => LoreError::SlowDown,
            LinkError::Oversized(_) => LoreError::Oversized,
            LinkError::FileNotFound(_) => LoreError::FileNotFound,
            LinkError::NotFound(_)
            | LinkError::LayerNotFound(_)
            | LinkError::RevisionNotFound(_)
            | LinkError::BranchNotFound(_)
            | LinkError::LinkNotFound(_)
            | LinkError::LinkPathNotFound(_) => LoreError::NotFound,
            LinkError::AddressNotFound(_) => LoreError::AddressNotFound,
            LinkError::PayloadNotFound(_) => LoreError::PayloadNotFound,
            LinkError::InvalidPath(_) | LinkError::InvalidArguments(_) => {
                LoreError::InvalidArguments
            }
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Context information for discovered links during tree traversal
#[derive(Debug, Clone)]
pub struct LinkContext {
    /// The repository ID that the link points to
    pub link_repository_id: RepositoryId,
    /// The node ID of the link in the parent repository
    pub link_node_id: NodeID,
    /// The repository ID where the link resides
    pub parent_repository_id: RepositoryId,
    /// Path to the link from the parent repository root
    pub link_path: RelativePathBuf,
    /// The state of the linked repository
    pub link_state: Arc<State>,
}

impl PartialEq for LinkContext {
    fn eq(&self, other: &Self) -> bool {
        self.link_repository_id == other.link_repository_id
            && self.link_node_id == other.link_node_id
            && self.parent_repository_id == other.parent_repository_id
            && self.link_path == other.link_path
    }
}

impl Eq for LinkContext {}

impl std::hash::Hash for LinkContext {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.link_repository_id.hash(state);
        self.link_node_id.hash(state);
        self.parent_repository_id.hash(state);
        self.link_path.hash(state);
    }
}

/// Maps link contexts to whether they need rehashing
/// Uses `DashMap` for concurrent access without additional wrapper
#[derive(Debug, Default)]
pub struct LinkTracker {
    links: DashMap<LinkContext, bool>,
}

impl LinkTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn add_link(&self, link_context: LinkContext) {
        self.links.insert(link_context, false);
    }

    pub fn on_node_changed(&self, repository_id: RepositoryId) {
        for mut entry in self.links.iter_mut() {
            if entry.key().link_repository_id == repository_id {
                *entry.value_mut() = true;
            }
        }
    }

    pub fn get_links_needing_rehash(&self) -> Vec<LinkContext> {
        self.links
            .iter()
            .filter_map(|entry| {
                if *entry.value() {
                    Some(entry.key().clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn has_modifications(&self) -> bool {
        self.links.iter().any(|entry| *entry.value())
    }

    pub fn find_link_context(&self, repository_id: RepositoryId) -> Option<LinkContext> {
        self.links
            .iter()
            .find(|entry| entry.key().link_repository_id == repository_id)
            .map(|entry| entry.key().clone())
    }
}

/// Data for an event reporting a change to a link.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLinkChangeEventData {
    /// Path of the link within the parent repository.
    pub link_path: LoreString,
    /// Identifier of the repository the link points to.
    pub link_repository: RepositoryId,
    /// Identifier of the branch the link is pinned to.
    pub branch: BranchId,
    /// Hash of the revision the link is pinned to.
    pub revision: Hash,
    /// Kind of change applied to the link.
    pub action: LoreFileAction,
}

impl LoreLinkChangeEventData {
    fn new(
        link_path: &str,
        link_repository: RepositoryId,
        branch: BranchId,
        revision: Hash,
        action: LoreFileAction,
    ) -> Self {
        Self {
            link_path: if link_path.is_empty() {
                LoreString::from("/")
            } else {
                LoreString::from(link_path)
            },
            link_repository,
            branch,
            revision,
            action,
        }
    }
}

/// Data for an event describing a single link in a repository.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLinkEntryEventData {
    /// Identifier of the repository the link points to.
    pub link: RepositoryId,
    /// Identifier of the link node in the parent repository.
    pub link_node: u32,
    /// Path of the link within the parent repository.
    pub link_path: LoreString,
    /// Identifier of the source node in the linked repository.
    pub source_node: u32,
    /// Path of the source within the linked repository.
    pub source_path: LoreString,
    /// Identifier of the branch the link is pinned to.
    pub branch: BranchId,
    /// Name of the branch the link is pinned to.
    pub branch_name: LoreString,
    /// Hash of the revision the link is pinned to.
    pub revision: Hash,
    /// Link flags.
    pub flags: u32,
}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    pub struct LinkFlags: u32 {
        /// No flags
        const NoFlags = 0;

        /// Disable auto-follow for branch creation
        const DisableAutoFollow = 0b1;
    }
}
bitflagsops!(LinkFlags, u32);

pub async fn create_branch(
    repository: Arc<RepositoryContext>,
    remote: Arc<Connection>,
    branch_id: Context,
    branch_name: String,
    branch_category: String,
    parent_id: Context,
    parent_latest: Hash,
) -> Result<Hash, LinkError> {
    lore_debug!(
        "Creating link branch {branch_name} for link id {} at revision {parent_latest}",
        repository.id
    );

    let user_id = execution_context().user_id().await;

    let branch_stack = vec![BranchPoint {
        branch: parent_id,
        revision: parent_latest,
    }];

    let revision = remote
        .revision(repository.id)
        .await
        .forward::<LinkError>("Not connected")?;

    let branch_name =
        if let Ok(_previous_id) = branch::load_name_to_id(repository.clone(), &branch_name).await {
            lore_debug!("Link branch with name {branch_name} already exists, appending branch ID");
            format!("{branch_name}-{branch_id}")
        } else {
            branch_name
        };

    revision
        .branch_create(
            branch_id,
            &branch_name,
            &branch_category,
            user_id.as_str(),
            &branch_stack,
        )
        .await
        .forward::<LinkError>("Failed to create branch in linked repository")
}

pub async fn resolve_pin(
    link: Arc<RepositoryContext>,
    pin: String,
) -> Result<(Hash, Context), LinkError> {
    let pin_signature = revision::resolve(
        link.clone(),
        pin,
        execution_context().globals().search_limit(),
        execution_context().globals().search_location(),
    )
    .await
    .forward::<LinkError>("Invalid pin specified")?;

    let pin_state = State::deserialize(link.clone(), pin_signature)
        .await
        .forward::<LinkError>("Failed deserializing state")?;

    let pin_metadata = pin_state
        .revision_metadata(link.clone())
        .await
        .forward::<LinkError>("Failed getting revision metadata")?;

    lore_debug!(
        "Resolved link pin with revision {pin_signature} on branch {}",
        pin_metadata.branch
    );

    Ok((pin_signature, pin_metadata.branch))
}

/// Remaps change paths from the linked repository's source subtree to the
/// local link mount point. Strips the `source_path` prefix from each change
/// and replaces it with `link_path`.
pub fn remap_changes(
    link_path: RelativePath,
    source_path: RelativePath,
    changes: Vec<NodeChange>,
) -> Arc<Vec<NodeChange>> {
    let mut changes = changes;
    let prefix_len = source_path.len();

    let remap = |path: &RelativePath| -> RelativePath {
        RelativePath::new_from_clean_parts(link_path.as_str(), &path.as_str()[prefix_len..])
    };

    for change in changes.iter_mut() {
        change.path = remap(&change.path);
        if let Some(from_path) = change.from_path.as_mut() {
            *from_path = remap(from_path);
        }
    }

    Arc::new(changes)
}

/// Updates a link pin in the block tree and link registry for a pre-resolved node.
///
/// Writes `new_signature` to the module node's `address.hash` in the block tree
/// and calls `link_update` to update the link registry. This is the common pattern
/// used after any operation that changes a linked repository's revision (branch
/// creation, stage, commit, unstage).
pub async fn update_link_pin_by_node(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
    link_repo_id: RepositoryId,
    branch: BranchId,
    new_signature: Hash,
    node_id: NodeID,
) -> Result<(), StateError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);

    let block = state.block(repository.clone(), block_index).await?;

    {
        let mut block_writer = block.write();
        let node = block_writer.node(node_index);
        node.address.hash = new_signature;
        block_writer.mark_dirty();
    }

    // Always re-mark the state as dirty here, even when `mark_dirty()` reports
    // the block was already dirty. `State::serialize` clears
    // `NodeBlockFlags::Dirty` only on the on-disk clone it writes; the
    // in-memory block keeps the flag set across serialize calls. So a sequence
    // of `update_link_pin_by_node` -> `serialize` -> `update_link_pin_by_node`
    // -> `serialize` would leave the state-level dirty flag clear on the
    // second pass and `State::serialize` would early-return the previous
    // hash. This bit is what `merge_resolve` hits when resolving multiple
    // paths inside a link in one CLI invocation.
    state.block_modified(block.clone(), block_index);
    state.mark_dirty();

    state
        .link_update(repository, link_repo_id, branch, new_signature, node_id)
        .await
}

/// Reserializes a tracked link's state and updates the parent repository's
/// block tree and link registry.
///
/// This is the shared workflow between `process_link_updates` (stage) and
/// `process_link_unstage_updates` (unstage):
/// 1. Get linked repository context via `to_link_context`
/// 2. Set parent on linked state and reset revision number
/// 3. Serialize the linked state
/// 4. Update the block tree hash and link registry via `update_link_pin_by_node`
///
/// Returns the new signature so callers can use it for additional work.
pub async fn reserialize_tracked_link(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    link_context: &LinkContext,
    parent_signature: Hash,
    branch: BranchId,
) -> Result<Hash, StateError> {
    let linked_repository = Arc::new(
        repository
            .to_link_context(link_context.link_repository_id)
            .await,
    );

    let linked_state = link_context.link_state.clone();
    linked_state.set_parent_self(parent_signature);
    linked_state.set_revision_number(0);

    let new_signature = linked_state.serialize(linked_repository, token).await?;

    update_link_pin_by_node(
        state,
        repository,
        link_context.link_repository_id,
        branch,
        new_signature,
        link_context.link_node_id,
    )
    .await?;

    Ok(new_signature)
}

/// Force-realizes specific file paths from a linked repository state at the
/// mount path, regardless of any state-to-state diff.
///
/// Restore on-disk content for `paths` (link-relative) from `link_state`
/// and unlink any `.mine`/`.theirs`/`.base` sidecars at the same paths.
///
/// Used during merge abort to clean up filesystem-only artifacts: marker
/// bytes inside conflicted file contents and sidecar files. The
/// state-to-state diff from `realize_link_pin_change` doesn't touch these
/// (sidecars aren't in any state, and marker bytes inside a file are not
/// produced by `realize_changes` — they're produced by `realize_conflicts`).
///
/// `link_path` is the link's mount path in the parent repository. Paths
/// are remapped under it for absolute filesystem access.
pub async fn restore_link_paths_from_state(
    repository: Arc<RepositoryContext>,
    link_context: Arc<RepositoryContext>,
    link_path: RelativePath,
    link_state: Arc<State>,
    paths: &[RelativePath],
) -> Result<(), LinkError> {
    if paths.is_empty() {
        return Ok(());
    }

    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .forward::<LinkError>("Failed starting filesystem operation")?;

    for link_relative in paths {
        let mount_relative = link_path.join(link_relative.as_str());
        let absolute = mount_relative.to_absolute_path(repository.require_path()?);
        sync::unlink_merge_mine_theirs_base(absolute.as_path()).await;

        let node_link = link_state
            .find_node_link(link_context.clone(), link_relative.as_str())
            .await
            .forward::<LinkError>("Failed resolving link node")?;
        if !node_link.is_valid_or_root() {
            continue;
        }
        let block = link_state
            .block(link_context.clone(), NodeBlock::index(node_link.node))
            .await
            .forward::<LinkError>("Failed deserializing state node block")?;
        let node = block.node(Node::index(node_link.node));
        if !node.is_file() {
            continue;
        }
        crate::fs::realize::realize_file(
            link_context.clone(),
            operation.clone(),
            &mount_relative,
            node,
            Arc::default(),
        )
        .await
        .forward::<LinkError>("Failed synchronizing link changes")?;
    }

    operation
        .finalize(true)
        .await
        .forward::<LinkError>("Failed finalizing filesystem operation")?;

    Ok(())
}

/// Returns the absolute path of every staged link node (add, delete, or
/// update) across both states' link registries.
pub async fn list_staged_node_paths(
    state_current: &Arc<State>,
    state_staged: &Arc<State>,
    repository: Arc<RepositoryContext>,
) -> Result<Vec<String>, LinkError> {
    let staged_links = state_staged
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list staged links")?;
    let current_links = state_current
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list current links")?;

    let mut seen: std::collections::HashSet<NodeID> = std::collections::HashSet::new();
    let mut paths = Vec::new();

    for link_reference in staged_links.iter().chain(current_links.iter()) {
        if !seen.insert(link_reference.local_node) {
            continue;
        }
        let Ok(node) = state_staged
            .node(repository.clone(), link_reference.local_node)
            .await
        else {
            continue;
        };
        if !node.is_staged() {
            continue;
        }
        let Ok(path) = state_staged
            .node_path(repository.clone(), link_reference.local_node)
            .await
        else {
            continue;
        };
        let absolute = repository.require_path()?.join(&path);
        paths.push(absolute.to_string_lossy().into_owned());
    }
    Ok(paths)
}

/// Returns true if `staged_node` is a staged link pin change (an update to an
/// existing link, not an add or remove) with no staged changes on the linked
/// side.
pub async fn is_staged_pin_change(
    staged_node: &Node,
    current_node: &Node,
    parent_repository: Arc<RepositoryContext>,
) -> Result<bool, LinkError> {
    if !(staged_node.is_link()
        && staged_node.is_staged()
        && !staged_node.is_staged_add()
        && !staged_node.is_staged_delete()
        && staged_node.address.hash != current_node.address.hash)
    {
        return Ok(false);
    }

    let link = staged_node.linked_node();
    let linked_repository = Arc::new(parent_repository.to_link_context(link.repository).await);
    let linked_state = State::deserialize(linked_repository.clone(), link.revision)
        .await
        .forward::<LinkError>("Failed deserializing linked state")?;
    let has_staged_children = linked_state
        .node_has_staged_children(linked_repository, link.node)
        .await
        .forward::<LinkError>("Failed checking linked node children")?;
    Ok(!has_staged_children)
}

/// Realizes on-disk content changes when a link pin changes.
///
/// Deserializes the old and new link states, computes a 2-way diff scoped to
/// the linked node, remaps change paths to the mount point, verifies filesystem
/// consistency, and realizes the changes on disk.
pub async fn realize_link_pin_change(
    repository: Arc<RepositoryContext>,
    link_context: Arc<RepositoryContext>,
    link_path: RelativePath,
    old_sig: Hash,
    new_sig: Hash,
    linked_node: NodeID,
) -> Result<(), LinkError> {
    lore_debug!("Load link revision states");
    let link_state_current = state::State::deserialize(link_context.clone(), old_sig)
        .await
        .forward::<LinkError>("Failed deserializing state")?;

    let link_state_target = state::State::deserialize(link_context.clone(), new_sig)
        .await
        .forward::<LinkError>("Failed deserializing state")?;

    lore_debug!("Find link target node");
    let linked_node_path = link_state_current
        .node_path(link_context.clone(), linked_node)
        .await
        .forward::<LinkError>("Failed resolving link node")?;

    let Ok(linked_node_path) = RelativePath::from_str(&linked_node_path);

    let changes = state::diff_collect(
        link_context.clone(),
        link_state_current.clone(),
        link_context.clone(),
        link_state_target.clone(),
        Some(linked_node_path.clone()),
        FilterMode::View,
    )
    .await
    .forward::<LinkError>("Failed syncing target link")?;

    lore_debug!("Remap changes to link path {}", link_path.as_str());
    let changes = remap_changes(link_path, linked_node_path, changes);

    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .forward::<LinkError>("Failed starting filesystem operation")?;

    let changes = if !changes.is_empty() {
        lore_info!(
            "Verifying {} link changes with local file system",
            changes.len()
        );

        let options = Arc::new(SyncOptions {
            revision: Some(new_sig.to_string()),
            ..Default::default()
        });

        sync_verify_filesystem(
            link_context.clone(),
            Arc::new(SyncVerifyArgs {
                changes: changes.clone(),
                repository_current: link_context.clone(),
                operation: operation.clone(),
                state_current: link_state_current.clone(),
                options: options.clone(),
            }),
        )
        .await
        .forward::<LinkError>("Failed verifying local file system")?
    } else {
        changes
    };

    let stats: Arc<sync::SyncRealizeStats> = Arc::default();

    lore_debug!("Realize link changes");

    crate::fs::realize::realize_changes(
        repository,
        operation.clone(),
        changes,
        None,
        false, /* Not dry run */
        false, /* Not a merge */
        stats,
    )
    .await
    .forward::<LinkError>("Failed synchronizing link changes")?;

    operation
        .finalize(true)
        .await
        .forward::<LinkError>("Failed finalizing filesystem operation")?;

    Ok(())
}

/// Result of resolving a link path to its full context.
pub struct ResolvedLink {
    /// The module node at the link path
    pub link_node: Node,
    /// The linked repository context
    pub link_context: Arc<RepositoryContext>,
    /// The link reference metadata from the link registry
    pub link_reference: LinkReference,
}

/// Resolves a link by path to its full context.
///
/// Finds the node via `find_node_link`, validates it's a module, creates
/// the linked repository context via `to_link_context`, and looks up
/// the `LinkReference`. Returns all three in a `ResolvedLink`.
pub async fn resolve_link_at_path(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
    link_path: &str,
) -> Result<ResolvedLink, LinkError> {
    let node_link = state
        .find_node_link(repository.clone(), link_path)
        .await
        .forward::<LinkError>("Invalid path")?;

    if !node_link.is_valid() {
        return Err(InvalidPath {
            path: link_path.to_string(),
        }
        .into());
    }

    let link_node = state
        .node(repository.clone(), node_link.node)
        .await
        .forward::<LinkError>("Failed deserializing state")?;

    if !link_node.is_link() {
        return Err(NotALink {
            path: link_path.to_string(),
        }
        .into());
    }

    let link_context = Arc::new(
        repository
            .to_link_context(link_node.address.context.into())
            .await,
    );

    let link_reference = state
        .link_find(repository.clone(), link_context.id, node_link.node)
        .await
        .forward::<LinkError>("Failed to find link")?;

    Ok(ResolvedLink {
        link_node,
        link_context,
        link_reference,
    })
}

/// Updates a link pin when only the mount path is known.
///
/// Finds the module node at the mount path, then delegates to
/// `update_link_pin_by_node` to update the block tree and link registry.
pub async fn update_link_pin_by_path(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
    link_path: &str,
    branch: BranchId,
    new_signature: Hash,
) -> Result<(), LinkError> {
    let resolved = resolve_link_at_path(state, repository.clone(), link_path).await?;

    update_link_pin_by_node(
        state,
        repository,
        resolved.link_context.id,
        branch,
        new_signature,
        resolved.link_reference.local_node,
    )
    .await
    .forward::<LinkError>("Failed to update link")
}

/// Atomically updates a link pin in the parent repository state.
///
/// Performs the common sequence of: realize on-disk content at the mount path,
/// stage the updated link node, and update the link registry. Does not serialize
/// state or flush the anchor — the caller is responsible for that.
#[allow(clippy::too_many_arguments)]
pub async fn stage_link_pin(
    repository: Arc<RepositoryContext>,
    state: &Arc<State>,
    link_context: &Arc<RepositoryContext>,
    link_path: RelativePath,
    link_node: Node,
    old_signature: Hash,
    new_signature: Hash,
    new_branch: BranchId,
) -> Result<NodeID, LinkError> {
    // Resolve the source_node in the new link state. The parent's link node
    // stores a NodeID (`link_node.child`) that points into the linked state's
    // tree at the mount's source_path. Node IDs aren't stable across link
    // revisions, so after a merge the old child ID may be stale (e.g. pointing
    // at a deleted node, or at an unrelated node in the new state). Look up
    // the source_path in the old state, then resolve it fresh in the new state
    // so clone/switch can walk the correct subtree.
    let link_state_old = state::State::deserialize(link_context.clone(), old_signature)
        .await
        .forward::<LinkError>("Failed deserializing state")?;
    let source_path = link_state_old
        .node_path(link_context.clone(), link_node.child)
        .await
        .forward::<LinkError>("Failed resolving link node")?;

    let link_state_new = state::State::deserialize(link_context.clone(), new_signature)
        .await
        .forward::<LinkError>("Failed deserializing state")?;
    let new_source_link = link_state_new
        .find_node_link(link_context.clone(), source_path.as_str())
        .await
        .forward::<LinkError>("Invalid path")?;
    if !new_source_link.is_valid_or_root() {
        return Err(InvalidPath { path: source_path }.into());
    }
    let new_source_node = new_source_link.node;

    // Realize on-disk content
    realize_link_pin_change(
        repository.clone(),
        link_context.clone(),
        link_path.clone(),
        old_signature,
        new_signature,
        link_node.child,
    )
    .await?;

    // Stage the link node with updated revision hash
    let mut staged_node = link_node;
    staged_node.address.hash = new_signature;
    staged_node.child = new_source_node;

    let staged_link_node = stage::stage_single_node(
        repository.clone(),
        state.clone(),
        link_path,
        staged_node,
        Arc::default(),
        None,
        FilterMode::View,
    )
    .await
    .forward::<LinkError>("Failed staging the link node")?;

    // Update link pin in the link registry
    state
        .link_update(
            repository.clone(),
            link_context.id,
            new_branch,
            new_signature,
            staged_link_node.node,
        )
        .await
        .forward::<LinkError>("Failed to update link")?;

    Ok(staged_link_node.node)
}

/// Result of checking whether a link is eligible for a merge operation.
pub enum LinkMergeEligibility {
    /// The link is eligible for merge.
    Eligible,
    /// The link should be silently skipped (inaccessible remote or branch not found).
    Skip,
    /// The link has auto-follow disabled — this is a hard error.
    AutoFollowDisabled,
}

/// Checks whether a linked repository is eligible for merge operations.
///
/// Returns `Eligible` if the link is auto-follow enabled, the remote is
/// accessible, and the target branch exists. Returns `Skip` for silent
/// skips (inaccessible link or branch not found). Returns
/// `AutoFollowDisabled` when the link has auto-follow off.
pub async fn check_link_merge_eligible(
    link_context: &Arc<RepositoryContext>,
    link_reference: &LinkReference,
    target_branch: BranchId,
) -> LinkMergeEligibility {
    if link_reference.flags & LinkFlags::DisableAutoFollow != 0 {
        return LinkMergeEligibility::AutoFollowDisabled;
    }

    let link_remote = match link_context.remote().await {
        Ok(remote) => remote,
        Err(_) => return LinkMergeEligibility::Skip,
    };

    if branch::load_remote_latest(link_remote, link_context.id, target_branch)
        .await
        .is_err()
    {
        return LinkMergeEligibility::Skip;
    }

    LinkMergeEligibility::Eligible
}

/// Checks whether a linked repository has content divergence between two branches.
///
/// Computes a diff3 between the source branch revision and the current pin revision.
/// Returns `true` if there are changes or conflicts that require a real merge.
/// Returns `false` if the branches haven't diverged (e.g., one side is ahead of the
/// other with no conflicting changes), meaning a merge would produce no content changes
/// and can be skipped.
pub async fn link_has_content_divergence(
    link_context: &Arc<RepositoryContext>,
    source_branch: BranchId,
    source_revision: Hash,
    current_branch: BranchId,
    current_revision: Hash,
) -> bool {
    let diff = Box::pin(crate::branch::diff3_collect(
        link_context.clone(),
        source_branch,
        source_revision,
        current_branch,
        current_revision,
        None,
        false,
        false,
    ))
    .await;

    match diff {
        Ok(d) => !d.changes.is_empty() || !d.conflicts.is_empty(),
        Err(e) => {
            // Log the failure so it doesn't disappear silently — assuming
            // divergence here means the caller will create a synthetic merge
            // revision when the cause was actually a transient failure
            // (network, missing fragment). Surface enough context for a
            // user looking at logs to find this site.
            crate::lore_warn!(
                "link_has_content_divergence: diff3 failed for repo {}, \
                 source revision {source_revision} on branch {source_branch}, \
                 current revision {current_revision} on branch {current_branch}. \
                 Assuming divergence. Cause: {e}",
                link_context.id
            );
            true
        }
    }
}

/// Extracts the mount prefix from a full path given a link-relative path.
///
/// Given a full path through a mount point (e.g. `linked/repo/src/data.txt`)
/// and a link-relative path (e.g. `src/data.txt`), returns the mount prefix
/// (`linked/repo`).
pub fn link_mount_prefix(full_path: &str, link_relative: &str) -> String {
    if link_relative.is_empty() {
        return full_path.to_string();
    }
    let trimmed = full_path
        .strip_suffix(link_relative)
        .unwrap_or(full_path)
        .trim_end_matches('/');
    trimmed.to_string()
}
