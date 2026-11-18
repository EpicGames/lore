// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What an SWFS operation reads back of the directories it created and the files it wrote from a
//! store, none of which reach the mount before the operation finalizes.
// Each test drives an operation over its mount resources and a revision, whose future is
// larger than the library's threshold; a test runs it once.
#![allow(clippy::large_futures)]
// The fixtures write and remove the files a mount holds directly, as a user would.
#![allow(clippy::disallowed_methods)]
use std::sync::Arc;

use bytes::Bytes;
use lore_base::test_util::TempDir;
use lore_revision::fs::filesystem_provider::FileInfo;
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::swfs::api_interface::SwfsInterface;
use lore_revision::fs::swfs::filesystem::SwfsOperation;
use lore_revision::fs::swfs::mount_resources::MountResources;
use lore_revision::fs::swfs::paths::MountPath;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::repository::RepositoryContext;

use super::listing_fixture::Held;
use super::listing_fixture::address_of;
use super::listing_fixture::in_context;
use super::listing_fixture::node;
use super::listing_fixture::revision_state;
use super::listing_fixture::test_repository;

const CONTENT: &[u8] = b"held content";

struct Fixture {
    operation: SwfsOperation,
    repository: Arc<RepositoryContext>,
    _mount: TempDir,
}

/// An operation over a mount serving a revision that holds `tracked.txt` and nothing else.
async fn operation() -> Fixture {
    operation_serving(&["tracked.txt"]).await
}

