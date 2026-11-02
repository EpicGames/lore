// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::auth::userinfo::*;

#[test]
fn strip_pulls_current_user_out_and_preserves_other_order() {
    let ids = vec![
        "other-a".to_string(),
        "self-id".to_string(),
        "other-b".to_string(),
    ];
    let (has_current, remaining) = strip_current_user(ids, "self-id");
    assert!(has_current);
    assert_eq!(
        remaining,
        vec!["other-a".to_string(), "other-b".to_string()]
    );
}

#[test]
fn strip_reports_absent_current_user() {
    let ids = vec!["other-a".to_string(), "other-b".to_string()];
    let (has_current, remaining) = strip_current_user(ids, "self-id");
    assert!(!has_current);
    assert_eq!(
        remaining,
        vec!["other-a".to_string(), "other-b".to_string()]
    );
}

#[test]
fn strip_with_only_current_user_leaves_remaining_empty() {
    let ids = vec!["self-id".to_string()];
    let (has_current, remaining) = strip_current_user(ids, "self-id");
    assert!(has_current);
    assert!(
        remaining.is_empty(),
        "current-only list yields empty remaining; got {remaining:?}"
    );
}

#[tokio::test]
async fn supplied_token_describes_only_the_identity_the_call_acts_as() {
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::relay::EventDispatcher;

    /// `{"iss":"lore","sub":"alice","name":"Alice","exp":2000000000,"aud":["example.com"]}`
    const ALICE_TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJuYW1lIjoiQWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdfQ.signature";

    // `identity` is what `LoreGlobalArgs::validate` derives from the token.
    let globals = LoreGlobalArgs {
        identity: "alice".into(),
        identity_token: ALICE_TOKEN.into(),
        ..Default::default()
    };
    let execution = Arc::new(ExecutionContext::new_client(
        globals,
        EventDispatcher::no_dispatch(),
    ));

    // An empty auth URL keeps the token store out of this: a lookup there
    // fails before it reads anything, so whatever comes back can only have
    // come from the supplied token.
    let resolved = LORE_CONTEXT
        .scope(execution, async {
            resolve_local_user_info("", &["alice".to_string(), "bob".to_string()]).await
        })
        .await;

    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].id, "alice");
    assert_eq!(
        resolved[0]
            .local_user_info
            .as_ref()
            .map(|info| info.token.as_str()),
        Some(ALICE_TOKEN),
        "the identity the call acts as is described by the supplied token"
    );
    assert_eq!(
        resolved[0].refresh_expires, None,
        "a supplied token is never refreshed"
    );
    assert_eq!(resolved[1].id, "bob");
    assert!(
        resolved[1].local_user_info.is_none(),
        "another user must not be described by this call's token"
    );
}

#[test]
fn strip_removes_every_occurrence_of_current_user() {
    // The fast path emits one event for the current user; the remote
    // call must not see any self-id so it cannot emit a duplicate.
    let ids = vec![
        "self-id".to_string(),
        "other".to_string(),
        "self-id".to_string(),
    ];
    let (has_current, remaining) = strip_current_user(ids, "self-id");
    assert!(has_current);
    assert_eq!(
        remaining,
        vec!["other".to_string()],
        "every occurrence of the current user must be stripped"
    );
}

const PAST: u64 = 1;
const FUTURE: u64 = i64::MAX as u64;

fn identity(expires: u64, refresh_expires: Option<u64>) -> ResolvedIdentity {
    ResolvedIdentity {
        id: "alice".to_string(),
        name: "Alice".to_string(),
        local_user_info: Some(lore_credential::UserInfo {
            expires,
            ..Default::default()
        }),
        refresh_expires,
    }
}

#[test]
fn session_with_unexpired_token_is_valid() {
    assert!(identity(FUTURE, None).session_valid());
    assert!(identity(FUTURE, Some(PAST)).session_valid());
}

#[test]
fn session_with_expired_token_is_valid_while_refresh_token_is() {
    assert!(identity(PAST, Some(FUTURE)).session_valid());
}

#[test]
fn session_with_expired_token_and_refresh_token_without_deadline_is_valid() {
    assert!(identity(PAST, Some(0)).session_valid());
}

#[test]
fn session_with_expired_token_and_expired_refresh_token_is_expired() {
    assert!(!identity(PAST, Some(PAST)).session_valid());
}

#[test]
fn session_with_expired_token_and_no_refresh_token_is_expired() {
    assert!(!identity(PAST, None).session_valid());
}

#[test]
fn session_without_token_is_expired() {
    let resolved = ResolvedIdentity {
        local_user_info: None,
        ..identity(FUTURE, Some(FUTURE))
    };
    assert!(!resolved.session_valid());
}
