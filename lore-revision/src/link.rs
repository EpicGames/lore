// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;

use bitflags::bitflags;
use dashmap::DashMap;
use futures::StreamExt;
use lore_base::types::BranchPoint;
use lore_error_set::prelude::*;
use lore_transport::Connection;
use serde::Serialize;

use crate::bitflagsops;
use crate::branch;
use crate::change::NodeChange;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::filter::FilterMode;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::with_operation;
use crate::interface::LoreError;
use crate::interface::LoreFileAction;
use crate::interface::LoreNodeType;
use crate::interface::LoreString;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_info;
use crate::lore_trace;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeExtFlags;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::repository::is_reserved_node_name;
use crate::revision;
use crate::revision::sync;
use crate::revision::sync::SyncOptions;
use crate::revision::sync::SyncVerifyArgs;
use crate::revision::sync::sync_verify_filesystem;
use crate::stage;
use crate::state;
use crate::state::Clear;
use crate::state::Ext;
use crate::state::LinkPin;
use crate::state::LinkReference;
use crate::state::Mark;
use crate::state::NodeMapping;
use crate::state::State;
use crate::state::StateError;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;
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
///
/// A link is named by the repository holding it, the node it sits at there, and the
/// repository it mounts. That triple is its identity, so a walk reaching the same link twice
/// registers it once whatever path it arrived by.
#[derive(Debug, Clone)]
pub struct LinkContext {
    /// The repository ID that the link points to
    pub link_repository_id: RepositoryId,
    /// The node ID of the link in the parent repository
    pub link_node_id: NodeID,
    /// The repository ID where the link resides
    pub parent_repository_id: RepositoryId,
    /// The state of the linked repository
    pub link_state: Arc<State>,
}

impl PartialEq for LinkContext {
    fn eq(&self, other: &Self) -> bool {
        self.link_repository_id == other.link_repository_id
            && self.link_node_id == other.link_node_id
            && self.parent_repository_id == other.parent_repository_id
    }
}

impl Eq for LinkContext {}

impl std::hash::Hash for LinkContext {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.link_repository_id.hash(state);
        self.link_node_id.hash(state);
        self.parent_repository_id.hash(state);
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

