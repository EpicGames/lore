// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The final switch's locks: the store set read from the virtual instances' configs, and the
//! stores' locks taken in path order with bounded tries. A store's lock that another holder takes
//! through an `FSLock` of its own contends as another process's would.
// Fixtures plant external directories and stores directly, outside any repository.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use lore_base::fs::lock::FSLock;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_revision::global::external_dir::locks::*;
use lore_revision::instance::InstanceId;
use lore_revision::shared_store::SHARED_STORE_DIR;
use lore_revision::shared_store::shared_store_subdir_for_remote;
use lore_storage::local::store_lock::Intent;
use lore_storage::local::store_lock::StoreLock;

use crate::helper::execution_in;

/// The remote every planted config names.
const REMOTE: &str = "lore://example.invalid/repository";

/// A root of external directories, and a directory of shared stores beside it.
struct Fixture {
    root: TempDir,
    stores: TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            root: TempDir::new("lore-external-locks-root-"),
            stores: TempDir::new("lore-external-locks-stores-"),
        }
    }

    /// The directory of a shared store under `name`, which a config names by `name`.
    fn store(&self, name: &str) -> PathBuf {
        self.stores
            .path()
            .join(name)
            .join(shared_store_subdir_for_remote(REMOTE))
            .join(SHARED_STORE_DIR)
    }

    /// Makes the shared store under `name`, with its immutable and mutable directories.
    fn make_store(&self, name: &str) -> PathBuf {
        let store = self.store(name);
        std::fs::create_dir_all(store.join("immutable")).expect("make the immutable store");
        std::fs::create_dir_all(store.join("mutable")).expect("make the mutable store");
        store
    }

    /// Plants an external directory at `name` whose config is `config`.
    fn plant(&self, name: &str, config: &str) {
        let dot_lore = self.root.path().join(name).join(".lore");
        std::fs::create_dir_all(&dot_lore).expect("make the external directory");
        std::fs::write(dot_lore.join("config.toml"), config).expect("write the config");
    }

    /// Plants a virtual instance's external directory whose config names the store under `name`.
    fn plant_using(&self, name: &str) -> InstanceId {
        let instance = InstanceId::generate();
        self.plant(
            &instance.text_encoding(),
            &format!(
                "remote_url = \"{REMOTE}\"\n[shared_store_to_use]\nuse_shared_store = true\n\
                 shared_store_path = {:?}\n",
                self.stores.path().join(name)
            ),
        );
        instance
    }

    async fn store_set(&self) -> Result<Vec<PathBuf>, LocksError> {
        LORE_CONTEXT
            .scope(
                execution_in(self.stores.path()),
                store_set(self.root.path()),
            )
            .await
    }
}

/// Takes the lock file of `store`'s immutable directory through an `FSLock` of its own.
async fn hold_elsewhere(store: &Path) -> FSLock {
    FSLock::try_acquire_exact_path(&store.join("immutable").join("lock"))
        .await
        .expect("look at the flock")
        .expect("take the flock")
}

/// The store set holds each store a virtual instance's config names, once, and skips what holds
/// no virtual instance: the marker, an aside sibling, a file, a directory with no `.lore`, a
/// `.lore` with no config, and a config whose shared store no longer exists.
#[tokio::test]
async fn the_store_set_holds_the_stores_the_configs_name() {
    let fixture = Fixture::new();
    let store = fixture.make_store("a");
    let first = fixture.plant_using("a");
    fixture.plant_using("a");
    fixture.plant_using("missing");
    std::fs::write(fixture.root.path().join(".lore-external-root"), "id = 1").expect("marker");
    std::fs::write(fixture.root.path().join("file"), "").expect("a file");
    std::fs::create_dir(
        fixture
            .root
            .path()
            .join(InstanceId::generate().text_encoding()),
    )
    .expect("an external directory with no config");
    std::fs::create_dir_all(
        fixture
            .root
            .path()
            .join(InstanceId::generate().text_encoding())
            .join(".lore"),
    )
    .expect("an external directory whose .lore holds no config");
    fixture.plant(
        &format!("{}.deleting-{first}", first.text_encoding()),
        "not toml = [",
    );

    assert_eq!(fixture.store_set().await.expect("the store set"), [store]);
}

/// The store set is in path order.
#[tokio::test]
async fn the_store_set_is_in_path_order() {
    let fixture = Fixture::new();
    let stores = [fixture.make_store("b"), fixture.make_store("a")];
    fixture.plant_using("b");
    fixture.plant_using("a");

    assert_eq!(
        fixture.store_set().await.expect("the store set"),
        [stores[1].clone(), stores[0].clone()]
    );
}

