// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A move of the external root by rename, and a first root's creation, in a global data directory
//! of the test's own, with no service. A store's lock that the test takes through an `FSLock` of
//! its own contends as another process's would.
// Fixtures plant roots, external directories and stores directly, outside any repository.
#![allow(clippy::disallowed_methods)]

use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;

use lore_base::fs::lock::FSLock;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_revision::global::external_dir::marker::Marker;
use lore_revision::global::external_dir::moving::*;
use lore_revision::global::external_dir::record::MoveKind;
use lore_revision::global::external_dir::record::MovePhase;
use lore_revision::global::external_dir::record::MoveRecord;
use lore_revision::global::external_dir::recovery::Recovered;
use lore_revision::global::external_dir::resolve::Availability;
use lore_revision::global::external_dir::resolve::resolve_in;
use lore_revision::global::external_dir::setting::RootId;
use lore_revision::global::external_dir::setting::Setting;
use lore_revision::instance::InstanceId;
use lore_revision::shared_store::SHARED_STORE_DIR;
use lore_revision::shared_store::shared_store_subdir_for_remote;
use lore_revision::test_util::MoveStep;
use lore_revision::test_util::step_moves;

use crate::helper::execution_in;
use crate::helper::verified_temp_dir;

/// The remote every planted config names.
const REMOTE: &str = "lore://example.invalid/repository";

/// A step a move reached, with the sender the test answers it on.
type Reached = (MoveStep, tokio::sync::oneshot::Sender<bool>);

/// A global data directory, a directory beside it for configured roots, and one for shared
/// stores.
pub(crate) struct Fixture {
    pub(crate) global: TempDir,
    pub(crate) volume: TempDir,
    pub(crate) stores: TempDir,
}

impl Fixture {
    pub(crate) fn new() -> Fixture {
        Fixture {
            global: verified_temp_dir("lore-external-move-global-"),
            volume: verified_temp_dir("lore-external-move-volume-"),
            stores: verified_temp_dir("lore-external-move-stores-"),
        }
    }

    pub(crate) fn global(&self) -> &Path {
        self.global.path()
    }

    pub(crate) fn default_location(&self) -> PathBuf {
        self.global.path().join("external")
    }

    /// A root named `name` beside the global data directory, not yet created.
    pub(crate) fn root(&self, name: &str) -> PathBuf {
        self.volume.path().join(name)
    }

    pub(crate) fn setting(&self) -> Option<Setting> {
        Setting::load(self.global()).expect("read the setting")
    }

    pub(crate) fn record(&self) -> Option<MoveRecord> {
        MoveRecord::load(self.global()).expect("read the record")
    }

    /// The exclusive access a move holds, which no other holder has.
    pub(crate) fn exclusive(&self) -> FSLock {
        lore_revision::global::external_dir::access::try_exclusive_for_move(self.global())
            .expect("try exclusive access")
            .expect("no other holder")
    }

    pub(crate) fn availability(&self) -> Availability {
        resolve_in(self.global().to_path_buf()).availability
    }

    /// A default root holding an external directory with a file in it, which answers the
    /// external directory's name.
    pub(crate) fn default_root_with_an_instance(&self) -> String {
        let name = InstanceId::generate().text_encoding();
        std::fs::create_dir_all(self.default_location().join(&name)).expect("the default root");
        std::fs::write(self.default_location().join(&name).join("file"), "kept").expect("a file");
        name
    }

    /// Plants a virtual instance's external directory in `root` whose config names a shared
    /// store, which is made, and answers the store and the instance.
    pub(crate) fn plant_virtual_instance(&self, root: &Path) -> (PathBuf, InstanceId) {
        let instance = InstanceId::generate();
        let dot_lore = root.join(instance.text_encoding()).join(".lore");
        std::fs::create_dir_all(&dot_lore).expect("the external directory");
        std::fs::write(
            dot_lore.join("config.toml"),
            format!(
                "remote_url = \"{REMOTE}\"\n[shared_store_to_use]\nuse_shared_store = true\n\
                 shared_store_path = {:?}\n",
                self.stores.path()
            ),
        )
        .expect("the config");
        let store = self
            .stores
            .path()
            .join(shared_store_subdir_for_remote(REMOTE))
            .join(SHARED_STORE_DIR);
        std::fs::create_dir_all(store.join("immutable")).expect("the immutable store");
        std::fs::create_dir_all(store.join("mutable")).expect("the mutable store");
        (store, instance)
    }

