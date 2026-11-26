// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
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
use crate::fs::realize::VerifiedChanges;
use crate::interface::LoreError;
use crate::interface::LoreFileAction;
use crate::interface::LoreNodeType;
use crate::interface::LoreString;
use crate::lore::Address;
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
use crate::state::FoundNode;
use crate::state::LinkPin;
use crate::state::LinkPinAgreement;
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
    Cancelled,
    NodeNotFound,
    LinkNotFound,
    LinkPinsDiverge,
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
    /// The state the mounts of a repository stage into, by parent and repository.
    states: SharedByPin<(RepositoryId, RepositoryId), Arc<State>>,
}

/// What the mounts of one linked repository share, opened once: one for every mount, and one for
/// each mount pinning a revision that one is not staged on, as mounts written before every mount
/// shared one pin can.
#[derive(Debug)]
pub(crate) struct SharedByPin<K, V>(parking_lot::Mutex<OpenedOnce<K, V>>);

type OpenedOnce<K, V> = HashMap<(K, Option<Hash>), Arc<tokio::sync::OnceCell<V>>>;

impl<K, V> Default for SharedByPin<K, V> {
    fn default() -> Self {
        Self(parking_lot::Mutex::default())
    }
}

impl<K: Copy + Eq + std::hash::Hash, V: Clone> SharedByPin<K, V> {
    fn cell(&self, key: K, own: Option<Hash>) -> Arc<tokio::sync::OnceCell<V>> {
        self.0.lock().entry((key, own)).or_default().clone()
    }

    /// The value of `key` for a mount pinning `pin`: the shared one, opened by `open_shared`, where
    /// `pin` is its revision or the one it is staged on (`staged_on`), else the mount's own, opened
    /// by `open_own`.
    pub(crate) async fn get_or_open<E, Shared, Own>(
        &self,
        (key, pin): (K, Hash),
        staged_on: impl Fn(&V) -> [Hash; 2],
        open_shared: impl FnOnce() -> Shared,
        open_own: impl FnOnce() -> Own,
    ) -> Result<V, E>
    where
        Shared: Future<Output = Result<V, E>>,
        Own: Future<Output = Result<V, E>>,
    {
        let shared = self
            .cell(key, None)
            .get_or_try_init(open_shared)
            .await?
            .clone();
        if staged_on(&shared).contains(&pin) {
            return Ok(shared);
        }
        Ok(self
            .cell(key, Some(pin))
            .get_or_try_init(open_own)
            .await?
            .clone())
    }
}

impl LinkTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    #[lore_macro::test_pub]
    fn add_link(&self, link_context: LinkContext) {
        // The guard dies at the `;`, so the shard lock is not held across an await. A link a task
        // tracks again keeps the modification another task marked on it.
        #[allow(clippy::disallowed_methods)]
        self.links.entry(link_context).or_insert(false);
    }

    /// Tracks the link at `link_node` of `parent`, pinning `pin` of `link_repository_id`, and
    /// answers the state it stages into, which every mount of it in `parent` shares
    /// ([`open_shared_linked_state`]), so the tasks of one walk stage into one revision.
    pub async fn track(
        &self,
        (parent_repository, parent_state): (&Arc<RepositoryContext>, &Arc<State>),
        link_node: NodeID,
        link_repository_id: RepositoryId,
        pin: Hash,
    ) -> Result<Arc<State>, StateError> {
        // Boxed, as only a walk crossing a link reaches it, and inline it would grow the future of
        // every directory the walk stages.
        let link_state = Box::pin(self.states.get_or_open(
            ((parent_repository.id, link_repository_id), pin),
            |state| [state.revision(), state.committed_revision()],
            || open_shared_linked_state(parent_repository, parent_state, link_repository_id, pin),
            || async {
                let link_context = parent_repository.to_link_context(link_repository_id).await;
                State::deserialize(link_context, pin).await
            },
        ))
        .await?;
        self.add_link(LinkContext {
            link_repository_id,
            link_node_id: link_node,
            parent_repository_id: parent_repository.id,
            link_state: link_state.clone(),
        });
        Ok(link_state)
    }

    /// Stops tracking the link to `link_repository_id` at `link_node_id` in
    /// `parent_repository_id`, which the unstage dropped.
    pub fn remove_link(
        &self,
        link_repository_id: RepositoryId,
        link_node_id: NodeID,
        parent_repository_id: RepositoryId,
    ) {
        self.links.retain(|link, _| {
            (
                link.link_repository_id,
                link.link_node_id,
                link.parent_repository_id,
            ) != (link_repository_id, link_node_id, parent_repository_id)
        });
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

    /// All tracked link contexts, regardless of whether they were marked as
    /// modified. Used to resolve parent states when folding a nested change up
    /// through intermediate links that were crossed but not directly modified.
    pub fn all_links(&self) -> Vec<LinkContext> {
        self.links.iter().map(|entry| entry.key().clone()).collect()
    }
}

/// The state of `link_repository_id` that the mounts of it in `parent_state` share, opened for a
/// mount pinning `pin`: the first uncommitted revision a mount pins, as a stage leaves the mounts it
/// changed at an uncommitted revision and the others at the committed pin, else `pin`.
pub(crate) async fn open_shared_linked_state(
    parent_repository: &Arc<RepositoryContext>,
    parent_state: &State,
    link_repository_id: RepositoryId,
    pin: Hash,
) -> Result<Arc<State>, StateError> {
    let link_context = parent_repository.to_link_context(link_repository_id).await;
    let mut pins: Vec<Hash> = parent_state
        .link_list(parent_repository.clone())
        .await?
        .iter()
        .filter(|row| row.repository == link_repository_id)
        .map(|row| row.signature)
        .chain([pin])
        .collect();
    pins.sort_unstable();
    pins.dedup();
    let mut opened = futures::future::try_join_all(
        pins.iter()
            .map(|&pinned| State::deserialize(link_context.clone(), pinned)),
    )
    .await?;
    let shared = opened
        .iter()
        .position(|state| state.revision_number() == 0)
        .or_else(|| pins.iter().position(|&pinned| pinned == pin))
        .unwrap_or_default();
    Ok(opened.swap_remove(shared))
}

/// Data for an event reporting a change to a link.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
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

/// The context of a failure to read the linked repository `id`.
pub(crate) fn unreachable(id: RepositoryId) -> String {
    format!("Failed to reach linked repository {id}")
}

pub async fn create_branch(
    repository: Arc<RepositoryContext>,
    remote: Arc<Connection>,
    branch_id: Context,
    branch_name: &str,
    branch_category: &str,
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
        .forward_with::<LinkError, _>(|| unreachable(repository.id))?;

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
        if let Ok(_previous_id) = branch::load_name_to_id(repository.clone(), branch_name).await {
            lore_debug!("Link branch with name {branch_name} already exists, appending branch ID");
            std::borrow::Cow::Owned(format!("{branch_name}-{branch_id}"))
        } else {
            std::borrow::Cow::Borrowed(branch_name)
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
            branch_category,
            user_id.as_str(),
            &branch_stack,
        )
        .await
        .forward_with::<LinkError, _>(|| {
            format!(
                "Failed to create branch in linked repository {}",
                repository.id
            )
        })?
        .latest;

    Ok(LinkBranchOutcome {
        revision: latest,
        reused: false,
    })
}

/// Links a merge or a branch cascade works on at once. Each pins deserialized revisions of its
/// linked repository, as a history walk does, or holds a connection to it, so it shares the bound
/// measured for those.
#[lore_macro::test_pub]
pub(crate) const MAX_CONCURRENT_LINK_TASKS: usize =
    crate::revision::DEFAULT_HISTORY_WALK_CONCURRENCY;

/// Awaits per-link work [`MAX_CONCURRENT_LINK_TASKS`] at a time, in order. After the first error
/// no more work starts, and the error is returned once the work in flight settles, so none is
/// dropped halfway, as a push would be. A future starts no work until it is polled.
#[lore_macro::test_pub]
pub(crate) async fn try_join_links<T, E>(
    work: Vec<impl Future<Output = Result<T, E>>>,
) -> Result<Vec<T>, E> {
    let mut in_flight = futures::stream::FuturesOrdered::new();
    let mut results = Vec::with_capacity(work.len());
    let mut first_error = None;
    let mut work = work.into_iter();
    loop {
        while first_error.is_none()
            && in_flight.len() < MAX_CONCURRENT_LINK_TASKS
            && let Some(future) = work.next()
        {
            in_flight.push_back(future);
        }
        match in_flight.next().await {
            Some(Ok(result)) => results.push(result),
            Some(Err(error)) => {
                first_error.get_or_insert(error);
            }
            None => break,
        }
    }
    first_error.map_or(Ok(results), Err)
}

/// A following link a merge brings in: one the merged side holds to a repository the side it
/// merges into does not link, at a path where the base of the merge does not link it.
#[lore_macro::test_pub]
#[derive(Clone)]
pub(crate) struct IncomingLink {
    /// The path the merged side holds the link at.
    pub link_path: String,
    /// The merged side's row of the link.
    pub link_reference: LinkReference,
}

/// The following links a merge of `state_source` into `state_target`, from `state_base`, brings
/// in, grouped by linked repository.
#[lore_macro::test_pub]
pub(crate) async fn incoming_following_links(
    repository: &Arc<RepositoryContext>,
    state_target: &Arc<State>,
    state_base: &Arc<State>,
    state_source: &Arc<State>,
) -> Result<Vec<(RepositoryId, Vec<IncomingLink>)>, LinkError> {
    let rows = state_source
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list links")?;
    let incoming = try_join_links(
        rows.into_iter()
            .filter(|row| row.branch.is_zero() && row.flags & LinkFlags::DisableAutoFollow == 0)
            .map(async |link_reference| {
                let link_path = state_source
                    .node_path(repository.clone(), link_reference.local_node)
                    .await
                    .forward::<LinkError>("Failed resolving link node path")?;
                let (target, base) = tokio::try_join!(
                    link_node_to(
                        state_target,
                        repository,
                        &link_path,
                        link_reference.repository
                    ),
                    link_node_to(
                        state_base,
                        repository,
                        &link_path,
                        link_reference.repository
                    ),
                )?;
                Ok::<_, LinkError>(
                    (target.is_none() && base.is_none()).then_some(IncomingLink {
                        link_path,
                        link_reference,
                    }),
                )
            })
            .collect(),
    )
    .await?;
    Ok(group_by(incoming.into_iter().flatten(), |link| {
        link.link_reference.repository
    }))
}

