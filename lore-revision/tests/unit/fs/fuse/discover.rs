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

/// The iterator yields every `fuse.lore` mount in table order, past any number of optional fields
/// and with its mount point un-escaped, and leaves out other file systems and sources that name no
/// instance.
#[test]
fn lore_mounts_yields_every_fuse_lore_mount() {
    use lore_revision::fs::fuse::discover::lore_mounts;

    let first = InstanceId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").expect("valid hex");
    let second = InstanceId::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").expect("valid hex");
    let mountinfo = concat!(
        "22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw\n",
        "25 22 0:24 / /a/My\\040Repo rw,relatime - fuse.lore lore:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa rw\n",
        "26 22 0:25 / /b rw shared:3 master:7 - fuse.lore lore:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb rw\n",
        "27 22 0:26 / /c rw - fuse.lore odd\\040source rw\n",
        "28 22 0:27 / /d rw - fuse.lore lore:0aaa rw\n",
        "29 22 0:28 / /e rw - fuse.sshfs lore:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa rw\n",
        "30 22 0:29 / /f rw\n",
    );

    let mounts: Vec<_> = lore_mounts(mountinfo).collect();
    assert_eq!(
        mounts,
        vec![
            (PathBuf::from("/a/My Repo"), first),
            (PathBuf::from("/b"), second),
        ]
    );
}

/// Of two mounts stacked at one mount point above a path, the one listed first answers.
#[test]
fn the_first_of_stacked_mounts_answers_a_path() {
    let first = InstanceId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").expect("valid hex");
    let mountinfo = concat!(
        "25 24 0:24 / /a/b rw - fuse.lore lore:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa rw\n",
        "26 25 0:25 / /a/b rw - fuse.lore lore:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb rw\n",
        "27 24 0:26 / /a rw - fuse.lore lore:cccccccccccccccccccccccccccccccc rw\n",
    );

    assert_eq!(
        lore_mount_for_path(mountinfo, Path::new("/a/b/c")),
        Some((PathBuf::from("/a/b"), first))
    );
}

/// The listing yields every `fuse.lore` mount of the table read for a path, each that answers,
/// and fails where the table can't be read.
#[test]
fn the_listing_yields_every_answering_lore_mount() {
    use lore_revision::fs::fuse::discover::live_lore_mounts_kept;
    use lore_revision::fs::fuse::discover::test_util::fuse_lore_entry;
    use lore_revision::fs::fuse::discover::test_util::plant_table;
    use lore_revision::fs::fuse::discover::test_util::plant_unreadable_table;

    let root = lore_base::test_util::TempDir::new("lore-discover-listing-");
    let (first, second) = (root.path().join("first"), root.path().join("second"));
    let (first_id, second_id) = (InstanceId::generate(), InstanceId::generate());
    let table = [
        fuse_lore_entry(&first, first_id),
        fuse_lore_entry(&second, second_id),
    ]
    .concat();
    let planted = plant_table(root.path(), table);

    assert_eq!(
        live_lore_mounts_kept(root.path(), |_| Ok(true)).expect("the planted table reads"),
        vec![(first, first_id), (second, second_id)]
    );
    drop(planted);
    let _unreadable = plant_unreadable_table(root.path());
    assert!(live_lore_mounts_kept(root.path(), |_| Ok(true)).is_err());
}

