// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod caller_buffer;
pub mod chunker;
pub mod compress;
pub mod concurrency;
pub mod conformance;
pub mod content;
pub mod defragment;
pub mod error;
pub mod errors;
pub mod fragment_engine;
pub mod fragment_flags;
pub mod fs_util;
pub mod gc_event;
#[cfg(feature = "test-util")]
pub mod mutable_conformance;

use std::sync::OnceLock;

pub use lore_base::allocator::GrowVec;
pub use lore_base::allocator::GrowVecMemoryStats;
pub mod hash;
pub mod immutable_store;
pub mod local;
pub mod maintenance;
pub mod mutable_store;
pub mod options;
pub mod packstore;
pub mod read;
#[cfg(not(feature = "test-util"))]
mod read_flight;
#[cfg(feature = "test-util")]
pub mod read_flight;
pub(crate) mod resume;
pub mod store_types;
#[cfg(feature = "test-util")]
pub mod test_util;
pub(crate) mod typed_bytes;
pub(crate) mod types;
pub mod write;
pub mod write_stats;
pub mod write_tracker;

pub use caller_buffer::CallerBuffer;
pub use caller_buffer::Lender;
pub use caller_buffer::Loan;
// Re-export compress types
pub use compress::COMPRESSION_MODE;
pub use compress::CompressionMode;
pub use compress::FRAGMENT_COMPRESS_SIZE_LIMIT;
pub use compress::FRAGMENT_SIZE_THRESHOLD;
pub use compress::FragmentError;
pub use compress::compress;
pub use compress::decompress;
pub use compress::decompress_into_slice;
pub use compress::set_compression_level;
pub use compress::suggest_compression_mode;
pub use compress::writable_compression_mode;
// Re-export concurrency primitives
pub use concurrency::FILE_COUNT_LIMIT_DEFAULT;
pub use concurrency::FRAGMENT_MINIMUM_COST_KIB;
pub use concurrency::FRAGMENT_SIZE_EXPECTED;
pub use concurrency::FRAGMENT_SIZE_MINIMUM;
pub use concurrency::SemaphoreError;
pub use concurrency::compress_limit_acquire;
pub use concurrency::configure;
pub use concurrency::configure_compress_limiter;
pub use concurrency::file_count_limit_acquire;
pub use concurrency::file_count_limiter;
pub use concurrency::fragment_limiter;
pub use concurrency::fragment_permit_count;
pub use content::ContentHandle;
pub use content::ContentSource;
pub use content::WindowRead;
// Re-export new read/write/defragment types
pub use defragment::DefragmentSink;
pub use error::StorageError;
// Re-export error types
pub use errors::AddressNotFound;
pub use errors::Disconnected;
pub use errors::NotConnected;
pub use errors::Oversized;
pub use errors::PayloadNotFound;
pub use errors::SlowDown;
pub use fragment_engine::write_fragmented;
pub use fragment_engine::write_fragmentlist;
// Re-export fragment and hash utilities
pub use fragment_flags::FragmentFlags;
pub use hash::StringHash;
pub use hash::hash_fragment;
pub use hash::hash_function;
pub use hash::hash_function_arg;
pub use hash::hash_function_arg_slice;
pub use hash::hash_function_args;
pub use hash::hash_function_args_slice;
pub use hash::hash_function_strs_slice;
pub use hash::hash_slice;
pub use hash::hash_string;
pub use hash::hash_string_bytes;
// Re-export store traits
pub use immutable_store::CorrelationHold;
pub use immutable_store::ImmutableStore;
pub use immutable_store::StoreError;
pub(crate) use immutable_store::payload_is_content;
pub(crate) use immutable_store::validate_buffer_capacity;
pub use immutable_store::validate_fragment_list;
pub use immutable_store::validate_fragment_metadata;
pub use immutable_store::validate_fragment_payload;
pub use immutable_store::validate_fragment_size;
// Re-export local store implementations
pub use local::immutable_store::ImmutableStoreSettings;
pub use local::immutable_store::LocalImmutableStore;
pub use local::immutable_store::LocalImmutableStoreError;
pub use local::mutable_store::LocalMutableStore;
pub use local::mutable_store::LocalMutableStoreError;
pub use local::mutable_store::MutableStoreSettings;
use lore_base::lore_info;
use lore_base::lore_warn;
pub use lore_base::retry::DEFAULT_JITTER;
pub use lore_base::retry::Retry;
pub use lore_base::retry::retry;
pub use lore_base::retry::retry_with_jitter;
pub use lore_io::DataCursor;
// Re-export maintenance functions
pub use maintenance::compactor;
pub use maintenance::evictor;
pub use maintenance::gc;
pub use mutable_store::MutableStore;
pub use mutable_store::flush_stores;
// Re-export options types
pub use options::FlushOptions;
pub use options::ReadOptions;
pub use options::WriteOptions;
// Re-export packstore
pub use packstore::PackStore;
pub use packstore::PackStoreRef;
pub use packstore::PackfileError;
pub use packstore::PackfileSize;
pub use read::REMOTE_FETCH_INFLIGHT;
pub use read::check_payload_sizes;
pub use read::expand_payload;
pub use read::load_fragment;
pub use read::load_root;
pub use read::read;
pub use read::read_into;
pub use read::read_into_buffer;
pub use read::read_into_file;
pub use read::read_raw;
pub use read::read_resolved;
pub use read::read_resolved_into_buffer;
pub use read::read_resolved_into_file;
pub use read::read_resolved_stream;
pub use read::read_stream;
pub use read::remote_fetch_bytes;
pub use read::remote_fetch_inflight;
pub use read::stream_root;
pub use read::write_all_to_file;
// Re-export store types
pub use store_types::KeyType;
pub use store_types::KeyValueStream;
pub use store_types::PayloadRead;
pub use store_types::StoreGetData;
pub use store_types::StoreMatch;
pub use store_types::StoreMatchResult;
pub use store_types::StoreObliterateStats;
pub use typed_bytes::TypedBytes;
pub use typed_bytes::TypedBytesMut;
pub use types::Address;
pub use types::CloneHeapAlloc;
pub use types::Context;
pub use types::Fragment;
pub use types::FragmentReference;
pub use types::HASH_STRING_LENGTH;
pub use types::Hash;
pub use types::Partition;
pub use types::VecBytes;
pub use types::ZeroHeapAlloc;
/// Serde field-level helpers for hex encoding. Use with `#[serde(serialize_with = "...")]`.
pub use types::deserialize_context;
/// Serde field-level helpers for hex encoding. Use with `#[serde(deserialize_with = "...")]`.
pub use types::deserialize_hash;
/// Serde field-level helpers for hex encoding. Use with `#[serde(serialize_with = "...")]`.
pub use types::serialize_hex;
pub use write::ContentHashes;
pub use write::CopyOutcome;
pub use write::FileMatch;
pub use write::FusedPublish;
pub use write::StoreResult;
pub use write::content_write_inflight;
pub use write::content_write_peak;
pub use write::file_matches;
pub use write::hash_file;
pub use write::remote_copies;
pub use write::remote_copy_retry;
pub use write::reset_content_write_peak;
pub use write::reset_remote_copies;
pub use write::store_fragment;
pub use write::store_raw_local;
pub use write::stored_in_flight;
pub use write::write_content;
pub use write::write_content_borrowed;
pub use write::write_from_file;
pub use write::write_raw;
pub use write::write_resolved;
pub use write::write_resolved_from_file;
pub use write_stats::FragmentWriteCounts;
pub use write_stats::FragmentWriteStats;
pub use write_tracker::WriteContext;
pub use write_tracker::WriteTracker;