/// An operation over a mount serving a revision that holds `files`, each holding [`CONTENT`].
async fn operation_serving(files: &[&str]) -> Fixture {
    let mount = TempDir::new("swfs-operation-intents");
    let repository = test_repository().await;
    let held: Vec<(&str, Held<'_>)> = files
        .iter()
        .map(|path| (*path, Held::File(CONTENT)))
        .collect();
    let state = revision_state(&repository, &held).await;
    let mount_path = MountPath::new(&mount.path().join("mount")).expect("a mount path");
    std::fs::create_dir_all(mount.path().join("mount")).expect("making the mount directory");
    let resources = MountResources::for_test(repository.clone(), Some(state), mount_path).await;
    // Trusted, as an operation's record is once rebuilt, so the view answers from the revision.
    resources.modified().set_trusted(true);
    let interface = SwfsInterface::unmounted(resources);
    Fixture {
        operation: SwfsOperation::for_test(interface, &mount.path().join("mount")),
        repository,
        _mount: mount,
    }
}

fn file_node() -> Node {
    Node {
        flags: NodeFlags::File.bits(),
        address: address_of(CONTENT),
        size: CONTENT.len() as u64,
        ..Default::default()
    }
}

async fn names(operation: &SwfsOperation, path: &str) -> Vec<String> {
    let mut listing = operation
        .read_directory(&node(path))
        .await
        .expect("listing the directory");
    let mut names = Vec::new();
    while let Some(entry) = listing.next().await {
        names.push(entry.expect("a listing entry").name);
    }
    names.sort();
    names
}

#[tokio::test]
async fn a_created_directory_and_its_parents_are_directories() {
    in_context(async {
        let fixture = operation().await;
        fixture
            .operation
            .create_dir_all(&node("new/inner"))
            .await
            .expect("creating the directory");

        for path in ["new/inner", "new"] {
            assert_eq!(
                fixture.operation.file_info(&node(path)).await.ok(),
                Some(FileInfo::Directory),
                "{path}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn a_created_directory_holds_its_name_in_the_spelling_it_was_created_with() {
    in_context(async {
        let fixture = operation().await;
        fixture
            .operation
            .create_dir_all(&node("New/Inner"))
            .await
            .expect("creating the directory");

        assert_eq!(
            fixture
                .operation
                .holds_name_exactly(&node("New/Inner"))
                .await,
            Some(true)
        );
        assert_eq!(
            fixture.operation.holds_name_exactly(&node("New")).await,
            Some(true)
        );
    })
    .await;
}

#[tokio::test]
async fn a_created_directory_is_listed_in_its_parent_and_lists_its_children() {
    in_context(async {
        let fixture = operation().await;
        fixture
            .operation
            .create_dir_all(&node("new/inner"))
            .await
            .expect("creating the directory");

        assert_eq!(names(&fixture.operation, "").await, ["new", "tracked.txt"]);
        assert_eq!(names(&fixture.operation, "new").await, ["inner"]);
        assert!(names(&fixture.operation, "new/inner").await.is_empty());
    })
    .await;
}

#[tokio::test]
async fn the_directories_leading_to_a_written_node_are_directories_listing_it() {
    in_context(async {
        let fixture = operation().await;
        fixture
            .operation
            .write_node(
                fixture.repository.clone(),
                &file_node(),
                &node("dir/sub/file.txt"),
            )
            .await
            .expect("writing the node");

        assert_eq!(
            fixture.operation.file_info(&node("dir")).await.ok(),
            Some(FileInfo::Directory)
        );
        assert_eq!(
            fixture.operation.holds_name_exactly(&node("dir/sub")).await,
            Some(true)
        );
        assert_eq!(names(&fixture.operation, "dir").await, ["sub"]);
        assert_eq!(names(&fixture.operation, "dir/sub").await, ["file.txt"]);
    })
    .await;
}

#[tokio::test]
async fn a_removed_directory_is_not_reported() {
    in_context(async {
        let fixture = operation().await;
        let operation = &fixture.operation;
        operation
            .create_dir_all(&node("new"))
            .await
            .expect("creating the directory");
        operation
            .remove_recursive(&node("new"))
            .await
            .expect("removing the directory");

        assert_eq!(
            operation.file_info(&node("new")).await.ok(),
            Some(FileInfo::NotExist)
        );
        assert_eq!(names(operation, "").await, ["tracked.txt"]);
    })
    .await;
}

#[tokio::test]
async fn a_directory_leading_only_to_a_removed_node_is_not_reported() {
    in_context(async {
        let fixture = operation().await;
        let operation = &fixture.operation;
        operation
            .write_node(
                fixture.repository.clone(),
                &file_node(),
                &node("dir/file.txt"),
            )
            .await
            .expect("writing the node");
        operation
            .remove_recursive(&node("dir/file.txt"))
            .await
            .expect("removing the node");

        assert_eq!(
            operation.file_info(&node("dir")).await.ok(),
            Some(FileInfo::NotExist)
        );
        assert_eq!(names(operation, "").await, ["tracked.txt"]);
    })
    .await;
}

/// Here the operation's revision leaves nothing stored for the mount to serve once it finalizes,
/// as a sync to a revision lacking the file does, so the removal is described, not made.
#[tokio::test]
async fn removing_a_served_file_is_left_to_the_thaw_where_nothing_serves_it_after() {
    in_context(async {
        let fixture = operation().await;
        let operation = &fixture.operation;
        let on_mount = fixture._mount.path().join("mount").join("tracked.txt");
        std::fs::write(&on_mount, CONTENT).expect("holding the file on the mount");

        operation
            .remove(&node("tracked.txt"))
            .await
            .expect("removing the file");

        assert!(
            on_mount.exists(),
            "the removal went through the mount during the freeze"
        );
        assert_eq!(
            operation.file_info(&node("tracked.txt")).await.ok(),
            Some(FileInfo::NotExist)
        );

        operation.realize_for_test().await.expect("realizing");

        assert!(
            on_mount.exists(),
            "the removal went through the mount when finalizing"
        );
        assert_eq!(operation.described_for_test(), ["tracked.txt"]);
    })
    .await;
}

/// A directory the revision holds, emptied by removing what it held, goes as an intent too, as
/// `remove` takes it, and stays on the mount until the operation finalizes.
#[tokio::test]
async fn removing_an_emptied_tracked_directory_is_left_to_finalizing() {
    in_context(async {
        let fixture = operation_serving(&["dir/file.txt"]).await;
        let operation = &fixture.operation;
        let on_mount = fixture._mount.path().join("mount").join("dir");
        std::fs::create_dir_all(&on_mount).expect("holding the directory on the mount");
        operation
            .remove(&node("dir/file.txt"))
            .await
            .expect("removing the file");

        operation
            .remove_empty_directory(&node("dir"))
            .await
            .expect("removing the emptied directory");

        assert!(
            on_mount.exists(),
            "the removal went through the mount during the freeze"
        );
        assert_eq!(
            operation.file_info(&node("dir")).await.ok(),
            Some(FileInfo::NotExist)
        );
        assert!(names(operation, "").await.is_empty());
        assert_eq!(operation.described_for_test(), ["dir", "dir/file.txt"]);
    })
    .await;
}

/// With no current revision stored, the thaw describes nothing, so what the operation wrote from a
/// store goes to the mount.
#[tokio::test]
async fn a_file_written_with_no_current_revision_is_written_to_the_mount() {
    in_context(async {
        let fixture = operation().await;
        let operation = &fixture.operation;
        operation
            .write_node(
                fixture.repository.clone(),
                &file_node(),
                &node("tracked.txt"),
            )
            .await
            .expect("writing the node");

        operation.realize_for_test().await.expect("realizing");

        assert_eq!(
            std::fs::read(fixture._mount.path().join("mount").join("tracked.txt")).ok(),
            Some(CONTENT.to_vec())
        );
        assert!(operation.described_for_test().is_empty());
    })
    .await;
}

/// A directory the operation removes much of and writes much into lists what the operation means
/// it to hold, a removed directory it writes below again included, however many entries it holds.
#[tokio::test]
async fn a_large_directory_lists_what_the_operation_leaves_in_it() {
    const COUNT: usize = 3000;
    let tracked = |index: usize| format!("tracked{index:04}.txt");
    let new = |index: usize| format!("new{index:04}.txt");
    in_context(async {
        let mut files: Vec<String> = (0..COUNT)
            .map(|index| format!("big/{}", tracked(index)))
            .collect();
        files.push("big/kept/inner.txt".to_owned());
        files.push("big/written/inner.txt".to_owned());
        let files: Vec<&str> = files.iter().map(String::as_str).collect();
        let fixture = operation_serving(&files).await;
        let operation = &fixture.operation;
        for index in (0..COUNT).step_by(2) {
            operation
                .remove_recursive(&node(&format!("big/{}", tracked(index))))
                .await
                .expect("removing a tracked file");
        }
        for index in 0..COUNT {
            operation
                .write_node(
                    fixture.repository.clone(),
                    &file_node(),
                    &node(&format!("big/{}", new(index))),
                )
                .await
                .expect("writing a new file");
        }
        // Removed, then written below again: from a store, and to the mount.
        operation
            .remove_recursive(&node("big/kept"))
            .await
            .expect("removing a directory");
        operation
            .write_node(
                fixture.repository.clone(),
                &file_node(),
                &node("big/kept/again.txt"),
            )
            .await
            .expect("writing below the removed directory");
        operation
            .remove_recursive(&node("big/written"))
            .await
            .expect("removing a directory");
        std::fs::create_dir_all(fixture._mount.path().join("mount/big/written"))
            .expect("making the directory on the mount");
        operation
            .write_file(&node("big/written/again.txt"), Bytes::from_static(b"again"))
            .await
            .expect("writing below the removed directory on the mount");

        let mut expected: Vec<String> = (1..COUNT)
            .step_by(2)
            .map(tracked)
            .chain((0..COUNT).map(new))
            .chain(["kept".to_owned(), "written".to_owned()])
            .collect();
        expected.sort();
        assert_eq!(names(operation, "big").await, expected);
    })
    .await;
}

/// At the root, as below it: a removed directory the operation writes below again stays listed,
/// whether written from a store or to the mount.
#[tokio::test]
async fn a_removed_root_directory_written_below_again_is_listed() {
    in_context(async {
        let fixture =
            operation_serving(&["kept/inner.txt", "written/inner.txt", "gone/inner.txt"]).await;
        let operation = &fixture.operation;
        for removed in ["kept", "written", "gone"] {
            operation
                .remove_recursive(&node(removed))
                .await
                .expect("removing a directory");
        }
        operation
            .write_node(
                fixture.repository.clone(),
                &file_node(),
                &node("kept/again.txt"),
            )
            .await
            .expect("writing below the removed directory");
        std::fs::create_dir_all(fixture._mount.path().join("mount/written"))
            .expect("making the directory on the mount");
        operation
            .write_file(&node("written/again.txt"), Bytes::from_static(b"again"))
            .await
            .expect("writing below the removed directory on the mount");

        assert_eq!(names(operation, "").await, ["kept", "written"]);
    })
    .await;
}

/// A file taken from a store without its node below a removed directory is listed from what the
/// revision holds there: its own intent, not the removal above it, decides.
#[tokio::test]
async fn content_taken_from_a_store_below_a_removed_directory_is_listed() {
    in_context(async {
        let fixture = operation_serving(&["big/x.txt", "big/y.txt"]).await;
        let operation = &fixture.operation;
        operation
            .remove_recursive(&node("big"))
            .await
            .expect("removing the directory");
        operation
            .set_file_to_immutable_store_contents(
                fixture.repository.clone(),
                address_of(CONTENT),
                &node("big/x.txt"),
            )
            .await
            .expect("taking the content from the store");

        assert_eq!(names(operation, "big").await, ["x.txt"]);
    })
    .await;
}

/// A removed directory the operation writes into again, to the mount and from a store, lists both
/// files, as `file_info` reports them.
#[tokio::test]
async fn a_file_written_to_the_mount_in_a_removed_directory_is_listed() {
    in_context(async {
        let fixture = operation_serving(&["a/inner.txt"]).await;
        let operation = &fixture.operation;
        operation
            .remove_recursive(&node("a"))
            .await
            .expect("removing the directory");
        std::fs::create_dir_all(fixture._mount.path().join("mount/a"))
            .expect("making the directory on the mount");
        operation
            .write_file(&node("a/f.txt"), Bytes::from_static(b"f"))
            .await
            .expect("writing to the mount");
        operation
            .write_node(fixture.repository.clone(), &file_node(), &node("a/h.txt"))
            .await
            .expect("writing from the store");

        assert!(
            operation
                .file_info(&node("a/f.txt"))
                .await
                .expect("describing the file")
                .is_file()
        );
        assert_eq!(names(operation, "a").await, ["f.txt", "h.txt"]);
    })
    .await;
}

/// A file written to the mount in a removed directory stands there, so no directory is made over
/// it, as on disk.
#[tokio::test]
async fn no_directory_is_made_over_a_file_written_to_the_mount_in_a_removed_directory() {
    in_context(async {
        let fixture = operation_serving(&["a/inner.txt"]).await;
        let operation = &fixture.operation;
        operation
            .remove_recursive(&node("a"))
            .await
            .expect("removing the directory");
        std::fs::create_dir_all(fixture._mount.path().join("mount/a"))
            .expect("making the directory on the mount");
        operation
            .write_file(&node("a/f.txt"), Bytes::from_static(b"f"))
            .await
            .expect("writing to the mount");

        assert!(
            operation
                .create_dir_all(&node("a/f.txt/inner"))
                .await
                .is_err()
        );
    })
    .await;
}

/// A listing leaves out what the operation means gone in that directory alone, as its removals
/// come and go: removals elsewhere leave a directory with none whole, and a removal written over
/// again, from a store or to the mount, no longer hides the entry.
#[tokio::test]
async fn a_listing_follows_the_removals_in_its_directory() {
    const COUNT: usize = 3;
    in_context(async {
        let mut files: Vec<String> = (0..COUNT)
            .map(|index| format!("many/f{index:04}.txt"))
            .collect();
        files.extend(["other/a.txt", "other/b.txt", "other/c.txt"].map(str::to_owned));
        let files: Vec<&str> = files.iter().map(String::as_str).collect();
        let fixture = operation_serving(&files).await;
        let operation = &fixture.operation;
        for index in 0..COUNT {
            operation
                .remove_recursive(&node(&format!("many/f{index:04}.txt")))
                .await
                .expect("removing a file");
        }
        assert!(names(operation, "many").await.is_empty());
        assert_eq!(names(operation, "other").await, ["a.txt", "b.txt", "c.txt"]);

        for removed in ["other/a.txt", "other/b.txt", "other/c.txt"] {
            operation
                .remove_recursive(&node(removed))
                .await
                .expect("removing a file");
        }
        operation
            .write_node(
                fixture.repository.clone(),
                &file_node(),
                &node("other/a.txt"),
            )
            .await
            .expect("writing over the removal from the store");
        std::fs::create_dir_all(fixture._mount.path().join("mount/other"))
            .expect("making the directory on the mount");
        operation
            .write_file(&node("other/b.txt"), Bytes::from_static(b"b"))
            .await
            .expect("writing over the removal to the mount");
        assert_eq!(names(operation, "other").await, ["a.txt", "b.txt"]);

        operation
            .remove_recursive(&node("other/a.txt"))
            .await
            .expect("removing the file again");
        assert_eq!(names(operation, "other").await, ["b.txt"]);
    })
    .await;
}

/// A file the operation wrote to the mount and then removed is gone, as on disk: a directory can
/// be made in its place, and its directory no longer lists it.
#[tokio::test]
async fn a_file_written_to_the_mount_then_removed_is_gone() {
    in_context(async {
        let fixture = operation().await;
        let operation = &fixture.operation;
        operation
            .write_file(&node("x.txt"), Bytes::from_static(b"x"))
            .await
            .expect("writing to the mount");
        operation
            .remove_recursive(&node("x.txt"))
            .await
            .expect("removing the file");

        assert_eq!(
            operation.file_info(&node("x.txt")).await.ok(),
            Some(FileInfo::NotExist)
        );
        assert_eq!(names(operation, "").await, ["tracked.txt"]);
        operation
            .create_dir_all(&node("x.txt"))
            .await
            .expect("making a directory where the file was");
    })
    .await;
}

/// A file the operation wrote to the mount goes with a directory above it that it removes later.
#[tokio::test]
async fn a_file_written_to_the_mount_goes_with_a_directory_removed_after() {
    in_context(async {
        let fixture = operation().await;
        let operation = &fixture.operation;
        std::fs::create_dir_all(fixture._mount.path().join("mount/a"))
            .expect("making the directory on the mount");
        operation
            .write_file(&node("a/f.txt"), Bytes::from_static(b"f"))
            .await
            .expect("writing to the mount");
        operation
            .remove_recursive(&node("a"))
            .await
            .expect("removing the directory");

        assert_eq!(
            operation.file_info(&node("a/f.txt")).await.ok(),
            Some(FileInfo::NotExist)
        );
        operation
            .create_dir_all(&node("a/f.txt/inner"))
            .await
            .expect("making a directory where the file was");
    })
    .await;
}

/// A removed directory the operation writes a file into again through the mount lists that file
/// alone, not what the mount still holds of the directory until the operation finalizes.
#[tokio::test]
async fn a_removed_directory_written_into_through_the_mount_lists_only_what_was_written() {
    in_context(async {
        let fixture = operation_serving(&["a/inner.txt"]).await;
        let operation = &fixture.operation;
        let on_mount = fixture._mount.path().join("mount/a");
        std::fs::create_dir_all(&on_mount).expect("making the directory on the mount");
        std::fs::write(on_mount.join("inner.txt"), CONTENT).expect("holding the file on the mount");
        operation
            .remove_recursive(&node("a"))
            .await
            .expect("removing the directory");
        operation
            .write_file(&node("a/f.txt"), Bytes::from_static(b"f"))
            .await
            .expect("writing to the mount");

        assert_eq!(names(operation, "a").await, ["f.txt"]);
    })
    .await;
}

/// A file the operation wrote to the mount and then removed with what lies below a path leaves
/// the mount too, where no revision holds it for the thaw to drop.
#[tokio::test]
async fn a_file_written_to_the_mount_then_removed_leaves_the_mount() {
    in_context(async {
        let fixture = operation().await;
        let operation = &fixture.operation;
        let on_mount = fixture._mount.path().join("mount").join("x.txt");
        operation
            .write_file(&node("x.txt"), Bytes::from_static(b"x"))
            .await
            .expect("writing to the mount");
        operation
            .remove_recursive(&node("x.txt"))
            .await
            .expect("removing the file");

        operation.realize_for_test().await.expect("realizing");

        assert!(!on_mount.exists(), "the removed file stayed on the mount");
    })
    .await;
}

/// A file the operation wrote from a store goes with a directory above it that it removes later,
/// as one written to the mount does.
#[tokio::test]
async fn a_file_written_from_a_store_goes_with_a_directory_removed_after() {
    in_context(async {
        let fixture = operation().await;
        let operation = &fixture.operation;
        operation
            .write_node(fixture.repository.clone(), &file_node(), &node("a/f.txt"))
            .await
            .expect("writing from the store");
        operation
            .remove_recursive(&node("a"))
            .await
            .expect("removing the directory");

        assert_eq!(
            operation.file_info(&node("a/f.txt")).await.ok(),
            Some(FileInfo::NotExist)
        );
        assert_eq!(names(operation, "").await, ["tracked.txt"]);

        operation.realize_for_test().await.expect("realizing");

        assert!(
            !fixture._mount.path().join("mount/a/f.txt").exists(),
            "the removed file was written to the mount"
        );
    })
    .await;
}

/// A directory the operation moved on the mount, with what it holds, leaves the mount when the
/// operation removes it.
#[tokio::test]
async fn a_directory_moved_on_the_mount_then_removed_leaves_the_mount() {
    in_context(async {
        let fixture = operation().await;
        let operation = &fixture.operation;
        let mount = fixture._mount.path().join("mount");
        std::fs::create_dir_all(mount.join("d")).expect("making the directory on the mount");
        std::fs::write(mount.join("d/x.txt"), b"x").expect("holding a file in it");
        operation
            .rename(&node("d"), &node("e"))
            .await
            .expect("moving the directory");

        operation
            .remove_recursive(&node("e"))
            .await
            .expect("removing the moved directory");

        assert!(
            !mount.join("e").exists(),
            "the moved directory stayed on the mount"
        );
    })
    .await;
}

/// A file the operation wrote to the mount that could not be removed there is still the
/// operation's to remove: removing it again, once it can be, takes it off the mount.
#[cfg(windows)]
#[tokio::test]
async fn a_file_written_to_the_mount_that_could_not_be_removed_is_removed_again() {
    use std::os::windows::fs::OpenOptionsExt;
    in_context(async {
        let fixture = operation().await;
        let operation = &fixture.operation;
        let on_mount = fixture._mount.path().join("mount").join("x.txt");
        operation
            .write_file(&node("x.txt"), Bytes::from_static(b"x"))
            .await
            .expect("writing to the mount");
        // Held open without sharing deletion, so removing it fails.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&on_mount)
            .expect("holding the file open");
        assert!(operation.remove_recursive(&node("x.txt")).await.is_err());
        drop(held);

        operation
            .remove_recursive(&node("x.txt"))
            .await
            .expect("removing the file again");

        assert!(!on_mount.exists(), "the file stayed on the mount");
    })
    .await;
}

/// What the operation made at any depth below a directory it then removes is forgotten, and what
/// it makes there afterwards is listed alone.
#[tokio::test]
async fn what_was_made_at_any_depth_below_a_removed_directory_is_forgotten() {
    in_context(async {
        let fixture = operation().await;
        let operation = &fixture.operation;
        operation
            .create_dir_all(&node("a/b"))
            .await
            .expect("making the directories");
        operation
            .write_node(fixture.repository.clone(), &file_node(), &node("a/b/f.txt"))
            .await
            .expect("writing below them");
        operation
            .remove_recursive(&node("a"))
            .await
            .expect("removing the directory");
        operation
            .write_node(fixture.repository.clone(), &file_node(), &node("a/c.txt"))
            .await
            .expect("writing into it again");

        assert_eq!(names(operation, "a").await, ["c.txt"]);
        assert_eq!(names(operation, "").await, ["a", "tracked.txt"]);
        assert_eq!(
            operation.file_info(&node("a/b")).await.ok(),
            Some(FileInfo::NotExist)
        );
    })
    .await;
}

/// A file the operation wrote to the mount and then removed is not counted as written below the
/// directories above it, so a directory removed after it reads as gone.
#[tokio::test]
async fn a_removed_file_written_to_the_mount_leaves_its_removed_directory_gone() {
    in_context(async {
        let fixture = operation_serving(&["a/inner.txt"]).await;
        let operation = &fixture.operation;
        std::fs::create_dir_all(fixture._mount.path().join("mount/a"))
            .expect("making the directory on the mount");
        operation
            .write_file(&node("a/f.txt"), Bytes::from_static(b"f"))
            .await
            .expect("writing to the mount");
        operation
            .remove_recursive(&node("a/f.txt"))
            .await
            .expect("removing the file");
        operation
            .remove_recursive(&node("a"))
            .await
            .expect("removing the directory");

        assert_eq!(
            operation.file_info(&node("a")).await.ok(),
            Some(FileInfo::NotExist)
        );
        assert!(names(operation, "").await.is_empty());
    })
    .await;
}
