// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic;

use crate::compress::FRAGMENT_SIZE_THRESHOLD;
use crate::concurrency::LOCAL_ISOLATION;
use crate::fragment_flags::FragmentFlags;

/// Options controlling how a fragment is written to storage.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct WriteOptions {
    /// This fragment stores a revision state
    pub revision_state: bool,
    /// This fragment should be cached locally with payload if possible
    pub local_cache_priority: bool,
    /// Allow attempting to write the fragment to a remote
    pub remote_write: bool,
    /// Fixed size chunking if nonzero
    pub fixed_size_chunk: usize,
    /// Clone the memory buffer before hashing for consistent reads
    pub clone_buffer: bool,
}

impl WriteOptions {
    pub fn as_u32(&self) -> u32 {
        (if self.revision_state {
            FragmentFlags::PayloadRevisionState.bits()
        } else {
            0u32
        }) | if self.local_cache_priority {
            FragmentFlags::PayloadLocalCachePriority.bits()
        } else {
            0u32
        }
    }

    pub fn with_revision_state(mut self) -> Self {
        self.revision_state = true;
        self
    }

    pub fn with_local_cache_priority(mut self) -> Self {
        self.local_cache_priority = true;
        self
    }

    pub fn with_remote_write(mut self) -> Self {
        self.remote_write = true;
        self
    }

    pub fn no_remote_write(mut self) -> Self {
        self.remote_write = false;
        self
    }

    pub fn with_fixed_size_chunk(mut self, size: usize) -> Self {
        self.fixed_size_chunk = std::cmp::min(FRAGMENT_SIZE_THRESHOLD, size);
        self
    }

    pub fn with_max_size_chunk(mut self) -> Self {
        self.fixed_size_chunk = FRAGMENT_SIZE_THRESHOLD;
        self
    }
}

impl From<WriteOptions> for u32 {
    fn from(val: WriteOptions) -> Self {
        val.as_u32()
    }
}

/// Options controlling how a fragment is read from storage.
#[derive(Copy, Clone, Debug)]
pub struct ReadOptions {
    /// Enforce repository isolation
    pub isolate: bool,
    /// Decompress data
    pub decompress: bool,
    /// Verify data
    pub verify: bool,
    /// Probe the local store before falling through to remote. Setting this to `false` makes
    /// reads bypass the local store entirely; combined with `remote: true` and a session, the
    /// reader always fetches from the peer. Used by handles bound to remote-only mode.
    pub local: bool,
    /// Fallback to remote
    pub remote: bool,
    /// Cache locally
    pub cache: bool,
    /// Write to file directly
    pub direct_write: bool,
    /// Use file read/write instead of memory mapping
    pub direct_file_io: bool,
    /// Force sync data to storage media after write
    pub sync_data: bool,
    /// Priority read hint for stream scheduling (metadata/tree blocks)
    pub priority: bool,
    /// Refuse to read if the fragment's declared `size_content` exceeds this
    /// cap. `None` means "no cap" (existing behavior). Callers that know the
    /// maximum legitimate blob size for the data they are reading should set
    /// this so a corrupt or hostile root fragment cannot trigger an arbitrary
    /// defragment allocation.
    pub max_content_size: Option<u64>,
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            isolate: LOCAL_ISOLATION.load(atomic::Ordering::Relaxed),
            decompress: true,
            verify: true,
            local: true,
            remote: true,
            cache: false,
            direct_write: false,
            direct_file_io: false,
            sync_data: false,
            priority: false,
            max_content_size: None,
        }
    }
}

impl ReadOptions {
    pub fn with_decompress(mut self) -> Self {
        self.decompress = true;
        self
    }

    pub fn no_decompress(mut self) -> Self {
        self.decompress = false;
        self
    }

    pub fn with_verify(mut self) -> Self {
        self.verify = true;
        self
    }

    pub fn no_verify(mut self) -> Self {
        self.verify = false;
        self
    }

    pub fn with_remote(mut self) -> Self {
        self.remote = true;
        self
    }

    pub fn no_remote(mut self) -> Self {
        self.remote = false;
        self
    }

    pub fn with_local(mut self) -> Self {
        self.local = true;
        self
    }

    pub fn no_local(mut self) -> Self {
        self.local = false;
        self
    }

    pub fn with_cache(mut self) -> Self {
        self.cache = true;
        self
    }

    pub fn optional_cache(mut self, cache: bool) -> Self {
        self.cache = cache;
        self
    }

    pub fn no_cache(mut self) -> Self {
        self.cache = false;
        self
    }

    pub fn with_isolation(mut self) -> Self {
        self.isolate = true;
        self
    }

    pub fn no_isolation(mut self) -> Self {
        self.isolate = false;
        self
    }

    pub fn with_direct_write(mut self) -> Self {
        self.direct_write = true;
        self
    }

    pub fn with_direct_file_io(mut self) -> Self {
        self.direct_file_io = true;
        self
    }

    pub fn with_priority(mut self) -> Self {
        self.priority = true;
        self
    }

    pub fn with_max_content_size(mut self, max: u64) -> Self {
        self.max_content_size = Some(max);
        self
    }

    pub fn no_max_content_size(mut self) -> Self {
        self.max_content_size = None;
        self
    }
}
