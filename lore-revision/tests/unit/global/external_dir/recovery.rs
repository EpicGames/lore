// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The steps of a move of the external root, called directly, the direction recovery takes, and
//! the leftover list.
// Fixtures plant roots and files directly; what these test is how the steps read them.
#![allow(clippy::disallowed_methods)]

use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;

use lore_base::test_util::TempDir;
use lore_revision::global::external_dir::leftovers::Leftovers;
use lore_revision::global::external_dir::marker::MARKER;
use lore_revision::global::external_dir::marker::Marker;
use lore_revision::global::external_dir::record::MoveKind;
use lore_revision::global::external_dir::record::MovePhase;
use lore_revision::global::external_dir::record::MoveRecord;
use lore_revision::global::external_dir::recovery::*;
use lore_revision::global::external_dir::setting::RootId;
use lore_revision::global::external_dir::setting::Setting;
use lore_revision::test_util::MoveStep;
use lore_revision::test_util::step_moves;

/// A step a move reached, with the sender the test answers it on.
type Reached = (MoveStep, tokio::sync::oneshot::Sender<bool>);

/// A global data directory, and a directory beside it for configured roots.
struct Fixture {
    global: TempDir,
    volume: TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            global: TempDir::new("lore-external-recovery-global-"),
            volume: TempDir::new("lore-external-recovery-volume-"),
        }
    }

    fn global(&self) -> &Path {
        self.global.path()
    }

    fn default_location(&self) -> PathBuf {
        self.global.path().join("external")
    }

    /// A root named `name` beside the global data directory, not yet created.
    fn root(&self, name: &str) -> PathBuf {
        self.volume.path().join(name)
    }

    fn setting(&self) -> Option<Setting> {
        Setting::load(self.global()).expect("read the setting")
    }

    fn leftovers(&self) -> Leftovers {
        Leftovers::load(self.global()).expect("read the leftovers")
    }

    fn record(&self) -> Option<MoveRecord> {
        MoveRecord::load(self.global()).expect("read the record")
    }

    fn assess(&self) -> Assessment {
        assess(self.global())
            .expect("assess the move")
            .expect("a move is recorded")
    }
}

/// The text of `path` as a record or setting names it.
fn text(path: &Path) -> String {
    path.to_str().expect("UTF-8").to_owned()
}

fn marker(id: RootId) -> Marker {
    Marker {
        id,
        generation: NonZeroU64::MIN,
    }
}

/// Makes the root `root` with mode `0o700`, holding `marker`.
async fn marked_root(root: &Path, marker: Marker) {
    lore_io::create_dir_all_private(root).expect("make the root");
    marker.save(root).await.expect("write the marker");
}

/// Writes a setting naming `path` with `marker`'s root ID and generation into `global`.
async fn store_setting(global: &Path, path: String, marker: Marker) {
    Setting {
        path,
        id: marker.id,
        generation: marker.generation,
    }
    .save(global)
    .await
    .expect("write the setting");
}

/// A record of a move of `kind` from `from` to `to`, at `phase`, written into `global`.
async fn record(
    global: &Path,
    kind: MoveKind,
    from: String,
    to: String,
    phase: MovePhase,
) -> MoveRecord {
    let record = MoveRecord {
        from,
        to,
        kind,
        phase,
    };
    record.save(global).await.expect("write the record");
    record
}

/// Runs `future` under `step_moves`, answering every step with `true`, and answers its output and
/// the steps it reached in order.
async fn steps_of<T>(future: impl Future<Output = T>) -> (T, Vec<MoveStep>) {
    let (reached, mut steps) = tokio::sync::mpsc::unbounded_channel::<Reached>();
    let answering = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some((step, answer)) = steps.recv().await {
            seen.push(step);
            let _ = answer.send(true);
        }
        seen
    });
    let output = step_moves(reached, future).await;
    (output, answering.await.expect("the answering task"))
}

