// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Listing the registered shared stores beside the Lore service and beside other processes that
//! hold a store.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use lore::interface::LoreEvent;
use lore::interface::LoreEventCallback;
use lore::shared_store::LoreSharedStoreListArgs;
use lore_base::env::CallEnvironment;
use lore_base::log::LoreLogLevel;
use lore_base::test_util::TempDir;
use lore_revision::global::external_dir::locks::Taken;
use lore_revision::instance::InstanceId;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::shared_store::registry::SharedStoreRegistryEntry;
use lore_storage::FlushOptions;

use crate::test_util::hold_store;
use crate::test_util::listed_path;
use crate::test_util::register;

/// The time a listing that must not wait for a held store is given before the test fails.
const PATIENCE: Duration = Duration::from_secs(30);

/// How long another process holds a store for a moment, well within
/// [`lore::shared_store::HELD_STORE_WAIT`].
const MOMENT: Duration = Duration::from_millis(500);

/// The status the service socket answers a relayed listing with, which no listing here returns.
const RELAYED_STATUS: i32 = 17;

/// What a listing delivered: each store's path with its count of instances, and the warnings.
#[derive(Default)]
struct Listed {
    stores: Vec<(String, usize)>,
    warnings: Vec<String>,
}

/// The environment of a caller whose `LORE_GLOBAL_PATH` is `global`.
fn environment(global: &Path) -> CallEnvironment {
    CallEnvironment {
        values: [Some(global.display().to_string()), None, None],
    }
}

/// Lists the shared stores, with their instances where `include_instances` is set, for a caller
/// whose `LORE_GLOBAL_PATH` is `global`, and returns the call's exit code and what it delivered.
async fn list_in(global: &Path, include_instances: bool) -> (i32, Listed) {
    let listed = Arc::new(Mutex::new(Listed::default()));
    let recorder = Arc::clone(&listed);
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        let mut listed = recorder.lock().expect("the collector lock is not poisoned");
        match event {
            LoreEvent::SharedStoreList(data) => listed.stores.extend(
                data.stores
                    .as_slice()
                    .iter()
                    .map(|store| (store.store_path.to_string(), store.instance_ids.len())),
            ),
            LoreEvent::Log(data) if data.level == LoreLogLevel::Warn => {
                listed.warnings.push(data.message.to_string());
            }
            _ => {}
        }
    }));
    let code = lore::call::with_relayed_environment(
        environment(global),
        lore::shared_store::list(
            LoreGlobalArgs::default(),
            LoreSharedStoreListArgs {
                include_instances: u8::from(include_instances),
            },
            callback,
        ),
    )
    .await;
    let listed = std::mem::take(&mut *listed.lock().expect("the collector lock is not poisoned"));
    (code, listed)
}

/// Registers an instance at `instance` in the shared store in `store`, a directory that holds or
/// is to hold a `shared_store` directory, for a caller whose `LORE_GLOBAL_PATH` is `global`, and
/// flushes the store, as Lore does once it registers an instance.
async fn register_instance(global: &Path, store: &Path, instance: &Path) {
    let entry = SharedStoreRegistryEntry::in_directory(store.display().to_string());
    let instance = instance.display().to_string();
    let code = lore::call::with_relayed_environment(
        environment(global),
        lore::call::no_repository_call(
            LoreGlobalArgs::default(),
            None,
            (),
            "register_instance",
            |()| async move {
                let repository = entry.create_null_repository_context().await?;
                lore_revision::instance::register_instance(
                    &repository,
                    InstanceId::generate(),
                    &instance,
                )
                .await
                .expect("register the instance");
                repository.flush(FlushOptions::default()).await
            },
        ),
    )
    .await;
    assert_eq!(code, 0, "the instance is registered");
}

/// A listing with instances lists each store another process holds without its instances, with a
/// warning naming it, rather than waiting for it, and waits for every held store together for no
/// longer than it waits for one. It lists a store no process holds, after that wait, with its
/// instances and without a warning.
#[tokio::test]
async fn a_listing_reports_the_stores_another_process_holds_rather_than_waiting() {
    let _socket = crate::SERVICE_SOCKET_TESTS.lock().await;
    let global = TempDir::new("lore-shared-store-list-held-");
    let stores = [
        global.child("first"),
        global.child("second"),
        global.child("free"),
    ];
    let _holds = [hold_store(&stores[0]).await, hold_store(&stores[1]).await];
    register_instance(global.path(), &stores[2], &global.child("instance")).await;
    register(global.path(), &[&stores[0], &stores[1], &stores[2]]);

    let started = tokio::time::Instant::now();
    let (code, listed) = tokio::time::timeout(PATIENCE, list_in(global.path(), true))
        .await
        .expect("the listing doesn't wait for the stores held");
    let waited = started.elapsed();

    assert_eq!(code, 0);
    let paths: Vec<String> = stores.iter().map(|store| listed_path(store)).collect();
    assert_eq!(
        listed.stores,
        vec![
            (paths[0].clone(), 0),
            (paths[1].clone(), 0),
            (paths[2].clone(), 1),
        ],
        "each store is listed, the free one with its instance"
    );
    for path in &paths[..2] {
        assert!(
            listed.warnings.iter().any(|warning| warning.contains(path)),
            "a warning names the store {path} held: {:?}",
            listed.warnings
        );
    }
    assert!(
        !listed
            .warnings
            .iter()
            .any(|warning| warning.contains(&paths[2])),
        "no warning names the store no process holds: {:?}",
        listed.warnings
    );
    assert!(
        waited < 2 * lore::shared_store::HELD_STORE_WAIT,
        "the listing waited {waited:?} for two stores held, as long as for each in turn"
    );
}

