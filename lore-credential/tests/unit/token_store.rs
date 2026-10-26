// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use base64::prelude::BASE64_STANDARD;
use base64::prelude::Engine as _;
use lore_credential::IdentityToken;
use lore_credential::token_store::Refreshed;
use lore_credential::token_store::RemoteIdentity;
use lore_credential::token_store::RemovedRefreshToken;
use lore_credential::token_store::TokenMap;
use lore_credential::token_store::load_user_token;
use lore_credential::token_store::load_user_token_from_store;
use lore_credential::token_store::refresh_user_token;
use lore_credential::token_store::remove_all_tokens_for_auth_url;
use lore_credential::token_store::remove_user_token_if;
use lore_credential::token_store::remove_user_tokens_for_auth_url;
use lore_credential::token_store::store_user_token;
use lore_credential::token_store::store_user_token_and_refresh_token;
use lore_credential::token_store::test_util::encryption_key_from_stored;
use lore_credential::token_store::test_util::generate_encryption_key;
use lore_credential::token_store::test_util::is_entry_for_auth_url;
use lore_credential::token_store::test_util::open_token;
use lore_credential::token_store::test_util::seal_token;
use lore_credential::token_store::test_util::single_nonce_sequence;
use lore_credential::token_store::test_util::split_remote_resource;
use lore_credential::token_store::test_util::store_file;
use lore_credential::token_store::test_util::stored_login;
use lore_credential::token_store::tokens_only_for_recipient_domain;
use lore_credential::token_store::vulnerable_all_tokens;
use ring::aead::AES_256_GCM;
use ring::aead::NONCE_LEN;
use ring::aead::NonceSequence;

#[test]
fn refresh_token_serde_default_none() {
    // Old store format without refresh_token field
    let toml_str = r#"
user_id = "user-1"
token = "encrypted-token"
acceptable_root_domains = ["example.com"]
"#;
    let token: IdentityToken = toml::from_str(toml_str).unwrap();
    assert!(token.refresh_token().is_none());
    assert!(token.scope().is_none());
    assert_eq!(token.user_id(), "user-1");
    assert_eq!(token.token(), "encrypted-token");
}

#[test]
fn refresh_token_serde_roundtrip() {
    let token = IdentityToken::new(
        "user-1",
        "encrypted-auth",
        vec!["example.com".into()],
        Some("encrypted-refresh".into()),
        None,
    );
    let serialized = toml::to_string_pretty(&token).unwrap();
    let deserialized: IdentityToken = toml::from_str(&serialized).unwrap();
    assert_eq!(deserialized.refresh_token(), Some("encrypted-refresh"));
    assert_eq!(deserialized.user_id(), "user-1");
}

/// An entry the new client writes, read by a client that predates the scope field, loses only
/// that field.
#[test]
fn an_older_client_reads_an_entry_with_a_scope() {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct OlderIdentityToken {
        user_id: String,
        token: String,
        #[serde(default)]
        acceptable_root_domains: Vec<String>,
        #[serde(default)]
        refresh_token: Option<String>,
    }
    let token = IdentityToken::new(
        "user-1",
        "encrypted-auth",
        vec!["example.com".into()],
        Some("encrypted-refresh".into()),
        Some("openid offline_access".into()),
    );
    let serialized = toml::to_string_pretty(&token).unwrap();

    let older: OlderIdentityToken = toml::from_str(&serialized).unwrap();
    assert_eq!(older.token, "encrypted-auth");
    assert_eq!(older.refresh_token.as_deref(), Some("encrypted-refresh"));

    let newer: IdentityToken = toml::from_str(&serialized).unwrap();
    assert_eq!(newer.scope(), Some("openid offline_access"));
}

#[test]
fn identity_token_without_refresh_backward_compat() {
    // Simulates an old store file structure
    let toml_str = r#"
[[remotes]]
remote = "https://auth.example.com"

[[remotes.token]]
user_id = "alice"
token = "tok-a"
acceptable_root_domains = ["example.com"]

[[remotes.token]]
user_id = "bob"
token = "tok-b"
"#;
    let map: TokenMap = toml::from_str(toml_str).unwrap();
    assert_eq!(map.remotes().len(), 1);
    assert_eq!(map.remotes()[0].tokens().len(), 2);
    assert!(map.remotes()[0].tokens()[0].refresh_token().is_none());
    assert!(map.remotes()[0].tokens()[1].refresh_token().is_none());
}

