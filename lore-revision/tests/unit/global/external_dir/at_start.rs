// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Recovery at a service start and in the background, in a global data directory of the test's
//! own, with no mount: the mounting is tested in `fuse_mount.rs` and `swfs_external_root.rs`. A
//! lock another holder takes through an `FSLock` of its own contends as another process's would.
// Fixtures plant roots, records and stores directly, outside any repository.
#![allow(clippy::disallowed_methods)]

use std::num::NonZeroU64;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::fs::lock::FSLock;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::global::external_dir::leftovers::Leftovers;
use lore_revision::global::external_dir::marker::MARKER;
use lore_revision::global::external_dir::marker::Marker;
use lore_revision::global::external_dir::record::MoveKind;
use lore_revision::global::external_dir::record::MovePhase;
use lore_revision::global::external_dir::record::MoveRecord;
use lore_revision::global::external_dir::recovery::AtStart;
use lore_revision::global::external_dir::recovery::AtStartFound;
use lore_revision::global::external_dir::recovery::examine_at_start;
use lore_revision::global::external_dir::recovery::list_leftover;
use lore_revision::global::external_dir::recovery::recover_at_start;
use lore_revision::global::external_dir::recovery::recover_in_background;
use lore_revision::global::external_dir::recovery::report_default_location;
use lore_revision::global::external_dir::resolve::Availability;
use lore_revision::global::external_dir::setting::RootId;
use lore_revision::global::external_dir::setting::Setting;
use lore_storage::local::store_lock::Intent;
use lore_storage::local::store_lock::StoreLock;

use super::moving::Fixture;
use crate::helper::execution_in;

/// Whether another holder could take exclusive access to the external root of `global`.
fn others_could_take_exclusive(global: &Path) -> bool {
    FSLock::try_acquire_file_lock(global.join("external"))
        .expect("attempt")
        .is_some()
}

impl Fixture {
    /// A first move from the default location to `target` interrupted at its switch, after the
    /// root was renamed: recovery finishes it.
    async fn renamed_before_the_switch(&self, target: &Path) {
        let id = RootId::generate();
        Marker {
            id,
            generation: NonZeroU64::MIN,
        }
        .save(&self.default_location())
        .await
        .expect("the marker");
        Setting {
            path: String::new(),
            id,
            generation: NonZeroU64::MIN,
        }
        .save(self.global())
        .await
        .expect("the setting");
        std::fs::rename(self.default_location(), target).expect("the rename");
        MoveRecord {
            from: String::new(),
            to: target.display().to_string(),
            kind: MoveKind::Rename,
            phase: MovePhase::Switch,
        }
        .save(self.global())
        .await
        .expect("the record");
    }

    async fn at_start(&self) -> AtStart {
        LORE_CONTEXT
            .scope(
                execution_in(self.stores.path()),
                recover_at_start(self.global(), Duration::ZERO),
            )
            .await
    }

    async fn in_background(&self, mount: impl Future<Output = ()>) {
        LORE_CONTEXT
            .scope(
                execution_in(self.stores.path()),
                recover_in_background(self.global().to_path_buf(), mount),
            )
            .await;
    }
}

/// With no move recorded, a start recovers nothing and takes no access.
#[tokio::test]
async fn a_start_with_no_move_takes_no_access() {
    let fixture = Fixture::new();
    fixture.default_root_with_an_instance();

    assert_eq!(examine_at_start(fixture.global()), AtStartFound::Unmoved);
    assert_eq!(fixture.at_start().await, AtStart::NoMove);
    let missing = fixture.global().join("missing");
    assert_eq!(examine_at_start(&missing), AtStartFound::Unmoved);
    assert!(!missing.exists(), "the examination makes nothing");
    assert!(others_could_take_exclusive(fixture.global()));
}

