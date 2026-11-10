// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Derived inode numbers: a pure function of an inode's identity, so numbers
//! survive remounts and service restarts without a persisted counter.

use lore_base::text::fold_eq;
use xxhash_rust::xxh3::xxh3_64;

use crate::fs::fuse::inode::InodeEntry;

/// What besides parent and folded name identifies an inode. Content is no part of it, so a file
/// keeps its number through edits, commits and syncs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityKind {
    Dir,
    File,
    Symlink,
}

/// The identity an inode number is derived from.
#[derive(Clone, Copy)]
pub struct Identity<'a> {
    pub parent_ino: u64,
    pub name: &'a str,
    /// [`hash_string`](lore_storage::hash::hash_string) of `name`, which the number derives from.
    pub name_hash: u64,
    pub kind: IdentityKind,
}

const DIR_TAG: u8 = 0x01;
const FILE_TAG: u8 = 0x02;
const SYMLINK_TAG: u8 = 0x03;

/// `xxh3(tag, parent LE, name hash LE)`, never 0 or 1.
pub fn derive_inode_number(identity: &Identity<'_>) -> u64 {
    let tag = match identity.kind {
        IdentityKind::Dir => DIR_TAG,
        IdentityKind::File => FILE_TAG,
        IdentityKind::Symlink => SYMLINK_TAG,
    };
    let mut bytes = [0u8; 17];
    bytes[0] = tag;
    bytes[1..9].copy_from_slice(&identity.parent_ino.to_le_bytes());
    bytes[9..].copy_from_slice(&identity.name_hash.to_le_bytes());
    skip_reserved(xxh3_64(&bytes))
}

/// The next candidate after `previous` collided: `xxh3(previous LE, round LE)`.
pub fn rehash(previous: u64, round: u32) -> u64 {
    let mut bytes = [0u8; 12];
    bytes[..8].copy_from_slice(&previous.to_le_bytes());
    bytes[8..].copy_from_slice(&round.to_le_bytes());
    xxh3_64(&bytes)
}

/// Maps the reserved numbers 0 (invalid) and 1 (`FUSE_ROOT_ID`) elsewhere.
#[lore_macro::test_pub]
pub(crate) fn skip_reserved(mut n: u64) -> u64 {
    let mut round = 0;
    while n < 2 {
        round += 1;
        n = rehash(n, round);
    }
    n
}

/// Whether `entry` is the inode `identity` describes (orphaned or not).
pub fn identity_matches(entry: &InodeEntry, identity: &Identity<'_>) -> bool {
    if entry.parent != identity.parent_ino {
        return false;
    }
    entry.kind.identity() == identity.kind
        && std::str::from_utf8(&entry.name).is_ok_and(|name| fold_eq(name, identity.name))
}