#[test]
fn token_map_with_refresh_token_roundtrip() {
    let map = TokenMap::new(vec![RemoteIdentity::new(
        "https://auth.example.com",
        vec![IdentityToken::new(
            "alice",
            "auth-tok",
            vec!["example.com".into()],
            Some("refresh-tok".into()),
            None,
        )],
    )]);
    let serialized = toml::to_string_pretty(&map).unwrap();
    let deserialized: TokenMap = toml::from_str(&serialized).unwrap();
    assert_eq!(
        deserialized.remotes()[0].tokens()[0].refresh_token(),
        Some("refresh-tok")
    );
}

#[test]
fn encryption_key_from_stored_accepts_a_bare_key() {
    let key = generate_encryption_key().unwrap();
    assert_eq!(key.len(), AES_256_GCM.key_len());
    assert_eq!(encryption_key_from_stored(&key).as_ref(), Some(&key));
}

#[test]
fn encryption_key_from_stored_rejects_malformed() {
    assert!(encryption_key_from_stored(&[]).is_none());
    assert!(encryption_key_from_stored(&[0u8; 16]).is_none());
    // The layout earlier versions wrote: a nonce counter ahead of the key.
    assert!(encryption_key_from_stored(&[0u8; 4 + 32]).is_none());
}

#[test]
fn seal_and_open_token_round_trip() {
    let key = generate_encryption_key().unwrap();
    let sealed = seal_token(&key, "a-user-token").unwrap();
    assert_eq!(open_token(&key, &sealed).unwrap(), "a-user-token");
}

#[test]
fn seal_token_prefixes_the_nonce() {
    let key = generate_encryption_key().unwrap();
    let blob = BASE64_STANDARD
        .decode(seal_token(&key, "a-user-token").unwrap())
        .unwrap();
    assert_eq!(blob.len(), NONCE_LEN + "a-user-token".len() + 16);
}

#[test]
fn seal_token_draws_a_fresh_nonce_each_time() {
    let key = generate_encryption_key().unwrap();
    let first = seal_token(&key, "a-user-token").unwrap();
    let second = seal_token(&key, "a-user-token").unwrap();
    assert_ne!(first, second);
}

#[test]
fn open_token_rejects_short_blobs() {
    let key = generate_encryption_key().unwrap();
    let short = BASE64_STANDARD.encode([0u8; NONCE_LEN - 1]);
    assert!(open_token(&key, &short).is_err());
}

#[test]
fn open_token_rejects_a_foreign_key() {
    let sealed = seal_token(&generate_encryption_key().unwrap(), "a-user-token").unwrap();
    assert!(open_token(&generate_encryption_key().unwrap(), &sealed).is_err());
}

#[test]
fn single_nonce_sequence_refuses_second_advance() {
    let mut sequence = single_nonce_sequence([0u8; NONCE_LEN]);
    assert!(sequence.advance().is_ok());
    assert!(sequence.advance().is_err());
}

#[test]
fn split_remote_resource_new_format() {
    let (auth, resource) =
        split_remote_resource("https://auth.example.com/00112233445566778899aabbccddeeff");
    assert_eq!(auth, "https://auth.example.com");
    assert_eq!(resource, "00112233445566778899aabbccddeeff");
}

#[test]
fn split_remote_resource_legacy_format() {
    let (auth, resource) =
        split_remote_resource("https://auth.example.com/urc-00112233445566778899aabbccddeeff");
    assert_eq!(auth, "https://auth.example.com");
    assert_eq!(resource, "urc-00112233445566778899aabbccddeeff");
}

#[test]
fn split_remote_resource_no_resource() {
    let (auth, resource) = split_remote_resource("https://auth.example.com");
    assert_eq!(auth, "https://auth.example.com");
    assert!(resource.is_empty());
}

