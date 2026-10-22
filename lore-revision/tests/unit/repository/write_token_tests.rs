// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_methods)]

//! Regression coverage for `clone --no-tracking`: a `NoStore` context that
//! attaches the outer `Client` write token (held by clone for cross-thread
//! exclusion on the destination path) must grant write capability via
//! `try_write_mutable_store`. Without this, `branch::create`'s internal
//! helpers (`store_name_to_id`, `metadata_store`, `store_latest`) fail
//! with `WriteRequired`.
use std::sync::Arc;

use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::repository::get_or_create_repository_lock;
use lore_storage::local::store_lock::StoreHold;

use crate::helper::default_repository_creation_args;

async fn in_memory_context() -> Arc<RepositoryContext> {
    let (immutable, mutable) = lore_revision::repository::create_client_memory_stores()
        .await
        .expect("in-memory stores should be creatable");
    Arc::new(RepositoryContext::new(default_repository_creation_args(
        immutable, mutable,
    )))
}

/// A `NoStore` context with a `Client` write token attached must grant
/// write capability. This is the path `clone --no-tracking` takes: it
/// holds the per-path mutex via the outer Client token (clone.rs:877)
/// and shares siblings to every constructed context.
#[tokio::test]
async fn no_store_context_with_client_token_grants_write_capability() {
    let temp_dir = lore_base::test_util::TempDir::new("lore-write-token-test-");
    let token = RepositoryWriteToken::acquire(&temp_dir).await;
    let ctx = in_memory_context().await;
    let with_token = Arc::new(
        Arc::try_unwrap(ctx)
            .expect("sole owner")
            .with_write_token(token),
    );
    assert!(
        with_token.try_write_mutable_store().is_some(),
        "context with Client token should expose a write handle"
    );
}

/// Without an attached token, an in-memory context is read-only — confirms
/// that `repository_call_no_store` callers (e.g. `config_get`) keep their
/// fail-loud behavior on accidental writes.
#[tokio::test]
async fn no_token_means_no_write_capability() {
    let ctx = in_memory_context().await;
    assert!(
        ctx.try_write_mutable_store().is_none(),
        "context without a token must not expose a write handle"
    );
}

/// A context without command locks holds no write token, so once the context it came from is
/// gone a later command takes the token while the copy lives. It holds another filesystem provider
/// and keeps the settings of the context it came from.
#[tokio::test]
async fn a_context_without_command_locks_holds_no_write_token() {
    let temp_dir = lore_base::test_util::TempDir::new("lore-write-token-test-");
    let token = RepositoryWriteToken::acquire(&temp_dir).await;
    let ctx = Arc::try_unwrap(in_memory_context().await)
        .expect("sole owner")
        .with_write_token(token);
    ctx.set_direct_file_write(true);
    let provider = ctx.file_system();

    let unlocked = ctx.without_command_locks();
    drop(ctx);

    assert!(unlocked.try_write_token().is_none());
    let _later = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        RepositoryWriteToken::acquire(&temp_dir),
    )
    .await
    .expect("a later command takes the token while the copy lives");
    assert!(!Arc::ptr_eq(&unlocked.file_system(), &provider));
    assert!(unlocked.direct_file_write(), "the settings come along");
}

/// A context without command locks holds no reference to the repository lock: once the context it
/// came from is gone, the lock is released while the copy lives, and taken again at once.
#[tokio::test]
async fn a_context_without_command_locks_holds_no_repository_lock() {
    let temp_dir = lore_base::test_util::TempDir::new("lore-repository-lock-test-");
    let dot_path = temp_dir.path().to_path_buf();
    let (lock, stores) = get_or_create_repository_lock(dot_path.clone(), StoreHold::default())
        .await
        .expect("the repository lock is taken");
    let held = Arc::downgrade(&lock);
    let ctx = Arc::try_unwrap(in_memory_context().await)
        .expect("sole owner")
        .with_repository_lock(lock, stores);

    let unlocked = ctx.without_command_locks();
    drop(ctx);

    assert!(held.upgrade().is_none(), "the copy holds the lock");
    let _later = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        get_or_create_repository_lock(dot_path, StoreHold::default()),
    )
    .await
    .expect("a later command takes the lock while the copy lives")
    .expect("the repository lock is taken again");
    drop(unlocked);
}