/// A live mount belongs to the external root that holds its instance's external directory, and
/// the check fails where the mount table can't be read.
#[test]
fn a_live_mount_belongs_to_the_root_holding_its_instance() {
    use lore_revision::fs::fuse::discover::test_util::fuse_lore_entry;
    use lore_revision::fs::fuse::discover::test_util::plant_table;
    use lore_revision::fs::fuse::discover::test_util::plant_unreadable_table;
    use lore_revision::global::external_dir::mounts::live_mounts_in;

    let root = lore_base::test_util::TempDir::new("lore-discover-member-");
    let elsewhere = lore_base::test_util::TempDir::new("lore-discover-member-elsewhere-");
    let (member, other) = (InstanceId::generate(), InstanceId::generate());
    std::fs::create_dir(root.path().join(member.text_encoding().as_str()))
        .expect("create the member's external directory");
    std::fs::create_dir(elsewhere.path().join(other.text_encoding().as_str()))
        .expect("create the other external directory");
    let (member_mount, other_mount) = (
        elsewhere.path().join("member"),
        elsewhere.path().join("other"),
    );
    let table = [
        fuse_lore_entry(&member_mount, member),
        fuse_lore_entry(&other_mount, other),
    ]
    .concat();
    let planted = plant_table(root.path(), table);

    assert_eq!(
        live_mounts_in(root.path()).expect("the planted table reads"),
        vec![member_mount]
    );
    drop(planted);
    let _unreadable = plant_unreadable_table(root.path());
    assert!(live_mounts_in(root.path()).is_err());
}

/// The check probes only the mounts of the root's instances, so a mount of another root, whose
/// server may not answer, can't hold it up.
#[test]
fn the_check_probes_only_the_roots_mounts() {
    use lore_revision::fs::fuse::discover::test_util::fuse_lore_entry;
    use lore_revision::fs::fuse::discover::test_util::plant_table;
    use lore_revision::fs::fuse::discover::test_util::probes;
    use lore_revision::global::external_dir::mounts::live_mounts_in;

    let root = lore_base::test_util::TempDir::new("lore-discover-probed-");
    let elsewhere = lore_base::test_util::TempDir::new("lore-discover-probed-elsewhere-");
    let member = InstanceId::generate();
    std::fs::create_dir(root.path().join(member.text_encoding().as_str()))
        .expect("create the member's external directory");
    let table = [
        fuse_lore_entry(&elsewhere.path().join("member"), member),
        fuse_lore_entry(&elsewhere.path().join("other"), InstanceId::generate()),
    ]
    .concat();
    let _planted = plant_table(root.path(), table);

    let before = probes();
    live_mounts_in(root.path()).expect("the planted table reads");

    assert_eq!(probes() - before, 1, "only the member's mount is probed");
}

/// The tests that mount FUSE, which the FUSE CI jobs select by this module's name.
mod needs_fuse {
    use super::*;

    /// The listing leaves out a stale mount, left by a server that ended without unmounting it.
    #[test]
    fn the_listing_leaves_out_a_stale_mount() {
        use lore_fuse::mount::MountOptions;
        use lore_fuse::mount::mount;
        use lore_fuse::mount::unmount_path;
        use lore_revision::fs::fuse::discover::live_lore_mounts_kept;
        use lore_revision::fs::fuse::discover::test_util::fuse_lore_entry;
        use lore_revision::fs::fuse::discover::test_util::plant_table;

        struct Unmount<'a>(&'a Path);
        impl Drop for Unmount<'_> {
            fn drop(&mut self) {
                let _ = unmount_path(self.0, true);
            }
        }

        lore_fuse::skip_without_fuse!();

        let root = lore_base::test_util::TempDir::new("lore-discover-stale-");
        let stale = root.path().join("stale");
        let live = root.path().join("live");
        std::fs::create_dir(&stale).expect("create the stale mount point");
        let (stale_id, live_id) = (InstanceId::generate(), InstanceId::generate());
        let options = MountOptions {
            fsname: format!("lore:{}", stale_id.text_encoding()),
            max_read: 1 << 20,
            fusermount: None,
        };
        drop(mount(&stale, &options).expect("mount"));
        let _unmount = Unmount(&stale);
        let table = [
            fuse_lore_entry(&stale, stale_id),
            fuse_lore_entry(&live, live_id),
        ]
        .concat();
        let _planted = plant_table(root.path(), table);

        assert_eq!(
            live_lore_mounts_kept(root.path(), |_| Ok(true)).expect("the planted table reads"),
            vec![(live, live_id)]
        );
    }
}
