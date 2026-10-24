// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What a process loads and keeps of its stores around API calls, with and without
//! `LORE_STORE_RETAIN`.
//!
//! A call loads each store it uses at most once: a store it opens is read under the claim it was
//! opened under, which the call's own claim joins. Without `LORE_STORE_RETAIN` a process keeps
//! nothing after a call unless the call asks for a keep-alive, which keeps the stores and their
//! claims for the window. With it, the process keeps its stores after every call without their
//! claims, and the next call reuses them, loading a store again only if the epoch moved.
//!
//! Its own target: whether a process retains stores is read once per process, so each test runs
//! its scenario in a copy of this binary started with the variable set or removed. Needs no
//! service from compose.yaml, so it is not behind `integration_tests`.
//!
//! [`across_processes`] has processes reload what they keep of a store after another process
//! changes it. [`locks`] has calls take the stores' and the repository's locks in one order,
//! whatever the process kept, and processes running calls on one store at once all finish.

#[path = "store_retention/across_processes.rs"]
mod across_processes;
#[path = "store_retention/locks.rs"]
mod locks;

use std::io::Read;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Duration;
use std::time::Instant;

use lore::call::repository_call_read;
use lore::call::repository_call_write;
use lore::repository;
use lore::storage::close;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::open;
use lore_base::runtime::runtime_flush_guarded;
use lore_base::test_util::TempDir;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Partition;
use lore_revision::event::LoreBytes;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::repository::LoreSharedStoreMode;
use lore_revision::repository::RepositoryError;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::local::store_lock::Intent;
use lore_storage::local::store_lock::StoreLock;

/// Names, for a copy of this binary, the file it writes once its scenario has passed.
const CHILD_DONE: &str = "LORE_STORE_RETENTION_TEST_DONE";

/// How long a copy of this binary gets before it is counted as hung. It bounds a hang, not how
/// long a scenario takes.
const HANG_GUARD: Duration = Duration::from_secs(120);

/// The keep-alive window the calls give, in seconds: longer than any test runs, so a store a call
/// keeps is still kept when the test looks, however loaded the machine. A copy of this binary exits
/// without waiting it out.
const WINDOW_SECONDS: u64 = 60 * 60;

/// The keep-alive window, in seconds, of a call whose test waits for the window to end.
const ENDING_WINDOW_SECONDS: u64 = 1;

/// The remote the repositories and shared stores name. Nothing connects to it.
const REMOTE_URL: &str = "lore://localhost/store-retention";

/// The test `test` of the module of this crate at `module`, as the harness names it: by its path
/// within the crate.
fn harness_name(module: &str, test: &str) -> String {
    let within = module.split_once("::").map_or("", |(_, module)| module);
    format!("{within}::{test}")
}

/// A copy of this binary that runs the test named `test`, with its global config, credentials
/// and service socket in `sandbox`, its stdout piped to this process, and `LORE_STORE_RETAIN`
/// removed. What else the copy is told is the caller's to add.
fn copy_of_this_binary(test: &str, sandbox: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("locate this binary"));
    command
        .args([test, "--exact"])
        .env("LORE_GLOBAL_PATH", sandbox)
        .env("LORE_AUTH_PATH", sandbox)
        .env("LORE_USE_SERVICE", "0")
        .env(
            "LORE_SERVICE_SOCKET",
            format!("lore_service-sr-{}", std::process::id()),
        )
        .env_remove("LORE_STORE_RETAIN")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    command
}

/// Waits for `child` to exit, killing it once it has run for [`HANG_GUARD`], and returns its
/// status with the harness report it wrote.
async fn wait_for_copy(mut child: Child) -> (ExitStatus, String) {
    let hung_at = Instant::now() + HANG_GUARD;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll the copy") {
            break status;
        }
        if Instant::now() >= hung_at {
            let _ = child.kill();
            break child.wait().expect("reap the copy");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    // Read once the copy has exited: the harness report is a few lines, well within the pipe.
    let mut report = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut report);
    }
    (status, report)
}

