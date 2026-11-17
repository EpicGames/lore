// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::path::PathBuf;

use lore_revision::fs::fuse::discover::lore_mount_for_path;
use lore_revision::instance::InstanceId;

#[test]
fn parses_mountinfo_line_with_escaped_spaces() {
    let instance_id = InstanceId::from_hex("0123456789abcdef0123456789abcdef").expect("valid hex");
    let line = "25 24 0:24 / /home/user/My\\040Repo rw,relatime shared:1 - fuse.lore \
                 lore:0123456789abcdef0123456789abcdef rw,user_id=1000\n";
    let found = lore_mount_for_path(line, Path::new("/home/user/My Repo/sub/dir"));
    assert_eq!(
        found,
        Some((PathBuf::from("/home/user/My Repo"), instance_id))
    );
}

#[test]
fn finds_innermost_fuse_lore_mount_for_nested_path() {
    let outer_id = InstanceId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").expect("valid hex");
    let inner_id = InstanceId::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").expect("valid hex");
    let mountinfo = concat!(
        "25 24 0:24 / /a/b rw,relatime - fuse.lore lore:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa rw\n",
        "26 25 0:25 / /a/b/c rw,relatime - fuse.lore lore:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb rw\n",
    );

    let nested = lore_mount_for_path(mountinfo, Path::new("/a/b/c/d/e"));
    assert_eq!(nested, Some((PathBuf::from("/a/b/c"), inner_id)));

    let above_inner_mount = lore_mount_for_path(mountinfo, Path::new("/a/b/x"));
    assert_eq!(above_inner_mount, Some((PathBuf::from("/a/b"), outer_id)));
}

#[test]
fn ignores_other_fuse_subtypes() {
    let mountinfo = "25 24 0:24 / /mnt/other rw,relatime - fuse.sshfs user@host: rw\n";
    let found = lore_mount_for_path(mountinfo, Path::new("/mnt/other/file"));
    assert_eq!(found, None);
}

#[test]
fn returns_none_when_no_lore_mount() {
    let mountinfo = "25 24 8:1 / / rw,relatime - ext4 /dev/sda1 rw\n";
    let found = lore_mount_for_path(mountinfo, Path::new("/home/user/project"));
    assert_eq!(found, None);
}

/// A table planted for a mount point is what a lookup of a path below it reads, on any thread,
/// and matches as the file would; a path elsewhere reads the real table.
#[test]
fn a_planted_table_matches_as_the_file_would() {
    use lore_revision::fs::fuse::discover::lore_mount_containing;
    use lore_revision::fs::fuse::discover::test_util::fuse_lore_entry;
    use lore_revision::fs::fuse::discover::test_util::plant_table;

    let root = lore_base::test_util::TempDir::new("lore-discover-planted-");
    let mount_point = root.path().join("my instance");
    let instance_id = InstanceId::generate();
    let _planted = plant_table(&mount_point, fuse_lore_entry(&mount_point, instance_id));

    let below = mount_point.join("sub").join("file");
    let found = std::thread::spawn(move || lore_mount_containing(&below))
        .join()
        .expect("look up on another thread");

    assert_eq!(found, Some((mount_point.clone(), instance_id)));
    assert_eq!(lore_mount_containing(&root.path().join("elsewhere")), None);
}

/// A table planted to fail to read reads as no mount, as an unreadable `/proc/self/mountinfo`
/// does, and its guard's drop ends it.
#[test]
fn an_unreadable_planted_table_reads_as_no_mount() {
    use lore_revision::fs::fuse::discover::lore_mount_containing;
    use lore_revision::fs::fuse::discover::test_util::fuse_lore_entry;
    use lore_revision::fs::fuse::discover::test_util::plant_table;
    use lore_revision::fs::fuse::discover::test_util::plant_unreadable_table;

    let root = lore_base::test_util::TempDir::new("lore-discover-unreadable-");
    let mount_point = root.path().join("instance");
    let instance_id = InstanceId::generate();
    let outer = plant_table(&mount_point, fuse_lore_entry(&mount_point, instance_id));
    let inner = plant_unreadable_table(&mount_point.join("sub"));

    assert_eq!(
        lore_mount_containing(&mount_point.join("sub").join("file")),
        None
    );
    drop(inner);
    assert_eq!(
        lore_mount_containing(&mount_point.join("sub").join("file")),
        Some((mount_point.clone(), instance_id))
    );
    drop(outer);
    assert_eq!(lore_mount_containing(&mount_point.join("sub")), None);
}
