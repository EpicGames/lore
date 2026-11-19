// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Crash tests: a move, an adopt, an abandon or a takeover dropped at each of its phases, as a
//! crash drops it, then recovered, against the row of the LEP's table each state falls in; and a
//! command in another process that opens a store the final switch holds.
// Fixtures plant and copy roots directly, outside any repository.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::path::PathBuf;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::global::external_dir::locks::Joining;
use lore_revision::global::external_dir::locks::LocksError;
use lore_revision::global::external_dir::record::MovePhase;
use lore_revision::global::external_dir::recovery::Locking;
use lore_revision::global::external_dir::recovery::Recovered;
use lore_revision::global::external_dir::recovery::Waiting;
use lore_revision::global::external_dir::recovery::recover;
use lore_revision::global::external_dir::resolve::Availability;
use lore_revision::test_util::MoveStep;
use lore_revision::test_util::step_moves;

use super::moving::Fixture;
use super::takeover::copy_root;
use crate::helper::execution_in;

/// How recovery took a crashed move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Taken {
    Finished,
    Undone,
    Waiting,
}

/// Runs `future` until it reaches `crash`, where it is dropped as a crash drops it, and answers
/// whether it reached it. Boxes `future` as it runs it, since this future would otherwise hold
/// its state twice, past the `clippy::large_futures` limit for a move.
async fn crash_at<T>(crash: MoveStep, future: impl Future<Output = T>) -> bool {
    let (reached, mut steps) =
        tokio::sync::mpsc::unbounded_channel::<(MoveStep, tokio::sync::oneshot::Sender<bool>)>();
    let (crashed, crashing) = tokio::sync::oneshot::channel::<()>();
    let answering = tokio::spawn(async move {
        let mut crashed = Some(crashed);
        let mut parked = Vec::new();
        while let Some((step, answer)) = steps.recv().await {
            if step == crash
                && let Some(crashed) = crashed.take()
            {
                let _ = crashed.send(());
                parked.push(answer);
            } else {
                let _ = answer.send(true);
            }
        }
    });
    let reached_it = tokio::select! {
        _ = Box::pin(step_moves(reached, future)) => false,
        _ = crashing => true,
    };
    answering.abort();
    reached_it
}

impl Fixture {
    /// Recovers the move recorded here, as a service start or the next move does, and answers
    /// what recovery did.
    async fn recovery(&self) -> Result<Recovered, LocksError> {
        LORE_CONTEXT
            .scope(
                execution_in(self.stores.path()),
                recover(
                    self.global(),
                    Joining::Never,
                    Locking::Restarting(&mut |_store: &Path| {}),
                    std::future::pending(),
                ),
            )
            .await
    }

    /// Recovers the move recorded here, as [`Self::recovery`] does, which succeeds.
    async fn recover(&self) -> Taken {
        match self.recovery().await.expect("the recovery") {
            Recovered::Finished(_) => Taken::Finished,
            Recovered::Undone(_) => Taken::Undone,
            Recovered::Waiting(Waiting::Unavailable) => Taken::Waiting,
            other => panic!("{other:?}"),
        }
    }

    /// Asserts that recovery of the crashed move takes it as `taken`, that `root` is then the
    /// available root and, unless it waits, that no record is left.
    async fn recovers_as(&self, taken: Taken, root: &Path, phase: &str) {
        assert_eq!(self.recover().await, taken, "{phase}");
        assert_eq!(
            self.availability(),
            Availability::Available(root.to_path_buf()),
            "{phase}"
        );
        assert_eq!(self.record().is_none(), taken != Taken::Waiting, "{phase}");
    }
}

