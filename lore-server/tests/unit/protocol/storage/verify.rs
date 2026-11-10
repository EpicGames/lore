// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::HealResult;
use lore_revision::fragment::generate_random;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore::RepositoryId;
use lore_revision::relay::EventDispatcher;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::protocol::attribute_map::AttributeMap;
use lore_server::protocol::storage::messages::LoreResponse;
use lore_server::protocol::storage::messages::Message;
use lore_server::protocol::storage::messages::MessageHandleError;
use lore_server::protocol::storage::messages::MessageParseError;
use lore_server::protocol::storage::verify::*;
use lore_storage::FlushOptions;
use lore_storage::ImmutableStore;
use lore_storage::StoreMatch;
use lore_storage::local::immutable_store::ImmutableStoreSettings;
use rand::random;

use crate::store::test_support::test_store_create;
use crate::uds_server::counted_heals;

fn allow_all() -> Arc<dyn RepositoryAuthorizer> {
    Arc::new(lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer)
}

fn make_verify_bytes(address: Address, heal: bool) -> Bytes {
    #[allow(unused_imports)]
    use zerocopy::IntoBytes;

    let mut bytes = address.as_bytes().to_vec();
    bytes.push(if heal { 1 } else { 0 });
    Bytes::from(bytes)
}

fn generate_tempdir() -> lore_base::test_util::TempDir {
    lore_base::test_util::TempDir::new("lore-verify-test-")
}

fn setup_test_execution() -> Arc<ExecutionContext> {
    Arc::new(ExecutionContext::new_client(
        LoreGlobalArgs::default(),
        EventDispatcher::no_dispatch(),
    ))
}

#[test]
fn test_parse_valid() {
    let hash = random::<Hash>();
    let context = random::<Context>();
    let address = Address { hash, context };

    assert_eq!(
        Verify::parse(make_verify_bytes(address, false)),
        Ok(Verify { address, heal: 0 })
    );
}

#[test]
fn test_parse_with_heal_flag() {
    let hash = random::<Hash>();
    let context = random::<Context>();
    let address = Address { hash, context };

    assert_eq!(
        Verify::parse(make_verify_bytes(address, true)),
        Ok(Verify { address, heal: 1 })
    );
}

#[test]
fn test_parse_too_short() {
    // We expect 49 bytes total, so trying to parse less than that should fail...
    assert_eq!(
        Verify::parse(Bytes::from(vec![0u8; 10])),
        Err(MessageParseError::InvalidFieldLength)
    );
}

