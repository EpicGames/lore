// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The bound on the syscalls an SWFS operation makes on its mount (ADR-00035), and the content of
//! what it wrote there, read through the mount a range at a time within it.
// Each test drives an operation over its mount resources and a revision, whose future is
// larger than the library's threshold; a test runs it once.
#![allow(clippy::large_futures)]
use bytes::Bytes;
use bytes::BytesMut;
use futures::future::join_all;
use lore_base::test_util::TempDir;
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::swfs::api_interface::SwfsInterface;
use lore_revision::fs::swfs::filesystem::SwfsOperation;
use lore_revision::fs::swfs::frozen_files::FrozenSnapshot;
use lore_revision::fs::swfs::mount_io::MountIo;
use lore_revision::fs::swfs::mount_resources::MountResources;
use lore_revision::fs::swfs::paths::MountPath;
use lore_storage::WindowRead;

use super::listing_fixture::in_context;
use super::listing_fixture::node;
use super::listing_fixture::writable_test_repository;

/// An operation over an empty mount whose syscalls there `mount_io` bounds.
async fn bounded_by(mount_io: &'static MountIo) -> (SwfsOperation, TempDir) {
    let mount = TempDir::new("swfs-mount-io");
    let repository = writable_test_repository().await;
    let mount_root = mount.path().join("mount");
    std::fs::create_dir_all(&mount_root).expect("making the mount directory");
    let mount_path = MountPath::new(&mount_root).expect("a mount path");
    let resources = MountResources::for_test(repository, None, mount_path).await;
    resources.modified().set_trusted(true);
    let operation = SwfsOperation::for_test_bounded(
        SwfsInterface::unmounted(resources),
        &mount_root,
        FrozenSnapshot::empty(),
        mount_io,
    );
    (operation, mount)
}

/// `size` bytes no window of which repeats another at a different offset.
fn pattern(size: usize) -> Bytes {
    (0..size)
        .map(|index| (index % 251) as u8 ^ (index / 251) as u8)
        .collect::<Vec<u8>>()
        .into()
}

#[tokio::test]
async fn reads_of_what_the_operation_wrote_to_the_mount_never_exceed_the_bound() {
    in_context(async {
        let mount_io: &'static MountIo = Box::leak(Box::new(MountIo::new(2)));
        let (operation, mount) = bounded_by(mount_io).await;
        let paths: Vec<_> = (0..32)
            .map(|index| node(&format!("dir/{index}.txt")))
            .collect();
        std::fs::create_dir_all(mount.path().join("mount/dir")).expect("making dir");
        for path in &paths {
            operation
                .write_file(path, pattern(4096))
                .await
                .expect("writing to the mount");
        }

        let reads = paths.iter().map(|path| {
            let operation = &operation;
            async move {
                operation.file_info(path).await.expect("reading file info");
                let source = operation
                    .content_source(path)
                    .await
                    .expect("naming the content");
                source.read_all().await.expect("reading the content")
            }
        });
        let directory = node("dir");
        let listings = (0..8).map(|_| operation.read_directory(&directory));
        let (contents, listings) = futures::join!(join_all(reads), join_all(listings));

        assert!(contents.iter().all(|content| *content == pattern(4096)));
        assert!(listings.into_iter().all(|listing| listing.is_ok()));
        assert_eq!(
            mount_io.most_in_flight(),
            2,
            "the reads either exceeded the bound or never reached it"
        );
    })
    .await;
}

#[tokio::test]
async fn what_the_operation_wrote_to_the_mount_reads_back_across_windows() {
    in_context(async {
        let (operation, _mount) = bounded_by(MountIo::global()).await;
        let size = 5000 + 17;
        let written = pattern(size);
        operation
            .write_file(&node("merged.txt"), written.clone())
            .await
            .expect("writing to the mount");

        let source = operation
            .content_source(&node("merged.txt"))
            .await
            .expect("naming the content");
        let (content, length) = source.open_once().await.expect("opening the content");
        assert_eq!(length, size as u64);
        // Windows of 1000 bytes, the last one short, each at its own offset.
        let mut offset = 0;
        while offset < size {
            let want = (size - offset).min(1000);
            let read = content
                .read_window(
                    WindowRead::new(BytesMut::zeroed(want), 0, want),
                    offset as u64,
                )
                .await
                .expect("reading a window");
            assert_eq!(
                &read[..want],
                &written[offset..offset + want],
                "at {offset}"
            );
            offset += want;
        }
        // A window reaching past the end is refused, as a file's is.
        assert!(
            content
                .read_window(WindowRead::new(BytesMut::zeroed(1000), 0, 1000), 4500)
                .await
                .is_err()
        );
    })
    .await;
}

#[tokio::test]
async fn a_copy_of_what_the_operation_wrote_to_the_mount_crosses_its_copy_windows() {
    in_context(async {
        let (operation, mount) = bounded_by(MountIo::global()).await;
        // Larger than two of the windows a copy is made in, and not a multiple of one.
        let written = pattern(2 * 1024 * 1024 + 123);
        operation
            .write_file(&node("merged.txt"), written.clone())
            .await
            .expect("writing to the mount");

        operation
            .copy_file(&node("merged.txt"), &node("merged.txt~mine"))
            .await
            .expect("copying on the mount");

        let copied = std::fs::read(mount.path().join("mount/merged.txt~mine")).expect("the copy");
        assert!(copied == written, "the copy differs from what was written");
    })
    .await;
}

/// Whether `call` waits while no permit is free, and completes once one is.
async fn waits_for_a_permit<T>(
    mount_io: &MountIo,
    call: impl std::future::Future<Output = T>,
) -> T {
    let mut call = std::pin::pin!(call);
    {
        let _held = mount_io.permit().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), call.as_mut())
                .await
                .is_err(),
            "the call ran on the mount without a permit"
        );
    }
    call.await
}

#[tokio::test]
async fn each_call_on_the_mount_waits_for_a_permit() {
    in_context(async {
        let mount_io: &'static MountIo = Box::leak(Box::new(MountIo::new(1)));
        let (operation, _mount) = bounded_by(mount_io).await;
        let path = node("merged.txt");
        waits_for_a_permit(mount_io, operation.write_file(&path, pattern(4096)))
            .await
            .expect("writing to the mount");

        waits_for_a_permit(mount_io, operation.file_info(&path))
            .await
            .expect("reading file info");
        waits_for_a_permit(mount_io, operation.read_directory(&node("")))
            .await
            .expect("listing the directory");
        // Answered by the OS for a file on the mount, which macOS cannot say.
        let held_exactly = if cfg!(target_os = "macos") {
            None
        } else {
            Some(true)
        };
        assert_eq!(
            waits_for_a_permit(mount_io, operation.holds_name_exactly(&path)).await,
            held_exactly
        );
        let source = waits_for_a_permit(mount_io, operation.content_source(&path))
            .await
            .expect("naming the content");
        let read = waits_for_a_permit(mount_io, source.read_all())
            .await
            .expect("reading the content");
        assert!(read == pattern(4096));
        waits_for_a_permit(mount_io, operation.copy_file(&path, &node("copy.txt")))
            .await
            .expect("copying on the mount");
        waits_for_a_permit(mount_io, operation.remove(&node("copy.txt")))
            .await
            .expect("removing from the mount");
    })
    .await;
}
