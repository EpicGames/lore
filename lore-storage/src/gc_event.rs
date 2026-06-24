// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Raw progress sink for store garbage collection.
//!
//! The eviction and compaction passes report lifecycle and progress through
//! this sink using raw values only, so the storage layer carries no user-facing
//! event types. A sink is passed per call to the eviction/compaction entry
//! points (never a global), so passes running in parallel for different stores
//! report to their own observer. The embedder (the lore layer) implements the
//! trait to construct and dispatch the corresponding events.

use std::sync::Arc;
use std::sync::OnceLock;

/// Builds a sink bound to the calling operation's context. Registered once by the
/// embedder (the lore layer) so the storage layer can obtain a correctly-routed sink
/// for an automatically-triggered GC pass without depending on the embedder's types.
pub type GcEventSinkProvider = fn() -> Option<GcEventSinkRef>;

static GC_EVENT_SINK_PROVIDER: OnceLock<GcEventSinkProvider> = OnceLock::new();

/// Register the process-wide sink provider (idempotent; first registration wins).
pub fn set_gc_event_sink_provider(provider: GcEventSinkProvider) {
    let _ = GC_EVENT_SINK_PROVIDER.set(provider);
}

/// A sink bound to the current operation's context, if a provider is registered and a
/// context is active. Called synchronously on the triggering call's stack so the sink
/// routes to that command — correct even when commands run concurrently in one process.
pub fn current_gc_event_sink() -> Option<GcEventSinkRef> {
    GC_EVENT_SINK_PROVIDER.get().and_then(|provider| provider())
}

/// Receives store garbage-collection lifecycle and progress callbacks.
///
/// All methods take raw values; constructing and emitting events is the
/// implementor's responsibility. A pass reports `*_begin` only once it has
/// determined there is work to do (the store is above the limit), one
/// `*_progress` per evicted bucket / compacted group, and `*_end` only on
/// natural completion — an interrupted pass (store dropped mid-step) emits no
/// `*_end`.
pub trait GcEventSink: Send + Sync {
    /// An eviction pass started, targeting `target_fragments` total fragments.
    fn eviction_begin(&self, target_fragments: u64);
    /// `evicted` fragments were dropped from one bucket.
    fn eviction_progress(&self, evicted: u64);
    /// An eviction pass finished, `total_evicted` fragments dropped in total.
    fn eviction_end(&self, total_evicted: u64);
    /// A compaction pass started, targeting `target_bytes` total store size.
    fn compaction_begin(&self, target_bytes: u64);
    /// `compacted_bytes` bytes were reclaimed from one group.
    fn compaction_progress(&self, compacted_bytes: u64);
    /// A compaction pass finished, `total_compacted_bytes` reclaimed in total.
    fn compaction_end(&self, total_compacted_bytes: u64);
}

/// A per-call garbage-collection event sink, shared with the spawned per-group
/// compaction tasks.
pub type GcEventSinkRef = Arc<dyn GcEventSink>;
