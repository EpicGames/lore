// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Every spelling the kernel's case-sensitive dcache may hold per child-index
//! key, positive or negative, so a swap can invalidate exactly those dentries.

use std::time::Duration;

use dashmap::DashMap;
use lore_fuse::reply::Timeouts;
use smallvec::SmallVec;

use crate::fs::fuse::inode::Name;

pub const SPELLING_BOUND: usize = 8;

pub const DEFAULT_TIMEOUTS: Timeouts = Timeouts {
    entry: Duration::from_secs(3600),
    attr: Duration::from_secs(3600),
};

pub const NEGATIVE_TIMEOUTS: Timeouts = Timeouts {
    entry: Duration::from_secs(60),
    attr: Duration::from_secs(60),
};

#[derive(Default)]
pub struct Spellings {
    pub seen: SmallVec<[Name; 2]>,
    pub negative: bool,
    pub overflowed: bool,
}

#[derive(Default)]
pub struct SpellingRecord {
    keys: DashMap<(u64, u64), Spellings>,
}

impl SpellingRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that the kernel will cache `name` under this key and returns
    /// the timeouts to reply with. Past [`SPELLING_BOUND`] spellings a
    /// dentry could go unrecorded, so every reply for the key gets zero.
    pub fn record(&self, parent: u64, name_hash: u64, name: &[u8], positive: bool) -> Timeouts {
        let key = (parent, name_hash);
        // A known key through a plain lookup: the entry API reserves room first, and grows a
        // full shard even for a key it then finds.
        let mut spellings = match self.keys.get_mut(&key) {
            Some(spellings) => spellings,
            // One short-lived write lock on this key; nothing else is locked inside.
            #[allow(clippy::disallowed_methods)]
            None => self.keys.entry(key).or_default(),
        };
        spellings.negative |= !positive;
        if !spellings.overflowed && !spellings.seen.iter().any(|seen| **seen == *name) {
            if spellings.seen.len() < SPELLING_BOUND {
                spellings.seen.push(Name::from_slice(name));
            } else {
                spellings.overflowed = true;
            }
        }
        if spellings.overflowed {
            Timeouts::ZERO
        } else if positive {
            DEFAULT_TIMEOUTS
        } else {
            NEGATIVE_TIMEOUTS
        }
    }

    pub fn take(&self, parent: u64, name_hash: u64) -> Option<Spellings> {
        self.keys
            .remove(&(parent, name_hash))
            .map(|(_, spellings)| spellings)
    }

    /// Moves out every key as `(parent, spellings)`.
    pub fn take_all(&self, out: &mut Vec<(u64, Spellings)>) {
        self.keys.retain(|&(parent, _), spellings| {
            out.push((parent, std::mem::take(spellings)));
            false
        });
    }

    /// Copies out every spelling recorded under `parent`, leaving the record as it is.
    pub fn copy_children_of(&self, parent: u64, out: &mut Vec<Name>) {
        for entry in self.keys.iter() {
            if entry.key().0 == parent {
                out.extend(entry.seen.iter().cloned());
            }
        }
    }

    /// Moves out every key under `parent` as `(name_hash, spellings)`.
    pub fn take_children_of(&self, parent: u64, out: &mut Vec<(u64, Spellings)>) {
        self.keys.retain(|&(key_parent, name_hash), spellings| {
            if key_parent != parent {
                return true;
            }
            out.push((name_hash, std::mem::take(spellings)));
            false
        });
    }
}
