// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `--adopt` and `--abandon`, and their takeovers of a move that waits for a root, in a global
//! data directory of the test's own, with no service.
// Fixtures plant and copy roots directly, outside any repository.
#![allow(clippy::disallowed_methods)]

use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::global::external_dir::leftovers::Leftovers;
use lore_revision::global::external_dir::marker::Marker;
use lore_revision::global::external_dir::moving::MoveError;
use lore_revision::global::external_dir::moving::Moved;
use lore_revision::global::external_dir::record::MoveKind;
use lore_revision::global::external_dir::record::MovePhase;
use lore_revision::global::external_dir::record::MoveRecord;
use lore_revision::global::external_dir::resolve::Availability;
use lore_revision::global::external_dir::resolve::Unavailable;
use lore_revision::global::external_dir::setting::RootId;
use lore_revision::global::external_dir::setting::Setting;
use lore_revision::global::external_dir::takeover::abandon;
use lore_revision::global::external_dir::takeover::adopt;
use lore_revision::test_util::MoveStep;
use lore_revision::test_util::step_moves;

use super::moving::Fixture;
use crate::helper::execution_in;

impl Fixture {
    pub(crate) async fn adopt(&self, target: &Path) -> Result<Moved, MoveError> {
        self.adopt_with(target, std::future::pending()).await
    }

    pub(crate) async fn adopt_with(
        &self,
        target: &Path,
        termination: impl Future<Output = ()>,
    ) -> Result<Moved, MoveError> {
        LORE_CONTEXT
            .scope(
                execution_in(self.stores.path()),
                adopt(
                    self.global(),
                    target,
                    &self.exclusive(),
                    termination,
                    |_store| {},
                ),
            )
            .await
    }

    pub(crate) async fn abandon(&self, target: &Path) -> Result<Moved, MoveError> {
        self.abandon_with(target, std::future::pending()).await
    }

    pub(crate) async fn abandon_with(
        &self,
        target: &Path,
        termination: impl Future<Output = ()>,
    ) -> Result<Moved, MoveError> {
        LORE_CONTEXT
            .scope(
                execution_in(self.stores.path()),
                abandon(
                    self.global(),
                    target,
                    &self.exclusive(),
                    termination,
                    |_store| {},
                ),
            )
            .await
    }

    pub(crate) fn leftovers(&self) -> Leftovers {
        Leftovers::load(self.global()).expect("read the leftovers")
    }

    /// A first move to a configured root named `name`, which answers it.
    pub(crate) async fn moved_to(&self, name: &str) -> PathBuf {
        self.default_root_with_an_instance();
        let root = self.root(name);
        self.move_to(&root).await.expect("the move");
        root
    }

    /// A move from the configured root `from` to `to`, recorded at its `place` phase and waiting,
    /// since `from` is moved away.
    pub(crate) async fn waiting_rename(&self, from: &Path, to: &Path) -> PathBuf {
        let away = self.root("away");
        std::fs::rename(from, &away).expect("take the root away");
        MoveRecord {
            from: from.display().to_string(),
            to: to.display().to_string(),
            kind: MoveKind::Rename,
            phase: MovePhase::Place,
        }
        .save(self.global())
        .await
        .expect("the record");
        away
    }
}

impl Fixture {
    /// An adopt of a copy of the configured root, recorded at its `switch` phase, whose copy has
    /// gone, as has the configured root, whose volume is lost. Answers the configured root's path
    /// and another copy of it.
    pub(crate) async fn adopt_whose_root_has_gone(&self) -> (PathBuf, PathBuf) {
        let root = self.moved_to("root").await;
        let (gone, other) = (self.root("gone"), self.root("other"));
        copy_root(&root, &other);
        std::fs::rename(&root, self.root("away")).expect("lose the volume");
        MoveRecord {
            from: root.display().to_string(),
            to: gone.display().to_string(),
            kind: MoveKind::Adopt,
            phase: MovePhase::Switch,
        }
        .save(self.global())
        .await
        .expect("the record");
        (root, other)
    }
}