/// Runs `future` under `step_moves`, failing it at `failing` and answering every other step with
/// `true`.
async fn failing_at<T>(failing: MoveStep, future: impl Future<Output = T>) -> T {
    let (reached, mut steps) = tokio::sync::mpsc::unbounded_channel::<Reached>();
    let answering = tokio::spawn(async move {
        while let Some((step, answer)) = steps.recv().await {
            let _ = answer.send(step != failing);
        }
    });
    let output = step_moves(reached, future).await;
    answering.abort();
    output
}

/// Every row of the LEP's table, from constructed states.
#[test]
fn every_row_of_the_table_picks_its_direction() {
    use Direction::*;
    use MoveKind::*;
    use Named::*;

    for kind in [Rename, Create, Copy] {
        assert_eq!(direction(kind, From, true, false), Undo, "{kind:?}");
        assert_eq!(direction(kind, From, true, true), Undo, "{kind:?}");
        assert_eq!(direction(kind, To, true, true), Finish, "{kind:?}");
        assert_eq!(direction(kind, To, false, true), Finish, "{kind:?}");
        assert_eq!(direction(kind, To, true, false), Wait, "{kind:?}");
        assert_eq!(direction(kind, From, false, false), Wait, "{kind:?}");
        assert_eq!(direction(kind, To, false, false), Wait, "{kind:?}");
        for (from, to) in [(true, true), (true, false), (false, true), (false, false)] {
            assert_eq!(direction(kind, Neither, from, to), Refuse, "{kind:?}");
        }
    }
    assert_eq!(direction(Rename, From, false, true), Finish);
    assert_eq!(direction(Create, From, false, true), Finish);
    assert_eq!(
        direction(Copy, From, false, true),
        Wait,
        "only a rename finishes forward"
    );
    for kind in [Adopt, Abandon] {
        for named in [From, To, Neither] {
            for (from, to) in [(true, true), (true, false), (false, true), (false, false)] {
                assert_eq!(
                    direction(kind, named, from, to),
                    Finish,
                    "{kind:?} {named:?}"
                );
            }
        }
    }
}

/// A setting that names neither root refuses, and recovery reads it so from disk.
#[tokio::test]
async fn a_setting_naming_neither_root_refuses() {
    let fixture = Fixture::new();
    let elsewhere = fixture.root("elsewhere");
    let current = marker(RootId::generate());
    marked_root(&elsewhere, current).await;
    store_setting(fixture.global(), text(&elsewhere), current).await;
    record(
        fixture.global(),
        MoveKind::Rename,
        text(&fixture.root("old")),
        text(&fixture.root("new")),
        MovePhase::Place,
    )
    .await;

    let assessment = fixture.assess();
    assert_eq!(assessment.named, Named::Neither);
    assert_eq!(assessment.direction, Direction::Refuse);
}

/// A first rename interrupted before it stored the root ID is undone, and with the root ID stored
/// and the root renamed, before the switch, it is finished.
#[tokio::test]
async fn recovery_reads_each_side_of_a_first_rename_from_disk() {
    let fixture = Fixture::new();
    let target = fixture.root("target");
    std::fs::create_dir(fixture.default_location()).expect("the default root");
    record(
        fixture.global(),
        MoveKind::Rename,
        String::new(),
        text(&target),
        MovePhase::Identify,
    )
    .await;
    let before = fixture.assess();
    assert_eq!(
        (before.named, before.from_available),
        (Named::From, true),
        "with no setting, the default root is the current one"
    );
    assert_eq!(before.direction, Direction::Undo);

    let current = marker(RootId::generate());
    current
        .save(&fixture.default_location())
        .await
        .expect("mark");
    store_setting(fixture.global(), String::new(), current).await;
    std::fs::rename(fixture.default_location(), &target).expect("the rename");
    let after = fixture.assess();
    assert_eq!((after.from_available, after.to_available), (false, true));
    assert_eq!(after.direction, Direction::Finish);
}

