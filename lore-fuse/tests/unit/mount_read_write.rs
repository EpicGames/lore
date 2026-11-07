// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A read-write mount served by an in-memory `Filesystem`, driven through real system calls
//! against this machine's `/dev/fuse`. Gated by `skip_without_fuse!`.

use std::ffi::CString;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::test_util::TempDir;
use lore_fuse::abi::Opcode;
use lore_fuse::init::READ_WRITE_MAX_WRITE;
use lore_fuse::mount::unmount_path;
use lore_fuse::skip_without_fuse;

use crate::memory_fs::MemoryFs;
use crate::memory_fs::mount_read_write;

/// Lazily unmounts `path` on drop, so a test that panics midway never leaves a mount behind.
struct PathUnmountGuard(PathBuf);

impl Drop for PathUnmountGuard {
    fn drop(&mut self) {
        let _ = unmount_path(&self.0, true);
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + i / 4096) as u8).collect()
}

fn is_not_found(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_err_and(|error| error.kind() == ErrorKind::NotFound)
}

fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    let to = CString::new(to.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    // SAFETY: both paths are valid NUL-terminated strings kept alive for the call.
    let ret = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn read_write_mount_serves_create_write_read_truncate_chmod_rename_and_unlink() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-read-write-test-");
    let guard = PathUnmountGuard(dir.path().to_path_buf());
    let fs = MemoryFs::new();
    let (_mount, session) = mount_read_write(dir.path(), Arc::clone(&fs), 2).await;
    let root = dir.path().to_path_buf();
    let content = pattern(3 << 20);

    let path = root.join("a.bin");
    let written = content.clone();
    let file = lore_base::lore_spawn_blocking!(move || -> std::io::Result<File> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&path)?;
        file.write_all(&written)?;
        Ok(file)
    })
    .await
    .expect("blocking task must not panic")
    .expect("open O_CREAT and write 3 MiB through the mount");

    let create_fh = fs.last_create_fh.load(Ordering::SeqCst);
    assert_ne!(create_fh, 0, "CREATE must have run");
    assert_eq!(
        fs.last_write_fh.load(Ordering::SeqCst),
        create_fh,
        "writes on the created descriptor carry the CREATE handle"
    );
    assert_eq!(
        fs.largest_write.load(Ordering::SeqCst),
        READ_WRITE_MAX_WRITE as usize,
        "a large buffered write arrives in WRITEs of the negotiated max_write"
    );
    assert!(fs.content(&[b"a.bin"]) == Some(content.clone()));

    let path = root.join("a.bin");
    let (read_back, truncated_len, mode, file) =
        lore_base::lore_spawn_blocking!(move || -> std::io::Result<(Vec<u8>, u64, u32, File)> {
            let read_back = std::fs::read(&path)?;
            file.set_len(1000)?;
            let truncated_len = file.metadata()?.len();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            let mode = std::fs::metadata(&path)?.permissions().mode();
            Ok((read_back, truncated_len, mode, file))
        })
        .await
        .expect("blocking task must not panic")
        .expect("read back, ftruncate and chmod through the mount");
    assert!(
        read_back == content,
        "read back must return what was written"
    );
    assert_eq!(truncated_len, 1000);
    assert_eq!(fs.content(&[b"a.bin"]).map(|bytes| bytes.len()), Some(1000));
    assert_eq!(mode & 0o7777, 0o600);
    lore_base::lore_spawn_blocking!(move || drop(file))
        .await
        .expect("closing must not panic");

    let moved = root.clone();
    let observed = lore_base::lore_spawn_blocking!(move || -> std::io::Result<[bool; 5]> {
        std::fs::create_dir(moved.join("d"))?;
        let made_dir = std::fs::metadata(moved.join("d"))?.is_dir();
        std::fs::rename(moved.join("a.bin"), moved.join("d/b.bin"))?;
        let renamed = is_not_found(&moved.join("a.bin"))
            && std::fs::metadata(moved.join("d/b.bin"))?.len() == 1000;
        rename_noreplace(&moved.join("d/b.bin"), &moved.join("d/c.bin"))?;
        let renamed2 = is_not_found(&moved.join("d/b.bin"));
        let reopened = OpenOptions::new().write(true).open(moved.join("d/c.bin"))?;
        reopened.write_all_at(b"xyz", 0)?;
        drop(reopened);
        let unlinked_before = std::fs::metadata(moved.join("d/c.bin")).is_ok();
        std::fs::remove_file(moved.join("d/c.bin"))?;
        let unlinked = is_not_found(&moved.join("d/c.bin"));
        Ok([made_dir, renamed, renamed2, unlinked_before, unlinked])
    })
    .await
    .expect("blocking task must not panic")
    .expect("mkdir, rename, renameat2, write and unlink through the mount");
    assert_eq!(
        observed, [true; 5],
        "mkdir, rename, rename2, written file present, unlinked"
    );
    assert_eq!(
        fs.last_write_fh.load(Ordering::SeqCst),
        0,
        "a write after zero-message open carries handle 0"
    );
    assert_eq!(fs.content(&[b"d", b"c.bin"]), None);

    let metrics = session.metrics();
    assert_eq!(
        metrics.snapshot(Opcode::Open).count,
        1,
        "after one ENOSYS the kernel must stop sending OPEN"
    );
    for (opcode, at_least) in [
        (Opcode::Create, 1),
        (Opcode::Write, 4),
        (Opcode::Setattr, 2),
        (Opcode::Mkdir, 1),
        (Opcode::Rename, 1),
        (Opcode::Rename2, 1),
        (Opcode::Unlink, 1),
    ] {
        let count = metrics.snapshot(opcode).count;
        assert!(count >= at_least, "{opcode:?} served {count} times");
    }

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown must complete promptly");
    drop(guard);
}
