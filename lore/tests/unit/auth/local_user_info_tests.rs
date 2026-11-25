// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore::auth::LoreAuthLocalUserInfoArgs;
use lore::auth::local_user_info;
use lore::auth::stored_logins_for_remote;
use lore::interface::LoreString;
use lore_base::error::Disconnected;
use lore_base::error::NoRemote;
use lore_base::error::NotAuthenticated;
use lore_base::test_util::TempDir;
use lore_credential::token_store::store_user_token;
use lore_credential::unsigned_identity_token;
use lore_error_set::FfiError;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use parking_lot::Mutex;

use super::repository_with_config;

/// Each test names hosts under its own domain, since the tests share the process's token
/// store. Nothing listens at any of them: a call that connects fails.
async fn store_login(auth_url: &str, user_id: &str, accepted_domain: &str) {
    let token = unsigned_identity_token(user_id).expect("encoding the token");
    store_user_token(
        auth_url,
        user_id,
        &token,
        vec![format!(".{accepted_domain}")],
    )
    .await
    .expect("storing the login");
}

fn no_users() -> LoreAuthLocalUserInfoArgs {
    LoreAuthLocalUserInfoArgs {
        auth_endpoint: LoreString::default(),
        user_ids: LoreArray::default(),
        with_identity_token: 0,
        with_access_token: 0,
        with_claims: 0,
    }
}

fn local() -> LoreGlobalArgs {
    LoreGlobalArgs {
        local: 1,
        ..Default::default()
    }
}

#[derive(Default)]
struct Reported {
    status: i32,
    tokens: Vec<String>,
    infos: Vec<String>,
}

async fn auth_info(domain: &str, user_ids: &[&str], globals: LoreGlobalArgs) -> Reported {
    let args = LoreAuthLocalUserInfoArgs {
        user_ids: LoreArray::from_vec(user_ids.iter().map(|id| LoreString::from(*id)).collect()),
        ..no_users()
    };
    auth_info_in(domain, "", args, globals).await
}

/// `lore auth info` in a repository whose remote is under `domain` and whose config also holds
/// `config`.
async fn auth_info_in(
    domain: &str,
    config: &str,
    args: LoreAuthLocalUserInfoArgs,
    globals: LoreGlobalArgs,
) -> Reported {
    let root = repository_with_config(
        domain,
        Some(&format!(
            "remote_url = \"lore://lore.{domain}:41337\"\n{config}"
        )),
    );
    call(&root, args, globals).await
}

/// A repository created offline, as a client creates one locally, with its remote under
/// `domain`.
async fn created_repository(domain: &str) -> TempDir {
    let root = TempDir::new(&format!("lore-auth-{domain}-"));
    let offline = LoreGlobalArgs {
        repository_path: root.path().into(),
        offline: 1,
        ..Default::default()
    };
    let create = lore::repository::LoreRepositoryCreateArgs {
        repository_url: format!("lore://lore.{domain}:41337/project")
            .as_str()
            .into(),
        id: LoreString::default(),
        description: LoreString::default(),
        vfs: lore::repository::LoreVfsType::default(),
        use_shared_store: lore_revision::repository::LoreSharedStoreMode::Disabled,
        shared_store_path: LoreString::default(),
    };
    assert_eq!(
        lore::repository::create(offline, create, None).await,
        0,
        "creating the repository offline"
    );
    root
}

/// `lore auth info` in the repository at `root`, with `globals` validated as the C API entry
/// points validate them.
async fn call(
    root: &TempDir,
    args: LoreAuthLocalUserInfoArgs,
    globals: LoreGlobalArgs,
) -> Reported {
    let reported = Arc::new(Mutex::new(Reported::default()));
    let sink = reported.clone();
    let callback: lore_revision::interface::LoreEventCallback =
        Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::AuthUserToken(data) => sink.lock().tokens.push(data.id.to_string()),
            LoreEvent::AuthUserInfo(data) => sink.lock().infos.push(data.id.to_string()),
            LoreEvent::Complete(complete) => sink.lock().status = complete.status,
            _ => {}
        }));
    let mut globals = LoreGlobalArgs {
        repository_path: root.path().into(),
        ..globals
    };
    globals
        .validate()
        .expect("the C API entry points validate the globals");
    local_user_info(globals, args, callback).await;
    Arc::try_unwrap(reported)
        .ok()
        .expect("the call has finished with the callback")
        .into_inner()
}

