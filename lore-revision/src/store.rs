// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod composite;
pub mod event;
pub mod handles;
pub mod immutable;
pub mod mutable;
pub mod remote;
#[cfg(feature = "seeding")]
pub mod seeder;
pub mod write_discarding;

// Re-export store types and traits from lore-storage for internal use
pub(crate) use lore_base::types::KeyType;
pub(crate) use lore_storage::ImmutableStore;
pub(crate) use lore_storage::KeyValueStream;
pub(crate) use lore_storage::MutableStore;
pub(crate) use lore_storage::StoreError;
pub(crate) use lore_storage::StoreGetData;
pub(crate) use lore_storage::StoreMatch;
pub(crate) use lore_storage::StoreMatchResult;
pub(crate) use lore_storage::StoreObliterateStats;
pub(crate) use lore_storage::immutable_store::MatchedStoreError;
pub(crate) use lore_storage::immutable_store::query_one;
// Re-export maintenance functions (used as crate::store::gc etc.)
pub(crate) use lore_storage::maintenance::gc;

/// Whether this process keeps its client stores in memory after each call, without their
/// claims, and reuses their state at the next call when the store's epoch is unchanged:
/// `LORE_STORE_RETAIN` set to `1` or `true`. Read once per process.
pub fn retain_stores() -> bool {
    static RETAIN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *RETAIN.get_or_init(|| {
        std::env::var("LORE_STORE_RETAIN")
            .is_ok_and(|var| var == "1" || var.eq_ignore_ascii_case("true"))
    })
}