/// A first rename finishes from `mark`, through every phase in order: it marks the current root,
/// stores the root ID, renames the root, switches the setting and makes the redirect, then removes
/// the record.
#[tokio::test]
async fn a_first_rename_finishes_step_by_step() {
    let fixture = Fixture::new();
    let target = fixture.root("target");
    std::fs::create_dir(fixture.default_location()).expect("the default root");
    std::fs::write(fixture.default_location().join("kept"), "").expect("a file in the root");
    let mut record = record(
        fixture.global(),
        MoveKind::Rename,
        String::new(),
        text(&target),
        MovePhase::Mark,
    )
    .await;

    let (finished, steps) = steps_of(finish(fixture.global(), &mut record)).await;

    finished.expect("the move finishes");
    assert_eq!(
        steps,
        [
            MovePhase::Mark,
            MovePhase::Identify,
            MovePhase::Place,
            MovePhase::Switch,
            MovePhase::Redirect
        ]
        .map(MoveStep::Phase)
    );
    let setting = fixture.setting().expect("the setting");
    assert_eq!(setting.path, text(&target));
    assert_eq!(
        Marker::load(&target).expect("read the marker"),
        Some(Marker {
            id: setting.id,
            generation: setting.generation
        })
    );
    assert!(target.join("kept").exists());
    assert_eq!(
        std::fs::read_link(fixture.default_location()).expect("the redirect"),
        target
    );
    assert!(
        MoveRecord::load(fixture.global())
            .expect("read the record")
            .is_none()
    );
}

/// Each step whose effect is on disk runs no second time: a marked root keeps its marker, a stored
/// root ID keeps its setting, and a placed root isn't renamed again.
#[tokio::test]
async fn each_step_checks_for_its_effect_first() {
    let fixture = Fixture::new();
    let target = fixture.root("target");
    let current = marker(RootId::generate());
    marked_root(&fixture.default_location(), current).await;
    let mut marking = record(
        fixture.global(),
        MoveKind::Rename,
        String::new(),
        text(&target),
        MovePhase::Mark,
    )
    .await;
    failing_at(
        MoveStep::Phase(MovePhase::Place),
        finish(fixture.global(), &mut marking),
    )
    .await
    .expect_err("the test fails the place");
    assert_eq!(
        Marker::load(&fixture.default_location()).expect("read"),
        Some(current),
        "the marker already there is kept"
    );
    assert_eq!(
        fixture.setting().map(|setting| setting.id),
        Some(current.id)
    );

    std::fs::rename(fixture.default_location(), &target).expect("the rename");
    let mut placing = record(
        fixture.global(),
        MoveKind::Rename,
        String::new(),
        text(&target),
        MovePhase::Place,
    )
    .await;
    finish(fixture.global(), &mut placing)
        .await
        .expect("a placed root isn't renamed again");
    assert_eq!(fixture.setting().expect("the setting").path, text(&target));
}