/// A copy of `root` at `copy`, its directories made with access for the user only, as Lore makes
/// a root.
pub(crate) fn copy_root(root: &Path, copy: &Path) {
    lore_io::create_dir_all_private(copy).expect("make the copy");
    for entry in std::fs::read_dir(root).expect("list the root") {
        let entry = entry.expect("an entry");
        let to = copy.join(entry.file_name());
        if entry.file_type().expect("its type").is_dir() {
            copy_root(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).expect("copy a file");
        }
    }
}

/// A private directory at `path` holding `marker`, or none.
async fn marked_dir(path: &Path, marker: Option<Marker>) {
    lore_io::create_dir_all_private(path).expect("the directory");
    if let Some(marker) = marker {
        marker.save(path).await.expect("the marker");
    }
}

fn generation(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("non-zero")
}

/// An adopt of a copy of the root points the setting at the copy and makes the redirect lead
/// there.
#[tokio::test]
async fn an_adopt_takes_a_copy_of_the_root() {
    let fixture = Fixture::new();
    let root = fixture.moved_to("root").await;
    let copy = fixture.root("copy");
    copy_root(&root, &copy);

    let adopted = fixture.adopt(&copy).await.expect("the adopt");

    assert_eq!(adopted.kind, Some(MoveKind::Adopt));
    assert_eq!(
        fixture.availability(),
        Availability::Available(copy.clone())
    );
    assert_eq!(
        std::fs::read_link(fixture.default_location()).expect("the redirect"),
        copy
    );
    assert!(fixture.record().is_none());
}

/// An adopt refuses a directory with no marker, a marker of another root ID or another
/// generation, and, on Unix, one others can write.
#[tokio::test]
async fn an_adopt_refuses_what_isnt_the_current_root() {
    let fixture = Fixture::new();
    let id = RootId::generate();
    let root = fixture.root("root");
    let current = Marker {
        id,
        generation: generation(2),
    };
    marked_dir(&root, Some(current)).await;
    Setting {
        path: root.display().to_string(),
        id,
        generation: generation(2),
    }
    .save(fixture.global())
    .await
    .expect("the setting");

    let unmarked = fixture.root("unmarked");
    marked_dir(&unmarked, None).await;
    let foreign = fixture.root("foreign");
    marked_dir(
        &foreign,
        Some(Marker {
            id: RootId::generate(),
            generation: generation(2),
        }),
    )
    .await;
    let earlier = fixture.root("earlier");
    marked_dir(
        &earlier,
        Some(Marker {
            id,
            generation: generation(1),
        }),
    )
    .await;
    #[cfg_attr(not(target_family = "unix"), allow(unused_mut))]
    let mut refused = vec![unmarked, foreign, earlier, fixture.root("missing")];
    #[cfg(target_family = "unix")]
    {
        use std::os::unix::fs::PermissionsExt;
        let shared = fixture.root("shared");
        marked_dir(&shared, Some(current)).await;
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).expect("chmod");
        refused.push(shared);
    }

    for target in refused {
        let refusal = fixture.adopt(&target).await;
        assert!(
            matches!(&refusal, Err(err) if err.is_invalid_arguments()),
            "{}: {refusal:?}",
            target.display()
        );
    }
    assert_eq!(fixture.availability(), Availability::Available(root));
}

/// With `external-root` lost, an adopt takes the marker's root ID and generation and writes the
/// setting again.
#[tokio::test]
async fn an_adopt_writes_a_lost_setting_again() {
    let fixture = Fixture::new();
    let root = fixture.moved_to("root").await;
    let setting = fixture.setting().expect("the setting");
    std::fs::remove_file(fixture.global().join("external-root")).expect("lose the setting");
    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::RedirectWithoutSetting)
    );

    fixture.adopt(&root).await.expect("the adopt");

    assert_eq!(fixture.setting(), Some(setting));
    assert_eq!(fixture.availability(), Availability::Available(root));
}

/// The adopted root is taken off the leftover list.
#[tokio::test]
async fn an_adopted_root_leaves_the_leftover_list() {
    let fixture = Fixture::new();
    let root = fixture.moved_to("root").await;
    let copy = fixture.root("copy");
    copy_root(&root, &copy);
    Leftovers {
        paths: vec![copy.display().to_string()],
        marked: Vec::new(),
    }
    .save(fixture.global())
    .await
    .expect("list the copy");

    fixture.adopt(&copy).await.expect("the adopt");

    assert!(fixture.leftovers().paths.is_empty());
    assert!(copy.join(".lore-external-root").exists());
}