/// A first rename from the default location, crashed at each phase: undone before its root is
/// renamed, and finished after.
#[tokio::test]
async fn a_first_rename_crashed_at_each_phase_is_recovered() {
    for (phase, taken) in [
        (MovePhase::Mark, Taken::Undone),
        (MovePhase::Identify, Taken::Undone),
        (MovePhase::Place, Taken::Undone),
        (MovePhase::Switch, Taken::Finished),
        (MovePhase::Redirect, Taken::Finished),
    ] {
        let fixture = Fixture::new();
        let name = fixture.default_root_with_an_instance();
        let target = fixture.root("target");
        assert!(crash_at(MoveStep::Phase(phase), fixture.move_to(&target)).await);

        let root = match taken {
            Taken::Finished => target.clone(),
            _ => fixture.default_location(),
        };
        fixture
            .recovers_as(taken, &root, &format!("{phase:?}"))
            .await;
        assert!(root.join(&name).join("file").exists(), "{phase:?}");
    }
}

/// A rename between two configured roots, crashed at each phase of its final switch.
#[tokio::test]
async fn a_rename_between_roots_crashed_at_each_phase_is_recovered() {
    for (phase, taken) in [
        (MovePhase::Place, Taken::Undone),
        (MovePhase::Switch, Taken::Finished),
        (MovePhase::Redirect, Taken::Finished),
    ] {
        let fixture = Fixture::new();
        let from = fixture.moved_to("from").await;
        let target = fixture.root("target");
        assert!(crash_at(MoveStep::Phase(phase), fixture.move_to(&target)).await);

        let root = if taken == Taken::Finished {
            &target
        } else {
            &from
        };
        fixture
            .recovers_as(taken, root, &format!("{phase:?}"))
            .await;
        assert_eq!(
            std::fs::read_link(fixture.default_location()).expect("the redirect"),
            *root,
            "{phase:?}"
        );
    }
}

/// A rename from a configured root back to the default location, which holds the redirect to it,
/// crashed at each phase of its final switch: undone with the configured root's marker kept, or
/// finished.
#[tokio::test]
async fn a_rename_back_to_the_default_location_crashed_at_each_phase_is_recovered() {
    for (phase, taken) in [
        (MovePhase::Place, Taken::Undone),
        (MovePhase::Switch, Taken::Finished),
        (MovePhase::Redirect, Taken::Finished),
    ] {
        let fixture = Fixture::new();
        let from = fixture.moved_to("from").await;
        let target = fixture.default_location();
        assert!(crash_at(MoveStep::Phase(phase), fixture.move_to(&target)).await);

        let root = if taken == Taken::Finished {
            &target
        } else {
            &from
        };
        fixture
            .recovers_as(taken, root, &format!("{phase:?}"))
            .await;
        if taken == Taken::Undone {
            assert_eq!(
                std::fs::read_link(&target).expect("the redirect"),
                from,
                "{phase:?}"
            );
        }
    }
}

/// A create crashed before its one write of `external-root` is undone, setting aside a root it
/// placed, and after that write is finished.
#[tokio::test]
async fn a_create_crashed_before_and_after_its_write_is_recovered() {
    for (phase, taken) in [
        (MovePhase::Place, Taken::Undone),
        (MovePhase::Switch, Taken::Undone),
        (MovePhase::Redirect, Taken::Finished),
    ] {
        let fixture = Fixture::new();
        let target = fixture.root("target");
        assert!(crash_at(MoveStep::Phase(phase), fixture.move_to(&target)).await);

        let root = if taken == Taken::Finished {
            target.clone()
        } else {
            fixture.default_location()
        };
        fixture
            .recovers_as(taken, &root, &format!("{phase:?}"))
            .await;
        if taken == Taken::Undone {
            assert!(!target.exists(), "{phase:?}");
            assert!(fixture.setting().is_none(), "{phase:?}");
        }
    }
}

/// An adopt crashed at each phase is finished.
#[tokio::test]
async fn an_adopt_crashed_at_each_phase_is_finished() {
    for phase in [MovePhase::Switch, MovePhase::Redirect] {
        let fixture = Fixture::new();
        let root = fixture.moved_to("root").await;
        let copy = fixture.root("copy");
        copy_root(&root, &copy);
        assert!(crash_at(MoveStep::Phase(phase), fixture.adopt(&copy)).await);

        fixture
            .recovers_as(Taken::Finished, &copy, &format!("{phase:?}"))
            .await;
    }
}