    /// Marks the links into `repository` as needing rehashing.
    ///
    /// A link's target is always a repository reached through a link, so for any other
    /// repository this returns before iterating the map, which allocates for every shard.
    pub fn on_node_changed(&self, repository: &RepositoryContext) {
        if !repository.is_link() {
            return;
        }
        for mut entry in self.links.iter_mut() {
            if entry.key().link_repository_id == repository.id {
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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

/// A mount at a repository root carries an empty path; report it as `/` so
/// every message and event names a path.
pub(crate) fn display_path(path: &str) -> &str {
    if path.is_empty() { "/" } else { path }
}

fn event_link_path(link_path: &str) -> LoreString {
    LoreString::from(display_path(link_path))
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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

    // A dry run reports the branch at the revision it would start at, creating nothing on the
    // remote.
    if execution_context().globals().dry_run() {
        return Ok(LinkBranchOutcome {
            revision: parent_latest,
            reused: false,
        });
    }

    let latest = revision
        .branch_create(
            branch_id,
            &branch_name,
            &branch_category,
            user_id.as_str(),
            &branch_stack,
        )
        .await
        .forward::<LinkError>("Failed to create branch in linked repository")?
        .latest;

    Ok(LinkBranchOutcome {
        revision: latest,
        reused: false,
    })
}

/// Links a merge works on at once. Each pins deserialized revisions of its linked repository, as a
/// history walk does, so it shares the bound measured for those.
#[lore_macro::test_pub]
const MAX_CONCURRENT_LINK_TASKS: usize = crate::revision::DEFAULT_HISTORY_WALK_CONCURRENCY;

/// Awaits per-link work [`MAX_CONCURRENT_LINK_TASKS`] at a time, in order, stopping at the first
/// error. A future starts no work until it is polled, so only those in flight do any.
#[lore_macro::test_pub]
pub(crate) async fn try_join_links<T, E>(
    work: Vec<impl Future<Output = Result<T, E>>>,
) -> Result<Vec<T>, E> {
    let mut in_flight = futures::stream::FuturesOrdered::new();
    let mut results = Vec::with_capacity(work.len());
    for future in work {
        if in_flight.len() == MAX_CONCURRENT_LINK_TASKS
            && let Some(result) = in_flight.next().await
        {
            results.push(result?);
        }
        in_flight.push_back(future);
    }
    while let Some(result) = in_flight.next().await {
        results.push(result?);
    }
    Ok(results)
}

/// Gives each auto-following link a merge brought in a branch for the one it landed on.
///
/// `link add` and `branch create` make that branch when a link starts following a parent branch,
/// and a merged-in link skips both, so without this its revision has no branch to be pushed to. It
/// branches from where the link followed on the merged side, at the revision the link pins.
pub async fn create_branches_for_merged_links(
    repository: Arc<RepositoryContext>,
    state_current: &Arc<State>,
    state_staged: &State,
    current_branch: BranchId,
    merged_branch: BranchId,
) -> Result<(), LinkError> {
    let staged_links = state_staged
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list links")?;
    let merged_in: Vec<(LinkReference, String)> = try_join_links(
        staged_links
            .into_iter()
            .filter(|link| link.branch.is_zero())
            .map(|link| {
                let repository = repository.clone();
                async move {
                    let link_path = state_staged
                        .node_path(repository.clone(), link.local_node)
                        .await
                        .forward::<LinkError>("Failed resolving link node path")?;
                    let merged_in = link_node_at_path(state_current, repository, &link_path)
                        .await?
                        .is_none_or(|(_, node)| {
                            RepositoryId::from(node.address.context) != link.repository
                        });
                    Ok::<_, LinkError>(merged_in.then_some((link, link_path)))
                }
            })
            .collect(),
    )
    .await?
    .into_iter()
    .flatten()
    .collect();
    if merged_in.is_empty() {
        return Ok(());
    }

    let branch_metadata = branch::metadata(repository.clone(), current_branch)
        .await
        .forward::<LinkError>("Failed getting branch metadata")?;
    let branch_name =
        branch::name(&branch_metadata).forward::<LinkError>("Failed getting branch metadata")?;
    let branch_category = branch::category(&branch_metadata).unwrap_or_default();

    let links: Vec<LinkReference> = merged_in.iter().map(|(link, _)| *link).collect();
    // Every creation runs to completion, so a failure knows each branch the others made.
    let results = try_join_links(
        auto_following_mounts(&links)
            .into_iter()
            .map(|(link_id, mounts)| {
                let link = repository.to_link_context(link_id);
                let leader = mounts[0];
                async move {
                    let created = create_merged_link_branch(
                        link.await,
                        current_branch,
                        branch_name,
                        branch_category,
                        leader.resolve_branch(merged_branch),
                        leader.signature,
                    )
                    .await;
                    Ok::<_, LinkError>((link_id, mounts, created))
                }
            })
            .collect(),
    )
    .await?;

    let mut failure = None;
    let mut created = Vec::with_capacity(results.len());
    for (link_id, mounts, result) in results {
        match result {
            Ok((link_remote, outcome)) => created.push((link_id, mounts, link_remote, outcome)),
            Err(err) => {
                failure.get_or_insert(err);
            }
        }
    }
    if let Some(err) = failure {
        // A branch this merge made would outlive the merge, so it is taken back. A retry
        // recreates it.
        for (link_id, _, link_remote, outcome) in created {
            if outcome.reused {
                continue;
            }
            if let Err(delete_err) =
                branch::delete_remote(link_remote, link_id, current_branch).await
            {
                crate::lore_warn!(
                    "Failed to remove branch {current_branch} from linked repository {link_id}: \
                     {delete_err}"
                );
            }
        }
        return Err(err);
    }

    for (link_id, mounts, _, outcome) in created {
        let paths = merged_in.iter().filter(|(link, _)| {
            mounts
                .iter()
                .any(|mount| mount.local_node == link.local_node)
        });
        for (_, link_path) in paths {
            report_branch_outcome(
                link_path,
                link_id,
                current_branch,
                outcome.revision,
                outcome.reused,
            );
        }
    }

    Ok(())
}

/// [`create_branch`] in a linked repository, returning the connection a rollback deletes through.
async fn create_merged_link_branch(
    link: Arc<RepositoryContext>,
    branch: BranchId,
    branch_name: &str,
    branch_category: &str,
    parent_branch: BranchId,
    parent_latest: Hash,
) -> Result<(Arc<Connection>, LinkBranchOutcome), LinkError> {
    let link_remote = link
        .remote()
        .await
        .forward::<LinkError>("Failed to connect to link repository")?;
    let outcome = create_branch(
        link,
        link_remote.clone(),
        branch,
        branch_name.to_string(),
        branch_category.to_string(),
        parent_branch,
        parent_latest,
    )
    .await?;
    Ok((link_remote, outcome))
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
    pub mount_path: String,
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
    group_by_repository(
        link_list.iter().copied().filter(|link_reference| {
            let follows = link_reference.flags & LinkFlags::DisableAutoFollow == 0;
            if !follows {
                lore_debug!(
                    "Auto follow disabled for link {}",
                    link_reference.repository
                );
            }
            follows
        }),
        |link_reference| link_reference.repository,
    )
}

/// Groups `items` by the linked repository `repository` names for each, in the order the
/// repositories first appear.
pub fn group_by_repository<T>(
    items: impl IntoIterator<Item = T>,
    repository: impl Fn(&T) -> RepositoryId,
) -> Vec<(RepositoryId, Vec<T>)> {
    let mut groups: Vec<(RepositoryId, Vec<T>)> = Vec::new();
    for item in items {
        let id = repository(&item);
        match groups.iter_mut().find(|(link_id, _items)| *link_id == id) {
            Some((_link_id, group)) => group.push(item),
            None => groups.push((id, vec![item])),
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

            let full_path = if path_prefix.is_empty() {
                local_path.clone()
            } else {
                format!("{path_prefix}/{local_path}")
            };
            let target = LinkTarget {
                mount_path: full_path,
                repository: link_id,
                context: repository.to_link_context(link_id).await,
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
                target.mount_path.clone(),
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
        mount_path: path.to_string(),
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
    let resolved = revision::resolve_in_branch(
        link.clone(),
        pin,
        execution_context().globals().search_location(),
    )
    .await
    .forward::<LinkError>("Invalid pin specified")?;

    // A caller that tracks a branch of its own overrides the branch pinned here.
    let pin_signature = resolved.revision;
    let pin_branch = resolved.branch;

    lore_debug!("Resolved link pin with revision {pin_signature} on branch {pin_branch}");

    Ok((pin_signature, pin_branch))
}

/// The subtree a link exposes once a change to it lands.
///
/// A link's mount is a node of the revision it pins, and node ids differ between revisions, so it
/// has to be found again whenever the pin moves.
pub enum LinkMountSource {
    /// The path the link exposes today, found again in the revision the link moves to.
    SamePath,
    /// A node of the revision the link moves to.
    Node(NodeID),
}

/// What to realize at a mount: the revisions the pin moves between, the node the mount stands at
/// in the one it leaves, and the subtree it exposes once it lands.
pub struct LinkPinRealizeTarget {
    pub link_path: RelativePath,
    pub old_signature: Hash,
    pub new_signature: Hash,
    pub current_node: NodeID,
    pub mount_source: LinkMountSource,
}

/// A link pin move read against the two revisions it moves between.
pub(crate) struct ResolvedLinkPin {
    link_path: RelativePath,
    new_signature: Hash,
    state_current: Arc<State>,
    state_target: Arc<State>,
    node_current: NodeID,
    /// The node the link exposes in the revision it moves to.
    pub(crate) node_target: NodeID,
}

impl ResolvedLinkPin {
    /// A revision numbers its nodes as it pleases, so a node id is only meaningful in the revision
    /// it was read from, and a mount named by the revision it leaves is carried across by path.
    pub(crate) async fn resolve(
        link_context: &Arc<RepositoryContext>,
        target: LinkPinRealizeTarget,
    ) -> Result<Self, LinkError> {
        let LinkPinRealizeTarget {
            link_path,
            old_signature,
            new_signature,
            current_node,
            mount_source,
        } = target;

        let (state_current, state_target) = tokio::try_join!(
            State::deserialize(link_context.clone(), old_signature),
            State::deserialize(link_context.clone(), new_signature),
        )
        .forward::<LinkError>("Failed deserializing state")?;

        let node_at = async |path: RelativePath| -> Result<NodeID, LinkError> {
            let node_link = state_target
                .find_node_link(link_context.clone(), path.as_str())
                .await
                .forward::<LinkError>("Failed resolving the subtree a link exposes")?;
            if !node_link.is_valid_or_root() {
                return Err(InvalidPath {
                    path: path.to_string(),
                }
                .into());
            }
            Ok(node_link.node)
        };
        let node_target = match mount_source {
            LinkMountSource::Node(node) => node,
            LinkMountSource::SamePath if current_node == ROOT_NODE => ROOT_NODE,
            LinkMountSource::SamePath => {
                let path = link_source_path(link_context.clone(), &state_current, current_node)
                    .await
                    .forward::<LinkError>("Failed resolving link node")?;
                node_at(path).await?
            }
        };

        Ok(Self {
            link_path,
            new_signature,
            state_current,
            state_target,
            node_current: current_node,
            node_target,
        })
    }
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
    // A sequence of `update_link_pin_by_node` -> `serialize` -> `update_link_pin_by_node` ->
    // `serialize`, which `merge_resolve` runs resolving several paths inside a link in one CLI
    // invocation, writes each pin: the save's release leaves the block clean, so the next repin
    // registers it again (`node_set_link_target`).
    state
        .node_set_link_target(&repository, node_id, LinkPin::Revision(new_signature))
        .await?;

    state
        .link_update(repository, link_repo_id, branch, new_signature, node_id)
        .await
}

/// [`update_link_pin_by_node`], then `mark` on the link node and up to the root.
pub async fn repin_link_node(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
    link_repo_id: RepositoryId,
    branch: BranchId,
    new_signature: Hash,
    node_id: NodeID,
    mark: NodeFlags,
) -> Result<(), StateError> {
    update_link_pin_by_node(
        state,
        repository.clone(),
        link_repo_id,
        branch,
        new_signature,
        node_id,
    )
    .await?;
    if mark.intersects(NodeFlags::StagedBits) {
        state
            .node_mark(&repository, node_id, Mark::staged(mark, true))
            .await
    } else {
        state
            .node_mark(
                &repository,
                node_id,
                Mark::dirty(mark, Ext::Set(NodeExtFlags::NoFlags), true),
            )
            .await
    }
}

/// A linked repository's state, with the context reaching it.
pub type LinkedState = (Arc<RepositoryContext>, Arc<State>);

/// What a linked staged state holds below its root, which decides how
/// [`settle_linked_staged_state`] pins its link.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LinkedStagedContent {
    /// At least one dirty marker, with or without staged nodes.
    Dirty,
    /// Staged nodes and no dirty marker.
    Staged,
    /// Neither.
    Empty,
}

impl LinkedStagedContent {
    /// What `state` holds below its root. A node that cannot be read counts as holding a marker,
    /// so the link keeps its pin.
    pub async fn of(state: &Arc<State>, repository: &Arc<RepositoryContext>) -> Self {
        if state
            .node_has_dirty_children(repository.clone(), ROOT_NODE)
            .await
            .unwrap_or(true)
        {
            LinkedStagedContent::Dirty
        } else if state
            .node_has_staged_children(repository.clone(), ROOT_NODE)
            .await
            .unwrap_or(true)
        {
            LinkedStagedContent::Staged
        } else {
            LinkedStagedContent::Empty
        }
    }
}

/// Pin the link at `link_node` in `state` by what `linked` holds.
///
/// A state holding anything is serialized on the committed revision it derives from, so a commit
/// can restore that revision from the pin alone, and the link keeps its dirty flag only while a
/// marker remains. An empty state has nothing to pin, so the link goes back to that revision.
pub async fn settle_linked_staged_state(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
    link_node: NodeID,
    branch: BranchId,
    (linked_repository, linked_state): &LinkedState,
    content: LinkedStagedContent,
    token: &RepositoryWriteToken,
) -> Result<(), StateError> {
    let committed = linked_state.committed_revision();
    let signature = if content == LinkedStagedContent::Empty {
        committed
    } else {
        linked_state.reparent_onto(committed);
        linked_state
            .serialize(linked_repository.clone(), token)
            .await?
    };

    if content == LinkedStagedContent::Dirty {
        return repin_link_node(
            state,
            repository,
            linked_repository.id,
            branch,
            signature,
            link_node,
            NodeFlags::Dirty,
        )
        .await;
    }
    update_link_pin_by_node(
        state,
        repository.clone(),
        linked_repository.id,
        branch,
        signature,
        link_node,
    )
    .await?;
    state.node_clear(&repository, link_node, Clear::Dirty).await
}

/// Reserializes a tracked link's state and updates the parent repository's
/// block tree and link registry.
///
/// This is the shared workflow between `process_link_updates` (stage) and
/// `process_link_unstage_updates` (unstage):
/// 1. Get linked repository context via `to_link_context`
/// 2. Reparent the linked state onto `parent_signature` ([`State::reparent_onto`])
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
    let linked_repository = repository
        .to_link_context(link_context.link_repository_id)
        .await;

    let linked_state = link_context.link_state.clone();
    linked_state.reparent_onto(parent_signature);

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
    /// The branch the link resolves to.
    pub branch: BranchId,
    /// The branch the link row stores, per [`state::LinkReference::branch_to_store`].
    pub stored_branch: BranchId,
    /// Child's committed pin; used as parent-self when reserializing.
    pub old_signature: Hash,
}

/// A link path resolved through zero or more link boundaries. `levels` is empty
/// for a plain top-level path, in which case `innermost` maps the caller's
/// top-level staged state.
pub struct ResolvedLinkChain {
    /// Ordered outer -> inner.
    pub levels: Vec<LinkChainLevel>,
    /// The deepest mapping reached: the innermost mount crossed, or the base the walk started from
    /// where it crossed none.
    pub innermost: NodeMapping,
    /// The innermost repository's committed state. Carries registry entries that
    /// the staged state has already dropped, such as a link staged for removal.
    pub innermost_current_state: Arc<State>,
    /// What the walk did not consume, named from `innermost`. The path asked for where no
    /// link was crossed, and what stands below the innermost mount where one was.
    pub remainder_path: RelativePathBuf,
}

/// Resolve `remainder_path`, which names a path below `base`, down through any
/// link boundaries into the crossed links, the innermost mapping and what is left
/// unconsumed there. Like `find_relative_node_link` but records each crossed link.
/// Bounded by `MAX_LINK_DEPTH` and a visited-repository set.
///
/// Both ends take the same shape: a mapping plus the remainder below it is the path in the
/// repository instance's file system throughout, so a walk starting below the repository root
/// answers with the same paths one starting at it does.
pub async fn resolve_link_chain(
    base: NodeMapping,
    state_current: Arc<State>,
    mut remainder_path: RelativePath,
    parent_branch: BranchId,
) -> Result<ResolvedLinkChain, LinkError> {
    let mut levels: Vec<LinkChainLevel> = Vec::new();
    let mut seen: std::collections::HashSet<RepositoryId> = std::collections::HashSet::new();
    seen.insert(base.repository.id);

    let mut cur_repository = base.repository;
    let mut cur_state = base.state;
    let mut cur_current_state = state_current;
    let mut cur_branch = parent_branch;
    let mut cur_node = base.node;
    let mut base_node = base.node;
    let mut mount_path = base.path.into_buf();
    let mut below_mount = RelativePathBuf::new();

    while !remainder_path.is_empty() {
        let name = remainder_path.pop_root();
        if is_reserved_node_name(name) {
            return Err(InvalidPath {
                path: name.to_string(),
            }
            .into());
        }
        let name_hash = crate::hash::hash_string(name);
        below_mount.push(name);

        // A segment that does not resolve is the boundary of what exists: the
        // current level is innermost and the rest is the target to create. This
        // is the `link add` case, whose target does not exist yet.
        if let Ok(node_id) = cur_state
            .find_subnode(cur_repository.clone(), cur_node, name_hash)
            .await
        {
            cur_node = node_id;
        } else {
            while !remainder_path.is_empty() {
                below_mount.push(remainder_path.pop_root());
            }
            break;
        }

        if remainder_path.is_empty() {
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

            let child_repository = cur_repository.to_link_context(child_repository_id).await;
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
                stored_branch: link_reference.branch_to_store(resolved_branch, cur_branch),
                old_signature,
            });

            cur_repository = child_repository;
            cur_state = child_state;
            cur_current_state = child_current_state;
            cur_branch = resolved_branch;
            cur_node = link.node;
            base_node = link.node;
            mount_path.push(below_mount.as_str());
            below_mount.clear();
        }
    }

    Ok(ResolvedLinkChain {
        levels,
        innermost: NodeMapping {
            repository: cur_repository,
            state: cur_state,
            path: mount_path.freeze(),
            node: base_node,
        },
        innermost_current_state: cur_current_state,
        remainder_path: below_mount,
    })
}

impl ResolvedLinkChain {
    /// The branch of the repository holding the innermost link, under a top level on
    /// `parent_branch`.
    pub fn owner_branch(&self, parent_branch: BranchId) -> BranchId {
        self.levels
            .last()
            .map_or(parent_branch, |level| level.branch)
    }

    /// The child (state, repository) mounted by level `index`: the next level's
    /// parent for an intermediate level, or `innermost`'s for the last level.
    /// Used by the inner -> outer folding passes.
    pub fn child_at(&self, index: usize) -> (Arc<State>, Arc<RepositoryContext>) {
        if index + 1 < self.levels.len() {
            (
                self.levels[index + 1].state.clone(),
                self.levels[index + 1].repository.clone(),
            )
        } else {
            (
                self.innermost.state.clone(),
                self.innermost.repository.clone(),
            )
        }
    }

    /// Register a `LinkContext` in `tracker` for every link crossed by this
    /// chain, so a filesystem-walk operation (stage / unstage) folds a nested
    /// change up through all intermediate links. Each level's child state is
    /// the next level's parent state, or `innermost_state` for the last level.
    ///
    /// `innermost_state` must be the exact state instance the caller mutates
    /// (the one the deep change is staged into), so the tracker reserializes
    /// the changes rather than a fresh deserialization of the same revision.
    pub fn record_tracker_contexts(&self, tracker: &LinkTracker, innermost_state: &Arc<State>) {
        for (index, level) in self.levels.iter().enumerate() {
            let child_state = if index + 1 < self.levels.len() {
                self.levels[index + 1].state.clone()
            } else {
                innermost_state.clone()
            };
            tracker.add_link(LinkContext {
                link_repository_id: level.child_repository_id,
                link_node_id: level.link_node_id,
                parent_repository_id: level.repository.id,
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

        child_state.reparent_onto(level.old_signature);
        let new_signature = child_state
            .serialize(child_repository, token)
            .await
            .forward::<LinkError>("Failed to serialize linked state")?;

        repin_link_node(
            &level.state,
            level.repository.clone(),
            level.child_repository_id,
            level.stored_branch,
            new_signature,
            level.link_node_id,
            NodeFlags::Staged,
        )
        .await
        .forward::<LinkError>("Failed to update link pin")?;
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
    // parent whose pin folds it in. Depth is the length of the ancestor chain,
    // following `parent_repository_id` up to the top-level repo.
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
                let parent_repository = repository
                    .to_link_context(parent_ctx.link_repository_id)
                    .await;
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
                    &parent_repository,
                    link_context.link_node_id,
                    Mark::staged(NodeFlags::Staged, true),
                )
                .await?;
        }
    }

    Ok(())
}

/// Force-realizes specific file paths from a linked repository state at the
/// mount path, regardless of any state-to-state diff.
///
/// Restore on-disk content for `paths` (mount-relative) from `link_state`
/// and unlink any `~mine`/`~theirs`/`~base` sidecars at the same paths.
///
/// Used during merge abort to clean up filesystem-only artifacts: marker
/// bytes inside conflicted file contents and sidecar files. The
/// state-to-state diff from `realize_link_pin_change` doesn't touch these
/// (sidecars aren't in any state, and marker bytes inside a file are not
/// produced by `realize_changes` — they're produced by `realize_conflicts`).
///
/// `link_path` is the link's mount path in the parent repository, and `mounted` the node of
/// `link_state` mounted there. `paths` are below the mount.
pub async fn restore_link_paths_from_state(
    repository: Arc<RepositoryContext>,
    link_context: Arc<RepositoryContext>,
    link_path: RelativePath,
    link_state: Arc<State>,
    mounted: NodeID,
    paths: &[RelativePath],
) -> Result<(), LinkError> {
    if paths.is_empty() {
        return Ok(());
    }

    with_operation(repository.file_system(), async |operation| {
        for link_relative in paths {
            let mount_path = link_path.join(link_relative.as_str());
            sync::unlink_merge_artifacts(&operation, &mount_path).await;

            let node_link = link_state
                .find_relative_node_link(link_context.clone(), mounted, link_relative.as_str())
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
                mount_path,
                node,
                Arc::default(),
            )
            .await
            .forward::<LinkError>("Failed synchronizing link changes")?;
        }

        Ok(())
    })
    .await
}

/// The links of `state`'s registry, each with whether the merge in progress in `state` merged it.
pub(crate) async fn links_and_merged(
    state: &State,
    repository: Arc<RepositoryContext>,
) -> Result<Vec<(LinkReference, bool)>, LinkError> {
    let (entries, links) = tokio::try_join!(
        state.deserialize_link_merge_state(repository.clone()),
        state.link_list(repository),
    )
    .forward::<LinkError>("Failed to read the links of the merge")?;
    Ok(links
        .into_iter()
        .map(|link| {
            let merged = entries
                .iter()
                .any(|entry| entry.local_node == link.local_node);
            (link, merged)
        })
        .collect())
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

/// A link in a revision's registry, with its path and the node it mounts.
pub struct LinkAtPath {
    pub path: String,
    pub reference: LinkReference,
    pub mount_source: Option<NodeID>,
}

/// Each link in `state`'s registry. Drops stale registry entries whose node is
/// gone from the tree.
pub async fn link_pins_by_path(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
) -> Result<Vec<LinkAtPath>, LinkError> {
    let links = state
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list links")?;
    links_at_paths(state, repository, &links).await
}

/// A function of its own so its loop's locals, which cross two awaits, are not
/// reserved while [`link_pins_by_path`] reads the registry.
async fn links_at_paths(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
    links: &[LinkReference],
) -> Result<Vec<LinkAtPath>, LinkError> {
    let mut pins = Vec::with_capacity(links.len());
    for link_reference in links {
        let path = match state
            .node_path(repository.clone(), link_reference.local_node())
            .await
        {
            Ok(path) => path,
            Err(err) => {
                lore_debug!(
                    "Skipping link registry entry for node {} with no resolvable path: {err}",
                    link_reference.local_node(),
                );
                continue;
            }
        };
        let mount_source = state
            .node(repository.clone(), link_reference.local_node())
            .await
            .forward::<LinkError>("Failed to read link node")?
            .mount_source();
        pins.push(LinkAtPath {
            path,
            reference: *link_reference,
            mount_source,
        });
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
    let to_pins = link_pins_by_path(state_to, repository).await?;
    Ok(link_pin_changes(&from_pins, &to_pins))
}

/// [`diff_link_pins`] over the pins [`link_pins_by_path`] returned.
pub fn link_pin_changes(from_pins: &[LinkAtPath], to_pins: &[LinkAtPath]) -> Vec<LinkPinChange> {
    to_pins
        .iter()
        .filter_map(|to| {
            let from_reference = &from_pins
                .iter()
                .find(|from| from.path == to.path)?
                .reference;
            let to_reference = &to.reference;
            (from_reference.signature() != to_reference.signature()
                || from_reference.branch() != to_reference.branch())
            .then(|| LinkPinChange {
                link_path: to.path.clone(),
                link_repository: to_reference.repository(),
                revision_from: from_reference.signature(),
                revision_to: to_reference.signature(),
                tracking_from: from_reference.is_tracking(),
                tracking_to: to_reference.is_tracking(),
            })
        })
        .collect()
}

/// Repositories of the links that differ between `from_pins` and `to_pins`: a
/// path with a link on one side only, or with another repository, branch, pin
/// or mount source.
pub fn touched_link_repositories<'a>(
    from_pins: &'a [LinkAtPath],
    to_pins: &'a [LinkAtPath],
) -> impl Iterator<Item = RepositoryId> + 'a {
    let unmatched = |pins: &'a [LinkAtPath], others: &'a [LinkAtPath]| {
        pins.iter()
            .filter(move |link| {
                !others.iter().any(|other| {
                    other.path == link.path
                        && other.reference.repository() == link.reference.repository()
                        && other.reference.branch() == link.reference.branch()
                        && other.reference.signature() == link.reference.signature()
                        && other.mount_source == link.mount_source
                })
            })
            .map(|link| link.reference.repository())
    };
    unmatched(from_pins, to_pins).chain(unmatched(to_pins, from_pins))
}

/// Returns true if `staged_node` is a staged link pin or mount change (an update
/// to an existing link, not an add or remove) with no staged changes on the
/// linked side.
pub async fn is_staged_link_update(
    staged_node: &Node,
    current_node: &Node,
    parent_repository: Arc<RepositoryContext>,
) -> Result<bool, LinkError> {
    let is_update = staged_node.is_link()
        && staged_node.is_staged()
        && !staged_node.is_staged_add()
        && !staged_node.is_staged_delete();
    let moved = staged_node.address.hash != current_node.address.hash
        || !staged_node.mounts_same_source(current_node);
    if !is_update || !moved {
        return Ok(false);
    }

    let link = staged_node.linked_node();
    let linked_repository = parent_repository.to_link_context(link.repository).await;
    let linked_state = State::deserialize(linked_repository.clone(), link.revision)
        .await
        .forward::<LinkError>("Failed deserializing linked state")?;
    let has_staged_children = linked_state
        .node_has_staged_children(linked_repository, link.node)
        .await
        .forward::<LinkError>("Failed checking linked node children")?;
    Ok(!has_staged_children)
}

/// What a link pin move changes on disk, and the tree those changes are measured against.
struct LinkPinDiff {
    /// The changes from the pin in place to the pin asked for, spelled from the mount.
    changes: Arc<Vec<NodeChange>>,
    /// The pinned subtree as it stands, which the working tree is verified against.
    current: NodeMapping,
}

/// Realizes on-disk content changes when a link pin changes.
///
/// Deserializes the old and new link states, computes a 2-way diff scoped to
/// the linked node and spelled from the mount, verifies filesystem consistency,
/// and realizes the changes on disk.
///
/// Opens the filesystem operation the realize takes, so a caller already holding one calls
/// [`realize_link_pin_change_in_operation`] instead.
pub async fn realize_link_pin_change(
    repository: Arc<RepositoryContext>,
    link_context: Arc<RepositoryContext>,
    pin_change: LinkPinRealizeTarget,
) -> Result<(), LinkError> {
    let pin = ResolvedLinkPin::resolve(&link_context, pin_change).await?;
    realize_resolved_link_pin(repository, link_context, &pin).await
}

/// [`realize_link_pin_change`] for a pin already read against both revisions.
pub(crate) async fn realize_resolved_link_pin(
    repository: Arc<RepositoryContext>,
    link_context: Arc<RepositoryContext>,
    pin: &ResolvedLinkPin,
) -> Result<(), LinkError> {
    let diff = link_pin_diff(&link_context, pin).await?;
    with_operation(repository.file_system(), async |operation| {
        realize_link_pin_diff(
            &operation,
            repository,
            link_context,
            pin.new_signature,
            diff,
            false,
        )
        .await
    })
    .await
}

/// [`realize_link_pin_change`] for a caller that already holds the filesystem operation.
///
/// A filesystem holds one operation at a time, and a link context shares its parent's provider, so
/// the realize takes the caller's rather than opening a second one on a frozen filesystem.
///
/// `overwrite_local` realizes the pin over local changes to the files it changes, which are
/// otherwise refused.
pub(crate) async fn realize_link_pin_change_in_operation(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    link_context: Arc<RepositoryContext>,
    pin_change: LinkPinRealizeTarget,
    overwrite_local: bool,
) -> Result<(), LinkError> {
    let pin = ResolvedLinkPin::resolve(&link_context, pin_change).await?;
    let diff = link_pin_diff(&link_context, &pin).await?;
    realize_link_pin_diff(
        operation,
        repository,
        link_context,
        pin.new_signature,
        diff,
        overwrite_local,
    )
    .await
}

/// Diffs the pinned subtree between the two pins, spelled from the mount path.
///
/// Reads state alone, so it runs outside the filesystem operation the realize needs.
async fn link_pin_diff(
    link_context: &Arc<RepositoryContext>,
    pin: &ResolvedLinkPin,
) -> Result<LinkPinDiff, LinkError> {
    let link_path = &pin.link_path;
    let current_tree = crate::state::NodeMapping {
        repository: link_context.clone(),
        state: pin.state_current.clone(),
        path: link_path.clone(),
        node: pin.node_current,
    };

    let changes = state::diff_collect_subtree(
        state::node_change_state(
            link_context,
            &pin.state_current,
            pin.node_current,
            link_path.clone(),
        )
        .await,
        state::node_change_state(
            link_context,
            &pin.state_target,
            pin.node_target,
            link_path.clone(),
        )
        .await,
        link_path.clone(),
        FilterMode::View,
    )
    .await
    .forward::<LinkError>("Failed syncing target link")?;
    Ok(LinkPinDiff {
        changes: Arc::new(changes),
        current: current_tree,
    })
}

/// Verifies the working tree against the changes the pin move makes, refusing where a local
/// modification would be overwritten, and returns the changes with what the check settled.
async fn verify_link_pin_diff(
    operation: &Arc<InstanceOperationImpl>,
    link_context: Arc<RepositoryContext>,
    new_sig: Hash,
    diff: LinkPinDiff,
    carries_modes: bool,
) -> Result<Arc<Vec<NodeChange>>, LinkError> {
    let LinkPinDiff { changes, current } = diff;
    if changes.is_empty() {
        return Ok(changes);
    }
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
            changes,
            repository_current: link_context,
            operation: operation.clone(),
            current,
            options,
            carries_modes,
        }),
    )
    .await
    .forward::<LinkError>("Failed verifying local file system")
}

