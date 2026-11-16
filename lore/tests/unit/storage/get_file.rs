// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_get_file` and `lore_storage_get_file_resolved` keep the temporary file of a get
//! that fails, and resume from it.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use lore::interface::LoreGlobalArgs;
use lore::storage::get_file::LoreStorageGetFileArgs;
use lore::storage::get_file::LoreStorageGetFileItem;
use lore::storage::get_file::get_file;
use lore::storage::get_file_resolved::LoreStorageGetFileResolvedArgs;
use lore::storage::get_file_resolved::LoreStorageGetFileResolvedItem;
use lore::storage::get_file_resolved::get_file_resolved;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::put::LoreStoragePutArgs;
use lore::storage::put::LoreStoragePutItem;
use lore::storage::put::put;
use lore::storage::store::BoundFlags;
use lore::storage::store::StoreInternal;
use lore::storage::store::in_memory_for_tests;
use lore_base::test_util::TempDir;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_revision::event::LoreBytes;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreString;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::errors::Disconnected;
use lore_storage::local::store_lock::StoreHold;
use lore_storage::test_util::HookedImmutableStore;
use lore_storage::test_util::ImmutableCall;

/// The leaves of each content here, each [`LEAF`] bytes: one leaf more than a get must write to
/// resume.
const LEAVES: usize = lore_storage::options::RESUME_WRITE_MIN_LEN / LEAF + 1;

/// The size each content here is chunked to, the most a fragment holds. The content is larger than
/// one fragment holds, so a put stores a fragment list.
const LEAF: usize = lore_base::types::FRAGMENT_SIZE_THRESHOLD;

fn partition() -> Partition {
    Partition::from([0xD4; 16])
}

fn context() -> Context {
    Context::from([0xD4; 16])
}

/// Content of [`LEAVES`] leaves, leaf `index` made with `seeds[index]`: the index and the seed,
/// then a fill. Returns the content and the addresses of its leaves in content order.
fn seeded_content(seeds: &[u8; LEAVES]) -> (Vec<u8>, Vec<Address>) {
    let mut content = Vec::with_capacity(LEAVES * LEAF);
    for (index, seed) in seeds.iter().enumerate() {
        let start = content.len();
        content.resize(start + LEAF, seed.wrapping_add(0x5A));
        content[start..start + 8].copy_from_slice(&(index as u64).to_le_bytes());
        content[start + 8] = *seed;
    }
    let leaves = content
        .chunks(LEAF)
        .map(|leaf| Address {
            hash: lore_storage::hash_slice(leaf),
            context: context(),
        })
        .collect();
    (content, leaves)
}

/// Puts `content` through a handle over `bare`, chunked to [`LEAF`] bytes, returning its address.
async fn put_content(bare: &Arc<StoreInternal>, content: &[u8]) -> Address {
    let handle = handle::register(bare.clone());
    let stored = Arc::new(Mutex::new(Vec::new()));
    let sink = stored.clone();
    let status = put(
        LoreGlobalArgs::default(),
        LoreStoragePutArgs {
            handle,
            items: LoreArray::from_vec(vec![LoreStoragePutItem {
                id: 1,
                partition: partition(),
                context: context(),
                data: LoreBytes {
                    ptr: content.as_ptr().cast(),
                    len: content.len(),
                },
                remote_write: 0,
                local_cache: 0,
                remote_query: 0,
                fixed_size_chunk: LEAF as u64,
            }]),
        },
        Some(Box::new(move |event| {
            if let lore_revision::event::LoreEvent::StoragePutItemComplete(data) = event {
                sink.lock().unwrap().push(data.address);
            }
        })),
    )
    .await;
    handle::unregister(handle);
    assert_eq!(status, 0, "the put succeeds");
    let stored = stored.lock().unwrap();
    stored[0]
}

/// A handle over `bare` whose store fails every load of `failing`, with the address of every load
/// it forwards kept in the returned list.
fn hooked_handle(
    bare: &StoreInternal,
    failing: Option<Address>,
) -> (LoreStore, Arc<Mutex<Vec<Address>>>) {
    let loads = Arc::new(Mutex::new(Vec::new()));
    let kept = loads.clone();
    let immutable: Arc<dyn ImmutableStore> =
        HookedImmutableStore::wrap(bare.immutable.clone(), move |call, _partition, address| {
            if call != ImmutableCall::Get {
                return Ok(());
            }
            if Some(address) == failing {
                return Err(StoreError::from(Disconnected));
            }
            kept.lock().unwrap().push(address);
            Ok(())
        });
    let store = StoreInternal::new(
        bare.identity.clone(),
        immutable,
        bare.mutable.clone(),
        None,
        BoundFlags::default(),
        false,
        StoreHold::default(),
    );
    (handle::register(Arc::new(store)), loads)
}

/// The indices of the `leaves` loaded to be written, in order and once each. The walk of the list
/// loads its first leaf once to learn that the list holds leaves, so that leaf counts only from
/// its second load.
fn leaves_loaded(loads: &Mutex<Vec<Address>>, leaves: &[Address]) -> Vec<usize> {
    let loads = loads.lock().unwrap();
    (0..leaves.len())
        .filter(|index| {
            let count = loads.iter().filter(|load| **load == leaves[*index]).count();
            count > usize::from(*index == 0)
        })
        .collect()
}

