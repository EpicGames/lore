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
use crate::local::store_lock::StoreGuard;
use crate::store_types::PayloadRead;
use crate::store_types::StoreGetData;
use crate::store_types::StoreMatch;
use crate::store_types::StoreMatchResult;
use crate::store_types::StoreObliterateStats;

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

/// Validate that a fragment's sizes appear valid. Use before allocating or
/// streaming a payload buffer based on attacker-influenced metadata.
/// These checks are necessary for data that may exist before hardening at the point of ingress
/// was corrected
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

    if (fragment.flags & FragmentFlags::PayloadFragmented) == 0 {
        let size_content = fragment.size_content as usize;
        if size_content > crate::FRAGMENT_SIZE_THRESHOLD {
            return Err(StoreError::from(Oversized {
                context: format!(
                    "unfragmented size_content {size_content} exceeds FRAGMENT_SIZE_THRESHOLD {}",
                    crate::FRAGMENT_SIZE_THRESHOLD
                ),
            }));
        }
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

/// Whether the stored payload is the content itself, needing neither reassembly nor expansion, so
/// a read of it can be handed over as it lies.
///
/// The sizes have to agree for that to hold. [`validate_fragment_metadata`] refuses a fragment
/// where they do not, but only at ingress, and a store may already hold one from before that
/// boundary existed: its payload is shorter than the content it claims, so it is not the content.
pub(crate) fn payload_is_content(fragment: &Fragment) -> bool {
    let fragmented =
        (fragment.flags & FragmentFlags::PayloadFragmented) == FragmentFlags::PayloadFragmented;
    let compressed = (fragment.flags & FragmentFlags::PayloadCompressed) != 0;
    !fragmented && !compressed && fragment.size_payload as u64 == fragment.size_content
}

/// Refuse content the destination has no room for, rather than truncating it.
pub(crate) fn validate_buffer_capacity(size: usize, capacity: usize) -> Result<(), StoreError> {
    if size > capacity {
        return Err(StoreError::from(Oversized {
            context: format!(
                "content of {size} bytes exceeds the {capacity} byte destination buffer"
            ),
        }));
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

/// Resolve one address. The batched form is what stores implement, because batching is a real
/// capability and asking about one address is the degenerate case of it; this is a free function
/// rather than a trait method so no store can override it and reintroduce the divergence between
/// asking about one address and asking about several.
pub async fn query_one(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
) -> Result<StoreMatchResult, StoreError> {
    let mut result = [StoreMatchResult::default()];
    store
        .clone()
        .query(partition, &[address], &mut result)
        .await?;
    Ok(result[0])
}

#[async_trait]
pub trait ImmutableStore: Any + Send + Sync {
    /// Check if this store is backed by local disk
    fn is_local(&self) -> bool {
        false
    }

    /// Claims this store for the span it is about to be used — a storage handle from
    /// open to close, or one repository command.
    ///
    /// The claim is what tells other processes this one is using the store, so it
    /// belongs to the span rather than to each call within it. It is also the pair a
    /// repository lock is contained within: store locks are the outer ones, because
    /// they are the pair the storage API can also take, and two orders over one pair
    /// would be a cycle.
    ///
    /// `None` means this store has nothing on disk to claim — a remote store, or an
    /// in-memory one. **A store that wraps one which does have files must delegate to
    /// it rather than inherit this**, or a handle opens holding nothing and then reads
    /// and writes a store no process has claimed.
    ///
    /// # Errors
    ///
    /// [`StoreError`] if the lock cannot be taken, or if the reload a stale
    /// acquisition demands fails.
    async fn hold_for_command(self: Arc<Self>) -> Result<Option<StoreGuard>, StoreError> {
        Ok(None)
    }

    /// Whether this store refuses to serve a payload it only found under another partition.
    ///
    /// Content is addressed by hash, so the same bytes stored by two tenants resolve to one entry.
    /// A process that holds content for more than one of them has to decide whether a caller naming
    /// a partition it does have access to may be served bytes that were only ever written under a
    /// partition it does not. A single-tenant process has nothing to protect and answers `false`.
    fn isolates_partitions(&self) -> bool {
        false
    }

    /// How widely this store searches when serving content: the widest match it will answer
    /// [`ImmutableStore::get`] or [`ImmutableStore::get_metadata`] with.
    ///
    /// A store that isolates partitions serves the association it was asked for and nothing else.
    /// It sits behind a trust boundary, and no protocol carrying a served payload carries the level
    /// it was found at, so whatever such a store serves is read as a full match by whoever receives
    /// it. Answering only exact associations is what makes that reading true. A single-tenant store
    /// has no boundary to cross and serves whatever it holds for the hash.
    ///
    /// One policy for both reads, so `get` and `get_metadata` cannot drift apart into describing
    /// content one of them would refuse to hand over. What a store will *report* reaches further —
    /// see [`ImmutableStore::query_scope`].
    fn read_scope(&self) -> StoreMatch {
        if self.isolates_partitions() {
            StoreMatch::MatchFull
        } else {
            StoreMatch::MatchHash
        }
    }

    /// How widely this store searches when reporting what it holds, which is wider than what it
    /// will serve.
    ///
    /// A partition match says the payload is already in the partition the caller asked about, so an
    /// association can be duplicated with [`ImmutableStore::copy`] rather than transferred. That is
    /// a fact about a partition the caller already holds, and acting on it takes a separate
    /// operation the store authorizes in its own right — not bytes handed over here. So a store may
    /// report a level it would refuse to read at, and an isolating store does.
    fn query_scope(&self) -> StoreMatch {
        if self.isolates_partitions() {
            StoreMatch::MatchPartition
        } else {
            StoreMatch::MatchHash
        }
    }

    /// Serve the payload for an address, searching as widely as this store permits.
    ///
    /// There is no level to choose. A context is not an access boundary, so content stored under a
    /// sibling context in the same partition is always readable; whether the search may cross a
    /// partition is [`ImmutableStore::isolates_partitions`], which the store answers from its own
    /// configuration rather than the caller from the call.
    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError>;

    /// Read the payload stored under `address`, into `dst` when the payload is the content itself
    /// and into a buffer of its own otherwise, reporting the fragment that describes it.
    ///
    /// One lookup settles where the payload belongs and reads it there, so a reader that already
    /// has somewhere for the content to go pays no second lookup either way. What a returned
    /// payload has to become — expanded, or walked as a fragment list — is the reader's to do; this
    /// only decides where the stored bytes land. A payload `dst` has no room for is [`Oversized`];
    /// a returned one is the reader's to size.
    ///
    /// The default implementation goes through [`get`](ImmutableStore::get) and copies. A store
    /// overrides it to read its index once and have the read itself land in `dst`.
    async fn get_into(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        dst: &mut crate::CallerBuffer,
    ) -> Result<(Fragment, PayloadRead), StoreError> {
        let data = self.get(partition, address).await?;
        let fragment = data.fragment;
        let payload = data
            .payload
            .ok_or_else(|| StoreError::from(PayloadNotFound::from(address.hash)))?;
        validate_fragment_payload(&fragment, payload.len())?;

        if !payload_is_content(&fragment) {
            return Ok((fragment, PayloadRead::Returned(payload)));
        }

        let capacity = dst.len();
        let Some(target) = dst.as_mut_slice().get_mut(..payload.len()) else {
            return Err(StoreError::from(Oversized {
                context: format!(
                    "payload of {} bytes exceeds the {capacity} byte destination buffer",
                    payload.len()
                ),
            }));
        };
        target.copy_from_slice(&payload);
        Ok((fragment, PayloadRead::IntoBuffer))
    }

    /// Check if this store is available for service
    async fn is_available(self: Arc<Self>, _timeout: Duration) -> bool {
        true
    }

    /// Resolve addresses against this store, writing one result per address into `results`, in the
    /// order the addresses were given. `results` must be as long as `addresses`.
    ///
    /// One question, asked once, with no requested level: the store reports the best match it
    /// establishes rather than confirming a level the caller guessed at.
    ///
    /// The contract, checked for every implementation by [`crate::conformance`]:
    ///
    /// 1. **Never over-report.** A reported level must hold: the association it names exists. It
    ///    says nothing about whether the payload can be handed over here — that is
    ///    [`StoreMatchResult::stored_local`] and [`StoreMatchResult::stored_durable`]. A store may
    ///    hold the representation and not the bytes, and reports a full match when it does.
    /// 2. **May under-report.** A store may answer with a weaker level than the truth when
    ///    establishing the stronger one costs more than it is worth — a durable store need not
    ///    spend a lookup to distinguish "in this partition" from "somewhere". Callers must read a
    ///    weak level as "no shortcut available", never as proof of absence.
    /// 3. **Obliterated never matches**, here and through
    ///    [`ImmutableStore::get_metadata`] and [`ImmutableStore::get`] alike.
    /// 4. **Reads do not under-serve, and agree with each other.** Whatever
    ///    [`ImmutableStore::get`] will serve, [`ImmutableStore::get_metadata`] will describe: both
    ///    reach exactly as far as [`ImmutableStore::read_scope`], so neither describes what the
    ///    other would refuse. This reaches further, to [`ImmutableStore::query_scope`], because
    ///    what it reports is a level to act on with another operation rather than bytes owed here.
    /// 5. **A match names where it was found, and prefers where it was asked.** Where the partition
    ///    asked about holds the hash, that is the one reported; another may be named only when it
    ///    does not, which only a store reading across partitions can do.
    ///
    /// `results` must be the same length as `addresses`, and each entry is written in place.
    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError>;

    /// Query the fragment describing the payload stored for the given address.
    ///
    /// Where [`ImmutableStore::query`] answers whether a payload exists and where it is stored,
    /// this answers *what it is* — its compression and its sizes. The two are separate because a
    /// store that keeps the fragment beside the payload rather than in an index has to go and read
    /// it, which costs a round trip that `query` deliberately avoids: `query` sits on the ingress
    /// write path and runs once per fragment stored.
    ///
    /// Searched at the same scope as [`ImmutableStore::get`], and for the same reason: the store
    /// decides how widely it looks, not the caller. Describing a payload is strictly less than
    /// handing it over, so anything this store would serve the bytes of, it will also describe. A
    /// weaker level than `MatchFull` says the representation belongs to content reached under
    /// another context or partition — the same bytes, since the hash is the same.
    ///
    /// Required rather than defaulted. A default delegating to `query` is right for a store whose
    /// `query` already reports the representation, and silently wrong for a wrapper that forwards
    /// `query` alone — the wrapper's own `query` would answer, the inner store's override would
    /// never run, and the caller would get a well-formed fragment with no sizes and no error. Every
    /// implementor decides instead.
    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError>;

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

    /// Stop eviction and compaction, returning once the passes in flight have given up.
    /// With `terminate` the stop stays raised and the store never collects again; without
    /// it the stop is lifted before returning, since a store is shared by path and a caller
    /// quiescing it must not disable collection for the others. Stores that do not collect
    /// need no implementation.
    async fn stop_gc(self: Arc<Self>, terminate: bool) {
        let _ = terminate;
    }

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
    /// The source is named one of two ways, and the caller chooses which:
    ///
    /// - A context names one exact association, resolved exactly. A store that does not hold that
    ///   tuple reports [`StoreError::AddressNotFound`] — there is no widening to a sibling context.
    /// - A zero context names any association of the hash in `source_partition`, which is what a
    ///   caller acting on a partition match has: that level says the partition holds the hash and
    ///   never says under which context. Every association there names the same bytes, so which one
    ///   answers does not change what the destination ends up holding. None at all is
    ///   [`StoreError::AddressNotFound`] as before.
    ///
    /// Every store supporting `copy` supports both. The exact form is the one that can be answered
    /// with a keyed read, so a caller holding a context passes it.
    ///
    /// `durable` controls the destination's `PayloadStoredDurable` flag: pass `true` only when
    /// the caller has independent confirmation that the destination tuple is durably stored
    /// (typically a successful remote round-trip). The source's own durable flag never
    /// propagates — durability is a per-(partition, address) property and a local copy of an
    /// already-durable source does not make the new destination tuple durable.
    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        durable: bool,
    ) -> Result<(), StoreError>;

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
