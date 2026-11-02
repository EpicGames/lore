// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use lore_base::types::Address;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_storage::ImmutableStore;

use crate::store::test_support::SlowDownImmutableStore;

/// Sanity check test to ensure that the `lore-server` test suites are running
/// using the server store policies, and not the library (i.e. client focused)
/// defaults, which have many retry attempts.
/// The server cannot attempt an operation over many minutes and should bail out
/// early. Without the server policies, this test would time out as the Lore library
/// retries up to 60 times by default.
#[tokio::test]
async fn slow_down_store_exhausts_retries_with_server_policy() {
    let store: Arc<dyn ImmutableStore> = Arc::new(SlowDownImmutableStore);
    // Non-zero so read_raw's debug_assert passes and the store is actually called.
    let address = Address::zero_context_hash(Hash::from([1u8; 32]));

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        lore_storage::read_raw(store, Partition::default(), address),
    )
    .await
    .expect("read_raw did not return within 10s — server retry policy may not be active");

    assert!(
        result.unwrap_err().is_slow_down(),
        "Expected SlowDown after retry exhaustion"
    );
}
