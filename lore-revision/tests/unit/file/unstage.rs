// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use lore_base::types::Address;
use lore_base::types::Hash;
use lore_revision::file::unstage::UnstageError;
use lore_revision::file::unstage::takes_back_content;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;

/// A file holding `flags`, `size` and the hash `hash`, a zero hash where `hash` is 0.
fn file(flags: NodeFlags, hash: u64, size: u64) -> Node {
    Node {
        flags: (flags | NodeFlags::File).bits(),
        size,
        address: Address {
            hash: Hash::from_u64(hash),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A staged modification of a file with the hash `hash` and `size`.
fn staged_file(hash: u64, size: u64) -> Node {
    file(NodeFlags::StagedModify, hash, size)
}

/// A file the current revision holds.
fn current_file() -> Node {
    file(NodeFlags::NoFlags, 7, 9)
}

/// An unstage takes back the current revision's file for a staged zero hash, content not yet
/// stored or an empty file, and leaves a staged hash as it is.
#[test]
fn an_unstage_takes_back_the_current_file_for_a_zero_hash() {
    assert!(takes_back_content(&staged_file(0, 9), &current_file()).expect("A live file"));
    assert!(takes_back_content(&staged_file(0, 0), &current_file()).expect("A live file"));
    assert!(!takes_back_content(&staged_file(1, 9), &current_file()).expect("A stored file"));
}

/// Where the current revision holds no file to take back, an unstage refuses to leave a zero hash
/// with a nonzero size, content no commit stored, and leaves an empty file as it is.
#[test]
fn an_unstage_refuses_to_leave_content_no_commit_stored() {
    let refused = takes_back_content(&staged_file(0, 9), &Node::default());
    assert!(
        matches!(refused, Err(UnstageError::Internal(_))),
        "{refused:?}"
    );
    assert!(!takes_back_content(&staged_file(0, 0), &Node::default()).expect("An empty file"));
}
