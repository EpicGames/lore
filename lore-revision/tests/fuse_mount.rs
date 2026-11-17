// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! FUSE instances mounted through the service's `MountManager` and its FUSE manager:
//! create, sync, the refusal outside the service, remount after a restart, unmount on stop, and
//! the allocations of the service's lookup of a mounted path.
//!
//! Every test takes `SERIAL`: they share the process-wide `MountManagerState` and the
//! `LORE_GLOBAL_PATH` override, and `unmount_all` acts on every mount the manager holds.
//!
//! A process of its own: the tests initialise the process-wide mount manager, which every
//! repository path resolution in the process consults from then on, and set `LORE_GLOBAL_PATH`
//! while the process runs. `integration` drives a mounted instance through the same manager.
#![cfg(target_os = "linux")]

use std::num::NonZeroU64;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::runtime::LORE_CONTEXT;
use lore_fuse::skip_without_fuse;
use lore_revision::fs::fuse::inode::ROOT_INO;
use lore_revision::fs::fuse::mount_manager::StopHook;
use lore_revision::fs::fuse::mount_manager::StopStep;
use lore_revision::fs::mount_manager::MountManager;
use lore_revision::fs::mount_manager_state::MountManagerState;
use lore_revision::global::external_dir::external_dir_for_instance;
use lore_revision::global::external_dir::resolve::test_util::reads_in;
use lore_revision::global::external_dir::setting::RootId;
use lore_revision::global::external_dir::setting::Setting;
use lore_revision::instance::InstanceId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::DOT_LORE;
use lore_revision::repository::DOT_LOREIGNORE;
use lore_revision::repository::DOT_URC;
use lore_revision::repository::DOT_URCIGNORE;
use lore_revision::repository::RepositoryAccess;
use lore_revision::repository::RepositoryConfig;
use lore_revision::repository::RepositoryError;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::repository::VfsType;
use lore_revision::repository::create_local;
use lore_revision::repository::get_dot_lore_path;
use lore_revision::repository::load_and_connect;
use lore_revision::repository::load_repository_config_from_dot_dir;
use lore_revision::revision::sync::SyncOptions;
use lore_revision::shared_store::get_shared_store_path_for_repo;
use lore_revision::state::State;
use lore_storage::local::store_lock::StoreLock;

#[path = "support/fuse_service.rs"]
mod fuse_service;
#[path = "support/fuse_test_util.rs"]
mod fuse_test_util;
#[path = "helper.rs"]
mod helper;
#[path = "fuse_mount/integration.rs"]
mod integration;
#[path = "fuse_mount/needs_fuse.rs"]
mod needs_fuse;
#[path = "support/xorshift.rs"]
mod xorshift;

use fuse_service::*;
use lore_storage::FlushOptions;

const CHILD_ENV: &str = "LORE_FUSE_MOUNT_TEST_OUTSIDE_SERVICE_CHILD";

/// Whether this process holds the claim on the store `name` of the shared store `config` names,
/// as a command or a mount using it does. A lock opened on a store directory shares that
/// directory's state with every other lock in the process, so it reports what the process holds.
async fn shared_store_held(config: &RepositoryConfig, name: &str) -> bool {
    let shared = get_shared_store_path_for_repo(config)
        .await
        .expect("the shared store resolves")
        .expect("the instance uses a shared store");
    StoreLock::new(shared.join(name), false)
        .expect("a store directory")
        .is_held()
}

