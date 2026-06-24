// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Per-item event data for the content-addressed storage API.
//!
//! Each put/get/copy/obliterate/query/upload operation terminates an item
//! with an `*_ITEM_COMPLETE` event. `get` additionally emits a `HEADER` and
//! one or more `DATA` events for each item before the terminal. `open`
//! emits a single `OPENED` event on success before `Complete`.
//!
//! All event-data structs here are `#[repr(C)]` PODs carrying the item's
//! correlation `id`, the relevant addresses/partitions, and a
//! [`LoreErrorCode`] discriminator. The companion `LORE_EVENT_ERROR` event
//! (emitted alongside per-item failures) carries the human-readable detail.

use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::Partition;
use serde::Deserialize;
use serde::Serialize;

use crate::event::LoreBytes;
use crate::event::LoreErrorCode;

/// Delivered on successful `lore_storage_open`. Carries the handle id the
/// caller must pass to subsequent ops against this store.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageOpenedEventData {
    /// Handle id for the opened store.
    pub handle_id: u64,
}

/// Terminal per-item event for `put` and `put_file`. On success
/// `error_code == None` and `address` is the computed content hash; on
/// failure `error_code` is populated and `address` is zero.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStoragePutItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The computed content address of the stored item.
    pub address: Address,
    /// The outcome for the item.
    pub error_code: LoreErrorCode,
}

/// Leading event for each regular `get` item. Reports the total
/// reassembled content size before any `GET_DATA` events arrive.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageGetHeaderEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The content address of the item.
    pub address: Address,
    /// The total reassembled content size in bytes.
    pub size_content: u64,
}

/// Per-fragment (or single-buffer) payload event for `get`. The `bytes`
/// view is valid only during the callback invocation.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageGetDataEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The content address of the item.
    pub address: Address,
    /// The byte offset of this payload within the item's content.
    pub offset: u64,
    /// The payload bytes for this part of the item.
    pub bytes: LoreBytes,
}

/// Terminal per-item event for `get` and `get_file`. For `get_file` this
/// is emitted without any preceding `HEADER`/`DATA` events — the payload
/// is written directly to the filesystem.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageGetItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The content address of the item.
    pub address: Address,
    /// The outcome for the item.
    pub error_code: LoreErrorCode,
}

/// Terminal per-item event for `copy`. `source_partition` /
/// `target_partition` disambiguate the per-item source and target. The item's content hash is
/// preserved across the copy so only `source_address` is carried; `target_context` is the
/// destination tuple's context — the destination address is `(target_partition,
/// source_address.hash, target_context)`.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageCopyItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The partition the item was copied from.
    pub source_partition: Partition,
    /// The partition the item was copied to.
    pub target_partition: Partition,
    /// The address of the item in the source.
    pub source_address: Address,
    /// The context of the item in the target.
    pub target_context: Context,
    /// The outcome for the item.
    pub error_code: LoreErrorCode,
}

/// Terminal per-item event for `obliterate`. `local_success` / `remote_success` report
/// whether the corresponding side completed without error. `local_skipped` / `remote_skipped`
/// report whether the corresponding side was suppressed up front by the handle's bound flags
/// (`globals.offline`/`local`/`remote`) — when a side is skipped, its `_success` flag is `0`
/// rather than a misleading `1`. `error_code` is populated if either side that DID run
/// failed.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageObliterateItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The content address of the item.
    pub address: Address,
    /// 1 when the local side completed without error.
    pub local_success: u8,
    /// 1 when the remote side completed without error.
    pub remote_success: u8,
    /// 1 when the local side was skipped.
    pub local_skipped: u8,
    /// 1 when the remote side was skipped.
    pub remote_skipped: u8,
    /// The outcome for the item.
    pub error_code: LoreErrorCode,
}

/// Terminal per-item event for `get_metadata`. On success `fragment` is
/// valid and `error_code == None`; on miss `error_code == ADDRESS_NOT_FOUND`.
/// Mirrors `LoreStorageGetItemCompleteEventData`'s shape minus the absence of
/// any preceding `GET_HEADER` / `GET_DATA` events — `get_metadata` carries no
/// payload bytes.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageGetMetadataItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The content address of the item.
    pub address: Address,
    /// The metadata fragment for the item.
    pub fragment: Fragment,
    /// The outcome for the item.
    pub error_code: LoreErrorCode,
}

/// Terminal per-item event for `upload`. `already_durable` is 1 when the
/// item was already flagged durable and no upload was performed.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageUploadItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The content address of the item.
    pub address: Address,
    /// 1 when the item was already durable and no upload was performed.
    pub already_durable: u8,
    /// The outcome for the item.
    pub error_code: LoreErrorCode,
}

/// Terminal per-item event for `mutable_load`. On success `error_code == None` and `value` is
/// the loaded value hash (`Hash::default()` when the key holds a null/removed value); on miss
/// `error_code == ADDRESS_NOT_FOUND` and `value` is zero.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageMutableLoadItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The value stored for the key.
    pub value: Hash,
    /// The outcome for the item.
    pub error_code: LoreErrorCode,
}

/// Terminal per-item event for `mutable_store`. `error_code == None` on a successful store.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageMutableStoreItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The outcome for the item.
    pub error_code: LoreErrorCode,
}

/// Terminal per-item event for `mutable_compare_and_swap`. `previous` is the value the key held
/// before the swap (equal to the caller's `expected` when the swap took effect, otherwise the
/// actual current value). `error_code == None` on success.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageMutableCompareAndSwapItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The value the key held before the swap.
    pub previous: Hash,
    /// The outcome for the item.
    pub error_code: LoreErrorCode,
}

/// One `(key, value)` pair emitted by `mutable_list`, before the item's terminal event.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageMutableListEntryEventData {
    /// Correlation id of the listing item.
    pub id: u64,
    /// The key of this entry.
    pub key: Hash,
    /// The value stored for the key.
    pub value: Hash,
}

/// Terminal per-item event for `mutable_list`, emitted after every `MUTABLE_LIST_ENTRY` for the
/// item. `error_code == None` once the listing completes.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageMutableListItemCompleteEventData {
    /// Correlation id of the listing item.
    pub id: u64,
    /// The outcome for the item.
    pub error_code: LoreErrorCode,
}
