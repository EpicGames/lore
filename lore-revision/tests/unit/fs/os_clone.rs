// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures build filesystem state directly; what these test is how a clone reads it.
#![allow(clippy::disallowed_methods)]

//! What a clone of an on-disk instance leaves out, what the OS provider answers for one, and the
//! kind of error each failure of one reports.

use std::path::Path;

use lore_base::test_util::TempDir;
use lore_io::clone::Kind;
use lore_io::clone::Visit;
use lore_revision::fs::filesystem_provider::clone_io_failure;
use lore_revision::fs::os::clone::clone_tree;
use lore_revision::fs::os::clone::directories_above_nested_repositories;
use lore_revision::fs::os::clone::visit;
use lore_revision::fs::os::clone::visit_tree;

#[test]
fn a_clone_leaves_out_the_lore_directorys_transient_state() {
    let dot = Path::new(".lore");

    for skipped in [
        ".lore/instance",
        ".lore/clone-in-progress",
        ".lore/records/no-store",
        ".lore/lock",
        ".lore/service",
        ".lore/config.toml.lock",
        ".lore/config.toml.tmp",
        ".lore/view.~loretemp",
        ".lore/records/01.tmp",
        ".lore/.clone-probe-123-0",
    ] {
        assert_eq!(
            visit(dot, Path::new(skipped), Kind::File),
            Visit::Skip,
            "{skipped}"
        );
    }
    for kept in [".lore/config.toml", ".lore/records/01", ".lore/view"] {
        assert_eq!(
            visit(dot, Path::new(kept), Kind::File),
            Visit::Clone,
            "{kept}"
        );
    }
}

#[test]
fn a_clone_walks_the_lore_directory_and_clones_the_tree_whole() {
    let dot = Path::new(".lore");

    assert_eq!(
        visit(dot, Path::new(".lore"), Kind::Directory),
        Visit::EachEntry
    );
    assert_eq!(
        visit(dot, Path::new(".lore/records"), Kind::Directory),
        Visit::EachEntry
    );
    assert_eq!(visit(dot, Path::new("src"), Kind::Directory), Visit::Clone);
    assert_eq!(visit(dot, Path::new("src/lock"), Kind::File), Visit::Clone);
    assert_eq!(
        visit(dot, Path::new("build.~loretemp"), Kind::File),
        Visit::Clone
    );
    assert_eq!(visit(dot, Path::new("notes.tmp"), Kind::File), Visit::Clone);
}

/// A failure keeps the kind a caller can act on: a missing path, a path taken, an entry that
/// cannot be cloned, and a full volume or quota.
#[test]
fn a_clone_failure_reports_its_kind() {
    let failure = |kind| clone_io_failure(std::io::Error::from(kind), "cloning");

    assert!(failure(std::io::ErrorKind::NotFound).is_file_not_found());
    assert!(failure(std::io::ErrorKind::AlreadyExists).is_invalid_arguments());
    assert!(failure(std::io::ErrorKind::Unsupported).is_not_supported());
    assert!(failure(std::io::ErrorKind::StorageFull).is_oversized());
    assert!(failure(std::io::ErrorKind::QuotaExceeded).is_oversized());
    assert!(failure(std::io::ErrorKind::FileTooLarge).is_oversized());
    assert!(failure(std::io::ErrorKind::PermissionDenied).is_internal());
}

/// On APFS, which macOS volumes use, the OS provider answers that a clone shares blocks.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn the_os_provider_answers_that_a_clone_on_apfs_shares_blocks() {
    use lore_revision::fs::filesystem_provider::CloneAnswer;
    use lore_revision::fs::filesystem_provider::FilesystemProvider;
    use lore_revision::fs::os::OsFilesystem;

    use crate::fs::filesystem_provider::setup_test_execution;

    lore_base::runtime::LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let root = TempDir::new("lore-os-clone-answer-");
            std::fs::create_dir_all(root.join("tree/.lore")).expect("create the tree");
            std::fs::create_dir_all(root.join("tree/sub/nested/.lore")).expect("nest a repository");
            let provider = OsFilesystem::new(root.join("tree"));

            let answer = provider
                .clone_answer(
                    &root.join("tree"),
                    &root.join("tree/.lore"),
                    &root.join("clone"),
                )
                .await
                .expect("an answer");

            let CloneAnswer::SharesBlocks {
                backend: lore_io::clone::Backend::Apfs,
                entry_by_entry,
                ..
            } = &answer
            else {
                panic!("{answer:?}");
            };
            assert_eq!(
                **entry_by_entry,
                std::collections::HashSet::from([std::path::PathBuf::from("sub")]),
                "the plan finds the directory above the nested repository"
            );
            assert_eq!(
                std::fs::read_dir(root.join("tree/.lore"))
                    .expect("list the Lore directory")
                    .count(),
                0,
                "the probe leaves nothing in the Lore directory"
            );
        })
        .await;
}