/// An abandon crashed at each phase is finished, with the next generation.
#[tokio::test]
async fn an_abandon_crashed_at_each_phase_is_finished() {
    for phase in [MovePhase::Place, MovePhase::Switch, MovePhase::Redirect] {
        let fixture = Fixture::new();
        let root = fixture.moved_to("root").await;
        std::fs::rename(&root, fixture.root("away")).expect("lose the volume");
        let new = fixture.root("new");
        assert!(crash_at(MoveStep::Phase(phase), fixture.abandon(&new)).await);

        fixture
            .recovers_as(Taken::Finished, &new, &format!("{phase:?}"))
            .await;
        assert_eq!(
            fixture.setting().expect("the setting").generation.get(),
            2,
            "{phase:?}"
        );
    }
}

/// Each takeover of a rename that waits for a root, crashed once it listed what it gives up,
/// leaves the rename waiting, and crashed once it replaced the record, is finished.
#[tokio::test]
async fn each_takeover_crashed_at_each_phase_is_recovered() {
    for adopting in [true, false] {
        let first = if adopting {
            MovePhase::Switch
        } else {
            MovePhase::Place
        };
        for crash in [MoveStep::TakeoverListed, MoveStep::Phase(first)] {
            let fixture = Fixture::new();
            let root = fixture.moved_to("root").await;
            let copy = fixture.root("copy");
            copy_root(&root, &copy);
            fixture.waiting_rename(&root, &fixture.root("target")).await;
            let replacement = if adopting {
                copy.clone()
            } else {
                fixture.root("new")
            };
            let takeover = async {
                if adopting {
                    fixture.adopt(&replacement).await
                } else {
                    fixture.abandon(&replacement).await
                }
            };
            assert!(crash_at(crash, takeover).await);

            let case = format!("adopting {adopting}, at {crash:?}");
            if crash == MoveStep::TakeoverListed {
                assert_eq!(fixture.recover().await, Taken::Waiting, "{case}");
                assert!(fixture.record().is_some(), "{case}");
            } else {
                fixture
                    .recovers_as(Taken::Finished, &replacement, &case)
                    .await;
            }
        }
    }
}

/// The variable that names the store a child process opens.
const CHILD_STORE: &str = "LORE_TEST_EXTERNAL_ROOT_CHILD_STORE";
/// The variable that names the directory a child process signals through: it makes `opening` as
/// it begins to open the store, and `opened` once it has it.
const CHILD_SIGNALS: &str = "LORE_TEST_EXTERNAL_ROOT_CHILD_SIGNALS";

/// In a child process this binary runs, opens the shared store `CHILD_STORE` names, as an
/// ordinary-instance command does, signalling through `CHILD_SIGNALS`, and returns once it has
/// it. Elsewhere it does nothing.
#[tokio::test]
async fn opening_a_store_child() {
    let Ok(store) = std::env::var(CHILD_STORE) else {
        return;
    };
    let store = PathBuf::from(store);
    let signals = PathBuf::from(std::env::var(CHILD_SIGNALS).expect("the signals directory"));
    std::fs::write(signals.join("opening"), "").expect("signal the open");
    LORE_CONTEXT
        .scope(
            execution_in(store.parent().expect("a store directory")),
            async {
                lore_revision::shared_store::registry::SharedStoreRegistryEntry::in_directory(
                    store.parent().expect("a directory").display().to_string(),
                )
                .create_null_repository_context()
                .await
                .expect("open the store");
            },
        )
        .await;
    std::fs::write(signals.join("opened"), "").expect("signal the store opened");
}

