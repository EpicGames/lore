// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A move of the external root by copy, forced through the test override of the choice between
//! rename and copy, in a global data directory of the test's own, with no service.
// Fixtures plant roots, external directories and stores directly, outside any repository.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use lore_base::fs::lock::FSLock;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::fs::walk::Progress;
use lore_revision::global::external_dir::check::require_copy_space;
use lore_revision::global::external_dir::marker::MARKER;
use lore_revision::global::external_dir::moving::*;
use lore_revision::global::external_dir::record::MoveKind;
use lore_revision::global::external_dir::record::MovePhase;
use lore_revision::global::external_dir::recovery::copying_path;
use lore_revision::global::external_dir::recovery::leftover_path;
use lore_revision::global::external_dir::recovery::remove_leftover;
use lore_revision::global::external_dir::resolve::Availability;
use lore_revision::global::external_dir::resolve::resolve_in;
use lore_revision::test_util::MoveOverride;
use lore_revision::test_util::MoveStep;
use lore_revision::test_util::override_moves;
use lore_revision::test_util::step_moves;

use super::moving::Fixture;
use crate::helper::execution_in;

impl Fixture {
    /// Moves the root to `target`, which copies, until `termination`, telling `on_held` each held
    /// store and `on_copying` each external directory the copy reaches.
    async fn copy_with(
        &self,
        target: &Path,
        termination: impl Future<Output = ()>,
        on_held: impl FnMut(&Path) + Send,
        on_copying: impl Fn(&str) + Send + Sync + 'static,
    ) -> Result<Moved, MoveError> {
        LORE_CONTEXT
            .scope(
                execution_in(self.stores.path()),
                Box::pin(move_root(
                    self.global(),
                    target,
                    &self.exclusive(),
                    termination,
                    on_held,
                    on_copying,
                )),
            )
            .await
    }

    /// The leftovers listed in `paths`, as paths.
    fn listed_leftovers(&self) -> Vec<PathBuf> {
        self.leftovers()
            .paths
            .iter()
            .map(|entry| leftover_path(self.global(), entry))
            .collect()
    }
}

/// A move by copy from the default location, between two configured roots, and back: the
/// external directories are copied, the setting names the copy and the redirect leads to it, and
/// what is left of each old root is set aside as a leftover, with no marker. The copy holds no
/// marker while it is copied, and no temporary directory is left.
#[tokio::test]
async fn a_copy_moves_the_root_between_roots_and_back() {
    let fixture = Fixture::new();
    let _copying = override_moves(fixture.global(), MoveOverride::Copy);
    let name = fixture.default_root_with_an_instance();
    let staging = fixture.default_location().join(&name).join("write-staging");
    std::fs::create_dir(&staging).expect("the staging directory");
    std::fs::write(staging.join("staged"), "staged").expect("a staged file");
    let (first, second) = (fixture.root("first"), fixture.root("second"));
    let mut from = fixture.default_location();

    for target in [first.clone(), second.clone(), fixture.default_location()] {
        let reported = Arc::new(Mutex::new(Vec::new()));
        let (recorded, copying) = (Arc::clone(&reported), copying_path(&target));
        let moved = fixture
            .copy_with(
                &target,
                std::future::pending(),
                |_store| {},
                move |copied| {
                    let marked = copying.join(MARKER).exists();
                    recorded
                        .lock()
                        .expect("lock")
                        .push((copied.to_string(), marked));
                },
            )
            .await
            .expect("the move");

        assert_eq!(moved.kind, Some(MoveKind::Copy));
        assert_eq!(
            *reported.lock().expect("lock"),
            [(name.clone(), false)],
            "each external directory is reported, while the copy holds no marker"
        );
        assert_eq!(
            fixture.availability(),
            Availability::Available(target.clone())
        );
        assert_eq!(
            std::fs::read_to_string(target.join(&name).join("file")).expect("copied"),
            "kept"
        );
        assert!(target.join(&name).join("write-staging").is_dir());
        #[cfg(target_os = "linux")]
        assert!(
            !target
                .join(&name)
                .join("write-staging")
                .join("staged")
                .exists(),
            "what FUSE staged isn't copied"
        );
        assert!(!copying_path(&target).exists());
        assert!(fixture.record().is_none());
        let set_aside = fixture.listed_leftovers();
        let aside = set_aside
            .iter()
            .find(|aside| aside.parent() == from.parent())
            .expect("the old root is set aside");
        assert!(aside.join(&name).join("file").exists());
        assert!(!aside.join(MARKER).exists(), "a leftover holds no marker");
        assert!(!from.exists() || from == fixture.default_location());
        from = target;
    }
    assert!(
        !std::fs::symlink_metadata(fixture.default_location())
            .expect("the root")
            .file_type()
            .is_symlink()
    );
    assert!(fixture.setting().expect("the setting").path.is_empty());
    assert!(!first.exists() && !second.exists());
}