/// A listing with instances waits for a store another process holds for a moment, and lists it with
/// its instances once that process lets it go, without a warning.
#[tokio::test]
async fn a_listing_waits_for_a_store_held_for_a_moment() {
    let _socket = crate::SERVICE_SOCKET_TESTS.lock().await;
    let global = TempDir::new("lore-shared-store-list-moment-");
    let store = global.child("store");
    register_instance(global.path(), &store, &global.child("instance")).await;
    register(global.path(), &[&store]);
    let hold = hold_store(&store).await;
    let release = async move {
        tokio::time::sleep(MOMENT).await;
        drop(hold);
    };

    let ((code, listed), ()) = tokio::time::timeout(PATIENCE, async {
        tokio::join!(list_in(global.path(), true), release)
    })
    .await
    .expect("the listing completes");

    assert_eq!(code, 0);
    let path = listed_path(&store);
    assert_eq!(
        listed.stores,
        vec![(path.clone(), 1)],
        "the store is listed with its instance"
    );
    assert!(
        !listed
            .warnings
            .iter()
            .any(|warning| warning.contains(&path)),
        "no warning names the store: {:?}",
        listed.warnings
    );
}

/// Locking a store no other process holds takes nothing off a listing's wait, which counts only
/// the time spent on a store held elsewhere.
#[tokio::test]
async fn locking_a_store_no_process_holds_takes_nothing_off_the_wait() {
    let directory = TempDir::new("lore-shared-store-lock-free-");
    let store = directory.child("store");
    drop(hold_store(&store).await);
    let entry = SharedStoreRegistryEntry::in_directory(store.display().to_string());
    let mut wait = lore::shared_store::HELD_STORE_WAIT;

    let taken = lore::shared_store::lock_shared_store(&entry, &mut wait)
        .await
        .expect("lock the store");

    assert!(matches!(taken, Taken::Locked(_)), "the store is locked");
    assert_eq!(
        wait,
        lore::shared_store::HELD_STORE_WAIT,
        "the wait is left whole"
    );
}

/// A listing with instances asks a service that answers on the service socket once, and lists
/// here when the socket hands it back during a move of the external root. A listing without
/// instances lists here, without asking the socket.
#[tokio::test]
async fn a_listing_with_instances_asks_a_running_service_once_and_lists_here_when_handed_back() {
    let _socket = crate::SERVICE_SOCKET_TESTS.lock().await;
    let replier = crate::test_util::Replier::move_reply(lore::remote::service_socket_name());
    let global = TempDir::new("lore-shared-store-list-handed-back-");
    let store = global.child("store");
    register(global.path(), &[&store]);

    let (code, listed) = list_in(global.path(), false).await;
    assert_eq!(code, 0);
    assert_eq!(listed.stores.len(), 1, "the store is listed");
    assert_eq!(replier.answered(), 0, "the listing doesn't ask the socket");

    let (code, listed) = tokio::time::timeout(PATIENCE, list_in(global.path(), true))
        .await
        .expect("the listing handed back completes");
    assert_eq!(code, 0);
    assert_eq!(
        listed.stores,
        vec![(listed_path(&store), 0)],
        "the listing handed back lists here"
    );
    assert_eq!(replier.answered(), 1, "the listing asks the socket once");
}

/// A listing with instances that a service runs, answering on the service socket, returns the
/// service's status and lists nothing here.
#[tokio::test]
async fn a_listing_with_instances_returns_the_status_of_the_service_that_ran_it() {
    let _socket = crate::SERVICE_SOCKET_TESTS.lock().await;
    let mut reply = Vec::new();
    lore::remote::message::write_payload(
        &mut reply,
        &lore::remote::message::encode_message(&lore::remote::message::MessageToClient::<
            lore_revision::event::LoreEvent,
        >::ApiResult(RELAYED_STATUS)),
    )
    .expect("frame the reply");
    let replier =
        crate::test_util::Replier::service(lore::remote::service_socket_name(), vec![reply]);
    let global = TempDir::new("lore-shared-store-list-relayed-");
    register(global.path(), &[&global.child("store")]);

    let (code, listed) = tokio::time::timeout(PATIENCE, list_in(global.path(), true))
        .await
        .expect("the relayed listing completes");
    assert_eq!(code, RELAYED_STATUS, "the service's status");
    assert!(listed.stores.is_empty(), "nothing is listed here");
    assert_eq!(replier.answered(), 1, "the listing asks the socket once");
}