/// Runs only as the child of `fuse_instance_outside_service_is_refused`: a process that never
/// initialised a mount manager, as a command outside the service is.
#[tokio::test(flavor = "multi_thread")]
async fn outside_service_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    use_private_global_dir();
    Box::pin(LORE_CONTEXT.scope(setup_test_execution(), async move {
        assert!(MountManagerState::mount_manager().is_none());
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");

        let (_create_dir, create_path) = canonical_dir("lore-fuse-mount-outside-create-");
        let token = RepositoryWriteToken::acquire(&create_path).await;
        let refused = create_local(
            &create_path,
            &token,
            RepositoryId::from(uuid::Uuid::now_v7()),
            lore_base::types::Context::from(uuid::Uuid::now_v7()),
            lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
            shared_store_config(&store, VfsType::Fuse),
            false,
        )
        .await
        .expect_err("creating a FUSE instance outside the service is refused");
        assert!(
            refused.to_string().contains("outside the service"),
            "unexpected refusal: {refused}"
        );
        drop(token);

        // An instance whose on-disk config says FUSE, as one left behind by a stopped service.
        let (_load_dir, load_path) = canonical_dir("lore-fuse-mount-outside-load-");
        let (repository, token) = create_instance(
            &load_path,
            RepositoryId::from(uuid::Uuid::now_v7()),
            shared_store_config(&store, VfsType::None),
        )
        .await;
        drop(repository);
        drop(token);
        lore_revision::util::config::save(
            &shared_store_config(&store, VfsType::Fuse),
            load_path
                .join(DOT_LORE)
                .join(lore_revision::repository::CONFIG),
        )
        .await
        .expect("config rewrites");
        let refused = load_and_connect(&load_path, RepositoryAccess::ReadOnly)
            .await
            .expect_err("loading a FUSE instance outside the service is refused");
        let text = refused.to_string();
        assert!(
            text.contains("Loading a fuse repository without using the service"),
            "unexpected refusal: {text}"
        );
    }))
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fuse_instance_outside_service_is_refused() {
    let exe = std::env::current_exe().expect("test binary path");
    let output = lore_base::lore_spawn_blocking!(move || {
        std::process::Command::new(exe)
            .args(["--exact", "outside_service_child", "--nocapture"])
            .env(CHILD_ENV, "1")
            .env_remove("LORE_GLOBAL_PATH")
            .output()
    })
    .await
    .expect("child task completes")
    .expect("child runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "child failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The repository context the mount at `path` currently serves reads through.
fn served_repository(
    manager: &lore_revision::fs::mount_manager::MountManager,
    path: &Path,
) -> Arc<lore_revision::repository::RepositoryContext> {
    manager
        .fuse()
        .mounted(path)
        .expect("a FUSE mount at the path")
        .filesystem
        .served()
        .repository
        .clone()
}

/// Creates a FUSE instance at `path` on `store`, as a clone does, and answers its failure.
async fn clone_fails(store: &Path, path: &Path) -> Option<RepositoryError> {
    let token = RepositoryWriteToken::acquire(path).await;
    let created = create_local(
        path,
        &token,
        RepositoryId::from(uuid::Uuid::now_v7()),
        lore_base::types::Context::from(uuid::Uuid::from_u128(0x5eed)),
        lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
        shared_store_config(store, VfsType::Fuse),
        false,
    )
    .await;
    created.err()
}

/// Asserts that `error` is the stopping manager's refusal of a mount.
fn assert_refused_by_the_stop(error: Option<RepositoryError>) {
    let error = error.expect("a mount made while the service stops fails");
    let text = error.to_string();
    assert!(
        text.contains("the service is stopping"),
        "not the stop's refusal: {text}"
    );
}

/// The names in `dir`, none when it does not exist.
// A fixture read, outside any repository access.
#[allow(clippy::disallowed_methods)]
fn names_in(dir: &Path) -> std::collections::BTreeSet<std::ffi::OsString> {
    std::fs::read_dir(dir).map_or_default(|entries| {
        entries
            .filter_map(|entry| Some(entry.ok()?.file_name()))
            .collect()
    })
}

/// Lets the shared manager mount again when dropped, so a test that stops it leaves it usable for
/// the tests after it even when one of its assertions fails first.
struct ResumeMounting(Arc<MountManager>);

impl Drop for ResumeMounting {
    fn drop(&mut self) {
        self.0.fuse().resume_mounting();
    }
}

/// The hook a test runs at each step of the shared manager's stops; none outside such a test.
static STOP_HOOK: std::sync::Mutex<Option<StopHook>> = std::sync::Mutex::new(None);

/// Runs `hook` at each step of `manager`'s stops until the answer is dropped.
fn on_stop_steps(manager: &MountManager, hook: StopHook) -> StopHookSet {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        manager.fuse().set_stop_hook(Arc::new(|step| {
            let hook = STOP_HOOK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            Box::pin(async move {
                if let Some(hook) = hook {
                    hook(step).await;
                }
            })
        }));
    });
    *STOP_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    StopHookSet
}