/// A create places a new, marked root, switches the setting to it, and makes the redirect. Undone
/// before the switch, the placed root loses its marker, is listed and set aside, and the setting
/// stays missing.
#[tokio::test]
async fn a_create_finishes_or_is_undone_step_by_step() {
    let fixture = Fixture::new();
    let target = fixture.root("parent").join("target");
    let mut created = record(
        fixture.global(),
        MoveKind::Create,
        String::new(),
        text(&target),
        MovePhase::Place,
    )
    .await;
    finish(fixture.global(), &mut created)
        .await
        .expect("the create finishes");
    let setting = fixture.setting().expect("the setting");
    assert_eq!(setting.path, text(&target));
    assert_eq!(
        Marker::load(&target).expect("read").map(|marker| marker.id),
        Some(setting.id)
    );
    assert_eq!(
        std::fs::read_link(fixture.default_location()).expect("the redirect"),
        target
    );

    let fixture = Fixture::new();
    let target = fixture.root("target");
    let mut placing = record(
        fixture.global(),
        MoveKind::Create,
        String::new(),
        text(&target),
        MovePhase::Place,
    )
    .await;
    failing_at(
        MoveStep::Phase(MovePhase::Switch),
        finish(fixture.global(), &mut placing),
    )
    .await
    .expect_err("the test fails the switch");
    assert!(Marker::load(&target).expect("read").is_some(), "placed");
    let assessment = fixture.assess();
    assert_eq!(assessment.direction, Direction::Undo);

    let (undone, steps) = steps_of(undo(fixture.global(), &assessment.record)).await;
    undone.expect("the undo");
    assert_eq!(
        steps,
        [
            UndoStep::Unmark,
            UndoStep::List,
            UndoStep::SetAside,
            UndoStep::Redirect,
            UndoStep::Forget
        ]
        .map(MoveStep::Undo)
        .into_iter()
        .chain([MoveStep::LeftoverUnmarked])
        .collect::<Vec<_>>()
    );
    assert!(!target.exists());
    assert!(fixture.setting().is_none());
    assert!(
        fixture.leftovers().paths.is_empty(),
        "with no setting, what the undo set aside is removed at once"
    );
    assert_eq!(
        std::fs::read_dir(fixture.volume.path())
            .expect("the volume")
            .count(),
        0
    );
    assert!(
        MoveRecord::load(fixture.global())
            .expect("read the record")
            .is_none()
    );
}

/// An undo interrupted after it listed the aside name sets the target aside under that name, and
/// lists nothing more.
#[tokio::test]
async fn an_undo_set_aside_uses_the_name_it_listed() {
    let fixture = Fixture::new();
    let (from, target) = (fixture.root("from"), fixture.root("target"));
    let current = marker(RootId::generate());
    marked_root(&from, current).await;
    marked_root(&target, current).await;
    store_setting(fixture.global(), text(&from), current).await;
    let record = record(
        fixture.global(),
        MoveKind::Rename,
        text(&from),
        text(&target),
        MovePhase::Place,
    )
    .await;
    assert_eq!(fixture.assess().direction, Direction::Undo);
    failing_at(
        MoveStep::Undo(UndoStep::SetAside),
        undo(fixture.global(), &record),
    )
    .await
    .expect_err("the test fails the set-aside");
    let listed = fixture.leftovers().paths;
    assert_eq!(listed.len(), 1);
    assert!(target.is_dir() && !target.join(MARKER).exists());

    undo(fixture.global(), &record)
        .await
        .expect("the undo runs again");
    assert_eq!(fixture.leftovers().paths, listed);
    assert!(leftover_path(fixture.global(), &listed[0]).is_dir());
    assert!(!target.exists());
}

/// Two writers of the leftover list are serialized: each one's entries are kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_writers_of_the_leftover_list_are_serialized() {
    let fixture = Fixture::new();
    let global = fixture.global().to_path_buf();
    let writers = (0..2).map(|writer| {
        let global = global.clone();
        tokio::spawn(async move {
            for entry in 0..10 {
                list_leftover(&global, &global.join(format!("w{writer}-{entry}")), false)
                    .await
                    .expect("list");
            }
        })
    });
    for writer in writers.collect::<Vec<_>>() {
        writer.await.expect("the writer");
    }
    assert_eq!(fixture.leftovers().paths.len(), 20);
}

/// A leftover's removal deletes its marker before its contents, then drops its entry.
#[tokio::test]
async fn a_leftover_loses_its_marker_before_its_contents() {
    let fixture = Fixture::new();
    let leftover = fixture.root("leftover");
    marked_root(&leftover, marker(RootId::generate())).await;
    std::fs::write(leftover.join("contents"), "").expect("contents");
    list_leftover(fixture.global(), &leftover, false)
        .await
        .expect("list");

    let (reached, mut steps) = tokio::sync::mpsc::unbounded_channel::<Reached>();
    let observed = leftover.clone();
    let checking = tokio::spawn(async move {
        let (step, answer) = steps.recv().await.expect("a step");
        assert_eq!(step, MoveStep::LeftoverUnmarked);
        let seen = (
            observed.join(MARKER).exists(),
            observed.join("contents").exists(),
        );
        let _ = answer.send(true);
        seen
    });
    step_moves(reached, remove_leftover(fixture.global(), &text(&leftover)))
        .await
        .expect("the removal");
    assert_eq!(checking.await.expect("the check"), (false, true));
    assert!(!leftover.exists());
    assert!(fixture.leftovers().paths.is_empty());
}