/// Runs `scenario` where this process is a copy [`copy_of_this_binary`] started, and marks it
/// passed. Elsewhere runs the test named `test` in such a copy, retaining stores if `retain`,
/// and fails unless the copy passes the scenario.
async fn in_child<F: Future<Output = ()>>(test: &str, retain: bool, scenario: impl FnOnce() -> F) {
    if let Some(done) = std::env::var_os(CHILD_DONE) {
        scenario().await;
        std::fs::write(done, b"").expect("marks the scenario passed");
        return;
    }

    let sandbox = TempDir::new("lore-store-retention-");
    let done = sandbox.path().join("done");
    let mut copy = copy_of_this_binary(test, sandbox.path());
    copy.env(CHILD_DONE, &done);
    if retain {
        copy.env("LORE_STORE_RETAIN", "1");
    }
    let child = copy.spawn().expect("spawn a copy of this binary");
    let (status, report) = wait_for_copy(child).await;
    assert!(
        status.success(),
        "the copy running {test} failed: {status}\n{report}"
    );
    assert!(done.exists(), "the copy ran the scenario of {test}");
}

/// Globals for calls against `repository`: local and offline, asking for a keep-alive if
/// `keep_alive`, with a window of [`WINDOW_SECONDS`].
fn globals(repository: &Path, keep_alive: bool) -> LoreGlobalArgs {
    windowed_globals(repository, keep_alive, WINDOW_SECONDS)
}

/// As [`globals`], with a keep-alive window of `window_seconds`.
fn windowed_globals(repository: &Path, keep_alive: bool, window_seconds: u64) -> LoreGlobalArgs {
    LoreGlobalArgs {
        repository_path: LoreString::from(repository.to_str().expect("a UTF-8 path")),
        offline: 1,
        local: 1,
        identity: "store-retention".into(),
        store_keep_alive: u8::from(keep_alive),
        store_keep_alive_seconds: window_seconds,
        ..LoreGlobalArgs::default()
    }
}

/// Creates an offline repository at `path` with stores of its own, and returns them. The creation
/// keeps nothing past [`ENDING_WINDOW_SECONDS`], so a test that waits for the stores to go is
/// waiting for its own call's window.
async fn create_repository(path: &Path) -> Stores {
    create_repository_over(path, None).await;
    Stores::of(path)
}

/// Creates an offline repository at `path` over the shared store at `shared`, or with stores of
/// its own, keeping nothing past [`ENDING_WINDOW_SECONDS`].
async fn create_repository_over(path: &Path, shared: Option<&Path>) {
    let status = repository::create(
        windowed_globals(path, false, ENDING_WINDOW_SECONDS),
        repository::LoreRepositoryCreateArgs {
            repository_url: REMOTE_URL.into(),
            description: LoreString::default(),
            id: LoreString::default(),
            use_shared_store: if shared.is_some() {
                LoreSharedStoreMode::Enabled
            } else {
                LoreSharedStoreMode::Disabled
            },
            shared_store_path: shared.map_or_else(LoreString::default, |shared| {
                LoreString::from(shared.to_str().expect("a UTF-8 path"))
            }),
            vfs: Default::default(),
        },
        None,
    )
    .await;
    assert_eq!(status, 0, "creating the repository at {}", path.display());
}

/// Locks on a repository's two store directories, held for a scenario so the directories' load
/// counts last across the calls it makes. They take no flock of their own.
struct Stores([Arc<StoreLock>; 2]);

impl Stores {
    fn of(repository: &Path) -> Self {
        let dot = lore_revision::repository::get_dot_lore_path(repository).expect("a repository");
        Self(
            ["immutable", "mutable"]
                .map(|store| StoreLock::new(dot.join(store), false).expect("a store directory")),
        )
    }

    /// The loads of the immutable and the mutable store so far.
    fn loads(&self) -> [usize; 2] {
        self.0.each_ref().map(|lock| lock.loads())
    }

    /// Whether this process holds the immutable and the mutable store's flock.
    fn held(&self) -> [bool; 2] {
        self.0.each_ref().map(|lock| lock.is_held())
    }

    /// Runs `call`, lets the flush it leaves behind finish, and returns the loads it caused.
    async fn loads_of(&self, call: impl Future<Output = ()>) -> [usize; 2] {
        let before = self.loads();
        call.await;
        runtime_flush_guarded().await;
        let after = self.loads();
        [after[0] - before[0], after[1] - before[1]]
    }
}

/// The stores a call used, as weak references.
struct Seen {
    immutable: Weak<dyn ImmutableStore>,
    mutable: Weak<dyn MutableStore>,
}