/// Verifies the working tree against the changes the pin move makes and realizes them there.
async fn realize_link_pin_diff(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    link_context: Arc<RepositoryContext>,
    new_sig: Hash,
    diff: LinkPinDiff,
    overwrite_local: bool,
) -> Result<(), LinkError> {
    let changes = if overwrite_local {
        diff.changes
    } else {
        verify_link_pin_diff(operation, link_context, new_sig, diff, true).await?
    };

    // The realize deletes before it moves, so a directory whose files move out is still occupied
    // when its own delete runs. A path the realize writes again is left to what it wrote.
    let written: HashSet<&str> = changes
        .iter()
        .filter(|change| change.action != crate::change::FileAction::Delete)
        .map(|change| change.path().as_lowercase_str())
        .collect();
    let mut emptied: Vec<RelativePath> = changes
        .iter()
        .filter(|change| {
            change.action == crate::change::FileAction::Delete
                && change.from.flags.node_type() == LoreNodeType::Directory
                && !written.contains(change.path().as_lowercase_str())
        })
        .map(|change| change.path().clone())
        .collect();
    emptied.sort_by(|left, right| left.as_str().cmp(right.as_str()));

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

    // Deepest first, so a directory is reached after the ones it holds.
    for path in emptied.iter().rev() {
        match operation.remove(path).await {
            Ok(()) => lore_debug!("Removed {path}, emptied by the subtree the mount now exposes"),
            Err(err) => lore_trace!("Kept {path}, which still holds something of its own: {err}"),
        }
    }

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

/// The path a link exposes from the repository it points at, as stored, so a
/// root mount is the empty path.
pub async fn link_source_path(
    link_context: Arc<RepositoryContext>,
    link_state: &State,
    source_node: NodeID,
) -> Result<RelativePath, StateError> {
    let source_path = link_state
        .node_path(link_context, source_node)
        .await
        .forward::<StateError>("Failed resolving link node")?;

    RelativePath::new_from_initial_path(source_path)
        .forward::<StateError>("Invalid link source path")
}

/// The source path a link node exposes, read from the revision it pins.
pub async fn pinned_source_path(
    link_context: Arc<RepositoryContext>,
    link_node: &Node,
) -> Result<RelativePath, StateError> {
    let link_state = State::deserialize(link_context.clone(), link_node.address.hash)
        .await
        .forward::<StateError>("Failed deserializing state")?;

    link_source_path(link_context, &link_state, link_node.child).await
}

/// Whether link node `to` exposes another subtree than `from`, the link it replaces at one path.
pub(crate) async fn link_remounted(
    link_context: &Arc<RepositoryContext>,
    from: &Node,
    to: &Node,
    state_to: &State,
) -> Result<bool, StateError> {
    if from.address.context != to.address.context {
        return Ok(true);
    }
    if from.address.hash == to.address.hash {
        return Ok(!from.mounts_same_source(to));
    }
    if from.mounts_root() && to.mounts_root() {
        return Ok(false);
    }
    let state_from = State::deserialize(link_context.clone(), from.address.hash)
        .await
        .forward::<StateError>("Failed deserializing state")?;
    let (from_mount, to_mount) = tokio::try_join!(
        link_source_path(link_context.clone(), &state_from, from.child),
        link_source_path(link_context.clone(), state_to, to.child),
    )?;
    Ok(from_mount.as_lowercase_str() != to_mount.as_lowercase_str())
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

    let source_path = link_source_path(link_context, &link_state, link_node.child)
        .await
        .forward::<LinkError>("Failed resolving link source path")?;

    Ok(DescribedLink {
        link_state,
        source_path: display_path(source_path.as_str()).to_string(),
    })
}

/// Two mounts of one repository whose source subtrees strictly nest place the
/// same content at both mount paths under separate pins, so an edit through one
/// is invisible to the other and whichever mount the stage walk reaches last
/// decides the content. Identical subtrees resolve to one shared linked state
/// and advance together, so they are left alone.
///
/// Rejects an incoming mount whose source subtree nests with one `state_target`
/// already holds, or with another mount arriving in the same set of changes.
///
/// Runs before realization: the realize loop clones a mount's content and stages
/// its node before the registry write that would reject it, so rejecting there
/// would leave untracked content behind with no merge state to abort.
pub async fn check_incoming_mount_overlaps(
    repository: Arc<RepositoryContext>,
    state_target: &State,
    changes: &[NodeChange],
) -> Result<(), StateError> {
    let mut incoming: Vec<(RepositoryId, RelativePath)> = Vec::new();

    for change in changes.iter() {
        if change.action == crate::change::FileAction::Delete
            || !change.to.mapping.node.is_valid_node_id()
        {
            continue;
        }

        let node = change
            .to
            .mapping
            .state
            .node(change.to.mapping.repository.clone(), change.to.mapping.node)
            .await?;

        if !node.is_link() {
            continue;
        }

        let link_id: RepositoryId = node.address.context.into();
        let link_context = repository.to_link_context(link_id).await;
        let source_path = pinned_source_path(link_context.clone(), &node).await?;

        check_source_path_overlap(
            state_target,
            repository.clone(),
            link_context,
            source_path.clone(),
            change.from.mapping.node,
        )
        .await?;

        if let Some((_, other)) = incoming.iter().find(|(other_id, other_path)| {
            *other_id == link_id && source_paths_nest(&source_path, other_path)
        }) {
            return Err(InvalidArguments {
                reason: format!(
                    "incoming source path {} overlaps incoming source path {} in the same \
                     repository",
                    display_path(source_path.as_str()),
                    display_path(other.as_str()),
                ),
            }
            .into());
        }

        incoming.push((link_id, source_path));
    }

    Ok(())
}

/// Whether two source subtrees overlap without being the same subtree.
///
/// Compared case-insensitively: the two paths are read from the revisions their
/// own mounts pin, so a case-only rename between those revisions leaves one
/// spelling of a subtree both still address.
pub fn source_paths_nest(left: &RelativePath, right: &RelativePath) -> bool {
    left.as_lowercase_str() != right.as_lowercase_str() && left.overlaps_ignore_case(right)
}

/// `source_path` must be the path as stored in the linked repository, so that it
/// compares directly against what the existing mounts resolve to.
/// `exclude_node` is the mount being written, which must not be compared against
/// itself.
pub async fn check_source_path_overlap(
    state: &State,
    repository: Arc<RepositoryContext>,
    link_context: Arc<RepositoryContext>,
    source_path: RelativePath,
    exclude_node: NodeID,
) -> Result<(), StateError> {
    let link_list = state
        .link_list(repository.clone())
        .await
        .forward::<StateError>("Failed to list links")?;

    for link_reference in link_list.iter().filter(|reference| {
        reference.repository == link_context.id && reference.local_node != exclude_node
    }) {
        let Ok(link_node) = state
            .node(repository.clone(), link_reference.local_node)
            .await
        else {
            lore_debug!(
                "Skipping unresolvable link node {}",
                link_reference.local_node
            );
            continue;
        };

        let mounted_source = pinned_source_path(link_context.clone(), &link_node).await?;

        if !source_paths_nest(&source_path, &mounted_source) {
            continue;
        }

        let link_path = state
            .node_path(repository.clone(), link_reference.local_node)
            .await
            .forward::<StateError>("Failed resolving link node")?;

        return Err(InvalidArguments {
            reason: format!(
                "source path {} overlaps the link already mounted at {} from source path {}",
                display_path(source_path.as_str()),
                display_path(&link_path),
                display_path(mounted_source.as_str()),
            ),
        }
        .into());
    }

    Ok(())
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
        let owning = repository.to_link_context(node_link.repository).await;
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

    let link_context = parent_repository
        .to_link_context(link_node.address.context.into())
        .await;

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

pub async fn validate_link_message_paths(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    paths: impl IntoIterator<Item = &String>,
) -> Result<(), LinkError> {
    futures::future::try_join_all(paths.into_iter().map(|path| async move {
        match resolve_link_at_path(state, repository.clone(), path).await {
            Ok(_) => Ok(()),
            Err(error)
                if error.is_node_not_found()
                    || error.is_invalid_path()
                    || error.is_not_a_link() =>
            {
                Err(InvalidArguments {
                    reason: format!(
                        "Link message path '{path}' does not match any linked repository"
                    ),
                }
                .into())
            }
            Err(error) => Err(error)
                .forward_with::<LinkError, _>(|| format!("resolving link message path {path}")),
        }
    }))
    .await?;
    Ok(())
}

/// Whether `state` holds a link with its row at `path`.
pub(crate) async fn holds_link_at(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    path: &str,
) -> Result<bool, LinkError> {
    match resolve_link_at_path(state, repository.clone(), path).await {
        Ok(_) => Ok(true),
        Err(error)
            if error.is_node_not_found()
                || error.is_invalid_path()
                || error.is_not_a_link()
                || error.is_link_not_found() =>
        {
            Ok(false)
        }
        Err(error) => Err(error).forward_with::<LinkError, _>(|| format!("resolving link {path}")),
    }
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
    mount_source: LinkMountSource,
    realize: LinkPinRealize,
) -> Result<NodeID, LinkError> {
    // Only the working tree honours the view; the staged state takes the pin wherever the link
    // stands.
    let in_view = !repository
        .filter
        .excludes_tree(&link_path, true, FilterMode::View);
    let realize_here = in_view && matches!(realize, LinkPinRealize::WorkingTree);
    let node_target = match mount_source {
        LinkMountSource::Node(node) if !realize_here => node,
        LinkMountSource::SamePath if !realize_here && link_node.mounts_root() => ROOT_NODE,
        mount_source => {
            let pin = ResolvedLinkPin::resolve(
                link_context,
                LinkPinRealizeTarget {
                    link_path: link_path.clone(),
                    old_signature,
                    new_signature,
                    current_node: link_node.child,
                    mount_source,
                },
            )
            .await?;
            if realize_here {
                realize_resolved_link_pin(repository.clone(), link_context.clone(), &pin).await?;
            }
            pin.node_target
        }
    };

    // Stage the link node with updated revision hash
    let mut staged_node = link_node;
    staged_node.address.hash = new_signature;
    staged_node.child = node_target;

    let staged_link_node = stage::stage_single_node(
        repository.clone(),
        state.clone(),
        link_path,
        staged_node,
        Arc::default(),
        None,
        FilterMode::empty(),
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
    /// The path the merged branch mounts and its node at the pin the link lands on, for a remount.
    remount: Option<(RelativePath, NodeID)>,
}

impl PlannedLinkPin {
    /// Where the link's mount lands at the pin it moves to.
    fn mount_source(&self) -> LinkMountSource {
        match &self.remount {
            Some((_, node)) => LinkMountSource::Node(*node),
            None => LinkMountSource::SamePath,
        }
    }
}

/// Check the working tree against what [`apply_link_pins`] realizes for `planned`, refusing where a
/// local modification would be overwritten. Changes nothing in the working tree: the realize the
/// pins' application runs verifies again, and carries the executable bits then.
pub async fn verify_link_pins(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    planned: &[PlannedLinkPin],
) -> Result<(), LinkError> {
    for planned_pin in planned {
        let link_path = RelativePath::from_str(&planned_pin.link_path)
            .internal_with(|| format!("Invalid link path {}", planned_pin.link_path))?;
        if repository
            .filter
            .excludes_tree(&link_path, true, FilterMode::View)
        {
            continue;
        }
        let link_context = repository
            .to_link_context(planned_pin.link_node.address.context.into())
            .await;
        let pin = ResolvedLinkPin::resolve(
            &link_context,
            LinkPinRealizeTarget {
                link_path,
                old_signature: planned_pin.target_pin,
                new_signature: planned_pin.incoming_pin,
                current_node: planned_pin.link_node.child,
                mount_source: planned_pin.mount_source(),
            },
        )
        .await?;
        let diff = link_pin_diff(&link_context, &pin).await?;
        verify_link_pin_diff(operation, link_context, pin.new_signature, diff, false).await?;
    }
    Ok(())
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

/// Whether `changes`, a merge's diff sorted by path, delete the link to `link_repository` at
/// `path` of the merge's target: a delete of a link to that repository at the path or of a
/// directory above it, a change leaving no link to that repository at the path, or one leaving no
/// directory above it. A delete of a node of another kind leaves the link, which the current
/// branch put in its place. The diff reports a move of the link's node or of a directory above it
/// only for a rename in case, which carries the link to the move's destination; a move elsewhere
/// it reports as a delete and an add, which deletes the link here.
#[lore_macro::test_pub]
pub(crate) fn merge_deletes_link(
    changes: &[NodeChange],
    path: &str,
    link_repository: RepositoryId,
) -> bool {
    let links = |side: &crate::change::NodeChangeState| {
        side.is_link() && RepositoryId::from(side.address.context) == link_repository
    };
    covering_paths(path).any(|(covering, at_link)| {
        changes_at(changes, covering).any(|change| {
            match (change.action == crate::change::FileAction::Delete, at_link) {
                (true, true) => links(&change.from),
                (true, false) => change.from.flags.is_directory(),
                (false, true) => !links(&change.to),
                (false, false) => !change.to.flags.is_directory(),
            }
        })
    })
}

/// Each directory above `path` and `path` itself, outermost first, with whether it is `path`.
fn covering_paths(path: &str) -> impl Iterator<Item = (&str, bool)> {
    path.match_indices('/')
        .map(|(end, _)| (&path[..end], false))
        .chain(std::iter::once((path, true)))
}

/// The changes among `changes`, sorted by path, at `path`.
fn changes_at<'a>(changes: &'a [NodeChange], path: &str) -> impl Iterator<Item = &'a NodeChange> {
    let first = changes.partition_point(|change| change.path().as_str() < path);
    changes[first..]
        .iter()
        .take_while(move |change| change.path().as_str() == path)
}

/// A state's link rows, read once and sorted by the node and repository each names.
#[lore_macro::test_pub]
pub(crate) struct LinkRows {
    state: Arc<State>,
    rows: Vec<LinkReference>,
}

impl LinkRows {
    /// The rows `state` holds.
    #[lore_macro::test_pub]
    pub(crate) async fn read(
        state: &Arc<State>,
        repository: Arc<RepositoryContext>,
    ) -> Result<Self, LinkError> {
        let mut rows = state
            .link_list(repository)
            .await
            .forward::<LinkError>("Failed to list links")?;
        rows.sort_unstable_by_key(|row| (row.local_node, row.repository));
        Ok(Self {
            state: state.clone(),
            rows,
        })
    }

    /// The row of the link to `link_repository` at the node `local_node`.
    fn row(&self, local_node: NodeID, link_repository: RepositoryId) -> Option<LinkReference> {
        self.rows
            .binary_search_by_key(&(local_node, link_repository), |row| {
                (row.local_node, row.repository)
            })
            .ok()
            .map(|index| self.rows[index])
    }
}

/// Where `staged`, the rows of a merge's result, holds the link to `link_repository` at the node
/// `local_node` on `branch`, as its row resolves under the parent's `parent_branch`: the path it
/// stands at, which a move of the node or of a directory above it changes. `None` where the merge
/// removed the link, left it on another branch, or left the node linking another repository.
#[lore_macro::test_pub]
pub(crate) async fn merged_link_path(
    repository: &Arc<RepositoryContext>,
    staged: &LinkRows,
    local_node: NodeID,
    link_repository: RepositoryId,
    branch: BranchId,
    parent_branch: BranchId,
) -> Result<Option<RelativePath>, LinkError> {
    if !staged
        .row(local_node, link_repository)
        .is_some_and(|row| row.resolve_branch(parent_branch) == branch)
    {
        return Ok(None);
    }
    let path = staged
        .state
        .node_path(repository.clone(), local_node)
        .await
        .forward::<LinkError>("Failed resolving link node path")?;
    let path =
        RelativePath::from_str(&path).internal_with(|| format!("Invalid link path {path}"))?;
    Ok(Some(path))
}

/// Compare the parent's link rows across a merge and return the ones the
/// incoming state moved on its own. A row both sides moved is an error.
///
/// The parent-level diff never carries a pin change, so the rows are compared
/// here instead. Only explicitly pinned links are considered: an auto-following
/// link's pin only means something on the parent branch whose mirror produced
/// it, so it moves by its own branch merge. Links whose node is in `skip_nodes`
/// keep the pin they have.
pub fn classify_link_pins<'a>(
    repository: Arc<RepositoryContext>,
    state_staged: &'a Arc<State>,
    state_incoming: &'a Arc<State>,
    resolution: LinkPinResolution,
    skip_nodes: &'a [NodeID],
) -> impl Future<Output = Result<Vec<PlannedLinkPin>, LinkError>> + 'a {
    classify_link_pins_except(
        repository,
        state_staged,
        state_incoming,
        resolution,
        skip_nodes,
        &[],
    )
}

