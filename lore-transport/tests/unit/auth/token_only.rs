// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::types::RepositoryId;
use lore_transport::UserService;
use lore_transport::auth::token_only::*;

/// `{"iss":"lore","sub":"alice","name":"Alice Example","preferred_username":"alice.e","exp":2000000000,"aud":["example.com"]}`
const FULL: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJuYW1lIjoiQWxpY2UgRXhhbXBsZSIsInByZWZlcnJlZF91c2VybmFtZSI6ImFsaWNlLmUiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdfQ.signature";
/// `{"iss":"lore","sub":"alice","name":"Alice Example","exp":2000000000,"aud":["example.com"]}`
const NAME_ONLY: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJuYW1lIjoiQWxpY2UgRXhhbXBsZSIsImV4cCI6MjAwMDAwMDAwMCwiYXVkIjpbImV4YW1wbGUuY29tIl19.signature";
/// `{"iss":"lore","sub":"alice","preferred_username":"alice.e","exp":2000000000,"aud":["example.com"]}`
const PREFERRED_ONLY: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJwcmVmZXJyZWRfdXNlcm5hbWUiOiJhbGljZS5lIiwiZXhwIjoyMDAwMDAwMDAwLCJhdWQiOlsiZXhhbXBsZS5jb20iXX0.signature";
/// `{"iss":"lore","sub":"alice","exp":2000000000,"aud":["example.com"]}`
const NAMELESS: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdfQ.signature";

async fn info(token: &str, ids: &[&str]) -> Vec<(String, String)> {
    let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
    TokenOnlyUserService
        .get_user_info("", token, RepositoryId::default(), &ids, "")
        .await
        .expect("the default never fails")
        .into_iter()
        .map(|user| (user.user_id, user.user_name))
        .collect()
}

async fn id(token: &str, display_name: &str) -> Option<String> {
    TokenOnlyUserService
        .get_user_id("", token, RepositoryId::default(), display_name, "")
        .await
        .expect("the default never fails")
        .map(|user| user.user_id)
}

fn pair(id: &str, name: &str) -> (String, String) {
    (id.to_string(), name.to_string())
}

#[tokio::test]
async fn bearer_is_named_from_the_token_and_others_are_echoed() {
    assert_eq!(
        info(FULL, &["bob", "alice", "carol"]).await,
        vec![
            pair("bob", "bob"),
            pair("alice", "Alice Example"),
            pair("carol", "carol"),
        ],
        "one answer per requested id, in request order"
    );
}

#[tokio::test]
async fn bearer_name_prefers_full_name_then_preferred_username_then_id() {
    assert_eq!(
        info(FULL, &["alice"]).await,
        vec![pair("alice", "Alice Example")],
        "the full name, not the login handle, is the display name"
    );
    assert_eq!(
        info(NAME_ONLY, &["alice"]).await,
        vec![pair("alice", "Alice Example")]
    );
    assert_eq!(
        info(PREFERRED_ONLY, &["alice"]).await,
        vec![pair("alice", "alice.e")]
    );
    assert_eq!(
        info(NAMELESS, &["alice"]).await,
        vec![pair("alice", "alice")]
    );
}

#[tokio::test]
async fn an_unreadable_token_names_nobody() {
    assert_eq!(
        info("not-a-jwt", &["alice", "bob"]).await,
        vec![pair("alice", "alice"), pair("bob", "bob")]
    );
    assert_eq!(id("not-a-jwt", "alice.e").await, None);
}

#[tokio::test]
async fn only_the_bearers_names_resolve_to_an_id() {
    assert_eq!(id(FULL, "alice.e").await, Some("alice".to_string()));
    assert_eq!(id(FULL, "Alice Example").await, Some("alice".to_string()));
    assert_eq!(id(FULL, "bob").await, None);
    // The bearer's ID is not one of their names.
    assert_eq!(id(FULL, "alice").await, None);
    assert_eq!(id(NAMELESS, "alice").await, None);
}