    /// Moves the root to `target`, in a call of the test's own, with no termination.
    pub(crate) async fn move_to(&self, target: &Path) -> Result<Moved, MoveError> {
        self.move_with(target, std::future::pending()).await
    }

    pub(crate) async fn move_with(
        &self,
        target: &Path,
        termination: impl Future<Output = ()>,
    ) -> Result<Moved, MoveError> {
        LORE_CONTEXT
            .scope(
                execution_in(self.stores.path()),
                move_root(
                    self.global(),
                    target,
                    &self.exclusive(),
                    termination,
                    |_store| {},
                    |_name| {},
                ),
            )
            .await
    }
}

/// Answers every step `steps` reports with `true`, calling `at` with each first.
#[cfg(target_os = "linux")]
fn answer_steps(
    mut steps: tokio::sync::mpsc::UnboundedReceiver<Reached>,
    mut at: impl FnMut(MoveStep) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some((step, answer)) = steps.recv().await {
            at(step);
            let _ = answer.send(true);
        }
    })
}

/// A move by rename from the default location, between two configured roots, and back: the
/// external directories move with the root, the setting names it, and the redirect leads to it.
#[tokio::test]
async fn a_rename_moves_the_root_between_roots_and_back() {
    let fixture = Fixture::new();
    let name = fixture.default_root_with_an_instance();
    let (first, second) = (fixture.root("first"), fixture.root("second"));

    for target in [&first, &second] {
        let moved = fixture.move_to(target).await.expect("the move");
        assert_eq!(moved.kind, Some(MoveKind::Rename));
        assert_eq!(
            fixture.setting().expect("the setting").path,
            target.display().to_string()
        );
        assert_eq!(
            fixture.availability(),
            Availability::Available(target.clone())
        );
        assert_eq!(
            std::fs::read_to_string(target.join(&name).join("file")).expect("moved"),
            "kept"
        );
        assert_eq!(
            std::fs::read_link(fixture.default_location()).expect("the redirect"),
            *target
        );
        assert!(fixture.record().is_none());
    }
    assert!(!first.exists());

    let moved = fixture
        .move_to(&fixture.default_location())
        .await
        .expect("the move back");
    assert_eq!(moved.kind, Some(MoveKind::Rename));
    assert!(fixture.setting().expect("the setting").path.is_empty());
    assert!(
        !std::fs::symlink_metadata(fixture.default_location())
            .expect("the root")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(fixture.default_location().join(&name).join("file")).expect("back"),
        "kept"
    );
    assert!(!second.exists());
}

/// A target equal to the current root succeeds with nothing moved.
#[tokio::test]
async fn a_move_to_the_current_root_moves_nothing() {
    let fixture = Fixture::new();
    fixture.default_root_with_an_instance();

    let moved = fixture
        .move_to(&fixture.default_location())
        .await
        .expect("the move");

    assert_eq!(moved.kind, None);
    assert!(fixture.setting().is_none());
    assert!(fixture.record().is_none());
}

/// With no default root, a move creates the target as the first root.
#[tokio::test]
async fn a_move_with_no_default_root_creates_the_target() {
    let fixture = Fixture::new();
    let target = fixture.root("parent").join("first");

    let moved = fixture.move_to(&target).await.expect("the move");

    assert_eq!(moved.kind, Some(MoveKind::Create));
    assert_eq!(
        fixture.availability(),
        Availability::Available(target.clone())
    );
    assert_eq!(
        std::fs::read_link(fixture.default_location()).expect("the redirect"),
        target
    );
}