/// Clears the test's stop hook when dropped.
struct StopHookSet;

impl Drop for StopHookSet {
    fn drop(&mut self) {
        *STOP_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// Writes `contents` to `path` through the mount, as a program would.
// A fixture write through the mount, outside any repository write.
#[allow(clippy::disallowed_methods)]
async fn write_through(path: std::path::PathBuf, contents: &'static [u8]) {
    lore_base::lore_spawn_blocking!(move || std::fs::write(&path, contents))
        .await
        .expect("write task completes")
        .expect("the write through the mount succeeds");
}

/// The instance at `path` opened as the service opens it for a move or a delete: without its
/// stores, its directory lock or its write token, which the command takes when it needs them.
/// The context `fresh_fuse_instance` answers holds the write token, so it is dropped first.
async fn opened_as_a_command(
    instance: Arc<lore_revision::repository::RepositoryContext>,
    path: &Path,
) -> Arc<lore_revision::repository::RepositoryContext> {
    drop(instance);
    lore_revision::repository::load_and_connect_with_token(path, RepositoryAccess::NoStore, None)
        .await
        .expect("the instance opens")
}

/// Runs `on_unmounted` where a move has unmounted its instance and has yet to mount it at its
/// target, until the answer is dropped.
fn on_move_unmounted(
    manager: &MountManager,
    on_unmounted: impl Fn() -> lore_revision::BoxFuture<'static, ()> + Send + Sync + 'static,
) -> StopHookSet {
    on_stop_steps(
        manager,
        Arc::new(move |step| {
            if step == StopStep::MoveUnmounted {
                on_unmounted()
            } else {
                Box::pin(async {})
            }
        }),
    )
}

const INITIALIZE_CHILD_ENV: &str = "LORE_FUSE_MOUNT_TEST_INITIALIZE_CHILD";

/// Removes `path` through the mount, as a program would.
// A fixture removal through the mount, outside any repository write.
#[allow(clippy::disallowed_methods)]
async fn remove_through(path: std::path::PathBuf) {
    lore_base::lore_spawn_blocking!(move || std::fs::remove_file(&path))
        .await
        .expect("remove task completes")
        .expect("the removal through the mount succeeds");
}

/// The modification time `path` reports through the mount.
async fn mtime_of(path: std::path::PathBuf) -> std::time::SystemTime {
    lore_base::lore_spawn_blocking!(move || std::fs::metadata(&path)?.modified())
        .await
        .expect("stat task completes")
        .expect("stat through the mount succeeds")
}

/// The name of the file [`synced_fuse_instance`]'s second sync adds, for which that swap records
/// a time.
const SWAPPED: &str = "swapped.txt";

/// A virtual clone at `path` on `store`, synced to a revision of `files` committed from an OS
/// instance, then to one that adds [`SWAPPED`], as syncs swap them in and record them. The first
/// publish onto the empty revision records no time, so every file of `files` reports the base time,
/// and the second swap records one for [`SWAPPED`].
async fn synced_fuse_instance(store: &Path, path: &Path, files: &[(&str, &[u8])]) -> InstanceId {
    let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
    let (repository, token) = create_instance(
        path,
        repository_id,
        shared_store_config(store, VfsType::Fuse),
    )
    .await;
    let twin = Box::pin(Twin::new(store, repository_id)).await;
    let first = Box::pin(twin.commit(files)).await;
    let second = Box::pin(twin.commit(&[(SWAPPED, b"added by the second sync")])).await;
    let mut served = State::new();
    for target in [first, second] {
        let operation = repository
            .file_system()
            .begin_operation()
            .await
            .expect("the FUSE provider begins an operation");
        lore_revision::fs::realize::realize_state(
            repository.clone(),
            repository.clone(),
            operation,
            served,
            target.clone(),
            SyncOptions::default(),
        )
        .await
        .expect("syncing into the virtual clone succeeds");
        // As a sync records the revision it swapped in, which a mount of the instance serves.
        lore_revision::instance::store_current_anchor(&repository, target.revision())
            .await
            .expect("record the current revision");
        served = target;
    }
    repository
        .flush(FlushOptions::default())
        .await
        .expect("flush the anchors");
    let instance_id = repository.instance_id;
    drop(repository);
    drop(token);
    instance_id
}

/// The instances the store of the instance at `path` registers.
async fn instances_registered_beside(path: &Path) -> Vec<InstanceId> {
    let repository = load_and_connect(path, RepositoryAccess::ReadOnly)
        .await
        .expect("the instance opens");
    lore_revision::instance::list_instances(&repository)
        .await
        .expect("list the instances")
        .iter()
        .map(|instance| instance.instance_id)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Plans cloning the instance at `path` to `destination` over the instance opened without its
/// stores, as the command plans it in the service, and opens the instance holding its repository
/// lock alone, as the command then does, for `clone` to clone it with. Finishes cloning whatever
/// `clone` answers.
async fn clone_instance_at(
    path: &Path,
    destination: &Path,
    allow_copy: bool,
    clone: impl AsyncFnOnce(CommandContext, &ClonePlan) -> Result<(), InstanceError>,
) -> Result<Option<InstanceId>, InstanceError> {
    let unlocked = lore_revision::repository::load_and_connect_with_token(
        path,
        RepositoryAccess::NoStore,
        None,
    )
    .await
    .expect("the instance opens without its stores");
    let planned = lore_revision::instance::instance_clone_plan(
        unlocked.clone(),
        &destination.display().to_string(),
        allow_copy,
    )
    .await;
    drop(unlocked);
    let Some(plan) = planned? else {
        return Ok(None);
    };
    let token = RepositoryWriteToken::acquire(path).await;
    let repository = lore_revision::repository::load_and_connect_exclusive(path, token.share())
        .await
        .expect("the instance opens holding its lock alone");
    let cloned = clone(repository, &plan).await;
    drop(token);
    let clone_id = plan.clone_id();
    lore_revision::instance::instance_clone_finish(plan).await;
    cloned.map(|()| Some(clone_id))
}

type CommandContext = Arc<lore_revision::repository::RepositoryContext>;
type ClonePlan = lore_revision::instance::InstanceClonePlan;
type InstanceError = lore_revision::instance::InstanceError;

/// Whether the volume of the external directories clones files, so cloning a virtual clone
/// shares blocks rather than copying them.
fn external_dirs_clone_files() -> bool {
    let root = lore_revision::global::external_dir::external_dirs_root(
        &lore_revision::global::external_dir::resolve::resolve()
            .expect("resolve the external root"),
    )
    .map(std::path::Path::to_path_buf)
    .expect("external root");
    std::fs::create_dir_all(&root).expect("external root");
    lore_io::clone::detect(&root, &root)
        .expect("probe the external directories' volume")
        .backend
        .shares_blocks()
}

/// A start removes what a crash left while cloning among the external directories: a marked
/// external directory no instance is registered for, with the empty mount point its mark names,
/// one a removal renamed aside, and a lock file no clone holds. It keeps a marked one whose
/// instance is registered, removing only its mark, one whose lock another clone holds, and every
/// other entry. Where some instances are unknown it removes no marked directory.
#[tokio::test(flavor = "multi_thread")]
// Fixtures written and cleaned up beside the new instances, outside any repository write.
#[allow(clippy::disallowed_methods)]
async fn a_start_removes_what_interrupted_clones_left_and_keeps_registered_clones() {
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        use lore_base::fs::lock::FSLock;
        use lore_revision::fs::fuse::mount_manager::remove_interrupted_clones;
        use lore_revision::instance::CLONE_IN_PROGRESS_MARK;

        let root = lore_revision::global::external_dir::external_dirs_root(
            &lore_revision::global::external_dir::resolve::resolve()
                .expect("resolve the external root"),
        )
        .map(std::path::Path::to_path_buf)
        .expect("root");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-sweep-target-");
        let marked = |id: InstanceId, names: &Path| {
            let dot = root.join(id.text_encoding()).join(DOT_LORE);
            std::fs::create_dir_all(&dot).expect("a Lore directory");
            test_file_write(
                &dot.join(CLONE_IN_PROGRESS_MARK),
                names.as_os_str().as_encoded_bytes(),
            );
            test_file_write(&root.join(id.text_encoding()).join("deleted"), b"a\n");
        };
        let (unregistered, registered, held, unmarked, stray) = (
            InstanceId::generate(),
            InstanceId::generate(),
            InstanceId::generate(),
            InstanceId::generate(),
            InstanceId::generate(),
        );
        let mount_point = target_parent.join("made");
        std::fs::create_dir(&mount_point).expect("the mount point cloning made");
        let filled = target_parent.join("filled");
        std::fs::create_dir(&filled).expect("a directory holding a file");
        test_file_write(&filled.join("kept.txt"), b"kept");
        marked(unregistered, &mount_point);
        marked(registered, &target_parent.join("registered"));
        marked(held, &filled);
        let held_lock = FSLock::try_acquire_file_lock(root.join(held.text_encoding()))
            .expect("lock")
            .expect("the lock of an instance still being cloned");
        let unmarked_dot = root.join(unmarked.text_encoding()).join(DOT_LORE);
        std::fs::create_dir_all(&unmarked_dot).expect("a Lore directory");
        let aside = root.join(format!(
            "{}.removing",
            InstanceId::generate().text_encoding()
        ));
        std::fs::create_dir_all(aside.join("write")).expect("a removal cut short");
        let stray_lock = root.join(format!("{}.lock", stray.text_encoding()));
        test_file_write(&stray_lock, b"");
        let other = root.join("not-an-instance.removing");
        std::fs::create_dir_all(&other).expect("another entry");

        remove_interrupted_clones(
            &lore_revision::global::external_dir::resolve::resolve()
                .expect("resolve the external root"),
            None,
        )
        .await;
        assert!(
            root.join(unregistered.text_encoding()).exists(),
            "with instances unknown, no marked directory is removed"
        );
        assert!(mount_point.is_dir());
        assert!(!aside.exists(), "a removal cut short is finished");
        assert!(
            !stray_lock.exists(),
            "a lock file no clone holds is removed"
        );

        remove_interrupted_clones(
            &lore_revision::global::external_dir::resolve::resolve()
                .expect("resolve the external root"),
            Some(&std::collections::BTreeSet::from([registered])),
        )
        .await;
        assert!(!root.join(unregistered.text_encoding()).exists());
        assert!(
            !mount_point.exists(),
            "with the empty mount point its mark names"
        );
        let kept = root.join(registered.text_encoding());
        assert!(kept.join("deleted").is_file(), "a registered clone stays");
        assert!(!kept.join(DOT_LORE).join(CLONE_IN_PROGRESS_MARK).exists());
        assert!(
            root.join(held.text_encoding()).join("deleted").is_file(),
            "an instance whose lock another holds stays"
        );
        assert!(filled.join("kept.txt").is_file());
        assert!(unmarked_dot.is_dir(), "an unmarked directory stays");
        assert!(
            !root
                .join(format!("{}.lock", unmarked.text_encoding()))
                .exists(),
            "and gets no lock file"
        );
        assert!(other.is_dir(), "an entry no clone names stays");
        drop(held_lock);
        for left in [
            kept,
            root.join(held.text_encoding()),
            root.join(unmarked.text_encoding()),
            other,
        ] {
            std::fs::remove_dir_all(left).expect("clean up");
        }
        let _ = std::fs::remove_file(root.join(format!("{}.lock", held.text_encoding())));
    }))
    .await;
}

/// Allocations the service's lookup of a mounted FUSE instance's path may make, measured on a
/// mount. `main`'s lookup makes 13, read from its code with `LORE_GLOBAL_PATH` naming the global
/// data directory, as here: the mount path 3, its canonical parent and the join of its name; the
/// global data directory 3, the variable and the join of `data`; the joins of `external`, the
/// instance ID and `.lore` 2 each, a copy and the growth of its push; and the ID's text 1.
const LOOKUP_ALLOCATION_BUDGET: u64 = 6;

struct DisableCountingOnDrop;

impl Drop for DisableCountingOnDrop {
    fn drop(&mut self) {
        lore_base::allocator::set_allocation_counting(false);
    }
}