/// Runs one call against `repository` that writes nothing, asking for a keep-alive of
/// `window_seconds` if `keep_alive`, and returns the stores it used.
async fn call(repository: &Path, keep_alive: bool, window_seconds: u64) -> Seen {
    let slot: Arc<Mutex<Option<Seen>>> = Arc::default();
    let sink = Arc::clone(&slot);
    let status = repository_call_write(
        windowed_globals(repository, keep_alive, window_seconds),
        None,
        (),
        "store_retention",
        move |context, _token, ()| async move {
            let mutable = context
                .try_mutable_store_arc()
                .expect("a write call has its mutable store");
            *sink.lock().expect("the slot") = Some(Seen {
                immutable: Arc::downgrade(&context.immutable_store()),
                mutable: Arc::downgrade(&mutable),
            });
            Ok::<(), RepositoryError>(())
        },
    )
    .await;
    assert_eq!(status, 0, "the call succeeds");
    let seen = slot.lock().expect("the slot").take();
    seen.expect("the call ran")
}

/// Runs a read call against `repository`, asking for no keep-alive.
async fn read(repository: &Path) {
    let status = repository_call_read(
        globals(repository, false),
        None,
        (),
        "store_retention",
        |_context, ()| async { Ok::<(), RepositoryError>(()) },
    )
    .await;
    assert_eq!(status, 0, "the read call succeeds");
}

/// Stages every file in `repository`.
async fn stage(repository: &Path) {
    let status = lore::file::stage(
        globals(repository, false),
        lore::file::LoreFileStageArgs {
            paths: LoreArray::from_vec(vec![LoreString::from(".")]),
            case_change: 0,
            scan: 1,
        },
        None,
    )
    .await;
    assert_eq!(status, 0, "staging");
}

/// Commits what is staged in `repository`.
async fn commit(repository: &Path) {
    let status = lore::revision::commit(
        globals(repository, false),
        lore::revision::LoreRevisionCommitArgs {
            message: "store retention".into(),
            ..Default::default()
        },
        None,
    )
    .await;
    assert_eq!(status, 0, "committing");
}

/// Opens a storage handle on `repository`.
async fn open_handle(repository: &Path) -> LoreStore {
    let opened: Arc<Mutex<Option<u64>>> = Arc::default();
    let sink = Arc::clone(&opened);
    let status = open::open(
        globals(repository, false),
        open::LoreStorageOpenArgs {
            repository_path: LoreString::from(repository.to_str().expect("a UTF-8 path")),
            ..Default::default()
        },
        Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageOpened(data) = event {
                *sink.lock().expect("the slot") = Some(data.handle_id);
            }
        })),
    )
    .await;
    assert_eq!(status, 0, "opening a storage handle");
    let handle_id = opened.lock().expect("the slot").take();
    LoreStore {
        handle_id: handle_id.expect("the open reported its handle"),
    }
}

/// Closes the storage handle `store` on `repository`.
async fn close_handle(repository: &Path, store: LoreStore) {
    let status = close::close(
        globals(repository, false),
        close::LoreStorageCloseArgs { handle: store },
        None,
    )
    .await;
    assert_eq!(status, 0, "closing the storage handle");
}

/// The partition the storage handle's items are put in.
fn partition() -> Partition {
    Partition::from([0x5au8; 16])
}

/// Puts `payload` through the storage handle `store` and returns its address.
async fn put(repository: &Path, store: LoreStore, payload: &[u8]) -> Address {
    let address: Arc<Mutex<Option<Address>>> = Arc::default();
    let sink = Arc::clone(&address);
    let status = lore::storage::put::put(
        globals(repository, false),
        lore::storage::put::LoreStoragePutArgs {
            handle: store,
            items: LoreArray::from_vec(vec![lore::storage::put::LoreStoragePutItem {
                id: 1,
                partition: partition(),
                context: Context::default(),
                data: LoreBytes {
                    ptr: payload.as_ptr().cast(),
                    len: payload.len(),
                },
                remote_write: 0,
                local_cache: 0,
                fixed_size_chunk: 0,
            }]),
        },
        Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StoragePutItemComplete(data) = event
                && data.error.error_code == 0
            {
                *sink.lock().expect("the slot") = Some(data.address);
            }
        })),
    )
    .await;
    assert_eq!(status, 0, "putting through the storage handle");
    let address = address.lock().expect("the slot").take();
    address.expect("the put stored its item")
}

/// The content at `address`, read through the storage handle `store`.
async fn get(repository: &Path, store: LoreStore, address: Address) -> Vec<u8> {
    let content: Arc<Mutex<Vec<u8>>> = Arc::default();
    let sink = Arc::clone(&content);
    let status = lore::storage::get::get(
        globals(repository, false),
        lore::storage::get::LoreStorageGetArgs {
            handle: store,
            items: LoreArray::from_vec(vec![lore::storage::get::LoreStorageGetItem {
                id: 1,
                partition: partition(),
                address,
                ..Default::default()
            }]),
        },
        Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageGetData(data) = event
                && data.bytes.len > 0
            {
                // SAFETY: the bytes of a `GET_DATA` event are valid for the callback.
                let bytes = unsafe {
                    std::slice::from_raw_parts(data.bytes.ptr.cast::<u8>(), data.bytes.len)
                };
                sink.lock().expect("the slot").extend_from_slice(bytes);
            }
        })),
    )
    .await;
    assert_eq!(status, 0, "reading through the storage handle");
    std::mem::take(&mut *content.lock().expect("the slot"))
}