/// Awaits per-link work [`MAX_CONCURRENT_LINK_TASKS`] at a time, in order, starting none after the
/// first that fails. The work in flight then finishes, so what it did can be undone, and the
/// results end where the work stopped.
pub(crate) async fn join_links_until_failure<T, E>(
    work: Vec<impl Future<Output = Result<T, E>>>,
) -> Vec<Result<T, E>> {
    let mut in_flight = futures::stream::FuturesOrdered::new();
    let mut results = Vec::with_capacity(work.len());
    let mut work = work.into_iter();
    loop {
        let failed = results.iter().any(Result::is_err);
        if !failed
            && in_flight.len() < MAX_CONCURRENT_LINK_TASKS
            && let Some(future) = work.next()
        {
            in_flight.push_back(future);
            continue;
        }
        let Some(result) = in_flight.next().await else {
            return results;
        };
        results.push(result);
    }
}

/// The repositories `state_target` links at a path a merge of `state_source` into it, from
/// `state_base`, keeps them at, each with the pin `state_target` holds it at: each but those the
/// base links there and the merged side removed.
#[lore_macro::test_pub]
pub(crate) async fn repositories_the_merge_keeps(
    repository: &Arc<RepositoryContext>,
    state_target: &Arc<State>,
    state_base: &Arc<State>,
    state_source: &Arc<State>,
) -> Result<HashMap<RepositoryId, Hash>, LinkError> {
    let rows = state_target
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list links")?;
    let kept = try_join_links(
        rows.into_iter()
            .map(async |row| {
                let link_path = state_target
                    .node_path(repository.clone(), row.local_node)
                    .await
                    .forward::<LinkError>("Failed resolving link node path")?;
                let (base, source) = tokio::try_join!(
                    link_node_to(state_base, repository, &link_path, row.repository),
                    link_node_to(state_source, repository, &link_path, row.repository),
                )?;
                Ok::<_, LinkError>(
                    (base.is_none() || source.is_some()).then_some((row.repository, row.signature)),
                )
            })
            .collect(),
    )
    .await?;
    Ok(kept.into_iter().flatten().collect())
}

/// The repositories a merge brings in to `holder`, on `branch`, that hold no branch for their links
/// to land on.
pub(crate) struct MergedLinks<'a> {
    pub holder: &'a Arc<RepositoryContext>,
    pub incoming: &'a [(RepositoryId, Vec<IncomingLink>)],
    pub branch: BranchId,
}

