// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Read-only, bounded inspection of the content an obliteration can remove.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use lore_base::types::FragmentFlags;
use lore_base::types::FragmentReference;
use lore_error_set::prelude::*;
use lore_storage::TypedBytes;
use tokio_stream::StreamExt;

use super::ObliterateError;
use crate::branch;
use crate::immutable;
use crate::lore::Address;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_warn;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateNodeChildrenIterator;
use crate::store::StoreMatch;
use crate::store::remote::RemoteImmutableStore;
use crate::store::remote::RemoteMutableStore;

/// A warning must not leave an administrative deletion waiting on an unbounded scan.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BRANCHES: usize = 10_000;
const MAX_FRAGMENT_DEPTH: usize = 8;
const MAX_FRAGMENT_REFERENCES: usize = 100_000;

#[lore_macro::test_pub]
pub(super) struct Budget {
    nodes: usize,
    fragments: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            nodes: 1_000_000,
            fragments: MAX_FRAGMENT_REFERENCES,
        }
    }
}

fn spend(remaining: &mut usize) -> Result<(), ObliterateError> {
    *remaining = remaining
        .checked_sub(1)
        .ok_or_else(|| ObliterateError::internal("obliteration impact check limit reached"))?;
    Ok(())
}

/// Reads list payloads only. A leaf's bytes are never needed to establish a dependency.
async fn children(
    repository: &Arc<RepositoryContext>,
    address: Address,
) -> Result<Vec<Address>, ObliterateError> {
    let metadata = repository
        .immutable_store()
        .get_metadata(repository.id, address)
        .await
        .forward::<ObliterateError>("checking fragment metadata")?;
    if metadata.match_made != StoreMatch::MatchFull {
        return Err(ObliterateError::internal(
            "fragment metadata is unavailable",
        ));
    }
    if metadata.fragment.flags & FragmentFlags::PayloadFragmented == 0 {
        return Ok(Vec::new());
    }
    if metadata.fragment.size_payload as usize
        > MAX_FRAGMENT_REFERENCES * std::mem::size_of::<FragmentReference>()
    {
        return Err(ObliterateError::internal(
            "fragment list size limit reached",
        ));
    }
    let (fragment, payload) = immutable::load_raw(
        repository.clone(),
        address,
        lore_storage::ReadOptions::default().no_remote(),
    )
    .await
    .forward::<ObliterateError>("reading fragment dependencies")?;
    if payload.len() > MAX_FRAGMENT_REFERENCES * std::mem::size_of::<FragmentReference>() {
        return Err(ObliterateError::internal(
            "fragment list size limit reached",
        ));
    }
    if payload.len() % std::mem::size_of::<FragmentReference>() != 0 {
        return Err(ObliterateError::internal("invalid fragment list length"));
    }
    let payload = payload.to_aligned::<FragmentReference>();
    let references = payload.as_type_slice::<FragmentReference>();
    let start = references
        .first()
        .ok_or_else(|| ObliterateError::internal("fragment dependency list is empty"))?
        .offset_content;
    let end = start
        .checked_add(fragment.size_content)
        .ok_or_else(|| ObliterateError::internal("fragment dependency content offset overflow"))?;
    lore_storage::defragment::wanted_entries(references, end, &(start..end))
        .forward::<ObliterateError>("validating fragment dependencies")?;
    Ok(references
        .iter()
        .map(|reference| Address {
            hash: reference.hash,
            context: address.context,
        })
        .collect())
}

/// Records even a partial deletion set, so a failed child read does not hide a known root match.
#[lore_macro::test_pub]
#[allow(clippy::while_let_loop)] // Keep the popped value out of the await state.
pub(super) async fn deletion_set(
    repository: &Arc<RepositoryContext>,
    address: Address,
    budget: &mut Budget,
    addresses: &mut HashSet<Address>,
) -> Result<(), ObliterateError> {
    let mut pending = vec![(address, 0)];
    loop {
        let Some((address, depth)) = pending.pop() else {
            break;
        };
        if !addresses.insert(address) {
            continue;
        }
        spend(&mut budget.fragments)?;
        if depth > MAX_FRAGMENT_DEPTH {
            return Err(ObliterateError::internal(
                "fragment dependency depth limit reached",
            ));
        }
        for child in children(repository, address).await? {
            if pending.len() >= budget.fragments {
                return Err(ObliterateError::internal(
                    "fragment dependency limit reached",
                ));
            }
            pending.push((child, depth + 1));
        }
    }
    Ok(())
}

