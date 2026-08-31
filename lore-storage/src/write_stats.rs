// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Counters describing what an operation's fragment writes actually cost.
//!
//! One [`FragmentWriteStats`] is shared by every write an operation performs,
//! including the background leader tasks a [`crate::write_tracker::WriteTracker`]
//! dispatches. Each counter is a relaxed atomic add taken at the point in the
//! write pipeline where the fact becomes known, and the operation snapshots the
//! totals once it has drained its writes.
//!
//! # Cost
//!
//! Between two and thirteen relaxed `fetch_add`s per fragment, on counters that
//! share cache lines, against a hash, a compression pass and at least one store
//! write. Reading is a snapshot of plain loads. Nothing here allocates, and
//! nothing is retained per fragment.
//!
//! # Consistency
//!
//! A snapshot taken while writes are in flight is not a consistent cut: the
//! counters are independent, and a fragment part-way through the pipeline has
//! incremented some and not others. Only a snapshot taken after the operation
//! has drained its writes reports totals that add up. Every invariant below is
//! stated for a drained snapshot.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;

use crate::fragment_flags::FragmentFlags;
use crate::types::Fragment;

/// Live counters an operation's fragment writes report into. See
/// [`FragmentWriteCounts`] for what each one covers.
///
/// Shared behind an [`Arc`](std::sync::Arc) by the dispatching task and every
/// leader it spawns. See the module documentation for what a snapshot does and
/// does not guarantee.
#[derive(Default)]
pub struct FragmentWriteStats {
    /// Fragments handed to the store, whatever came of them, and the content
    /// they stand for.
    fragments_produced: AtomicU64,
    fragment_content_bytes: AtomicU64,
    /// Fragments the stores already held in the form this write wanted, so no
    /// payload was loaded, compressed or uploaded for them.
    fragments_deduplicated: AtomicU64,
    deduplicated_content_bytes: AtomicU64,
    /// Fragments that entered the write pipeline.
    fragments_processed: AtomicU64,
    processed_content_bytes: AtomicU64,

    /// Fragments whose payload was prepared, the payload being content.
    data_fragments: AtomicU64,
    /// Stored payload bytes of `data_fragments` — post-compression where the
    /// pipeline compressed them.
    data_payload_bytes: AtomicU64,
    /// Reassembled content bytes `data_fragments` stand for.
    data_content_bytes: AtomicU64,
    /// Fragments whose payload was prepared, the payload being a fragment list.
    fragmentlists: AtomicU64,
    /// Stored payload bytes of `fragmentlists`.
    fragmentlist_payload_bytes: AtomicU64,
    /// Fragments that needed no payload, so none was prepared.
    no_payload_fragments: AtomicU64,
    /// Content bytes `no_payload_fragments` stand for.
    no_payload_content_bytes: AtomicU64,

    /// Terminal entries written to the local store.
    local_writes: AtomicU64,
    /// Of `local_writes`, those that recorded only the header.
    local_metadata_writes: AtomicU64,
    /// Of `local_writes`, those that also wrote a payload.
    local_payload_writes: AtomicU64,
    /// Payload bytes written by `local_payload_writes`.
    local_payload_bytes: AtomicU64,

    /// Fragments registered with the remote.
    remote_writes: AtomicU64,
    /// Of `remote_writes`, those the peer duplicated from an association it
    /// already held, so no payload crossed the wire.
    remote_copy_writes: AtomicU64,
    /// Of `remote_writes`, those whose payload was uploaded.
    remote_put_writes: AtomicU64,
    /// Payload bytes uploaded by `remote_put_writes`.
    remote_put_bytes: AtomicU64,
    /// Fragments the remote already held under this very address, so they were
    /// processed — the local store still wanted the payload — but needed neither
    /// a copy nor an upload.
    remote_already_durable: AtomicU64,
    /// Fragments written with no remote consulted, the caller having asked for a
    /// local-only write.
    local_only_writes: AtomicU64,
    /// Fragments whose upload did not land, leaving them stored only locally.
    ///
    /// Not an error: the remote leg is best-effort and the write succeeds with the
    /// payload retained, for a later push to offer again. The payload is therefore
    /// counted under `local_payload_writes` as well, indistinguishably from one
    /// kept because the caller asked for it.
    remote_upload_failed: AtomicU64,
}