#[test]
fn split_remote_resource_scheme_with_hostname() {
    let (auth, resource) =
        split_remote_resource("ucs-auth://auth.example.com/aabbccdd00112233aabbccdd00112233");
    assert_eq!(auth, "ucs-auth://auth.example.com");
    assert_eq!(resource, "aabbccdd00112233aabbccdd00112233");
}

#[test]
fn is_entry_for_auth_url_base() {
    assert!(is_entry_for_auth_url(
        "https://auth.example.com",
        "https://auth.example.com"
    ));
}

#[test]
fn is_entry_for_auth_url_new_format() {
    assert!(is_entry_for_auth_url(
        "https://auth.example.com/00112233445566778899aabbccddeeff",
        "https://auth.example.com"
    ));
}

#[test]
fn is_entry_for_auth_url_legacy_format() {
    assert!(is_entry_for_auth_url(
        "https://auth.example.com/urc-00112233445566778899aabbccddeeff",
        "https://auth.example.com"
    ));
}

#[test]
fn is_entry_for_auth_url_different_host() {
    assert!(!is_entry_for_auth_url(
        "https://other.example.com/00112233445566778899aabbccddeeff",
        "https://auth.example.com"
    ));
}

#[test]
fn is_entry_for_auth_url_non_hex_suffix() {
    assert!(!is_entry_for_auth_url(
        "https://auth.example.com/not-a-resource",
        "https://auth.example.com"
    ));
}

/// A supplied token is passed through untouched, so it need not be a JWT.
const SUPPLIED_TOKEN: &str = "supplied-authentication-token";

#[tokio::test]
async fn supplied_identity_token_is_used_without_the_store() {
    // No auth endpoint and no store entry: the supplied token is returned on
    // its own, where a store read would report TokenNotFound.
    let token = load_user_token(
        "",
        "alice",
        tokens_only_for_recipient_domain("nowhere.example".to_string()),
        SUPPLIED_TOKEN,
        "",
    )
    .await
    .expect("the supplied token is used as given");
    assert_eq!(token, SUPPLIED_TOKEN);

    // An access token alongside it changes nothing: the identity token is
    // still the authentication token to use.
    let token = load_user_token(
        "",
        "alice",
        tokens_only_for_recipient_domain("nowhere.example".to_string()),
        SUPPLIED_TOKEN,
        "supplied-access-token",
    )
    .await
    .expect("the supplied token is used as given");
    assert_eq!(token, SUPPLIED_TOKEN);
}

#[tokio::test]
async fn no_supplied_token_reads_the_store() {
    // Nothing supplied, so this is a plain store read, which has no entry
    // for an empty endpoint.
    let result = load_user_token(
        "",
        "alice",
        tokens_only_for_recipient_domain("nowhere.example".to_string()),
        "",
        "",
    )
    .await;
    assert!(result.is_err());
}

/// A token removed only where it is a given one is compared with the store as its file holds it,
/// so a token another process stored in its place after this process cached the store is left
/// alone, and the given one is removed. The test stands in for the other process by writing the
/// store's file back as it was before this process stored the given token.
#[tokio::test]
async fn a_conditional_removal_leaves_a_token_another_process_stored() {
    let _alone = STORE_FILE.write().await;
    const ENDPOINT: &str = "https://auth.conditional-removal.test.invalid/repository";
    let file = store_file().expect("the token store's file");
    store_user_token(ENDPOINT, "alice", "newer", Vec::new())
        .await
        .expect("store the newer token");
    let newer = std::fs::read(&file).expect("read the store holding the newer token");
    store_user_token(ENDPOINT, "alice", "refused", Vec::new())
        .await
        .expect("store the refused token");
    std::fs::write(&file, newer).expect("store the newer token from another process");

    remove_user_token_if(ENDPOINT, "alice", "refused")
        .await
        .expect("remove the refused token");
    let kept = load_user_token_from_store(ENDPOINT, "alice", vulnerable_all_tokens()).await;
    assert_eq!(kept.ok().as_deref(), Some("newer"));

    remove_user_token_if(ENDPOINT, "alice", "newer")
        .await
        .expect("remove the newer token");
    let removed = load_user_token_from_store(ENDPOINT, "alice", vulnerable_all_tokens()).await;
    assert!(removed.is_err(), "the given token was left in the store");
}

