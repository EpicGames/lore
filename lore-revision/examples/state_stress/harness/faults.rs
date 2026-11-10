// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A store double that fails its reads on demand, for the scenarios of an operation meeting an
//! error part way (H6), and the stress suite's failure windows.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::Partition;
use lore_storage::FlushOptions;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreGetData;

use crate::harness::catalogue::call::failed_read;

/// No countdown armed ([`FailingStore::fail_after`]).
const NO_COUNTDOWN: u64 = u64::MAX;

/// The text of the error a failed read answers.
pub const INJECTED: &str = "a read the test fails";

/// The texts an operation's error carries where a failed read reached it: the store's own, the
/// State's for a block whose fetch failed (`AddressNotFound`), or the storage's refusal of content
/// larger than its reader's type, as a node block whose read failed falls back to the older block
/// formats, whose reads of the block's content refuse it so (`NodeBlock::deserialize`).
pub const INJECTED_AS: &[&str] = &[
    INJECTED,
    "Address not found: ",
    "exceeds caller-supplied max",
];

/// An immutable store over another, whose reads fail as it is told: all of them, all after a
/// number more, or one in a number, chosen from a seed. The modes combine, a read failing where
/// any armed mode fails it; a countdown spends every read the store does not fail outright, so
/// with a window armed beside it fewer reads than its number may succeed. A read it does not fail
/// may stall until let through, as a slow store's does ([`Self::stall`]). Writes always go
/// through.
pub struct FailingStore {
    inner: Arc<dyn ImmutableStore>,
    failing: AtomicBool,
    /// The reads left to succeed before every read fails; [`NO_COUNTDOWN`] for none.
    countdown: AtomicU64,
    /// One read in this many fails, by [`Self::seed`] and the read's ordinal; 0 for none.
    one_in: AtomicU64,
    seed: AtomicU64,
    /// Every read asked for, failed or not.
    reads: AtomicU64,
    /// The reads failed.
    failed: AtomicU64,
    /// The reads failed outside every call's own steps ([`Self::failed_outside_calls`]).
    failed_outside: AtomicU64,
    /// Whether reads stall until let through.
    stalling: AtomicBool,
    /// The passes stalled reads wait for ([`Self::pass`]).
    passes: tokio::sync::Semaphore,
    /// Every read that stalled.
    stalls: AtomicU64,
}