/// A root moved by rename gets access for its owner only.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_root_moved_by_rename_gets_owner_only_access() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fixture.default_root_with_an_instance();
    let first = fixture.root("first");
    fixture.move_to(&first).await.expect("the first move");
    std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert_eq!(
        fixture.availability(),
        Availability::Available(first.clone())
    );

    let second = fixture.root("second");
    fixture.move_to(&second).await.expect("the move");

    let mode = std::fs::metadata(&second)
        .expect("the root")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

/// A first move from a default root other users can read and enter, cancelled while it polls a
/// held store's lock, is undone, and leaves the default root available with its root ID stored.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_first_move_cancelled_during_the_lock_poll_leaves_the_root_available() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    std::fs::create_dir(fixture.default_location()).expect("the default root");
    let (store, _) = fixture.plant_virtual_instance(&fixture.default_location());
    std::fs::set_permissions(
        fixture.default_location(),
        std::fs::Permissions::from_mode(0o775),
    )
    .expect("chmod");
    let target = fixture.root("target");

    let (reached, steps) = tokio::sync::mpsc::unbounded_channel::<Reached>();
    let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
    let lock_path = store.join("immutable").join("lock");
    let answering = tokio::spawn(async move {
        let mut steps = steps;
        let mut holder = None;
        while let Some((step, answer)) = steps.recv().await {
            if step == MoveStep::Phase(MovePhase::Identify) {
                holder = FSLock::try_acquire_exact_path(&lock_path)
                    .await
                    .expect("look at the flock");
                assert!(holder.is_some(), "the store is free to hold");
            }
            let _ = answer.send(true);
        }
        drop(holder);
    });
    let mut cancel = Some(cancel);
    let exclusive = fixture.exclusive();
    let moving = LORE_CONTEXT.scope(
        execution_in(fixture.stores.path()),
        move_root(
            fixture.global(),
            &target,
            &exclusive,
            async {
                let _ = cancelled.await;
            },
            |_store| {
                if let Some(cancel) = cancel.take() {
                    let _ = cancel.send(());
                }
            },
            |_name| {},
        ),
    );
    let refused = step_moves(reached, moving).await;
    answering.abort();

    assert!(refused.expect_err("cancelled").is_internal());
    assert!(fixture.record().is_none());
    assert!(!target.exists());
    assert_eq!(
        fixture.availability(),
        Availability::Available(fixture.default_location())
    );
    assert!(fixture.setting().is_some(), "the root ID stays stored");
    let mode = std::fs::metadata(fixture.default_location())
        .expect("the root")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

/// A live mount of a virtual instance in the root, which the mount table lists once the service
/// is stopped, refuses the move with `MountsBusy`, changing nothing.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_live_mount_after_the_stop_refuses_the_move() {
    use lore_revision::fs::fuse::discover::test_util::fuse_lore_entry;
    use lore_revision::fs::fuse::discover::test_util::plant_table;

    let fixture = Fixture::new();
    let name = fixture.default_root_with_an_instance();
    let mount = TempDir::new("lore-external-move-mount-");
    let instance = InstanceId::from_hex(&name).expect("an instance ID");
    let _planted = plant_table(
        &fixture.default_location(),
        fuse_lore_entry(mount.path(), instance),
    );
    let target = fixture.root("target");

    let refused = fixture.move_to(&target).await;

    assert!(matches!(refused, Err(err) if err.is_mounts_busy()));
    assert!(fixture.record().is_none());
    assert!(fixture.setting().is_none());
    assert!(!target.exists());
    assert!(fixture.default_location().join(&name).is_dir());
}

/// A mount table that can't be read fails the move, which can't tell whether a virtual instance
/// is mounted.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_unreadable_mount_table_fails_the_move() {
    use lore_revision::fs::fuse::discover::test_util::plant_unreadable_table;

    let fixture = Fixture::new();
    fixture.default_root_with_an_instance();
    let _planted = plant_unreadable_table(&fixture.default_location());

    let refused = fixture.move_to(&fixture.root("target")).await;

    assert!(refused.expect_err("refused").is_internal());
    assert!(fixture.record().is_none());
}

