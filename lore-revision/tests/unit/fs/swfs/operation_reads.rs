// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What an SWFS operation answers for paths the revision does not track, and what it copies,
//! from what SWFS froze and the store rather than by reading the mount.
// Each test drives an operation over its mount resources and a revision, whose future is
// larger than the library's threshold; a test runs it once.
#![allow(clippy::large_futures)]
// The fixtures write and remove the files a mount holds directly, as a user would.
#![allow(clippy::disallowed_methods)]
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_base::test_util::TempDir;
use lore_revision::fs::filesystem_provider::FileInfo;
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::swfs::api_interface::SwfsInterface;
use lore_revision::fs::swfs::filesystem::SwfsOperation;
use lore_revision::fs::swfs::frozen_files::FrozenSnapshot;
use lore_revision::fs::swfs::mount_resources::MountResources;
use lore_revision::fs::swfs::paths::MountPath;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::repository::RepositoryContext;
use lore_storage::FRAGMENT_SIZE_THRESHOLD;

use super::fake_frozen::FakeFrozen;
use super::listing_fixture::Held;
use super::listing_fixture::in_context;
use super::listing_fixture::node;
use super::listing_fixture::revision_state;
use super::listing_fixture::test_repository;

const TRACKED: &[u8] = b"tracked as stored";

struct Fixture {
    operation: SwfsOperation,
    repository: Arc<RepositoryContext>,
    mount: TempDir,
}

impl Fixture {
    fn on_mount(&self, path: &str) -> std::path::PathBuf {
        self.mount.path().join("mount").join(path)
    }
}

/// An operation over a mount serving a revision that holds `tracked.txt`, begun with SWFS
/// holding `notes.txt~mine` and `made/inner.txt` of its own. The mount itself holds other bytes
/// at `tracked.txt` and a `stray.txt` SWFS did not freeze, which a read of the mount would find.
async fn operation() -> Fixture {
    let frozen = FakeFrozen::default()
        .with_file("\\notes.txt~mine", b"mine")
        .with_file("\\made\\inner.txt", b"inner");
    operation_over(Arc::new(frozen)).await
}

/// [`operation`], begun with SWFS holding what `frozen` holds.
async fn operation_over(frozen: Arc<FakeFrozen>) -> Fixture {
    let mount = TempDir::new("swfs-operation-reads");
    let repository = test_repository().await;
    let state = revision_state(&repository, &[("tracked.txt", Held::File(TRACKED))]).await;
    let mount_root = mount.path().join("mount");
    std::fs::create_dir_all(&mount_root).expect("making the mount directory");
    std::fs::write(mount_root.join("tracked.txt"), b"what the mount holds").expect("writing");
    std::fs::write(mount_root.join("stray.txt"), b"stray").expect("writing");
    let mount_path = MountPath::new(&mount_root).expect("a mount path");
    let resources = MountResources::for_test(repository.clone(), Some(state), mount_path).await;
    resources.modified().set_trusted(true);
    let snapshot = FrozenSnapshot::capture(frozen).expect("listing the frozen files");
    Fixture {
        operation: SwfsOperation::for_test_frozen(
            SwfsInterface::unmounted(resources),
            &mount_root,
            snapshot,
        ),
        repository,
        mount,
    }
}

#[tokio::test]
async fn an_untracked_file_swfs_froze_is_described_as_frozen() {
    in_context(async {
        let fixture = operation().await;

        let info = fixture
            .operation
            .untracked_file_info(&node("notes.txt~mine"))
            .await
            .expect("describing the sidecar");

        assert!(info.is_file());
        assert_eq!(info.size(), 4);
    })
    .await;
}

#[tokio::test]
async fn a_directory_swfs_froze_something_in_is_a_directory() {
    in_context(async {
        let fixture = operation().await;

        let info = fixture
            .operation
            .untracked_file_info(&node("made"))
            .await
            .expect("describing the directory");

        assert_eq!(info, FileInfo::Directory);
    })
    .await;
}

#[tokio::test]
async fn an_untracked_path_swfs_holds_nothing_of_is_absent_without_asking_the_mount() {
    in_context(async {
        let fixture = operation().await;

        let info = fixture
            .operation
            .untracked_file_info(&node("stray.txt"))
            .await
            .expect("describing the path");

        // SWFS freezes every file it holds changed, so one it did not freeze is not there.
        assert_eq!(info, FileInfo::NotExist);
    })
    .await;
}

#[tokio::test]
async fn copying_a_file_nothing_changed_copies_what_the_store_holds() {
    in_context(async {
        let fixture = operation().await;

        fixture
            .operation
            .copy_file(&node("tracked.txt"), &node("tracked.txt~mine"))
            .await
            .expect("copying the file");

        let copied = std::fs::read(fixture.on_mount("tracked.txt~mine")).expect("reading");
        assert_eq!(copied, TRACKED);
    })
    .await;
}

#[tokio::test]
async fn copying_a_file_swfs_froze_copies_what_it_froze() {
    in_context(async {
        let fixture = operation().await;

        fixture
            .operation
            .copy_file(&node("notes.txt~mine"), &node("notes.txt~base"))
            .await
            .expect("copying the file");

        let copied = std::fs::read(fixture.on_mount("notes.txt~base")).expect("reading");
        assert_eq!(copied, b"mine");
    })
    .await;
}

