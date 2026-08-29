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
use crate::event;
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
use crate::util::path::RepositoryPath;
use crate::util::serde::u8_as_bool;

pub mod add;
pub mod info;
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

    /// All tracked link contexts, regardless of whether they were marked as
    /// modified. Used to resolve parent states when folding a nested change up
    /// through intermediate links that were crossed but not directly modified.
    pub fn all_links(&self) -> Vec<LinkContext> {
        self.links.iter().map(|entry| entry.key().clone()).collect()
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

/// A link mounted at the repository root carries an empty path; report it as
/// `/` so every link event names a path.
fn event_link_path(link_path: &str) -> LoreString {
    if link_path.is_empty() {
        LoreString::from("/")
    } else {
        LoreString::from(link_path)
    }
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
            link_path: event_link_path(link_path),
            link_repository,
            branch,
            revision,
            action,
        }
    }
}

/// Data for an event reporting how a link's branch was resolved in the linked
/// repository. A repository can be linked at more than one mount path, so a
/// consumer must key on `link_path` together with `link_repository`.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLinkBranchCreateEventData {
    /// Path of the link within the parent repository.
    pub link_path: LoreString,
    /// Identifier of the repository the link points to.
    pub link_repository: RepositoryId,
    /// Identifier of the branch in the linked repository.
    pub branch: BranchId,
    /// Hash of the latest revision on that branch.
    pub revision: Hash,
    /// Set when a branch with this identifier was already present and was
    /// reused rather than created.
    #[serde(with = "u8_as_bool")]
    pub reused: u8,
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

/// How a link's branch was resolved in the linked repository.
///
/// A repository can be linked at more than one mount path, but the branch has a
/// single identity within it, so one outcome describes every mount that follows
/// the same repository.
pub struct LinkBranchOutcome {
    /// Hash of the latest revision on the resolved branch.
    pub revision: Hash,
    /// Set when a branch with this identifier was already present and was
    /// reused rather than created.
    pub reused: bool,
}

pub async fn create_branch(
    repository: Arc<RepositoryContext>,
    remote: Arc<Connection>,
    branch_id: Context,
    branch_name: String,
    branch_category: String,
    parent_id: Context,
    parent_latest: Hash,
) -> Result<LinkBranchOutcome, LinkError> {
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

    // The linked repository already owns this branch ID, so adopt it instead of
    // failing the whole cascade.
    if let Ok(existing) = branch::load_remote(remote.clone(), repository.id, branch_id).await
        && !existing.deleted
    {
        return Ok(LinkBranchOutcome {
            revision: existing.latest,
            reused: true,
        });
    }

    let branch_name =
        if let Ok(_previous_id) = branch::load_name_to_id(repository.clone(), &branch_name).await {
            lore_debug!("Link branch with name {branch_name} already exists, appending branch ID");
            format!("{branch_name}-{branch_id}")
        } else {
            branch_name
        };

    let latest = revision
        .branch_create(
            branch_id,
            &branch_name,
            &branch_category,
            user_id.as_str(),
            &branch_stack,
        )
        .await
        .forward::<LinkError>("Failed to create branch in linked repository")?;

    Ok(LinkBranchOutcome {
        revision: latest,
        reused: false,
    })
}

pub(crate) fn report_branch_outcome(
    link_path: &str,
    link_repository: RepositoryId,
    branch: BranchId,
    revision: Hash,
    reused: bool,
) {
    event::LoreEvent::LinkBranchCreate(LoreLinkBranchCreateEventData {
        link_path: event_link_path(link_path),
        link_repository,
        branch,
        revision,
        reused: reused as u8,
    })
    .send();
}

/// A linked repository a branch cascade acts on, with a mount path for
/// reporting.
pub struct LinkTarget {
    pub path: String,
    pub repository: RepositoryId,
    pub context: Arc<RepositoryContext>,
}

/// Groups the mounts in `link_list` by linked repository, skipping links that
/// opted out of following the parent's branches.
///
/// A repository can be linked at more than one mount path while the branch has a
/// single identity within it, so a cascade acts once per repository rather than
/// once per mount.
pub fn auto_following_mounts(
    link_list: &[LinkReference],
) -> Vec<(RepositoryId, Vec<LinkReference>)> {
    let mut groups: Vec<(RepositoryId, Vec<LinkReference>)> = Vec::new();

    for link_reference in link_list.iter() {
        if link_reference.flags & LinkFlags::DisableAutoFollow != 0 {
            lore_debug!(
                "Auto follow disabled for link {}",
                link_reference.repository
            );
            continue;
        }

        match groups
            .iter_mut()
            .find(|(link_id, _mounts)| *link_id == link_reference.repository)
        {
            Some((_link_id, mounts)) => mounts.push(*link_reference),
            None => groups.push((link_reference.repository, vec![*link_reference])),
        }
    }

    groups
}