#[tokio::test]
async fn local_call_without_a_login_for_the_remote_is_not_authenticated() {
    let domain = "local-info-none.test.invalid";
    store_login(
        "https://auth.local-info-elsewhere.test.invalid",
        "someone",
        "local-info-elsewhere.test.invalid",
    )
    .await;

    let reported = auth_info(domain, &[], local()).await;

    assert_eq!(reported.status, NotAuthenticated.ffi_code());
    assert!(reported.infos.is_empty());
}

#[tokio::test]
async fn offline_call_does_not_connect() {
    let domain = "offline-info-none.test.invalid";

    let globals = LoreGlobalArgs {
        offline: 1,
        ..Default::default()
    };
    let reported = auth_info(domain, &[], globals).await;

    assert_eq!(reported.status, NotAuthenticated.ffi_code());
}

#[tokio::test]
async fn local_call_reports_the_login_the_remote_accepts() {
    let domain = "local-info-login.test.invalid";
    store_login(&format!("https://auth.{domain}"), "alice", domain).await;

    let reported = auth_info(domain, &[], local()).await;

    assert_eq!(reported.status, 0);
    assert_eq!(reported.tokens, ["alice"]);
    assert_eq!(reported.infos, ["alice"]);
}

#[tokio::test]
async fn local_call_skips_a_user_whose_token_the_remote_does_not_accept() {
    let domain = "local-info-one-url.test.invalid";
    let auth_url = format!("https://auth.{domain}");
    store_login(&auth_url, "alice", "local-info-other.test.invalid").await;
    store_login(&auth_url, "bob", domain).await;

    let reported = auth_info(domain, &[], local()).await;

    assert_eq!(reported.status, 0);
    assert_eq!(reported.tokens, ["bob"]);
    assert_eq!(reported.infos, ["bob"]);
}

#[tokio::test]
async fn local_call_reports_the_current_user_at_each_matching_auth_url() {
    let domain = "local-info-each-url.test.invalid";
    store_login(&format!("https://auth-a.{domain}"), "alice", domain).await;
    store_login(&format!("https://auth-b.{domain}"), "bob", domain).await;

    let reported = auth_info(domain, &[], local()).await;

    assert_eq!(reported.status, 0);
    assert_eq!(reported.tokens, ["alice", "bob"]);
    assert_eq!(reported.infos, ["alice", "bob"]);
}

#[tokio::test]
async fn local_call_reports_the_identity_it_acts_as() {
    let domain = "local-info-identity.test.invalid";
    let auth_url = format!("https://auth.{domain}");
    store_login(&auth_url, "alice", domain).await;
    store_login(&auth_url, "bob", domain).await;

    let globals = LoreGlobalArgs {
        identity: "bob".into(),
        ..local()
    };
    let reported = auth_info(domain, &[], globals).await;

    assert_eq!(reported.status, 0);
    assert_eq!(reported.tokens, ["bob"]);
    assert_eq!(reported.infos, ["bob"]);
}

#[tokio::test]
async fn local_call_with_a_supplied_token_needs_no_stored_login() {
    let domain = "local-info-supplied.test.invalid";
    let token = unsigned_identity_token("carol").expect("encoding the token");

    let globals = LoreGlobalArgs {
        identity_token: token.as_str().into(),
        ..local()
    };
    let reported = auth_info(domain, &[], globals).await;

    assert_eq!(reported.status, 0);
    assert_eq!(reported.tokens, ["carol"]);
    assert_eq!(reported.infos, ["carol"]);
}

#[tokio::test]
async fn local_call_reports_the_identity_the_repository_records() {
    let domain = "local-info-config-identity.test.invalid";
    let auth_url = format!("https://auth.{domain}");
    store_login(&auth_url, "alice", domain).await;
    store_login(&auth_url, "bob", domain).await;

    let reported = auth_info_in(domain, "identity = \"bob\"\n", no_users(), local()).await;

    assert_eq!(reported.status, 0);
    assert_eq!(reported.tokens, ["bob"]);
    assert_eq!(reported.infos, ["bob"]);
}

#[tokio::test]
async fn a_requested_user_whose_login_the_remote_rejects_has_no_token() {
    let domain = "local-info-rejected-id.test.invalid";
    let auth_url = format!("https://auth.{domain}");
    store_login(&auth_url, "alice", "local-info-rejected-other.test.invalid").await;
    store_login(&auth_url, "bob", domain).await;

    let reported = auth_info(domain, &["alice"], local()).await;

    assert_eq!(reported.status, 0);
    assert!(reported.tokens.is_empty(), "{:?}", reported.tokens);
    assert_eq!(reported.infos, ["alice"]);
}