/// A live mount found just before the store locks, after the first phases ran, rolls the move
/// back.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_live_mount_before_the_locks_rolls_the_move_back() {
    use lore_revision::fs::fuse::discover::test_util::fuse_lore_entry;
    use lore_revision::fs::fuse::discover::test_util::plant_table;

    let fixture = Fixture::new();
    let name = fixture.default_root_with_an_instance();
    let mount = TempDir::new("lore-external-move-mount-");
    let instance = InstanceId::from_hex(&name).expect("an instance ID");
    let root = fixture.default_location();
    let target = fixture.root("target");

    let (reached, steps) = tokio::sync::mpsc::unbounded_channel::<Reached>();
    let planted = std::sync::Arc::new(std::sync::Mutex::new(None));
    let planting = planted.clone();
    let mount_point = mount.path().to_path_buf();
    let answering = answer_steps(steps, move |step| {
        if step == MoveStep::Phase(MovePhase::Identify) {
            *planting.lock().expect("the plant") =
                Some(plant_table(&root, fuse_lore_entry(&mount_point, instance)));
        }
    });
    let refused = step_moves(reached, fixture.move_to(&target)).await;
    answering.abort();

    assert!(matches!(refused, Err(err) if err.is_mounts_busy()));
    assert!(fixture.record().is_none());
    assert!(!target.exists());
    assert_eq!(
        fixture.availability(),
        Availability::Available(fixture.default_location())
    );
}

/// After the stop, a target inside the path of a virtual instance of the root that a store
/// registers is refused with `InvalidArguments`, changing nothing.
#[tokio::test]
async fn a_target_inside_a_registered_virtual_instance_is_refused() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.default_location()).expect("the default root");
    let (store, instance) = fixture.plant_virtual_instance(&fixture.default_location());
    let mounted_at = fixture.root("instance");
    LORE_CONTEXT
        .scope(execution_in(fixture.stores.path()), async {
            let directory = store.parent().expect("the store's directory");
            let repository =
                lore_revision::shared_store::registry::SharedStoreRegistryEntry::in_directory(
                    directory.display().to_string(),
                )
                .create_null_repository_context()
                .await
                .expect("open the store");
            lore_revision::instance::store_instance_registration(
                &repository,
                instance,
                mounted_at.to_str().expect("UTF-8"),
            )
            .await
            .expect("register");
            repository
                .flush(lore_storage::FlushOptions::default())
                .await
                .expect("flush the registration");
        })
        .await;

    let refused = fixture.move_to(&mounted_at.join("root")).await;

    assert!(
        matches!(&refused, Err(err) if err.is_invalid_arguments()),
        "{refused:?}"
    );
    assert!(fixture.record().is_none());
    assert!(fixture.setting().is_none());
}

/// A termination that completes while the move is parked inside the final switch takes effect
/// once the switch has finished: the move ends finished.
#[tokio::test]
async fn a_termination_inside_the_final_switch_lets_the_move_finish() {
    let fixture = Fixture::new();
    fixture.default_root_with_an_instance();
    let target = fixture.root("target");
    let (reached, mut steps) = tokio::sync::mpsc::unbounded_channel::<Reached>();
    let (terminate, terminated) = tokio::sync::oneshot::channel::<()>();

    let answering = tokio::spawn(async move {
        let mut terminate = Some(terminate);
        while let Some((step, answer)) = steps.recv().await {
            if step == MoveStep::Phase(MovePhase::Switch)
                && let Some(terminate) = terminate.take()
            {
                let _ = terminate.send(());
                tokio::task::yield_now().await;
            }
            let _ = answer.send(true);
        }
    });
    let moved = step_moves(
        reached,
        fixture.move_with(&target, async {
            let _ = terminated.await;
        }),
    )
    .await;
    answering.abort();

    assert_eq!(
        moved.expect("the move finishes").kind,
        Some(MoveKind::Rename)
    );
    assert_eq!(fixture.availability(), Availability::Available(target));
}

