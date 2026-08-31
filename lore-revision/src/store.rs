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
