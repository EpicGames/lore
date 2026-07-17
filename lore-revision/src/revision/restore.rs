// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::branch;
use crate::branch::BranchLatestStatus;
use crate::branch::push::PushStatistics;
use crate::branch::push::push_fragments;
use crate::branch::push::push_query;
use crate::change;
use crate::commit;
use crate::errors::*;
use crate::event::EventError;
use crate::filter::FilterMode;
use crate::find;
use crate::interface::LoreError;
use crate::interface::LoreEvent;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::metadata;
use crate::metadata::Metadata;
use crate::metadata::MetadataType;
use crate::metadata::RESTORED_FROM;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::revision::sync;
use crate::state;
use crate::util::serde::u8_as_bool;

/// Event data reported at the start of the file phase of a restore.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionRestoreFileBeginEventData {
    /// Number of files to process.
    pub count: usize,
}

/// Event data reported for a single file during a restore.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionRestoreFileEventData {
    /// Path of the file.
    pub path: LoreString,
    /// Action applied to the file.
    pub action: LoreFileAction,
    /// Size of the file in bytes.
    pub size: u64,
    /// Flag indicating the entry is a file.
    #[serde(with = "u8_as_bool")]
    pub is_file: u8,
    /// Flag indicating the entry is a directory.
    #[serde(with = "u8_as_bool")]
    pub is_directory: u8,
    /// Flag indicating the entry is a module.
    #[serde(with = "u8_as_bool")]
    pub is_module: u8,
}

/// Event data reported at the end of the file phase of a restore.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionRestoreFileEndEventData {
    /// Number of files processed.
    pub count: usize,
}

/// Event data reported at the start of the fragment phase of a restore.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionRestoreFragmentBeginEventData {
    /// Number of fragments to transfer.
    pub fragments: u64,
}

/// Event data reported on progress of the fragment phase of a restore.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionRestoreFragmentProgressEventData {
    /// Number of fragments completed.
    pub complete: u64,
    /// Total number of fragments.
    pub count: u64,
}

/// Event data reported at the end of the fragment phase of a restore.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionRestoreFragmentEndEventData {
    /// Number of fragments transferred.
    pub fragments: u64,
}

/// Event data reported with the resulting revision of a restore.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionRestoreRevisionEventData {
    /// Resulting revision hash signature.
    pub revision: Hash,
    /// Resulting revision number.
    pub revision_number: u64,
}

/// Event data reported at the start of the sync phase of a restore.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionRestoreSyncBeginEventData {
    /// Number of changes to apply.
    pub count: usize,
}

/// Event data reported at the end of the sync phase of a restore.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionRestoreSyncEndEventData {
    /// Number of changes applied.
    pub count: usize,
}

