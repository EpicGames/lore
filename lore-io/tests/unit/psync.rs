// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cell::RefCell;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use lore_base::test_util::TempDir;
use lore_io::priority::test_util::background_disk_priority;
use lore_io::priority::test_util::restorable_disk_priority;
use lore_io::priority::test_util::thread_disk_priority;
use lore_io::psync::*;

/// The shape of a Windows container bind-mount root: `mkdir` fails, but the target already
/// stats as a directory, so the failure is forgiven.
#[test]
fn forgive_existing_dir_forgives_a_failure_on_an_existing_directory() {
    let dir = lore_base::test_util::TempDir::new("lore-io-psync-forgive-");
    let synthetic_error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);

    forgive_existing_dir(Err(synthetic_error), dir.path())
        .expect("an existing directory must not fail the caller");
}

#[test]
fn forgive_existing_dir_propagates_a_failure_on_a_file() {
    let dir = lore_base::test_util::TempDir::new("lore-io-psync-forgive-");
    let file_path = dir.path().join("blocker");
    std::fs::File::create(&file_path).expect("create blocker file");
    let synthetic_error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);

    let error = forgive_existing_dir(Err(synthetic_error), &file_path)
        .expect_err("a file where a directory belongs must still fail");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn forgive_existing_dir_propagates_a_failure_on_a_missing_path() {
    let dir = lore_base::test_util::TempDir::new("lore-io-psync-forgive-");
    let missing_path = dir.path().join("nowhere");
    let synthetic_error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);

    let error = forgive_existing_dir(Err(synthetic_error), &missing_path)
        .expect_err("a genuinely missing path must still fail");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn forgive_existing_dir_passes_success_through() {
    let dir = lore_base::test_util::TempDir::new("lore-io-psync-forgive-");

    forgive_existing_dir(Ok(()), dir.path()).expect("success must not become a failure");
}

/// A tree at `path` in a new directory, with a file in a nested directory and one at its top, and
/// the name beside it to rename it to.
fn tree() -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new("lore-io-psync-remove-in-background-");
    let path = dir.child("tree");
    std::fs::create_dir_all(path.join("a/b")).expect("create the tree");
    std::fs::write(path.join("a/b/file.txt"), b"content").expect("write a nested file");
    std::fs::write(path.join("top.txt"), b"content").expect("write a file at the top");
    let aside = dir.child("tree.aside");
    (dir, path, aside)
}

fn rename(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

fn remove_tree(tree: &Path) -> io::Result<()> {
    std::fs::remove_dir_all(tree)
}

/// A rename refused with the code a mount point gives.
fn refused_as_a_mount_point(_from: &Path, _to: &Path) -> io::Result<()> {
    #[cfg(target_family = "unix")]
    let code = libc::EXDEV;
    #[cfg(target_family = "windows")]
    let code = windows_sys::Win32::Foundation::ERROR_NOT_SAME_DEVICE as i32;
    Err(io::Error::from_raw_os_error(code))
}

/// The codes a rename of a mount point fails with, and no others, tell a mount point.
#[test]
fn a_mount_point_is_told_by_the_codes_its_rename_fails_with() {
    #[cfg(target_family = "unix")]
    let (mount_point, other) = (
        [libc::EXDEV, libc::EBUSY],
        [libc::EACCES, libc::ENOENT, libc::ENOTEMPTY, libc::EPERM],
    );
    #[cfg(target_family = "windows")]
    let (mount_point, other) = {
        use windows_sys::Win32::Foundation::*;
        (
            [ERROR_NOT_SAME_DEVICE as i32],
            [
                ERROR_ACCESS_DENIED as i32,
                ERROR_FILE_NOT_FOUND as i32,
                ERROR_SHARING_VIOLATION as i32,
                ERROR_ALREADY_EXISTS as i32,
            ],
        )
    };
    for code in mount_point {
        assert!(
            is_mount_point_refusal(&io::Error::from_raw_os_error(code)),
            "{code}"
        );
    }
    for code in other {
        assert!(
            !is_mount_point_refusal(&io::Error::from_raw_os_error(code)),
            "{code}"
        );
    }
    assert!(!is_mount_point_refusal(&io::Error::from(
        io::ErrorKind::ResourceBusy
    )));
}

/// The tree is renamed aside and removed from there.
#[test]
fn a_tree_is_renamed_aside_and_removed() {
    let (_dir, path, aside) = tree();

    let (removed_from, removed) =
        remove_tree_in_background(path.clone(), Some(aside.clone()), rename, remove_tree);

    removed.expect("removes the tree");
    assert_eq!(removed_from, aside);
    assert!(!path.exists() && !aside.exists());
}

/// A mount point cannot be renamed, so its files are removed in place and nothing is renamed.
#[test]
fn a_mount_point_is_removed_in_place() {
    let (_dir, path, aside) = tree();

    let (removed_from, removed) = remove_tree_in_background(
        path.clone(),
        Some(aside.clone()),
        refused_as_a_mount_point,
        remove_tree,
    );

    removed.expect("removes the tree in place");
    assert_eq!(removed_from, path);
    assert!(!path.exists(), "the files are removed");
    assert!(!aside.exists(), "nothing was renamed aside");
}

/// A rename refused for any other reason removes nothing, and says the files are at the path.
#[test]
fn a_refused_rename_removes_nothing() {
    let (_dir, path, aside) = tree();

    let (removed_from, removed) = remove_tree_in_background(
        path.clone(),
        Some(aside.clone()),
        |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::PermissionDenied)),
        |_: &Path| panic!("nothing is removed"),
    );

    assert_eq!(
        removed.expect_err("the rename fails").kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(removed_from, path);
    assert!(path.join("a/b/file.txt").exists());
}

