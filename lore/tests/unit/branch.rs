// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore::branch::*;
use lore::interface::LoreString;
use lore_base::error::Disconnected;
use lore_base::error::InvalidArguments;
use lore_base::error::NotAuthenticated;
use lore_base::error::NotAuthorized;
use lore_base::runtime::LORE_CONTEXT;
use lore_error_set::FfiError;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::RemoteState;
use lore_revision::test_util::context_with_state;
use lore_transport::ProtocolError;

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
        let execution = Arc::new(ExecutionContext::new_client(
            LoreGlobalArgs::default(),
            EventDispatcher::no_dispatch(),
        ));
        let set = LORE_CONTEXT
            .scope(execution, async move {
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