#[test]
fn a_tree_outside_its_lore_directory_is_an_invalid_path() {
    let root = TempDir::new("lore-os-clone-outside-");

    let lore_directory = root.join("elsewhere/.lore");

    let outside = lore_revision::fs::filesystem_provider::lore_directory_within(
        &root.join("tree"),
        &lore_directory,
    );

    assert!(outside.is_err_and(|err| err.is_invalid_path()));
}

/// A tree holding a repository nested at `lib/vendor/nested`, an older `.urc` working tree at
/// `legacy`, whose files are the tree's own, and files beside them.
fn tree_with_a_nested_repository(root: &Path) -> std::path::PathBuf {
    let source = root.join("tree");
    std::fs::create_dir_all(source.join(".lore")).expect("create the Lore directory");
    std::fs::create_dir_all(source.join("lib/vendor/nested/.lore")).expect("nest a repository");
    std::fs::write(source.join("lib/vendor/nested/.lore/instance"), "nested")
        .expect("write the nested instance");
    std::fs::write(source.join("lib/vendor/nested/file"), "nested").expect("write a nested file");
    std::fs::create_dir_all(source.join("lib/other")).expect("create a sibling");
    std::fs::write(source.join("lib/other/file"), "kept").expect("write a sibling file");
    std::fs::write(source.join("lib/vendor/file"), "kept").expect("write a file beside it");
    std::fs::create_dir_all(source.join("legacy/.urc")).expect("nest an older repository");
    std::fs::write(source.join("legacy/.urc/instance"), "legacy").expect("write its instance");
    std::fs::write(source.join("legacy/file"), "kept").expect("write a file beside it");
    source
}

/// Asserts that the clone at `destination` holds the tree's own files, the older working tree's
/// included, and not the repository nested in it or the older working tree's Lore directory.
fn assert_the_nested_repository_is_left_out(destination: &Path) {
    for left_out in ["lib/vendor/nested", "legacy/.urc"] {
        assert!(
            std::fs::symlink_metadata(destination.join(left_out)).is_err(),
            "{left_out} is left out"
        );
    }
    for kept in ["lib/vendor/file", "lib/other/file", "legacy/file"] {
        assert_eq!(
            std::fs::read_to_string(destination.join(kept)).expect("read a kept file"),
            "kept",
            "{kept}"
        );
    }
}

/// [`clone_tree`] of the instance at `source`, whose Lore directory is `.lore`, with the
/// directories to clone entry by entry found first, as the clone's plan finds them.
fn planned_clone_tree(
    backend: lore_io::clone::Backend,
    source: &Path,
    destination: &Path,
    copy_unclonable: bool,
) -> std::io::Result<lore_io::clone::CloneStats> {
    let progress = lore_io::clone::Progress::default();
    let holds_nested =
        directories_above_nested_repositories(backend, source, Path::new(".lore"), &progress)?;
    clone_tree(
        backend,
        source,
        Path::new(".lore"),
        destination,
        &holds_nested,
        copy_unclonable,
        &progress,
    )
}

/// A repository nested in the tree is an instance of its own: a copy made entry by entry leaves
/// it out, so no two trees carry its instance ID.
#[test]
fn a_copy_leaves_out_a_repository_nested_in_the_tree() {
    let root = TempDir::new("lore-os-clone-nested-copy-");
    let source = tree_with_a_nested_repository(&root);
    let destination = root.join("clone");
    std::fs::create_dir(&destination).expect("create the destination");

    planned_clone_tree(lore_io::clone::Backend::Copy, &source, &destination, false)
        .expect("copy the tree");

    assert_the_nested_repository_is_left_out(&destination);
}