/// A run that renamed the tree and then failed to remove it leaves the files aside, and a run
/// without a rename removes them from there and leaves a new directory at the freed path alone.
#[test]
fn a_run_after_a_failed_removal_removes_the_tree_from_aside() {
    let (_dir, path, aside) = tree();
    let (removed_from, removed) =
        remove_tree_in_background(path.clone(), Some(aside.clone()), rename, |_: &Path| {
            Err(io::Error::from(io::ErrorKind::ResourceBusy))
        });
    removed.expect_err("the removal fails");
    assert_eq!(removed_from, aside);
    assert!(!path.exists() && aside.join("a/b/file.txt").exists());
    std::fs::create_dir(&path).expect("make a new directory at the freed path");
    std::fs::write(path.join("new.txt"), b"new").expect("write a file in the new directory");

    let (removed_from, removed) = remove_tree_in_background(
        aside.clone(),
        None,
        |_: &Path, _: &Path| panic!("nothing is renamed"),
        remove_tree,
    );

    removed.expect("removes what the first run left");
    assert_eq!(removed_from, aside);
    assert!(!aside.exists());
    assert!(path.join("new.txt").exists(), "the new directory is kept");
}

/// A run that finds nothing at `path` has nothing to remove, and leaves whatever is at `aside`.
#[test]
fn a_missing_path_leaves_the_aside_name_alone() {
    let (_dir, path, aside) = tree();
    std::fs::rename(&path, &aside).expect("put the tree at the aside name");

    let (removed_from, removed) =
        remove_tree_in_background(path.clone(), Some(aside.clone()), rename, |_: &Path| {
            panic!("nothing is removed")
        });

    removed.expect("a missing path counts as removed");
    assert_eq!(removed_from, path);
    assert!(aside.join("a/b/file.txt").exists());
}

/// The rename and every removal run at background disk priority, renamed aside or in place, and
/// the thread has its own priority back once the job ends.
#[test]
fn the_job_runs_at_background_disk_priority() {
    for in_place in [false, true] {
        let (_dir, path, aside) = tree();
        let before = thread_disk_priority();
        let seen = RefCell::new(Vec::new());

        let (_, removed) = remove_tree_in_background(
            path,
            Some(aside),
            |from: &Path, to: &Path| {
                seen.borrow_mut().push(thread_disk_priority());
                if in_place {
                    refused_as_a_mount_point(from, to)
                } else {
                    rename(from, to)
                }
            },
            |tree: &Path| {
                seen.borrow_mut().push(thread_disk_priority());
                remove_tree(tree)
            },
        );

        removed.expect("removes the tree");
        let seen = seen.into_inner();
        assert_eq!(seen.len(), 2, "renamed once and removed one tree");
        assert!(
            seen.iter()
                .all(|priority| *priority == background_disk_priority()),
            "{seen:?}"
        );
        assert_eq!(thread_disk_priority(), before.map(restorable_disk_priority));
    }
}

