// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests: the real device transport against this machine's `/dev/fuse`.

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lore_base::test_util::TempDir;
use lore_fuse::buffer::RequestBuffer;
use lore_fuse::mount::MountOptions;
use lore_fuse::mount::mount;
use lore_fuse::mount::unmount_path;
use lore_fuse::skip_without_fuse;
use lore_fuse::transport::Transport;
use lore_fuse::transport::TransportError;
use lore_fuse::transport::TransportWorker;
use lore_fuse::transport::device::DeviceTransport;

/// Lazily unmounts `path` on drop, so a test that panics midway never leaves a mount behind.
struct PathUnmountGuard(PathBuf);

impl Drop for PathUnmountGuard {
    fn drop(&mut self) {
        let _ = unmount_path(&self.0, true);
    }
}

#[tokio::test]
async fn device_transport_clones_a_distinct_fd_per_worker() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-transport-clone-test-");
    let options = MountOptions {
        fsname: "lore:clone".to_string(),
        max_read: 1 << 20,
        fusermount: None,
    };

    let mount_handle = Arc::new(mount(dir.path(), &options).expect("mount"));
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let transport = DeviceTransport::new(mount_handle.clone());
    let worker_a = transport.worker().expect("worker a");
    let worker_b = transport.worker().expect("worker b");

    assert_ne!(worker_a.as_raw_fd(), worker_b.as_raw_fd());

    drop(worker_a);
    drop(worker_b);
    // Lazily: nothing answers INIT here, so a desktop service probing the new mount blocks in it
    // and holds it busy until the connection closes.
    mount_handle.unmount(true).expect("unmount");
    drop(guard);
}

#[tokio::test]
async fn receive_after_unmount_returns_disconnected() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-transport-disconnect-test-");
    let options = MountOptions {
        // Nothing answers INIT here, so a desktop service probing the new mount would block in it
        // and hold it busy, and only a plain unmount ends the connection. GIO's services (such as
        // gvfsd-trash) skip a mount whose source is `none`.
        fsname: "none".to_string(),
        max_read: 1 << 20,
        fusermount: None,
    };

    let mount_handle = Arc::new(mount(dir.path(), &options).expect("mount"));
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let transport = DeviceTransport::new(mount_handle.clone());
    let mut worker = transport.worker().expect("worker");

    mount_handle.unmount(false).expect("unmount");

    let mut buffer = RequestBuffer::new();
    let outcome = tokio::time::timeout(Duration::from_secs(5), worker.receive(&mut buffer))
        .await
        .expect("worker.receive did not observe the unmount within 5s");

    let error = outcome.expect_err("must report disconnection");
    assert!(matches!(error, TransportError::Disconnected));

    drop(guard);
}
