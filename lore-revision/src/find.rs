// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cmp::Ordering;
use std::sync::Arc;

use lore_error_set::prelude::*;
use lore_storage::options::ReadOptions;
use serde::Deserialize;
use serde::Serialize;

use crate::branch;
use crate::errors::SlowDown;
use crate::errors::absent_unless;
use crate::event;
use crate::immutable;
use crate::immutable::ReadFromImmutable;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::Address;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_trace;
use crate::metadata::Metadata;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateData;

pub const DEFAULT_SEARCH_LIMIT: usize = 1000;

#[error_set]
pub enum FindError {
    SlowDown,
}

impl event::EventError for FindError {
    fn translated(&self) -> LoreError {
        match self {
            FindError::SlowDown(_) => LoreError::SlowDown,
            FindError::Internal(_) => LoreError::Internal,
        }
    }
}

pub enum FindMatchResult {
    Match,
    Continue,
    Abort,
}

/// Walk a branch's history from `revision` until `matcher` accepts a revision.
///
/// A zero `revision` starts from the branch latest, preferring the remote one so
/// a search covers revisions the local store has not seen. An offline or
/// local-only operation takes the local latest alone, since waiting on the
/// connect is the whole cost such an operation asked to avoid.
pub async fn find_revision<F>(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    revision: Hash,
    with_metadata: bool,
    search_limit: Option<usize>,
    mut matcher: F,
) -> Result<Hash, FindError>
where
    F: FnMut(Arc<State>, Option<Metadata>) -> FindMatchResult,
{
    let mut revision = revision;
    if revision.is_zero() {
        if !execution_context().globals().offline_or_local()
            && let Ok(remote) = repository.remote().await
        {
            // no propagation here: a throttled or unreachable remote is what
            // the local fallback below exists for, so it stays a zero latest
            // rather than a failure.
            revision = branch::load_remote_latest(remote, repository.id, branch)
                .await
                .unwrap_or_default();
        }
        if revision.is_zero() {
            revision = absent_unless::<_, _, FindError>(
                branch::load_latest(repository.clone(), branch).await,
                branch::BranchError::is_slow_down,
                "loading branch latest",
            )?
            .unwrap_or_default();
        }
    }
    lore_debug!("Find start revision {}", revision);
    let mut search_count = 0;
    while !revision.is_zero() {
        search_count += 1;
        if let Some(search_limit) = search_limit
            && search_count > search_limit
        {
            return Err(FindError::internal("search limit reached"));
        }

        // Check if we have the revision cached locally
        if !immutable::is_stored_local(repository.clone(), Address::zero_context_hash(revision))
            .await
        {
            // Batch load a chunk of history
            // TODO(mjansson): Hide batch cache latency by issuing requests while iterating
            //                 previous batch of history, trying to stay ahead of the load curve
            batch_load_history(repository.clone(), revision).await;
        }

        let state = State::deserialize(repository.clone(), revision)
            .await
            .forward_any::<FindError>("deserializing state")?;

        let metadata = if with_metadata {
            Some(
                Metadata::deserialize(repository.clone(), state.metadata_hash())
                    .await
                    .forward_any::<FindError>("deserializing metadata")?,
            )
        } else {
            None
        };

        match matcher(state.clone(), metadata) {
            FindMatchResult::Continue => {
                lore_trace!(
                    "Revision {} does not match, continue to parent revision {}",
                    revision,
                    state.parent_self()
                );
            }
            FindMatchResult::Match => {
                lore_debug!(
                    "Found matching revision {} after {} iterations",
                    revision,
                    search_count
                );
                return Ok(revision);
            }
            FindMatchResult::Abort => {
                lore_debug!("Revision {} does not match, abort search", revision,);
                break;
            }
        }

        revision = state.parent_self();
    }

    lore_debug!(
        "Found NO matching revision after {} iterations",
        search_count
    );

    // No emit, let caller decide if it is an error or not
    Err(FindError::internal("no revision found"))
}

/// Find revision in the current branch history which has a matching key-value pair in metadata
pub async fn revision_by_metadata(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    revision: Hash,
    key: &str,
    value: Option<&str>,
) -> Result<Hash, FindError> {
    lore_debug!(
        "Find revision where {} = {}",
        key,
        value.unwrap_or("<None>"),
    );
    find_revision(
        repository.clone(),
        branch,
        revision,
        true, /* With metadata */
        None,
        |state, metadata| {
            if let Some(metadata) = metadata
                && let Ok(found_value) = metadata.get_string(key)
            {
                if let Some(value) = value {
                    if found_value == value {
                        lore_debug!(
                            "Metadata match {} = {} for revision {}",
                            key,
                            value,
                            state.revision()
                        );
                        return FindMatchResult::Match;
                    }
                    return FindMatchResult::Continue;
                }
                lore_debug!(
                    "Metadata match key = {} for revision {}",
                    key,
                    state.revision()
                );
                return FindMatchResult::Match;
            }
            FindMatchResult::Continue
        },
    )
    .await
}