/// An abandon refuses while the root is available, and while `external-root` is missing.
#[tokio::test]
async fn an_abandon_refuses_an_available_root_or_no_setting() {
    let fixture = Fixture::new();
    fixture.default_root_with_an_instance();
    assert!(
        fixture
            .abandon(&fixture.root("new"))
            .await
            .expect_err("no setting")
            .is_invalid_arguments()
    );

    let root = fixture.moved_to("root").await;
    assert!(
        fixture
            .abandon(&fixture.root("new"))
            .await
            .expect_err("an available root")
            .is_invalid_arguments()
    );
    assert_eq!(fixture.availability(), Availability::Available(root));
    assert!(!fixture.root("new").exists());
}

/// An abandon makes a new, empty root of the next generation, and the abandoned root stays
/// unavailable when it returns.
#[tokio::test]
async fn an_abandon_makes_a_new_root_of_the_next_generation() {
    let fixture = Fixture::new();
    let root = fixture.moved_to("root").await;
    let away = fixture.root("away");
    std::fs::rename(&root, &away).expect("lose the volume");
    let new = fixture.root("new");

    let abandoned = fixture.abandon(&new).await.expect("the abandon");

    assert_eq!(abandoned.kind, Some(MoveKind::Abandon));
    assert_eq!(
        fixture.setting().expect("the setting").generation,
        generation(2)
    );
    assert_eq!(fixture.availability(), Availability::Available(new.clone()));
    assert_eq!(
        std::fs::read_dir(&new).expect("the new root").count(),
        1,
        "only the marker"
    );

    std::fs::rename(&away, &root).expect("the volume returns");
    assert_eq!(fixture.availability(), Availability::Available(new));
    assert!(
        !lore_revision::global::external_dir::resolve::root_available(
            fixture.global(),
            &root.display().to_string(),
            fixture.setting().as_ref()
        ),
        "the abandoned root is of an earlier generation"
    );
}

/// Each takeover of a rename that waits for a root lists what it gives up before it replaces the
/// record: failed there, the record is the rename's, and the leftovers are listed.
#[tokio::test]
async fn a_takeover_lists_what_it_gives_up_before_replacing_the_record() {
    for adopting in [true, false] {
        let fixture = Fixture::new();
        let root = fixture.moved_to("root").await;
        let copy = fixture.root("copy");
        copy_root(&root, &copy);
        let target = fixture.root("target");
        fixture.waiting_rename(&root, &target).await;
        let replacement = if adopting {
            copy.clone()
        } else {
            fixture.root("new")
        };

        let (reached, mut steps) =
            tokio::sync::mpsc::unbounded_channel::<(MoveStep, tokio::sync::oneshot::Sender<bool>)>(
            );
        let failing = tokio::spawn(async move {
            while let Some((step, answer)) = steps.recv().await {
                let _ = answer.send(step != MoveStep::TakeoverListed);
            }
        });
        let takeover = async {
            if adopting {
                fixture.adopt(&replacement).await
            } else {
                fixture.abandon(&replacement).await
            }
        };
        assert!(step_moves(reached, takeover).await.is_err());
        failing.abort();

        assert_eq!(
            fixture.record().expect("the record").kind,
            MoveKind::Rename,
            "the rename's record stays"
        );
        let mut listed = fixture.leftovers().marked;
        listed.sort();
        let mut expected = if adopting {
            vec![root.display().to_string(), target.display().to_string()]
        } else {
            vec![target.display().to_string()]
        };
        expected.sort();
        assert_eq!(listed, expected, "adopting: {adopting}");
    }
}

/// Each takeover of a rename that waits for a root replaces its record and finishes: an adopt
/// switches to the copy, and an abandon to a new root of the next generation.
#[tokio::test]
async fn each_takeover_of_a_waiting_rename_finishes() {
    for adopting in [true, false] {
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

        let taken = if adopting {
            fixture.adopt(&replacement).await
        } else {
            fixture.abandon(&replacement).await
        }
        .expect("the takeover");

        assert_eq!(
            taken.kind,
            Some(if adopting {
                MoveKind::Adopt
            } else {
                MoveKind::Abandon
            })
        );
        assert!(fixture.record().is_none());
        assert_eq!(fixture.availability(), Availability::Available(replacement));
    }
}

