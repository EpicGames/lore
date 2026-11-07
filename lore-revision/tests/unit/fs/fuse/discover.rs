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