#[tokio::test]
async fn a_current_user_whose_login_the_remote_rejects_is_not_authenticated() {
    let domain = "local-info-rejected-identity.test.invalid";
    let auth_url = format!("https://auth.{domain}");
    store_login(
        &auth_url,
        "alice",
        "local-info-rejected-elsewhere.test.invalid",
    )
    .await;
    store_login(&auth_url, "bob", domain).await;

    let reported = auth_info_in(domain, "identity = \"alice\"\n", no_users(), local()).await;

    assert_eq!(reported.status, NotAuthenticated.ffi_code());
    assert!(reported.tokens.is_empty(), "{:?}", reported.tokens);
}

#[tokio::test]
async fn local_call_with_a_supplied_token_reads_the_store_for_other_users() {
    let domain = "local-info-supplied-other.test.invalid";
    store_login(&format!("https://auth.{domain}"), "alice", domain).await;
    let token = unsigned_identity_token("carol").expect("encoding the token");

    let globals = LoreGlobalArgs {
        identity_token: token.as_str().into(),
        ..local()
    };
    let reported = auth_info(domain, &["carol", "alice"], globals).await;

    assert_eq!(reported.status, 0);
    assert_eq!(reported.tokens, ["carol", "alice"]);
    assert_eq!(reported.infos, ["carol", "alice"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_call_for_the_access_token_is_no_remote() {
    let domain = "offline-info-access-token.test.invalid";
    store_login(&format!("https://auth.{domain}"), "alice", domain).await;
    let root = created_repository(domain).await;

    let args = LoreAuthLocalUserInfoArgs {
        with_access_token: 1,
        ..no_users()
    };
    let globals = LoreGlobalArgs {
        offline: 1,
        ..Default::default()
    };
    let reported = call(&root, args, globals).await;

    assert_eq!(reported.status, NoRemote.ffi_code());
}

#[tokio::test(flavor = "multi_thread")]
async fn local_call_for_the_access_token_resolves_the_identities_connected() {
    let domain = "local-info-access-token.test.invalid";
    store_login(&format!("https://auth.{domain}"), "alice", domain).await;
    let root = created_repository(domain).await;

    let args = LoreAuthLocalUserInfoArgs {
        with_access_token: 1,
        ..no_users()
    };
    let reported = call(&root, args, local()).await;

    assert_eq!(reported.status, Disconnected.ffi_code());
    assert!(reported.tokens.is_empty(), "{:?}", reported.tokens);
}

#[tokio::test]
async fn a_supplied_token_is_reported_once_with_several_matching_auth_urls() {
    let domain = "local-info-supplied-urls.test.invalid";
    store_login(&format!("https://auth-a.{domain}"), "alice", domain).await;
    store_login(&format!("https://auth-b.{domain}"), "bob", domain).await;
    let token = unsigned_identity_token("carol").expect("encoding the token");

    let globals = LoreGlobalArgs {
        identity_token: token.as_str().into(),
        ..local()
    };
    let reported = auth_info(domain, &["carol", "alice"], globals).await;

    assert_eq!(reported.status, 0);
    assert_eq!(reported.tokens, ["carol", "alice"]);
    assert_eq!(reported.infos, ["carol", "alice"]);
}

#[tokio::test]
async fn connected_call_reports_the_identity_the_repository_records() {
    let domain = "info-config-identity.test.invalid";
    let auth_url = format!("https://auth.{domain}");
    store_login(&auth_url, "alice", domain).await;
    store_login(&auth_url, "bob", domain).await;

    let args = LoreAuthLocalUserInfoArgs {
        auth_endpoint: auth_url.as_str().into(),
        ..no_users()
    };
    let reported = auth_info_in(
        domain,
        "identity = \"bob\"\n",
        args,
        LoreGlobalArgs::default(),
    )
    .await;

    assert_eq!(reported.status, 0);
    assert_eq!(reported.tokens, ["bob"]);
    assert_eq!(reported.infos, ["bob"]);
}

#[tokio::test]
async fn a_requested_user_is_reported_without_any_stored_login() {
    let domain = "local-info-no-logins.test.invalid";

    let reported = auth_info(domain, &["alice"], local()).await;

    assert_eq!(reported.status, 0);
    assert!(reported.tokens.is_empty(), "{:?}", reported.tokens);
    assert_eq!(reported.infos, ["alice"]);
}

#[tokio::test]
async fn a_repository_without_a_remote_reports_the_identity_it_records() {
    let domain = "info-remote-less-identity.test.invalid";
    let auth_url = format!("https://auth.{domain}");
    store_login(&auth_url, "alice", domain).await;
    store_login(&auth_url, "bob", domain).await;
    let root = repository_with_config(domain, Some("identity = \"bob\"\n"));

    let args = LoreAuthLocalUserInfoArgs {
        auth_endpoint: auth_url.as_str().into(),
        ..no_users()
    };
    let reported = call(&root, args, LoreGlobalArgs::default()).await;

    assert_eq!(reported.status, 0);
    assert_eq!(reported.tokens, ["bob"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn local_call_for_the_claims_resolves_the_identities_connected() {
    let domain = "local-info-claims.test.invalid";
    store_login(&format!("https://auth.{domain}"), "alice", domain).await;
    let root = created_repository(domain).await;

    let args = LoreAuthLocalUserInfoArgs {
        with_claims: 1,
        ..no_users()
    };
    let reported = call(&root, args, local()).await;

    assert_eq!(reported.status, Disconnected.ffi_code());
    assert!(reported.tokens.is_empty(), "{:?}", reported.tokens);
}

#[tokio::test]
async fn user_info_for_the_current_user_does_not_connect_when_local() {
    let domain = "local-user-info.test.invalid";
    store_login(&format!("https://auth.{domain}"), "alice", domain).await;
    let root = repository_with_config(
        domain,
        Some(&format!("remote_url = \"lore://lore.{domain}:41337\"\n")),
    );

    let reported = Arc::new(Mutex::new(Reported::default()));
    let sink = reported.clone();
    let callback: lore_revision::interface::LoreEventCallback =
        Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::AuthUserInfo(data) => sink.lock().infos.push(data.id.to_string()),
            LoreEvent::Complete(complete) => sink.lock().status = complete.status,
            _ => {}
        }));
    let globals = LoreGlobalArgs {
        repository_path: root.path().into(),
        ..local()
    };
    lore::auth::resolve_user_info(
        globals,
        lore::auth::LoreAuthUserInfoArgs {
            user_ids: LoreArray::default(),
        },
        callback,
    )
    .await;

    let reported = reported.lock();
    assert_eq!(reported.status, 0);
    assert_eq!(reported.infos, ["alice"]);
}

#[tokio::test]
async fn an_id_without_a_token_at_any_matching_auth_url_is_reported_once() {
    let domain = "local-info-two-urls.test.invalid";
    store_login(&format!("https://auth-a.{domain}"), "alice", domain).await;
    store_login(&format!("https://auth-b.{domain}"), "bob", domain).await;

    let reported = auth_info(domain, &["alice", "nobody"], local()).await;

    assert_eq!(reported.status, 0);
    assert_eq!(reported.tokens, ["alice"]);
    assert_eq!(reported.infos, ["alice", "nobody"]);
}

#[tokio::test]
async fn access_tokens_are_not_logins() {
    let domain = "local-info-resource.test.invalid";
    store_login(
        &format!("https://auth.{domain}/0123456789abcdef0123456789abcdef"),
        "alice",
        domain,
    )
    .await;

    let logins = stored_logins_for_remote(&format!("lore://lore.{domain}:41337"))
        .await
        .expect("the remote URL parses");

    assert!(logins.is_empty(), "{logins:?}");
}

#[tokio::test]
async fn stored_logins_are_grouped_by_auth_url_in_store_order() {
    let domain = "local-info-urls.test.invalid";
    store_login(&format!("https://auth-b.{domain}"), "alice", domain).await;
    store_login(&format!("https://auth-a.{domain}"), "alice", domain).await;
    store_login(&format!("https://auth-a.{domain}"), "bob", domain).await;

    let logins = stored_logins_for_remote(&format!("lore://lore.{domain}:41337"))
        .await
        .expect("both auth URLs have logins the remote accepts");

    assert_eq!(
        logins,
        [
            (
                format!("https://auth-b.{domain}"),
                vec!["alice".to_string()]
            ),
            (
                format!("https://auth-a.{domain}"),
                vec!["alice".to_string(), "bob".to_string()]
            ),
        ]
    );
}
