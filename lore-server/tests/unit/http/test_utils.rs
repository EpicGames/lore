// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Shared helpers for the HTTP module's tests.

use std::sync::Arc;

use axum_test::TestServer;
use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_revision::immutable;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_server::auth::jwt::TokenVerification;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::http::security_headers::ContentTypeAllowlist;
use lore_server::http::security_headers::ContentTypePolicy;
use lore_server::http::server::LoreHttpServerSettings;
use lore_server::http::server::PresignConfig;
use lore_server::http::server::ServerHealth;
use lore_server::http::server::ServerState;
use lore_server::http::server::create_router;
use lore_storage::Fragment;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::WriteOptions;
use lore_storage::compress::CompressionMode;
use lore_storage::compress::compress;
use lore_storage::hash_slice;
use rand::random;

/// An address no test stores.
pub(crate) const MISSING_ADDRESS: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff";

/// The state of a server over `immutable_store` and `mutable_store` that verifies no token and
/// authorizes every repository.
pub(crate) fn test_state(
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    presign_config: Option<PresignConfig>,
) -> ServerState {
    ServerState {
        immutable_store,
        mutable_store,
        token_verification: TokenVerification::Ignore,
        repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
        max_file_size: 100,
        presign_config,
    }
}

/// A server with the [`test_state`] over `immutable_store` and `mutable_store`.
pub(crate) fn build_test_server(
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    presign_config: Option<PresignConfig>,
) -> TestServer {
    let test_health = ServerHealth::new_without_availability(immutable_store.clone());
    let state = test_state(immutable_store, mutable_store, presign_config);
    let settings = LoreHttpServerSettings::test_default();
    TestServer::new(create_router(state, test_health, &settings)).unwrap()
}

/// Size of the content `write_fragmented_content` writes.
pub(crate) const RANGED_CONTENT_SIZE: usize = 16 * 1024;

/// Writes `RANGED_CONTENT_SIZE` random bytes to `repository` in 4 KiB fragments, so a range
/// crosses fragment boundaries, returning the content's address and the content.
pub(crate) async fn write_fragmented_content(
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    repository: RepositoryId,
) -> (Address, Bytes) {
    let context = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository,
    ));
    let mut content = vec![0; RANGED_CONTENT_SIZE];
    rand::fill(&mut content[..]);
    let content = Bytes::from(content);
    let address = immutable::write(
        context,
        random::<Context>(),
        content.clone(),
        WriteOptions::default().with_fixed_size_chunk(4096),
    )
    .await
    .expect("Failed to write content");
    (address, content)
}

/// Puts 64 KiB of compressible content into `repository` as one LZ4-compressed fragment,
/// returning its address.
pub(crate) async fn put_compressed_content(
    immutable_store: Arc<dyn ImmutableStore>,
    repository: RepositoryId,
) -> Address {
    let content: Vec<u8> = (0..64 * 1024).map(|index| (index / 64) as u8).collect();
    let plain = Fragment {
        flags: 0,
        size_payload: content.len() as u32,
        size_content: content.len() as u64,
    };
    let (fragment, payload) =
        compress(plain, &content, CompressionMode::Lz4).expect("Failed to compress content");
    let address = Address {
        hash: hash_slice(&content),
        context: random::<Context>(),
    };
    immutable_store
        .put(repository, address, fragment, Some(payload), false)
        .await
        .expect("Failed to put content");
    address
}

pub(crate) fn content_type_policy(extra: &[&str], denied: &[&str]) -> ContentTypePolicy {
    ContentTypePolicy {
        extra: extra.iter().map(|t| (*t).to_string()).collect(),
        denied: denied.iter().map(|t| (*t).to_string()).collect(),
    }
}

pub(crate) fn presign_config() -> PresignConfig {
    presign_config_with_policy(ContentTypePolicy::default())
}

pub(crate) fn presign_config_with_policy(policy: ContentTypePolicy) -> PresignConfig {
    let key_bytes = [0u8; 32];
    PresignConfig {
        hmac_key: ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &key_bytes),
        key_id: "test_key_id_1234".to_string(),
        min_ttl_seconds: 1,
        default_ttl_seconds: 3600,
        max_ttl_seconds: 86400,
        content_type_allowlist: ContentTypeAllowlist::try_from_policy(&policy)
            .expect("test policy should resolve"),
    }
}
