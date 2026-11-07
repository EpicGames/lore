// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::path::PathBuf;

use lore_base::test_util::TempDir;
use lore_fuse::FuseError;
use lore_fuse::mount::MountAccess;
use lore_fuse::mount::MountOptions;
use lore_fuse::mount::is_stale_mount;
use lore_fuse::mount::mount;
use lore_fuse::mount::mount_with_access;
use lore_fuse::mount::unmount_path;
use lore_fuse::mount::*;
use lore_fuse::skip_without_fuse;

/// A mount is found by its source wherever it is listed: under a renamed parent, beneath or
/// above another mount at the same path, past any number of optional fields, and with the
/// kernel's octal escapes. A source that only appears inside a path is not a mount of it.
#[test]
fn the_mount_table_lists_a_mount_by_its_source() {
    let table = b"22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw\n\
        901 22 0:160 / /tmp/renamed\\040dir/clone rw,nosuid shared:1304 - fuse.lore lore:0aaa rw,user_id=1000\n\
        902 901 0:161 / /tmp/renamed\\040dir/clone rw shared:1305 master:7 - fuse.lore lore:0bbb rw\n\
        903 22 0:162 / /tmp/lore:0ccc rw - tmpfs tmpfs rw\n\
        904 22 0:163 / /tmp/odd rw - fuse.lore odd\\040source rw\n";

    assert!(
        mount_table_lists(table, "lore:0aaa"),
        "under a renamed parent"
    );
    assert!(mount_table_lists(table, "lore:0bbb"), "stacked on another");
    assert!(mount_table_lists(table, "odd source"), "an escaped source");
    assert!(
        !mount_table_lists(table, "lore:0ccc"),
        "a source only in a path"
    );
    assert!(
        !mount_table_lists(table, "lore:0aa"),
        "a prefix of a source"
    );
    assert!(!mount_table_lists(b"", "lore:0aaa"), "an empty table");
}

#[test]
fn unprivileged_unmount_failure_reports_fusermount_not_umount2() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 || locate_fusermount3().is_none() {
        return;
    }
    let dir = lore_base::test_util::TempDir::new("lore-fuse-unmount-error-test-");

    let error = unmount_path(dir.path(), false).expect_err("nothing is mounted there");

    let text = error.to_string();
    assert!(
        !text.contains("umount2"),
        "an unprivileged caller must see fusermount3's reason, not a masking EPERM: {text}"
    );
}

#[test]
fn missing_fusermount_gives_named_error() {
    let options = MountOptions {
        fsname: "lore:test".to_string(),
        max_read: 1 << 20,
        fusermount: Some(PathBuf::from("/nonexistent")),
    };

    let error = options.fusermount_path().expect_err("must be missing");

    assert!(matches!(error, FuseError::FusermountMissing));
}

#[test]
fn options_string_matches_spec() {
    let options = MountOptions {
        fsname: "lore:abc".to_string(),
        max_read: 1 << 20,
        fusermount: None,
    };

    assert_eq!(
        options.option_string_for(MountAccess::ReadOnly),
        "ro,default_permissions,max_read=1048576,fsname=lore:abc,subtype=lore,noatime"
    );
}

#[test]
fn access_defaults_to_read_only() {
    assert_eq!(MountAccess::default(), MountAccess::ReadOnly);
}

#[test]
fn read_write_option_string_drops_only_ro() {
    let options = MountOptions {
        fsname: "lore:abc".to_string(),
        max_read: 1 << 20,
        fusermount: None,
    };

    assert_eq!(
        options.option_string_for(MountAccess::ReadWrite),
        "default_permissions,max_read=1048576,fsname=lore:abc,subtype=lore,noatime"
    );
}

#[test]
fn privileged_mount_flags_carry_rdonly_only_for_read_only() {
    let common = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOATIME;
    assert_eq!(
        privileged_mount_flags(MountAccess::ReadOnly),
        common | libc::MS_RDONLY
    );
    assert_eq!(privileged_mount_flags(MountAccess::ReadWrite), common);
}

