// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use crate::compress::FRAGMENT_SIZE_THRESHOLD;
use crate::fragment_flags::FragmentFlags;

/// Options controlling how a fragment is written to storage.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct WriteOptions {
    /// This fragment stores a revision state. Recorded in the fragment's flags, so every store
    /// that fetches it keeps its payload too, unless the read stores nothing locally
    /// ([`ReadOptions::no_local_write`]).
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
    /// Ask the remote whether it holds a fragment's exact address before its payload is prepared,
    /// for every fragment the local store does not know as stored durable. Where the local store
    /// knows an association to copy from, the copy, which sends no payload either, comes first, and
    /// the remote is asked only if it refuses the copy. A fragment the remote holds is recorded as
    /// stored durable without loading, compressing or uploading its payload. A write that keeps its
    /// payload locally ([`cache_local`](Self::cache_local)) still loads or compresses the payload
    /// for the local copy, and only the upload is spared. A miss, or a query that fails, continues
    /// as without it.
    ///
    /// Takes effect only on a write given a remote session, and not under `hash_only`. Costs a
    /// round trip per fragment it asks about.
    pub remote_query: bool,
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
    /// Read only the ranges of a file the file system holds data for, and record each hole between
    /// them as a hole reference in the fragment list: one entry with the zero hash, spanning to the
    /// next entry, which stands for a range holding no data. A hole is never read, hashed or
    /// stored. Fixed-size chunks keep their place on the grid from the start of the file and are
    /// cut short where data starts or ends. Applies to content read from a file larger than one
    /// fragment; a file with no hole stores as it would without it.
    ///
    /// A hole is a range never written, or allocated and never written (an unwritten extent), as
    /// [`lore_io::IoFile::next_data`] finds it: from the extent map on Linux, so what the page
    /// cache holds does not change it, from `SEEK_DATA` on macOS, with the file's data written out
    /// first, as APFS allocates the small holes beside data it writes out, and on Windows a range
    /// a sparse file leaves unallocated, and on NTFS everything at or past the file's valid data
    /// length too. Without this option the file is read whole. Zeros written to the file are data, stored as any other content: a hole
    /// is not zeros, even though it reads as zeros. The address therefore depends on how the file
    /// is allocated as well as on what it reads as. A file with a hole and the same file with
    /// zeros written over the hole have different addresses, unless the file system itself leaves
    /// those zeros unallocated, as NTFS can for a small write of zeros alone in an allocation unit
    /// of a sparse file: then they are a hole.
    pub skip_holes: bool,
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

    /// See [`WriteOptions::remote_query`].
    pub fn with_remote_query(mut self) -> Self {
        self.remote_query = true;
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

    /// See [`WriteOptions::skip_holes`].
    pub fn with_skip_holes(mut self) -> Self {
        self.skip_holes = true;
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

/// The shortest range a file read with [`ReadOptions::resume_write`] resumes. A read of a shorter
/// range writes as one without it. Every resuming read that completes creates and removes the
/// resume record of its temporary file, which costs a read of two small leaves about a third of its
/// time, and one of this length 1.5% when reads run one at a time and 5% when they run at once,
/// both within the noise of the measurement.
pub const RESUME_WRITE_MIN_LEN: usize = 32 << 20;

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
    /// Keep the payload of what the read fetches in the local store, as a fragment with local
    /// cache priority or holding a revision state has it kept whatever this says. A fetched
    /// fragment whose payload the read does not keep is written there as an entry without its
    /// payload. Either is marked as stored on the remote. With `no_local_write` the read stores
    /// nothing, whatever this says.
    pub cache: bool,
    /// Store nothing the read fetches in the local store, neither a payload nor a fragment without
    /// one, whatever `cache`, a local copy that does not load, or the fragment asks.
    pub no_local_write: bool,
    /// Where no remote is in reach, answer a local read that fails with its error, and a local
    /// payload that fails its size check or does not decompress with an internal error, rather
    /// than with `AddressNotFound`, which stays the answer for an address the local store does not
    /// hold.
    pub strict_local: bool,
    /// Write to file directly
    pub direct_write: bool,
    /// Make the file a read of a fragment list writes sparse before sizing it
    /// ([`lore_io::IoFile::set_len_sparse`]), so a hole of the content holds no storage on a file
    /// system that holds none only for a file marked sparse, as those of Windows do. On Windows a
    /// file it writes over is deallocated whole first ([`lore_io::IoFile::empty`]). A resuming
    /// read ([`resume_write`](Self::resume_write)) writes its temporary file sparse whatever this
    /// says.
    pub sparse_write: bool,
    /// Resume a file read from what its file already holds. A read through a temporary file of a
    /// range of at least [`RESUME_WRITE_MIN_LEN`] keeps it when the read fails or stops, and resumes
    /// from a temporary file that a read of the same content and range left at the same path; any
    /// other temporary file is emptied. A read of a shorter range, or of a single fragment, writes
    /// as one without this, and so does one whose file system refuses the temporary file's lock.
    /// With `direct_write` a read writes no temporary file. A defragment into a file
    /// ([`DefragmentSink::File`]) with this aborts its fetch pool and write sink when it is
    /// dropped, so that nothing is written to the file past it.
    ///
    /// [`DefragmentSink::File`]: crate::defragment::DefragmentSink::File
    pub resume_write: bool,
    /// With `resume_write`, the file a defragment writes ([`DefragmentSink::File`]) already holds
    /// part of the content: a whole leaf it holds, checked against its hash, is taken rather than
    /// fetched. A file read sets it where its temporary file holds part of the content.
    ///
    /// [`DefragmentSink::File`]: crate::defragment::DefragmentSink::File
    pub resume_held: bool,
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
            sparse_write: false,
            resume_write: false,
            resume_held: false,
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

    /// See [`ReadOptions::sparse_write`].
    pub fn sparse_writing(mut self) -> Self {
        self.sparse_write = true;
        self
    }

    /// See [`ReadOptions::resume_write`].
    pub fn resuming_write(mut self) -> Self {
        self.resume_write = true;
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