#[tokio::test]
async fn test_handle_not_connected() {
    let hash = random::<Hash>();
    let context = random::<Context>();
    let address = Address { hash, context };

    // Empty context map - no repository set
    let context_map = Arc::new(AttributeMap::default());

    let (immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    LORE_CONTEXT
        .scope(execution, async move {
            let message = Verify { address, heal: 0 };
            match message
                .handle(context_map, immutable_store, allow_all())
                .await
            {
                Err(MessageHandleError::NotConnected) => (),
                Err(e) => panic!("Expected NotConnected error, got {e:?}"),
                Ok(_) => panic!("Expected NotConnected error, got Ok"),
            }
        })
        .await;
}

#[tokio::test]
async fn test_handle_not_found() {
    let dir = generate_tempdir();
    let dir_path = dir.path().to_path_buf();
    let execution = setup_test_execution();

    LORE_CONTEXT
        .scope(execution, async move {
            let store = lore_storage::LocalImmutableStore::new(
                Some(dir_path),
                ImmutableStoreSettings::default(),
            )
            .await
            .expect("Failed to create store");

            let repository: RepositoryId = random();

            // Write a fragment first so the index infrastructure exists
            let (fragment, address, payload) = generate_random();

            store
                .clone()
                .put(repository, address, fragment, Some(payload), false)
                .await
                .expect("Failed to put fragment");

            store
                .clone()
                .flush(FlushOptions::SYNC_DATA)
                .await
                .expect("Failed to flush");

            // Now try to verify a non-existent fragment in the same bucket
            let mut nonexistent_hash = address.hash;
            nonexistent_hash.data_mut()[2] = nonexistent_hash.data()[2].wrapping_add(1);
            let nonexistent_address = Address {
                hash: nonexistent_hash,
                context: random(),
            };

            let context_map = Arc::new(AttributeMap::default());
            context_map.insert(repository);

            let message = Verify {
                address: nonexistent_address,
                heal: 0,
            };

            match message.handle(context_map, store, allow_all()).await {
                Err(MessageHandleError::FragmentNotFound) => (),
                Err(e) => panic!("Expected FragmentNotFound error, got {e:?}"),
                Ok(_) => panic!("Expected FragmentNotFound error, got Ok"),
            }
        })
        .await;
}

#[tokio::test]
async fn test_handle_success() {
    let dir = generate_tempdir();
    let dir_path = dir.path().to_path_buf();
    let execution = setup_test_execution();

    LORE_CONTEXT
        .scope(execution, async move {
            let store = lore_storage::LocalImmutableStore::new(
                Some(dir_path),
                ImmutableStoreSettings::default(),
            )
            .await
            .expect("Failed to create store");

            let repository: RepositoryId = random();
            let (fragment, address, payload) = generate_random();

            store
                .clone()
                .put(repository, address, fragment, Some(payload), false)
                .await
                .expect("Failed to put fragment");

            store
                .clone()
                .flush(FlushOptions::SYNC_DATA)
                .await
                .expect("Failed to flush");

            let context_map = Arc::new(AttributeMap::default());
            context_map.insert(repository);

            let message = Verify { address, heal: 0 };

            match message.handle(context_map, store, allow_all()).await {
                Ok(LoreResponse::Verify(resp)) => {
                    assert_eq!(resp.corrupted, 0);
                    assert_eq!(resp.healed, HealResult::NotAttempted);
                }
                Ok(other) => panic!("Expected Verify response, got {other:?}"),
                Err(e) => panic!("Expected success, got error: {e:?}"),
            }
        })
        .await;
}

fn corrupt_packfile(
    store_path: &std::path::Path,
    group_index: u8,
    pack_file: u32,
    pack_offset: u32,
) {
    use std::io::Read;
    use std::io::Seek;
    use std::io::SeekFrom;
    use std::io::Write;

    let pack_path = store_path.join(format!(
        "immutable/index/{group_index:02x}/pack/{pack_file}"
    ));

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&pack_path)
        .expect("Failed to open packfile for corruption");

    file.seek(SeekFrom::Start(pack_offset as u64))
        .expect("Failed to seek to pack_offset");
    let mut buf = [0u8; 1];
    file.read_exact(&mut buf)
        .expect("Failed to read from packfile");

    file.seek(SeekFrom::Start(pack_offset as u64))
        .expect("Failed to seek to pack_offset");
    file.write_all(&[0xFF; 16])
        .expect("Failed to write corruption bytes");
    file.sync_all().expect("Failed to sync packfile");
}

/// A local store under `dir` holding one fragment whose packed bytes are corrupt, with the
/// repository and the address the fragment was put under.
async fn store_with_a_corrupt_fragment(
    dir: &std::path::Path,
) -> (
    Arc<lore_storage::LocalImmutableStore>,
    RepositoryId,
    Address,
) {
    let store = lore_storage::LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings::default(),
    )
    .await
    .expect("Failed to create store");

    let repository: RepositoryId = random();
    let (fragment, address, payload) = generate_random();

    store
        .clone()
        .put(repository, address, fragment, Some(payload), false)
        .await
        .expect("Failed to put fragment");

    store
        .clone()
        .flush(FlushOptions::SYNC_DATA)
        .await
        .expect("Failed to flush");

    let result = store
        .clone()
        .verify_fragment(address, repository, StoreMatch::MatchFull, false)
        .await
        .expect("verify_fragment failed");

    let pack_file = result.matches[0].data.pack_file;
    let pack_offset = result.matches[0].data.pack_offset;

    // Drop the store to release file handles before corrupting the packfile
    // (Windows does not allow writing to files opened by another handle without
    // FILE_SHARE_WRITE).
    drop(store);

    corrupt_packfile(dir, address.hash.data()[0], pack_file, pack_offset);

    // Recreate the store so it reloads from disk
    let store = lore_storage::LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings::default(),
    )
    .await
    .expect("Failed to recreate store");

    (store, repository, address)
}

#[tokio::test]
async fn test_handle_corrupted_heal() {
    let dir = generate_tempdir();
    let execution = setup_test_execution();

    LORE_CONTEXT
        .scope(execution, async move {
            let (store, repository, address) = store_with_a_corrupt_fragment(&dir).await;

            let context_map = Arc::new(AttributeMap::default());
            context_map.insert(repository);

            let message = Verify { address, heal: 1 };
            match message.handle(context_map, store, allow_all()).await {
                Ok(LoreResponse::Verify(resp)) => {
                    assert_eq!(resp.corrupted, 1);
                    assert_eq!(resp.healed, HealResult::Healed);
                }
                Ok(other) => panic!("Expected Verify response, got {other:?}"),
                Err(e) => panic!("Expected success with heal, got error: {e:?}"),
            }
        })
        .await;
}