/// A command in another process that opens a store while the final switch holds it waits, and
/// runs once the switch ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_waits_for_the_final_switch() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.default_location()).expect("the default root");
    let (store, _) = fixture.plant_virtual_instance(&fixture.default_location());
    let target = fixture.root("target");

    let (reached, mut steps) =
        tokio::sync::mpsc::unbounded_channel::<(MoveStep, tokio::sync::oneshot::Sender<bool>)>();
    let child_store = store.clone();
    let signals = lore_base::test_util::TempDir::new("lore-external-crash-signals-");
    let child_signals = signals.path().to_path_buf();
    let answering = tokio::spawn(async move {
        while let Some((step, answer)) = steps.recv().await {
            if step == MoveStep::Phase(MovePhase::Switch) {
                let exe = std::env::current_exe().expect("the test binary");
                let child = std::process::Command::new(exe)
                    .args([
                        "--exact",
                        "global::external_dir::crash::opening_a_store_child",
                        "--nocapture",
                    ])
                    .env(CHILD_STORE, &child_store)
                    .env(CHILD_SIGNALS, &child_signals)
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .expect("run the child");
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                while !child_signals.join("opening").exists() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the child never began to open the store"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                assert!(
                    !child_signals.join("opened").exists(),
                    "the child waits while the switch holds the store"
                );
                let _ = answer.send(true);
                drop(steps);
                return lore_base::lore_spawn_blocking!(move || child.wait_with_output())
                    .await
                    .expect("the wait")
                    .expect("the child ends");
            }
            let _ = answer.send(true);
        }
        panic!("the move never reached its switch");
    });
    step_moves(reached, fixture.move_to(&target))
        .await
        .expect("the move");
    let output = answering.await.expect("the answering task");
    assert!(
        signals.path().join("opened").exists(),
        "the child opened the store"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "the child failed:\n{stdout}"
    );
    assert_eq!(fixture.availability(), Availability::Available(target));
}

/// A create undone at recovery, crashed at each step of its undo but between its `Unmark` and
/// `List`, is undone by the next recovery.
#[tokio::test]
async fn an_undo_crashed_at_each_step_is_undone_again() {
    use lore_revision::global::external_dir::recovery::UndoStep;

    for step in [
        UndoStep::Unmark,
        UndoStep::SetAside,
        UndoStep::Redirect,
        UndoStep::Forget,
    ] {
        let fixture = Fixture::new();
        let target = fixture.root("target");
        assert!(crash_at(MoveStep::Phase(MovePhase::Switch), fixture.move_to(&target)).await);
        assert!(
            crash_at(MoveStep::Undo(step), fixture.recover()).await,
            "{step:?}"
        );

        fixture
            .recovers_as(
                Taken::Undone,
                &fixture.default_location(),
                &format!("{step:?}"),
            )
            .await;
        assert!(!target.exists(), "{step:?}");
    }
}

/// An abandon to the default location, which holds the command's redirect to the lost root,
/// crashed at each phase, is finished there.
#[tokio::test]
async fn an_abandon_to_the_default_location_crashed_at_each_phase_is_finished() {
    for phase in [MovePhase::Place, MovePhase::Switch, MovePhase::Redirect] {
        let fixture = Fixture::new();
        let root = fixture.moved_to("root").await;
        std::fs::rename(&root, fixture.root("away")).expect("lose the volume");
        let default = fixture.default_location();
        assert!(crash_at(MoveStep::Phase(phase), fixture.abandon(&default)).await);

        fixture
            .recovers_as(Taken::Finished, &default, &format!("{phase:?}"))
            .await;
    }
}

/// Each takeover of an adopt whose root has gone, crashed once it listed what it gives up, leaves
/// the adopt's record, which recovery fails on, and crashed at each phase of its own record, is
/// finished.
#[tokio::test]
async fn each_takeover_of_an_adopt_whose_root_has_gone_crashed_at_each_phase_is_recovered() {
    for adopting in [true, false] {
        let phases: &[MovePhase] = if adopting {
            &[MovePhase::Switch, MovePhase::Redirect]
        } else {
            &[MovePhase::Place, MovePhase::Switch, MovePhase::Redirect]
        };
        let crashes = std::iter::once(MoveStep::TakeoverListed)
            .chain(phases.iter().map(|phase| MoveStep::Phase(*phase)));
        for crash in crashes {
            let fixture = Fixture::new();
            let (_, other) = fixture.adopt_whose_root_has_gone().await;
            let gone = fixture.record().expect("the adopt's record");
            let replacement = if adopting { other } else { fixture.root("new") };
            let takeover = async {
                if adopting {
                    fixture.adopt(&replacement).await
                } else {
                    fixture.abandon(&replacement).await
                }
            };
            assert!(crash_at(crash, takeover).await);

            let case = format!("adopting {adopting}, at {crash:?}");
            if crash == MoveStep::TakeoverListed {
                assert!(fixture.recovery().await.is_err(), "{case}");
                assert_eq!(fixture.record(), Some(gone), "{case}");
            } else {
                fixture
                    .recovers_as(Taken::Finished, &replacement, &case)
                    .await;
            }
        }
    }
}