/// For operations that cascade into every linked repository, including the
/// links nested inside them.
///
/// `branch create` only seeds the top level, so the deeper repositories are
/// reached here for the sake of converging on whatever a branch has been left
/// in, however it got there.
pub async fn list_with_context(
    repository: Arc<RepositoryContext>,
) -> Result<Vec<LinkTarget>, LinkError> {
    let (state, _parent_branch) = current_or_staged_state(repository.clone()).await?;

    let mut seen = std::collections::HashSet::from([repository.id]);
    Box::pin(collect_with_context(
        repository,
        state,
        String::new(),
        0,
        &mut seen,
    ))
    .await
}

/// Bounded by `MAX_LINK_DEPTH` and a visited-repository set, so a link cycle
/// terminates and a repository reachable by several paths is acted on once.
///
/// Each context costs its own connection, so the links at one level are opened
/// concurrently rather than one handshake after another.
async fn collect_with_context(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    path_prefix: String,
    link_depth: usize,
    seen: &mut std::collections::HashSet<RepositoryId>,
) -> Result<Vec<LinkTarget>, LinkError> {
    let link_list = state
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list links")?;

    let mounts: Vec<(RepositoryId, Vec<LinkReference>)> = auto_following_mounts(&link_list)
        .into_iter()
        .filter(|(link_id, _mounts)| seen.insert(*link_id))
        .collect();

    let level = futures::future::join_all(mounts.into_iter().map(|(link_id, mounts)| {
        let repository = repository.clone();
        let state = state.clone();
        let path_prefix = path_prefix.clone();
        async move {
            let local_path = state
                .node_path(repository.clone(), mounts[0].local_node)
                .await
                .unwrap_or_default();

            let target = LinkTarget {
                path: if path_prefix.is_empty() {
                    local_path
                } else {
                    format!("{path_prefix}/{local_path}")
                },
                repository: link_id,
                context: Arc::new(repository.to_link_context(link_id).await),
            };

            (target, mounts[0].signature)
        }
    }))
    .await;

    let mut targets = Vec::new();
    for (target, signature) in level {
        let nested = if link_depth + 1 < crate::state::MAX_LINK_DEPTH {
            let context = target.context.clone();
            let nested_state = State::deserialize(context.clone(), signature)
                .await
                .forward::<LinkError>("Failed deserializing state")?;

            Box::pin(collect_with_context(
                context,
                nested_state,
                target.path.clone(),
                link_depth + 1,
                seen,
            ))
            .await?
        } else {
            Vec::new()
        };

        targets.push(target);
        targets.extend(nested);
    }

    Ok(targets)
}

/// Resolves the link mounted at `path` for a cascade scoped to one link.
///
/// Naming a link that opted out of following the parent's branches is refused
/// rather than honoured: the cascade that would have put the branch there never
/// ran, so acting on it would delete a branch this repository never created.
pub async fn find_with_context(
    repository: Arc<RepositoryContext>,
    path: &str,
) -> Result<LinkTarget, LinkError> {
    let (state, _parent_branch) = current_or_staged_state(repository.clone()).await?;
    let resolved = resolve_link_at_path(&state, repository, path).await?;

    if resolved.link_reference.flags & LinkFlags::DisableAutoFollow != 0 {
        return Err(InvalidArguments {
            reason: format!("link at {path} does not follow the parent's branches"),
        }
        .into());
    }

    Ok(LinkTarget {
        path: path.to_string(),
        repository: resolved.link_context.id,
        context: resolved.link_context,
    })
}