/// A recorded move is finished at start, and the service then holds shared access.
#[tokio::test]
async fn a_start_finishes_a_recorded_move() {
    let fixture = Fixture::new();
    let name = fixture.default_root_with_an_instance();
    let target = fixture.root("target");
    fixture.renamed_before_the_switch(&target).await;
    assert_eq!(examine_at_start(fixture.global()), AtStartFound::Recorded);

    assert_eq!(fixture.at_start().await, AtStart::Recovered);

    assert!(fixture.record().is_none());
    assert_eq!(
        fixture.availability(),
        Availability::Available(target.clone())
    );
    assert!(target.join(&name).join("file").exists());
    assert!(
        !others_could_take_exclusive(fixture.global()),
        "shared access is held"
    );
}

/// A store held at start leaves the move waiting under exclusive access; the background retry,
/// once the other holder lets go, joins a hold of a command in this process, finishes the move,
/// and mounts.
#[tokio::test]
async fn a_held_store_is_recovered_in_the_background_and_then_mounted() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.default_location()).expect("the default root");
    let (store, _) = fixture.plant_virtual_instance(&fixture.default_location());
    let target = fixture.root("target");
    fixture.renamed_before_the_switch(&target).await;
    let other = FSLock::try_acquire_exact_path(&store.join("immutable").join("lock"))
        .await
        .expect("look at the flock")
        .expect("hold the store");

    assert_eq!(fixture.at_start().await, AtStart::Retry);
    assert!(fixture.record().is_some());
    assert!(
        FSLock::try_acquire_shared_file_lock(fixture.global().join("external"))
            .expect("attempt")
            .is_none(),
        "the pending recovery holds exclusive access"
    );

    drop(other);
    let command = StoreLock::new(store.join("immutable"), true).expect("the lock");
    let mut held = command
        .acquire(Intent::Read)
        .await
        .expect("a command holds it");
    held.note_refreshed();
    let mounted = Arc::new(AtomicBool::new(false));
    let mounting = Arc::clone(&mounted);
    fixture
        .in_background(async move {
            mounting.store(true, Ordering::SeqCst);
        })
        .await;
    drop(held);

    assert!(
        mounted.load(Ordering::SeqCst),
        "the recovered root's instances mount"
    );
    assert!(fixture.record().is_none());
    assert_eq!(fixture.availability(), Availability::Available(target));
}

/// A move that waits for a root that isn't available keeps its record until the next start, with
/// no access held.
#[tokio::test]
async fn a_move_waiting_for_a_root_waits_for_the_next_start() {
    let fixture = Fixture::new();
    fixture.default_root_with_an_instance();
    let target = fixture.root("target");
    fixture.renamed_before_the_switch(&target).await;
    std::fs::rename(&target, fixture.root("away")).expect("lose the volume");

    assert_eq!(fixture.at_start().await, AtStart::NextStart);

    assert!(fixture.record().is_some());
    assert!(others_could_take_exclusive(fixture.global()));
}

/// The background task deletes a leftover's marker and removes its contents, and drops its entry,
/// and never removes the root the setting names.
#[tokio::test]
async fn leftovers_are_removed_and_the_configured_root_kept() {
    let fixture = Fixture::new();
    fixture.default_root_with_an_instance();
    let root = fixture.root("root");
    fixture.move_to(&root).await.expect("the move");
    let leftover = fixture.root("leftover");
    std::fs::create_dir_all(leftover.join("contents")).expect("a leftover");
    std::fs::write(leftover.join(MARKER), "").expect("its marker");
    let returned = fixture.root("returned");
    std::fs::create_dir_all(&returned).expect("a returned leftover");
    std::fs::write(returned.join(MARKER), "").expect("its marker");
    for (path, marked) in [(&leftover, false), (&root, false), (&returned, true)] {
        list_leftover(fixture.global(), path, marked)
            .await
            .expect("list");
    }

    fixture.in_background(async {}).await;

    assert!(!leftover.exists());
    assert!(!returned.exists());
    assert!(root.join(MARKER).exists(), "the configured root is kept");
    assert_eq!(
        Leftovers::load(fixture.global()).expect("read"),
        Leftovers::default()
    );
}