/// A copy to `target`, forced for `fixture`, crashed at each phase, then recovered: undone until
/// the setting names the copy, since the old root stays available, and finished after, with the
/// old root's files at the root recovery leaves and no temporary directory.
async fn a_copy_crashed_at_each_phase(phases: &[(MovePhase, Taken)], between_roots: bool) {
    use lore_revision::global::external_dir::recovery::copying_path;
    use lore_revision::test_util::MoveOverride;
    use lore_revision::test_util::override_moves;

    for &(phase, taken) in phases {
        let fixture = Fixture::new();
        let _copying = override_moves(fixture.global(), MoveOverride::Copy);
        let from = if between_roots {
            fixture.moved_to("from").await
        } else {
            fixture.default_root_with_an_instance();
            fixture.default_location()
        };
        let name = std::fs::read_dir(&from)
            .expect("list the root")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| !name.starts_with('.'))
            .expect("an external directory");
        let target = fixture.root("target");
        assert!(crash_at(MoveStep::Phase(phase), fixture.move_to(&target)).await);

        let root = match taken {
            Taken::Finished => target.clone(),
            _ => from.clone(),
        };
        fixture
            .recovers_as(taken, &root, &format!("{phase:?}"))
            .await;
        assert!(root.join(&name).join("file").exists(), "{phase:?}");
        assert!(!copying_path(&target).exists(), "{phase:?}");
        if taken == Taken::Undone {
            assert!(!target.exists(), "{phase:?}");
        }
    }
}

/// A first copy from the default location, crashed at each phase.
#[tokio::test]
async fn a_first_copy_crashed_at_each_phase_is_recovered() {
    a_copy_crashed_at_each_phase(
        &[
            (MovePhase::Mark, Taken::Undone),
            (MovePhase::Identify, Taken::Undone),
            (MovePhase::Copy, Taken::Undone),
            (MovePhase::Place, Taken::Undone),
            (MovePhase::Switch, Taken::Undone),
            (MovePhase::Unmark, Taken::Finished),
            (MovePhase::SetAside, Taken::Finished),
            (MovePhase::Redirect, Taken::Finished),
        ],
        false,
    )
    .await;
}

/// A copy between two configured roots, crashed at each phase.
#[tokio::test]
async fn a_copy_between_roots_crashed_at_each_phase_is_recovered() {
    a_copy_crashed_at_each_phase(
        &[
            (MovePhase::Copy, Taken::Undone),
            (MovePhase::Place, Taken::Undone),
            (MovePhase::Switch, Taken::Undone),
            (MovePhase::Unmark, Taken::Finished),
            (MovePhase::SetAside, Taken::Finished),
            (MovePhase::Redirect, Taken::Finished),
        ],
        true,
    )
    .await;
}

/// A copy crashed once the setting names the copy, whose old root's volume is then gone, is
/// finished, and lists the old root as a leftover that keeps its marker until it returns.
#[tokio::test]
async fn a_copy_whose_old_root_is_gone_lists_it_as_marked() {
    use lore_revision::test_util::MoveOverride;
    use lore_revision::test_util::override_moves;

    let fixture = Fixture::new();
    let _copying = override_moves(fixture.global(), MoveOverride::Copy);
    let from = fixture.moved_to("from").await;
    let target = fixture.root("target");
    assert!(crash_at(MoveStep::Phase(MovePhase::Unmark), fixture.move_to(&target)).await);
    std::fs::rename(&from, fixture.root("away")).expect("the old root's volume goes");

    fixture
        .recovers_as(Taken::Finished, &target, "unmark")
        .await;

    assert_eq!(
        fixture.leftovers().marked,
        [from.display().to_string()],
        "the old root is listed as still marked"
    );
}
