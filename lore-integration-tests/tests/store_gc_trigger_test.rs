// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The local store's load-driven garbage collection trigger.
//!
//! Loading more of a store than its configured size cap starts a compaction pass without an
//! explicit `repository gc`. The pass is spawned detached and reports through the sink bound at
//! the moment it fired, so it only reports if it outlives whatever triggered it: a process that
//! exits promptly asks the pass to stop, and `compact_packfiles` checks that request twice
//! before reporting anything. This runs the trigger in a process that stays alive and waits for
//! the report, so the assertion is about the trigger rather than about who wins that race.
//!
//! Its own test target: [`lore_storage::gc_event::set_gc_event_sink_provider`] takes the first
//! registration process-wide, and `lore-revision` registers its own on the first client store
//! open, so a sink installed from inside the shared suite binary would be used or ignored
//! depending on test order.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Partition;
use lore_storage::ImmutableStore;
use lore_storage::ImmutableStoreSettings;
use lore_storage::LocalImmutableStore;
use lore_storage::gc_event::GcEventSink;
use lore_storage::gc_event::GcEventSinkRef;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::unbounded_channel;

/// How long the pass is given to report. Generous: the assertion is that the trigger fires at
/// all, and the pass reads every group's packstore and buckets before it reports.
const REPORT_DEADLINE: Duration = Duration::from_secs(60);

/// Bytes stored before the cap is lowered, spread over enough fragments that several groups
/// hold a packfile.
const FRAGMENT_COUNT: u16 = 64;
const FRAGMENT_SIZE: usize = 4096;

/// The size cap the reopened store runs under. Any real content exceeds it, so the load is
/// certain to cross it.
const TINY_MAX_SIZE: usize = 100;

/// Reports compaction begins to the test. The sink provider is a bare function pointer and
/// cannot carry state, so the sender lives in a process-wide slot the provider reads.
struct CapturingSink {
    compaction_begin: UnboundedSender<u64>,
}

impl GcEventSink for CapturingSink {
    fn eviction_begin(&self, _target_fragments: u64) {}
    fn eviction_progress(&self, _evicted: u64) {}
    fn eviction_end(&self, _total_evicted: u64) {}

    fn compaction_begin(&self, target_bytes: u64) {
        let _ = self.compaction_begin.send(target_bytes);
    }

    fn compaction_progress(&self, _compacted_bytes: u64) {}
    fn compaction_end(&self, _total_compacted_bytes: u64) {}
}

static SINK: OnceLock<Arc<CapturingSink>> = OnceLock::new();

fn capturing_sink() -> Option<GcEventSinkRef> {
    SINK.get().map(|sink| sink.clone() as GcEventSinkRef)
}

/// Content-addressed fragment `index`, distinct from every other.
fn fragment(index: u16) -> (Address, Fragment, Bytes) {
    let payload = Bytes::from(index.to_le_bytes().repeat(FRAGMENT_SIZE / 2));
    let address = Address {
        hash: lore_storage::hash_slice(payload.as_ref()),
        context: Context::from([0u8; 16]),
    };
    let fragment = Fragment {
        flags: 0,
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };
    (address, fragment, payload)
}

/// Fill a store at `path` and leave its packfiles on disk.
async fn seed_store(path: &std::path::Path, partition: Partition) {
    let store =
        LocalImmutableStore::new(Some(path.to_path_buf()), ImmutableStoreSettings::default())
            .await
            .expect("store opens");
    let dyn_store: Arc<dyn ImmutableStore> = store.clone();
    for index in 0..FRAGMENT_COUNT {
        let (address, fragment, payload) = fragment(index);
        dyn_store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put succeeds");
    }
    dyn_store.flush(true).await.expect("flush succeeds");
}

/// Loading a store that is over its size cap starts a compaction pass on its own.
///
/// The store is filled, closed, and reopened under a cap far below what it holds. Resuming the
/// packstores reports the loaded bytes to the GC counters, which cross the cap and fire the
/// pass. The test returns as soon as the pass reports, and fails if it has not within
/// [`REPORT_DEADLINE`].
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loading_a_store_over_its_size_cap_fires_compaction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let partition = Partition::from([0x21u8; 16]);
    seed_store(dir.path(), partition).await;

    let (sender, mut compaction_begins) = unbounded_channel();
    SINK.set(Arc::new(CapturingSink {
        compaction_begin: sender,
    }))
    .ok()
    .expect("sink slot is unset");
    lore_storage::gc_event::set_gc_event_sink_provider(capturing_sink);

    // Held for the whole test: the trigger upgrades a weak reference to the store, and the
    // pass stops when the last strong one goes.
    let store = LocalImmutableStore::new(
        Some(dir.path().to_path_buf()),
        ImmutableStoreSettings::default(),
    )
    .await
    .expect("store reopens");
    store.set_gc_caps(TINY_MAX_SIZE, usize::MAX, false);

    let loaded = store.packstore_total_size().await;
    assert!(
        loaded > TINY_MAX_SIZE,
        "the reopened store must load more than its cap, loaded {loaded} against {TINY_MAX_SIZE}"
    );

    let target_bytes = tokio::time::timeout(REPORT_DEADLINE, compaction_begins.recv())
        .await
        .expect("the load-driven trigger should fire a compaction pass")
        .expect("the sink outlives the pass");
    assert_eq!(target_bytes, TINY_MAX_SIZE as u64);
}