/// [`classify_link_pins`] read before the merge lands, for every link but those whose node is in
/// `left_out`.
///
/// A link merged in its own right lands at a pin its merge has yet to produce, which is what a
/// remount of it is read against, so the caller leaves it out.
pub fn classify_link_pins_before_merge<'a>(
    repository: Arc<RepositoryContext>,
    state_current: &'a Arc<State>,
    state_incoming: &'a Arc<State>,
    resolution: LinkPinResolution,
    left_out: &'a [NodeID],
) -> impl Future<Output = Result<Vec<PlannedLinkPin>, LinkError>> + 'a {
    classify_link_pins_except(
        repository,
        state_current,
        state_incoming,
        resolution,
        &[],
        left_out,
    )
}

/// [`classify_link_pins`] over every link but those whose node is in `left_out`.
async fn classify_link_pins_except(
    repository: Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    state_incoming: &Arc<State>,
    resolution: LinkPinResolution,
    skip_nodes: &[NodeID],
    left_out: &[NodeID],
) -> Result<Vec<PlannedLinkPin>, LinkError> {
    let mut link_list = state_staged
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list links")?;
    link_list.retain(|link_reference| !left_out.contains(&link_reference.local_node));

    // Classify every link before changing any of them, so a divergent pin stops
    // the operation with nothing carried.
    let planned = try_join_links(
        link_list
            .iter()
            .map(|link_reference| {
                classify_link_pin(
                    &repository,
                    state_staged,
                    state_incoming,
                    &resolution,
                    skip_nodes,
                    link_reference,
                )
            })
            .collect(),
    )
    .await?;

    Ok(planned.into_iter().flatten().collect())
}