/// A directory with entries at the default location, while the setting names another root, is
/// reported at start.
#[tokio::test]
async fn a_default_location_with_entries_is_reported() {
    let fixture = Fixture::new();
    fixture.default_root_with_an_instance();
    let root = fixture.root("root");
    fixture.move_to(&root).await.expect("the move");
    assert_eq!(examine_at_start(fixture.global()), AtStartFound::Configured);
    let setting = fixture.setting().expect("the setting");
    assert!(
        !report_default_location(fixture.global(), &setting),
        "a redirect"
    );

    std::fs::remove_file(fixture.default_location())
        .or_else(|_| std::fs::remove_dir(fixture.default_location()))
        .expect("remove the redirect");
    std::fs::create_dir_all(fixture.default_location().join("entry")).expect("an entry");
    assert!(report_default_location(fixture.global(), &setting));
}

/// A still-marked leftover whose path the setting has come to name, as a move to a root a
/// takeover gave up makes it, is taken off the list and keeps its marker.
#[tokio::test]
async fn a_marked_leftover_that_became_the_root_keeps_its_marker() {
    let fixture = Fixture::new();
    fixture.default_root_with_an_instance();
    let root = fixture.root("root");
    list_leftover(fixture.global(), &root, true)
        .await
        .expect("list the absent root as marked");
    fixture.move_to(&root).await.expect("the move");

    fixture.in_background(async {}).await;

    assert!(root.join(MARKER).exists(), "the root keeps its marker");
    assert_eq!(fixture.availability(), Availability::Available(root));
    assert_eq!(
        Leftovers::load(fixture.global()).expect("read"),
        Leftovers::default()
    );
}

/// Recovery in the background polls a held store's lock: a store let go while it polls is
/// taken, where a single attempt would leave the move waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_in_the_background_polls_a_held_store() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.default_location()).expect("the default root");
    let (store, _) = fixture.plant_virtual_instance(&fixture.default_location());
    let target = fixture.root("target");
    fixture.renamed_before_the_switch(&target).await;
    let other = FSLock::try_acquire_exact_path(&store.join("immutable").join("lock"))
        .await
        .expect("look at the flock")
        .expect("hold the store");
    let releasing = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        drop(other);
    });

    let at = LORE_CONTEXT
        .scope(
            execution_in(fixture.stores.path()),
            recover_at_start(
                fixture.global(),
                lore_revision::global::external_dir::locks::LOCK_POLL,
            ),
        )
        .await;
    releasing.await.expect("the release");

    assert_eq!(at, AtStart::Recovered);
    assert_eq!(fixture.availability(), Availability::Available(target));
}

/// A move by copy interrupted at its `copy` phase, while the setting names the root it leaves,
/// is undone at the start: what it copied is set aside, and the old root stays available.
#[tokio::test]
async fn a_copy_interrupted_at_its_copy_phase_is_undone_at_the_start() {
    let fixture = Fixture::new();
    let name = fixture.default_root_with_an_instance();
    let target = fixture.root("copy");
    let copying = lore_revision::global::external_dir::recovery::copying_path(&target);
    std::fs::create_dir_all(copying.join(&name)).expect("a partial copy");
    MoveRecord {
        from: String::new(),
        to: target.display().to_string(),
        kind: MoveKind::Copy,
        phase: MovePhase::Copy,
    }
    .save(fixture.global())
    .await
    .expect("the record");

    assert_eq!(fixture.at_start().await, AtStart::Recovered);

    assert!(fixture.record().is_none());
    assert!(!copying.exists(), "the partial copy is set aside");
    assert!(!target.exists());
    assert_eq!(
        fixture.availability(),
        Availability::Available(fixture.default_location())
    );
    assert!(fixture.default_location().join(&name).exists());
}

/// A record that names neither root the setting names waits for the next start with no access
/// held, and a record whose exclusive access another process holds is retried.
#[tokio::test]
async fn a_start_refuses_or_retries_as_the_record_and_the_access_say() {
    let fixture = Fixture::new();
    fixture.default_root_with_an_instance();
    let held = FSLock::try_acquire_file_lock(fixture.global().join("external"))
        .expect("attempt")
        .expect("hold exclusive access");
    MoveRecord {
        from: fixture.root("a").display().to_string(),
        to: fixture.root("b").display().to_string(),
        kind: MoveKind::Rename,
        phase: MovePhase::Place,
    }
    .save(fixture.global())
    .await
    .expect("the record");

    assert_eq!(fixture.at_start().await, AtStart::Retry);

    drop(held);
    assert_eq!(fixture.at_start().await, AtStart::NextStart);
    assert!(others_could_take_exclusive(fixture.global()));
    assert!(fixture.record().is_some());
}

