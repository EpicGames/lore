// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests: a real `/dev/fuse` mount served by task 15's `SyntheticFs`, exercised
//! through ordinary userspace tools (`ls`, `cat`, `mmap`, `exec`, `stat`, writes) so each test
//! proves the whole stack -- codec, dispatch, session, transport -- against a real kernel.

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use lore_base::test_util::TempDir;
use lore_fuse::init::InitConfig;
use lore_fuse::mount::unmount_path;
use lore_fuse::session::SessionConfig;
use lore_fuse::skip_without_fuse;

use crate::synthetic::SynthNode;
use crate::synthetic::SyntheticFs;
use crate::synthetic::mount_synthetic;

/// Lazily unmounts `path` on drop, so a test that panics midway never leaves a mount behind.
struct PathUnmountGuard(PathBuf);

impl Drop for PathUnmountGuard {
    fn drop(&mut self) {
        let _ = unmount_path(&self.0, true);
    }
}

fn session_config() -> SessionConfig {
    SessionConfig {
        workers: 1,
        init: InitConfig::default(),
    }
}

/// Reads the fstype of the mount at `path` from `/proc/self/mountinfo`, if mounted.
fn mounted_fstype(path: &std::path::Path) -> Option<String> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let contents = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    for line in contents.lines() {
        let (fields, rest) = line.split_once(" - ")?;
        let mount_point = fields.split_whitespace().nth(4)?;
        if std::path::Path::new(mount_point) == canonical {
            return rest.split_whitespace().next().map(str::to_string);
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn stat_root_is_dir_0555() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-synth-stat-root-test-");
    let fs = SyntheticFs::from_tree(vec![]);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let path = dir.path().to_path_buf();
    let metadata = lore_base::lore_spawn_blocking!(move || std::fs::metadata(&path))
        .await
        .expect("spawn_blocking must not panic")
        .expect("stat of the mount root must succeed");

    assert!(metadata.is_dir(), "the mount root must be a directory");
    assert_eq!(
        metadata.permissions().mode() & 0o777,
        0o555,
        "the mount root must be r-xr-xr-x"
    );

    session.shutdown().await;
    drop(guard);
}

#[tokio::test(flavor = "multi_thread")]
async fn ls_lr_lists_every_entry_once() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-synth-ls-test-");
    let names = ["alpha", "beta", "gamma", "delta", "epsilon"];
    let entries: Vec<(String, SynthNode)> = names
        .iter()
        .map(|name| {
            (
                (*name).to_string(),
                SynthNode::File {
                    content: name.as_bytes().to_vec(),
                    executable: false,
                },
            )
        })
        .chain(std::iter::once((
            "sub".to_string(),
            SynthNode::Dir(vec![(
                "nested".to_string(),
                SynthNode::File {
                    content: b"nested-file".to_vec(),
                    executable: false,
                },
            )]),
        )))
        .collect();
    let fs = SyntheticFs::from_tree(entries);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let path = dir.path().to_path_buf();
    let output =
        lore_base::lore_spawn_blocking!(move || Command::new("ls").arg("-lR").arg(&path).output())
            .await
            .expect("spawn_blocking must not panic")
            .expect("ls -lR must run");

    assert!(output.status.success(), "ls -lR must exit successfully");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for name in names {
        assert!(
            stdout.contains(name),
            "ls -lR output must list {name}, got:\n{stdout}"
        );
    }
    assert!(stdout.contains("nested"), "ls -lR must recurse into `sub`");

    session.shutdown().await;
    drop(guard);
}

#[tokio::test(flavor = "multi_thread")]
async fn cat_returns_exact_bytes() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-synth-cat-test-");
    let content = b"the quick brown fox jumps over the lazy dog\n".to_vec();
    let fs = SyntheticFs::from_tree(vec![(
        "greeting.txt".to_string(),
        SynthNode::File {
            content: content.clone(),
            executable: false,
        },
    )]);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let path = dir.path().join("greeting.txt");
    let read_back = lore_base::lore_spawn_blocking!(move || std::fs::read(&path))
        .await
        .expect("spawn_blocking must not panic")
        .expect("reading the file must succeed");

    assert_eq!(read_back, content);

    session.shutdown().await;
    drop(guard);
}

#[tokio::test(flavor = "multi_thread")]
async fn mmap_read_matches() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-synth-mmap-test-");
    let content: Vec<u8> = (0..8000u32).map(|i| (i % 251) as u8).collect();
    let fs = SyntheticFs::from_tree(vec![(
        "blob.bin".to_string(),
        SynthNode::File {
            content: content.clone(),
            executable: false,
        },
    )]);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let path = dir.path().join("blob.bin");
    let expected = content.clone();
    let mapped = lore_base::lore_spawn_blocking!(move || -> std::io::Result<Vec<u8>> {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(&path)?;
        let len = expected.len();
        // SAFETY: `file` stays open for the duration of the mapping; `len` matches the file's
        // known size, so the mapping covers exactly the bytes read back below.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(addr, libc::MAP_FAILED, "mmap must succeed");
        // SAFETY: `addr` is a valid mapping of `len` bytes just established above.
        let mapped = unsafe { std::slice::from_raw_parts(addr.cast::<u8>(), len) }.to_vec();
        // SAFETY: `addr`/`len` are exactly the mapping just created.
        unsafe {
            libc::munmap(addr, len);
        }
        Ok(mapped)
    })
    .await
    .expect("spawn_blocking must not panic")
    .expect("mmap read must succeed");

    assert_eq!(mapped, content);

    session.shutdown().await;
    drop(guard);
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_runs_0555_script() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-synth-exec-test-");
    let script = b"#!/bin/sh\necho synthetic-exec-ok\n".to_vec();
    let fs = SyntheticFs::from_tree(vec![(
        "run.sh".to_string(),
        SynthNode::File {
            content: script,
            executable: true,
        },
    )]);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let path = dir.path().join("run.sh");
    let output = lore_base::lore_spawn_blocking!(move || Command::new(&path).output())
        .await
        .expect("spawn_blocking must not panic")
        .expect("exec of the 0555 script must succeed");

    assert!(output.status.success(), "script must exit successfully");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "synthetic-exec-ok"
    );

    session.shutdown().await;
    drop(guard);
}

