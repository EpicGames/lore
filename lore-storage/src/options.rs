// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use crate::compress::FRAGMENT_SIZE_THRESHOLD;
use crate::fragment_flags::FragmentFlags;

/// Options controlling how a fragment is written to storage.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct WriteOptions {
    /// This fragment stores a revision state
    pub revision_state: bool,
    /// This fragment should be cached locally with payload if possible. Recorded in the fragment's
    /// flags, so it travels with the fragment and every store that fetches it caches it too, unless
    /// the read stores nothing locally ([`ReadOptions::no_local_write`]).
    pub local_cache_priority: bool,
    /// Keep the payload in this store even where the write makes it durable on the remote,
    /// without asking anyone else to cache it: unlike `local_cache_priority`, it is not recorded in
    /// the fragment.
    pub keep_local: bool,
    /// Allow attempting to write the fragment to a remote
    pub remote_write: bool,
    /// Fixed size chunking if nonzero
    pub fixed_size_chunk: usize,
    /// Compute the address the content would be stored under, storing nothing.
    ///
    /// Every fragment is still cut and hashed, since an address is a function of the chunking as
    /// much as of the content, but none is written locally or remotely.
    ///
    /// Not for a resolved write, which exists to publish a mapping to stored content and would
    /// publish one to content nobody stored.
    pub hash_only: bool,
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

    /// See [`WriteOptions::keep_local`].
    pub fn with_keep_local(mut self) -> Self {
        self.keep_local = true;
        self
    }

    /// Whether the write keeps the payload in this store once it is durable on the remote: asked
    /// for by [`WriteOptions::local_cache_priority`] or [`WriteOptions::keep_local`].
    pub fn cache_local(&self) -> bool {
        self.local_cache_priority || self.keep_local
    }

    pub fn with_remote_write(mut self) -> Self {
        self.remote_write = true;
        self
    }

    pub fn no_remote_write(mut self) -> Self {
        self.remote_write = false;
        self
    }

    /// The size to cut fixed-size chunks at, or `None` for content-defined chunking.
    ///
    /// The clamp lives here, not only in the builder, because `fixed_size_chunk` is `pub` and a
    /// struct literal bypasses it. Every cutting path asks this, so none can omit the bound.
    ///
    /// An oversized leaf is not just different chunking: `store_fragment` refuses it, and the
    /// compare walk reads any entry above the threshold as a nested list.
    pub fn cut_size(&self) -> Option<usize> {
        (self.fixed_size_chunk > 0).then(|| self.fixed_size_chunk.clamp(1, FRAGMENT_SIZE_THRESHOLD))
    }

    pub fn with_fixed_size_chunk(mut self, size: usize) -> Self {
        self.fixed_size_chunk = std::cmp::min(FRAGMENT_SIZE_THRESHOLD, size);
        self
    }

    pub fn with_max_size_chunk(mut self) -> Self {
        self.fixed_size_chunk = FRAGMENT_SIZE_THRESHOLD;
        self
    }

    /// Address the content without storing it. See [`hash_only`](Self::hash_only).
    pub fn hash_only(mut self) -> Self {
        self.hash_only = true;
        self
    }
}

impl From<WriteOptions> for u32 {
    fn from(val: WriteOptions) -> Self {
        val.as_u32()
    }
}

/// Options controlling how a store flushes its pending writes. The default syncs nothing and
/// replaces each index file through a temporary file.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct FlushOptions {
    /// Sync what the flush writes to the storage media.
    pub sync_data: bool,
    /// Write the files of a local store's index in place rather than replacing each through a
    /// temporary file, which is faster where creating files is slow. A process that stops, or a
    /// write that fails, while a file is being written leaves it partly written, and the next load
    /// drops every entry it held. Stores without local index files ignore it.
    pub fast_flush: bool,
}

impl FlushOptions {
    /// Sync what is written, replacing each index file through a temporary file.
    pub const SYNC_DATA: Self = Self {
        sync_data: true,
        fast_flush: false,
    };
}

/// Options controlling how a fragment is read from storage.
///
/// A payload the local store holds is read as stored. One fetched from a remote is checked against
/// its address on receipt, unless the remote is trusted ([`trust_remote`](Self::trust_remote)).
#[derive(Copy, Clone, Debug)]
pub struct ReadOptions {
    /// Decompress data
    pub decompress: bool,
    /// Treat the remote as a trusted source, as a client within its infrastructure may: a fetched
    /// payload has only its sizes checked against its length, is decompressed only where
    /// `decompress` asks, and is never hashed.
    pub trust_remote: bool,
    /// Probe the local store before falling through to remote. Setting this to `false` makes
    /// reads bypass the local store entirely; combined with `remote: true` and a session, the
    /// reader always fetches from the peer. Used by handles bound to remote-only mode.
    pub local: bool,
    /// Fallback to remote
    pub remote: bool,
    /// Cache locally
    pub cache: bool,
    /// Store nothing the read fetches in the local store, whatever `cache`, a local copy that does
    /// not load, or the fragment's local cache priority asks.
    pub no_local_write: bool,
    /// Where no remote is in reach, answer a local read that fails with its error, and a local
    /// payload that fails its size check or does not decompress with an internal error, rather
    /// than with `AddressNotFound`, which stays the answer for an address the local store does not
    /// hold.
    pub strict_local: bool,
    /// Write to file directly
    pub direct_write: bool,
    /// Force sync data to storage media after write
    pub sync_data: bool,
    /// Create the directory a file read lands in, with its missing ancestors, where the write
    /// finds it missing
    pub create_parent: bool,
    /// Priority read hint for stream scheduling (metadata/tree blocks)
    pub priority: bool,
    /// A local miss answers `AddressNotFound` at once: the load never waits on another task's
    /// store or fetch of the address, and never reaches the remote itself. The local probe still
    /// takes the store's own locks.
    pub never_wait: bool,
    /// Refuse to read if the fragment's declared `size_content` exceeds this
    /// cap. `None` means "no cap" (existing behavior). Callers that know the
    /// maximum legitimate blob size for the data they are reading should set
    /// this so a corrupt or hostile root fragment cannot trigger an arbitrary
    /// defragment allocation.
    pub max_content_size: Option<u64>,
}

impl Default for ReadOptions {
    fn default() -> Self {
        ReadOptions {
            decompress: true,
            trust_remote: false,
            local: true,
            remote: true,
            cache: false,
            no_local_write: false,
            strict_local: false,
            direct_write: false,
            sync_data: false,
            create_parent: false,
            priority: false,
            never_wait: false,
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

    /// See [`ReadOptions::trust_remote`].
    pub fn trusting_remote(mut self) -> Self {
        self.trust_remote = true;
        self
    }

    pub fn no_remote(mut self) -> Self {
        self.remote = false;
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

    /// See [`ReadOptions::no_local_write`].
    pub fn with_no_local_write(mut self) -> Self {
        self.no_local_write = true;
        self
    }

    /// See [`ReadOptions::strict_local`].
    pub fn with_strict_local(mut self) -> Self {
        self.strict_local = true;
        self
    }

    /// See [`ReadOptions::create_parent`].
    pub fn creating_parent(mut self) -> Self {
        self.create_parent = true;
        self
    }

    pub fn with_priority(mut self) -> Self {
        self.priority = true;
        self
    }

    /// See [`ReadOptions::never_wait`].
    pub fn never_wait(mut self) -> Self {
        self.never_wait = true;
        self
    }

    pub fn with_max_content_size(mut self, max: u64) -> Self {
        self.max_content_size = Some(max);
        self
    }
}
