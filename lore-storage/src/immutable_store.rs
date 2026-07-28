// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::any::Any;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use lore_error_set::prelude::*;

use crate::Address;
use crate::Context;
use crate::Fragment;
use crate::FragmentFlags;
use crate::FragmentReference;
use crate::Partition;
use crate::TypedBytes;
use crate::errors::AddressNotFound;
use crate::errors::Disconnected;
use crate::errors::Maintenance;
use crate::errors::NoRemote;
use crate::errors::NotAuthenticated;
use crate::errors::NotAuthorized;
use crate::errors::NotFound;
use crate::errors::NotSupported;
use crate::errors::Oversized;
use crate::errors::PayloadNotFound;
use crate::errors::SlowDown;
use crate::store_types::StoreMatch;
use crate::store_types::StoreObliterateStats;
use crate::store_types::StoreQueryResult;

#[error_set(clone)]
pub enum StoreError {
    AddressNotFound,
    PayloadNotFound,
    SlowDown,
    Oversized,
    NotFound,
    Disconnected,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NoRemote,
    NotSupported,
}

/// Validate that a fragment's declared payload size does not exceed the
/// protocol-level [`FRAGMENT_SIZE_THRESHOLD`]. Use before allocating or
/// streaming a payload buffer based on attacker-influenced metadata.
///
/// [`FRAGMENT_SIZE_THRESHOLD`]: crate::FRAGMENT_SIZE_THRESHOLD
pub fn validate_fragment_size(fragment: &Fragment) -> Result<(), StoreError> {
    let size_payload = fragment.size_payload as usize;
    if size_payload > crate::FRAGMENT_SIZE_THRESHOLD {
        return Err(StoreError::from(Oversized {
            context: format!(
                "fragment size_payload {size_payload} exceeds FRAGMENT_SIZE_THRESHOLD {}",
                crate::FRAGMENT_SIZE_THRESHOLD
            ),
        }));
    }
    Ok(())
}

/// Validate that a single fragment's declared payload size matches the buffer
/// length and does not exceed the protocol-level [`FRAGMENT_SIZE_THRESHOLD`].
///
/// Every [`ImmutableStore`] implementation should call this at both the get
/// (post-load) and put (on entry) boundaries so corrupt or hostile payload
/// sizes fail fast with an explicit [`StoreError::Oversized`] rather than
/// silently triggering large allocations downstream.
///
/// [`FRAGMENT_SIZE_THRESHOLD`]: crate::FRAGMENT_SIZE_THRESHOLD
pub fn validate_fragment_payload(
    fragment: &Fragment,
    payload_len: usize,
) -> Result<(), StoreError> {
    validate_fragment_size(fragment)?;
    let size_payload = fragment.size_payload as usize;
    if payload_len != size_payload {
        return Err(StoreError::internal(format!(
            "fragment payload length mismatch: buffer {payload_len} vs size_payload {size_payload}"
        )));
    }
    Ok(())
}

/// Flag bits that are managed by the server and must not be supplied by a
/// client on ingress. Other bits (`PayloadStored*`, `PayloadLocalCachePriority`,
/// `PayloadRevisionState`) are legitimately set by clients or replication
/// peers and must persist through the storage system.
const FRAGMENT_FLAGS_SERVER_MANAGED_INGRESS_REJECTED: u32 =
    FragmentFlags::PayloadObliteration.bits() | FragmentFlags::PayloadDoNotReplicate.bits();

/// Compression bits that are defined and meaningful. Any compression bit
/// outside this mask is reserved and must be rejected.
const FRAGMENT_FLAGS_DEFINED_COMPRESSORS: u32 = FragmentFlags::PayloadCompressedLZ4.bits()
    | FragmentFlags::PayloadCompressedOodle2.bits()
    | FragmentFlags::PayloadCompressedZstd.bits();

