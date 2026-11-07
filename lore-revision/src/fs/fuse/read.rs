// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! READ: local-first content reads with a fetch deadline.
//!
//! The dispatcher first asks with `fetch == false`; a local miss answers
//! [`ReadFault::WouldFetch`] and is retried off the worker with `fetch == true`, the only form
//! that may touch the remote and so the only one bounded by a deadline.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use dashmap::DashMap;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_base::types::Hash;
use lore_fuse::reply::Errno;
use lore_fuse::reply::ReadFault;

use crate::fs::fuse::fragment_cache::FragmentCache;
use crate::fs::fuse::inode::InodeEntry;
use crate::fs::fuse::inode::claim_once;
use crate::fs::fuse::served::Served;
use crate::immutable::Cached;
use crate::immutable::cache_counted;
use crate::lore_debug;
use crate::lore_warn;
use crate::repository::RepositoryContext;

pub const FETCH_DEADLINE: Duration = Duration::from_secs(60);

/// How long after an address's fetch missed its deadline a READ of it fails at once.
pub const FAIL_FAST_WINDOW: Duration = Duration::from_secs(5);

/// The most addresses [`RecentMisses`] remembers.
#[lore_macro::test_pub]
const RECENT_MISSES_MAX: usize = 4096;

/// Addresses whose fetch missed its deadline within [`FAIL_FAST_WINDOW`]. The kernel retries a
/// failed page read once, and without this the retry waits out a second deadline.
#[lore_macro::test_pub]
#[derive(Default)]
pub struct RecentMisses {
    at: DashMap<Hash, Instant>,
}

impl RecentMisses {
    #[lore_macro::test_pub]
    fn missed_recently(&self, address: &Address) -> bool {
        let Some(at) = self.at.get(&address.hash).map(|at| *at) else {
            return false;
        };
        if at.elapsed() < FAIL_FAST_WINDOW {
            return true;
        }
        self.at
            .remove_if(&address.hash, |_, at| at.elapsed() >= FAIL_FAST_WINDOW);
        false
    }

    /// Remembers `address`, first forgetting expired misses when full; a miss that still finds
    /// it full goes unremembered.
    #[lore_macro::test_pub]
    fn record(&self, address: &Address) {
        if self.at.len() >= RECENT_MISSES_MAX {
            self.at.retain(|_, at| at.elapsed() < FAIL_FAST_WINDOW);
            if self.at.len() >= RECENT_MISSES_MAX {
                return;
            }
        }
        self.at.insert(address.hash, Instant::now());
    }
}

/// Readahead fetches the whole file, so it is bounded; larger files rely on the kernel's own
/// readahead through deferred READs.
pub const READAHEAD_WHOLE_FILE_MAX: u64 = 64 << 20;

/// Whether a file of `size` may read ahead. A file of at most one fragment is skipped: the
/// local READ that got here already proved all of it local.
pub fn readahead_wanted(size: u64) -> bool {
    size > FRAGMENT_SIZE_THRESHOLD as u64 && size <= READAHEAD_WHOLE_FILE_MAX
}

/// Records a read of `len` bytes at `offset`; true exactly once per inode, on the first read
/// that continues the previous one.
pub fn note_read(entry: &InodeEntry, offset: u64, len: usize) -> bool {
    let previous = entry
        .read_cursor
        .swap(offset.saturating_add(len as u64), Ordering::Relaxed);
    offset > 0 && offset == previous && claim_once(&entry.readahead_started)
}

/// Prefetches `address` into the local store in the background, checking every payload on receipt
/// as the command's trust of the remote asks. Needs an execution context, which the spawned task
/// inherits from the caller.
pub fn start_readahead(repository: Arc<RepositoryContext>, address: Address) {
    if LORE_CONTEXT.try_with(|_| ()).is_err() {
        return;
    }
    let _task = lore_spawn!(async move {
        let cached = cache_counted(repository, vec![address], true).await;
        if let Err(error) = cached.and_then(Cached::into_result) {
            lore_debug!("FUSE readahead of {address} stopped: {error}");
        }
    });
}

/// Reads `address`'s bytes at `offset` into `out`, clamped to `size`; a read at or past the
/// end is short, not an error. A fetch of an address in `misses` fails at once.
#[allow(clippy::too_many_arguments)]
pub async fn read_file(
    served: &Served,
    cache: &FragmentCache,
    misses: &RecentMisses,
    ino: u64,
    address: Address,
    size: u64,
    offset: u64,
    out: &mut [u8],
    deadline: Duration,
    fetch: bool,
) -> Result<usize, ReadFault> {
    let available = size.saturating_sub(offset);
    let len = usize::try_from(available).map_or(out.len(), |available| available.min(out.len()));
    if len == 0 {
        return Ok(0);
    }
    let repository = &served.repository;
    let out = &mut out[..len];

    if !fetch {
        return match cache
            .read(repository, ino, address, size, offset, out, false)
            .await
        {
            Ok(()) => Ok(len),
            Err(error) if error.is_address_not_found() || error.is_payload_not_found() => {
                Err(ReadFault::WouldFetch)
            }
            Err(error) => {
                lore_warn!("FUSE local read of {address} failed: {error}");
                Err(ReadFault::Errno(Errno::EIO))
            }
        };
    }

    if misses.missed_recently(&address) {
        lore_debug!("FUSE read of {address} failed fast after a missed deadline");
        return Err(ReadFault::Errno(Errno::EIO));
    }
    match tokio::time::timeout(
        deadline,
        cache.read(repository, ino, address, size, offset, out, true),
    )
    .await
    {
        Ok(Ok(())) => Ok(len),
        Ok(Err(error)) => {
            lore_warn!("FUSE read of {address} failed: {error}");
            Err(ReadFault::Errno(Errno::EIO))
        }
        Err(_elapsed) => {
            misses.record(&address);
            lore_warn!("FUSE read of {address} missed its {deadline:?} deadline");
            Err(ReadFault::Errno(Errno::EIO))
        }
    }
}
