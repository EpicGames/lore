// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore::interface::LoreGlobalArgs;
use lore::storage::get::LoreStorageGetArgs;
use lore::storage::get::LoreStorageGetItem;
use lore::storage::get::get;
use lore::storage::store::in_memory_for_tests;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Partition;
use lore_revision::interface::LoreArray;

use super::held;

/// A `get` completes only once every buffer its items lent is let go: the store holds item 1's
/// buffer past its answer, as an I/O operation holds a dropped read's, while item 2, which names
/// content never stored, fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_get_completes_once_every_buffer_its_items_lent_is_let_go() {
    let bare = in_memory_for_tests("get-held-buffer").await;
    let partition = Partition::from([0x41; 16]);
    let payload = b"content whose buffer the store holds";
    let address = super::put_payload(&bare.immutable, partition, payload, 0).await;
    let (handle, holds) = held::holding_handle(&bare, address);

    let mut held_buffer = vec![0u8; payload.len()];
    let mut missing_buffer = vec![0u8; 8];
    let items = vec![
        LoreStorageGetItem {
            id: 1,
            partition,
            address,
            data_out: held::data_out(&mut held_buffer),
            ..Default::default()
        },
        LoreStorageGetItem {
            id: 2,
            partition,
            address: Address {
                hash: lore_storage::hash_slice(b"never stored"),
                context: Context::default(),
            },
            data_out: held::data_out(&mut missing_buffer),
            ..Default::default()
        },
    ];
    let (callback, completions) = held::item_completions();
    let call = get(
        LoreGlobalArgs::default(),
        LoreStorageGetArgs {
            handle,
            items: LoreArray::from_vec(items),
        },
        callback,
    );
    held::assert_completes_once_the_held_buffer_is_let_go(call, holds, completions).await;

    assert!(held_buffer.iter().all(|byte| *byte == held::HELD_FILL));
    lore::storage::handle::unregister(handle);
}