/// Creates a junction at `link` to the directory `target`, which Windows does without a privilege.
#[cfg(target_family = "windows")]
fn junction(link: &Path, target: &Path) {
    let created = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .expect("run mklink");
    assert!(created.status.success(), "mklink /J failed: {created:?}");
}

/// A junction carries the mount-point tag, as a volume mounted in a folder does, so it is a
/// mount folder; a plain directory is not, and a missing path is not found.
#[cfg(target_family = "windows")]
#[test]
fn a_junction_is_a_mount_folder_and_a_directory_is_not() {
    let (dir, path, _aside) = tree();
    let link = dir.child("link");
    junction(&link, &path);

    assert!(is_mount_folder(&link).expect("read the junction"));
    assert!(!is_mount_folder(&path).expect("read the directory"));
    assert_eq!(
        is_mount_folder(&dir.child("missing"))
            .expect_err("nothing is there")
            .kind(),
        io::ErrorKind::NotFound
    );
}

/// A mount folder is not renamed, which would move the junction without the files it leads to:
/// the files are removed through it, and the junction and the directory it leads to are kept.
#[cfg(target_family = "windows")]
#[test]
fn a_mount_folder_is_emptied_in_place_and_kept() {
    let (dir, target, _) = tree();
    let path = dir.child("link");
    let aside = dir.child("link.aside");
    junction(&path, &target);

    let (removed_from, removed) = remove_tree_in_background(
        path.clone(),
        Some(aside.clone()),
        |_: &Path, _: &Path| panic!("a mount folder is not renamed"),
        remove_tree,
    );

    removed.expect("removes the files through the junction");
    assert_eq!(removed_from, path);
    assert!(!aside.exists(), "nothing was renamed aside");
    assert!(
        is_mount_folder(&path).expect("the junction is kept"),
        "the junction is kept"
    );
    let left: Vec<_> = std::fs::read_dir(&target)
        .expect("the directory the junction leads to is kept")
        .collect();
    assert!(left.is_empty(), "the files are removed: {left:?}");
}

/// An entry counts as one the system keeps at a volume root only when it is both hidden and
/// system.
#[cfg(target_family = "windows")]
#[test]
fn only_a_hidden_system_entry_is_kept_at_a_volume_root() {
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SYSTEM;

    assert!(is_hidden_system(
        FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM
    ));
    assert!(is_hidden_system(
        FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_DIRECTORY
    ));
    assert!(!is_hidden_system(FILE_ATTRIBUTE_HIDDEN));
    assert!(!is_hidden_system(
        FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_DIRECTORY
    ));
    assert!(!is_hidden_system(0));
}

/// The root of a drive is a volume root; a junction to an ordinary directory, and the directory,
/// are not.
#[cfg(target_family = "windows")]
#[test]
fn a_junction_to_a_directory_is_not_a_volume_root() {
    let (dir, path, _aside) = tree();
    let link = dir.child("link");
    junction(&link, &path);

    assert!(!is_volume_root(&link));
    assert!(!is_volume_root(&path));
    let drive_root = path.ancestors().last().expect("the path has a root");
    assert!(
        is_volume_root(drive_root),
        "{drive_root:?} is a volume root"
    );
}

/// A junction to an ordinary directory has its hidden and system entries removed with the rest.
#[cfg(target_family = "windows")]
#[test]
fn a_junction_to_a_directory_removes_its_hidden_system_entries() {
    let (dir, target, _) = tree();
    let path = dir.child("link");
    let aside = dir.child("link.aside");
    junction(&path, &target);
    let marked = std::process::Command::new("attrib")
        .args(["+H", "+S"])
        .arg(target.join("top.txt"))
        .output()
        .expect("run attrib");
    assert!(marked.status.success(), "attrib failed: {marked:?}");

    let (_, removed) = remove_tree_in_background(
        path.clone(),
        Some(aside),
        |_: &Path, _: &Path| panic!("a mount folder is not renamed"),
        remove_tree,
    );

    removed.expect("removes the files through the junction");
    let left: Vec<_> = std::fs::read_dir(&target)
        .expect("the directory the junction leads to is kept")
        .collect();
    assert!(left.is_empty(), "the files are removed: {left:?}");
}