/// Validate fragment metadata at the protocol ingress boundary. Performs all
/// checks that don't require payload bytes:
///
/// - `size_payload` is in `(0, FRAGMENT_SIZE_THRESHOLD]`
/// - `size_payload <= size_content` (universal invariant)
/// - No unknown or reserved flag bits
/// - At most one compression flag is set, and only from the defined set
/// - No server-managed flags (`PayloadObliteration*`)
/// - Compressed and fragmented flags are mutually exclusive
/// - Compressed fragments have `size_content <= FRAGMENT_SIZE_THRESHOLD`
/// - Uncompressed, unfragmented fragments have `size_payload == size_content`
///
/// All three Put protocols (QUIC, gRPC storage v1, legacy gRPC storage) and
/// the replication-store Put call this at ingress so malformed metadata fails
/// fast with a specific error rather than propagating into the store layer.
pub fn validate_fragment_metadata(fragment: &Fragment) -> Result<(), StoreError> {
    validate_fragment_size(fragment)?;

    if fragment.flags & !FragmentFlags::all().bits() != 0 {
        return Err(StoreError::internal(format!(
            "fragment flags contain unknown bits: {:#x}",
            fragment.flags & !FragmentFlags::all().bits()
        )));
    }

    if fragment.flags & FRAGMENT_FLAGS_SERVER_MANAGED_INGRESS_REJECTED != 0 {
        return Err(StoreError::internal(format!(
            "fragment flags contain server-managed bits not permitted on ingress: {:#x}",
            fragment.flags & FRAGMENT_FLAGS_SERVER_MANAGED_INGRESS_REJECTED
        )));
    }

    let compressed_bits = fragment.flags & FragmentFlags::PayloadCompressed.bits();
    if compressed_bits.count_ones() > 1 {
        return Err(StoreError::internal(format!(
            "fragment has multiple compression flags set: {compressed_bits:#x}"
        )));
    }
    if compressed_bits & !FRAGMENT_FLAGS_DEFINED_COMPRESSORS != 0 {
        return Err(StoreError::internal(format!(
            "fragment has reserved compression bit set: {:#x}",
            compressed_bits & !FRAGMENT_FLAGS_DEFINED_COMPRESSORS
        )));
    }

    let is_compressed = compressed_bits != 0;
    let is_fragmented = fragment.flags & FragmentFlags::PayloadFragmented.bits() != 0;

    if is_compressed && is_fragmented {
        return Err(StoreError::internal(
            "fragment flags cannot combine compressed and fragmented".to_string(),
        ));
    }

    if fragment.size_payload == 0 {
        return Err(StoreError::internal(
            "fragment size_payload must be > 0".to_string(),
        ));
    }
    if fragment.size_payload as u64 > fragment.size_content {
        return Err(StoreError::internal(format!(
            "fragment size_payload {} exceeds size_content {}",
            fragment.size_payload, fragment.size_content
        )));
    }

    if is_compressed && fragment.size_content as usize > crate::FRAGMENT_SIZE_THRESHOLD {
        return Err(StoreError::from(Oversized {
            context: format!(
                "compressed fragment size_content {} exceeds FRAGMENT_SIZE_THRESHOLD {}",
                fragment.size_content,
                crate::FRAGMENT_SIZE_THRESHOLD
            ),
        }));
    }

    if !is_compressed && !is_fragmented && fragment.size_payload as u64 != fragment.size_content {
        return Err(StoreError::internal(format!(
            "uncompressed unfragmented fragment has mismatching size_payload {} and size_content {}",
            fragment.size_payload, fragment.size_content
        )));
    }

    Ok(())
}

/// Validate a fragmented fragment's payload bytes as a well-formed list of
/// [`FragmentReference`]s. Caller must have verified that
/// [`FragmentFlags::PayloadFragmented`] is set and that
/// [`validate_fragment_metadata`] has already passed.
///
/// Checks:
/// - `size_payload` is a non-zero multiple of `size_of::<FragmentReference>()`
/// - Payload buffer length matches `size_payload`
/// - At least two references are present
/// - `offset_content` is strictly increasing
/// - `first_offset + size_content` does not overflow u64
/// - `last_offset` is strictly inside the content window
///   `[first_offset, first_offset + size_content)`
pub fn validate_fragment_list(
    fragment: &Fragment,
    payload: &bytes::Bytes,
) -> Result<(), StoreError> {
    debug_assert!(fragment.flags & FragmentFlags::PayloadFragmented.bits() != 0);

    let ref_size = std::mem::size_of::<FragmentReference>();

    if !(fragment.size_payload as usize).is_multiple_of(ref_size) {
        return Err(StoreError::internal(format!(
            "fragmented fragment size_payload {} is not a multiple of FragmentReference size {ref_size}",
            fragment.size_payload
        )));
    }

    if payload.len() != fragment.size_payload as usize {
        return Err(StoreError::internal(format!(
            "fragmented fragment payload length {} does not match size_payload {}",
            payload.len(),
            fragment.size_payload
        )));
    }

    let aligned = payload.clone().to_aligned::<FragmentReference>();
    let references = aligned.as_type_slice::<FragmentReference>();

    if references.len() < 2 {
        return Err(StoreError::internal(format!(
            "fragmented fragment has fewer than 2 references ({})",
            references.len()
        )));
    }

    for i in 1..references.len() {
        if references[i].offset_content <= references[i - 1].offset_content {
            return Err(StoreError::internal(format!(
                "fragmented fragment offsets are not strictly increasing at index {i}"
            )));
        }
    }

    let first = references[0].offset_content;
    let last = references[references.len() - 1].offset_content;

    let content_end = first.checked_add(fragment.size_content).ok_or_else(|| {
        StoreError::internal(format!(
            "fragmented fragment first offset {first} + size_content {} overflows u64",
            fragment.size_content
        ))
    })?;

    if last >= content_end {
        return Err(StoreError::internal(format!(
            "fragmented fragment last offset {last} is outside content window [{first}, {content_end})"
        )));
    }

    Ok(())
}