#[error_set]
pub enum RestoreError {
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    RevisionNotFound,
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
    BranchNotFound,
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

impl EventError for RestoreError {
    fn translated(&self) -> LoreError {
        match self {
            RestoreError::Disconnected(_) => LoreError::Connection,
            RestoreError::SlowDown(_) => LoreError::SlowDown,
            RestoreError::Oversized(_) => LoreError::Oversized,
            RestoreError::FileNotFound(_) => LoreError::FileNotFound,
            RestoreError::NotFound(_)
            | RestoreError::LayerNotFound(_)
            | RestoreError::RevisionNotFound(_) => LoreError::NotFound,
            RestoreError::AddressNotFound(_) => LoreError::AddressNotFound,
            RestoreError::PayloadNotFound(_) => LoreError::PayloadNotFound,
            RestoreError::InvalidPath(_) | RestoreError::InvalidArguments(_) => {
                LoreError::InvalidArguments
            }
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[derive(Clone, Debug)]
pub struct RestoreOptions {
    pub message: Option<String>,
}

pub async fn restore(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: RestoreOptions,
) -> Result<(), RestoreError> {
    let context = execution_context();
    let call = context.globals();

    let (current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<RestoreError>("loading current anchor")?;

    let remote_head = if let Ok(remote) = repository.remote().await {
        branch::load_remote(remote.clone(), repository.id, current_branch)
            .await
            .forward::<RestoreError>("loading remote head")?
            .latest
    } else {
        Hash::default()
    };
    if remote_head.is_zero() {
        return Err(RestoreError::internal("Invalid head"));
    }

    let local_head = branch::load_latest(repository.clone(), current_branch)
        .await
        .forward::<RestoreError>("loading local head")?;

    if local_head != remote_head && !call.force() {
        return Err(RestoreError::internal("Branch behind latest revision"));
    }

    let head_revision = remote_head;

    let current_state = state::State::deserialize(repository.clone(), current_revision)
        .await
        .forward::<RestoreError>("deserializing current state")?;
    lore_debug!("Restoring {}", current_revision);

    // Verify we are not divergent unless forced
    if !call.force() {
        let result = find::find_revision(
            repository.clone(),
            current_branch,
            head_revision,
            false,
            None,
            |state, _metadata| {
                if state.revision() == current_state.revision() {
                    find::FindMatchResult::Match
                } else if state.revision_number() < current_state.revision_number() {
                    // Divergence, the remote branch history passed the point
                    // where local revision should have been found
                    find::FindMatchResult::Abort
                } else {
                    find::FindMatchResult::Continue
                }
            },
        )
        .await;
        if result.is_err() {
            return Err(RestoreError::internal("Branch divergent from remote"));
        }
    }

    let head_state = state::State::deserialize(repository.clone(), head_revision)
        .await
        .forward::<RestoreError>("deserializing head state")?;

    let mut changes = state::diff_collect(
        repository.clone(),
        head_state.clone(),
        repository.clone(),
        current_state.clone(),
        None, /* No subpath */
        FilterMode::View,
    )
    .await
    .forward::<RestoreError>("diffing states")?;
    lore_debug!(
        "Found {} change(s) to make on current head {}",
        changes.len(),
        head_revision
    );

    change::sort_by_path(&mut changes);

    let changes_count = changes.len();
    LoreEvent::RevisionRestoreFileBegin(LoreRevisionRestoreFileBeginEventData {
        count: changes_count,
    })
    .send();

    for change in &changes {
        let node = {
            if change.action == change::FileAction::Delete {
                let block = change
                    .from
                    .state
                    .block(
                        change.from.repository.clone(),
                        NodeBlock::index(change.from.node),
                    )
                    .await
                    .forward::<RestoreError>("deserializing state node block")?;
                block.node(Node::index(change.from.node))
            } else {
                let block = change
                    .to
                    .state
                    .block(
                        change.to.repository.clone(),
                        NodeBlock::index(change.to.node),
                    )
                    .await
                    .forward::<RestoreError>("deserializing state node block")?;
                block.node(Node::index(change.to.node))
            }
        };

        LoreEvent::RevisionRestoreFile(LoreRevisionRestoreFileEventData {
            path: LoreString::from(&change.path),
            action: change.action.into(),
            size: node.size,
            is_file: node.is_file() as u8,
            is_directory: node.is_directory() as u8,
            is_module: node.is_link() as u8,
        })
        .send();
    }

    LoreEvent::RevisionRestoreFileEnd(LoreRevisionRestoreFileEndEventData {
        count: changes_count,
    })
    .send();

    // Finish execution after gathering files potentially affected by restore
    if call.dry_run() {
        return Ok(());
    }

    // Prepare the staged state, starting from the head state
    let state_staged = state::State::deserialize(repository.clone(), head_state.revision())
        .await
        .forward::<RestoreError>("deserializing head state for staging")?;

    LoreEvent::RevisionRestoreSyncBegin(LoreRevisionRestoreSyncBeginEventData {
        count: changes_count,
    })
    .send();

    // Apply the changes on the state (but not disk)
    let stats = Arc::new(sync::SyncRealizeStats::default());
    sync::realize_changes(
        repository.clone(),
        Arc::new(changes.clone()),
        Some(state_staged.clone()),
        true,  /* No changes on disk */
        false, /* No merge */
        stats,
    )
    .await
    .forward::<RestoreError>("realizing changes on state")?;
    lore_debug!("Realized changes on state");

    LoreEvent::RevisionRestoreSyncEnd(LoreRevisionRestoreSyncEndEventData {
        count: changes_count,
    })
    .send();

    // Apply the metadata on the state
    branch::merge::merge_metadata(
        repository.clone(),
        Arc::new(changes.clone()),
        current_state.clone(),
        state_staged.clone(),
    )
    .await
    .forward::<RestoreError>("merging metadata on state")?;
    lore_debug!("Merged metadata on state");

    // Get or create metadata chunk
    let metadata_hash = state_staged.metadata_hash();
    if metadata_hash.is_zero() {
        return Err(RestoreError::internal("Failed to deserialize metadata"));
    }
    let original_metadata = Metadata::deserialize(repository.clone(), metadata_hash)
        .await
        .forward::<RestoreError>("deserializing original metadata")?;

    let message = options.message.unwrap_or(
        original_metadata
            .get_string(metadata::MESSAGE)
            .forward::<RestoreError>("reading commit message from metadata")?
            .to_owned(),
    );

    let metadata_keys = vec![String::from(RESTORED_FROM)];
    let metadata_values = vec![current_state.revision().to_string()];
    let metadata_formats = vec![MetadataType::Hash];

    let metadata = commit::prepare_commit_metadata(
        repository.clone(),
        original_metadata,
        current_branch,
        message.clone(),
        Some(metadata_keys),
        Some(metadata_values),
        Some(metadata_formats),
    )
    .await
    .forward::<RestoreError>("preparing commit metadata")?;

    commit::prune_dirty_for_commit(state_staged.clone(), repository.clone())
        .await
        .forward::<RestoreError>("pruning dirty nodes before rehash")?;

    // Own tracker scoped to this rehash step: await_all always runs before
    // propagating the rehash result so no spawned leader outlives the
    // function holding references to local state.
    let rehash_tracker = std::sync::Arc::new(lore_storage::write_tracker::WriteTracker::new());
    let rehash_result = commit::commit_files_and_rehash(
        repository.clone(),
        token.share(),
        state_staged.clone(),
        repository.require_path()?,
        metadata.clone(),
        None,
        std::sync::Arc::new(std::collections::HashMap::new()),
        current_branch,
        rehash_tracker.clone(),
    )
    .await;
    let drain_result = rehash_tracker.await_all().await;
    rehash_result.forward::<RestoreError>("rehashing state")?;
    drain_result.forward::<RestoreError>("draining rehash tracker")?;
    lore_debug!("Rehashed state");

    let new_state = state_staged;
    new_state.reset_merge_conflict_flags();
    new_state.set_parent_other(Hash::default());
    new_state.set_parent_self(head_state.revision());

    new_state.set_metadata_hash(
        metadata
            .serialize(repository.clone())
            .await
            .forward::<RestoreError>("serializing metadata")?,
    );

    commit::weave_history(repository.clone(), new_state.clone())
        .await
        .forward::<RestoreError>("weaving history")?;

    let signature = new_state
        .serialize(repository.clone(), token)
        .await
        .forward::<RestoreError>("serializing new state")?;

    // Check missing fragments on server
    lore_debug!(
        "Calculating new fragments from {} to {}",
        head_state.revision(),
        new_state.revision()
    );
    let fragments = state::collect_new_fragments(
        repository.clone(),
        head_state.clone(),
        new_state.clone(),
        true, /* Ignore already durably stored fragments */
    )
    .await
    .forward::<RestoreError>("collecting new fragments")?;

    let mut revision = signature;
    let mut revision_number = new_state.revision_number();
    let mut remote_pushed = false;
    if let Ok(remote) = repository.remote().await {
        let stats = Arc::new(PushStatistics::default());

        LoreEvent::RevisionRestoreFragmentBegin(LoreRevisionRestoreFragmentBeginEventData {
            fragments: fragments.len() as u64,
        })
        .send();

        let correlation_id = execution_context().globals().correlation_id.to_string();
        let storage_protocol = remote
            .session(repository.id, &correlation_id)
            .await
            .forward::<RestoreError>("opening storage session")?;
        let revision_protocol = remote
            .revision(repository.id)
            .await
            .forward::<RestoreError>("opening revision protocol")?;

        let missing_fragments = push_query(
            storage_protocol.clone(),
            fragments,
            remote.environment.max_query_batch(),
        )
        .await
        .forward::<RestoreError>("querying missing fragments from server")?;

        let mut push_task = lore_spawn!({
            let repository = repository.clone();
            let stats = stats.clone();
            async move { push_fragments(repository, storage_protocol, missing_fragments, stats).await }
        });

        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        let result = loop {
            tokio::select! {
                _ = ticker.tick() => {
                    LoreEvent::RevisionRestoreFragmentProgress(LoreRevisionRestoreFragmentProgressEventData {
                        complete: stats.fragment_complete.load(Ordering::Relaxed) as u64,
                        count: stats.fragment_count.load(Ordering::Relaxed) as u64,
                    }).send();
                },
                result = &mut push_task => {
                    break result.internal("push task panicked")?;
                }
            }
        };
        result.forward::<RestoreError>("pushing fragments to remote")?;

        LoreEvent::RevisionRestoreFragmentEnd(LoreRevisionRestoreFragmentEndEventData {
            fragments: stats.fragment_complete.load(Ordering::Relaxed) as u64,
        })
        .send();

        let response = revision_protocol
            .branch_push(current_branch, signature, false, false)
            .await
            .forward::<RestoreError>("pushing branch head pointer")?;

        if response.fast_forward_merged {
            return Err(RestoreError::internal(format!(
                "Branch was rebased before being pushed {}",
                response.revision
            )));
        }
        if response.revision_number == 0 {
            return Err(RestoreError::internal(format!(
                "Branch was moved before being pushed {}",
                response.revision
            )));
        }

        revision = response.revision;
        revision_number = response.revision_number;
        remote_pushed = true;
    }

    crate::instance::store_current_anchor(&repository, revision)
        .await
        .forward::<RestoreError>("storing current revision anchor")?;

    let _ = crate::instance::delete_staged_anchor(&repository).await;

    // When the new revision was pushed to remote, it is on the remote history line.
    // When offline, the rehash produced a local-only commit that the remote has never
    // seen — treat it as a local commit (Divergent, leave LAST_SYNC alone).
    let status = if remote_pushed {
        BranchLatestStatus::Convergent
    } else {
        BranchLatestStatus::Divergent
    };

    branch::store_latest(repository.clone(), current_branch, revision, status)
        .await
        .forward::<RestoreError>("storing branch head")?;

    if remote_pushed {
        branch::store_last_sync(repository.clone(), current_branch, revision).await;
    }

    LoreEvent::RevisionRestoreRevision(LoreRevisionRestoreRevisionEventData {
        revision,
        revision_number,
    })
    .send();

    Ok(())
}