async fn content_affected(
    repository: &Arc<RepositoryContext>,
    address: Address,
    deleted: &HashSet<Address>,
    budget: &mut Budget,
) -> Result<bool, ObliterateError> {
    let mut pending = vec![(address, 0)];
    let mut visited = HashSet::new();
    loop {
        let Some((address, depth)) = pending.pop() else {
            return Ok(false);
        };
        if deleted.contains(&address) {
            return Ok(true);
        }
        if !visited.insert(address) {
            continue;
        }
        spend(&mut budget.fragments)?;
        if depth > MAX_FRAGMENT_DEPTH {
            return Err(ObliterateError::internal(
                "fragment dependency depth limit reached",
            ));
        }
        for child in children(repository, address).await? {
            if pending.len() >= budget.fragments {
                return Err(ObliterateError::internal(
                    "fragment dependency limit reached",
                ));
            }
            pending.push((child, depth + 1));
        }
    }
}

/// Walks the snapshot itself, independent of the instance's sparse view and revision parents.
#[lore_macro::test_pub]
pub(super) async fn snapshot_affected(
    repository: &Arc<RepositoryContext>,
    signature: Hash,
    target: Address,
    deleted: &HashSet<Address>,
    budget: &mut Budget,
) -> Result<bool, ObliterateError> {
    let state = State::deserialize(repository.clone(), signature)
        .await
        .forward::<ObliterateError>("reading latest revision")?;
    let mut pending = vec![(repository.clone(), state, ROOT_NODE)];
    let mut visited = HashSet::new();
    let mut content = HashMap::new();
    loop {
        let Some((current, state, id)) = pending.pop() else {
            return Ok(false);
        };
        spend(&mut budget.nodes)?;
        if !visited.insert((current.id, state.revision(), id)) {
            continue;
        }
        let node = state
            .node(current.clone(), id)
            .await
            .forward::<ObliterateError>("reading latest node")?;
        if node.is_discarded() || node.is_staged_delete() {
            continue;
        }
        if node.is_file() {
            if current.id == repository.id
                && node.address.context == target.context
                && !node.address.hash.is_zero()
            {
                if let Some(affected) = content.get(&node.address) {
                    if *affected {
                        return Ok(true);
                    }
                } else {
                    let affected =
                        content_affected(&current, node.address, deleted, budget).await?;
                    content.insert(node.address, affected);
                    if affected {
                        return Ok(true);
                    }
                }
            }
        } else if node.is_link() {
            let link = node.linked_node();
            let linked = current.to_link_context(link.repository).await;
            let state = State::deserialize(linked.clone(), link.revision)
                .await
                .forward::<ObliterateError>("reading linked revision")?;
            pending.push((linked, state, link.node));
        } else {
            let mut iter =
                StateNodeChildrenIterator::from_parent(state.clone(), current.clone(), id, &node)
                    .await
                    .forward::<ObliterateError>("reading latest children")?;
            loop {
                let Some((child, _)) = iter
                    .next()
                    .await
                    .forward::<ObliterateError>("reading latest child")?
                else {
                    break;
                };
                if pending.len() >= budget.nodes {
                    return Err(ObliterateError::internal(
                        "snapshot dependency limit reached",
                    ));
                }
                pending.push((current.clone(), state.clone(), child));
            }
        }
    }
}