pub struct BehaviorFlags {
    pub do_not_replicate: bool,
}

/// Takes the fragment and removes behavioral flags that aren't meant for durable storage,
/// but merely exist to dictate behaviour when interacting with a Store at a point in time
pub fn sanitise_fragment_behavior_flags(fragment: &mut Fragment) -> BehaviorFlags {
    let do_not_replicate = if fragment.flags & FragmentFlags::PayloadDoNotReplicate
        == FragmentFlags::PayloadDoNotReplicate
    {
        fragment.flags &= !FragmentFlags::PayloadDoNotReplicate;
        true
    } else {
        false
    };

    BehaviorFlags { do_not_replicate }
}

#[async_trait]
pub trait ImmutableStore: Any + Send + Sync {
    /// Check if this store is backed by local disk
    fn is_local(&self) -> bool {
        false
    }

    /// Check if this store is available for service
    async fn is_available(self: Arc<Self>, _timeout: Duration) -> bool {
        true
    }

    /// Check the immutable store for existence of the given address within the partition.
    /// Match request can be used to early out during address-partition-context triplet
    /// matching if a full match is not required. Returns the match made.
    async fn exist(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreMatch, StoreError>;

    /// Check for the existence of a batch of addresses. Behavior is identical to `exist`.
    /// The order of results in the returned `Vec` matches the order of addresses provided as input.
    async fn exist_batch(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        match_requested: StoreMatch,
    ) -> Result<Vec<StoreMatch>, StoreError>;

    /// Query the immutable metadata for the given address within the partition.
    /// Match request can be used to early out during address-partition-context triplet
    /// matching if a full match is not required.
    async fn query(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreQueryResult, StoreError>;

    /// Get the immutable data for the given address within the partition.
    /// Match requirement controls cross-partition and cross-context load behavior.
    /// Returns `StoreError::AddressNotFound` if no match is made.
    /// Returns `StoreError::PayloadNotFound` if a match is made but no payload is stored locally.
    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_required: StoreMatch,
    ) -> Result<(Fragment, Bytes), StoreError>;

    /// Put the immutable data for the given address within the partition.
    /// If the payload buffer is not given and the store has no previous instance of the data,
    /// the function will return an error and the caller should try again after obtaining the payload.
    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError>;

    /// Obliterate the immutable data for the given address in the given partition.
    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError>;

    /// Evict fragments from the store until the given max capacity is reached.
    /// When `sync_data` is true, data is synced to the storage media (fsync).
    /// `sink`, when present, receives eviction lifecycle and per-bucket progress.
    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<crate::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError>;

    /// Compact storage and remove unreferenced payloads. Returns an optional non-zero
    /// resume point to denote it has completed a step.
    /// When `sync_data` is true, data is synced to the storage media (fsync).
    /// `sink`, when present, receives compaction lifecycle and per-group progress.
    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<crate::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError>;

    /// Return the current resume point for compaction
    async fn compact_resume_at(self: Arc<Self>) -> Option<usize>;

    /// Stop any ongoing compaction gracefully
    async fn compact_stop(self: Arc<Self>);

    /// Get maximum supported query batch size, if any
    fn max_query_batch(&self) -> Option<usize>;

    /// Flush any pending writes to durable storage.
    /// When `sync_data` is true, data is synced to the storage media (fsync).
    async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), StoreError>;