/// Gives each repository of each of `merged` the branch its links land on, and so each repository
/// its revision links at every depth, as `branch create` does, reusing one a repository holds
/// already.
///
/// `link add` and `branch create` make that branch when a link starts following a parent branch,
/// and a merged-in link skips both, so without this its revision has no branch to be pushed to. It
/// branches from where the link followed on `merged_branch`, at the revision the link pins.
pub(crate) async fn create_branches_for_merged_links(
    merged: &[MergedLinks<'_>],
    merged_branch: BranchId,
) -> Result<(), LinkError> {
    let holders = try_join_links(
        merged
            .iter()
            .filter(|merged| !merged.incoming.is_empty())
            .map(async |merged| {
                let metadata = branch::metadata(merged.holder.clone(), merged.branch)
                    .await
                    .forward::<LinkError>("Failed getting branch metadata")?;
                let name = branch::name(&metadata)
                    .forward::<LinkError>("Failed getting branch metadata")?
                    .to_owned();
                let category = branch::category(&metadata).unwrap_or_default().to_owned();
                let first_level = merged
                    .incoming
                    .iter()
                    .flat_map(|(link_id, mounts)| {
                        mounts.iter().map(|mount| LinkReach {
                            repository: *link_id,
                            pin: mount.link_reference.signature,
                            branch: mount.link_reference.resolve_branch(merged_branch),
                            mount_path: mount.link_path.clone(),
                            depth: 1,
                            leads: true,
                        })
                    })
                    .collect();
                let reached = Box::pin(collect_with_context(merged.holder, first_level)).await?;
                Ok::<_, LinkError>((merged.branch, name, category, reached))
            })
            .collect(),
    )
    .await?;

    // Every creation runs to completion, so a failure knows each branch the others made.
    let results = try_join_links(
        holders
            .iter()
            .flat_map(|(branch, name, category, reached)| {
                reached.iter().map(move |linked| {
                    let start = &linked.reaches[0];
                    async move {
                        let created = create_merged_link_branch(
                            linked.context.clone(),
                            *branch,
                            name,
                            category,
                            start.branch,
                            start.pin,
                        )
                        .await;
                        Ok::<_, LinkError>((linked, *branch, created))
                    }
                })
            })
            .collect(),
    )
    .await?;

    let mut failure = None;
    let mut created = Vec::with_capacity(results.len());
    for (linked, branch, result) in results {
        match result {
            Ok((link_remote, outcome)) => created.push((linked, branch, link_remote, outcome)),
            Err(err) => {
                failure.get_or_insert(err);
            }
        }
    }
    if let Some(err) = failure {
        // A branch this merge made would outlive the merge, so it is taken back. A retry
        // recreates it.
        for (linked, branch, link_remote, outcome) in created {
            if outcome.reused {
                continue;
            }
            let link_id = linked.repository;
            if let Err(delete_err) = branch::delete_remote(link_remote, link_id, branch).await {
                crate::lore_warn!(
                    "Failed to remove branch {branch} from linked repository {link_id}: \
                     {delete_err}"
                );
            }
        }
        return Err(err);
    }

    for (linked, branch, _, outcome) in created {
        for reach in &linked.reaches {
            report_branch_outcome(
                &reach.mount_path,
                linked.repository,
                branch,
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
        branch_name,
        branch_category,
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
    group_by(
        link_list.iter().copied().filter(|link_reference| {
            let follows = link_reference.auto_follows();
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

/// Groups `items` by the key `key` gives each, in the order the keys first appear.
pub fn group_by<T, K: PartialEq>(
    items: impl IntoIterator<Item = T>,
    key: impl Fn(&T) -> K,
) -> Vec<(K, Vec<T>)> {
    let mut groups: Vec<(K, Vec<T>)> = Vec::new();
    for item in items {
        let item_key = key(&item);
        match groups
            .iter_mut()
            .find(|(group_key, _items)| *group_key == item_key)
        {
            Some((_group_key, group)) => group.push(item),
            None => groups.push((item_key, vec![item])),
        }
    }
    groups
}

/// For operations that cascade into every linked repository, including the
/// links nested inside them.
///
/// A repository reached through several links is listed once, at the mount
/// path of the outermost link reaching it.
pub async fn list_with_context(
    repository: Arc<RepositoryContext>,
) -> Result<Vec<LinkTarget>, LinkError> {
    let (state, parent_branch) = current_or_staged_state(repository.clone()).await?;
    let links = state
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list links")?;
    let rows = links
        .into_iter()
        .filter(LinkReference::auto_follows)
        .map(|link| (state.as_ref(), link, true));
    let first_level = Box::pin(reach_links(&repository, rows, parent_branch, "", 0)).await;
    let reached = Box::pin(collect_with_context(&repository, first_level)).await?;
    Ok(reached
        .into_iter()
        .map(|mut linked| LinkTarget {
            mount_path: linked.reaches.swap_remove(0).mount_path,
            repository: linked.repository,
            context: linked.context,
        })
        .collect())
}

/// A link that a walk over nested links follows.
#[lore_macro::test_pub]
pub(crate) struct LinkReach {
    pub repository: RepositoryId,
    /// The revision the link pins.
    pub pin: Hash,
    /// The branch the link resolves to.
    pub branch: BranchId,
    /// The path the link is mounted at, from the root of the walk.
    pub mount_path: String,
    /// The number of links from the root of the walk to this one, one for a link the root holds.
    pub depth: usize,
    /// Whether the next commit keeps the link: a link the staged state replaces, or one below it,
    /// is followed but doesn't choose where a cascade's branch starts.
    pub leads: bool,
}

/// A revision of a linked repository that a walk read.
#[lore_macro::test_pub]
pub(crate) struct WalkedPin {
    pub pin: Hash,
    pub number: u64,
    /// The repositories the revision's auto-following links reach, none past the depth limit.
    pub linked: Vec<RepositoryId>,
}

/// A linked repository a walk over nested links reaches.
#[lore_macro::test_pub]
pub(crate) struct ReachedRepository {
    pub repository: RepositoryId,
    pub context: Arc<RepositoryContext>,
    /// Every link reaching the repository, the outermost first.
    pub reaches: Vec<LinkReach>,
    /// Every distinct pin of `reaches`.
    pub pins: Vec<WalkedPin>,
}

/// The `rows` a holder on `holder_branch` mounted at `mount_prefix` holds, `depth` links below the
/// root of the walk, as reaches. Each row names the state holding it, and whether it leads.
#[lore_macro::test_pub]
pub(crate) async fn reach_links<'a>(
    repository: &Arc<RepositoryContext>,
    rows: impl IntoIterator<Item = (&'a State, LinkReference, bool)>,
    holder_branch: BranchId,
    mount_prefix: &str,
    depth: usize,
) -> Vec<LinkReach> {
    futures::future::join_all(rows.into_iter().map(|(state, link, leads)| async move {
        let local_path = state
            .node_path(repository.clone(), link.local_node)
            .await
            .unwrap_or_default();
        LinkReach {
            repository: link.repository,
            pin: link.signature,
            branch: link.resolve_branch(holder_branch),
            mount_path: if mount_prefix.is_empty() {
                local_path
            } else {
                format!("{mount_prefix}/{local_path}")
            },
            depth: depth + 1,
            leads,
        }
    }))
    .await
}

/// Walks the links below `first_level`, the links the root of the walk holds, level by level, so
/// that a repository is first reached through the outermost link reaching it. Each distinct pin of
/// a repository is read once, so the links of whichever pin an operation settles on are reached,
/// and the pins of one level are read [`MAX_CONCURRENT_LINK_TASKS`] at a time.
///
/// Bounded by `MAX_LINK_DEPTH` and the pins already read, so a link cycle terminates. The root of
/// the walk is never listed.
#[lore_macro::test_pub]
pub(crate) async fn collect_with_context(
    root: &Arc<RepositoryContext>,
    first_level: Vec<LinkReach>,
) -> Result<Vec<ReachedRepository>, LinkError> {
    let mut walk = LinkWalk {
        root: root.id,
        reached: Vec::new(),
        index: HashMap::new(),
        read: HashSet::new(),
    };
    let mut level = walk.add(root, first_level).await;
    loop {
        if level.is_empty() {
            return Ok(walk.reached);
        }
        let reads = level
            .iter()
            .map(|&(position, reach)| {
                let linked = &walk.reached[position];
                let reach = &linked.reaches[reach];
                let context = &linked.context;
                async move {
                    let state = State::deserialize(context.clone(), reach.pin)
                        .await
                        .forward_with::<LinkError, _>(|| unreachable(reach.repository))?;
                    let nested = if reach.depth < crate::state::MAX_LINK_DEPTH {
                        let links = state
                            .link_list(context.clone())
                            .await
                            .forward_with::<LinkError, _>(|| unreachable(reach.repository))?;
                        let rows = links
                            .into_iter()
                            .filter(LinkReference::auto_follows)
                            .map(|link| (state.as_ref(), link, reach.leads));
                        reach_links(context, rows, reach.branch, &reach.mount_path, reach.depth)
                            .await
                    } else {
                        Vec::new()
                    };
                    Ok::<_, LinkError>((state.revision_number(), nested))
                }
            })
            .collect();
        // Boxed once a level, so a walk over no links holds none of the reads.
        let read = Box::pin(try_join_links(reads)).await?;

        let mut next = Vec::new();
        for (&(position, reach), (number, nested)) in level.iter().zip(read) {
            let mut linked: Vec<RepositoryId> = nested
                .iter()
                .map(|link| link.repository)
                .filter(|repository| *repository != walk.root)
                .collect();
            linked.sort_unstable();
            linked.dedup();
            let holder = &mut walk.reached[position];
            holder.pins.push(WalkedPin {
                pin: holder.reaches[reach].pin,
                number,
                linked,
            });
            let context = holder.context.clone();
            next.extend(walk.add(&context, nested).await);
        }
        level = next;
    }
}

/// The repositories a walk over nested links reached, and the pins it read.
struct LinkWalk {
    root: RepositoryId,
    reached: Vec<ReachedRepository>,
    index: HashMap<RepositoryId, usize>,
    read: HashSet<(RepositoryId, Hash)>,
}

impl LinkWalk {
    /// Records `reaches`, made from the revision of `holder` that holds them, returning the
    /// position of each pin not read yet and of the reach naming it.
    async fn add(
        &mut self,
        holder: &Arc<RepositoryContext>,
        mut reaches: Vec<LinkReach>,
    ) -> Vec<(usize, usize)> {
        reaches.retain(|reach| reach.repository != self.root);
        let mut new_repositories: Vec<RepositoryId> = reaches
            .iter()
            .map(|reach| reach.repository)
            .filter(|repository| !self.index.contains_key(repository))
            .collect();
        new_repositories.sort_unstable();
        new_repositories.dedup();
        let contexts = futures::future::join_all(
            new_repositories
                .iter()
                .map(|repository| holder.to_link_context(*repository)),
        )
        .await;
        for (repository, context) in new_repositories.into_iter().zip(contexts) {
            self.index.insert(repository, self.reached.len());
            self.reached.push(ReachedRepository {
                repository,
                context,
                reaches: Vec::new(),
                pins: Vec::new(),
            });
        }

        let mut unread = Vec::new();
        for reach in reaches {
            let position = self.index[&reach.repository];
            let linked = &mut self.reached[position];
            if self.read.insert((reach.repository, reach.pin)) {
                unread.push((position, linked.reaches.len()));
            }
            linked.reaches.push(reach);
        }
        unread
    }
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

    if !resolved.link_reference.auto_follows() {
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
#[derive(Clone, Copy)]
pub enum LinkMountSource {
    /// The path the link exposes today, found again in the revision the link moves to.
    SamePath,
    /// A node of the revision the link moves to.
    Node(NodeID),
}

/// What to realize at a mount: the revisions the pin moves between, the node the mount stands at
/// in the one it leaves, and the subtree it exposes once it lands.
#[derive(Clone)]
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
    /// A node id is only meaningful in the revision it was read from, so a mount named by the
    /// revision it leaves is carried across ([`carry_mount`]).
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

        let node_target = match mount_source {
            LinkMountSource::Node(node) => node,
            LinkMountSource::SamePath => {
                carry_mount(link_context, &state_current, current_node, &state_target)
                    .await
                    .forward::<LinkError>("Failed resolving the subtree a link exposes")?
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

/// A mount [`repin_repository`] moved: its link node, and the revision and node of the linked
/// repository it exposed before and exposes now.
#[lore_macro::test_pub]
#[derive(Clone)]
pub(crate) struct RepinnedMount {
    pub link_node: NodeID,
    /// The directory holding the link node.
    pub parent: NodeID,
    pub old_state: Arc<State>,
    pub old_child: NodeID,
    pub new_state: Arc<State>,
    pub child: NodeID,
}

impl RepinnedMount {
    /// The move of this mount, which the working tree holds at `link_path`, to realize.
    pub(crate) fn pin(&self, link_path: RelativePath) -> ResolvedLinkPin {
        ResolvedLinkPin {
            link_path,
            new_signature: self.new_state.revision(),
            state_current: self.old_state.clone(),
            state_target: self.new_state.clone(),
            node_current: self.old_child,
            node_target: self.child,
        }
    }
}

/// Pins every mount of `link_repository_id` in `state` at `signature` on the stored `branch`, and
/// answers the mounts that moved.
///
/// The pin is held twice, in the registry row and in the link node, so both are rewritten for
/// every mount. Each moved mount exposes the folder it exposed before ([`carry_mount`]). Fails
/// with [`LinkPinsDiverge`](crate::errors::LinkPinsDiverge) where the mounts carry different
/// flags, before any link node is rewritten.
#[lore_macro::test_pub]
pub(crate) async fn repin_repository(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
    link_repository_id: RepositoryId,
    signature: Hash,
    branch: BranchId,
) -> Result<Vec<RepinnedMount>, StateError> {
    let rows = state
        .link_repin(
            repository.clone(),
            link_repository_id,
            branch,
            signature,
            |_| true,
        )
        .await?;
    state
        .check_link_rows_agree(
            &repository,
            &rows,
            &[link_repository_id],
            LinkPinAgreement::Rows,
        )
        .await?;
    let link_context = repository.to_link_context(link_repository_id).await;
    let mut states = Vec::new();
    let mut repinned = Vec::with_capacity(rows.len());
    for row in rows {
        let link_node = row.local_node();
        let node = state.node(repository.clone(), link_node).await?;
        if node.address.hash == signature {
            continue;
        }
        let source = revision_state(&mut states, &link_context, node.address.hash).await?;
        let target = revision_state(&mut states, &link_context, signature).await?;
        let child = carry_mount(&link_context, &source, node.child, &target).await?;
        state
            .node_set_link_target(
                &repository,
                link_node,
                LinkPin::Whole {
                    address: Address {
                        hash: signature,
                        ..node.address
                    },
                    child,
                },
            )
            .await?;
        repinned.push(RepinnedMount {
            link_node,
            parent: node.parent,
            old_state: source,
            old_child: node.child,
            new_state: target,
            child,
        });
    }
    Ok(repinned)
}

/// Pins at `signature` the mounts of `link_repository_id` in `state` that `only` selects, and
/// answers their link nodes. `signature` is staged on the revisions they pin, so it keeps the node
/// ids of each, and every mount keeps the node it exposes.
#[lore_macro::test_pub]
pub(crate) async fn repin_staged_mounts(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    link_repository_id: RepositoryId,
    branch: BranchId,
    signature: Hash,
    only: impl Fn(&LinkReference) -> bool,
) -> Result<Vec<NodeID>, StateError> {
    let rows = state
        .link_repin(
            repository.clone(),
            link_repository_id,
            branch,
            signature,
            only,
        )
        .await?;
    let mut mounts = Vec::with_capacity(rows.len());
    for row in rows {
        state
            .node_set_link_target(repository, row.local_node(), LinkPin::Revision(signature))
            .await?;
        mounts.push(row.local_node());
    }
    Ok(mounts)
}

/// Selects the mounts a change to `linked`, the state its mounts share, moves: those of `holding`,
/// and those pinning the revision it was opened at where that is uncommitted. The others keep the
/// committed pin until a commit moves them.
#[lore_macro::test_pub]
pub(crate) fn sharing_mounts<'a>(
    linked: &State,
    holding: &'a [NodeID],
) -> impl Fn(&LinkReference) -> bool + 'a {
    let uncommitted = (linked.revision_number() == 0).then(|| linked.revision());
    move |row| holding.contains(&row.local_node()) || uncommitted == Some(row.signature())
}

/// The mounts of `walked`, link nodes of `parent_state` mounting `linked`, below which `linked`
/// holds a staged or dirty node: a walk through a mount below which the shared state holds no
/// change keeps its pin.
pub(crate) async fn mounts_holding_changes(
    (parent_repository, parent_state): (&Arc<RepositoryContext>, &State),
    (linked_repository, linked): (&Arc<RepositoryContext>, &State),
    walked: &[NodeID],
) -> Result<Vec<NodeID>, StateError> {
    let held = futures::future::try_join_all(walked.iter().map(async |&link_node| {
        let mounted = parent_state
            .node(parent_repository.clone(), link_node)
            .await?
            .child;
        linked
            .node_has_staged_or_dirty_children(linked_repository.clone(), mounted)
            .await
    }))
    .await?;
    Ok(walked
        .iter()
        .zip(held)
        .filter_map(|(&link_node, held)| held.then_some(link_node))
        .collect())
}

/// The mount of `link_repository_id` among `rows` of `state` that [`repin_repository`] couldn't
/// move to `signature`, as the folder it exposes is missing there.
pub(crate) async fn mount_missing_at(
    state: &State,
    repository: &Arc<RepositoryContext>,
    rows: &[LinkReference],
    link_repository_id: RepositoryId,
    signature: Hash,
) -> Result<Option<NodeID>, StateError> {
    let link_context = repository.to_link_context(link_repository_id).await;
    let mut states = Vec::new();
    for row in rows
        .iter()
        .filter(|row| row.repository() == link_repository_id)
    {
        let node = state.node(repository.clone(), row.local_node()).await?;
        if node.address.hash == signature {
            continue;
        }
        let source = revision_state(&mut states, &link_context, node.address.hash).await?;
        let target = revision_state(&mut states, &link_context, signature).await?;
        match carry_mount(&link_context, &source, node.child, &target).await {
            Err(err) if err.is_invalid_path() => return Ok(Some(row.local_node())),
            carried => carried?,
        };
    }
    Ok(None)
}

/// `revision` of `link_context`, read once into `states`.
async fn revision_state(
    states: &mut Vec<Arc<State>>,
    link_context: &Arc<RepositoryContext>,
    revision: Hash,
) -> Result<Arc<State>, StateError> {
    if let Some(state) = states.iter().find(|state| state.revision() == revision) {
        return Ok(state.clone());
    }
    let state = State::deserialize(link_context.clone(), revision).await?;
    states.push(state.clone());
    Ok(state)
}

/// The node of `target` at the path `node` has in `source`. A revision numbers its nodes as it
/// pleases, so a mount is carried across revisions by path; fails with [`InvalidPath`] where
/// `target` holds nothing there to mount, and with [`NodeNotFound`] where `source` holds no folder
/// at `node`.
async fn carry_mount(
    link_context: &Arc<RepositoryContext>,
    source: &State,
    node: NodeID,
    target: &State,
) -> Result<NodeID, StateError> {
    if node == ROOT_NODE {
        return Ok(ROOT_NODE);
    }
    // `link_source_path` spells a node `source` does not hold as the root, which would mount the
    // whole linked repository.
    if !source
        .holds_node(link_context, node, LoreNodeType::Directory)
        .await?
    {
        return Err(NodeNotFound.into());
    }
    let path = link_source_path(link_context.clone(), source, node).await?;
    let invalid = || InvalidPath {
        path: path.to_string(),
    };
    let found = match target
        .locate_node(link_context.clone(), path.as_str())
        .await
    {
        Err(err) if err.is_node_not_found() => {
            return Err(StateError::InvalidPath(
                invalid().chain_err_from(err, "finding the folder a mount exposes"),
            ));
        }
        found => found?,
    };
    found.own_node().ok_or_else(|| invalid().into())
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

/// Pin the mounts of `linked` in `state` that `only` selects by what `linked` holds, and answer
/// their link nodes.
///
/// A state holding anything is serialized on the committed revision it derives from, so a commit
/// can restore that revision from the pin alone, and a link keeps its dirty flag only while a
/// marker remains below it. An empty state has nothing to pin, so the links go back to that
/// revision.
pub async fn settle_linked_staged_state(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
    only: impl Fn(&LinkReference) -> bool,
    branch: BranchId,
    (linked_repository, linked_state): &LinkedState,
    content: LinkedStagedContent,
    token: &RepositoryWriteToken,
) -> Result<Vec<NodeID>, StateError> {
    let committed = linked_state.committed_revision();
    let signature = if content == LinkedStagedContent::Empty {
        committed
    } else {
        linked_state.reparent_onto(committed);
        linked_state
            .serialize(linked_repository.clone(), token)
            .await?
    };

    let mounts = repin_staged_mounts(
        state,
        &repository,
        linked_repository.id,
        branch,
        signature,
        only,
    )
    .await?;
    for &mount in &mounts {
        if content == LinkedStagedContent::Dirty
            && linked_state
                .node_has_dirty_children(
                    linked_repository.clone(),
                    state.node(repository.clone(), mount).await?.child,
                )
                .await?
        {
            state
                .node_mark(
                    &repository,
                    mount,
                    Mark::dirty(NodeFlags::Dirty, Ext::Set(NodeExtFlags::NoFlags), true),
                )
                .await?;
        } else {
            state.node_clear(&repository, mount, Clear::Dirty).await?;
        }
    }
    Ok(mounts)
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
    /// Where the instance's working tree mounts the child.
    pub mount: RelativePath,
    /// The child's committed state, at `old_signature`.
    pub current_child: Arc<State>,
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
/// unconsumed there. Like `State::locate_node_below` but records each crossed link.
/// Bounded by `MAX_LINK_DEPTH` and a visited-repository set.
///
/// Both ends take the same shape: a mapping plus the remainder below it is the path in the
/// repository instance's file system throughout, so a walk starting below the repository root
/// answers with the same paths one starting at it does.
pub async fn resolve_link_chain(
    base: NodeMapping,
    state_current: Arc<State>,
    remainder_path: RelativePath,
    parent_branch: BranchId,
) -> Result<ResolvedLinkChain, LinkError> {
    resolve_link_chain_in(base, state_current, remainder_path, parent_branch, None).await
}

/// [`resolve_link_chain`] for a walk that stages through `tracker`: every link crossed is tracked,
/// and each level reaches the state the tracker shares for its repository
/// ([`LinkTracker::track`]), so the walk folds the change up through every intermediate link.
pub async fn resolve_tracked_link_chain(
    base: NodeMapping,
    state_current: Arc<State>,
    remainder_path: RelativePath,
    tracker: &LinkTracker,
) -> Result<ResolvedLinkChain, LinkError> {
    resolve_link_chain_in(
        base,
        state_current,
        remainder_path,
        BranchId::default(),
        Some(tracker),
    )
    .await
}

async fn resolve_link_chain_in(
    base: NodeMapping,
    state_current: Arc<State>,
    mut remainder_path: RelativePath,
    parent_branch: BranchId,
    tracker: Option<&LinkTracker>,
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
            let child_state = if let Some(tracker) = tracker {
                tracker
                    .track(
                        (&cur_repository, &cur_state),
                        cur_node,
                        child_repository_id,
                        link.revision,
                    )
                    .await
            } else {
                State::deserialize(child_repository.clone(), link.revision).await
            }
            .forward::<LinkError>("Failed deserializing state")?;
            let child_current_state = State::deserialize(child_repository.clone(), old_signature)
                .await
                .forward::<LinkError>("Failed deserializing state")?;

            mount_path.push(below_mount.as_str());
            levels.push(LinkChainLevel {
                repository: cur_repository.clone(),
                state: cur_state.clone(),
                link_node_id: cur_node,
                child_repository_id,
                branch: resolved_branch,
                stored_branch: link_reference.branch_to_store(resolved_branch, cur_branch),
                old_signature,
                mount: mount_path.clone().freeze(),
                current_child: child_current_state.clone(),
            });

            cur_repository = child_repository;
            cur_state = child_state;
            cur_current_state = child_current_state;
            cur_branch = resolved_branch;
            cur_node = link.node;
            base_node = link.node;
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

    /// The repository and state of each level, outer -> inner, ending with `innermost`'s.
    pub fn holders(&self) -> impl Iterator<Item = (&Arc<RepositoryContext>, &Arc<State>)> {
        self.levels
            .iter()
            .map(|level| (&level.repository, &level.state))
            .chain([(&self.innermost.repository, &self.innermost.state)])
    }

    /// How a pin change staged in `innermost` realizes: in the working tree where `innermost` is
    /// the instance's repository, and otherwise with the outermost link's pin change
    /// ([`propagate_link_chain`]).
    pub fn innermost_realize(&self) -> LinkPinRealize {
        if self.levels.is_empty() {
            LinkPinRealize::WorkingTree
        } else {
            LinkPinRealize::StateOnly
        }
    }

    /// The link `remainder_path` names below `innermost`, with its node ID there. `link_path` is
    /// the path the chain was resolved from, which an error names.
    pub async fn innermost_link(
        &self,
        link_path: &RelativePath,
    ) -> Result<(NodeID, Node), LinkError> {
        let NodeMapping {
            repository, state, ..
        } = &self.innermost;
        let node_id = state
            .locate_node_below(
                repository.clone(),
                self.innermost.node,
                self.remainder_path.as_str(),
            )
            .await
            .and_then(FoundNode::into_own_node)
            .forward::<LinkError>("Invalid path")?;
        if !node_id.is_valid_node_id() {
            return Err(InvalidPath {
                path: link_path.to_string(),
            }
            .into());
        }
        let link_node = state
            .node(repository.clone(), node_id)
            .await
            .forward::<LinkError>("Failed deserializing state")?;
        if !link_node.is_link() {
            return Err(NotALink {
                path: link_path.to_string(),
            }
            .into());
        }
        Ok((node_id, link_node))
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
}

/// Reserialize each linked state inner -> outer and fold its new signature into
/// its parent's link node and registry, so a change made in the innermost repo
/// is captured transitively by the top-level staged anchor.
///
/// Each state stages first the moves its staged climbs promoted ([`drain_promoted_moves`]), the
/// top-level one included. The caller is responsible for serializing + anchoring the top-level
/// state afterwards.
///
/// `realize` applies to the outermost link's pin, whose change realizes the whole chain's at the
/// link's path in the repository the chain starts from, which is the working tree's path for a
/// chain resolved from the instance's root.
pub async fn propagate_link_chain(
    chain: &ResolvedLinkChain,
    token: &RepositoryWriteToken,
    realize: LinkPinRealize,
) -> Result<(), LinkError> {
    propagate_link_levels(chain, chain.levels.len(), token, realize).await
}

/// [`propagate_link_chain`] through the outermost `levels` of `chain` alone, where the levels
/// below them already pin what they should.
pub async fn propagate_link_levels(
    chain: &ResolvedLinkChain,
    levels: usize,
    token: &RepositoryWriteToken,
    realize: LinkPinRealize,
) -> Result<(), LinkError> {
    // Inner -> outer: serialize each child before repinning its parent.
    for i in (0..levels).rev() {
        let level = &chain.levels[i];
        let (child_state, child_repository) = chain.child_at(i);

        child_state.reparent_onto(level.old_signature);
        drain_promoted_moves(&child_repository, &child_state).await?;
        let new_signature = child_state
            .serialize(child_repository.clone(), token)
            .await
            .forward::<LinkError>("Failed to serialize linked state")?;

        if i == 0 && matches!(realize, LinkPinRealize::WorkingTree) {
            Box::pin(stage_level_pin(level, &child_repository, new_signature)).await?;
            continue;
        }
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
    let (top_repository, top_state) = match chain.levels.first() {
        Some(level) => (&level.repository, &level.state),
        None => (&chain.innermost.repository, &chain.innermost.state),
    };
    drain_promoted_moves(top_repository, top_state).await
}

/// Stages `new_signature` as the pin of `level`'s link, a link of the instance's own repository,
/// realizing the change at its mount. Boxed by its caller, which takes it only for a merge or its
/// abort.
async fn stage_level_pin(
    level: &LinkChainLevel,
    child_repository: &Arc<RepositoryContext>,
    new_signature: Hash,
) -> Result<(), LinkError> {
    let repository = &level.repository;
    let (path, link_node, reference) = tokio::try_join!(
        level
            .state
            .node_path_below(repository.clone(), level.link_node_id, ROOT_NODE),
        level.state.node(repository.clone(), level.link_node_id),
        level.state.link_find(
            repository.clone(),
            level.child_repository_id,
            level.link_node_id
        ),
    )
    .forward::<LinkError>("Failed reading the link")?;
    let path = path.ok_or_else(|| LinkError::internal("Failed reading the link"))?;
    stage_link_pin(
        repository.clone(),
        &level.state,
        child_repository,
        path,
        link_node,
        reference.signature,
        new_signature,
        level.stored_branch,
        LinkMountSource::SamePath,
        LinkPinRealize::WorkingTree,
    )
    .await
    .map(drop)
}

/// Stages each whole move a staged climb in `state` promoted, a link's repin or its add among
/// them, as staging inside the directory does
/// ([`crate::file::stage::stage_promoted_moves_of_call`]): boxed behind
/// [`State::holds_promoted_moves`], as only a climb through a directory marked dirty as moved
/// queues one.
async fn drain_promoted_moves(
    repository: &Arc<RepositoryContext>,
    state: &State,
) -> Result<(), LinkError> {
    if !state.holds_promoted_moves() {
        return Ok(());
    }
    Box::pin(crate::file::stage::stage_promoted_moves_of_call(
        repository, state, None,
    ))
    .await
    .forward::<LinkError>("Failed to stage the move of a directory holding a link")
    .map(drop)
}

/// Reserialize every link modified during a `stage`/`unstage` walk, folding
/// each linked repository's new revision into its parent's pin. Processes
/// innermost-first and closes over ancestors (if C changed, B must reserialize
/// so A's pin updates). The mounts of a repository in one parent share its state, which is
/// reserialized once and pinned at each mount [`sharing_mounts`] selects. `mark_staged` re-marks
/// the walked link nodes on stage; unstage manages its own staged flags.
pub async fn drain_link_tracker(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state: Arc<State>,
    link_tracker: &LinkTracker,
    mark_staged: bool,
) -> Result<(), StateError> {
    if !link_tracker.has_modifications() {
        return Ok(());
    }

    let all_contexts = link_tracker.all_links();

    // Modified links plus their transitive ancestors, each with every mount of it, which share
    // its state.
    let mut selected: Vec<LinkContext> = link_tracker.get_links_needing_rehash();
    let mut i = 0;
    while i < selected.len() {
        let parent_repo_id = selected[i].parent_repository_id;
        if parent_repo_id != repository.id {
            for parent_ctx in all_contexts
                .iter()
                .filter(|other| other.link_repository_id == parent_repo_id)
            {
                if !selected.contains(parent_ctx) {
                    selected.push(parent_ctx.clone());
                }
            }
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

    for (_, mounts) in group_by(&contexts, |ctx| {
        (
            ctx.parent_repository_id,
            ctx.link_repository_id,
            Arc::as_ptr(&ctx.link_state) as usize,
        )
    }) {
        let link_context = mounts[0];
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

        let linked_state = &link_context.link_state;
        let parent_signature = linked_state.committed_revision();
        lore_debug!("Setting link parent to {}", parent_signature);

        let linked_repository = parent_repository
            .to_link_context(link_context.link_repository_id)
            .await;
        let holding = mounts_holding_changes(
            (&parent_repository, &parent_state),
            (&linked_repository, linked_state),
            &mounts
                .iter()
                .map(|ctx| ctx.link_node_id)
                .collect::<Vec<_>>(),
        )
        .await?;
        let only = sharing_mounts(linked_state, &holding);
        linked_state.reparent_onto(parent_signature);
        let new_signature = linked_state.serialize(linked_repository, token).await?;
        let moved = repin_staged_mounts(
            &parent_state,
            &parent_repository,
            link_context.link_repository_id,
            staged_ref.branch,
            new_signature,
            only,
        )
        .await?;

        lore_debug!(
            "Updating link nodes {moved:?} of repository {}: old_hash={}, new_hash={}",
            link_context.link_repository_id,
            staged_ref.signature,
            new_signature
        );

        if mark_staged {
            for link_node in holding {
                parent_state
                    .node_mark(
                        &parent_repository,
                        link_node,
                        Mark::staged(NodeFlags::Staged, true),
                    )
                    .await?;
            }
        }
    }

    Ok(())
}

/// Force-realizes specific file paths from a linked repository state at the
/// mount path, regardless of any state-to-state diff.
///
/// Restore on-disk content from `link_state` for those of `paths` (mount-relative) the view
/// keeps, and unlink any `~mine`/`~theirs`/`~base` sidecars at the same paths.
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
            if repository
                .filter
                .excludes(&mount_path, false, FilterMode::View)
            {
                continue;
            }

            let Some(node_id) = link_state
                .locate_node_below(link_context.clone(), mounted, link_relative.as_str())
                .await
                .forward::<LinkError>("Failed resolving link node")?
                .own_node()
            else {
                continue;
            };
            let block = link_state
                .block(link_context.clone(), NodeBlock::index(node_id))
                .await
                .forward::<LinkError>("Failed deserializing state node block")?;
            let node = block.node(Node::index(node_id));
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
                .any(|entry| !entry.is_conflict() && entry.local_node == link.local_node);
            (link, merged)
        })
        .collect())
}

/// The node of `link`, a link row of `state`, with its path, or `None` where the node is not staged.
pub(crate) async fn staged_link_node(
    state: &State,
    repository: &Arc<RepositoryContext>,
    link: &LinkReference,
) -> Result<Option<(Node, String)>, StateError> {
    let node = state.node(repository.clone(), link.local_node).await?;
    if !node.is_staged() {
        return Ok(None);
    }
    let path = state.node_path(repository.clone(), link.local_node).await?;
    Ok(Some((node, path)))
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
        let Ok(Some((_, path))) = staged_link_node(state_staged, &repository, link_reference).await
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

impl LinkAtPath {
    /// Whether `other` is the same link: at the same path, of the same repository, branch, pin
    /// and flags, and mounting the same node.
    pub fn same_link(&self, other: &LinkAtPath) -> bool {
        self.path == other.path
            && self.reference.repository() == other.reference.repository()
            && self.reference.branch() == other.reference.branch()
            && self.reference.signature() == other.reference.signature()
            && self.reference.flags() == other.reference.flags()
            && self.mount_source == other.mount_source
    }
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

/// Whether a link of `state_from`'s registry differs from `state_to`'s, per
/// [`touched_link_repositories`].
pub async fn link_registries_differ(
    repository: Arc<RepositoryContext>,
    state_from: &Arc<State>,
    state_to: &Arc<State>,
) -> Result<bool, LinkError> {
    let (from_pins, to_pins) = tokio::try_join!(
        link_pins_by_path(state_from, repository.clone()),
        link_pins_by_path(state_to, repository),
    )?;
    Ok(touched_link_repositories(&from_pins, &to_pins)
        .next()
        .is_some())
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
/// path with a link on one side only, or with another link per
/// [`LinkAtPath::same_link`].
pub fn touched_link_repositories<'a>(
    from_pins: &'a [LinkAtPath],
    to_pins: &'a [LinkAtPath],
) -> impl Iterator<Item = RepositoryId> + 'a {
    let unmatched = |pins: &'a [LinkAtPath], others: &'a [LinkAtPath]| {
        pins.iter()
            .filter(move |link| !others.iter().any(|other| other.same_link(link)))
            .map(|link| link.reference.repository())
    };
    unmatched(from_pins, to_pins).chain(unmatched(to_pins, from_pins))
}

/// Returns true if `staged_node` is a staged change to an existing link itself (not an add or
/// remove), with no staged changes on the linked side.
pub async fn is_staged_link_update(
    staged_node: &Node,
    parent_repository: Arc<RepositoryContext>,
) -> Result<bool, LinkError> {
    if !staged_node.is_link()
        || !staged_node.is_staged()
        || staged_node.is_staged_add()
        || staged_node.is_staged_delete()
    {
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
/// and realizes the changes on disk. A provider holding its state is written
/// nothing of the link's content: it serves the link from its node, which moves to the new pin
/// ([`serve_link_pin`]).
///
/// Opens the filesystem operation the realize takes, so a caller already holding one calls
/// [`realize_link_pin_change_in_operation`] instead.
pub async fn realize_link_pin_change(
    repository: Arc<RepositoryContext>,
    link_context: Arc<RepositoryContext>,
    pin_change: LinkPinRealizeTarget,
) -> Result<(), LinkError> {
    if repository.file_system().holds_state() {
        return with_operation(repository.file_system(), async |operation| {
            serve_link_pin(&operation, &link_context, pin_change).await
        })
        .await;
    }
    let pin = ResolvedLinkPin::resolve(&link_context, pin_change).await?;
    realize_resolved_link_pin(repository, link_context, &pin).await
}

/// [`realize_link_pin_change`] for a pin already read against both revisions.
pub(crate) async fn realize_resolved_link_pin(
    repository: Arc<RepositoryContext>,
    link_context: Arc<RepositoryContext>,
    pin: &ResolvedLinkPin,
) -> Result<(), LinkError> {
    if repository.file_system().holds_state() {
        return with_operation(repository.file_system(), async |operation| {
            serve_resolved_link_pin(&operation, pin).await
        })
        .await;
    }
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
#[lore_macro::test_pub]
pub(crate) async fn realize_link_pin_change_in_operation(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    link_context: Arc<RepositoryContext>,
    pin_change: LinkPinRealizeTarget,
    overwrite_local: bool,
) -> Result<(), LinkError> {
    if operation.holds_state() {
        return serve_link_pin(operation, &link_context, pin_change).await;
    }
    let pin = ResolvedLinkPin::resolve(&link_context, pin_change).await?;
    realize_resolved_link_pin_in_operation(
        operation,
        repository,
        link_context,
        &pin,
        overwrite_local,
    )
    .await
}

/// [`realize_link_pin_change_in_operation`] for a pin already read against both revisions.
pub(crate) async fn realize_resolved_link_pin_in_operation(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    link_context: Arc<RepositoryContext>,
    pin: &ResolvedLinkPin,
    overwrite_local: bool,
) -> Result<(), LinkError> {
    if operation.holds_state() {
        return serve_resolved_link_pin(operation, pin).await;
    }
    let diff = link_pin_diff(&link_context, pin).await?;
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

/// Serves the link at `target`'s mount at its new pin through `operation`, whose provider holds
/// its state and serves a link from its node (ADR-00036): the link moves to the pin, mounting the
/// subtree `target` names, and a link whose new pin is none is taken away. The linked revisions
/// are read only to find a subtree mounted by its path.
async fn serve_link_pin(
    operation: &InstanceOperationImpl,
    link_context: &Arc<RepositoryContext>,
    target: LinkPinRealizeTarget,
) -> Result<(), LinkError> {
    if target.new_signature == Hash::default() {
        return operation
            .place_link(&target.link_path, None, false)
            .await
            .forward::<LinkError>("Failed taking away the link");
    }
    let source = match target.mount_source {
        LinkMountSource::Node(node) => node,
        LinkMountSource::SamePath if target.current_node == ROOT_NODE => ROOT_NODE,
        LinkMountSource::SamePath => {
            return serve_resolved_link_pin(
                operation,
                &ResolvedLinkPin::resolve(link_context, target).await?,
            )
            .await;
        }
    };
    operation
        .repin_link(&target.link_path, target.new_signature, source)
        .await
        .forward::<LinkError>("Failed serving the link at its new pin")
}

/// Serves through `operation` the outermost link `chain` crosses, as the staged state holds it
/// once a change below it is folded up into its pin ([`propagate_link_chain`]), where the provider
/// holds its state: such a provider serves a nested link through the outermost link's pin
/// (ADR-00036). Nothing where `chain` crosses no link.
pub(crate) async fn serve_outermost_link(
    operation: &InstanceOperationImpl,
    chain: &ResolvedLinkChain,
) -> Result<(), LinkError> {
    let Some(outermost) = chain.levels.first() else {
        return Ok(());
    };
    if !operation.holds_state() {
        return Ok(());
    }
    let node = outermost
        .state
        .node(outermost.repository.clone(), outermost.link_node_id)
        .await
        .forward::<LinkError>("Failed reading the outermost link")?;
    operation
        .place_link(
            &outermost.mount,
            Some(&node),
            execution_context().globals().force(),
        )
        .await
        .forward::<LinkError>("Failed serving the outermost link")
}

/// [`serve_link_pin`] of a pin already read against both revisions.
async fn serve_resolved_link_pin(
    operation: &InstanceOperationImpl,
    pin: &ResolvedLinkPin,
) -> Result<(), LinkError> {
    operation
        .repin_link(&pin.link_path, pin.new_signature, pin.node_target)
        .await
        .forward::<LinkError>("Failed serving the link at its new pin")
}

/// [`realize_link_pin_change_in_operation`] for a cold path, boxed so it doesn't grow the future of
/// its caller. Boxed with an explicit `Send`, which a caller the realize reaches again needs.
pub(crate) fn realize_link_pin_change_boxed(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    link_context: Arc<RepositoryContext>,
    pin_change: LinkPinRealizeTarget,
    overwrite_local: bool,
) -> crate::BoxFuture<'_, Result<(), LinkError>> {
    Box::pin(realize_link_pin_change_in_operation(
        operation,
        repository,
        link_context,
        pin_change,
        overwrite_local,
    ))
}

/// Diffs the pinned subtree between the two pins, spelled from the mount path, leaving out each
/// node both hold as it is, which only marks of a side's set it apart.
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

    let mut changes = state::diff_collect_subtree(
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
        state::DiffFlags::empty(),
        FilterMode::View,
    )
    .await
    .forward::<LinkError>("Failed syncing target link")?;
    // A node both pins hold as it is, marked staged or dirty on a side, holds nothing to realize,
    // and a staged file's content may be on disk alone, which a check against the pin refuses.
    let marks = crate::change::Flags::Staged | crate::change::Flags::Dirty;
    changes.retain(|change| {
        change.action != crate::change::FileAction::Keep || !(change.flags - marks).is_empty()
    });
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
) -> Result<VerifiedChanges, LinkError> {
    let LinkPinDiff { changes, current } = diff;
    if changes.is_empty() {
        return Ok(VerifiedChanges {
            changes,
            kept: Vec::new(),
        });
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
    let verified = if overwrite_local {
        VerifiedChanges {
            changes: diff.changes,
            kept: Vec::new(),
        }
    } else {
        verify_link_pin_diff(operation, link_context, new_sig, diff, true).await?
    };
    apply_link_pin_changes(operation, repository, verified).await
}

/// Realizes each of `pins`, a pin move of the linked repository its context names, verifying the
/// working tree at every mount before it writes to any, so a local modification at one leaves
/// every mount as it was. Where the provider holds its state, serves each link at its new pin
/// instead ([`serve_resolved_link_pin`]).
pub(crate) async fn realize_resolved_link_pins_in_operation(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    pins: &[(Arc<RepositoryContext>, ResolvedLinkPin)],
) -> Result<(), LinkError> {
    if operation.holds_state() {
        for (_, pin) in pins {
            serve_resolved_link_pin(operation, pin).await?;
        }
        return Ok(());
    }
    let verified = futures::future::try_join_all(pins.iter().map(async |(link_context, pin)| {
        let diff = link_pin_diff(link_context, pin).await?;
        verify_link_pin_diff(
            operation,
            link_context.clone(),
            pin.new_signature,
            diff,
            true,
        )
        .await
    }))
    .await?;
    futures::future::try_join_all(
        verified
            .into_iter()
            .map(|changes| apply_link_pin_changes(operation, repository.clone(), changes)),
    )
    .await?;
    Ok(())
}

/// Realizes the verified changes of a pin move in the working tree.
async fn apply_link_pin_changes(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    VerifiedChanges { changes, kept }: VerifiedChanges,
) -> Result<(), LinkError> {
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
        &kept,
        None, /* No subtrees */
        None,
        false, /* Not dry run */
        None,  /* Not a merge */
        Some(&crate::fs::realize::EmptiedDirectories::Excluded),
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

/// Where the working tree materializes the subtree at `source` of a repository.
#[derive(Clone)]
pub(crate) struct Mount {
    pub path: RelativePath,
    pub source: RelativePath,
}

impl Mount {
    /// The main repository's, materialized whole at the root of the working tree.
    pub(crate) fn root() -> Self {
        Self {
            path: RelativePath::new(),
            source: RelativePath::new(),
        }
    }

    /// The mount at `path` of the subtree whose root is `node` in `link_state`, a linked
    /// repository's state.
    pub(crate) async fn of_link(
        link_context: &Arc<RepositoryContext>,
        link_state: &State,
        node: NodeID,
        path: RelativePath,
    ) -> Result<Self, StateError> {
        Ok(Self {
            source: link_source_path(link_context.clone(), link_state, node).await?,
            path,
        })
    }

    /// Where the working tree holds `path`, a path of the repository, or `None` outside the
    /// subtree.
    pub(crate) fn spell(&self, path: &RelativePath) -> Option<RelativePath> {
        path.below(&self.source).map(|below| self.path.join(below))
    }

    /// [`Self::spell`] of `path`, a path of the repository as stored.
    pub(crate) fn spell_str(&self, path: &str) -> Option<RelativePath> {
        let Ok(path) = RelativePath::from_str(path);
        self.spell(&path)
    }
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

/// Rejects an incoming mount whose source subtree nests with one `state_target` already holds, or
/// with another mount arriving in the same set of changes. A mount of the same folder is left to
/// the merge: one moving a link holds the mount it removes beside the one it adds.
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

        let node = change.to.get_node().await?;

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
            SameFolder::Allow,
        )
        .await?;

        if let Some((_, other)) = incoming.iter().find(|(other_id, other_path)| {
            *other_id == link_id && SameFolder::Allow.overlap(&source_path, other_path)
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

/// Whether [`check_source_path_overlap`] refuses a mount of the same folder as another.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SameFolder {
    Refuse,
    Allow,
}

impl SameFolder {
    /// Whether two source subtrees overlap as this refuses. Compared case-insensitively: the two
    /// paths are read from the revisions their own mounts pin, so a case-only rename between
    /// those revisions leaves one spelling of a subtree both still address.
    fn overlap(self, left: &RelativePath, right: &RelativePath) -> bool {
        left.overlaps_ignore_case(right)
            && (self == Self::Refuse || left.as_lowercase_str() != right.as_lowercase_str())
    }
}

/// Refuses `source_path` of the repository of `link_context` where another mount of it in `state`
/// exposes a folder inside or above it, or, unless `same_folder` allows it, the same folder: the
/// working tree would hold the nodes they share twice, and a commit can read each of them from
/// only one place.
///
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
    same_folder: SameFolder,
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

        if !same_folder.overlap(&source_path, &mounted_source) {
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

/// The path of the first link mounted at, inside or above `path`, in `state_staged` or
/// `state_current`. A link staged for delete counts: unstaging the delete would bring it back over
/// whatever was mounted there. `lore link remove` takes such a link out of the staged registry, so
/// the current registry is read as well, each link's node placed where the staged tree, which
/// keeps a node staged for delete, holds it.
#[lore_macro::test_pub]
pub(crate) async fn overlapping_link_mount(
    state_current: &State,
    state_staged: &State,
    repository: Arc<RepositoryContext>,
    path: &RelativePath,
) -> Result<Option<RelativePath>, StateError> {
    let mut link_references = state_staged.link_list(repository.clone()).await?;
    link_references.extend(state_current.link_list(repository.clone()).await?);
    for link_reference in link_references {
        let node = link_reference.local_node;
        let link_path = match state_staged
            .node_path_below(repository.clone(), node, ROOT_NODE)
            .await?
        {
            Some(link_path) => Some(link_path),
            None => {
                state_current
                    .node_path_below(repository.clone(), node, ROOT_NODE)
                    .await?
            }
        };
        if let Some(link_path) = link_path
            && path.overlaps_ignore_case(&link_path)
        {
            return Ok(Some(link_path));
        }
    }
    Ok(None)
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
    let found = state
        .locate_node(repository.clone(), link_path)
        .await
        .forward::<LinkError>("Invalid path")?;

    if !found.is_valid() {
        return Err(InvalidPath {
            path: link_path.to_string(),
        }
        .into());
    }

    // The path may cross parent links, so the node and reference lookups target the registry of
    // the repository holding the link node.
    let (local_node, (parent_repository, parent_state)) =
        found.held((repository, state.clone())).into_parts();

    let link_node = parent_state
        .node(parent_repository.clone(), local_node)
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
        local_node,
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
/// Finds the node via `State::locate_node` (which descends through any parent
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
    /// Both sides having changed it is a conflict for the user to settle.
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
    target: LinkReference,
    incoming: LinkReference,
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

/// A link row no single side of a merge owns: both changed its pin, branch, flags or mount away
/// from their common ancestor, or neither had an ancestor to change it from. Each side's mount is
/// a node of the revision that side pins.
pub struct LinkPinConflict {
    pub link_path: String,
    pub source: LinkReference,
    pub source_mount: NodeID,
    pub target: LinkReference,
    pub target_mount: NodeID,
}

/// What comparing the parent's link rows across a merge found.
#[derive(Default)]
pub struct LinkPinClassification {
    pub planned: Vec<PlannedLinkPin>,
    pub conflicts: Vec<LinkPinConflict>,
}

/// What comparing one link row across a merge found.
enum LinkPinClass {
    Planned(PlannedLinkPin),
    Conflict(LinkPinConflict),
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
        let link_context = repository
            .to_link_context(planned_pin.link_node.address.context.into())
            .await;
        verify_link_pin_change(
            operation,
            repository,
            &link_context,
            LinkPinRealizeTarget {
                link_path,
                old_signature: planned_pin.target.signature(),
                new_signature: planned_pin.incoming.signature(),
                current_node: planned_pin.link_node.child,
                mount_source: planned_pin.mount_source(),
            },
        )
        .await?;
    }
    Ok(())
}

/// Check the working tree against what realizing `target`, a pin move of a link to
/// `link_context`, writes, refusing where a local modification would be overwritten.
pub(crate) async fn verify_link_pin_change(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    link_context: &Arc<RepositoryContext>,
    target: LinkPinRealizeTarget,
) -> Result<(), LinkError> {
    if repository
        .filter
        .excludes_tree(&target.link_path, true, FilterMode::View)
    {
        return Ok(());
    }
    let pin = ResolvedLinkPin::resolve(link_context, target).await?;
    let diff = link_pin_diff(link_context, &pin).await?;
    verify_link_pin_diff(
        operation,
        link_context.clone(),
        pin.new_signature,
        diff,
        false,
    )
    .await
    .map(drop)
}

/// The link node at `link_path` where it links `link_repository`, or `None` where the state has
/// no link to it there.
pub(crate) async fn link_node_to(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    link_path: &str,
    link_repository: RepositoryId,
) -> Result<Option<(NodeID, Node)>, LinkError> {
    Ok(link_node_at_path(state, repository.clone(), link_path)
        .await?
        .filter(|(_, node)| RepositoryId::from(node.address.context) == link_repository))
}

/// The link node at `link_path`, or `None` when the state has no link there.
async fn link_node_at_path(
    state: &Arc<State>,
    repository: Arc<RepositoryContext>,
    link_path: &str,
) -> Result<Option<(NodeID, Node)>, LinkError> {
    let found = match state.locate_node(repository.clone(), link_path).await {
        Ok(found) => found,
        Err(err) if err.is_node_not_found() => return Ok(None),
        Err(err) => return Err(err).forward::<LinkError>("Failed resolving link path"),
    };
    let Some(node_id) = found.own_node().filter(NodeIDExt::is_valid_node_id) else {
        return Ok(None);
    };

    let node = state
        .node(repository, node_id)
        .await
        .forward::<LinkError>("Failed deserializing state")?;

    Ok(node.is_link().then_some((node_id, node)))
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

/// The mounts of the links `base` holds at or below `scope`, or anywhere where `None`, that one
/// side of a merge removed while the other changed the link's row or the subtree it mounts. Each
/// side is a state with the branch it was committed on. Neither diff carries such a change: the
/// removal is one change at the mount, and the other side's is a pin or a row. A side linking the
/// repository at a path `base` does not link it at moved the link there rather than removed it.
pub(crate) async fn removed_links_changed_by_the_other_side(
    repository: &Arc<RepositoryContext>,
    base: &Arc<State>,
    sides: [(&Arc<State>, BranchId); 2],
    scope: Option<&RelativePath>,
) -> Result<Vec<RelativePath>, LinkError> {
    let base_rows = LinkRows::read(base, repository.clone()).await?;
    if base_rows.rows.is_empty() {
        return Ok(Vec::new());
    }
    let side_rows = tokio::try_join!(
        LinkRows::read(sides[0].0, repository.clone()),
        LinkRows::read(sides[1].0, repository.clone()),
    )?;
    let side_rows = [side_rows.0, side_rows.1];
    let mut base_branch = None;
    let mut mounts = Vec::new();
    for base_row in &base_rows.rows {
        let link_repository = base_row.repository;
        // A side that removed a link holds no row for its node.
        if side_rows
            .iter()
            .all(|rows| rows.row(base_row.local_node, link_repository).is_some())
        {
            continue;
        }
        let path = base
            .node_path(repository.clone(), base_row.local_node)
            .await
            .forward::<LinkError>("Failed resolving link node path")?;
        let mount =
            RelativePath::from_str(&path).internal_with(|| format!("Invalid link path {path}"))?;
        if scope.is_some_and(|scope| !scope.overlaps(&mount)) {
            continue;
        }
        let held = tokio::try_join!(
            row_at(repository, &side_rows[0], &path, link_repository),
            row_at(repository, &side_rows[1], &path, link_repository),
        )?;
        let held = [held.0, held.1];
        let (removing, changing, changed) = match &held {
            [None, Some(changed)] => (0, 1, changed),
            [Some(changed), None] => (1, 0, changed),
            _ => continue,
        };
        let Some(base_held) = row_at(repository, &base_rows, &path, link_repository).await? else {
            continue;
        };
        let base_branch = match base_branch {
            Some(branch) => branch,
            None => *base_branch.insert(parent_branch(base, repository).await?),
        };
        let unchanged = changed
            .0
            .same_row(sides[changing].1, &base_held.0, base_branch)
            && changed.1 == base_held.1;
        if !unchanged
            && !links_elsewhere(
                repository,
                &side_rows[removing],
                link_repository,
                &base_rows,
            )
            .await?
        {
            mounts.push(mount);
        }
    }
    Ok(mounts)
}

/// The row `rows` hold for the link at `path` of their state, to `link_repository` where given,
/// pinned at the revision its tree node carries, with the node it mounts.
#[lore_macro::test_pub]
pub(crate) async fn row_at(
    repository: &Arc<RepositoryContext>,
    rows: &LinkRows,
    path: &str,
    link_repository: impl Into<Option<RepositoryId>>,
) -> Result<Option<(LinkReference, NodeID)>, LinkError> {
    let Some((node_id, node)) = link_node_at_path(&rows.state, repository.clone(), path).await?
    else {
        return Ok(None);
    };
    let held_repository = RepositoryId::from(node.address.context);
    if link_repository
        .into()
        .is_some_and(|link_repository| link_repository != held_repository)
    {
        return Ok(None);
    }
    Ok(rows
        .row(node_id, held_repository)
        .map(|row| (row.with_signature(node.address.hash), node.child)))
}

/// Whether `rows` link `link_repository` at a path none of `base_rows` links it at.
async fn links_elsewhere(
    repository: &Arc<RepositoryContext>,
    rows: &LinkRows,
    link_repository: RepositoryId,
    base_rows: &LinkRows,
) -> Result<bool, LinkError> {
    let paths = async |rows: &LinkRows| {
        let mut paths = Vec::new();
        for row in rows
            .rows
            .iter()
            .filter(|row| row.repository == link_repository)
        {
            paths.push(
                rows.state
                    .node_path(repository.clone(), row.local_node)
                    .await
                    .forward::<LinkError>("Failed resolving link node path")?,
            );
        }
        Ok::<_, LinkError>(paths)
    };
    let base_paths = paths(base_rows).await?;
    Ok(paths(rows)
        .await?
        .iter()
        .any(|path| !base_paths.contains(path)))
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

/// The row `rows` hold for the link at `node_id`, pinned at the revision its tree node carries, or
/// `fallback`'s branch and flags where the registry has no row for it.
fn link_row(
    rows: &LinkRows,
    node_id: NodeID,
    node: &Node,
    fallback: &LinkReference,
) -> LinkReference {
    let link_id: RepositoryId = node.address.context.into();
    rows.row(node_id, link_id)
        .unwrap_or(LinkReference {
            repository: link_id,
            local_node: node_id,
            ..*fallback
        })
        .with_signature(node.address.hash)
}

/// The branch `state`'s revision was committed on, which its auto-following rows resolve to.
pub(crate) async fn parent_branch(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
) -> Result<BranchId, LinkError> {
    if state.metadata_hash().is_zero() {
        return Ok(BranchId::default());
    }
    Ok(state
        .revision_metadata(repository.clone())
        .await
        .forward::<LinkError>("Failed reading the branch a revision is on")?
        .branch)
}

/// The states a merge's link rows are compared between, with the branch each was committed on, and
/// the rows of the two the comparison looks rows up in.
struct LinkPinSides<'a> {
    staged: &'a Arc<State>,
    staged_branch: BranchId,
    incoming: LinkRows,
    incoming_branch: BranchId,
    base: Option<(LinkRows, BranchId)>,
}

/// Compare the parent's link rows across a merge, separating the ones the
/// incoming state changed on its own from the ones both sides changed.
///
/// The parent-level diff never carries a pin change, so the rows are compared
/// here instead. Where both sides follow their own parent's branch, the pin only
/// means something on the parent branch whose mirror produced it, so it moves by
/// the link's own branch merge. Links whose node is in `skip_nodes` keep the pin
/// that merge gave them.
pub fn classify_link_pins<'a>(
    repository: Arc<RepositoryContext>,
    state_staged: &'a Arc<State>,
    staged_branch: BranchId,
    state_incoming: &'a Arc<State>,
    resolution: LinkPinResolution,
    skip_nodes: &'a [NodeID],
) -> impl Future<Output = Result<LinkPinClassification, LinkError>> + 'a {
    classify_link_pins_except(
        repository,
        state_staged,
        staged_branch,
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
    current_branch: BranchId,
    state_incoming: &'a Arc<State>,
    resolution: LinkPinResolution,
    left_out: &'a [NodeID],
) -> impl Future<Output = Result<LinkPinClassification, LinkError>> + 'a {
    classify_link_pins_except(
        repository,
        state_current,
        current_branch,
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
    staged_branch: BranchId,
    state_incoming: &Arc<State>,
    resolution: LinkPinResolution,
    skip_nodes: &[NodeID],
    left_out: &[NodeID],
) -> Result<LinkPinClassification, LinkError> {
    let mut link_list = state_staged
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list links")?;
    link_list.retain(|link_reference| !left_out.contains(&link_reference.local_node));
    if link_list.is_empty() {
        return Ok(LinkPinClassification::default());
    }

    let state_base = match &resolution {
        LinkPinResolution::ThreeWay(state_base) => Some(state_base),
        LinkPinResolution::Incoming => None,
    };
    let (incoming_branch, incoming, base) = tokio::try_join!(
        parent_branch(state_incoming, &repository),
        LinkRows::read(state_incoming, repository.clone()),
        async {
            match state_base {
                Some(state_base) => Ok(Some((
                    LinkRows::read(state_base, repository.clone()).await?,
                    parent_branch(state_base, &repository).await?,
                ))),
                None => Ok(None),
            }
        },
    )?;
    let sides = LinkPinSides {
        staged: state_staged,
        staged_branch,
        incoming,
        incoming_branch,
        base,
    };

    // Classify every link before changing any of them, so a divergent row is
    // recorded with nothing carried.
    let classes = try_join_links(
        link_list
            .iter()
            .map(|link_reference| {
                classify_link_pin(&repository, &sides, skip_nodes, link_reference)
            })
            .collect(),
    )
    .await?;

    let mut classification = LinkPinClassification::default();
    for class in classes.into_iter().flatten() {
        match class {
            LinkPinClass::Planned(planned) => classification.planned.push(planned),
            LinkPinClass::Conflict(conflict) => classification.conflicts.push(conflict),
        }
    }
    Ok(classification)
}

/// [`classify_link_pins`] for one link.
async fn classify_link_pin(
    repository: &Arc<RepositoryContext>,
    sides: &LinkPinSides<'_>,
    skip_nodes: &[NodeID],
    link_reference: &LinkReference,
) -> Result<Option<LinkPinClass>, LinkError> {
    let link_path = sides
        .staged
        .node_path(repository.clone(), link_reference.local_node)
        .await
        .forward::<LinkError>("Failed resolving link node path")?;

    let link_node = sides
        .staged
        .node(repository.clone(), link_reference.local_node)
        .await
        .forward::<LinkError>("Failed deserializing state")?;
    let target_pin = link_node.address.hash;
    let target = link_reference.with_signature(target_pin);

    // An added or removed link has no row on one side; the tree carries it.
    let Some((incoming_node_id, incoming_node)) =
        link_node_at_path(&sides.incoming.state, repository.clone(), &link_path).await?
    else {
        return Ok(None);
    };
    if incoming_node.address.context != link_node.address.context {
        lore_debug!("Link {link_path} points at another repository on the incoming side");
        return Ok(None);
    }

    let incoming_pin = incoming_node.address.hash;
    // A link merged in its own right already holds the pin that merge produced, so its row is
    // compared on what else it carries.
    let pin_merged_already = skip_nodes.contains(&link_reference.local_node);
    let merged_pin = |row: LinkReference| {
        if pin_merged_already {
            row.with_signature(target_pin)
        } else {
            row
        }
    };
    let incoming_row = link_row(
        &sides.incoming,
        incoming_node_id,
        &incoming_node,
        link_reference,
    );
    let incoming = merged_pin(incoming_row);
    let row_unchanged = incoming.same_row(sides.incoming_branch, &target, sides.staged_branch);
    if row_unchanged && incoming_pin == target_pin && link_node.mounts_same_source(&incoming_node) {
        return Ok(None);
    }

    // The row both sides changed from, which the mount and the row are each judged against.
    let base = match &sides.base {
        Some((base_rows, base_branch)) => {
            link_node_at_path(&base_rows.state, repository.clone(), &link_path)
                .await?
                .map(|(base_node_id, base_node)| {
                    let base_row = merged_pin(link_row(
                        base_rows,
                        base_node_id,
                        &base_node,
                        link_reference,
                    ));
                    (base_node, base_row, *base_branch)
                })
        }
        None => None,
    };
    if base
        .as_ref()
        .is_some_and(|(base_node, base_row, base_branch)| {
            base_node.address == incoming_node.address
                && base_node.child == incoming_node.child
                && base_row.same_row(*base_branch, &incoming, sides.incoming_branch)
        })
    {
        return Ok(None);
    }
    let three_way = sides.base.is_some();
    let mut conflicted = false;

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
            state_incoming_pin,
            current_mount,
            incoming_mount,
        ))
    };

    // This branch's remount, where the merged branch left the mount alone, with the revision the
    // merged branch pins, which a pin the merge carries has to hold it.
    let mut kept_mount = None;
    if three_way && let Some((_, state_incoming_pin, current_mount, incoming_mount)) = &remount {
        // The row comparison below cannot see a side that moved only the mount, so the mount is
        // judged against the base on its own.
        let base_mount = match base.as_ref() {
            Some((base_node, _, _)) if base_node.address.context == link_node.address.context => {
                Some(
                    pinned_source_path(link_context.clone(), base_node)
                        .await
                        .forward::<LinkError>("Failed resolving the subtree a link exposes")?,
                )
            }
            _ => None,
        };
        match base_mount.as_ref().map(RelativePath::as_lowercase_str) {
            Some(base) if base == current_mount.as_lowercase_str() => {}
            Some(base) if base == incoming_mount.as_lowercase_str() => {
                kept_mount = Some((state_incoming_pin.clone(), current_mount.clone()));
                remount = None;
            }
            _ => conflicted = true,
        }
    }

    if pin_merged_already && remount.is_none() && row_unchanged {
        lore_debug!(
            "Link node {} pin is owned by its own merge, leaving it",
            link_reference.local_node
        );
        return Ok(None);
    }

    let mut adopt_row = !row_unchanged;

    // Rows following their own parent's branch on both sides differ in the pin alone, which
    // resolves against whichever branch the state ends up on, so it moves by the link's own merge.
    if adopt_row
        && incoming.foreign_branch(sides.incoming_branch).is_none()
        && target.foreign_branch(sides.staged_branch).is_none()
    {
        lore_debug!("Link {link_path} follows the parent branch, leaving its pin to its own merge");
        adopt_row = false;
    }

    if adopt_row && let Some((_, base_row, base_branch)) = base.as_ref() {
        if base_row.same_row(*base_branch, &incoming, sides.incoming_branch) {
            adopt_row = false;
        } else if !base_row.same_row(*base_branch, &target, sides.staged_branch) {
            conflicted = true;
        }
    } else if adopt_row && three_way {
        conflicted = true;
    }
    // The pin a link's own merge produced is on the linked branch this branch follows, so a row the
    // merged branch changed otherwise cannot land on it: the user chooses between the two.
    if adopt_row && pin_merged_already {
        conflicted = true;
    }
    if adopt_row
        && !conflicted
        && incoming_pin != target_pin
        && let Some((state_incoming_pin, current_mount)) = &kept_mount
    {
        conflicted = !state_incoming_pin
            .locate_node(link_context.clone(), current_mount.as_str())
            .await
            .is_ok_and(|found| found.is_valid_or_root());
    }

    let carried = incoming_row.carried(sides.incoming_branch, sides.staged_branch);
    let readable = repository.can_read_link(link_id);
    let conflict = |source: LinkReference, source_mount: NodeID| {
        if !readable {
            return Err(LinkError::internal(format!(
                "Link conflict at {link_path}: both branches changed a link to a repository this \
                 user cannot read, so its row cannot be settled here. Merge as a user who can \
                 read it"
            )));
        }
        Ok(Some(LinkPinClass::Conflict(LinkPinConflict {
            link_path: link_path.clone(),
            source,
            source_mount,
            target,
            target_mount: link_node.child,
        })))
    };

    let remount = match remount {
        None => None,
        Some(_) if conflicted => return conflict(carried, incoming_node.child),
        Some((_, _, _, incoming_mount)) if adopt_row || incoming_pin == target_pin => {
            Some((incoming_mount, incoming_node.child))
        }
        Some((state_target_pin, _, _, incoming_mount)) => {
            // A remount the pin does not travel with lands at the pin this branch keeps, and one
            // that pin does not hold leaves the two sides' rows for the user to choose between.
            let node_id = state_target_pin
                .locate_node(link_context, incoming_mount.as_str())
                .await
                .ok()
                .and_then(|found| found.own_node());
            match node_id {
                Some(node_id) => Some((incoming_mount, node_id)),
                None => return conflict(carried, incoming_node.child),
            }
        }
    };
    if conflicted {
        return conflict(carried, incoming_node.child);
    }

    if !adopt_row && remount.is_none() {
        return Ok(None);
    }

    Ok(Some(LinkPinClass::Planned(PlannedLinkPin {
        link_path,
        link_node,
        target,
        incoming: if adopt_row { carried } else { target },
        remount,
    })))
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
        let mount_source = planned_pin.mount_source();
        let PlannedLinkPin {
            link_path,
            link_node,
            target,
            incoming,
            remount,
        } = planned_pin;

        let link_context = repository
            .to_link_context(link_node.address.context.into())
            .await;

        if target.signature() != incoming.signature() {
            lore_info!(
                "Link {link_path} pin {} -> {} from merged branch",
                target.signature(),
                incoming.signature()
            );
        }
        if let Some((path, _)) = &remount {
            lore_info!("Link {link_path} exposes {path} from the merged branch");
        }

        stage_link_row(
            repository.clone(),
            state_staged,
            &link_context,
            &link_path,
            link_node,
            &target,
            &incoming,
            mount_source,
            realize,
        )
        .await?;

        carried.push(CarriedLinkPin {
            link_path,
            previous: target.signature(),
            adopted: incoming.signature(),
        });
    }

    Ok(carried)
}

/// Check the working tree against what [`apply_resolved_link_pin`] realizes for `chosen`, refusing
/// where a local modification would be overwritten.
pub async fn verify_resolved_link_pin(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    link_path: &str,
    chosen: &LinkReference,
    chosen_mount: NodeID,
) -> Result<(), LinkError> {
    let link_path_rel = RelativePath::from_str(link_path)
        .internal_with(|| format!("Invalid link path {link_path}"))?;
    let resolved = resolve_link_at_path(state_staged, repository.clone(), link_path).await?;
    verify_link_pin_change(
        operation,
        repository,
        &resolved.link_context,
        LinkPinRealizeTarget {
            link_path: link_path_rel,
            old_signature: resolved.link_node.address.hash,
            new_signature: chosen.signature(),
            current_node: resolved.link_node.child,
            mount_source: LinkMountSource::Node(chosen_mount),
        },
    )
    .await
}

/// Move the row at `link_path` to `chosen`, mounting `chosen_mount`, a node of the revision
/// `chosen` pins, and realizing the content change at the mount as `realize` says.
pub async fn apply_resolved_link_pin(
    repository: Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    link_path: &str,
    chosen: LinkReference,
    chosen_mount: NodeID,
    realize: LinkPinRealize,
) -> Result<(), LinkError> {
    let resolved = resolve_link_at_path(state_staged, repository.clone(), link_path).await?;
    let current = resolved
        .link_reference
        .with_signature(resolved.link_node.address.hash);
    stage_link_row(
        repository,
        state_staged,
        &resolved.link_context,
        link_path,
        resolved.link_node,
        &current,
        &chosen,
        LinkMountSource::Node(chosen_mount),
        realize,
    )
    .await
    .map(drop)
}

/// Write `chosen` over the row `current` at `link_path`, pin, branch and flags together, mounting
/// `mount_source` and realizing the content change at the mount. Returns the staged link node, and
/// does nothing where the row already is `chosen` at that mount.
#[allow(clippy::too_many_arguments)]
pub async fn stage_link_row(
    repository: Arc<RepositoryContext>,
    state: &Arc<State>,
    link_context: &Arc<RepositoryContext>,
    link_path: &str,
    link_node: Node,
    current: &LinkReference,
    chosen: &LinkReference,
    mount_source: LinkMountSource,
    realize: LinkPinRealize,
) -> Result<NodeID, LinkError> {
    if current.stores_same(chosen)
        && matches!(mount_source, LinkMountSource::Node(mount) if mount == link_node.child)
    {
        return Ok(current.local_node);
    }
    let link_path_rel = RelativePath::from_str(link_path)
        .internal_with(|| format!("Invalid link path {link_path}"))?;

    let node = stage_link_pin(
        repository.clone(),
        state,
        link_context,
        link_path_rel,
        link_node,
        current.signature(),
        chosen.signature(),
        chosen.branch(),
        mount_source,
        realize,
    )
    .await?;

    if current.flags() != chosen.flags() {
        state
            .link_replace(
                repository,
                LinkReference {
                    local_node: node,
                    ..*chosen
                },
            )
            .await
            .forward::<LinkError>("Failed to update link flags")?;
    }
    Ok(node)
}

/// [`classify_link_pins`] followed by [`apply_link_pins`].
pub async fn merge_link_pins(
    repository: Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    staged_branch: BranchId,
    state_incoming: &Arc<State>,
    resolution: LinkPinResolution,
    realize: LinkPinRealize,
    skip_nodes: &[NodeID],
) -> Result<Vec<CarriedLinkPin>, LinkError> {
    let classification = classify_link_pins(
        repository.clone(),
        state_staged,
        staged_branch,
        state_incoming,
        resolution,
        skip_nodes,
    )
    .await?;
    // Only a three-way comparison leaves a row for the user to settle.
    if let Some(conflict) = classification.conflicts.first() {
        return Err(LinkError::internal(format!(
            "Link conflict at {}: the incoming row cannot be carried over this branch's",
            conflict.link_path
        )));
    }

    Box::pin(apply_link_pins(
        repository,
        state_staged,
        classification.planned,
        realize,
    ))
    .await
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
    if !link_reference.auto_follows() {
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
