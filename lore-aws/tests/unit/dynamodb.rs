// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod batch_get_chunks;
mod cancellation_reason;
mod chunking;
mod paging;

use std::sync::Arc;
use std::time::Duration;

use lore_aws::aws::ClientCore;
use lore_aws::aws::creds::CachedCredentials;
use lore_aws::aws::creds::Credentials;
use lore_aws::aws::creds::StaticProvider;
use lore_aws::aws::dynamodb::DynamoClient;
use lore_aws::aws::http::HttpClient;
use lore_aws::clients::HttpClientSettings;
use lore_aws::dynamodb::DynamoDbImpl;
use lore_aws::dynamodb::default_unprocessed_keys_retry;
use lore_revision::util::time::RetryPolicy;

use crate::aws::test_server::StubServer;

/// The facade over a real client pointed at `server`, signing with static credentials.
fn dynamodb_for(server: &StubServer) -> DynamoDbImpl {
    dynamodb_resending_with(server, default_unprocessed_keys_retry())
}

/// [`dynamodb_for`], resending unprocessed keys with `unprocessed_keys_retry`.
fn dynamodb_resending_with(
    server: &StubServer,
    unprocessed_keys_retry: RetryPolicy,
) -> DynamoDbImpl {
    let http = HttpClient::new(&HttpClientSettings {
        pool_idle_timeout_seconds: 5,
        nodelay: true,
        max_connections: None,
        connection_acquire_timeout_millis: None,
    })
    .expect("a client builds");

    let core = Arc::new(ClientCore::new(
        http,
        Arc::new(CachedCredentials::new(Arc::new(StaticProvider::new(
            Credentials::new("AKIDTEST", "secret-for-tests", None, None),
        )))),
        Arc::from("eu-west-1"),
        server.endpoint(),
        Duration::from_secs(5),
        false,
    ));

    DynamoDbImpl::new(
        DynamoClient::new(core),
        Duration::MAX,
        Vec::new(),
        unprocessed_keys_retry,
    )
}