/// Held exclusively by a test that rewrites the store's file directly, which would undo what a
/// test writing through the store meanwhile stored, and shared by those.
static STORE_FILE: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// An unsigned JWT naming `subject`, expiring at `exp`, told apart from others by `serial`.
fn jwt(subject: &str, exp: u64, serial: u32) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let claims = format!(
        r#"{{"iss":"https://issuer.test.invalid","sub":"{subject}","aud":["lore.test.invalid"],"exp":{exp},"serial":{serial}}}"#
    );
    format!(
        "{}.{}.signature",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(claims)
    )
}

const EXPIRED: u64 = 1_000_000_000;
const VALID: u64 = 2_000_000_000;

fn refused() -> std::io::Error {
    std::io::Error::other("as scripted")
}

/// Stores `token` for alice at `endpoint`, with `refresh_token` and `scope`.
async fn logged_in(endpoint: &str, token: &str, refresh_token: Option<&str>, scope: Option<&str>) {
    store_user_token_and_refresh_token(endpoint, "alice", token, Vec::new(), refresh_token, scope)
        .await
        .expect("store the login");
}

/// A grant that answers `token` with `refresh_token`, recording the refresh token presented.
fn grant(
    token: String,
    refresh_token: Option<&'static str>,
    presented: &std::sync::Mutex<Vec<String>>,
) -> impl FnOnce(String) -> std::future::Ready<Refreshed<std::io::Error>> {
    move |refresh| {
        presented.lock().unwrap().push(refresh);
        std::future::ready(Refreshed::Token {
            token,
            refresh_token: refresh_token.map(str::to_owned),
            scope: None,
        })
    }
}

/// The grant receives the stored refresh token, and the store then holds the new token and the
/// rotated refresh token in place of the presented one. The scope stays, since the answer names
/// none.
#[tokio::test]
async fn a_refresh_stores_the_new_token_and_the_rotated_refresh_token() {
    let _shared = STORE_FILE.read().await;
    const ENDPOINT: &str = "oidc+https://refresh-rotates.test.invalid";
    let expired = jwt("alice", EXPIRED, 1);
    let fresh = jwt("alice", VALID, 2);
    logged_in(
        ENDPOINT,
        &expired,
        Some("refresh-1"),
        Some("openid offline_access"),
    )
    .await;
    let presented = std::sync::Mutex::new(Vec::new());

    let token = refresh_user_token(
        ENDPOINT,
        "alice",
        &expired,
        grant(fresh.clone(), Some("refresh-2"), &presented),
    )
    .await
    .expect("the refresh succeeds");

    assert_eq!(token, fresh);
    assert_eq!(*presented.lock().unwrap(), ["refresh-1"]);
    let stored = load_user_token_from_store(ENDPOINT, "alice", vulnerable_all_tokens()).await;
    assert_eq!(stored.ok(), Some(fresh));
    assert_eq!(
        stored_login(ENDPOINT, "alice").await,
        Some((
            Some("refresh-2".to_owned()),
            Some("openid offline_access".to_owned())
        ))
    );
}

/// RFC 6749 §6: an issuer that rotates no refresh token leaves the presented one valid, so the
/// store keeps it.
#[tokio::test]
async fn a_refresh_without_a_rotated_refresh_token_keeps_the_presented_one() {
    let _shared = STORE_FILE.read().await;
    const ENDPOINT: &str = "oidc+https://refresh-keeps.test.invalid";
    let expired = jwt("alice", EXPIRED, 1);
    let fresh = jwt("alice", VALID, 2);
    logged_in(ENDPOINT, &expired, Some("refresh-1"), None).await;
    let presented = std::sync::Mutex::new(Vec::new());

    refresh_user_token(ENDPOINT, "alice", &expired, grant(fresh, None, &presented))
        .await
        .expect("the refresh succeeds");

    assert_eq!(
        stored_login(ENDPOINT, "alice").await,
        Some((Some("refresh-1".to_owned()), None))
    );
}