/// A fragment's `size_content`, counted only where counting it does not count the
/// same bytes twice.
///
/// A fragment list's `size_content` is the whole of the content its leaves cover,
/// so adding it to a total that already has those leaves in it would double the
/// tree. A list contributes its payload and nothing else.
fn size_content_of(fragment: &Fragment) -> u64 {
    if (fragment.flags & FragmentFlags::PayloadFragmented) != 0 {
        0
    } else {
        fragment.size_content
    }
}

impl FragmentWriteStats {
    /// This fragment was handed to the store.
    pub fn fragment_produced(&self, fragment: &Fragment) {
        self.fragments_produced.fetch_add(1, Ordering::Relaxed);
        self.fragment_content_bytes
            .fetch_add(size_content_of(fragment), Ordering::Relaxed);
    }

    /// The stores already satisfied this fragment, so no payload was prepared for
    /// it.
    pub fn fragment_deduplicated(&self, fragment: &Fragment) {
        self.fragments_deduplicated.fetch_add(1, Ordering::Relaxed);
        self.deduplicated_content_bytes
            .fetch_add(size_content_of(fragment), Ordering::Relaxed);
    }

    /// This fragment entered the write pipeline.
    pub fn fragment_processed(&self, fragment: &Fragment) {
        self.fragments_processed.fetch_add(1, Ordering::Relaxed);
        self.processed_content_bytes
            .fetch_add(size_content_of(fragment), Ordering::Relaxed);
    }

    /// The remote already held this address, so it took neither a copy nor an
    /// upload.
    pub fn remote_already_durable(&self) {
        self.remote_already_durable.fetch_add(1, Ordering::Relaxed);
    }

    /// No remote was consulted for this fragment; the write was local by request.
    pub fn local_only_write(&self) {
        self.local_only_writes.fetch_add(1, Ordering::Relaxed);
    }

    /// A remote was consulted and the upload did not land.
    pub fn remote_upload_failed(&self) {
        self.remote_upload_failed.fetch_add(1, Ordering::Relaxed);
    }

    /// This fragment's payload was prepared, loaded back from the local store or
    /// compressed from the caller's buffer. `fragment` carries that payload, so
    /// `size_payload` is the compressed size where compression applied.
    ///
    /// A fragment the peer duplicates an association for, and which is not also
    /// cached locally, needs no payload and prepares none.
    pub fn payload_prepared(&self, fragment: &Fragment) {
        if (fragment.flags & FragmentFlags::PayloadFragmented) != 0 {
            self.fragmentlists.fetch_add(1, Ordering::Relaxed);
            self.fragmentlist_payload_bytes
                .fetch_add(u64::from(fragment.size_payload), Ordering::Relaxed);
        } else {
            self.data_fragments.fetch_add(1, Ordering::Relaxed);
            self.data_payload_bytes
                .fetch_add(u64::from(fragment.size_payload), Ordering::Relaxed);
            self.data_content_bytes
                .fetch_add(fragment.size_content, Ordering::Relaxed);
        }
    }

    /// This fragment needed no payload, so none was prepared.
    pub fn payload_not_prepared(&self, fragment: &Fragment) {
        self.no_payload_fragments.fetch_add(1, Ordering::Relaxed);
        self.no_payload_content_bytes
            .fetch_add(size_content_of(fragment), Ordering::Relaxed);
    }