async fn check_branches(
    repository: &Arc<RepositoryContext>,
    target: Address,
    branches: &[(crate::lore::BranchId, Hash)],
    location: &str,
) {
    let mut budget = Budget::default();
    let mut deleted = HashSet::new();
    if let Err(error) = deletion_set(repository, target, &mut budget, &mut deleted).await {
        lore_warn!(
            "Could not fully check {location} obliteration impact: {error}. Cloning or syncing affected revisions may fail after deletion."
        );
    }
    let mut checked = HashMap::new();
    for (branch, latest) in branches {
        if latest.is_zero() {
            continue;
        }
        let affected = if let Some(affected) = checked.get(latest) {
            *affected
        } else {
            match snapshot_affected(repository, *latest, target, &deleted, &mut budget).await {
                Ok(affected) => {
                    checked.insert(*latest, affected);
                    affected
                }
                Err(error) => {
                    lore_warn!(
                        "Could not check {location} branch {branch} for obliteration impact: {error}. Cloning or syncing affected revisions may fail after deletion."
                    );
                    if budget.nodes == 0 || budget.fragments == 0 {
                        break;
                    }
                    continue;
                }
            }
        };
        if affected {
            lore_warn!(
                "Obliterating {target} removes content required by {location} branch {branch} at latest revision {latest}. Cloning or syncing files that require this content will fail after deletion."
            );
        }
    }
}

#[allow(clippy::while_let_loop)] // Keep the stream item out of the await state.
async fn check(repository: &Arc<RepositoryContext>, target: Address) {
    if target.context.is_zero() {
        lore_warn!(
            "Cannot check branch impact for a zero-context address. Obliteration may remove revision metadata and prevent cloning or syncing."
        );
        return;
    }
    match repository.remote().await {
        Ok(remote) => {
            match branch::list_remote_bounded(remote.clone(), repository.id, MAX_BRANCHES).await {
                Ok(None) => lore_warn!(
                    "Remote branch impact check exceeded its limit; cloning or syncing affected revisions may fail after deletion."
                ),
                Ok(Some(branches)) => {
                    // Ordinary branch listing deliberately skips unreadable records and does
                    // not signal omissions. The advisory check cannot establish completeness.
                    lore_warn!(
                        "Remote branch listing may omit unreadable branches. Obliteration impact for omitted branches is unknown."
                    );
                    for branch in &branches {
                        if branch.latest.is_zero() {
                            lore_warn!(
                                "Remote branch {} has no latest revision or its latest could not be read. Its obliteration impact is unknown.",
                                branch.id
                            );
                        }
                    }
                    let execution = execution_context();
                    let identity = execution.globals().identity();
                    let remote_repository = Arc::new(RepositoryContext::new_server_context(
                        Arc::new(RemoteImmutableStore::new(remote.remote_url(), identity)),
                        Arc::new(RemoteMutableStore::new(remote.remote_url(), identity)),
                        repository.id,
                    ));
                    let branches: Vec<_> = branches
                        .iter()
                        .map(|branch| (branch.id, branch.latest))
                        .collect();
                    check_branches(&remote_repository, target, &branches, "remote").await;
                }
                Err(error) => lore_warn!(
                    "Could not list remote branches for obliteration impact: {error}. Cloning or syncing affected revisions may fail after deletion."
                ),
            }
        }
        Err(error) => lore_warn!(
            "Remote obliteration impact is unknown: {error}. Only local branches can be checked."
        ),
    }
    let mut branches = Vec::new();
    match branch::list(repository.clone()).await {
        Ok(mut stream) => loop {
            let Some(branch) = stream.next().await else {
                break;
            };
            if branch.is_zero() {
                continue;
            }
            if branches.len() == MAX_BRANCHES {
                lore_warn!(
                    "Local branch impact check exceeded its limit; syncing affected revisions may fail after deletion."
                );
                break;
            }
            match branch::load_latest(repository.clone(), branch).await {
                Ok(latest) => branches.push((branch, latest)),
                Err(error) => lore_warn!(
                    "Could not check local branch {branch}: {error}. Its obliteration impact is unknown."
                ),
            }
        },
        Err(error) => lore_warn!(
            "Could not list local branches: {error}. Local obliteration impact is unknown."
        ),
    }
    check_branches(repository, target, &branches, "local").await;
}

pub(super) async fn warn(repository: &Arc<RepositoryContext>, target: Address) {
    if tokio::time::timeout(CHECK_TIMEOUT, check(repository, target))
        .await
        .is_err()
    {
        lore_warn!(
            "Obliteration impact check timed out. Cloning or syncing affected revisions may fail after deletion."
        );
    }
}