/// Find revision by number in the current branch
pub async fn revision_by_number(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    revision_start: Hash,
    revision_number: u64,
) -> Result<Hash, FindError> {
    lore_debug!("Find revision {}", revision_number);
    find_revision(
        repository.clone(),
        branch,
        revision_start,
        false, /* No metadata */
        None,
        |state, _metadata| {
            let state_revision_number = state.revision_number();
            match state_revision_number.cmp(&revision_number) {
                Ordering::Equal => {
                    lore_debug!(
                        "Revision number {} match search number {} for revision {}",
                        state_revision_number,
                        revision_number,
                        state.revision()
                    );
                    FindMatchResult::Match
                }
                Ordering::Less => {
                    lore_debug!(
                        "Revision number {} less than search number {} for revision {}",
                        state_revision_number,
                        revision_number,
                        state.revision()
                    );
                    FindMatchResult::Abort
                }
                Ordering::Greater => {
                    lore_trace!(
                        "Revision number {} greater than search number {} for revision {}",
                        state_revision_number,
                        revision_number,
                        state.revision()
                    );
                    FindMatchResult::Continue
                }
            }
        },
    )
    .await
}

pub const BATCH_COUNT: usize = 100;

pub async fn batch_load_history(
    repository: Arc<RepositoryContext>,
    mut revision: Hash,
) -> Vec<Hash> {
    let mut history = Vec::with_capacity(BATCH_COUNT);

    while !revision.is_zero()
        && history.len() < BATCH_COUNT
        && let Ok(state) = StateData::read_from_immutable(
            repository.clone(),
            Address::zero_context_hash(revision),
            ReadOptions::default().no_remote(),
        )
        .await
    {
        history.push(revision);
        revision = state.parent[0];
    }

    if revision.is_zero() || history.len() >= BATCH_COUNT {
        return history;
    }

    let Ok(remote) = repository.remote().await else {
        return vec![];
    };
    let Ok(revision_protocol) = remote.revision(repository.id).await else {
        return vec![];
    };

    lore_debug!("Query remote for revision state history");

    let Ok(response) = revision_protocol.revision_list(revision.into()).await else {
        return vec![];
    };

    lore_debug!("Got {} revisions", response.items.len(),);

    cache_revision_list_states(repository.clone(), &response.items).await;

    let metadata_addresses: Vec<Address> = response
        .items
        .iter()
        .map(|item| Address::zero_context_hash(item.metadata))
        .collect();

    let _ = immutable::cache(repository, metadata_addresses, true).await;

    history.append(&mut response.items.iter().map(|item| item.signature).collect());

    history
}

/// Cache the per-item state blobs from a remote `revision_list` response into
/// the local immutable store. Each item carries the 320-byte serialized
/// `StateData` for its revision; verifying its blake3 hash against the
/// `signature` lets the local cache absorb the response without a follow-up
/// fetch and protects against a server returning bytes that do not match the
/// signature it advertises.
pub async fn cache_revision_list_states(
    repository: Arc<RepositoryContext>,
    items: &[lore_transport::RevisionItem],
) {
    let expected_len = std::mem::size_of::<StateData>();
    for item in items {
        if item.state.len() != expected_len {
            continue;
        }
        if Hash::hash_buffer(&item.state) != item.signature {
            continue;
        }
        let _ = immutable::write(
            repository.clone(),
            Context::default(),
            item.state.clone(),
            immutable::write_options_from_repository(repository.clone())
                .with_revision_state()
                .with_local_cache_priority()
                .no_remote_write(),
        )
        .await;
    }
}

/// Data for the event reporting a revision found by a search.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionFindEventData {
    /// Signature of the revision that was found.
    pub signature: Hash,
}

pub enum FindOptions {
    KeyValue { key: LoreString, value: LoreString },
    Number(u64),
}

pub async fn find_impl(
    repository: Arc<RepositoryContext>,
    options: FindOptions,
) -> Result<(), FindError> {
    let (_current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward_any::<FindError>("deserializing current anchor")?;

    let result = match options {
        FindOptions::KeyValue { key, value } => {
            let value = if !value.is_empty() {
                Some(value.as_str())
            } else {
                None
            };
            revision_by_metadata(
                repository.clone(),
                current_branch,
                Hash::default(),
                key.as_str(),
                value,
            )
            .await
        }
        FindOptions::Number(number) => {
            if number == 0 {
                Err(FindError::internal("no revision specified"))
            } else {
                revision_by_number(repository.clone(), current_branch, Hash::default(), number)
                    .await
            }
        }
    };

    match result {
        Ok(signature) => {
            event::LoreEvent::RevisionFind(LoreRevisionFindEventData { signature }).send();
            Ok(())
        }
        Err(err) => {
            event::LoreEvent::RevisionFind(LoreRevisionFindEventData {
                signature: Hash::default(),
            })
            .send();
            Err(err)
        }
    }
}