// Real mounts through `fusermount3` against this machine's `/dev/fuse`. Gated by
// `skip_without_fuse!`; when they run, they run for real (no mocked kernel).

/// Lazily unmounts `path` on drop, so a test that panics midway never leaves a mount behind
/// for the next run to trip over.
struct PathUnmountGuard(PathBuf);

impl Drop for PathUnmountGuard {
    fn drop(&mut self) {
        let _ = unmount_path(&self.0, true);
    }
}

/// The fstype `/proc/self/mountinfo` records for whatever is mounted at `path`, if anything.
fn mounted_fstype(path: &Path) -> Option<String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    mountinfo.lines().rev().find_map(|line| {
        let fields: Vec<&str> = line.split(' ').collect();
        let mount_point = *fields.get(4)?;
        if Path::new(mount_point) != path {
            return None;
        }
        let dash = fields.iter().position(|&field| field == "-")?;
        fields.get(dash + 1).map(|fstype| fstype.to_string())
    })
}

#[test]
fn mount_appears_in_mountinfo_as_fuse_lore_then_unmounts() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-mount-test-");
    let options = MountOptions {
        // Nothing answers INIT here, so a desktop service probing the new mount would block in it
        // and hold it busy, failing the plain unmount. GIO's services (such as gvfsd-trash) skip a
        // mount whose source is `none`.
        fsname: "none".to_string(),
        max_read: 1 << 20,
        fusermount: None,
    };

    let mount_handle = mount(dir.path(), &options).expect("mount");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    assert_eq!(mounted_fstype(dir.path()).as_deref(), Some("fuse.lore"));

    mount_handle.unmount(false).expect("unmount");
    assert_eq!(mounted_fstype(dir.path()), None);

    drop(guard);
}

/// The per-mount options `/proc/self/mountinfo` records for `path`, if mounted.
fn mount_options(path: &Path) -> Option<String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    mountinfo.lines().rev().find_map(|line| {
        let fields: Vec<&str> = line.split(' ').collect();
        if Path::new(*fields.get(4)?) != path {
            return None;
        }
        fields.get(5).map(|options| options.to_string())
    })
}

#[test]
fn read_write_mount_is_rw_and_the_default_mount_is_ro() {
    skip_without_fuse!();

    let options = MountOptions {
        fsname: "lore:access".to_string(),
        max_read: 1 << 20,
        fusermount: None,
    };

    let ro_dir = TempDir::new("lore-fuse-mount-ro-test-");
    let ro = mount(ro_dir.path(), &options).expect("read-only mount");
    let ro_guard = PathUnmountGuard(ro_dir.path().to_path_buf());
    let rw_dir = TempDir::new("lore-fuse-mount-rw-test-");
    let rw = mount_with_access(rw_dir.path(), &options, MountAccess::ReadWrite)
        .expect("read-write mount");
    let rw_guard = PathUnmountGuard(rw_dir.path().to_path_buf());

    let ro_options = mount_options(ro_dir.path()).expect("read-only mount is listed");
    let rw_options = mount_options(rw_dir.path()).expect("read-write mount is listed");
    assert_eq!(ro_options.split(',').next(), Some("ro"), "{ro_options}");
    assert_eq!(rw_options.split(',').next(), Some("rw"), "{rw_options}");

    ro.unmount(true).expect("unmount read-only");
    rw.unmount(true).expect("unmount read-write");
    drop(ro_guard);
    drop(rw_guard);
}

#[test]
fn stale_mount_detected_after_fd_close() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-stale-test-");
    let options = MountOptions {
        fsname: "lore:stale".to_string(),
        max_read: 1 << 20,
        fusermount: None,
    };

    let mount_handle = mount(dir.path(), &options).expect("mount");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    // Dropping the handle without calling `unmount` closes its sole device fd while the
    // mountpoint itself stays attached: exactly the "stale" state `is_stale_mount` detects.
    drop(mount_handle);

    assert!(is_stale_mount(dir.path()));

    drop(guard);
    assert_eq!(mounted_fstype(dir.path()), None);
}
