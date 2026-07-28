// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod compress;
pub mod concurrency;
pub mod defragment;
pub mod error;
pub mod errors;
pub mod fragment_engine;
pub mod fragment_flags;
pub mod fs_util;
pub mod gc_event;

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
pub mod write_tracker;

use std::time::Duration;

// Re-export compress types
pub use compress::COMPRESSION_MODE;
pub use compress::CompressionMode;
pub use compress::FRAGMENT_COMPRESS_SIZE_LIMIT;
pub use compress::FRAGMENT_SIZE_THRESHOLD;
pub use compress::FragmentError;
pub use compress::compress;
pub use compress::compress_async;
pub use compress::decompress;
pub use compress::decompress_async;
pub use compress::decompress_into_slice;
// Re-export concurrency primitives
pub use concurrency::FILE_COUNT_LIMIT_DEFAULT;
pub use concurrency::FRAGMENT_BUDGET_KIB;
pub use concurrency::FRAGMENT_MINIMUM_COST_KIB;
pub use concurrency::FRAGMENT_SIZE_EXPECTED;
pub use concurrency::FRAGMENT_SIZE_MINIMUM;
pub use concurrency::LOCAL_ISOLATION;
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
pub use read::read_stream;
pub use read::remote_fetch_inflight;
pub use read::write_all_to_file;
// Re-export store types
pub use store_types::KeyType;
pub use store_types::KeyValueStream;
pub use store_types::StoreMatch;
pub use store_types::StoreObliterateStats;
pub use store_types::StoreQueryResult;
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
pub use write::StoreResult;
pub use write::hash_file;
pub use write::store_fragment;
pub use write::store_raw_local;
pub use write::stored_in_flight;
pub use write::write_content;
pub use write::write_from_file;
pub use write::write_raw;

/// Retry waiter with exponential backoff and jitter.
pub struct Retry {
    current: u64,
    maximum: u64,
    jitter: f32,
    counter: usize,
    limit: usize,
}

const DEFAULT_JITTER: f32 = 0.1;

impl Retry {
    pub async fn wait(&mut self) -> bool {
        if self.counter >= self.limit {
            return false;
        }

        // Generate some jitter to avoid alignment storms
        let jitter = rand::random::<f32>() * self.jitter;
        let jitter = std::cmp::min((jitter * self.current as f32) as u64, 100);

        tokio::time::sleep(Duration::from_millis(self.current + jitter)).await;

        self.current = std::cmp::min(self.current * 2, self.maximum);
        self.counter += 1;

        true
    }

    pub fn counter(&self) -> usize {
        self.counter
    }

    pub fn limit(&self) -> usize {
        self.limit
    }
}

/// Create a retry waiter, start and maximum times in milliseconds. Will give up
/// after trying for the limit number of times.
pub fn retry(start: u64, maximum: u64, limit: usize) -> Retry {
    Retry {
        current: start,
        maximum,
        jitter: DEFAULT_JITTER,
        counter: 0,
        limit,
    }
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
