// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::hash::Hasher;

/// Takes the leading eight bytes of the last write a key makes as its hash, which for a key that
/// writes a `u64` is that `u64`. For a key that is already uniformly distributed, such as a
/// [`Hash`](super::Hash) or a digest, those bytes hash as well as any function of the whole key,
/// and hashing the key again would be a second pass over it. Each write must be at least eight
/// bytes long, which debug builds assert.
#[derive(Default)]
pub struct PassThroughHasher(u64);

impl Hasher for PassThroughHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        debug_assert!(
            bytes.len() >= 8,
            "a key hashed by its leading bytes writes at least eight"
        );
        let mut leading = [0u8; 8];
        let taken = bytes.len().min(leading.len());
        leading[..taken].copy_from_slice(&bytes[..taken]);
        self.0 = u64::from_ne_bytes(leading);
    }
}