/// Waits for `condition`, with no limit of its own: [`HANG_GUARD`] bounds the copy running it.
async fn until(mut condition: impl AsyncFnMut() -> bool) {
    while !condition().await {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Asserts that `loads` counts at most one load of each store, for the call named `call`.
fn at_most_once(call: &str, loads: [usize; 2]) {
    assert!(
        loads[0] <= 1 && loads[1] <= 1,
        "{call} loads each store at most once, loaded the immutable store {} times and the \
         mutable store {}",
        loads[0],
        loads[1]
    );
}

/// A read call, a stage, a commit, and a storage handle's open, put, get and close each load
/// each store at most once.
async fn each_call_loads_each_store_at_most_once() {
    let dir = TempDir::new("lore-store-retention-loads-");
    let stores = create_repository(dir.path()).await;

    at_most_once("a read call", stores.loads_of(read(dir.path())).await);

    std::fs::write(dir.path().join("file.txt"), b"store retention").expect("writes a file");
    at_most_once("a stage", stores.loads_of(stage(dir.path())).await);
    at_most_once("a commit", stores.loads_of(commit(dir.path())).await);

    let loads = stores
        .loads_of(async {
            let store = open_handle(dir.path()).await;
            let payload = b"through a storage handle";
            let address = put(dir.path(), store, payload).await;
            assert_eq!(get(dir.path(), store, address).await, payload);
            close_handle(dir.path(), store).await;
        })
        .await;
    at_most_once("a storage handle from open to close", loads);
}

/// **Each call loads each store at most once** in a process that does not retain stores: a store
/// a call opens is current at the call's claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_call_loads_each_store_at_most_once_without_retention() {
    in_child(
        "each_call_loads_each_store_at_most_once_without_retention",
        false,
        each_call_loads_each_store_at_most_once,
    )
    .await;
}

/// **Each call loads each store at most once** in a process that retains stores.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_call_loads_each_store_at_most_once_with_retention() {
    in_child(
        "each_call_loads_each_store_at_most_once_with_retention",
        true,
        each_call_loads_each_store_at_most_once,
    )
    .await;
}

/// **A call that asks for a keep-alive keeps its stores claimed for the window**, in a process
/// that does not retain stores: the flocks stay held once the call's own flush has let its
/// claims go.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_keep_alive_holds_the_stores_claims_without_retention() {
    in_child(
        "a_keep_alive_holds_the_stores_claims_without_retention",
        false,
        || async {
            let dir = TempDir::new("lore-store-retention-keep-alive-");
            let stores = create_repository(dir.path()).await;

            let seen = call(dir.path(), true, WINDOW_SECONDS).await;
            runtime_flush_guarded().await;
            assert_eq!(
                stores.held(),
                [true, true],
                "the keep-alive holds both stores' flocks past the call's flush"
            );
            assert!(seen.immutable.upgrade().is_some(), "and the stores");
        },
    )
    .await;
}

/// **A keep-alive lets the stores' claims go when its window ends**, in a process that does not
/// retain stores.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_keep_alive_lets_the_claims_go_when_its_window_ends_without_retention() {
    in_child(
        "a_keep_alive_lets_the_claims_go_when_its_window_ends_without_retention",
        false,
        || async {
            let dir = TempDir::new("lore-store-retention-keep-alive-end-");
            let stores = create_repository(dir.path()).await;

            let seen = call(dir.path(), true, ENDING_WINDOW_SECONDS).await;
            until(async || {
                stores.held() == [false, false]
                    && seen.immutable.upgrade().is_none()
                    && seen.mutable.upgrade().is_none()
            })
            .await;
        },
    )
    .await;
}

