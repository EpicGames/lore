// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A binary of its own: it lowers the periodic evictor's floor, which holds for the whole process.
use std::sync::Arc;
use std::time::Duration;

use lore_base::test_util::TempDir;
use lore_storage::FlushOptions;
use lore_storage::immutable_store::ImmutableStore;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use lore_storage::local::immutable_store::ImmutableStoreSettings;
use lore_storage::local::immutable_store::LocalImmutableStore;
use lore_storage::maintenance::spawn_gc;

fn eviction_options(max_capacity: usize, eviction_delay: Duration) -> ImmutableStoreCreateOptions {
    ImmutableStoreCreateOptions {
        max_capacity: Some(max_capacity),
        eviction_delay: Some(eviction_delay),
        ..ImmutableStoreCreateOptions::none()
    }
}

/// A store's running evictor enforces a limit given to the store after the evictor started.
#[tokio::test]
async fn spawn_gc_applies_new_limits_to_a_running_evictor() {
    lore_storage::test_util::lower_evictor_floor(1);
    let dir = TempDir::new("lore-storage-gc-limits-");
    let store: Arc<dyn ImmutableStore> = LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            protect_local_fragment: false,
            reuse_state_across_holds: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for i in 0u8..10 {
        let data = vec![i; 1024];
        let address = lore_storage::Address {
            hash: lore_storage::hash_slice(&data),
            context: lore_storage::Context::from([i; 16]),
        };
        let fragment = lore_storage::Fragment {
            flags: 0,
            size_payload: data.len() as u32,
            size_content: data.len() as u64,
        };
        store
            .clone()
            .put(
                lore_storage::Partition::from([0x01; 16]),
                address,
                fragment,
                Some(bytes::Bytes::from(data)),
                false,
            )
            .await
            .unwrap();
    }
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    let count_before = store.clone().fragment_count().await.unwrap();

    spawn_gc(
        &store,
        &eviction_options(1_000_000, Duration::from_millis(1)),
    );
    tokio::time::sleep(Duration::from_millis(10)).await;
    spawn_gc(&store, &eviction_options(2, Duration::from_secs(3600)));

    let mut count_after = count_before;
    for _ in 0..1000 {
        count_after = store.clone().fragment_count().await.unwrap();
        if count_after < count_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        count_after < count_before,
        "the evictor kept its first limit: before={count_before}, after={count_after}"
    );
}