/// A move interrupted after its root was renamed and before the switch is finished before a new
/// move starts, which then moves the root on.
#[tokio::test]
async fn an_interrupted_move_is_recovered_before_a_new_one() {
    let fixture = Fixture::new();
    let name = fixture.default_root_with_an_instance();
    let id = RootId::generate();
    Marker {
        id,
        generation: NonZeroU64::MIN,
    }
    .save(&fixture.default_location())
    .await
    .expect("the marker");
    Setting {
        path: String::new(),
        id,
        generation: NonZeroU64::MIN,
    }
    .save(fixture.global())
    .await
    .expect("the setting");
    let interrupted = fixture.root("interrupted");
    std::fs::rename(fixture.default_location(), &interrupted).expect("the rename");
    MoveRecord {
        from: String::new(),
        to: interrupted.display().to_string(),
        kind: MoveKind::Rename,
        phase: MovePhase::Switch,
    }
    .save(fixture.global())
    .await
    .expect("the record");

    let next = fixture.root("next");
    let moved = fixture.move_to(&next).await.expect("the move");

    assert!(matches!(moved.recovered, Recovered::Finished(_)));
    assert_eq!(moved.kind, Some(MoveKind::Rename));
    assert_eq!(
        fixture.availability(),
        Availability::Available(next.clone())
    );
    assert!(next.join(&name).join("file").exists());
    assert!(!interrupted.exists());
}

/// On Windows, a rename refused while a file in the root is open rolls the move back and fails,
/// once the retries while in use run out.
#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_rename_refused_while_in_use_rolls_back() {
    let fixture = Fixture::new();
    let name = fixture.default_root_with_an_instance();
    let _open = std::fs::File::open(fixture.default_location().join(&name).join("file"))
        .expect("hold a file open");
    let target = fixture.root("target");

    let refused = fixture.move_to(&target).await;

    assert!(refused.expect_err("refused").is_internal());
    assert!(fixture.record().is_none());
    assert!(!target.exists());
    assert_eq!(
        fixture.availability(),
        Availability::Available(fixture.default_location())
    );
}

/// A failure inside the final switch, after the root was renamed, is finished at once, as
/// recovery would finish it.
#[tokio::test]
async fn a_failure_inside_the_final_switch_is_finished_at_once() {
    let fixture = Fixture::new();
    let name = fixture.default_root_with_an_instance();
    let target = fixture.root("target");
    let (reached, mut steps) = tokio::sync::mpsc::unbounded_channel::<Reached>();
    let answering = tokio::spawn(async move {
        let mut failed = false;
        while let Some((step, answer)) = steps.recv().await {
            let fail = step == MoveStep::Phase(MovePhase::Switch) && !failed;
            failed |= fail;
            let _ = answer.send(!fail);
        }
    });

    let failed = step_moves(reached, fixture.move_to(&target)).await;
    answering.abort();

    assert!(failed.expect_err("the switch fails").is_internal());
    assert!(fixture.record().is_none());
    assert_eq!(
        fixture.availability(),
        Availability::Available(target.clone())
    );
    assert!(target.join(&name).join("file").exists());
}

/// A termination while the recovery of a failed final switch waits for a store another holder
/// took fails the move at once, keeping its record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_termination_while_a_failed_switch_waits_for_a_store_keeps_the_record() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.default_location()).expect("the default root");
    let (store, _) = fixture.plant_virtual_instance(&fixture.default_location());
    let target = fixture.root("target");
    let (reached, mut steps) = tokio::sync::mpsc::unbounded_channel::<Reached>();
    let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
    let answering = tokio::spawn(async move {
        let (mut cancel, mut held, mut failed) = (Some(cancel), None, false);
        while let Some((step, answer)) = steps.recv().await {
            let fail = step == MoveStep::Phase(MovePhase::Switch) && !failed;
            failed |= fail;
            if step == MoveStep::SwitchFailed {
                held = FSLock::try_acquire_exact_path(&store.join("immutable").join("lock"))
                    .await
                    .expect("look at the flock");
                assert!(held.is_some(), "the failed switch let go of the store");
            }
            let _ = answer.send(!fail);
            if step == MoveStep::SwitchFailed
                && let Some(cancel) = cancel.take()
            {
                let _ = cancel.send(());
            }
        }
        drop(held);
    });

    let moved = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        step_moves(
            reached,
            fixture.move_with(&target, async {
                let _ = cancelled.await;
            }),
        ),
    )
    .await
    .expect("the termination ends the move");
    answering.abort();

    assert!(moved.expect_err("the switch fails").is_internal());
    assert!(fixture.record().is_some(), "the record stays for recovery");
}