/// A move by copy gives the root it moves access for its owner only before it copies it, so a root
/// whose mode was widened since it was placed is placed at `0o700`, and the old root is left so.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_copy_places_a_root_with_access_for_its_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let _copying = override_moves(fixture.global(), MoveOverride::Copy);
    fixture.default_root_with_an_instance();
    let (first, second) = (fixture.root("first"), fixture.root("second"));
    fixture.move_to(&first).await.expect("the first move");
    std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o755)).expect("widen");

    fixture.move_to(&second).await.expect("the move");

    let mode = |path: &Path| std::fs::metadata(path).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode(&second), 0o700);
    let set_aside = fixture.listed_leftovers();
    let aside = set_aside
        .iter()
        .find(|aside| aside.parent() == first.parent())
        .expect("the old root is set aside");
    assert_eq!(mode(aside), 0o700);
}

/// The temporary directory of a copy holds the setting's marker once its copy is done, before
/// the copy is placed, and the target holds it once placed.
#[tokio::test]
async fn a_copy_is_marked_as_its_last_step_before_it_is_placed() {
    let fixture = Fixture::new();
    let _copying = override_moves(fixture.global(), MoveOverride::Copy);
    fixture.default_root_with_an_instance();
    let target = fixture.root("target");
    let (reached, mut steps) =
        tokio::sync::mpsc::unbounded_channel::<(MoveStep, tokio::sync::oneshot::Sender<bool>)>();
    let copying = copying_path(&target);
    let checked = target.clone();
    let answering = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some((step, answer)) = steps.recv().await {
            if let MoveStep::Phase(phase) = step {
                seen.push((
                    phase,
                    copying.join(MARKER).exists(),
                    checked.join(MARKER).exists(),
                ));
            }
            let _ = answer.send(true);
        }
        seen
    });

    step_moves(reached, fixture.move_to(&target))
        .await
        .expect("the move");

    let seen = answering.await.expect("the answers");
    let at = |phase| {
        seen.iter()
            .find(|(seen, ..)| *seen == phase)
            .map(|&(_, copy, target)| (copy, target))
    };
    assert_eq!(at(MovePhase::Copy), Some((false, false)));
    assert_eq!(at(MovePhase::Place), Some((true, false)));
    assert_eq!(at(MovePhase::Switch), Some((false, true)));
}