/// A stored token other than the expired one was stored by another caller, which presented the
/// refresh token already. It is returned, and the refresh token is not presented again.
#[tokio::test]
async fn a_token_replaced_already_is_returned_without_a_grant() {
    let _shared = STORE_FILE.read().await;
    const ENDPOINT: &str = "oidc+https://refresh-replaced.test.invalid";
    let expired = jwt("alice", EXPIRED, 1);
    let replacement = jwt("alice", VALID, 2);
    logged_in(ENDPOINT, &replacement, Some("refresh-2"), None).await;
    let presented = std::sync::Mutex::new(Vec::new());

    let token = refresh_user_token(
        ENDPOINT,
        "alice",
        &expired,
        grant(jwt("alice", VALID, 3), Some("refresh-3"), &presented),
    )
    .await
    .expect("the replacement is returned");

    assert_eq!(token, replacement);
    assert!(presented.lock().unwrap().is_empty());
}

/// Without a refresh token nothing is presented, and the caller learns there is no token.
#[tokio::test]
async fn without_a_refresh_token_nothing_is_presented() {
    let _shared = STORE_FILE.read().await;
    const ENDPOINT: &str = "oidc+https://refresh-none.test.invalid";
    let expired = jwt("alice", EXPIRED, 1);
    logged_in(ENDPOINT, &expired, None, None).await;
    let presented = std::sync::Mutex::new(Vec::new());

    let result = refresh_user_token(
        ENDPOINT,
        "alice",
        &expired,
        grant(jwt("alice", VALID, 2), None, &presented),
    )
    .await;

    assert!(result.is_err_and(|err| err.is_token_not_found()));
    assert!(presented.lock().unwrap().is_empty());
}

/// A refused refresh token is dropped, so it is not presented again; the expired token stays
/// for the caller to replace with a new login.
#[tokio::test]
async fn a_refused_refresh_token_is_dropped() {
    let _shared = STORE_FILE.read().await;
    const ENDPOINT: &str = "oidc+https://refresh-refused.test.invalid";
    let expired = jwt("alice", EXPIRED, 1);
    logged_in(ENDPOINT, &expired, Some("refresh-1"), None).await;

    let result = refresh_user_token(ENDPOINT, "alice", &expired, |_| {
        std::future::ready(Refreshed::Refused(refused()))
    })
    .await;

    assert!(result.is_err_and(|err| err.is_internal()));
    assert_eq!(stored_login(ENDPOINT, "alice").await, Some((None, None)));
    let stored = load_user_token_from_store(ENDPOINT, "alice", vulnerable_all_tokens()).await;
    assert_eq!(stored.ok(), Some(expired));
}

/// A grant that was not sent, such as one to an unreachable issuer, keeps the refresh token for
/// the next attempt.
#[tokio::test]
async fn an_unsent_refresh_keeps_the_refresh_token() {
    let _shared = STORE_FILE.read().await;
    const ENDPOINT: &str = "oidc+https://refresh-unsent.test.invalid";
    let expired = jwt("alice", EXPIRED, 1);
    logged_in(ENDPOINT, &expired, Some("refresh-1"), None).await;

    let result = refresh_user_token(ENDPOINT, "alice", &expired, |_| {
        std::future::ready(Refreshed::Unsent(refused()))
    })
    .await;

    assert!(result.is_err_and(|err| err.is_internal()));
    assert_eq!(
        stored_login(ENDPOINT, "alice").await,
        Some((Some("refresh-1".to_owned()), None))
    );
}

/// A grant that failed after it may have reached the issuer, such as one whose answer was lost,
/// drops the refresh token: the issuer may have consumed it, and presenting it again would be a
/// replay.
#[tokio::test]
async fn a_failed_refresh_drops_the_refresh_token() {
    let _shared = STORE_FILE.read().await;
    const ENDPOINT: &str = "oidc+https://refresh-failed.test.invalid";
    let expired = jwt("alice", EXPIRED, 1);
    logged_in(ENDPOINT, &expired, Some("refresh-1"), None).await;

    let result = refresh_user_token(ENDPOINT, "alice", &expired, |_| {
        std::future::ready(Refreshed::Failed(refused()))
    })
    .await;

    assert!(result.is_err_and(|err| err.is_internal()));
    assert_eq!(stored_login(ENDPOINT, "alice").await, Some((None, None)));
}

