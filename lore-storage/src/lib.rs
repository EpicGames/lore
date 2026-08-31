// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod chunker;
pub mod compress;
pub mod concurrency;
pub mod conformance;
pub mod defragment;
pub mod error;
pub mod errors;
pub mod fragment_engine;
pub mod fragment_flags;
pub mod fs_util;
pub mod gc_event;
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
pub mod store_types;
#[cfg(test)]
pub(crate) mod test_util;
pub(crate) mod typed_bytes;
pub(crate) mod types;
pub mod write;
pub mod write_stats;
pub mod write_tracker;

// Re-export compress types
pub use compress::COMPRESSION_MODE;
pub use compress::CompressionMode;
pub use compress::FRAGMENT_COMPRESS_SIZE_LIMIT;
pub use compress::FRAGMENT_SIZE_THRESHOLD;
pub use compress::FragmentError;
pub use compress::compress;
pub use compress::decompress;
pub use compress::decompress_into_slice;
// Re-export concurrency primitives
pub use concurrency::FILE_COUNT_LIMIT_DEFAULT;
pub use concurrency::FRAGMENT_BUDGET_KIB;
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
pub use immutable_store::ImmutableStore;
pub use immutable_store::StoreError;
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
// Re-export maintenance functions
pub use maintenance::compactor;
pub use maintenance::evictor;
pub use maintenance::gc;
pub use mutable_store::MutableStore;
// Re-export options types
pub use options::ReadOptions;
pub use options::WriteOptions;
// Re-export packstore
pub use packstore::PackStore;
pub use packstore::PackStoreRef;
pub use packstore::PackfileError;
pub use read::REMOTE_FETCH_INFLIGHT;
pub use read::decompress_and_verify;
pub use read::load_fragment;
pub use read::load_raw_local;
pub use read::read;
pub use read::read_into;
pub use read::read_into_file;
pub use read::read_raw;
pub use read::read_resolved;
pub use read::read_resolved_stream;
pub use read::read_stream;
pub use read::remote_fetch_inflight;
pub use read::write_all_to_file;
// Re-export store types
pub use store_types::KeyType;
pub use store_types::KeyValueStream;
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
pub use write::FileMatch;
pub use write::StoreResult;
pub use write::content_write_inflight;
pub use write::content_write_peak;
pub use write::file_matches;
pub use write::hash_file;
pub use write::remote_copies;
pub use write::reset_content_write_peak;
pub use write::reset_remote_copies;
pub use write::store_fragment;
pub use write::store_raw_local;
pub use write::stored_in_flight;
pub use write::write_content;
pub use write::write_from_file;
pub use write::write_raw;
pub use write::write_resolved;
pub use write_stats::FragmentWriteCounts;
pub use write_stats::FragmentWriteStats;
pub use write_tracker::WriteContext;
pub use write_tracker::WriteTracker;

/// Back-off for store operations, including every `SlowDown` retry path. Tops out at one second
/// so a caller that can recompute is not held for minutes.
pub fn store_retry() -> Retry {
    retry(
        50,
        1_000,
        *STORE_RETRY_ATTEMPTS.get_or_init(|| {
            60 //default try 60 times
        }),
    )
}

/// Store interactions use a retry policy to retry failures.
/// Server and Clients have different needs/expectations around retries
/// and this var lets each customize the behavior
pub static STORE_RETRY_ATTEMPTS: OnceLock<usize> = OnceLock::new();

/// In a server side context - assume store behaviors that make sense for this environment
pub fn assume_server_policies() {
    lore_info!("Assume server store policies");
    let _ = STORE_RETRY_ATTEMPTS
        .set(7)
        .inspect_err(|_e| lore_warn!("Could not set store retry attempts"));
}