/// A move, an adopt and an abandon, each cancelled while its recovery of an interrupted move waits
/// for a held store, fail at once, keeping the interrupted move's record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_cancelled_while_its_recovery_waits_for_a_store_keeps_the_record() {
    for kind in [MoveKind::Rename, MoveKind::Adopt, MoveKind::Abandon] {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.default_location()).expect("the default root");
        let (store, _) = fixture.plant_virtual_instance(&fixture.default_location());
        let target = fixture.root("target");
        fixture.renamed_before_the_switch(&target).await;
        let _other = FSLock::try_acquire_exact_path(&store.join("immutable").join("lock"))
            .await
            .expect("look at the flock")
            .expect("hold the store");
        let (next, cancel) = (
            fixture.root("next"),
            tokio::time::sleep(Duration::from_millis(200)),
        );

        let changed = tokio::time::timeout(
            Duration::from_secs(30),
            Box::pin(async {
                match kind {
                    MoveKind::Adopt => fixture.adopt_with(&next, cancel).await,
                    MoveKind::Abandon => fixture.abandon_with(&next, cancel).await,
                    _ => fixture.move_with(&next, cancel).await,
                }
            }),
        )
        .await
        .expect("the cancel ends the change");

        assert!(changed.expect_err("cancelled").is_internal(), "{kind:?}");
        assert_eq!(
            fixture.record().expect("the record stays").to,
            target.display().to_string(),
            "{kind:?}"
        );
    }
}

/// An adopt whose recovery of an interrupted move waits for a store another holder took goes on
/// once the holder lets go, finishing the move first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_adopt_goes_on_once_its_recovery_takes_a_held_store() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.default_location()).expect("the default root");
    let (store, _) = fixture.plant_virtual_instance(&fixture.default_location());
    let target = fixture.root("target");
    fixture.renamed_before_the_switch(&target).await;
    let other = FSLock::try_acquire_exact_path(&store.join("immutable").join("lock"))
        .await
        .expect("look at the flock")
        .expect("hold the store");
    let releasing = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(other);
    });

    let adopted = tokio::time::timeout(Duration::from_secs(30), fixture.adopt(&target))
        .await
        .expect("the adopt goes on");
    releasing.await.expect("the release");

    adopted.expect("the adopt of the root the recovery finished");
    assert!(fixture.record().is_none());
    assert_eq!(fixture.availability(), Availability::Available(target));
}

/// An adopt whose root has gone fails to recover, naming its record and its root, and waits for
/// the next start, with no access held and no retry.
#[tokio::test]
async fn an_adopt_whose_root_has_gone_waits_for_the_next_start() {
    let fixture = Fixture::new();
    let (_, _other) = fixture.adopt_whose_root_has_gone().await;
    let gone = fixture.root("gone");

    let failed = LORE_CONTEXT
        .scope(
            execution_in(fixture.stores.path()),
            lore_revision::global::external_dir::recovery::recover(
                fixture.global(),
                lore_revision::global::external_dir::locks::Joining::Never,
                lore_revision::global::external_dir::recovery::Locking::Restarting(
                    &mut |_store: &Path| {},
                ),
                std::future::pending(),
            ),
        )
        .await
        .expect_err("the recovery fails")
        .to_string();
    assert!(failed.contains(&gone.display().to_string()), "{failed}");
    assert!(failed.contains("external.move"), "{failed}");

    assert_eq!(fixture.at_start().await, AtStart::NextStart);
    assert!(others_could_take_exclusive(fixture.global()));
    tokio::time::timeout(Duration::from_secs(2), fixture.in_background(async {}))
        .await
        .expect("the background task doesn't retry the adopt");
    assert!(fixture.record().is_some(), "the record stays");
}