/// **A process that does not retain stores keeps none after a call that asks for no
/// keep-alive.**
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_without_a_keep_alive_keeps_nothing_without_retention() {
    in_child(
        "a_call_without_a_keep_alive_keeps_nothing_without_retention",
        false,
        || async {
            let dir = TempDir::new("lore-store-retention-none-");
            let stores = create_repository(dir.path()).await;

            let seen = call(dir.path(), false, WINDOW_SECONDS).await;
            runtime_flush_guarded().await;
            assert!(
                seen.immutable.upgrade().is_none() && seen.mutable.upgrade().is_none(),
                "no store outlives the call's flush"
            );
            assert_eq!(stores.held(), [false, false], "and no flock");
        },
    )
    .await;
}

/// **A process that retains stores keeps them after a call, without their claims**, although the
/// call asked for no keep-alive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retaining_process_keeps_its_stores_unclaimed_for_the_window() {
    in_child(
        "a_retaining_process_keeps_its_stores_unclaimed_for_the_window",
        true,
        || async {
            let dir = TempDir::new("lore-store-retention-window-");
            let stores = create_repository(dir.path()).await;

            let seen = call(dir.path(), false, WINDOW_SECONDS).await;
            runtime_flush_guarded().await;
            assert_eq!(
                stores.held(),
                [false, false],
                "no flock is kept past the call's flush"
            );
            assert!(
                seen.immutable.upgrade().is_some() && seen.mutable.upgrade().is_some(),
                "but both stores are"
            );
        },
    )
    .await;
}

/// **A process that retains stores lets them go when the window ends.**
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retaining_process_lets_its_stores_go_when_the_window_ends() {
    in_child(
        "a_retaining_process_lets_its_stores_go_when_the_window_ends",
        true,
        || async {
            let dir = TempDir::new("lore-store-retention-window-end-");
            create_repository(dir.path()).await;

            let seen = call(dir.path(), false, ENDING_WINDOW_SECONDS).await;
            until(async || seen.immutable.upgrade().is_none() && seen.mutable.upgrade().is_none())
                .await;
        },
    )
    .await;
}

/// **A process that retains stores reuses a kept store at the next call without loading it,
/// and loads it once if another store moved its epoch.** The test holds the stores between the
/// calls, so what it checks is the epoch rather than the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retaining_process_reloads_a_kept_store_only_when_its_epoch_moved() {
    in_child(
        "a_retaining_process_reloads_a_kept_store_only_when_its_epoch_moved",
        true,
        || async {
            let dir = TempDir::new("lore-store-retention-epoch-");
            let stores = create_repository(dir.path()).await;

            let seen = call(dir.path(), false, WINDOW_SECONDS).await;
            let kept = (
                seen.immutable
                    .upgrade()
                    .expect("the immutable store is kept"),
                seen.mutable.upgrade().expect("the mutable store is kept"),
            );
            runtime_flush_guarded().await;

            assert_eq!(
                stores.loads_of(read(dir.path())).await,
                [0, 0],
                "the next call reuses both stores without loading them"
            );

            let elsewhere = StoreLock::new(
                lore_revision::repository::get_dot_lore_path(dir.path())
                    .expect("a repository")
                    .join("immutable"),
                false,
            )
            .expect("a store directory");
            drop(
                elsewhere
                    .acquire(Intent::Write)
                    .await
                    .expect("advances the epoch"),
            );
            assert_eq!(
                stores.loads_of(read(dir.path())).await,
                [1, 0],
                "the call after another store moved the immutable store's epoch loads that \
                 store once"
            );
            drop(kept);
        },
    )
    .await;
}

/// **A process that retains stores keeps a closed storage handle's stores, without their
/// claims, and the next open reuses them without loading them.**
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retaining_process_keeps_a_closed_handles_stores_for_the_next_open() {
    in_child(
        "a_retaining_process_keeps_a_closed_handles_stores_for_the_next_open",
        true,
        || async {
            let dir = TempDir::new("lore-store-retention-handle-");
            let stores = create_repository(dir.path()).await;

            let first = open_handle(dir.path()).await;
            let immutable = Arc::downgrade(&handle::immutable_for_test(first).expect("open"));
            let mutable = Arc::downgrade(&handle::mutable_for_test(first).expect("open"));
            close_handle(dir.path(), first).await;
            runtime_flush_guarded().await;
            assert_eq!(
                stores.held(),
                [false, false],
                "the close's flush lets the flocks go"
            );
            let kept = (
                immutable.upgrade().expect("the immutable store is kept"),
                mutable.upgrade().expect("the mutable store is kept"),
            );

            let loads = stores
                .loads_of(async {
                    let second = open_handle(dir.path()).await;
                    close_handle(dir.path(), second).await;
                })
                .await;
            assert_eq!(loads, [0, 0], "the next open reuses both stores");
            drop(kept);
        },
    )
    .await;
}