/// The local store's heal drops a corrupt copy, so the next read of it misses, and answers that a
/// copy the store no longer holds needs no heal.
#[tokio::test]
async fn the_local_stores_heal_drops_a_corrupt_copy() {
    let dir = generate_tempdir();
    let execution = setup_test_execution();

    LORE_CONTEXT
        .scope(execution, async move {
            let (store, repository, address) = store_with_a_corrupt_fragment(&dir).await;

            let healed = store.clone().heal(repository, address).await;
            assert!(matches!(healed, Ok(HealResult::Healed)), "{healed:?}");
            assert!(
                store.clone().get(repository, address).await.is_err(),
                "the corrupt copy is gone"
            );
            let again = store.clone().heal(repository, address).await;
            assert!(matches!(again, Ok(HealResult::NotAttempted)), "{again:?}");
        })
        .await;
}

#[tokio::test]
async fn test_handle_corrupted_no_heal() {
    let dir = generate_tempdir();
    let execution = setup_test_execution();

    LORE_CONTEXT
        .scope(execution, async move {
            let (store, repository, address) = store_with_a_corrupt_fragment(&dir).await;

            let context_map = Arc::new(AttributeMap::default());
            context_map.insert(repository);

            let message = Verify { address, heal: 0 };
            match message.handle(context_map, store, allow_all()).await {
                Ok(LoreResponse::Verify(resp)) => {
                    assert_eq!(resp.corrupted, 1);
                    assert_eq!(resp.healed, HealResult::NotAttempted);
                }
                Ok(other) => panic!("Expected Verify response, got {other:?}"),
                Err(e) => panic!("Expected success, got error: {e:?}"),
            }
        })
        .await;
}

/// A heal of a corrupt local copy also asks the store beyond the local one to heal its own; a
/// verify without a heal asks nothing of it.
#[tokio::test]
async fn test_handle_corrupted_heal_asks_the_store_beyond_the_local_one() {
    let dir = generate_tempdir();
    let execution = setup_test_execution();

    LORE_CONTEXT
        .scope(execution, async move {
            let (local, repository, address) = store_with_a_corrupt_fragment(&dir).await;
            let healed = Arc::new(AtomicUsize::new(0));
            let beyond: Arc<dyn ImmutableStore> =
                Arc::new(counted_heals(local.clone(), healed.clone()));
            let verify = |heal_flag: u8| {
                handle_verify(
                    address,
                    heal_flag,
                    repository,
                    String::new(),
                    String::new(),
                    local.clone(),
                    beyond.clone(),
                )
            };

            let checked = verify(0).await;
            let heals_after_check = healed.load(Ordering::SeqCst);
            let healing = verify(1).await;

            match checked {
                Ok(LoreResponse::Verify(resp)) => {
                    assert_eq!(resp.corrupted, 1);
                    assert_eq!(resp.healed, HealResult::NotAttempted);
                }
                other => panic!("Expected a Verify response, got {other:?}"),
            }
            assert_eq!(heals_after_check, 0, "a verify without a heal asks none");
            match healing {
                Ok(LoreResponse::Verify(resp)) => {
                    assert_eq!(resp.corrupted, 1);
                    assert_eq!(resp.healed, HealResult::Healed);
                }
                other => panic!("Expected a Verify response, got {other:?}"),
            }
            assert_eq!(healed.load(Ordering::SeqCst), 1);
        })
        .await;
}

/// The verify of the legacy services heals beyond the local store as well, in the store they
/// pass beside it.
#[tokio::test]
async fn a_legacy_verify_heals_beyond_the_local_store() {
    let dir = generate_tempdir();
    let execution = setup_test_execution();

    LORE_CONTEXT
        .scope(execution, async move {
            let (local, repository, address) = store_with_a_corrupt_fragment(&dir).await;
            let healed = Arc::new(AtomicUsize::new(0));
            let beyond: Arc<dyn ImmutableStore> =
                Arc::new(counted_heals(local.clone(), healed.clone()));
            let context_map = Arc::new(AttributeMap::default());
            context_map.insert(repository);

            let response = Verify { address, heal: 1 }
                .handle_with(context_map, local, beyond)
                .await;

            match response {
                Ok(LoreResponse::Verify(resp)) => {
                    assert_eq!(resp.corrupted, 1);
                    assert_eq!(resp.healed, HealResult::Healed);
                }
                other => panic!("Expected a Verify response, got {other:?}"),
            }
            assert_eq!(healed.load(Ordering::SeqCst), 1);
        })
        .await;
}
