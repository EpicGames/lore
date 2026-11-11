// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The `session_start` fan-outs that remote reads through one storage handle begin, by the
//! correlation ids of the calls, and the session pools its remote endpoint resolves.

use std::sync::Arc;

use lore_base::types::Context;

use super::support::*;
use crate::common::net_common::put_content;

/// Each call under a fresh correlation id starts a session pool of its own.
#[tokio::test]
async fn fresh_correlation_ids_start_a_session_per_call() {
    let starts = session_starts_after_each_remote_get("").await;
    println!("fan-outs after each of three remote gets, fresh correlation ids: {starts:?}");

    assert_eq!(starts, [1, 2, 3]);
}

/// Calls under one correlation id share the session pool the first call starts.
#[tokio::test]
async fn a_stable_correlation_id_starts_one_session() {
    let starts = session_starts_after_each_remote_get("stable").await;
    println!("fan-outs after each of three remote gets, one correlation id: {starts:?}");

    assert_eq!(starts, [1, 1, 1]);
}

/// Two pool resolutions through the storage handle's remote endpoint under one correlation id
/// answer the pool the endpoint holds, which the first caller dropped before the second asked, and
/// begin one `session_start` fan-out.
#[tokio::test]
async fn endpoint_session_pool_is_reused_under_one_correlation_id() {
    let f = fixture().await;
    let endpoint = lore::storage::handle::lookup(f.store)
        .and_then(|store| store.remote.clone())
        .expect("the fixture's handle has a remote");

    let (first, second, starts) = in_client(async {
        let connection = endpoint
            .connection(f.repository)
            .await
            .expect("connects to the server");
        let before = connection.storage_session_starts();
        let first = endpoint
            .session_pool(&f.repository, "stable")
            .await
            .expect("starts a session pool");
        let kept = Arc::downgrade(&first);
        drop(first);
        let second = endpoint
            .session_pool(&f.repository, "stable")
            .await
            .expect("resolves the session pool");
        (kept, second, connection.storage_session_starts() - before)
    })
    .await;

    assert!(
        first
            .upgrade()
            .is_some_and(|first| Arc::ptr_eq(&first, &second)),
        "the second resolution must answer the pool the first started"
    );
    assert_eq!(starts, 1);
    f.close().await;
}

/// Stores content at the fixture's remote, then reads it there three times through the fixture's
/// storage handle, each call under `correlation_id`, or under a fresh one when it is empty. Returns
/// the `session_start` fan-outs the handle's connection has begun since the content was stored,
/// after each read.
///
/// Each run stores content of its own: a remote read joins a fetch of the same content that another
/// read in the process has in flight, under another partition too, and then starts no session.
async fn session_starts_after_each_remote_get(correlation_id: &str) -> [usize; 3] {
    let f = fixture().await;
    let content = uuid::Uuid::now_v7();
    let address = put_content(
        f.store,
        f.repository,
        Context::from(content),
        content.as_bytes(),
        1,
        0,
    )
    .await;
    let connection = connection_for(&f.url, f.repository).await;
    let before = connection.storage_session_starts();

    let mut starts = [0; 3];
    for after in &mut starts {
        remote_get(&f, address, correlation_id).await;
        *after = connection.storage_session_starts() - before;
    }
    f.close().await;
    starts
}
