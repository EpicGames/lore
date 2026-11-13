// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore::interface::LoreGlobalArgs;
use lore::storage::get_resolved::LoreStorageGetResolvedArgs;
use lore::storage::get_resolved::LoreStorageGetResolvedItem;
use lore::storage::get_resolved::fragments_read_options;
use lore::storage::get_resolved::get_resolved;
use lore::storage::store::in_memory_for_tests;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_revision::interface::LoreArray;
use lore_storage::options::ReadOptions;

use super::held;

/// `fragments` delivery reads payloads as stored from a trusted remote, whatever options the item
/// otherwise reads with, so no leaf is decoded or hashed on its way to the caller.
#[test]
fn fragments_delivery_reads_as_stored_from_a_trusted_remote() {
    for options in [
        ReadOptions::default(),
        ReadOptions::default().with_cache(),
        ReadOptions::default().no_local(),
    ] {
        let fragments = fragments_read_options(options);
        assert!(!fragments.decompress, "a leaf is expanded");
        assert!(fragments.trust_remote, "a fetched leaf is checked");
    }
}

/// A `get_resolved` completes only once every buffer its items lent is let go: the store holds item
/// 1's buffer past its answer, as an I/O operation holds a dropped read's, while item 2, whose key
/// names nothing, fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_get_resolved_completes_once_every_buffer_its_items_lent_is_let_go() {
    let bare = in_memory_for_tests("get-resolved-held-buffer").await;
    let partition = Partition::from([0x42; 16]);
    let payload = b"resolved content whose buffer the store holds";
    let address = super::put_payload(&bare.immutable, partition, payload, 0).await;
    let key = lore_storage::hash_slice(b"the key resolving to the held content");
    bare.mutable
        .clone()
        .store(partition, key, address.hash, KeyType::Resolve)
        .await
        .expect("the key is mapped");
    let (handle, holds) = held::holding_handle(&bare, address);

    let mut held_buffer = vec![0u8; payload.len()];
    let mut missing_buffer = vec![0u8; 8];
    let items = vec![
        LoreStorageGetResolvedItem {
            id: 1,
            partition,
            key,
            data_out: held::data_out(&mut held_buffer),
            ..Default::default()
        },
        LoreStorageGetResolvedItem {
            id: 2,
            partition,
            key: lore_storage::hash_slice(b"a key mapped to nothing"),
            data_out: held::data_out(&mut missing_buffer),
            ..Default::default()
        },
    ];
    let (callback, completions) = held::item_completions();
    let call = get_resolved(
        LoreGlobalArgs::default(),
        LoreStorageGetResolvedArgs {
            handle,
            items: LoreArray::from_vec(items),
        },
        callback,
    );
    held::assert_completes_once_the_held_buffer_is_let_go(call, holds, completions).await;

    assert!(held_buffer.iter().all(|byte| *byte == held::HELD_FILL));
    lore::storage::handle::unregister(handle);
}