/// The refresh token is removed from the store's file before the grant is sent, so a process
/// that dies during the grant leaves nothing to present again.
#[tokio::test]
async fn the_refresh_token_leaves_the_file_before_the_grant() {
    let _shared = STORE_FILE.read().await;
    const ENDPOINT: &str = "oidc+https://refresh-removed-first.test.invalid";
    let expired = jwt("alice", EXPIRED, 1);
    logged_in(ENDPOINT, &expired, Some("refresh-1"), None).await;
    let on_disk = || {
        let file = std::fs::read_to_string(store_file().expect("the store's file"))
            .expect("read the store");
        let map: TokenMap = toml::from_str(&file).expect("parse the store");
        map.remotes()
            .iter()
            .find(|remote| remote.remote() == ENDPOINT)
            .and_then(|remote| remote.tokens().first())
            .map(|token| token.refresh_token().is_some())
    };

    let during = std::sync::Mutex::new(None);
    refresh_user_token(ENDPOINT, "alice", &expired, |_| {
        *during.lock().unwrap() = on_disk();
        std::future::ready(Refreshed::Unsent(refused()))
    })
    .await
    .expect_err("the grant was not sent");

    assert_eq!(*during.lock().unwrap(), Some(false));
    assert_eq!(on_disk(), Some(true), "an unsent grant puts it back");
}

/// A grant cancelled while it waits for the answer leaves the refresh token removed, since the
/// issuer may have received it.
#[tokio::test]
async fn a_cancelled_refresh_leaves_the_refresh_token_removed() {
    let _shared = STORE_FILE.read().await;
    const ENDPOINT: &str = "oidc+https://refresh-cancelled.test.invalid";
    let expired = jwt("alice", EXPIRED, 1);
    logged_in(ENDPOINT, &expired, Some("refresh-1"), None).await;

    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        refresh_user_token(ENDPOINT, "alice", &expired, |_| {
            std::future::pending::<Refreshed<std::io::Error>>()
        }),
    )
    .await;

    assert!(cancelled.is_err(), "the grant never answers");
    assert_eq!(stored_login(ENDPOINT, "alice").await, Some((None, None)));
    let stored = load_user_token_from_store(ENDPOINT, "alice", vulnerable_all_tokens()).await;
    assert_eq!(stored.ok(), Some(expired));
}

/// A token naming someone else is not stored under alice. The grant has consumed the refresh
/// token, so it stays removed rather than presented again.
#[tokio::test]
async fn an_answer_naming_someone_else_is_refused() {
    let _shared = STORE_FILE.read().await;
    const ENDPOINT: &str = "oidc+https://refresh-other-user.test.invalid";
    let expired = jwt("alice", EXPIRED, 1);
    logged_in(ENDPOINT, &expired, Some("refresh-1"), None).await;
    let presented = std::sync::Mutex::new(Vec::new());

    let result = refresh_user_token(
        ENDPOINT,
        "alice",
        &expired,
        grant(jwt("mallory", VALID, 2), Some("refresh-2"), &presented),
    )
    .await;

    assert!(result.is_err_and(|err| err.is_internal()));
    assert_eq!(stored_login(ENDPOINT, "alice").await, Some((None, None)));
    let stored = load_user_token_from_store(ENDPOINT, "alice", vulnerable_all_tokens()).await;
    assert_eq!(stored.ok(), Some(expired));
}

/// Two callers that find the same expired token present the refresh token once between them,
/// and both get the new token.
#[tokio::test]
async fn concurrent_refreshes_present_the_refresh_token_once() {
    let _shared = STORE_FILE.read().await;
    const ENDPOINT: &str = "oidc+https://refresh-concurrent.test.invalid";
    let expired = jwt("alice", EXPIRED, 1);
    let fresh = jwt("alice", VALID, 2);
    logged_in(ENDPOINT, &expired, Some("refresh-1"), None).await;
    let presented = std::sync::Mutex::new(Vec::new());
    let refresh = || async {
        refresh_user_token(ENDPOINT, "alice", &expired, |refresh| async {
            presented.lock().unwrap().push(refresh);
            tokio::task::yield_now().await;
            Refreshed::<std::io::Error>::Token {
                token: fresh.clone(),
                refresh_token: Some("refresh-2".to_owned()),
                scope: None,
            }
        })
        .await
    };

    let (first, second) = tokio::join!(refresh(), refresh());

    assert_eq!(first.ok().as_ref(), Some(&fresh));
    assert_eq!(second.ok().as_ref(), Some(&fresh));
    assert_eq!(*presented.lock().unwrap(), ["refresh-1"]);
}