/// A termination while the copy runs stops it and undoes the move: the old root stays, and what
/// was copied, held once it had copied an entry until it was stopped, is set aside with no marker
/// and short of the source.
#[tokio::test]
async fn a_cancel_during_the_copy_undoes_it() {
    let fixture = Fixture::new();
    let _copying = override_moves(fixture.global(), MoveOverride::CopyHeld);
    let name = fixture.default_root_with_an_instance();
    for file in 0..200 {
        std::fs::write(
            fixture
                .default_location()
                .join(&name)
                .join(format!("f{file}")),
            "f",
        )
        .expect("a file");
    }
    let target = fixture.root("target");
    let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
    let cancel = Arc::new(Mutex::new(Some(cancel)));

    let refused = fixture
        .copy_with(
            &target,
            async {
                let _ = cancelled.await;
            },
            |_store| {},
            move |_copied| {
                if let Some(cancel) = cancel.lock().expect("lock").take() {
                    let _ = cancel.send(());
                }
            },
        )
        .await;

    assert!(refused.expect_err("cancelled").is_internal());
    assert!(fixture.record().is_none());
    assert!(!target.exists());
    assert!(!copying_path(&target).exists());
    assert_eq!(
        fixture.availability(),
        Availability::Available(fixture.default_location())
    );
    let source = fixture.default_location().join(&name);
    assert!(source.join("file").exists());
    let entries = |directory: &Path| std::fs::read_dir(directory).expect("listed").count();
    let set_aside = fixture.listed_leftovers();
    for aside in &set_aside {
        assert!(!aside.join(MARKER).exists());
    }
    let copied: Vec<usize> = set_aside
        .iter()
        .map(|aside| aside.join(&name))
        .filter(|copied| copied.is_dir())
        .map(|copied| entries(&copied))
        .collect();
    assert!(
        !copied.is_empty() && copied.iter().all(|&copied| copied < entries(&source)),
        "the copy is stopped short of the source's {} entries: {copied:?}",
        entries(&source)
    );
}

/// Gives the external directory `instance` a read-only file and, on Windows, a file whose access
/// control list denies everyone, this account among them, writing its data, its extended
/// attributes and its attributes, and deleting it, which leaves it readable.
fn protect_files_in(instance: &Path) {
    let read_only = instance.join("read-only");
    std::fs::write(&read_only, "read-only").expect("a file");
    let mut permissions = std::fs::metadata(&read_only).expect("stat").permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&read_only, permissions).expect("read-only");
    #[cfg(target_family = "windows")]
    {
        let denied = instance.join("denied");
        std::fs::write(&denied, "denied").expect("a file");
        let status = std::process::Command::new("icacls")
            .arg(&denied)
            .args(["/deny", "*S-1-1-0:(WD,AD,WEA,WA,DE)"])
            .status()
            .expect("icacls");
        assert!(status.success(), "deny write and delete");
    }
}

/// What a move by copy sets aside is removed as a leftover even where it holds a read-only file
/// and, on Windows, a file whose access control list denies this account delete and write: a copy
/// that isn't placed, as a move failed at its place undoes it, and the old root a copy replaced.
#[tokio::test]
async fn leftovers_of_a_copy_holding_protected_files_are_removed() {
    let fixture = Fixture::new();
    let _copying = override_moves(fixture.global(), MoveOverride::Copy);
    let name = fixture.default_root_with_an_instance();
    protect_files_in(&fixture.default_location().join(&name));
    let target = fixture.root("target");
    let (reached, mut steps) =
        tokio::sync::mpsc::unbounded_channel::<(MoveStep, tokio::sync::oneshot::Sender<bool>)>();
    let answering = tokio::spawn(async move {
        while let Some((step, answer)) = steps.recv().await {
            let _ = answer.send(step != MoveStep::Phase(MovePhase::Place));
        }
    });
    step_moves(reached, fixture.move_to(&target))
        .await
        .expect_err("the place fails");
    answering.await.expect("the answers");
    fixture
        .copy_with(&target, std::future::pending(), |_store| {}, |_copied| {})
        .await
        .expect("the move");
    let set_aside = fixture.listed_leftovers();
    assert_eq!(set_aside.len(), 2, "{set_aside:?}");
    for aside in &set_aside {
        assert!(aside.join(&name).join("read-only").exists(), "{aside:?}");
    }

    for entry in fixture.leftovers().paths {
        remove_leftover(fixture.global(), &entry)
            .await
            .expect("the leftover is removed");
    }

    for aside in &set_aside {
        assert!(!aside.exists(), "{aside:?}");
    }
    assert!(fixture.leftovers().paths.is_empty());
}

