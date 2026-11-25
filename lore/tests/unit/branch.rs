// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore::branch::*;
use lore::interface::LoreString;
use lore::revision_graph::locations::local_write_context;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::Disconnected;
use lore_base::error::InvalidArguments;
use lore_base::error::NotAuthenticated;
use lore_base::error::NotAuthorized;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchId;
use lore_error_set::FfiError;
use lore_revision::branch::BranchError;
use lore_revision::event::LoreEvent;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::RemoteState;
use lore_revision::repository::RepositoryContext;
use lore_revision::test_util::context_with_state;
use lore_transport::ProtocolError;

use crate::call::test_support::has_error_event;
use crate::call::test_support::recording_callback;
use crate::revision_graph::support::create_in_store;
use crate::revision_graph::support::hash;
use crate::revision_graph::support::new_branch;
use crate::revision_graph::support::new_repository;
use crate::revision_graph::support::store_repository_metadata;
use crate::storage::under_globals;

#[test]
fn cascade_scope_maps_each_field_combination() {
    assert!(matches!(
        CascadeScope::new(&LoreString::from(""), 0, "link", "include_links"),
        Ok(CascadeScope::OuterOnly)
    ));
    assert!(matches!(
        CascadeScope::new(&LoreString::from(""), 1, "link", "include_links"),
        Ok(CascadeScope::All)
    ));
    assert!(matches!(
        CascadeScope::new(&LoreString::from("lnk"), 0, "link", "include_links"),
        Ok(CascadeScope::Single(path)) if path == "lnk"
    ));
}

#[test]
fn cascade_scope_rejects_a_path_together_with_include_all() {
    let scope = CascadeScope::new(&LoreString::from("lnk"), 1, "link", "include_links");

    let err = scope.expect_err("a path with include_all must be refused");
    assert!(
        err.to_string().contains("link and include_links"),
        "expected the conflicting fields to be named, got: {err}"
    );
}

/// What [`archive_feature`] saw: the archive's result and every event its call sent.
struct Archived {
    result: Result<(), BranchError>,
    events: Vec<LoreEvent>,
}

/// Runs `archive_impl` of the branch named `feature`, cascading to the layer `layer` where it isn't
/// empty, in a repository with no remote, after `setup` has prepared the repository with the
/// branch's id.
async fn archive_feature<F: Future<Output = ()>>(
    layer: &str,
    setup: impl FnOnce(Arc<RepositoryContext>, BranchId) -> F,
) -> Archived {
    let (events, callback) = recording_callback();
    let execution = Arc::new(ExecutionContext::new_client(
        LoreGlobalArgs::default(),
        EventDispatcher::new(callback),
    ));
    let layer = LoreString::from(layer);
    let result = LORE_CONTEXT
        .scope(execution.clone(), async move {
            let store = in_memory_for_tests("archive").await;
            let repository_id = new_repository();
            let branch = new_branch();
            create_in_store(&store, repository_id, branch, "feature", "creator", hash(2)).await;
            let repository = local_write_context(&store, repository_id);
            setup(repository.clone(), branch).await;
            archive_impl(
                repository,
                LoreBranchArchiveArgs {
                    branch: LoreString::from("feature"),
                    layer,
                    include_layers: 0,
                    link: LoreString::from(""),
                    include_links: 0,
                },
            )
            .await
        })
        .await;
    execution.dispatcher.drain().await;
    let events = std::mem::take(&mut *events.lock().unwrap());
    Archived { result, events }
}

/// `lore branch archive` of the current branch fails with `DeleteCurrent` and sends no `Error` event.
#[tokio::test]
async fn archive_of_the_current_branch_fails_with_delete_current() {
    let archived = archive_feature("", |repository, branch| async move {
        lore_revision::instance::store_current_anchor_branch(&repository, branch)
            .await
            .expect("stores the current branch");
    })
    .await;

    let result = &archived.result;
    assert!(
        result.as_ref().is_err_and(|err| err.is_delete_current()),
        "got {result:?}"
    );
    assert!(!has_error_event(&archived.events));
}

/// `lore branch archive` of the repository's default branch fails with `DeleteDefault` and sends no
/// `Error` event.
#[tokio::test]
async fn archive_of_the_default_branch_fails_with_delete_default() {
    let archived = archive_feature("", |repository, branch| async move {
        store_repository_metadata(&repository, branch).await;
    })
    .await;

    let result = &archived.result;
    assert!(
        result.as_ref().is_err_and(|err| err.is_delete_default()),
        "got {result:?}"
    );
    assert!(!has_error_event(&archived.events));
}

/// `lore branch archive` of the default branch with a layer the repository doesn't have fails
/// with the local refusal, `DeleteDefault`, rather than with the layer's failure.
#[tokio::test]
async fn archive_refusal_takes_precedence_over_a_failed_cascade() {
    let archived = archive_feature("not-a-layer", |repository, branch| async move {
        store_repository_metadata(&repository, branch).await;
    })
    .await;

    let result = &archived.result;
    assert!(
        result.as_ref().is_err_and(|err| err.is_delete_default()),
        "got {result:?}"
    );
}

/// `lore branch metadata set --branch` fails with the remote's refusal of the branch lookup, and
/// with `InvalidArguments` where the remote cannot be reached.
#[tokio::test]
async fn metadata_set_fails_with_a_refused_branch_lookup() {
    let invalid_arguments = InvalidArguments {
        reason: String::new(),
    }
    .ffi_code();
    let mut differing = Vec::new();
    for (failure, expected) in [
        (
            ProtocolError::from(NotAuthenticated),
            NotAuthenticated.ffi_code(),
        ),
        (ProtocolError::from(NotAuthorized), NotAuthorized.ffi_code()),
        (ProtocolError::from(Disconnected), invalid_arguments),
    ] {
        let described = format!("{failure:?}");
        let set = under_globals(LoreGlobalArgs::default(), async move {
            let repository = context_with_state(RemoteState::Failed(failure)).await;
            metadata_set_impl(
                repository,
                LoreBranchMetadataSetArgs {
                    branch: LoreString::from("refused"),
                    ..Default::default()
                },
            )
            .await
        })
        .await;
        if set.as_ref().err().map(FfiError::ffi_code) != Some(expected) {
            differing.push(format!("{described}: got {set:?}"));
        }
    }
    assert!(differing.is_empty(), "{differing:#?}");
}