/// An abandon refuses to take over a waiting move whose old root is available, which an adopt
/// takes instead, and leaves the record as it is.
#[tokio::test]
async fn an_abandon_refuses_a_waiting_move_whose_old_root_is_available() {
    let fixture = Fixture::new();
    let root = fixture.moved_to("root").await;
    let target = fixture.root("target");
    copy_root(&root, &target);
    let setting = fixture.setting().expect("the setting");
    Setting {
        path: target.display().to_string(),
        ..setting
    }
    .save(fixture.global())
    .await
    .expect("the setting names the target");
    let away = fixture.root("away");
    std::fs::rename(&target, &away).expect("the target's volume goes");
    MoveRecord {
        from: root.display().to_string(),
        to: target.display().to_string(),
        kind: MoveKind::Rename,
        phase: MovePhase::Redirect,
    }
    .save(fixture.global())
    .await
    .expect("the record");

    let refused = fixture.abandon(&fixture.root("new")).await;

    assert!(refused.expect_err("refused").is_invalid_arguments());
    assert_eq!(
        fixture.record().map(|record| record.kind),
        Some(MoveKind::Rename)
    );
    assert!(!fixture.root("new").exists());
}

/// An adopt of the current root changes nothing.
#[tokio::test]
async fn an_adopt_of_the_current_root_changes_nothing() {
    let fixture = Fixture::new();
    let root = fixture.moved_to("root").await;
    let setting = fixture.setting();

    let adopted = fixture.adopt(&root).await.expect("the adopt");

    assert_eq!(adopted.kind, None);
    assert_eq!(fixture.setting(), setting);
    assert!(fixture.record().is_none());
}

/// An adopt of a copy holding an external directory whose config can't be read fails before it
/// records anything, and leaves the current root in place.
#[tokio::test]
async fn an_adopt_of_a_root_with_an_unreadable_config_records_nothing() {
    let fixture = Fixture::new();
    let root = fixture.moved_to("root").await;
    let copy = fixture.root("copy");
    copy_root(&root, &copy);
    let dot_lore = copy
        .join(lore_revision::instance::InstanceId::generate().text_encoding())
        .join(".lore");
    std::fs::create_dir_all(&dot_lore).expect("the external directory");
    std::fs::write(dot_lore.join("config.toml"), "remote_url = [").expect("the config");

    fixture.adopt(&copy).await.expect_err("the adopt");

    assert!(fixture.record().is_none());
    assert_eq!(fixture.availability(), Availability::Available(root));
}

/// An adopt refuses a link or a junction to the current root, which names the root without being
/// one, and changes nothing.
#[tokio::test]
async fn an_adopt_refuses_a_link_to_the_root() {
    let fixture = Fixture::new();
    let root = fixture.moved_to("root").await;
    let link = fixture.root("link");
    #[cfg(target_family = "unix")]
    std::os::unix::fs::symlink(&root, &link).expect("the link");
    #[cfg(target_family = "windows")]
    lore_io::junction::create_junction(&root, &link).expect("the junction");

    let refusal = fixture.adopt(&link).await;

    assert!(
        matches!(&refusal, Err(err) if err.is_invalid_arguments()),
        "{refusal:?}"
    );
    assert_eq!(
        fixture.setting().expect("the setting").path,
        root.display().to_string()
    );
}

/// An abandon to the default location, which holds the command's redirect to the lost root,
/// replaces the redirect with the new root.
#[tokio::test]
async fn an_abandon_to_the_default_location_replaces_the_redirect() {
    let fixture = Fixture::new();
    let root = fixture.moved_to("root").await;
    std::fs::rename(&root, fixture.root("away")).expect("lose the volume");

    fixture
        .abandon(&fixture.default_location())
        .await
        .expect("the abandon");

    assert_eq!(
        fixture.availability(),
        Availability::Available(fixture.default_location())
    );
    let setting = fixture.setting().expect("the setting");
    assert_eq!((setting.path.as_str(), setting.generation.get()), ("", 2));
}

