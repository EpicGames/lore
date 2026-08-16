// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use lore_aws::store::fragment_catalog::BeginObliteration;
use lore_aws::store::fragment_catalog::FragmentCatalog;
use lore_aws::store::fragment_catalog::ReleaseAssociation;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_postgres::PostgresFragmentCatalog;
use tokio::sync::Barrier;

#[tokio::test]
async fn postgres_catalog_satisfies_fragment_catalog_contract() {
    let catalog = PostgresFragmentCatalog::connect_for_test()
        .await
        .expect("failed to create isolated PostgreSQL catalog");

    lore_aws::store::fragment_catalog::contract::run(&catalog).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn obliteration_serializes_against_concurrent_associations() {
    let catalog = Arc::new(
        PostgresFragmentCatalog::connect_for_test()
            .await
            .expect("failed to create isolated PostgreSQL catalog"),
    );
    let repository = Context::from([0x61; 16]);
    let address = Address {
        hash: Hash::from([0x62; 32]),
        context: Context::from([0x63; 16]),
    };
    let fragment = Fragment {
        flags: 0,
        size_payload: 64,
        size_content: 64,
    };
    catalog
        .register_fragment(repository, address, fragment)
        .await
        .expect("registration failed");
    let lease = match catalog
        .begin_obliteration(address.hash)
        .await
        .expect("failed to begin obliteration")
    {
        BeginObliteration::AlreadyObliterated => panic!("fragment was unexpectedly terminal"),
        BeginObliteration::Acquired(lease) => lease,
    };

    let task_count = 32;
    let barrier = Arc::new(Barrier::new(task_count + 2));
    let mut tasks = Vec::with_capacity(task_count);
    for index in 0..task_count {
        let catalog = catalog.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            let mut repository_bytes = [0x70; 16];
            repository_bytes[0] = index as u8;
            let mut context_bytes = [0x71; 16];
            context_bytes[0] = index as u8;
            barrier.wait().await;
            catalog
                .associate_fragment(
                    Context::from(repository_bytes),
                    Address {
                        hash: address.hash,
                        context: Context::from(context_bytes),
                    },
                )
                .await
        }));
    }
    let release_catalog = catalog.clone();
    let release_barrier = barrier.clone();
    let release = tokio::spawn(async move {
        release_barrier.wait().await;
        release_catalog
            .release_association(repository, address, lease)
            .await
    });
    barrier.wait().await;

    for task in tasks {
        assert!(
            task.await.expect("association task panicked").is_err(),
            "an association crossed the obliteration marker"
        );
    }
    assert_eq!(
        release
            .await
            .expect("release task panicked")
            .expect("association release failed"),
        ReleaseAssociation::PayloadUnreferenced
    );
    assert_eq!(
        catalog
            .load_metadata(address.hash)
            .await
            .expect("failed to load marker"),
        lease.marker()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_begin_calls_share_one_deterministic_lease() {
    let catalog = Arc::new(
        PostgresFragmentCatalog::connect_for_test()
            .await
            .expect("failed to create isolated PostgreSQL catalog"),
    );
    let repository = Context::from([0x81; 16]);
    let address = Address {
        hash: Hash::from([0x82; 32]),
        context: Context::from([0x83; 16]),
    };
    catalog
        .register_fragment(
            repository,
            address,
            Fragment {
                flags: 0,
                size_payload: 32,
                size_content: 32,
            },
        )
        .await
        .expect("registration failed");

    let task_count = 32;
    let barrier = Arc::new(Barrier::new(task_count + 1));
    let mut tasks = Vec::with_capacity(task_count);
    for _ in 0..task_count {
        let catalog = catalog.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            catalog.begin_obliteration(address.hash).await
        }));
    }
    barrier.wait().await;

    let mut expected = None;
    for task in tasks {
        let result = task
            .await
            .expect("begin task panicked")
            .expect("begin operation failed");
        let BeginObliteration::Acquired(lease) = result else {
            panic!("begin operation returned terminal state");
        };
        if let Some(expected) = expected {
            assert_eq!(lease, expected);
        } else {
            expected = Some(lease);
        }
    }
}