/// Storing a token with a refresh token replaces the entry's refresh token and scope, also with
/// none, while storing a token alone keeps them.
#[tokio::test]
async fn storing_a_refresh_token_replaces_it_and_storing_a_token_alone_keeps_it() {
    let _shared = STORE_FILE.read().await;
    const ENDPOINT: &str = "oidc+https://login-replaces.test.invalid";
    logged_in(
        ENDPOINT,
        &jwt("alice", VALID, 1),
        Some("refresh-1"),
        Some("openid"),
    )
    .await;

    store_user_token(ENDPOINT, "alice", &jwt("alice", VALID, 2), Vec::new())
        .await
        .expect("store the token");
    assert_eq!(
        stored_login(ENDPOINT, "alice").await,
        Some((Some("refresh-1".to_owned()), Some("openid".to_owned())))
    );

    logged_in(ENDPOINT, &jwt("alice", VALID, 3), None, None).await;
    assert_eq!(stored_login(ENDPOINT, "alice").await, Some((None, None)));
}

fn removed_refresh_tokens(removed: Vec<RemovedRefreshToken>) -> Vec<(String, String)> {
    let mut removed: Vec<_> = removed
        .into_iter()
        .map(|removed| (removed.user_id, removed.refresh_token))
        .collect();
    removed.sort();
    removed
}

/// Removing a user's tokens returns their refresh token, for the caller to revoke. Their
/// authorization tokens are removed but not returned, and other users' tokens are kept.
#[tokio::test]
async fn removing_a_users_tokens_returns_their_refresh_token() {
    let _shared = STORE_FILE.read().await;
    const ENDPOINT: &str = "oidc+https://remove-user.test.invalid";
    let resource = format!("{ENDPOINT}/00112233445566778899aabbccddeeff");
    logged_in(ENDPOINT, "alice-token", Some("refresh-1"), None).await;
    store_user_token(&resource, "alice", "alice-partition-token", Vec::new())
        .await
        .expect("store the authorization token");
    store_user_token(ENDPOINT, "bob", "bob-token", Vec::new())
        .await
        .expect("store bob's token");

    let removed = remove_user_tokens_for_auth_url(ENDPOINT, "alice")
        .await
        .expect("remove alice's tokens");

    assert_eq!(
        removed_refresh_tokens(removed),
        [("alice".to_owned(), "refresh-1".to_owned())]
    );
    let partition = load_user_token_from_store(&resource, "alice", vulnerable_all_tokens()).await;
    assert!(partition.is_err(), "the authorization token was kept");
    let bob = load_user_token_from_store(ENDPOINT, "bob", vulnerable_all_tokens()).await;
    assert_eq!(bob.ok().as_deref(), Some("bob-token"));
}

/// Removing every token for an endpoint returns every refresh token there. A login without one
/// is removed and returns nothing, and so does a second removal.
#[tokio::test]
async fn removing_all_tokens_returns_every_refresh_token() {
    let _shared = STORE_FILE.read().await;
    const ENDPOINT: &str = "oidc+https://remove-all.test.invalid";
    logged_in(ENDPOINT, "alice-token", Some("refresh-1"), None).await;
    store_user_token(ENDPOINT, "bob", "bob-token", Vec::new())
        .await
        .expect("store bob's token");

    let removed = remove_all_tokens_for_auth_url(ENDPOINT)
        .await
        .expect("remove every token");

    assert_eq!(
        removed_refresh_tokens(removed),
        [("alice".to_owned(), "refresh-1".to_owned())]
    );
    let bob = load_user_token_from_store(ENDPOINT, "bob", vulnerable_all_tokens()).await;
    assert!(bob.is_err(), "bob's token was kept");
    let again = remove_all_tokens_for_auth_url(ENDPOINT)
        .await
        .expect("remove every token again");
    assert!(again.is_empty());
}