/// The staged state when there is one, otherwise the current state, together
/// with the branch they belong to.
pub(crate) async fn current_or_staged_state(
    repository: Arc<RepositoryContext>,
) -> Result<(Arc<State>, BranchId), LinkError> {
    let (current, staged, branch) = State::deserialize_current_and_staged(repository)
        .await
        .forward::<LinkError>("Failed deserializing state")?;

    Ok((staged.unwrap_or(current), branch))
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
    // the block was already dirty. `NodeBlockFlags::Dirty` is runtime state that
    // is never written, and `State::serialize` leaves it set on the in-memory
    // block until it has written everything. So a sequence
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

/// One level of a resolved link chain: a parent repository and the link node
/// mounting a child. Carries the parent's mutable staged state so mutations
/// made deeper in the chain accumulate before reserialization.
pub struct LinkChainLevel {
    pub repository: Arc<RepositoryContext>,
    pub state: Arc<State>,
    pub link_node_id: NodeID,
    pub child_repository_id: RepositoryId,
    pub branch: BranchId,
    /// Child's committed pin; used as parent-self when reserializing.
    pub old_signature: Hash,
}

/// A link path resolved through zero or more link boundaries. `levels` is empty
/// for a plain top-level path, in which case `innermost_state` is the caller's
/// top-level staged state.
pub struct ResolvedLinkChain {
    /// Ordered outer -> inner.
    pub levels: Vec<LinkChainLevel>,
    pub innermost_repository: Arc<RepositoryContext>,
    pub innermost_state: Arc<State>,
    /// The innermost repository's committed state. Carries registry entries that
    /// the staged state has already dropped, such as a link staged for removal.
    pub innermost_current_state: Arc<State>,
    /// Node the `remainder_path` is rooted at in the innermost repository.
    pub innermost_base_node: NodeID,
    /// `link_path` minus `remainder_path`; empty for the top level.
    pub innermost_mount_path: RelativePathBuf,
    /// Target path within the innermost repository.
    pub remainder_path: RelativePathBuf,
}

/// Resolve `link_path` down through any link boundaries into the crossed links
/// and the innermost containing repo, state and remainder path. Like
/// `find_relative_node_link` but records each crossed link. Bounded by
/// `MAX_LINK_DEPTH` and a visited-repository set.
pub async fn resolve_link_chain(
    repository: Arc<RepositoryContext>,
    state_staged: Arc<State>,
    state_current: Arc<State>,
    link_path: RelativePath,
    parent_branch: BranchId,
) -> Result<ResolvedLinkChain, LinkError> {
    let mut levels: Vec<LinkChainLevel> = Vec::new();
    let mut seen: std::collections::HashSet<RepositoryId> = std::collections::HashSet::new();
    seen.insert(repository.id);

    let mut cur_repository = repository;
    let mut cur_state = state_staged;
    let mut cur_current_state = state_current;
    let mut cur_branch = parent_branch;
    let mut remaining = RelativePath::from_str(link_path.as_str()).unwrap_or_default();
    let mut cur_node = crate::node::ROOT_NODE;
    let mut base_node = crate::node::ROOT_NODE;
    let mut mount_prefix = RelativePathBuf::new();
    let mut consumed = RelativePathBuf::new();
    let mut remainder = RelativePathBuf::new();

    while !remaining.is_empty() {
        let name = remaining.pop_root();
        let name_hash = crate::hash::hash_string(name);
        remainder.push(name);
        consumed.push(name);

        // A segment that does not resolve is the boundary of what exists: the
        // current level is innermost and the rest is the target to create. This
        // is the `link add` case, whose target does not exist yet.
        if let Ok(node_id) = cur_state
            .find_subnode(cur_repository.clone(), cur_node, name_hash)
            .await
        {
            cur_node = node_id;
        } else {
            while !remaining.is_empty() {
                remainder.push(remaining.pop_root());
            }
            break;
        }

        if remaining.is_empty() {
            break;
        }

        let node = cur_state
            .node(cur_repository.clone(), cur_node)
            .await
            .forward::<LinkError>("Invalid path")?;

        if node.is_link() {
            let link = node.linked_node();
            let child_repository_id = link.repository;

            // Match the read-side bound (`link_depth < MAX_LINK_DEPTH`): allow
            // up to MAX_LINK_DEPTH crossed links before rejecting the next.
            if levels.len() >= state::MAX_LINK_DEPTH {
                return Err(LinkError::internal("Maximum link nesting depth exceeded"));
            }
            if !seen.insert(child_repository_id) {
                return Err(LinkError::internal("Cyclic link nesting detected"));
            }

            let link_reference = cur_state
                .link_find(cur_repository.clone(), child_repository_id, cur_node)
                .await
                .forward::<LinkError>("Failed to find link")?;
            let resolved_branch = link_reference.resolve_branch(cur_branch);

            // Committed pin, or the staged pin for a freshly added link.
            let old_signature = match cur_current_state
                .link_find(cur_repository.clone(), child_repository_id, cur_node)
                .await
            {
                Ok(reference) => reference.signature,
                Err(_) => link_reference.signature,
            };

            let child_repository =
                Arc::new(cur_repository.to_link_context(child_repository_id).await);
            let child_state = State::deserialize(child_repository.clone(), link.revision)
                .await
                .forward::<LinkError>("Failed deserializing state")?;
            let child_current_state = State::deserialize(child_repository.clone(), old_signature)
                .await
                .forward::<LinkError>("Failed deserializing state")?;

            levels.push(LinkChainLevel {
                repository: cur_repository.clone(),
                state: cur_state.clone(),
                link_node_id: cur_node,
                child_repository_id,
                branch: resolved_branch,
                old_signature,
            });

            cur_repository = child_repository;
            cur_state = child_state;
            cur_current_state = child_current_state;
            cur_branch = resolved_branch;
            cur_node = link.node;
            base_node = link.node;
            mount_prefix = consumed.clone();
            remainder = RelativePathBuf::new();
        }
    }

    Ok(ResolvedLinkChain {
        levels,
        innermost_repository: cur_repository,
        innermost_state: cur_state,
        innermost_current_state: cur_current_state,
        innermost_base_node: base_node,
        innermost_mount_path: mount_prefix,
        remainder_path: remainder,
    })
}

impl ResolvedLinkChain {
    /// The child (state, repository) mounted by level `index`: the next level's
    /// parent for an intermediate level, or the innermost state/repository for
    /// the last level. Used by the inner -> outer folding passes.
    pub fn child_at(&self, index: usize) -> (Arc<State>, Arc<RepositoryContext>) {
        if index + 1 < self.levels.len() {
            (
                self.levels[index + 1].state.clone(),
                self.levels[index + 1].repository.clone(),
            )
        } else {
            (
                self.innermost_state.clone(),
                self.innermost_repository.clone(),
            )
        }
    }

    /// Register a `LinkContext` in `tracker` for every link crossed by this
    /// chain, so a filesystem-walk operation (stage / unstage) folds a nested
    /// change up through all intermediate links. Each level's child state is
    /// the next level's parent state, or `innermost_state` for the last level;
    /// `link_path` is the link node's path within its own parent repository
    /// (its `node_path` there, falling back to `fallback_path`).
    ///
    /// `innermost_state` must be the exact state instance the caller mutates
    /// (the one the deep change is staged into), so the tracker reserializes
    /// the changes rather than a fresh deserialization of the same revision.
    pub async fn record_tracker_contexts(
        &self,
        tracker: &LinkTracker,
        innermost_state: &Arc<State>,
        fallback_path: &str,
    ) {
        for (index, level) in self.levels.iter().enumerate() {
            let child_state = if index + 1 < self.levels.len() {
                self.levels[index + 1].state.clone()
            } else {
                innermost_state.clone()
            };
            let level_path = level
                .state
                .node_path(level.repository.clone(), level.link_node_id)
                .await
                .unwrap_or_else(|_| fallback_path.to_string());
            tracker.add_link(LinkContext {
                link_repository_id: level.child_repository_id,
                link_node_id: level.link_node_id,
                parent_repository_id: level.repository.id,
                link_path: RelativePathBuf::new_from_initial_path(level_path.as_str())
                    .unwrap_or_default(),
                link_state: child_state,
            });
        }
    }
}

/// Reserialize each linked state inner -> outer and fold its new signature into
/// its parent's link node and registry, so a change made in the innermost repo
/// is captured transitively by the top-level staged anchor.
///
/// No-op when `chain.levels` is empty (plain top-level operation). The caller
/// is responsible for serializing + anchoring the top-level state afterwards.
pub async fn propagate_link_chain(
    chain: &ResolvedLinkChain,
    token: &RepositoryWriteToken,
) -> Result<(), LinkError> {
    // Inner -> outer: serialize each child before repinning its parent.
    for i in (0..chain.levels.len()).rev() {
        let level = &chain.levels[i];
        let (child_state, child_repository) = chain.child_at(i);

        child_state.set_parent_self(level.old_signature);
        child_state.set_revision_number(0);
        let new_signature = child_state
            .serialize(child_repository, token)
            .await
            .forward::<LinkError>("Failed to serialize linked state")?;

        update_link_pin_by_node(
            &level.state,
            level.repository.clone(),
            level.child_repository_id,
            level.branch,
            new_signature,
            level.link_node_id,
        )
        .await
        .forward::<LinkError>("Failed to update link pin")?;

        level
            .state
            .node_mark(
                level.repository.clone(),
                level.link_node_id,
                crate::node::NodeFlags::Staged,
                true,
            )
            .await
            .forward::<LinkError>("Failed to mark link node staged")?;
    }

    Ok(())
}

/// Reserialize every link modified during a `stage`/`unstage` walk, folding
/// each linked repository's new revision into its parent's pin. Processes
/// innermost-first and closes over ancestors (if C changed, B must reserialize
/// so A's pin updates). `mark_staged` re-marks link nodes on stage; unstage
/// manages its own staged flags.
pub async fn drain_link_tracker(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state_current: Arc<State>,
    state: Arc<State>,
    link_tracker: &LinkTracker,
    mark_staged: bool,
) -> Result<(), StateError> {
    if !link_tracker.has_modifications() {
        return Ok(());
    }

    let all_contexts = link_tracker.all_links();

    // Modified links plus their transitive ancestors.
    let mut selected: Vec<LinkContext> = link_tracker.get_links_needing_rehash();
    let mut i = 0;
    while i < selected.len() {
        let parent_repo_id = selected[i].parent_repository_id;
        if parent_repo_id != repository.id
            && let Some(parent_ctx) = all_contexts
                .iter()
                .find(|other| other.link_repository_id == parent_repo_id)
            && !selected.iter().any(|s| s == parent_ctx)
        {
            selected.push(parent_ctx.clone());
        }
        i += 1;
    }

    let mut contexts = selected;

    // Order deepest-nested first so each child is reserialized before the
    // parent whose pin folds it in. Depth is the length of the ancestor chain
    // (following `parent_repository_id` up to the top-level repo), NOT a
    // proxy like the mount path's slash count — the two tracker-population
    // sites store different path semantics, so the path length is unreliable.
    let depth_of = |ctx: &LinkContext| -> usize {
        let mut depth = 0usize;
        let mut parent_id = ctx.parent_repository_id;
        // Bounded by the number of tracked links; also guards against a cycle.
        while parent_id != repository.id && depth <= all_contexts.len() {
            let Some(parent) = all_contexts
                .iter()
                .find(|other| other.link_repository_id == parent_id)
            else {
                break;
            };
            depth += 1;
            parent_id = parent.parent_repository_id;
        }
        depth
    };
    // Descending depth: deepest-nested links reserialize before their parents.
    contexts.sort_by_key(|ctx| std::cmp::Reverse(depth_of(ctx)));

    for link_context in &contexts {
        // The parent state holding this link's pin: the top-level state, or the
        // tracked child state of the context mounting this one's parent.
        let (parent_state, parent_repository) =
            if link_context.parent_repository_id == repository.id {
                (state.clone(), repository.clone())
            } else if let Some(parent_ctx) = all_contexts
                .iter()
                .find(|other| other.link_repository_id == link_context.parent_repository_id)
            {
                let parent_repository = Arc::new(
                    repository
                        .to_link_context(parent_ctx.link_repository_id)
                        .await,
                );
                (parent_ctx.link_state.clone(), parent_repository)
            } else {
                (state.clone(), repository.clone())
            };

        let staged_ref = parent_state
            .link_find(
                parent_repository.clone(),
                link_context.link_repository_id,
                link_context.link_node_id,
            )
            .await?;

        // Parent-self is the committed pin for the top level, else the staged pin.
        let parent_signature = if link_context.parent_repository_id == repository.id {
            state_current
                .link_find(
                    repository.clone(),
                    link_context.link_repository_id,
                    link_context.link_node_id,
                )
                .await
                .map_or(staged_ref.signature, |reference| reference.signature)
        } else {
            staged_ref.signature
        };

        lore_debug!("Setting link parent to {}", parent_signature);

        let new_signature = reserialize_tracked_link(
            &parent_state,
            parent_repository.clone(),
            token,
            link_context,
            parent_signature,
            staged_ref.branch,
        )
        .await?;

        lore_debug!(
            "Updating link node hash: node={}, old_hash={}, new_hash={}",
            link_context.link_node_id,
            staged_ref.signature,
            new_signature
        );

        if mark_staged {
            parent_state
                .node_mark(
                    parent_repository,
                    link_context.link_node_id,
                    crate::node::NodeFlags::Staged,
                    true,
                )
                .await?;
        }
    }

    Ok(())
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
        let mount_path =
            RepositoryPath::from_relative(&repository, link_path.join(link_relative.as_str()))?;
        sync::unlink_merge_mine_theirs_base(mount_path.absolute()).await;

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
            &mount_path,
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

#[derive(Debug, Clone)]
pub struct LinkPinChange {
    pub link_path: String,
    pub link_repository: RepositoryId,
    pub revision_from: Hash,
    pub revision_to: Hash,
    pub tracking_from: bool,
    pub tracking_to: bool,
}

/// Drops stale registry entries whose node is gone from the tree.
async fn link_pins_by_path(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
) -> Result<Vec<(String, LinkReference)>, LinkError> {
    let links = state
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list links")?;

    let mut pins = Vec::with_capacity(links.len());
    for link_reference in links.iter() {
        match state
            .node_path(repository.clone(), link_reference.local_node())
            .await
        {
            Ok(path) => pins.push((path, *link_reference)),
            Err(err) => {
                lore_debug!(
                    "Skipping link registry entry for node {} with no resolvable path: {err}",
                    link_reference.local_node(),
                );
            }
        }
    }
    Ok(pins)
}

/// Reports only links present in both revisions whose pin moved. A link added
/// or removed between them already reaches a consumer as a single Add/Delete
/// from the content walk.
///
/// Links are matched by path, not node id: node ids are not stable across
/// revisions.
pub async fn diff_link_pins(
    repository: Arc<RepositoryContext>,
    state_from: &Arc<State>,
    state_to: &Arc<State>,
) -> Result<Vec<LinkPinChange>, LinkError> {
    let from_pins = link_pins_by_path(state_from, repository.clone()).await?;
    let to_pins = link_pins_by_path(state_to, repository.clone()).await?;

    let mut changes = Vec::new();

    for (path, to_reference) in to_pins.iter() {
        let Some(from_reference) = from_pins
            .iter()
            .find(|(from_path, _)| from_path == path)
            .map(|(_, from_reference)| from_reference)
        else {
            continue;
        };
        if from_reference.signature() == to_reference.signature()
            && from_reference.branch() == to_reference.branch()
        {
            continue;
        }
        changes.push(LinkPinChange {
            link_path: path.clone(),
            link_repository: to_reference.repository(),
            revision_from: from_reference.signature(),
            revision_to: to_reference.signature(),
            tracking_from: from_reference.is_tracking(),
            tracking_to: to_reference.is_tracking(),
        });
    }

    Ok(changes)
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
    /// The repository that OWNS the link node (i.e. holds its registry entry).
    /// This is the top-level repository for a plain link, or the innermost
    /// containing linked repository for a nested link.
    pub parent_repository: Arc<RepositoryContext>,
    /// The owning repository's state that the link node lives in.
    pub parent_state: Arc<State>,
    /// Node id of the link within `parent_state`.
    pub local_node: NodeID,
}

/// The fields of a link that must be read out of the linked repository, shared
/// by `link list` and `link info`. The caller supplies what it already holds —
/// the link context, the link node and the pin.
pub struct DescribedLink {
    pub link_state: Arc<State>,
    pub source_path: String,
}

/// Reads the pinned state of a linked repository and the path the link exposes
/// from it, normalising a root mount to `/`.
pub async fn describe_link(
    link_context: Arc<RepositoryContext>,
    signature: Hash,
    link_node: &Node,
) -> Result<DescribedLink, LinkError> {
    let link_state = State::deserialize(link_context.clone(), signature)
        .await
        .forward::<LinkError>("Failed deserializing state node block")?;

    let source_path = link_state
        .node_path(link_context.clone(), link_node.child)
        .await
        .forward::<LinkError>("Failed resolving link node")?;

    let source_path = if source_path.is_empty() {
        String::from("/")
    } else {
        source_path
    };

    Ok(DescribedLink {
        link_state,
        source_path,
    })
}

/// A link's node and the repository that owns the mount, without consulting the
/// link registry.
struct LinkNodeAtPath {
    parent_repository: Arc<RepositoryContext>,
    parent_state: Arc<State>,
    local_node: NodeID,
    link_node: Node,
    link_context: Arc<RepositoryContext>,
}

/// Everything about a link that the tree alone answers.
///
/// Kept separate from [`resolve_link_at_path`] because the registry lookup is
/// the one part a link staged for removal can no longer satisfy.
async fn resolve_link_node_at_path(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
    link_path: &str,
) -> Result<LinkNodeAtPath, LinkError> {
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

    // `find_node_link` may have crossed parent links; resolve the owning repo
    // and its state so the node/reference lookups target the right registry.
    let (parent_repository, parent_state) = if node_link.repository == repository.id {
        (repository.clone(), state.clone())
    } else {
        let owning = Arc::new(repository.to_link_context(node_link.repository).await);
        let owning_state = State::deserialize(owning.clone(), node_link.revision)
            .await
            .forward::<LinkError>("Failed deserializing state")?;
        (owning, owning_state)
    };

    let link_node = parent_state
        .node(parent_repository.clone(), node_link.node)
        .await
        .forward::<LinkError>("Failed deserializing state")?;

    if !link_node.is_link() {
        return Err(NotALink {
            path: link_path.to_string(),
        }
        .into());
    }

    let link_context = Arc::new(
        parent_repository
            .to_link_context(link_node.address.context.into())
            .await,
    );

    Ok(LinkNodeAtPath {
        parent_repository,
        parent_state,
        local_node: node_link.node,
        link_node,
        link_context,
    })
}

/// The linked repository's context for the link mounted at `link_path`.
///
/// A link's branch belongs to the repository it points at, so anything that
/// reads branch state for a link has to ask that repository rather than the one
/// holding the mount. Answers for a link staged for removal too, since the node
/// names the repository without the registry entry that staging the removal
/// dropped.
pub async fn link_context_at_path(
    repository: Arc<RepositoryContext>,
    link_path: &str,
) -> Result<Arc<RepositoryContext>, LinkError> {
    let (state_current, state_staged, _parent_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<LinkError>("Failed deserializing state")?;
    let state = state_staged.unwrap_or(state_current);

    Ok(resolve_link_node_at_path(&state, repository, link_path)
        .await?
        .link_context)
}

/// Resolves a link by path to its full context.
///
/// Finds the node via `find_node_link` (which descends through any parent
/// links), then looks up the node and its `LinkReference` in the OWNING
/// repository — the innermost containing repo for a nested link, not the
/// top-level one. `parent_repository`/`parent_state`/`local_node` identify
/// where the link's registry entry lives so callers repin the right registry.
pub async fn resolve_link_at_path(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
    link_path: &str,
) -> Result<ResolvedLink, LinkError> {
    let LinkNodeAtPath {
        parent_repository,
        parent_state,
        local_node,
        link_node,
        link_context,
    } = resolve_link_node_at_path(state, repository, link_path).await?;

    let link_reference = parent_state
        .link_find(parent_repository.clone(), link_context.id, local_node)
        .await
        .forward::<LinkError>("Failed to find link")?;

    Ok(ResolvedLink {
        link_node,
        link_context,
        link_reference,
        parent_repository,
        parent_state,
        local_node,
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

/// Whether a pin change also updates the working tree at the mount path.
#[derive(Clone, Copy)]
pub enum LinkPinRealize {
    /// Realize the content delta on disk.
    WorkingTree,
    /// Leave the working tree alone.
    StateOnly,
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
    realize: LinkPinRealize,
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
    if matches!(realize, LinkPinRealize::WorkingTree) {
        realize_link_pin_change(
            repository.clone(),
            link_context.clone(),
            link_path.clone(),
            old_signature,
            new_signature,
            link_node.child,
        )
        .await?;
    }

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

/// Which side decides a link pin that the two sides of a merge disagree on.
pub enum LinkPinResolution {
    /// Adopt the incoming pin only when the target left it at the base revision.
    /// Both sides having moved it is a conflict.
    ThreeWay(Arc<State>),
    /// Adopt the incoming pin whatever the target holds. For an operation whose
    /// target branch is required to already be merged into the incoming branch.
    Incoming,
}

/// A link whose pin the staged state took from the incoming state.
pub struct CarriedLinkPin {
    pub link_path: String,
    pub previous: Hash,
    pub adopted: Hash,
}

/// A link row for [`apply_link_pins`] to write.
pub struct PlannedLinkPin {
    pub link_path: String,
    link_node: Node,
    target_pin: Hash,
    incoming_pin: Hash,
    branch: BranchId,
}

/// The link node at `link_path`, or `None` when the state has no link there.
async fn link_node_at_path(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
    link_path: &str,
) -> Result<Option<(NodeID, Node)>, LinkError> {
    let node_link = match state.find_node_link(repository.clone(), link_path).await {
        Ok(node_link) => node_link,
        Err(err) if err.is_node_not_found() => return Ok(None),
        Err(err) => return Err(err).forward::<LinkError>("Failed resolving link path"),
    };
    if !node_link.is_valid() || node_link.repository != repository.id {
        return Ok(None);
    }

    let node = state
        .node(repository, node_link.node)
        .await
        .forward::<LinkError>("Failed deserializing state")?;

    Ok(node.is_link().then_some((node_link.node, node)))
}

/// Compare the parent's link rows across a merge and return the ones the
/// incoming state moved on its own. A row both sides moved is an error.
///
/// The parent-level diff never carries a pin change, so the rows are compared
/// here instead. Only explicitly pinned links are considered: an auto-following
/// link's pin only means something on the parent branch whose mirror produced
/// it, so it moves by its own branch merge. Links whose node is in `skip_nodes`
/// keep the pin they have.
pub async fn classify_link_pins(
    repository: Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    state_incoming: &Arc<State>,
    resolution: LinkPinResolution,
    skip_nodes: &[NodeID],
) -> Result<Vec<PlannedLinkPin>, LinkError> {
    let link_list = state_staged
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list links")?;

    // Classify every link before changing any of them, so a divergent pin stops
    // the operation with nothing carried.
    let mut planned = Vec::new();
    for link_reference in &link_list {
        if skip_nodes.contains(&link_reference.local_node) {
            lore_debug!(
                "Link node {} pin is owned by its own merge, leaving it",
                link_reference.local_node
            );
            continue;
        }

        let link_path = state_staged
            .node_path(repository.clone(), link_reference.local_node)
            .await
            .forward::<LinkError>("Failed resolving link node path")?;

        let link_node = state_staged
            .node(repository.clone(), link_reference.local_node)
            .await
            .forward::<LinkError>("Failed deserializing state")?;
        let target_pin = link_node.address.hash;

        // An added or removed link has no row on one side; the tree carries it.
        let Some((incoming_node_id, incoming_node)) =
            link_node_at_path(state_incoming, repository.clone(), &link_path).await?
        else {
            continue;
        };
        if incoming_node.address.context != link_node.address.context {
            lore_debug!("Link {link_path} points at another repository on the incoming side");
            continue;
        }

        let incoming_pin = incoming_node.address.hash;
        if incoming_pin == target_pin {
            continue;
        }

        let branch = state_incoming
            .link_find(
                repository.clone(),
                link_node.address.context.into(),
                incoming_node_id,
            )
            .await
            .map_or(link_reference.branch, |reference| reference.branch);

        // An auto-following row resolves against whichever branch the state ends
        // up on, so its pin cannot travel between parent branches.
        if branch.is_zero() {
            lore_debug!(
                "Link {link_path} follows the parent branch, leaving its pin to its own merge"
            );
            continue;
        }

        if let LinkPinResolution::ThreeWay(ref state_base) = resolution {
            let base_pin = link_node_at_path(state_base, repository.clone(), &link_path)
                .await?
                .map(|(_, node)| node.address.hash);
            match base_pin {
                Some(base_pin) if base_pin == target_pin => {}
                Some(base_pin) if base_pin == incoming_pin => continue,
                // A link carrying an explicit branch cannot merge its signature
                // automatically, so both sides moving it is a flag change; no
                // base row at all is an add on both sides.
                _ => {
                    let subtype = if base_pin.is_some() {
                        "flag change"
                    } else {
                        "add/add, no common base"
                    };
                    return Err(LinkError::internal(format!(
                        "Link pin conflict at {link_path} ({subtype}): this branch pins \
                         {target_pin}, the merged branch pins {incoming_pin}. Resolving a link \
                         pin conflict in place is not supported yet. Set the row you want with \
                         `lore link update {link_path} --pin <revision>`, commit, and merge again"
                    )));
                }
            }
        }

        planned.push(PlannedLinkPin {
            link_path,
            link_node,
            target_pin,
            incoming_pin,
            branch,
        });
    }

    Ok(planned)
}

/// Write the rows [`classify_link_pins`] selected into the staged state.
pub async fn apply_link_pins(
    repository: Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    planned: Vec<PlannedLinkPin>,
    realize: LinkPinRealize,
) -> Result<Vec<CarriedLinkPin>, LinkError> {
    let mut carried = Vec::new();
    for planned_pin in planned {
        let PlannedLinkPin {
            link_path,
            link_node,
            target_pin,
            incoming_pin,
            branch,
        } = planned_pin;

        let link_context = Arc::new(
            repository
                .to_link_context(link_node.address.context.into())
                .await,
        );

        let link_path_rel = RelativePath::from_str(&link_path)
            .internal_with(|| format!("Invalid link path {link_path}"))?;

        lore_info!("Link {link_path} pin {target_pin} -> {incoming_pin} from merged branch");

        stage_link_pin(
            repository.clone(),
            state_staged,
            &link_context,
            link_path_rel,
            link_node,
            target_pin,
            incoming_pin,
            branch,
            realize,
        )
        .await?;

        carried.push(CarriedLinkPin {
            link_path,
            previous: target_pin,
            adopted: incoming_pin,
        });
    }

    Ok(carried)
}

/// [`classify_link_pins`] followed by [`apply_link_pins`].
pub async fn merge_link_pins(
    repository: Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    state_incoming: &Arc<State>,
    resolution: LinkPinResolution,
    realize: LinkPinRealize,
    skip_nodes: &[NodeID],
) -> Result<Vec<CarriedLinkPin>, LinkError> {
    let planned = classify_link_pins(
        repository.clone(),
        state_staged,
        state_incoming,
        resolution,
        skip_nodes,
    )
    .await?;

    apply_link_pins(repository, state_staged, planned, realize).await
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