/// First wait of the store ladder, in milliseconds.
#[lore_macro::test_pub]
const STORE_RETRY_START_MS: u64 = 50;
/// Ceiling on any single wait of the store ladder, in milliseconds.
#[lore_macro::test_pub]
const STORE_RETRY_MAX_MS: u64 = 1_000;
/// Waits an operation gets where nothing has narrowed it.
#[lore_macro::test_pub]
const STORE_RETRY_ATTEMPTS_DEFAULT: usize = 60;
/// Waits an operation gets inside a request handler.
#[lore_macro::test_pub]
const STORE_RETRY_ATTEMPTS_SERVER: usize = 9;

/// Back-off for store operations, including every `SlowDown` retry path.
///
/// Waits start at 50ms and double up to a ceiling of one second, for as many waits as
/// [`STORE_RETRY_ATTEMPTS`] holds: sixty, about a minute in total, unless the process set another
/// count first, as [`assume_server_policies`] does.
///
/// The jitter clamp is raised to the ceiling, so each wait may deviate by half its interval either
/// way. Callers shed at the same instant hold the same back-off, and the spread keeps their retries
/// apart.
pub fn store_retry() -> Retry {
    retry(
        STORE_RETRY_START_MS,
        STORE_RETRY_MAX_MS,
        *STORE_RETRY_ATTEMPTS.get_or_init(|| STORE_RETRY_ATTEMPTS_DEFAULT),
    )
    .with_jitter_clamp(STORE_RETRY_MAX_MS)
}

/// Store interactions use a retry policy to retry failures.
/// Server and Clients have different needs/expectations around retries
/// and this var lets each customize the behavior
pub static STORE_RETRY_ATTEMPTS: OnceLock<usize> = OnceLock::new();

/// Sets the store ladder to nine waits, for a process that answers requests.
///
/// The nine waits total about five and a half seconds, not counting the time each attempt itself
/// takes, so an operation fails inside a request handler's budget rather than holding the handler
/// for the default minute. Has no effect once [`store_retry`] has read the attempt count or an
/// earlier call has set it.
pub fn assume_server_policies() {
    lore_info!("Assume server store policies");
    let _ = STORE_RETRY_ATTEMPTS
        .set(STORE_RETRY_ATTEMPTS_SERVER)
        .inspect_err(|_e| lore_warn!("Could not set store retry attempts"));
}
