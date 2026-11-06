// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The library's shutdown closing the cached connections. A binary of its own: the shutdown empties
//! the process-wide connection cache.
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Partition;
use lore_transport::auth::AuthMode;
use lore_transport::connection::*;
use lore_transport::test_util::CopyingPeer;

/// The shutdown frees each connection it closes, though the pool it pins holds it.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_frees_the_connections_it_closes() {
    LORE_CONTEXT
        .scope(Arc::new(()), async {
            let connection = Connection::detached(vec![CopyingPeer::new(0, None)]);
            connection
                .session_pool(Partition::from([0x5au8; 16]), "first")
                .await
                .expect("a pinned pool");
            add_connection(
                "lore://shutdown.test.invalid",
                "",
                false,
                AuthMode::Auto,
                connection.clone(),
            );

            drop_connections();

            assert_eq!(
                Arc::strong_count(&connection),
                1,
                "the pool it pins still holds the connection"
            );
        })
        .await;
}
