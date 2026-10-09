// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::num::NonZeroU64;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

// Re-export all immutable store types from lore-storage for internal use
pub(crate) use lore_storage::local::immutable_store::*;

// Backward compatibility alias
pub(crate) type ImmutableStore = lore_storage::local::immutable_store::LocalImmutableStore;

/// Seconds after a write that an immutable disk store this process opens flushes its index in
/// the background; 0, the default, for none.
static BACKGROUND_FLUSH_SECONDS: AtomicU64 = AtomicU64::new(0);

/// Makes every immutable disk store this process opens from now on flush its index `seconds` after
/// a write, durable writes included. A durable write otherwise schedules no flush, so a crash loses every
/// index entry written since the last explicit flush.
pub fn set_background_flush_delay(seconds: NonZeroU64) {
    BACKGROUND_FLUSH_SECONDS.store(seconds.get(), Ordering::Relaxed);
}

/// The settings a client disk store opens with: local fragments protected from eviction, writes
/// verified as `verify_write` says, and the background flush [`set_background_flush_delay`] set.
pub(crate) fn client_settings(verify_write: bool) -> ImmutableStoreSettings {
    let seconds = BACKGROUND_FLUSH_SECONDS.load(Ordering::Relaxed);
    let defaults = ImmutableStoreSettings::default();
    ImmutableStoreSettings {
        protect_local_fragment: true,
        verify_write,
        flush_background: seconds > 0,
        flush_delay_seconds: if seconds > 0 {
            seconds
        } else {
            defaults.flush_delay_seconds
        },
        ..defaults
    }
}
