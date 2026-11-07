// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_fuse::abi::DirentType;
use lore_fuse::abi::EntryOut;
use lore_fuse::abi::dir::dirent_size;
use lore_fuse::abi::dir::direntplus_size;
use lore_fuse::codec::dirent::DirentWriter;
use lore_fuse::codec::dirent::referenced_nodeids;

#[test]
fn push_pads_name_to_8() {
    let mut storage = [0xAA_u8; 64];
    let mut writer = DirentWriter::new(&mut storage);

    assert!(writer.push(1, 1, DirentType::Reg, b"foo"));

    let expected = dirent_size(3);
    assert_eq!(writer.used(), expected);
    // The alignment padding between the name and the next record must be zeroed, not left
    // as whatever garbage the buffer held before.
    assert_eq!(storage[24 + 3..expected], vec![0_u8; expected - 27][..]);
}

#[test]
fn push_refuses_entry_that_does_not_fit_and_leaves_used_unchanged() {
    let mut storage = [0_u8; 10];
    let mut writer = DirentWriter::new(&mut storage);

    assert!(!writer.push(1, 1, DirentType::Reg, b"foo"));
    assert_eq!(writer.used(), 0);
}

#[test]
fn push_plus_exact_fit_accounting() {
    let name = b"exact";
    let size = direntplus_size(name.len());
    let mut storage = vec![0_u8; size];
    let mut writer = DirentWriter::new(&mut storage);
    let entry = EntryOut::default();

    assert!(writer.push_plus(&entry, 2, DirentType::Dir, name));
    assert_eq!(writer.used(), size);
}

#[test]
fn referenced_nodeids_are_the_records_with_a_node_id_in_order() {
    let mut storage = [0_u8; 1024];
    let mut writer = DirentWriter::new(&mut storage);
    let records: [(u64, &[u8]); 5] = [
        (0, b"."),
        (0, b".."),
        (11, b"a"),
        (0, b"plain-name"),
        (12, b"name-of-24-bytes-exactly"),
    ];
    for (nodeid, name) in records {
        let entry = EntryOut {
            nodeid,
            ..EntryOut::default()
        };
        assert!(writer.push_plus(&entry, 1, DirentType::Reg, name));
    }
    let used = writer.finish();

    let referenced: Vec<u64> = referenced_nodeids(&storage[..used]).collect();
    assert_eq!(referenced, [11, 12]);
    assert_eq!(referenced_nodeids(&[]).count(), 0);
}

/// The kernel takes no reference for `.` or `..`, whatever node id they carry.
#[test]
fn referenced_nodeids_leave_out_the_dots_whatever_their_node_id() {
    let mut storage = [0_u8; 1024];
    let mut writer = DirentWriter::new(&mut storage);
    for (nodeid, name) in [(1, &b"."[..]), (2, b".."), (3, b"..."), (4, b".a")] {
        let entry = EntryOut {
            nodeid,
            ..EntryOut::default()
        };
        assert!(writer.push_plus(&entry, 1, DirentType::Dir, name));
    }
    let used = writer.finish();

    let referenced: Vec<u64> = referenced_nodeids(&storage[..used]).collect();
    assert_eq!(referenced, [3, 4]);
}

#[test]
fn finish_returns_bytes_used() {
    let mut storage = [0_u8; 64];
    let mut writer = DirentWriter::new(&mut storage);
    writer.push(1, 1, DirentType::Reg, b"foo");
    let used_before_finish = writer.used();

    assert_eq!(writer.finish(), used_before_finish);
}