/// A config that can't be read, or that names no shared store, refuses the store set.
#[tokio::test]
async fn an_unreadable_config_refuses_the_store_set() {
    let fixture = Fixture::new();
    fixture.plant(&InstanceId::generate().text_encoding(), "not toml = [");
    assert!(
        fixture
            .store_set()
            .await
            .expect_err("refused")
            .is_internal()
    );

    let fixture = Fixture::new();
    fixture.plant(
        &InstanceId::generate().text_encoding(),
        &format!("remote_url = \"{REMOTE}\"\n"),
    );
    assert!(
        fixture
            .store_set()
            .await
            .expect_err("refused")
            .is_internal()
    );
}

/// An external directory whose config can't be looked at, as its `.lore` is a file, refuses the
/// store set.
#[tokio::test]
async fn a_config_that_cannot_be_looked_at_refuses_the_store_set() {
    let fixture = Fixture::new();
    let dir = fixture
        .root
        .path()
        .join(InstanceId::generate().text_encoding());
    std::fs::create_dir(&dir).expect("make the external directory");
    std::fs::write(dir.join(".lore"), "").expect("a file in place of .lore");

    assert!(
        fixture
            .store_set()
            .await
            .expect_err("refused")
            .is_internal()
    );
}

/// An external directory whose `.lore` is a link or a junction to nothing has no config, and
/// contributes no store.
#[tokio::test]
async fn a_dot_lore_that_leads_nowhere_contributes_no_store() {
    let fixture = Fixture::new();
    let dir = fixture
        .root
        .path()
        .join(InstanceId::generate().text_encoding());
    std::fs::create_dir(&dir).expect("make the external directory");
    let nowhere = fixture.root.path().join("nowhere");
    #[cfg(target_family = "unix")]
    std::os::unix::fs::symlink(&nowhere, dir.join(".lore")).expect("the link");
    #[cfg(target_family = "windows")]
    lore_io::junction::create_junction(&nowhere, &dir.join(".lore")).expect("the junction");

    assert!(fixture.store_set().await.expect("the store set").is_empty());
}

/// A poll that runs out on a store another holds releases every lock, names the store, and
/// starts over in the same order, which then takes every lock.
#[tokio::test]
async fn a_poll_that_runs_out_releases_every_lock_and_starts_over() {
    let fixture = Fixture::new();
    let stores = [fixture.make_store("a"), fixture.make_store("b")];
    let mut holder = Some(hold_elsewhere(&stores[1]).await);
    let first = StoreLock::new(stores[0].join("immutable"), false).expect("the first lock");
    let mut named = Vec::new();

    let locks = lock_stores(&stores, Joining::Never, |store| {
        assert!(
            !first.is_held(),
            "every lock is released before the order starts over"
        );
        named.push(store.to_path_buf());
        holder = None;
    })
    .await
    .expect("every lock is taken");

    assert_eq!(named, [stores[1].clone()]);
    assert!(first.is_held());
    drop(locks);
    assert!(!first.is_held());
}

/// One attempt at each lock answers the store another holds, holding nothing.
#[tokio::test]
async fn one_attempt_answers_a_held_store() {
    let fixture = Fixture::new();
    let stores = [fixture.make_store("a"), fixture.make_store("b")];
    let _holder = hold_elsewhere(&stores[1]).await;

    let taken = try_lock_stores(&stores, Joining::Never, Duration::ZERO)
        .await
        .expect("the attempt");

    assert!(matches!(taken, Taken::Held(store) if store == stores[1]));
    let first = StoreLock::new(stores[0].join("immutable"), false).expect("the first lock");
    assert!(!first.is_held());
}

/// A store this process holds refuses the command's locks with `MountsBusy`, and recovery in the
/// service joins its hold.
#[tokio::test]
async fn a_store_this_process_holds_is_refused_unless_joined() {
    let fixture = Fixture::new();
    let stores = [fixture.make_store("a")];
    let held = StoreLock::new(stores[0].join("immutable"), true).expect("the lock");
    let mut guard = held.acquire(Intent::Read).await.expect("hold the store");
    guard.note_refreshed();

    let refused = try_lock_stores(&stores, Joining::Never, Duration::ZERO).await;
    assert!(matches!(refused, Err(err) if err.is_mounts_busy()));

    let joined = try_lock_stores(&stores, Joining::OwnProcess, Duration::ZERO)
        .await
        .expect("the attempt");
    assert!(matches!(joined, Taken::Locked(_)));
}

/// A store directory that no longer exists is skipped, and isn't made again.
#[tokio::test]
async fn a_store_that_no_longer_exists_is_skipped() {
    let fixture = Fixture::new();
    let gone = fixture.store("gone");

    let taken = try_lock_stores(std::slice::from_ref(&gone), Joining::Never, Duration::ZERO)
        .await
        .expect("the attempt");

    assert!(matches!(taken, Taken::Locked(_)));
    assert!(!gone.exists());
}