/// A final switch that fails, and whose recovery fails too, fails with the switch's error, which
/// names the recovery's failure, and keeps the record.
#[tokio::test]
async fn a_failed_switch_whose_recovery_fails_reports_the_switch() {
    let fixture = Fixture::new();
    fixture.default_root_with_an_instance();
    let target = fixture.root("target");
    let (reached, mut steps) = tokio::sync::mpsc::unbounded_channel::<Reached>();
    let answering = tokio::spawn(async move {
        while let Some((step, answer)) = steps.recv().await {
            let _ = answer.send(step != MoveStep::Phase(MovePhase::Switch));
        }
    });

    let failed = step_moves(reached, fixture.move_to(&target)).await;
    answering.abort();

    let error = failed.expect_err("the switch fails");
    let contexts: Vec<&str> = lore_error_set::prelude::HasTrace::trace(&error)
        .locations()
        .iter()
        .filter_map(|location| location.context())
        .collect();
    assert!(
        contexts
            .iter()
            .any(|context| context.starts_with("running the final switch")
                && context.contains("recovery then failed")),
        "{contexts:?}"
    );
    assert!(fixture.record().is_some(), "the record stays for recovery");
}

/// A termination that has completed when the recovery of a failed final switch begins, with every
/// store free, leaves the recovery to finish the move: free locks are taken whether or not the
/// termination has completed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_switch_with_free_stores_is_finished_despite_a_termination() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.default_location()).expect("the default root");
    fixture.plant_virtual_instance(&fixture.default_location());
    let target = fixture.root("target");
    let (reached, mut steps) = tokio::sync::mpsc::unbounded_channel::<Reached>();
    let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
    let answering = tokio::spawn(async move {
        let (mut cancel, mut failed) = (Some(cancel), false);
        while let Some((step, answer)) = steps.recv().await {
            let fail = step == MoveStep::Phase(MovePhase::Switch) && !failed;
            failed |= fail;
            if step == MoveStep::SwitchFailed
                && let Some(cancel) = cancel.take()
            {
                let _ = cancel.send(());
            }
            let _ = answer.send(!fail);
        }
    });

    let moved = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        step_moves(
            reached,
            fixture.move_with(&target, async {
                let _ = cancelled.await;
            }),
        ),
    )
    .await
    .expect("the move ends");
    answering.abort();

    assert!(moved.expect_err("the switch fails").is_internal());
    assert!(fixture.record().is_none(), "the recovery finished the move");
    assert_eq!(fixture.availability(), Availability::Available(target));
}

/// A termination while the registered paths are read, behind a held store, cancels the move with
/// nothing changed.
#[tokio::test]
async fn a_termination_while_the_registrations_are_read_changes_nothing() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.default_location()).expect("the default root");
    let (store, _) = fixture.plant_virtual_instance(&fixture.default_location());
    let _held = FSLock::try_acquire_exact_path(&store.join("immutable").join("lock"))
        .await
        .expect("look at the flock")
        .expect("hold the store");
    let target = fixture.root("target");

    let cancelled = fixture
        .move_with(
            &target,
            tokio::time::sleep(std::time::Duration::from_millis(200)),
        )
        .await;

    assert!(cancelled.expect_err("cancelled").is_internal());
    assert!(fixture.record().is_none());
    assert!(fixture.setting().is_none());
    assert!(!target.exists());
}