/// Before a copy starts, a store held open is named, as the registered-path check reads the
/// registrations under the store locks, and the copy starts once it is free. The store is matched
/// by the stores directory's random name, as on Windows its path may mix separators.
#[tokio::test]
async fn a_held_store_is_reported_before_the_copy() {
    let fixture = Fixture::new();
    let _copying = override_moves(fixture.global(), MoveOverride::Copy);
    std::fs::create_dir(fixture.default_location()).expect("the default root");
    let (store, instance) = fixture.plant_virtual_instance(&fixture.default_location());
    let held = FSLock::try_acquire_exact_path(&store.join("immutable").join("lock"))
        .await
        .expect("look at the lock")
        .expect("the store is free to hold");
    let held = Mutex::new(Some(held));
    let stores = fixture
        .stores
        .path()
        .file_name()
        .expect("the stores' directory has a name")
        .to_string_lossy()
        .into_owned();
    let events = Arc::new(Mutex::new(Vec::new()));
    let copied_events = Arc::clone(&events);
    let target = fixture.root("target");

    fixture
        .copy_with(
            &target,
            std::future::pending(),
            |named| {
                let named = if named.to_string_lossy().contains(&stores) {
                    "held a store in the stores' directory".to_string()
                } else {
                    format!("held {}", named.display())
                };
                events.lock().expect("lock").push(named);
                held.lock().expect("lock").take();
            },
            move |copied| {
                copied_events
                    .lock()
                    .expect("lock")
                    .push(format!("copying {copied}"));
            },
        )
        .await
        .expect("the move");

    assert_eq!(
        *events.lock().expect("lock"),
        [
            "held a store in the stores' directory".to_string(),
            format!("copying {}", instance.text_encoding())
        ]
    );
    assert_eq!(fixture.availability(), Availability::Available(target));
}

/// A copy to a volume with less free space than the root takes is refused before anything stops,
/// through the test point that reads the target's volume as full, and the root takes what its
/// files take otherwise.
#[test]
fn a_full_target_volume_refuses_a_copy() {
    let fixture = Fixture::new();
    fixture.default_root_with_an_instance();
    let target = fixture.root("target");
    let state = resolve_in(fixture.global().to_path_buf());
    {
        let _copying = override_moves(fixture.global(), MoveOverride::Copy);
        let needed = require_copy_space(&state, &target, &Progress::default()).expect("room");
        assert_eq!(needed.file_count, 1);
    }

    let _full = override_moves(fixture.global(), MoveOverride::CopyToFullVolume);
    let refused = require_copy_space(&state, &target, &Progress::default());

    assert!(refused.expect_err("a full volume").is_invalid_arguments());
}

/// A copy of an external root leaves out the root's marker, and on Linux the contents of each
/// external directory's `write-staging/`, which it keeps, and names each external directory it
/// reaches, and no directory below one.
#[test]
fn a_copy_of_a_root_leaves_out_its_marker_and_what_fuse_staged() {
    use lore_io::clone::Kind;
    use lore_revision::fs::walk::Visit;
    use lore_revision::global::external_dir::recovery::copied_visit;

    let (reached, mut names) = tokio::sync::mpsc::unbounded_channel();
    let visit = |relative: &str, kind| copied_visit(Path::new(relative), kind, &reached);

    assert_eq!(visit(MARKER, Kind::File), Visit::Skip);
    assert_eq!(visit("instance", Kind::Directory), Visit::EachEntry);
    assert_eq!(visit("instance/.lore", Kind::Directory), Visit::EachEntry);
    assert_eq!(
        visit(&format!("instance/{MARKER}"), Kind::File),
        Visit::Clone
    );
    assert_eq!(
        visit("instance/write-staging", Kind::Directory),
        Visit::EachEntry
    );
    let staged = visit("instance/write-staging/staged", Kind::File);
    #[cfg(target_os = "linux")]
    assert_eq!(staged, Visit::Skip);
    #[cfg(not(target_os = "linux"))]
    assert_eq!(staged, Visit::Clone);
    drop(reached);

    let mut reported = Vec::new();
    while let Ok(name) = names.try_recv() {
        reported.push(name);
    }
    assert_eq!(reported, ["instance"]);
}