/// Each takeover replaces an adopt whose root has gone, and finishes, giving up no root: an adopt
/// switches to another copy, and an abandon to a new root of the next generation.
#[tokio::test]
async fn each_takeover_replaces_an_adopt_whose_root_has_gone() {
    for adopting in [true, false] {
        let fixture = Fixture::new();
        let (_, other) = fixture.adopt_whose_root_has_gone().await;
        let replacement = if adopting { other } else { fixture.root("new") };

        let taken = if adopting {
            fixture.adopt(&replacement).await
        } else {
            fixture.abandon(&replacement).await
        }
        .expect("the takeover");

        assert_eq!(
            taken.kind,
            Some(if adopting {
                MoveKind::Adopt
            } else {
                MoveKind::Abandon
            }),
            "adopting: {adopting}"
        );
        assert!(fixture.record().is_none(), "adopting: {adopting}");
        assert_eq!(fixture.availability(), Availability::Available(replacement));
        assert_eq!(
            fixture.leftovers(),
            Leftovers::default(),
            "adopting: {adopting}"
        );
    }
}

/// A takeover refuses an adopt whose root is there but can't be read, here a directory with no
/// marker and a path under a file, and leaves its record as it is.
#[tokio::test]
async fn a_takeover_refuses_an_adopt_whose_root_fails_to_read() {
    for under_a_file in [false, true] {
        let fixture = Fixture::new();
        let (_, other) = fixture.adopt_whose_root_has_gone().await;
        let unreadable = if under_a_file {
            std::fs::write(fixture.root("file"), "").expect("a file");
            fixture.root("file").join("root")
        } else {
            lore_io::create_dir_all_private(&fixture.root("unmarked")).expect("a directory");
            fixture.root("unmarked")
        };
        let mut record = fixture.record().expect("the record");
        record.to = unreadable.display().to_string();
        record.save(fixture.global()).await.expect("the record");

        let adopted = fixture.adopt(&other).await;

        assert!(adopted.is_err(), "under a file: {under_a_file}");
        assert_eq!(
            fixture.record(),
            Some(record),
            "under a file: {under_a_file}"
        );
    }
}

/// An adopt cancelled while it waits for a store another holder took fails with nothing changed:
/// no move record, the setting as it was, and no leftover listed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_adopt_cancelled_while_it_waits_for_a_store_changes_nothing() {
    let fixture = Fixture::new();
    let root = fixture.moved_to("root").await;
    let (store, _) = fixture.plant_virtual_instance(&root);
    let copy = fixture.root("copy");
    copy_root(&root, &copy);
    let setting = fixture.setting();
    let _other =
        lore_base::fs::lock::FSLock::try_acquire_exact_path(&store.join("immutable").join("lock"))
            .await
            .expect("look at the flock")
            .expect("hold the store");

    let adopted = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        fixture.adopt_with(
            &copy,
            tokio::time::sleep(std::time::Duration::from_millis(300)),
        ),
    )
    .await
    .expect("the cancel ends the adopt");

    assert!(adopted.expect_err("cancelled").is_internal());
    assert!(fixture.record().is_none(), "no move is recorded");
    assert_eq!(fixture.setting(), setting);
    assert_eq!(fixture.leftovers(), Leftovers::default());
}

/// Each takeover of a copy that waits for its old root's volume replaces its record and
/// finishes, and lists the copy's temporary directory, with what else it gives up, as a
/// leftover.
#[tokio::test]
async fn each_takeover_of_a_waiting_copy_finishes() {
    use lore_revision::global::external_dir::recovery::copying_path;

    for adopting in [true, false] {
        let fixture = Fixture::new();
        let root = fixture.moved_to("root").await;
        let copy = fixture.root("copy");
        copy_root(&root, &copy);
        let target = fixture.root("target");
        let copying = copying_path(&target);
        std::fs::create_dir_all(copying.join("partial")).expect("a partial copy");
        std::fs::rename(&root, fixture.root("away")).expect("the old root's volume goes");
        MoveRecord {
            from: root.display().to_string(),
            to: target.display().to_string(),
            kind: MoveKind::Copy,
            phase: MovePhase::Copy,
        }
        .save(fixture.global())
        .await
        .expect("the record");
        let replacement = if adopting {
            copy.clone()
        } else {
            fixture.root("new")
        };

        let taken = if adopting {
            fixture.adopt(&replacement).await
        } else {
            fixture.abandon(&replacement).await
        }
        .expect("the takeover");

        assert!(taken.kind.is_some(), "adopting: {adopting}");
        assert!(fixture.record().is_none());
        assert_eq!(fixture.availability(), Availability::Available(replacement));
        let leftovers = fixture.leftovers();
        let listed = copying.display().to_string();
        assert!(
            leftovers.paths.contains(&listed) || leftovers.marked.contains(&listed),
            "adopting: {adopting}"
        );
    }
}