    /// Get number of fragments in store, if available
    async fn fragment_count(self: Arc<Self>) -> Option<usize> {
        None
    }

    /// Verify the integrity of the store. If `heal` is true, attempt to repair any issues found.
    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError>;

    /// Copy a fragment from one `(partition, address)` tuple to another within the same store.
    ///
    /// The destination tuple is `(destination_partition, source_address.hash, destination_context)` —
    /// the hash is preserved (content-addressed) but partition and context can both differ from the
    /// source, enabling within-partition deduplication when only the dedup tag changes.
    ///
    /// `durable` controls the destination's `PayloadStoredDurable` flag: pass `true` only when
    /// the caller has independent confirmation that the destination tuple is durably stored
    /// (typically a successful remote round-trip). The source's own durable flag never
    /// propagates — durability is a per-(partition, address) property and a local copy of an
    /// already-durable source does not make the new destination tuple durable.
    async fn copy(
        self: Arc<Self>,
        _source_partition: Partition,
        _source_address: Address,
        _destination_partition: Partition,
        _destination_context: Context,
        _durable: bool,
    ) -> Result<(), StoreError> {
        Err(StoreError::internal("Copy not supported by this store"))
    }

    fn as_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>
    where
        Self: Sized,
    {
        self
    }
}

impl Debug for dyn ImmutableStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ImmutableStore")
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::Hash;

    fn make_fragment(size_payload: u32) -> Fragment {
        Fragment {
            flags: 0,
            size_payload,
            size_content: size_payload as u64,
        }
    }

    #[test]
    fn validate_size_accepts_zero() {
        assert!(validate_fragment_size(&make_fragment(0)).is_ok());
    }

    #[test]
    fn validate_size_accepts_exact_threshold() {
        let fragment = make_fragment(crate::FRAGMENT_SIZE_THRESHOLD as u32);
        assert!(validate_fragment_size(&fragment).is_ok());
    }

    #[test]
    fn validate_size_rejects_over_threshold() {
        let fragment = make_fragment(crate::FRAGMENT_SIZE_THRESHOLD as u32 + 1);
        let err = validate_fragment_size(&fragment).expect_err("should reject oversize");
        assert!(matches!(err, StoreError::Oversized(_)));
    }

    #[test]
    fn validate_payload_accepts_matching() {
        let fragment = make_fragment(128);
        assert!(validate_fragment_payload(&fragment, 128).is_ok());
    }

    #[test]
    fn validate_payload_rejects_length_mismatch() {
        let fragment = make_fragment(128);
        let err = validate_fragment_payload(&fragment, 127).expect_err("should reject mismatch");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn validate_payload_rejects_oversize_before_mismatch() {
        // Oversize must be reported even when the buffer length also doesn't match,
        // because the oversize check happens first.
        let fragment = make_fragment(crate::FRAGMENT_SIZE_THRESHOLD as u32 + 1);
        let err = validate_fragment_payload(&fragment, 0).expect_err("should reject oversize");
        assert!(matches!(err, StoreError::Oversized(_)));
    }

    mod validate_metadata {
        use super::*;

        #[test]
        fn accepts_uncompressed_unfragmented() {
            assert!(validate_fragment_metadata(&make_fragment(128)).is_ok());
        }

        #[test]
        fn rejects_zero_size_payload() {
            let err = validate_fragment_metadata(&make_fragment(0)).expect_err("size_payload=0");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn rejects_oversize_payload() {
            let fragment = make_fragment(crate::FRAGMENT_SIZE_THRESHOLD as u32 + 1);
            let err = validate_fragment_metadata(&fragment).expect_err("oversize");
            assert!(matches!(err, StoreError::Oversized(_)));
        }

        #[test]
        fn rejects_size_payload_greater_than_size_content() {
            let fragment = Fragment {
                flags: 0,
                size_payload: 100,
                size_content: 50,
            };
            let err = validate_fragment_metadata(&fragment).expect_err("payload > content");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn rejects_unknown_flag_bits() {
            let fragment = Fragment {
                flags: 1 << 31,
                size_payload: 128,
                size_content: 128,
            };
            let err = validate_fragment_metadata(&fragment).expect_err("unknown flags");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn rejects_multiple_compression_flags() {
            let fragment = Fragment {
                flags: (FragmentFlags::PayloadCompressedLZ4 | FragmentFlags::PayloadCompressedZstd)
                    .into(),
                size_payload: 100,
                size_content: 200,
            };
            let err = validate_fragment_metadata(&fragment).expect_err("multi compression");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn rejects_reserved_compression_bit() {
            // bit 4 is inside PayloadCompressed mask but not a defined compressor
            let fragment = Fragment {
                flags: 1 << 4,
                size_payload: 100,
                size_content: 200,
            };
            let err = validate_fragment_metadata(&fragment).expect_err("reserved compression bit");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn rejects_obliterated_flag_on_ingress() {
            let fragment = Fragment {
                flags: FragmentFlags::PayloadObliterated.into(),
                size_payload: 128,
                size_content: 128,
            };
            let err = validate_fragment_metadata(&fragment).expect_err("obliterated");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn rejects_do_not_replicate_flag_on_ingress() {
            let fragment = Fragment {
                flags: FragmentFlags::PayloadDoNotReplicate.into(),
                size_payload: 128,
                size_content: 128,
            };
            let err = validate_fragment_metadata(&fragment).expect_err("do_not_replicate");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn accepts_local_cache_priority_on_ingress() {
            // PayloadLocalCachePriority is a client-set write hint that must
            // persist through the storage system; it must not be rejected at
            // validation.
            let fragment = Fragment {
                flags: FragmentFlags::PayloadLocalCachePriority.into(),
                size_payload: 128,
                size_content: 128,
            };
            assert!(validate_fragment_metadata(&fragment).is_ok());
        }

        #[test]
        fn accepts_payload_stored_flags() {
            // PayloadStored* is set by peers during replication and cleared by the
            // Put handler; it must not be rejected at validation.
            let fragment = Fragment {
                flags: FragmentFlags::PayloadStoredDurable.into(),
                size_payload: 128,
                size_content: 128,
            };
            assert!(validate_fragment_metadata(&fragment).is_ok());
        }

        #[test]
        fn rejects_compressed_and_fragmented_combo() {
            let fragment = Fragment {
                flags: (FragmentFlags::PayloadCompressedLZ4 | FragmentFlags::PayloadFragmented)
                    .into(),
                size_payload: 80,
                size_content: 200,
            };
            let err = validate_fragment_metadata(&fragment).expect_err("compressed+fragmented");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn rejects_uncompressed_unfragmented_size_mismatch() {
            let fragment = Fragment {
                flags: 0,
                size_payload: 100,
                size_content: 200,
            };
            let err = validate_fragment_metadata(&fragment).expect_err("size mismatch");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn accepts_compressed_with_shrinking_content() {
            let fragment = Fragment {
                flags: FragmentFlags::PayloadCompressedLZ4.into(),
                size_payload: 100,
                size_content: 200,
            };
            assert!(validate_fragment_metadata(&fragment).is_ok());
        }

        #[test]
        fn rejects_compressed_with_oversize_content() {
            let fragment = Fragment {
                flags: FragmentFlags::PayloadCompressedLZ4.into(),
                size_payload: 100,
                size_content: crate::FRAGMENT_SIZE_THRESHOLD as u64 + 1,
            };
            let err = validate_fragment_metadata(&fragment).expect_err("oversize content");
            assert!(matches!(err, StoreError::Oversized(_)));
        }

        #[test]
        fn accepts_fragmented_with_large_content() {
            // Fragmented fragments can address any amount of content
            let fragment = Fragment {
                flags: FragmentFlags::PayloadFragmented.into(),
                size_payload: 80,
                size_content: 10 * 1024 * 1024 * 1024, // 10 GiB
            };
            assert!(validate_fragment_metadata(&fragment).is_ok());
        }
    }

    mod sanitise_behavior_flags {
        use super::*;

        mod do_not_replicate {
            use super::*;

            #[test]
            fn strips_and_returns_true() {
                let mut fragment = make_fragment(128);
                fragment.flags |= FragmentFlags::PayloadDoNotReplicate;

                let behaviour = sanitise_fragment_behavior_flags(&mut fragment);

                assert!(behaviour.do_not_replicate);
                assert_eq!(fragment.flags & FragmentFlags::PayloadDoNotReplicate, 0);
            }

            #[test]
            fn returns_false_when_flag_absent() {
                let mut fragment = make_fragment(128);

                let behaviour = sanitise_fragment_behavior_flags(&mut fragment);

                assert!(!behaviour.do_not_replicate);
                assert_eq!(fragment.flags, 0);
            }
        }

        #[test]
        fn preserves_other_flags() {
            let mut fragment = make_fragment(128);
            fragment.flags |=
                FragmentFlags::PayloadStoredDurable | FragmentFlags::PayloadDoNotReplicate;

            let behaviour = sanitise_fragment_behavior_flags(&mut fragment);

            assert!(behaviour.do_not_replicate);
            assert_ne!(fragment.flags & FragmentFlags::PayloadStoredDurable, 0);
            assert_eq!(fragment.flags & FragmentFlags::PayloadDoNotReplicate, 0);
        }
    }

    mod validate_list {
        use super::*;

        fn make_refs_payload(refs: &[FragmentReference]) -> Bytes {
            Bytes::copy_from_slice(refs.as_bytes())
        }

        fn make_fragmented(refs_len: usize, size_content: u64) -> Fragment {
            Fragment {
                flags: FragmentFlags::PayloadFragmented.into(),
                size_payload: (refs_len * std::mem::size_of::<FragmentReference>()) as u32,
                size_content,
            }
        }

        #[test]
        fn accepts_well_formed_list() {
            let refs = [
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 0,
                },
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 1000,
                },
            ];
            let fragment = make_fragmented(refs.len(), 2000);
            let payload = make_refs_payload(&refs);
            assert!(validate_fragment_list(&fragment, &payload).is_ok());
        }

        #[test]
        fn rejects_non_multiple_of_ref_size() {
            let fragment = Fragment {
                flags: FragmentFlags::PayloadFragmented.into(),
                size_payload: 41,
                size_content: 1000,
            };
            let payload = Bytes::from(vec![0u8; 41]);
            let err = validate_fragment_list(&fragment, &payload).expect_err("non-multiple");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn rejects_payload_length_mismatch() {
            let refs = [
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 0,
                },
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 500,
                },
            ];
            let fragment = make_fragmented(refs.len(), 1000);
            // Report payload bytes but declare more payload via size_payload
            let short_payload = Bytes::from(vec![0u8; fragment.size_payload as usize - 40]);
            let err = validate_fragment_list(&fragment, &short_payload)
                .expect_err("payload len mismatch");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn rejects_fewer_than_two_refs() {
            let refs = [FragmentReference {
                hash: Hash::default(),
                offset_content: 0,
            }];
            let fragment = make_fragmented(refs.len(), 1000);
            let payload = make_refs_payload(&refs);
            let err = validate_fragment_list(&fragment, &payload).expect_err("<2 refs");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn rejects_non_increasing_offsets() {
            let refs = [
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 0,
                },
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 1000,
                },
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 500,
                },
            ];
            let fragment = make_fragmented(refs.len(), 2000);
            let payload = make_refs_payload(&refs);
            let err = validate_fragment_list(&fragment, &payload).expect_err("non-increasing");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn rejects_equal_offsets() {
            let refs = [
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 0,
                },
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 500,
                },
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 500,
                },
            ];
            let fragment = make_fragmented(refs.len(), 2000);
            let payload = make_refs_payload(&refs);
            let err = validate_fragment_list(&fragment, &payload).expect_err("equal offsets");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn rejects_last_offset_at_content_end() {
            let refs = [
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 0,
                },
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 2000,
                },
            ];
            let fragment = make_fragmented(refs.len(), 2000);
            let payload = make_refs_payload(&refs);
            let err = validate_fragment_list(&fragment, &payload).expect_err("last at end");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn rejects_last_offset_beyond_content_end() {
            let refs = [
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 500,
                },
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 3000,
                },
            ];
            let fragment = make_fragmented(refs.len(), 2000);
            let payload = make_refs_payload(&refs);
            let err = validate_fragment_list(&fragment, &payload).expect_err("last beyond");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn rejects_content_end_overflow() {
            let refs = [
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: u64::MAX - 10,
                },
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: u64::MAX,
                },
            ];
            let fragment = make_fragmented(refs.len(), 100);
            let payload = make_refs_payload(&refs);
            let err = validate_fragment_list(&fragment, &payload).expect_err("overflow");
            assert!(matches!(err, StoreError::Internal(_)));
        }

        #[test]
        fn accepts_non_zero_first_offset() {
            // Interior/child-list case
            let refs = [
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 10_000_000,
                },
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 10_500_000,
                },
            ];
            let fragment = make_fragmented(refs.len(), 1_000_000);
            let payload = make_refs_payload(&refs);
            assert!(validate_fragment_list(&fragment, &payload).is_ok());
        }
    }
}