/// A still-marked leftover whose path is absent stays listed with its marker; once its path is
/// present, its marker is deleted and it is listed as unmarked.
#[tokio::test]
async fn a_still_marked_leftover_is_unmarked_once_present() {
    let fixture = Fixture::new();
    let leftover = fixture.root("leftover");
    list_leftover(fixture.global(), &leftover, true)
        .await
        .expect("list");

    unmark_returned_leftovers(fixture.global())
        .await
        .expect("unmark");
    assert_eq!(fixture.leftovers().marked, [text(&leftover)]);

    marked_root(&leftover, marker(RootId::generate())).await;
    unmark_returned_leftovers(fixture.global())
        .await
        .expect("unmark");
    let leftovers = fixture.leftovers();
    assert!(leftovers.marked.is_empty());
    assert_eq!(leftovers.paths, [text(&leftover)]);
    assert!(!leftover.join(MARKER).exists());
}

/// A leftover in the global data directory is listed by its name alone.
#[tokio::test]
async fn a_leftover_in_the_global_data_directory_is_listed_by_name() {
    let fixture = Fixture::new();
    let leftover = fixture.global().join("external.lore-aside-1");
    list_leftover(fixture.global(), &leftover, false)
        .await
        .expect("list");
    assert_eq!(fixture.leftovers().paths, ["external.lore-aside-1"]);
    assert_eq!(
        leftover_path(fixture.global(), "external.lore-aside-1"),
        leftover
    );
}

/// An undo leaves alone a target the move didn't make: one holding no marker, or a marker of
/// another root.
#[tokio::test]
async fn an_undo_leaves_a_target_the_move_did_not_make() {
    for foreign in [None, Some(marker(RootId::generate()))] {
        let fixture = Fixture::new();
        let (from, target) = (fixture.root("from"), fixture.root("target"));
        let current = marker(RootId::generate());
        marked_root(&from, current).await;
        store_setting(fixture.global(), text(&from), current).await;
        std::fs::create_dir(&target).expect("someone else's directory");
        std::fs::write(target.join("theirs"), "kept").expect("their file");
        if let Some(foreign) = foreign {
            foreign
                .save(&target)
                .await
                .expect("a marker of another root");
        }
        let record = record(
            fixture.global(),
            MoveKind::Rename,
            text(&from),
            text(&target),
            MovePhase::Place,
        )
        .await;

        undo(fixture.global(), &record).await.expect("the undo");

        assert!(target.join("theirs").exists(), "{foreign:?}");
        assert!(fixture.leftovers().paths.is_empty(), "{foreign:?}");
        assert!(fixture.record().is_none(), "{foreign:?}");
    }
}

/// A create whose placing was interrupted, leaving its temporary directory, places the new root
/// afresh and leaves no temporary directory.
#[tokio::test]
async fn a_create_after_an_interrupted_placing_places_afresh() {
    let fixture = Fixture::new();
    let target = fixture.root("target");
    let placing = fixture.root("target.lore-placing");
    std::fs::create_dir_all(&placing).expect("the interrupted placing");
    std::fs::write(placing.join("partial"), "").expect("what it made");
    let mut record = record(
        fixture.global(),
        MoveKind::Create,
        String::new(),
        text(&target),
        MovePhase::Place,
    )
    .await;

    finish(fixture.global(), &mut record)
        .await
        .expect("the create");

    assert!(!placing.exists());
    assert!(!target.join("partial").exists());
    assert!(Marker::load(&target).expect("read").is_some());
}