    /// A terminal entry was written to the local store. `payload_bytes` is the
    /// payload that went with it, or `None` for a header-only write.
    pub fn local_write(&self, payload_bytes: Option<u64>) {
        self.local_writes.fetch_add(1, Ordering::Relaxed);
        match payload_bytes {
            Some(bytes) => {
                self.local_payload_writes.fetch_add(1, Ordering::Relaxed);
                self.local_payload_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            None => {
                self.local_metadata_writes.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The peer duplicated an association it already held, so no payload was sent.
    pub fn remote_copy(&self) {
        self.remote_writes.fetch_add(1, Ordering::Relaxed);
        self.remote_copy_writes.fetch_add(1, Ordering::Relaxed);
    }

    /// A payload was uploaded to the peer.
    pub fn remote_put(&self, payload_bytes: u64) {
        self.remote_writes.fetch_add(1, Ordering::Relaxed);
        self.remote_put_writes.fetch_add(1, Ordering::Relaxed);
        self.remote_put_bytes
            .fetch_add(payload_bytes, Ordering::Relaxed);
    }

    /// Read every counter. See the module documentation on consistency.
    pub fn snapshot(&self) -> FragmentWriteCounts {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        FragmentWriteCounts {
            fragments_produced: load(&self.fragments_produced),
            fragment_content_bytes: load(&self.fragment_content_bytes),
            fragments_deduplicated: load(&self.fragments_deduplicated),
            deduplicated_content_bytes: load(&self.deduplicated_content_bytes),
            fragments_processed: load(&self.fragments_processed),
            processed_content_bytes: load(&self.processed_content_bytes),
            data_fragments: load(&self.data_fragments),
            data_payload_bytes: load(&self.data_payload_bytes),
            data_content_bytes: load(&self.data_content_bytes),
            fragmentlists: load(&self.fragmentlists),
            fragmentlist_payload_bytes: load(&self.fragmentlist_payload_bytes),
            no_payload_fragments: load(&self.no_payload_fragments),
            no_payload_content_bytes: load(&self.no_payload_content_bytes),
            local_writes: load(&self.local_writes),
            local_metadata_writes: load(&self.local_metadata_writes),
            local_payload_writes: load(&self.local_payload_writes),
            local_payload_bytes: load(&self.local_payload_bytes),
            remote_writes: load(&self.remote_writes),
            remote_copy_writes: load(&self.remote_copy_writes),
            remote_put_writes: load(&self.remote_put_writes),
            remote_put_bytes: load(&self.remote_put_bytes),
            remote_already_durable: load(&self.remote_already_durable),
            local_only_writes: load(&self.local_only_writes),
            remote_upload_failed: load(&self.remote_upload_failed),
        }
    }
}

/// A snapshot of [`FragmentWriteStats`], as plain numbers, and the payload an
/// operation reports them in.
///
/// Only the `data_*` and `fragmentlist_*` fields split by what a payload is. Every
/// other count covers a fragment whatever its payload, content or a fragment list.
/// The content totals take a fragment list as zero, its `size_content` being the
/// content of its leaves.
///
/// For a drained operation, unless a fragment failed part-way through the
/// pipeline:
///
/// - `fragments_produced == fragments_deduplicated + fragments_processed`.
/// - `fragment_content_bytes == deduplicated_content_bytes + processed_content_bytes`.
/// - `local_writes == local_metadata_writes + local_payload_writes`.
/// - `remote_writes == remote_copy_writes + remote_put_writes`.
/// - `remote_writes`, `remote_already_durable`, `local_only_writes` and
///   `remote_upload_failed` sum to `fragments_processed`: every processed
///   fragment reaches exactly one of those outcomes.
/// - `data_fragments + fragmentlists + no_payload_fragments == fragments_processed`.
/// - `data_content_bytes + no_payload_content_bytes == processed_content_bytes`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FragmentWriteCounts {
    /// Fragments handed to the store, whatever came of them.
    pub fragments_produced: u64,
    /// Uncompressed content bytes the produced fragments stand for.
    pub fragment_content_bytes: u64,
    /// Fragments the stores already held in the form the write wanted, so no
    /// payload was loaded, compressed or uploaded for them.
    pub fragments_deduplicated: u64,
    /// Content bytes of `fragments_deduplicated`.
    pub deduplicated_content_bytes: u64,
    /// Fragments that entered the write pipeline.
    pub fragments_processed: u64,
    /// Content bytes of `fragments_processed`.
    pub processed_content_bytes: u64,
    /// Of `fragments_processed`, those that produced a stored payload of content.
    pub data_fragments: u64,
    /// Stored payload bytes of `data_fragments`, after compression where the
    /// pipeline compressed them.
    pub data_payload_bytes: u64,
    /// Uncompressed content bytes `data_fragments` stand for. Compare against
    /// `data_payload_bytes` for the compression ratio.
    pub data_content_bytes: u64,
    /// Of `fragments_processed`, those that produced a stored fragment list.
    pub fragmentlists: u64,
    /// Stored payload bytes of `fragmentlists`.
    pub fragmentlist_payload_bytes: u64,
    /// Of `fragments_processed`, those that needed no payload, so none was
    /// prepared: the remote duplicated an association for them and the write did
    /// not ask for the payload to be cached locally.
    pub no_payload_fragments: u64,
    /// Content bytes `no_payload_fragments` stand for.
    pub no_payload_content_bytes: u64,
    /// Terminal entries written to the local store.
    pub local_writes: u64,
    /// Of `local_writes`, those that recorded only the fragment header — the
    /// payload lives on the remote and was not cached here.
    pub local_metadata_writes: u64,
    /// Of `local_writes`, those that also wrote a payload.
    pub local_payload_writes: u64,
    /// Payload bytes written by `local_payload_writes`.
    pub local_payload_bytes: u64,
    /// Fragments registered with the remote.
    pub remote_writes: u64,
    /// Of `remote_writes`, those the remote duplicated from an association it
    /// already held, so no payload crossed the wire.
    pub remote_copy_writes: u64,
    /// Of `remote_writes`, those whose payload was uploaded.
    pub remote_put_writes: u64,
    /// Payload bytes uploaded by `remote_put_writes`.
    pub remote_put_bytes: u64,
    /// Fragments the remote already held under this very address, so they took
    /// neither a copy nor an upload.
    pub remote_already_durable: u64,
    /// Fragments written with no remote consulted, a local-only write having been
    /// asked for. Branch latest history is one such write, which the server does
    /// not store, so a commit against a remote has exactly one.
    pub local_only_writes: u64,
    /// Fragments whose upload did not land, leaving them stored only locally for
    /// a later push to offer again. Their payloads are counted under
    /// `local_payload_writes` too, indistinguishably from those kept by request.
    pub remote_upload_failed: u64,
}

impl FragmentWriteCounts {
    /// What has been counted since `baseline` was taken.
    ///
    /// The counters are per call rather than per operation, so an operation that
    /// writes before it commits — a merge serializing its staged state — reports
    /// the commit's own writes by taking the difference.
    pub fn since(&self, baseline: &Self) -> Self {
        Self {
            fragments_produced: self
                .fragments_produced
                .saturating_sub(baseline.fragments_produced),
            fragment_content_bytes: self
                .fragment_content_bytes
                .saturating_sub(baseline.fragment_content_bytes),
            fragments_deduplicated: self
                .fragments_deduplicated
                .saturating_sub(baseline.fragments_deduplicated),
            deduplicated_content_bytes: self
                .deduplicated_content_bytes
                .saturating_sub(baseline.deduplicated_content_bytes),
            fragments_processed: self
                .fragments_processed
                .saturating_sub(baseline.fragments_processed),
            processed_content_bytes: self
                .processed_content_bytes
                .saturating_sub(baseline.processed_content_bytes),
            data_fragments: self.data_fragments.saturating_sub(baseline.data_fragments),
            data_payload_bytes: self
                .data_payload_bytes
                .saturating_sub(baseline.data_payload_bytes),
            data_content_bytes: self
                .data_content_bytes
                .saturating_sub(baseline.data_content_bytes),
            fragmentlists: self.fragmentlists.saturating_sub(baseline.fragmentlists),
            fragmentlist_payload_bytes: self
                .fragmentlist_payload_bytes
                .saturating_sub(baseline.fragmentlist_payload_bytes),
            no_payload_fragments: self
                .no_payload_fragments
                .saturating_sub(baseline.no_payload_fragments),
            no_payload_content_bytes: self
                .no_payload_content_bytes
                .saturating_sub(baseline.no_payload_content_bytes),
            local_writes: self.local_writes.saturating_sub(baseline.local_writes),
            local_metadata_writes: self
                .local_metadata_writes
                .saturating_sub(baseline.local_metadata_writes),
            local_payload_writes: self
                .local_payload_writes
                .saturating_sub(baseline.local_payload_writes),
            local_payload_bytes: self
                .local_payload_bytes
                .saturating_sub(baseline.local_payload_bytes),
            remote_writes: self.remote_writes.saturating_sub(baseline.remote_writes),
            remote_copy_writes: self
                .remote_copy_writes
                .saturating_sub(baseline.remote_copy_writes),
            remote_put_writes: self
                .remote_put_writes
                .saturating_sub(baseline.remote_put_writes),
            remote_put_bytes: self
                .remote_put_bytes
                .saturating_sub(baseline.remote_put_bytes),
            remote_already_durable: self
                .remote_already_durable
                .saturating_sub(baseline.remote_already_durable),
            local_only_writes: self
                .local_only_writes
                .saturating_sub(baseline.local_only_writes),
            remote_upload_failed: self
                .remote_upload_failed
                .saturating_sub(baseline.remote_upload_failed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_fragment(payload: u32, content: u64) -> Fragment {
        Fragment {
            flags: 0,
            size_payload: payload,
            size_content: content,
        }
    }

    fn list_fragment(payload: u32, content: u64) -> Fragment {
        Fragment {
            flags: FragmentFlags::PayloadFragmented.bits(),
            size_payload: payload,
            size_content: content,
        }
    }

    #[test]
    fn a_fresh_snapshot_is_all_zero() {
        assert_eq!(
            FragmentWriteStats::default().snapshot(),
            FragmentWriteCounts::default()
        );
    }

    /// Every processed fragment either has a payload prepared or does not, so the
    /// three counts partition it. A fragment reported in none of them is one the
    /// report cannot account for.
    #[test]
    fn payload_outcomes_account_for_every_processed_fragment() {
        let stats = FragmentWriteStats::default();
        let data = data_fragment(100, 1000);
        let list = list_fragment(80, 4096);
        for _ in 0..5 {
            stats.fragment_processed(&data);
        }
        stats.fragment_processed(&list);
        stats.payload_prepared(&data);
        stats.payload_prepared(&data);
        stats.payload_prepared(&list);
        for _ in 0..3 {
            stats.payload_not_prepared(&data);
        }

        let counts = stats.snapshot();
        assert_eq!(
            counts.data_fragments + counts.fragmentlists + counts.no_payload_fragments,
            counts.fragments_processed
        );
        assert_eq!(
            counts.data_content_bytes + counts.no_payload_content_bytes,
            counts.processed_content_bytes
        );
    }

    /// The split the report leans on: a list fragment must not land in the data
    /// buckets, or the compression ratio would be computed against reference
    /// bytes that were never file content.
    #[test]
    fn a_list_fragment_is_counted_apart_from_data() {
        let stats = FragmentWriteStats::default();
        stats.payload_prepared(&data_fragment(100, 400));
        stats.payload_prepared(&list_fragment(64, 4096));

        let counts = stats.snapshot();
        assert_eq!(counts.data_fragments, 1);
        assert_eq!(counts.data_payload_bytes, 100);
        assert_eq!(counts.data_content_bytes, 400);
        assert_eq!(counts.fragmentlists, 1);
        assert_eq!(counts.fragmentlist_payload_bytes, 64);
    }

    #[test]
    fn local_writes_split_into_metadata_only_and_payload_bearing() {
        let stats = FragmentWriteStats::default();
        stats.local_write(Some(300));
        stats.local_write(Some(700));
        stats.local_write(None);

        let counts = stats.snapshot();
        assert_eq!(counts.local_writes, 3);
        assert_eq!(counts.local_payload_writes, 2);
        assert_eq!(counts.local_payload_bytes, 1000);
        assert_eq!(counts.local_metadata_writes, 1);
        assert_eq!(
            counts.local_writes,
            counts.local_metadata_writes + counts.local_payload_writes
        );
    }

    /// Every processed fragment reaches exactly one of four outcomes against the
    /// remote. A fragment counted in none of them is one the report cannot
    /// account for.
    #[test]
    fn the_remote_outcomes_account_for_every_processed_fragment() {
        let stats = FragmentWriteStats::default();
        let fragment = data_fragment(64, 1024);
        for _ in 0..9 {
            stats.fragment_processed(&fragment);
        }
        stats.remote_copy();
        stats.remote_copy();
        stats.remote_put(64);
        stats.remote_put(64);
        stats.remote_put(64);
        stats.remote_already_durable();
        stats.remote_already_durable();
        stats.local_only_write();
        stats.remote_upload_failed();

        let counts = stats.snapshot();
        assert_eq!(
            counts.remote_writes
                + counts.remote_already_durable
                + counts.local_only_writes
                + counts.remote_upload_failed,
            counts.fragments_processed
        );
        assert_eq!(counts.remote_upload_failed, 1);
    }

    /// A copy is a remote write that sends no bytes. Folding it into the put
    /// totals would hide exactly the saving the copy path exists to make.
    #[test]
    fn a_copy_is_a_remote_write_that_carries_no_bytes() {
        let stats = FragmentWriteStats::default();
        stats.remote_copy();
        stats.remote_put(2048);

        let counts = stats.snapshot();
        assert_eq!(counts.remote_writes, 2);
        assert_eq!(counts.remote_copy_writes, 1);
        assert_eq!(counts.remote_put_writes, 1);
        assert_eq!(counts.remote_put_bytes, 2048);
    }

    #[test]
    fn offered_fragments_split_into_deduplicated_and_processed() {
        let stats = FragmentWriteStats::default();
        let fragment = data_fragment(20, 100);
        for _ in 0..5 {
            stats.fragment_produced(&fragment);
        }
        for _ in 0..2 {
            stats.fragment_deduplicated(&fragment);
        }
        for _ in 0..3 {
            stats.fragment_processed(&fragment);
        }

        let counts = stats.snapshot();
        assert_eq!(counts.fragments_produced, 5);
        assert_eq!(
            counts.fragments_produced,
            counts.fragments_deduplicated + counts.fragments_processed
        );
        assert_eq!(counts.fragment_content_bytes, 500);
        assert_eq!(
            counts.fragment_content_bytes,
            counts.deduplicated_content_bytes + counts.processed_content_bytes
        );
    }

    /// Distinct values per field, so a difference wired to the wrong field shows
    /// up rather than passing on a shared zero. Against no baseline every count
    /// stands as it is; against itself every count is spent.
    #[test]
    fn a_difference_is_taken_field_by_field() {
        let mut counts = FragmentWriteCounts::default();
        for (index, field) in [
            &mut counts.fragments_produced,
            &mut counts.fragment_content_bytes,
            &mut counts.fragments_deduplicated,
            &mut counts.deduplicated_content_bytes,
            &mut counts.fragments_processed,
            &mut counts.processed_content_bytes,
            &mut counts.data_fragments,
            &mut counts.data_payload_bytes,
            &mut counts.data_content_bytes,
            &mut counts.fragmentlists,
            &mut counts.fragmentlist_payload_bytes,
            &mut counts.no_payload_fragments,
            &mut counts.no_payload_content_bytes,
            &mut counts.local_writes,
            &mut counts.local_metadata_writes,
            &mut counts.local_payload_writes,
            &mut counts.local_payload_bytes,
            &mut counts.remote_writes,
            &mut counts.remote_copy_writes,
            &mut counts.remote_put_writes,
            &mut counts.remote_put_bytes,
            &mut counts.remote_already_durable,
            &mut counts.local_only_writes,
            &mut counts.remote_upload_failed,
        ]
        .into_iter()
        .enumerate()
        {
            *field = index as u64 + 1;
        }

        assert_eq!(counts.since(&FragmentWriteCounts::default()), counts);
        assert_eq!(counts.since(&counts), FragmentWriteCounts::default());
    }

    /// A list's `size_content` is the content of the whole tree beneath it, so
    /// counting it alongside its leaves would report the content twice.
    #[test]
    fn a_fragment_list_contributes_no_content_to_a_total() {
        assert_eq!(size_content_of(&data_fragment(100, 400)), 400);
        assert_eq!(size_content_of(&list_fragment(64, 4096)), 0);
    }
}