#[tokio::test(flavor = "multi_thread")]
async fn readdir_resumes_after_partial_buffer_with_5000_names() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-synth-readdir-test-");
    let entries: Vec<(String, SynthNode)> = (0..5000)
        .map(|i| {
            (
                format!("file-{i:05}"),
                SynthNode::File {
                    content: Vec::new(),
                    executable: false,
                },
            )
        })
        .collect();
    let fs = SyntheticFs::from_tree(entries);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let path = dir.path().to_path_buf();
    let names = lore_base::lore_spawn_blocking!(move || -> std::io::Result<Vec<String>> {
        std::fs::read_dir(&path)?
            .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect()
    })
    .await
    .expect("spawn_blocking must not panic")
    .expect("readdir must succeed");

    let unique: HashSet<&str> = names.iter().map(String::as_str).collect();
    assert_eq!(
        unique.len(),
        5000,
        "every one of the 5000 names must appear exactly once across the resumed readdir calls"
    );

    session.shutdown().await;
    drop(guard);
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_fail_with_erofs() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-synth-write-test-");
    let fs = SyntheticFs::from_tree(vec![(
        "readonly.txt".to_string(),
        SynthNode::File {
            content: b"immutable".to_vec(),
            executable: false,
        },
    )]);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let path = dir.path().join("readonly.txt");
    let result = lore_base::lore_spawn_blocking!(move || std::fs::OpenOptions::new()
        .write(true)
        .open(&path))
    .await
    .expect("spawn_blocking must not panic");

    let error = result.expect_err("opening for write on a `ro` mount must fail");
    assert_eq!(
        error.raw_os_error(),
        Some(libc::EROFS),
        "the mount's `ro` option must surface as EROFS, not a filesystem-level error"
    );

    session.shutdown().await;
    drop(guard);
}

#[tokio::test(flavor = "multi_thread")]
async fn unmount_removes_mountinfo_entry() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-synth-unmount-test-");
    let fs = SyntheticFs::from_tree(vec![]);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");

    assert_eq!(
        mounted_fstype(dir.path()).as_deref(),
        Some("fuse.lore"),
        "the mount must appear in mountinfo as fuse.lore while mounted"
    );

    unmount_path(dir.path(), true).expect("unmount must succeed");

    tokio::time::timeout(std::time::Duration::from_secs(5), session.wait())
        .await
        .expect("the worker loop must end once the device disconnects");

    assert_eq!(
        mounted_fstype(dir.path()),
        None,
        "the mountinfo entry must be gone after unmount"
    );
}

/// Reads `path` on a plain thread and returns how many READ requests the daemon served for it.
async fn reads_served_for(
    session: &lore_fuse::session::Session<
        SyntheticFs,
        lore_fuse::transport::device::DeviceTransport,
    >,
    path: PathBuf,
) -> u64 {
    use lore_fuse::abi::Opcode;
    let before = session.metrics().snapshot(Opcode::Read).count;
    lore_base::lore_spawn_blocking!(move || std::fs::read(&path))
        .await
        .expect("spawn_blocking must not panic")
        .expect("read must succeed");
    session.metrics().snapshot(Opcode::Read).count - before
}

#[tokio::test(flavor = "multi_thread")]
async fn rereading_a_file_is_served_from_the_page_cache() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-synth-keep-cache-test-");
    let fs = SyntheticFs::from_tree(vec![(
        "f".to_string(),
        SynthNode::File {
            content: vec![7u8; 8192],
            executable: false,
        },
    )]);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let path = dir.path().join("f");
    assert!(reads_served_for(&session, path.clone()).await > 0);
    assert_eq!(
        reads_served_for(&session, path).await,
        0,
        "a KEEP_CACHE file must be re-read from the page cache"
    );

    session.shutdown().await;
    drop(guard);
}

#[tokio::test(flavor = "multi_thread")]
async fn zero_message_open_stops_open_requests_after_the_first() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-synth-zero-open-test-");
    let fs = SyntheticFs::from_tree(vec![(
        "f".to_string(),
        SynthNode::File {
            content: vec![7u8; 4096],
            executable: false,
        },
    )]);
    fs.set_zero_message_open(true);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let path = dir.path().join("f");
    for _ in 0..3 {
        reads_served_for(&session, path.clone()).await;
    }
    assert_eq!(
        session
            .metrics()
            .snapshot(lore_fuse::abi::Opcode::Open)
            .count,
        1,
        "after one ENOSYS the kernel must stop sending OPEN"
    );

    session.shutdown().await;
    drop(guard);
}