/// A file of any size is copied a window at a time: no read asks SWFS for more than a window, and
/// the copy is the file whole, past the largest file the IO driver writes in one call.
#[tokio::test]
async fn copying_a_large_file_swfs_froze_holds_one_window_at_a_time() {
    in_context(async {
        let window = 2 * FRAGMENT_SIZE_THRESHOLD;
        let large: Vec<u8> = (0..17 * window + 7)
            .map(|index| (index % 251) as u8)
            .collect();
        let frozen = Arc::new(FakeFrozen::default().with_file("\\big.bin~mine", &large));
        let fixture = operation_over(frozen.clone()).await;

        fixture
            .operation
            .copy_file(&node("big.bin~mine"), &node("big.bin~base"))
            .await
            .expect("copying the file");

        let copied = std::fs::read(fixture.on_mount("big.bin~base")).expect("reading");
        assert!(copied == large, "the copy differs from the file");
        let largest = frozen.largest_read.load(Ordering::Relaxed);
        assert!(
            largest <= window,
            "a read asked SWFS for {largest} bytes, past the window of {window}"
        );
    })
    .await;
}

/// As on disk, a directory is not made where a file stands that the user holds, at the path or
/// above it: a thaw describing it would replace the user's file. A directory SWFS froze only
/// something in stays one to make directories below.
#[tokio::test]
async fn a_directory_is_not_made_over_a_file_swfs_froze() {
    in_context(async {
        let fixture = operation().await;
        let file = Node {
            flags: NodeFlags::File.bits(),
            ..Default::default()
        };

        for path in ["notes.txt~mine", "notes.txt~mine/below"] {
            assert!(
                fixture.operation.create_dir_all(&node(path)).await.is_err(),
                "{path} lies at or below the user's file"
            );
        }
        // A file below it would make the directory too, as a thaw describing it makes it.
        let below = node("notes.txt~mine/inner.txt");
        assert!(
            fixture
                .operation
                .write_node(fixture.repository.clone(), &file, &below)
                .await
                .is_err(),
            "writing a node below the user's file"
        );
        assert!(
            fixture
                .operation
                .set_file_to_immutable_store_contents(
                    fixture.repository.clone(),
                    file.address,
                    &below
                )
                .await
                .is_err(),
            "writing stored contents below the user's file"
        );
        assert_eq!(
            fixture.operation.described_for_test(),
            Vec::<String>::new(),
            "nothing refused is described to the thaw"
        );
        fixture
            .operation
            .create_dir_all(&node("made/sub"))
            .await
            .expect("making a directory below one SWFS froze something in");
    })
    .await;
}

/// What the operation wrote to the mount and renamed there is read at its new path, from the
/// mount, and nothing is left at its old one.
#[tokio::test]
async fn a_file_the_operation_wrote_is_read_where_it_renamed_it() {
    in_context(Box::pin(async {
        let fixture = operation().await;
        let (from, to) = (node("merged.txt"), node("renamed.txt"));
        fixture
            .operation
            .write_file(&from, Bytes::from_static(b"merged"))
            .await
            .expect("writing the file");

        fixture
            .operation
            .rename(&from, &to)
            .await
            .expect("renaming the file");

        let info = fixture
            .operation
            .file_info(&to)
            .await
            .expect("describing the renamed file");
        assert!(info.is_file());
        assert_eq!(info.size(), 6);
        let content = fixture
            .operation
            .content_source(&to)
            .await
            .expect("finding the renamed file's content")
            .read_all()
            .await
            .expect("reading the renamed file");
        assert_eq!(&content[..], b"merged");
        assert_eq!(
            fixture
                .operation
                .file_info(&from)
                .await
                .expect("describing the old path"),
            FileInfo::NotExist
        );
    }))
    .await;
}

/// A file the operation wrote to the mount and removed there stands in the way of nothing: a
/// directory is made where it stood, as on disk.
#[tokio::test]
async fn a_directory_is_made_where_the_operation_removed_a_file_it_wrote() {
    in_context(Box::pin(async {
        let fixture = operation().await;
        let path = node("merged.txt");
        fixture
            .operation
            .write_file(&path, Bytes::from_static(b"merged"))
            .await
            .expect("writing the file");
        fixture
            .operation
            .remove(&path)
            .await
            .expect("removing the file");

        fixture
            .operation
            .create_dir_all(&path)
            .await
            .expect("making a directory where the removed file stood");

        assert_eq!(
            fixture
                .operation
                .file_info(&path)
                .await
                .expect("describing the directory"),
            FileInfo::Directory
        );
    }))
    .await;
}

/// A directory the operation wrote two files to is still read from the mount once it renamed one
/// of them out of it, which holds the other.
#[tokio::test]
async fn a_directory_still_holding_a_file_the_operation_wrote_is_listed_from_the_mount() {
    in_context(Box::pin(async {
        let fixture = operation().await;
        std::fs::create_dir_all(fixture.on_mount("out")).expect("making a directory");
        for name in ["out/a.txt", "out/b.txt"] {
            fixture
                .operation
                .write_file(&node(name), Bytes::from_static(b"merged"))
                .await
                .expect("writing a file");
        }
        fixture
            .operation
            .rename(&node("out/a.txt"), &node("c.txt"))
            .await
            .expect("renaming a file");

        let mut listing = fixture
            .operation
            .read_directory(&node("out"))
            .await
            .expect("listing the directory");
        let mut names = Vec::new();
        while let Some(entry) = listing.next().await {
            names.push(entry.expect("a listing entry").name);
        }
        assert_eq!(names, ["b.txt"]);
    }))
    .await;
}