/// A backend that clones whole directories clones the directories above a nested repository
/// entry by entry, so it leaves the repository out too.
#[cfg(target_os = "macos")]
#[test]
fn a_whole_directory_clone_leaves_out_a_repository_nested_in_the_tree() {
    let root = TempDir::new("lore-os-clone-nested-whole-");
    let source = tree_with_a_nested_repository(&root);
    let destination = root.join("clone");
    std::fs::create_dir(&destination).expect("create the destination");

    let stats = planned_clone_tree(lore_io::clone::Backend::Apfs, &source, &destination, false)
        .expect("clone the tree");

    assert_the_nested_repository_is_left_out(&destination);
    assert!(
        stats.cloned_directory_count > 0,
        "the directories beside it are still cloned whole: {stats:?}"
    );
}

/// A Lore directory found anywhere in the tree but the instance's own is left out, as of a
/// repository created in a directory after the clone checked it.
#[test]
fn a_lore_directory_outside_the_instances_own_is_left_out() {
    let dot = Path::new(".lore");
    let tree = Path::new("/no/such/tree");

    for stray in ["src/.lore", "src/deep/.urc"] {
        assert_eq!(
            visit_tree(
                dot,
                &std::collections::HashSet::new(),
                &tree.join(stray),
                Path::new(stray),
                Kind::Directory
            ),
            Visit::Skip,
            "{stray}"
        );
    }
    assert_eq!(
        visit_tree(
            dot,
            &std::collections::HashSet::new(),
            &tree.join(".lore"),
            Path::new(".lore"),
            Kind::Directory
        ),
        Visit::EachEntry
    );
}

/// A disk image attached at `mount_point` for the life of the value, detached when dropped.
#[cfg(target_os = "macos")]
struct Attached(std::path::PathBuf);

#[cfg(target_os = "macos")]
impl Attached {
    fn new(image: &Path, mount_point: &Path) -> Self {
        let created = std::process::Command::new("hdiutil")
            .args([
                "create", "-size", "8m", "-fs", "APFS", "-volname", "mounted",
            ])
            .arg(image)
            .output()
            .expect("run hdiutil create");
        assert!(created.status.success(), "{created:?}");
        let attached = std::process::Command::new("hdiutil")
            .arg("attach")
            .arg(image)
            .arg("-mountpoint")
            .arg(mount_point)
            .arg("-nobrowse")
            .output()
            .expect("run hdiutil attach");
        assert!(attached.status.success(), "{attached:?}");
        Self(mount_point.to_path_buf())
    }
}

#[cfg(target_os = "macos")]
impl Drop for Attached {
    fn drop(&mut self) {
        let _ = std::process::Command::new("hdiutil")
            .arg("detach")
            .arg(&self.0)
            .arg("-force")
            .output();
    }
}

/// A volume mounted inside the tree is not cloned whole with the directories above it, which
/// would copy none of its files: its files are copied where the clone allows a copy, and fail
/// the clone otherwise.
#[cfg(target_os = "macos")]
#[test]
fn a_volume_mounted_in_the_tree_is_copied_or_fails_the_clone() {
    let root = TempDir::new("lore-os-clone-mounted-");
    let source = root.join("tree");
    std::fs::create_dir_all(source.join(".lore")).expect("create the Lore directory");
    std::fs::create_dir_all(source.join("assets/mounted")).expect("create the mount point");
    std::fs::create_dir_all(source.join("other")).expect("create a sibling");
    std::fs::write(source.join("other/file"), "kept").expect("write a sibling file");
    let _attached = Attached::new(&root.join("image.dmg"), &source.join("assets/mounted"));
    std::fs::write(source.join("assets/mounted/file"), "on the volume").expect("write on it");

    for copy_unclonable in [true, false] {
        let destination = root.join(format!("clone-{copy_unclonable}"));
        std::fs::create_dir(&destination).expect("create the destination");

        let cloned = planned_clone_tree(
            lore_io::clone::Backend::Apfs,
            &source,
            &destination,
            copy_unclonable,
        );

        if copy_unclonable {
            cloned.expect("the clone copies the volume's files");
            assert_eq!(
                std::fs::read_to_string(destination.join("assets/mounted/file"))
                    .expect("read the copied file"),
                "on the volume"
            );
            assert_eq!(
                std::fs::read_to_string(destination.join("other/file")).expect("read a sibling"),
                "kept"
            );
        } else {
            assert_eq!(
                cloned.map(drop).map_err(|err| err.kind()),
                Err(std::io::ErrorKind::Unsupported)
            );
        }
    }
}