/// [`classify_link_pins`] for one link.
async fn classify_link_pin(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    state_incoming: &Arc<State>,
    resolution: &LinkPinResolution,
    skip_nodes: &[NodeID],
    link_reference: &LinkReference,
) -> Result<Option<PlannedLinkPin>, LinkError> {
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
        return Ok(None);
    };
    if incoming_node.address.context != link_node.address.context {
        lore_debug!("Link {link_path} points at another repository on the incoming side");
        return Ok(None);
    }

    let incoming_pin = incoming_node.address.hash;
    if incoming_pin == target_pin && link_node.mounts_same_source(&incoming_node) {
        return Ok(None);
    }

    // The row both sides moved from, which the mount and the pin are each judged against.
    let base_node = match resolution {
        LinkPinResolution::ThreeWay(state_base) => {
            link_node_at_path(state_base, repository.clone(), &link_path)
                .await?
                .map(|(_, node)| node)
        }
        LinkPinResolution::Incoming => None,
    };
    if base_node.as_ref().is_some_and(|base_node| {
        base_node.address == incoming_node.address && base_node.child == incoming_node.child
    }) {
        return Ok(None);
    }
    let three_way = matches!(resolution, LinkPinResolution::ThreeWay(_));

    let link_id: RepositoryId = link_node.address.context.into();
    let link_context = repository.to_link_context(link_id).await;
    // Node ids do not compare across revisions, so a subtree mount is read as a path from each
    // revision it pins. A linked repository this caller cannot read keeps the mount it has.
    let mut remount = if (link_node.mounts_root() && incoming_node.mounts_root())
        || !repository.can_read_link(link_id)
    {
        None
    } else {
        let (state_target_pin, state_incoming_pin) = if incoming_pin == target_pin {
            let state = State::deserialize(link_context.clone(), target_pin)
                .await
                .forward::<LinkError>("Failed deserializing state")?;
            (state.clone(), state)
        } else {
            tokio::try_join!(
                State::deserialize(link_context.clone(), target_pin),
                State::deserialize(link_context.clone(), incoming_pin),
            )
            .forward::<LinkError>("Failed deserializing state")?
        };
        let (current_mount, incoming_mount) = tokio::try_join!(
            link_source_path(link_context.clone(), &state_target_pin, link_node.child),
            link_source_path(
                link_context.clone(),
                &state_incoming_pin,
                incoming_node.child
            ),
        )
        .forward::<LinkError>("Failed resolving the subtree a link exposes")?;
        (current_mount.as_lowercase_str() != incoming_mount.as_lowercase_str()).then_some((
            state_target_pin,
            current_mount,
            incoming_mount,
        ))
    };

    if three_way && let Some((_, current_mount, incoming_mount)) = remount.as_ref() {
        // The pin comparison below cannot see a side that moved only the mount, so the mount is
        // judged against the base on its own.
        let base_mount = match base_node.as_ref() {
            Some(base_node) if base_node.address.context == link_node.address.context => Some(
                pinned_source_path(link_context.clone(), base_node)
                    .await
                    .forward::<LinkError>("Failed resolving the subtree a link exposes")?,
            ),
            _ => None,
        };
        match base_mount.as_ref().map(RelativePath::as_lowercase_str) {
            Some(base) if base == current_mount.as_lowercase_str() => {}
            Some(base) if base == incoming_mount.as_lowercase_str() => remount = None,
            _ => {
                let current = display_path(current_mount.as_str());
                let incoming = display_path(incoming_mount.as_str());
                return Err(LinkError::internal(format!(
                    "Link mount conflict at {link_path}: this branch mounts {current}, the \
                     merged branch mounts {incoming}. Resolving a link mount conflict in place is \
                     not supported yet. Set the mount you want with `lore link remove \
                     {link_path}` and `lore link add {link_path} <repository> <source path>`, \
                     commit, and merge again"
                )));
            }
        }
    }

    // A link merged in its own right already holds the pin that merge produced, so only a remount
    // is still outstanding here.
    let pin_merged_already = skip_nodes.contains(&link_reference.local_node);
    if pin_merged_already && remount.is_none() {
        lore_debug!(
            "Link node {} pin is owned by its own merge, leaving it",
            link_reference.local_node
        );
        return Ok(None);
    }

    let branch = state_incoming
        .link_find(
            repository.clone(),
            link_node.address.context.into(),
            incoming_node_id,
        )
        .await
        .map_or(link_reference.branch, |reference| reference.branch);

    let mut adopt_pin = !pin_merged_already && incoming_pin != target_pin;

    // An auto-following row resolves against whichever branch the state ends
    // up on, so its pin cannot travel between parent branches.
    if adopt_pin && branch.is_zero() {
        lore_debug!("Link {link_path} follows the parent branch, leaving its pin to its own merge");
        adopt_pin = false;
    }

    if adopt_pin && three_way {
        let base_pin = base_node.as_ref().map(|node| node.address.hash);
        match base_pin {
            Some(base_pin) if base_pin == target_pin => {}
            Some(base_pin) if base_pin == incoming_pin => adopt_pin = false,
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

    if !adopt_pin && remount.is_none() {
        return Ok(None);
    }

    let remount = match remount {
        None => None,
        Some((_, _, incoming_mount)) if adopt_pin || incoming_pin == target_pin => {
            Some((incoming_mount, incoming_node.child))
        }
        Some((state_target_pin, _, incoming_mount)) => {
            // A remount the pin does not travel with lands at the pin this branch keeps.
            let node_link = state_target_pin
                .find_node_link(link_context, incoming_mount.as_str())
                .await
                .ok()
                .filter(|node_link| node_link.is_valid_or_root());
            let Some(node_link) = node_link else {
                let incoming = display_path(incoming_mount.as_str());
                return Err(LinkError::internal(format!(
                    "Link mount conflict at {link_path}: the merged branch mounts {incoming}, which \
                 the revision this branch pins does not hold. Pin the link where it does with \
                 `lore link update {link_path} --pin <revision>`, commit, and merge again"
                )));
            };
            Some((incoming_mount, node_link.node))
        }
    };

    let (incoming_pin, branch) = if adopt_pin {
        (incoming_pin, branch)
    } else {
        (target_pin, link_reference.branch)
    };
    Ok(Some(PlannedLinkPin {
        link_path,
        link_node,
        target_pin,
        incoming_pin,
        branch,
        remount,
    }))
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
            remount,
        } = planned_pin;

        let link_path_rel = RelativePath::from_str(&link_path)
            .internal_with(|| format!("Invalid link path {link_path}"))?;

        let link_context = repository
            .to_link_context(link_node.address.context.into())
            .await;

        if target_pin != incoming_pin {
            lore_info!("Link {link_path} pin {target_pin} -> {incoming_pin} from merged branch");
        }
        let mount_source = match remount {
            Some((path, node)) => {
                lore_info!("Link {link_path} exposes {path} from the merged branch");
                LinkMountSource::Node(node)
            }
            None => LinkMountSource::SamePath,
        };

        stage_link_pin(
            repository.clone(),
            state_staged,
            &link_context,
            link_path_rel,
            link_node,
            target_pin,
            incoming_pin,
            branch,
            mount_source,
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

    Box::pin(apply_link_pins(repository, state_staged, planned, realize)).await
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