/// Gets `address` into `target` through `handle`, returning the call's status.
async fn get_into(handle: LoreStore, address: Address, target: &Path) -> i32 {
    let status = get_file(
        LoreGlobalArgs::default(),
        LoreStorageGetFileArgs {
            handle,
            items: LoreArray::from_vec(vec![LoreStorageGetFileItem {
                id: 1,
                partition: partition(),
                address,
                path: LoreString::from(target.to_string_lossy().as_ref()),
                ..Default::default()
            }]),
        },
        None,
    )
    .await;
    handle::unregister(handle);
    status
}

/// Gets the content `key` resolves to into `target` through `handle`, returning the call's status.
async fn get_resolved_into(handle: LoreStore, key: Hash, target: &Path) -> i32 {
    let status = get_file_resolved(
        LoreGlobalArgs::default(),
        LoreStorageGetFileResolvedArgs {
            handle,
            items: LoreArray::from_vec(vec![LoreStorageGetFileResolvedItem {
                id: 1,
                partition: partition(),
                key,
                context: context(),
                path: LoreString::from(target.to_string_lossy().as_ref()),
                ..Default::default()
            }]),
        },
        None,
    )
    .await;
    handle::unregister(handle);
    status
}

/// Maps `key` to the content at `address` in `bare`'s mutable store.
async fn map_key(bare: &StoreInternal, key: Hash, address: Address) {
    bare.mutable
        .clone()
        .store(partition(), key, address.hash, KeyType::Resolve)
        .await
        .expect("the key is mapped");
}

/// A get that fails keeps its temporary file, and a retry of it loads none of the leaves the failed
/// get wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_get_file_resumes_from_its_temporary_file() {
    const FAILING: usize = 8;

    let bare = in_memory_for_tests("get-file-resume").await;
    let target_dir = TempDir::new("lore-get-file-resume-");
    let target = target_dir.path().join("content.bin");
    let (content, leaves) = seeded_content(&[0; LEAVES]);
    let address = put_content(&bare, &content).await;

    let (failing, _) = hooked_handle(&bare, Some(leaves[FAILING]));
    assert_ne!(
        get_into(failing, address, &target).await,
        0,
        "the get fails"
    );
    assert!(
        target.with_extension("bin.loretmp").exists(),
        "the failed get keeps its temporary file"
    );

    let (resumed, loads) = hooked_handle(&bare, None);
    assert_eq!(
        get_into(resumed, address, &target).await,
        0,
        "the retry succeeds"
    );
    let loaded = leaves_loaded(&loads, &leaves);
    assert!(
        loaded.contains(&FAILING) && loaded.iter().all(|index| *index >= FAILING),
        "the retry loads the leaf the get failed at and none it wrote: {loaded:?}"
    );
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
}

/// A resolved get that fails is resumed by a retry whose key still names the same content, and
/// not by one whose key has moved to other content, even content sharing leaves at the same
/// offsets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_get_file_resolved_resumes_only_for_the_content_it_wrote() {
    let last = LEAVES - 1;
    let mut moved_seeds = [0; LEAVES];
    moved_seeds[last] = 1;

    let bare = in_memory_for_tests("get-file-resolved-resume").await;
    let target_dir = TempDir::new("lore-get-file-resolved-resume-");
    let target = target_dir.path().join("content.bin");
    let key = lore_storage::hash_slice(b"the key a resolved get resumes for");
    let (content, leaves) = seeded_content(&[0; LEAVES]);
    let (moved_content, moved_leaves) = seeded_content(&moved_seeds);
    let address = put_content(&bare, &content).await;
    let moved = put_content(&bare, &moved_content).await;
    map_key(&bare, key, address).await;

    let (failing, _) = hooked_handle(&bare, Some(leaves[last]));
    assert_ne!(get_resolved_into(failing, key, &target).await, 0);
    let (resumed, loads) = hooked_handle(&bare, None);
    assert_eq!(get_resolved_into(resumed, key, &target).await, 0);
    assert_eq!(
        leaves_loaded(&loads, &leaves),
        [last],
        "the retry loads only the leaf the get failed at"
    );
    assert_eq!(std::fs::read(&target).expect("read the target"), content);

    std::fs::remove_file(&target).expect("remove the target");
    let (failing, _) = hooked_handle(&bare, Some(leaves[last]));
    assert_ne!(get_resolved_into(failing, key, &target).await, 0);
    map_key(&bare, key, moved).await;
    let (moved_get, loads) = hooked_handle(&bare, None);
    assert_eq!(get_resolved_into(moved_get, key, &target).await, 0);
    assert_eq!(
        leaves_loaded(&loads, &moved_leaves),
        (0..LEAVES).collect::<Vec<_>>(),
        "a get of the content the key moved to loads every leaf of it"
    );
    assert_eq!(
        std::fs::read(&target).expect("read the target"),
        moved_content
    );
}