/// The messages of a move that waits, collected by the log callback.
static HELD_STORE_LOGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Collects each message of a move that waits, for a shared store or for exclusive access, in
/// [`HELD_STORE_LOGS`].
fn collect_held_store_logs(_level: lore_base::log::LoreLogLevel, _location: &str, message: &str) {
    if message.contains("waits for the shared store") || message.contains("waits for exclusive") {
        HELD_STORE_LOGS
            .lock()
            .expect("the log collector lock is not poisoned")
            .push(message.to_string());
    }
}

/// Recovery in the background that finds a store held across several attempts logs the wait
/// once, and recovers the move once the holder lets go. The messages are told from other tests'
/// by the name of this fixture's stores directory, which every spelling of the store's path
/// shares, as an 8.3 short form or a verbatim prefix doesn't change a name made here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_held_across_retries_logs_the_wait_once() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.default_location()).expect("the default root");
    let (store, _) = fixture.plant_virtual_instance(&fixture.default_location());
    let target = fixture.root("target");
    fixture.renamed_before_the_switch(&target).await;
    let other = FSLock::try_acquire_exact_path(&store.join("immutable").join("lock"))
        .await
        .expect("look at the flock")
        .expect("hold the store");
    assert_eq!(fixture.at_start().await, AtStart::Retry);
    let attempts = 2;
    let held_for = lore_revision::global::external_dir::recovery::RETRY_INTERVAL
        .saturating_add(lore_revision::global::external_dir::locks::LOCK_POLL)
        .saturating_mul(attempts)
        .saturating_add(Duration::from_secs(1));
    let releasing = tokio::spawn(async move {
        tokio::time::sleep(held_for).await;
        drop(other);
    });
    let capture = crate::helper::LOG_CAPTURE.lock().await;
    let level = lore_base::log::log_level();
    lore_base::log::set_log_level(lore_base::log::LoreLogLevel::Info);
    lore_base::log::set_log_callback(Some(collect_held_store_logs));

    fixture.in_background(async {}).await;

    lore_base::log::set_log_callback(None);
    lore_base::log::set_log_level(level);
    drop(capture);
    releasing.await.expect("the release");
    let stores = fixture
        .stores
        .path()
        .file_name()
        .expect("the stores' directory has a name")
        .to_string_lossy()
        .into_owned();
    let logged = HELD_STORE_LOGS
        .lock()
        .expect("the log collector lock is not poisoned")
        .iter()
        .filter(|message| message.contains(&stores))
        .count();
    assert_eq!(
        logged, 1,
        "the wait is logged once over {attempts} attempts"
    );
    assert!(fixture.record().is_none());
    assert_eq!(fixture.availability(), Availability::Available(target));
}

/// A start that finds another holder of access to the external root leaves the move for a retry
/// and reports that it waits for exclusive access.
#[tokio::test]
async fn a_start_refused_exclusive_access_reports_it() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.default_location()).expect("the default root");
    let target = fixture.root("target");
    fixture.renamed_before_the_switch(&target).await;
    let other = FSLock::try_acquire_shared_file_lock(fixture.global().join("external"))
        .expect("attempt")
        .expect("hold shared access");
    let capture = crate::helper::LOG_CAPTURE.lock().await;
    HELD_STORE_LOGS
        .lock()
        .expect("the log collector lock is not poisoned")
        .clear();
    let level = lore_base::log::log_level();
    lore_base::log::set_log_level(lore_base::log::LoreLogLevel::Info);
    lore_base::log::set_log_callback(Some(collect_held_store_logs));

    let at = fixture.at_start().await;

    lore_base::log::set_log_callback(None);
    lore_base::log::set_log_level(level);
    drop(capture);
    drop(other);
    assert_eq!(at, AtStart::Retry);
    assert!(
        HELD_STORE_LOGS
            .lock()
            .expect("the log collector lock is not poisoned")
            .iter()
            .any(|message| message.contains("exclusive access")),
        "the wait for exclusive access is reported"
    );
}