impl FailingStore {
    /// A store reading through `inner`, failing nothing yet.
    pub fn new(inner: Arc<dyn ImmutableStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            failing: AtomicBool::new(false),
            countdown: AtomicU64::new(NO_COUNTDOWN),
            one_in: AtomicU64::new(0),
            seed: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            failed_outside: AtomicU64::new(0),
            stalling: AtomicBool::new(false),
            passes: tokio::sync::Semaphore::new(0),
            stalls: AtomicU64::new(0),
        })
    }

    /// Fails every read from now on, or none, dropping any countdown and window.
    pub fn fail(&self, failing: bool) {
        self.countdown.store(NO_COUNTDOWN, Ordering::SeqCst);
        self.one_in.store(0, Ordering::SeqCst);
        self.failing.store(failing, Ordering::SeqCst);
    }

    /// Lets the next `reads` reads pass the countdown, then fails every read until [`Self::fail`]
    /// says otherwise. A window armed beside it still fails the reads it fails meanwhile.
    pub fn fail_after(&self, reads: u64) {
        self.failing.store(false, Ordering::SeqCst);
        self.countdown.store(reads, Ordering::SeqCst);
    }

    /// Fails one read in `one_in`, chosen from `seed` and each read's ordinal, so a run with the
    /// same seed and the same reads fails the same ones; 0 fails none.
    pub fn fail_one_in(&self, one_in: u64, seed: u64) {
        self.seed.store(seed, Ordering::SeqCst);
        self.one_in.store(one_in, Ordering::SeqCst);
    }

    /// The reads asked for so far, failed or not.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }

    /// The reads failed so far.
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::SeqCst)
    }

    /// The reads failed so far outside every call's own steps: a call's own failed read answers it
    /// failed, and what it left part way its records tell, as they tell a dropped call's. One
    /// elsewhere, in an operation no call holds, or in a future a call spawned, which counts on the
    /// call too but may have stopped the future part way where the call answered done, may excuse
    /// closure on the whole State.
    pub fn failed_outside_calls(&self) -> u64 {
        self.failed_outside.load(Ordering::SeqCst)
    }

    /// Stalls every read from now on, each until [`Self::pass`] lets one through, or no read
    /// after; a read stalled already still waits for its pass.
    pub fn stall(&self, stalling: bool) {
        self.stalling.store(stalling, Ordering::SeqCst);
    }

    /// Lets one stalled read go on, the next to stall where none waits.
    pub fn pass(&self) {
        self.passes.add_permits(1);
    }

    /// The reads that have stalled so far.
    pub fn stalls(&self) -> u64 {
        self.stalls.load(Ordering::SeqCst)
    }

    /// Fails the read where an armed mode fails it, and stalls it where the store stalls reads.
    async fn admit(&self) -> Result<(), StoreError> {
        self.refuse()?;
        if self.stalling.load(Ordering::SeqCst) {
            self.stalls.fetch_add(1, Ordering::SeqCst);
            self.passes
                .acquire()
                .await
                .map_err(|error| StoreError::internal(format!("the passes closed: {error}")))?
                .forget();
        }
        Ok(())
    }

    fn refuse(&self) -> Result<(), StoreError> {
        let ordinal = self.reads.fetch_add(1, Ordering::SeqCst);
        let fails =
            self.failing.load(Ordering::SeqCst) || self.counted_down() || self.windowed(ordinal);
        if fails {
            self.failed.fetch_add(1, Ordering::SeqCst);
            if !failed_read() {
                self.failed_outside.fetch_add(1, Ordering::SeqCst);
            }
            Err(StoreError::internal(INJECTED))
        } else {
            Ok(())
        }
    }

    /// Whether the countdown has run out, spending one read of it where it has not.
    fn counted_down(&self) -> bool {
        self.countdown
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| match left {
                NO_COUNTDOWN | 0 => None,
                left => Some(left - 1),
            })
            .is_err_and(|left| left == 0)
    }

    /// Whether the window fails read `ordinal`: a `SplitMix64` step over the seed and the ordinal.
    fn windowed(&self, ordinal: u64) -> bool {
        let one_in = self.one_in.load(Ordering::SeqCst);
        if one_in == 0 {
            return false;
        }
        let mut mixed = self
            .seed
            .load(Ordering::SeqCst)
            .wrapping_add((ordinal + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (mixed ^ (mixed >> 31)).is_multiple_of(one_in)
    }
}

#[async_trait::async_trait]
impl ImmutableStore for FailingStore {
    async fn hold_for_command(
        self: Arc<Self>,
    ) -> Result<Option<lore_storage::local::store_lock::StoreGuard>, StoreError> {
        self.inner.clone().hold_for_command().await
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.admit().await?;
        self.inner.clone().get(partition, address).await
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [lore_storage::StoreMatchResult],
    ) -> Result<(), StoreError> {
        self.admit().await?;
        self.inner
            .clone()
            .query(partition, addresses, results)
            .await
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.admit().await?;
        self.inner.clone().get_metadata(partition, address).await
    }

    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: lore_base::types::Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .put(partition, address, fragment, payload, force)
            .await
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<lore_storage::StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .obliterate(partition, address, stats)
            .await
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        self.inner
            .clone()
            .evict(max_capacity, sync_data, sink)
            .await
    }

    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        self.inner
            .clone()
            .compact(max_size, at, sync_data, sink)
            .await
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        self.inner.clone().compact_resume_at().await
    }

    fn max_query_batch(&self) -> Option<usize> {
        self.inner.max_query_batch()
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
        self.inner.clone().verify(heal).await
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: lore_base::types::Context,
        behavior: lore_storage::immutable_store::CopyBehavior,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                behavior,
            )
            .await
    }
}
