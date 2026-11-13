// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A storage handle whose store holds the buffer of one item's read past its answer, as an I/O
//! operation holds a dropped read's, and the checks a call over it must pass.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use lore::interface::LoreEventCallback;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::BoundFlags;
use lore::storage::store::StoreInternal;
use lore_base::types::Address;
use lore_revision::event::LoreBytesMut;
use lore_revision::event::LoreEvent;
use lore_storage::CallerBuffer;
use lore_storage::test_util::HeldAnswer;
use lore_storage::test_util::HoldingStore;
use tokio::sync::mpsc::UnboundedReceiver;

/// What the holder writes into the buffer it holds before letting it go.
pub(super) const HELD_FILL: u8 = 0x77;

/// The time a wait that should end is given before the test fails.
const PATIENCE: Duration = Duration::from_secs(5);

/// `buffer` as an item's `data_out`.
pub(super) fn data_out(buffer: &mut [u8]) -> LoreBytesMut {
    LoreBytesMut {
        ptr: buffer.as_mut_ptr().cast(),
        len: buffer.len(),
    }
}

/// A handle over `bare`'s stores whose immutable side holds the buffer of every read into a
/// caller's buffer of `held`, sending it to the receiver, and answers that it placed the payload.
pub(super) fn holding_handle(
    bare: &StoreInternal,
    held: Address,
) -> (LoreStore, UnboundedReceiver<CallerBuffer>) {
    let (immutable, holds) = HoldingStore::wrap(bare.immutable.clone(), held, HeldAnswer::Placed);
    let store = StoreInternal::new(
        bare.identity.clone(),
        immutable,
        bare.mutable.clone(),
        None,
        BoundFlags::default(),
        false,
        lore_storage::local::store_lock::StoreHold::default(),
    );
    (handle::register(Arc::new(store)), holds)
}

/// A callback sending the id and error code of each item as it completes.
pub(super) fn item_completions() -> (LoreEventCallback, UnboundedReceiver<(u64, i32)>) {
    let (sender, completions) = tokio::sync::mpsc::unbounded_channel();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::StorageGetItemComplete(data) = event {
            let _ = sender.send((data.id, data.error.error_code));
        }
    }));
    (callback, completions)
}

/// Run `call`, whose item 1 has its buffer held by the store and whose item 2 fails, and check
/// that both items are reported as they ended and that the call completes only once the held
/// buffer, written with [`HELD_FILL`], is let go.
pub(super) async fn assert_completes_once_the_held_buffer_is_let_go(
    call: impl Future<Output = i32> + Send + 'static,
    mut holds: UnboundedReceiver<CallerBuffer>,
    mut completions: UnboundedReceiver<(u64, i32)>,
) {
    let mut call = lore_base::lore_spawn!(call);
    let mut held = tokio::time::timeout(PATIENCE, holds.recv())
        .await
        .expect("the store is asked for item 1")
        .expect("the store holds item 1's buffer");
    let mut ended = Vec::new();
    for _ in 0..2 {
        ended.push(
            tokio::time::timeout(PATIENCE, completions.recv())
                .await
                .expect("each item completes")
                .expect("the call reports each item"),
        );
    }
    ended.sort_unstable();
    assert_eq!(ended[0], (1, 0), "item 1 is answered");
    assert_eq!(ended[1].0, 2);
    assert_ne!(ended[1].1, 0, "item 2 fails");

    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut call)
            .await
            .is_err(),
        "the call completed while a buffer it lent was held"
    );
    held.as_mut_slice().fill(HELD_FILL);
    drop(held);
    tokio::time::timeout(PATIENCE, call)
        .await
        .expect("the call completes once the buffer is let go")
        .expect("the call's task finishes");
}
