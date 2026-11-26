// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The store limits `lore::repository::clone` writes into the new instance's `[store]` config, and
//! those the instance's store enforces when opened for writing, as the clone opens it once the
//! instance is created.

use std::path::Path;

use lore::repository::LoreRepositoryCloneArgs;
use lore::shared_store::LoreSharedStoreCreateArgs;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::repository::CONFIG;
use lore_revision::repository::LoreSharedStoreMode;
use lore_revision::repository::RepositoryAccess;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::repository::StoreConfig;
use lore_revision::repository::load_and_connect_with_token;
use lore_revision::util::config::load_blocking;

use super::support::*;

/// 50,000,000 fragments, above the default.
const MAX_CAPACITY: u64 = 50_000_000;

/// 1 TiB.
const MAX_SIZE: u64 = 1 << 40;

/// The fragments a test's shared store holds, neither the default nor [`MAX_CAPACITY`].
const SHARED_MAX_CAPACITY: u64 = 3_000_000;

/// The bytes a test's shared store holds, 32 GiB: neither the default nor [`MAX_SIZE`].
const SHARED_MAX_SIZE: u64 = 1 << 35;

/// Clones the fixture repository into `dir` with `args`.
async fn clone_into(f: &Fixture, dir: &Path, args: LoreRepositoryCloneArgs) {
    events_of("cloning the repository", |callback| {
        lore::repository::clone(
            LoreGlobalArgs {
                repository_path: dir.into(),
                ..graph_globals()
            },
            LoreRepositoryCloneArgs {
                repository_url: LoreString::from(format!("{}/{}", f.url, f.repository).as_str()),
                ..args
            },
            callback,
        )
    })
    .await;
}

/// The `[store]` config of the instance at `dir`.
fn instance_store_config(dir: &Path) -> StoreConfig {
    load_blocking::<lore_revision::repository::RepositoryConfig>(dir.join(".lore").join(CONFIG))
        .expect("the instance config loads")
        .store
        .expect("the clone writes a [store] config")
}

/// The `(max_size, max_capacity)` caps the store of the instance at `dir` enforces, opened for
/// writing as the clone opens it.
async fn opened_store_caps(dir: &Path) -> (usize, usize) {
    LORE_CONTEXT
        .scope(crate::setup_execution(String::new()), async {
            let token = RepositoryWriteToken::acquire(dir).await;
            load_and_connect_with_token(dir, RepositoryAccess::ReadWrite, Some(token))
                .await
                .expect("the instance opens")
                .immutable_store()
                .gc_counters()
                .expect("a local store has GC counters")
                .caps()
        })
        .await
}

/// The default `(max_size, max_capacity)` caps.
fn default_caps() -> (usize, usize) {
    let default = StoreConfig::client_default();
    (
        default.max_size.expect("a default size"),
        default.max_capacity.expect("a default capacity"),
    )
}

/// A clone given limits writes them into the instance's `[store]` config, and its store enforces
/// them.
#[tokio::test]
async fn a_clone_writes_and_enforces_the_limits_it_is_given() {
    let f = fixture().await;
    seed_main(&f).await;
    let dir = TempDir::new("clone-store-limits-given-");

    clone_into(
        &f,
        dir.path(),
        LoreRepositoryCloneArgs {
            use_shared_store: LoreSharedStoreMode::Disabled,
            max_capacity: MAX_CAPACITY,
            max_size: MAX_SIZE,
            ..Default::default()
        },
    )
    .await;

    let config = instance_store_config(dir.path());
    assert_eq!(config.max_capacity, Some(MAX_CAPACITY as usize));
    assert_eq!(config.max_size, Some(MAX_SIZE as usize));
    assert_eq!(
        opened_store_caps(dir.path()).await,
        (MAX_SIZE as usize, MAX_CAPACITY as usize)
    );
    f.close().await;
}

/// A clone given no limits writes and enforces the defaults.
#[tokio::test]
async fn a_clone_without_limits_writes_and_enforces_the_defaults() {
    let f = fixture().await;
    seed_main(&f).await;
    let default = StoreConfig::client_default();
    let dir = TempDir::new("clone-store-limits-zero-");

    clone_into(
        &f,
        dir.path(),
        LoreRepositoryCloneArgs {
            use_shared_store: LoreSharedStoreMode::Disabled,
            ..Default::default()
        },
    )
    .await;

    let config = instance_store_config(dir.path());
    assert_eq!(config.max_capacity, default.max_capacity);
    assert_eq!(config.max_size, default.max_size);
    assert_eq!(opened_store_caps(dir.path()).await, default_caps());
    f.close().await;
}

/// A clone over a shared store writes the default limits into the instance's config, and the
/// shared store's limits stay in force.
#[tokio::test]
async fn a_clone_over_a_shared_store_ignores_its_limits() {
    let f = fixture().await;
    seed_main(&f).await;
    let default = StoreConfig::client_default();
    let dir = TempDir::new("clone-store-limits-shared-");
    let store_dir = TempDir::new("clone-store-limits-shared-store-");
    events_of("creating the shared store", |callback| {
        lore::shared_store::create(
            graph_globals(),
            LoreSharedStoreCreateArgs {
                remote_url: LoreString::from(f.url.as_str()),
                path: store_dir.path().into(),
                make_default: 0,
                max_size: SHARED_MAX_SIZE,
                max_capacity: SHARED_MAX_CAPACITY,
            },
            callback,
        )
    })
    .await;

    clone_into(
        &f,
        dir.path(),
        LoreRepositoryCloneArgs {
            use_shared_store: LoreSharedStoreMode::Enabled,
            shared_store_path: store_dir.path().into(),
            max_capacity: MAX_CAPACITY,
            max_size: MAX_SIZE,
            ..Default::default()
        },
    )
    .await;

    let config = instance_store_config(dir.path());
    assert_eq!(config.max_capacity, default.max_capacity);
    assert_eq!(config.max_size, default.max_size);
    assert_eq!(
        opened_store_caps(dir.path()).await,
        (SHARED_MAX_SIZE as usize, SHARED_MAX_CAPACITY as usize)
    );
    f.close().await;
}