/// An abandon of a copy back to the default location that waits for its old root's volume lists
/// the copy's temporary directory as a leftover, in a global data directory whose path isn't
/// UTF-8.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_abandon_gives_up_a_waiting_copys_directory_on_a_path_that_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt;

    use lore_revision::global::external_dir::recovery::copying_path;
    use lore_revision::global::external_dir::recovery::leftover_path;

    let base = crate::helper::verified_temp_dir("lore-external-takeover-not-utf8-");
    let parent = base
        .path()
        .join(std::ffi::OsStr::from_bytes(b"not-\xFF-utf8"));
    std::fs::create_dir(&parent).expect("the parent");
    let fixture = Fixture {
        global: lore_base::test_util::TempDir::new_in(&parent, "lore-external-move-global-"),
        ..Fixture::new()
    };
    let root = fixture.moved_to("root").await;
    let copying = copying_path(&fixture.default_location());
    std::fs::create_dir_all(copying.join("partial")).expect("a partial copy");
    std::fs::rename(&root, fixture.root("away")).expect("the old root's volume goes");
    MoveRecord {
        from: root.display().to_string(),
        to: String::new(),
        kind: MoveKind::Copy,
        phase: MovePhase::Copy,
    }
    .save(fixture.global())
    .await
    .expect("the record");

    fixture
        .abandon(&fixture.root("new"))
        .await
        .expect("the abandon");

    let leftovers = fixture.leftovers();
    assert!(
        leftovers
            .paths
            .iter()
            .chain(&leftovers.marked)
            .any(|entry| leftover_path(fixture.global(), entry) == copying),
        "{leftovers:?}"
    );
}

/// An adopt of the finished copy that a waiting copy made, its temporary directory, gives up the
/// move's old root but never the copy it adopts: failed once it listed what it gives up, the
/// adopted directory isn't listed as a leftover, which a service start would remove.
#[tokio::test]
async fn an_adopt_of_a_waiting_copys_directory_never_lists_it() {
    use lore_revision::global::external_dir::recovery::copying_path;

    let fixture = Fixture::new();
    let root = fixture.moved_to("root").await;
    let target = fixture.root("target");
    let copying = copying_path(&target);
    copy_root(&root, &copying);
    std::fs::rename(&root, fixture.root("away")).expect("the old root's volume goes");
    MoveRecord {
        from: root.display().to_string(),
        to: target.display().to_string(),
        kind: MoveKind::Copy,
        phase: MovePhase::Place,
    }
    .save(fixture.global())
    .await
    .expect("the record");
    let (reached, mut steps) =
        tokio::sync::mpsc::unbounded_channel::<(MoveStep, tokio::sync::oneshot::Sender<bool>)>();
    let failing = tokio::spawn(async move {
        while let Some((step, answer)) = steps.recv().await {
            let _ = answer.send(step != MoveStep::TakeoverListed);
        }
    });

    assert!(step_moves(reached, fixture.adopt(&copying)).await.is_err());
    failing.abort();

    let leftovers = fixture.leftovers();
    let adopted = copying.display().to_string();
    assert!(!leftovers.marked.contains(&adopted), "{leftovers:?}");
    assert!(!leftovers.paths.contains(&adopted), "{leftovers:?}");
    assert!(
        leftovers.marked.contains(&root.display().to_string()),
        "the old root is given up: {leftovers:?}"
    );
}
