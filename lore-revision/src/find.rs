// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cmp::Ordering;
use std::sync::Arc;

use lore_error_set::prelude::*;
use lore_storage::options::ReadOptions;
use serde::Serialize;
use zerocopy::FromBytes;

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
            let _ = batch_load_history(repository.clone(), revision).await;
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

/// The line from `revision` back through self parents, newest first, each revision with its
/// number: the revisions the local store holds, up to [`BATCH_COUNT`], then the remote's page from
/// the first revision the store lacks, whole. Empty where the remote cannot list that page. The
/// number of a listed revision is read from its state, as [`verified_number`] describes, and the
/// page's states are cached as [`cache_page_states`] describes. Fails where the page lists a
/// revision on the line without its own state.
pub async fn batch_load_history(
    repository: Arc<RepositoryContext>,
    mut revision: Hash,
) -> Result<Vec<(u64, Hash)>, FindError> {
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
        history.push((state.revision_number, revision));
        revision = state.parent[0];
    }

    if revision.is_zero() || history.len() >= BATCH_COUNT {
        return Ok(history);
    }

    let Ok(remote) = repository.remote().await else {
        return Ok(vec![]);
    };
    let Ok(revision_protocol) = remote.revision(repository.id).await else {
        return Ok(vec![]);
    };

    lore_debug!("Query remote for revision state history");

    let Ok(response) = revision_protocol.revision_list(revision.into()).await else {
        return Ok(vec![]);
    };

    lore_debug!("Got {} revisions", response.items.len(),);

    let line = response.items_from(revision);
    for item in line {
        let Some(number) = verified_number(item) else {
            return Err(FindError::internal(format!(
                "the remote listed revision {} without its own state",
                item.signature
            )));
        };
        history.push((number, item.signature));
    }
    let metadata_addresses: Vec<Address> = line
        .iter()
        .map(|item| Address::zero_context_hash(item.metadata))
        .collect();
    let unverified = response.items.len() - line.len();

    let _ = immutable::cache(repository.clone(), metadata_addresses, true).await;
    cache_page_states(repository, &response.items, unverified).await;

    Ok(history)
}

/// Cache the per-item state blobs from a remote `revision_list` response into
/// the local immutable store. Each item carries the 320-byte serialized
/// `StateData` for its revision; verifying its blake3 hash against the
/// `signature` lets the local cache absorb the response without a follow-up
/// fetch and protects against a server returning bytes that do not match the
/// signature it advertises.
pub fn cache_revision_list_states(
    repository: Arc<RepositoryContext>,
    items: &[lore_transport::RevisionItem],
) -> impl Future<Output = ()> + '_ {
    cache_page_states(repository, items, items.len())
}

/// Write the states a remote page's `items` carry to the local immutable store, so the revisions
/// they describe are read without a remote fetch. Each of the first `unverified` items is written
/// where its state is its revision's own, as [`holds_its_state`] checks, so bytes that don't hash
/// to the item's signature are not stored. Each item after them is one the caller has checked the
/// same way, as [`verified_number`] does, so it is written without hashing it again.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub(crate) fn cache_page_states(
    repository: Arc<RepositoryContext>,
    items: &[lore_transport::RevisionItem],
    mut unverified: usize,
) -> impl Future<Output = ()> + '_ {
    async move {
        for item in items {
            if unverified > 0 {
                unverified -= 1;
                if !holds_its_state(item) {
                    continue;
                }
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
}

/// Whether `item` carries its revision's state: the bytes its signature is the hash of.
fn holds_its_state(item: &lore_transport::RevisionItem) -> bool {
    item.state.len() == std::mem::size_of::<StateData>()
        && Hash::hash_buffer(&item.state) == item.signature
}

/// The revision number in the state `item` carries, where that state is the revision's own, as
/// [`holds_its_state`] checks. The number beside it in the item is the server's claim; the state's
/// is the one its signature fixes.
pub(crate) fn verified_number(item: &lore_transport::RevisionItem) -> Option<u64> {
    if !holds_its_state(item) {
        return None;
    }
    StateData::read_from_bytes(&item.state)
        .ok()
        .map(|state| state.revision_number)
}

/// Data for the event reporting a revision found by a search.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionFindEventData {
    /// Signature of the revision that was found.
    pub signature: Hash,
}

pub enum FindOptions {
    KeyValue { key: LoreString, value: LoreString },
    Number(u64),
}

pub(crate) async fn find(
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

/// Boxed version of [`find`] for cross-crate use.
pub fn find_boxed(
    repository: Arc<RepositoryContext>,
    options: FindOptions,
) -> crate::BoxFuture<'static, Result<(), FindError>> {
    Box::pin(find(repository, options))
}
