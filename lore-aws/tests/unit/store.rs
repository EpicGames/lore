// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod fake;
mod immutable_store;
mod lock_store;
mod mutable_store;
mod object_metadata;
mod single_flight;

use std::pin::Pin;
use std::task::Poll;

/// An execution context for `user_id`, dispatching no events.
pub fn setup_execution(
    user_id: String,
) -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    std::sync::Arc::new(lore_revision::interface::ExecutionContext::new_server(
        lore_revision::interface::LoreGlobalArgs::default(),
        lore_revision::relay::EventDispatcher::no_dispatch(),
        user_id,
    ))
}

/// Polls `future` once. The faked services answer on the first poll, so a store call still pending
/// after one is waiting on something other than them.
pub async fn poll_once<F: Future + ?Sized>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
}
