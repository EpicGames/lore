// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Per-item event data for the content-addressed storage API.
//!
//! Each put/get/copy/obliterate/query/upload operation terminates an item
//! with an `*_ITEM_COMPLETE` event. `get` additionally emits a `HEADER` and
//! one or more `DATA` events for each item before the terminal. `open`
//! emits a single `OPENED` event on success before `Complete`.
//!
//! All event-data structs here are `#[repr(C)]` structs carrying the item's
//! correlation `id`, the relevant addresses/partitions, and a
//! [`LoreErrorDetail`] holding the failing error's own code, message and trace.
//!
//! The detail owns heap data, so these structs are `Clone` rather than `Copy`,
//! and the pointers it carries are valid only for the callback invocation that
//! delivers the event.

use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::Partition;
use serde::Deserialize;
use serde::Serialize;

use crate::event::LoreBytes;
use crate::event::LoreErrorDetail;

/// Delivered on successful `lore_storage_open`. Carries the handle id the
/// caller must pass to subsequent ops against this store.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageOpenedEventData {
    /// Handle id for the opened store.
    pub handle_id: u64,
}

/// Terminal per-item event for `put`, `put_file`, `put_resolved` and
/// `put_file_resolved`. On success `error.error_code == 0` and `address` is the
/// computed content hash — for the resolved variants, the content the key now
/// resolves to; on failure `error` is populated and `address` is zero.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStoragePutItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The computed content address of the stored item.
    pub address: Address,
    /// The outcome for the item.
    pub error: LoreErrorDetail,
    /// Non-zero when the local store holds the content. Trailing, so a payload that lacks it still
    /// decodes: the IPC wire format is non-self-describing, where only a missing trailing field is
    /// recoverable.
    #[serde(default)]
    pub stored_local: u8,
    /// Non-zero when the content reached the remote, or was already durable there. A remote
    /// write that fails still reports success if the local write succeeded — this is how a
    /// caller tells the two apart. For fragmented content it is the intersection across every
    /// fragment, so it is set only when the whole tree is remote.
    #[serde(default)]
    pub stored_remote: u8,
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

/// Terminal per-item event for `get`, `get_file`, `get_resolved` and
/// `get_file_resolved`. For the two file variants this is emitted without any
/// preceding `HEADER`/`DATA` events — the payload is written directly to the
/// filesystem. For the two resolved variants `address` is the address the key
/// resolved to, so it is an output rather than an echo of the request.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageGetItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The content address of the item.
    pub address: Address,
    /// The outcome for the item.
    pub error: LoreErrorDetail,
}

/// Terminal per-item event for `copy`. `source_partition` /
/// `target_partition` disambiguate the per-item source and target. The item's content hash is
/// preserved across the copy so only `source_address` is carried; `target_context` is the
/// destination tuple's context — the destination address is `(target_partition,
/// source_address.hash, target_context)`.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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
    pub error: LoreErrorDetail,
}

/// Terminal per-item event for `obliterate`. `local_success` / `remote_success` report
/// whether the corresponding side completed without error. `local_skipped` / `remote_skipped`
/// report whether the corresponding side was suppressed up front by the handle's bound flags
/// (`globals.offline`/`local`/`remote`) — when a side is skipped, its `_success` flag is `0`
/// rather than a misleading `1`. `error` is populated if either side that DID run failed.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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
    pub error: LoreErrorDetail,
}

/// Terminal per-item event for `get_metadata`. On success `fragment` is valid and
/// `error.error_code == 0`; on miss `error` carries the address-not-found error. Mirrors
/// `LoreStorageGetItemCompleteEventData`'s shape minus the absence of any preceding
/// `GET_HEADER` / `GET_DATA` events — `get_metadata` carries no payload bytes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageGetMetadataItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The content address of the item.
    pub address: Address,
    /// The metadata fragment for the item.
    pub fragment: Fragment,
    /// The outcome for the item.
    pub error: LoreErrorDetail,
}

/// Terminal per-item event for `upload`. `already_durable` is 1 when the
/// item was already flagged durable and no upload was performed.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageUploadItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The content address of the item.
    pub address: Address,
    /// 1 when the item was already durable and no upload was performed.
    pub already_durable: u8,
    /// The outcome for the item.
    pub error: LoreErrorDetail,
}

/// Terminal per-item event for `mutable_load`. On success `error.error_code == 0` and `value`
/// is the loaded value hash (`Hash::default()` when the key holds a null/removed value); on
/// miss `error` carries the miss the answering backend raised — `AddressNotFound` from a local
/// store, `NotFound` from a remote one — and `value` is zero.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageMutableLoadItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The value stored for the key.
    pub value: Hash,
    /// The outcome for the item.
    pub error: LoreErrorDetail,
}

/// Terminal per-item event for `mutable_store`. `error.error_code == 0` on a successful store.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageMutableStoreItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The outcome for the item.
    pub error: LoreErrorDetail,
}

/// Terminal per-item event for `mutable_compare_and_swap`. `previous` is the value the key held
/// before the swap (equal to the caller's `expected` when the swap took effect, otherwise the
/// actual current value). `error.error_code == 0` on success.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageMutableCompareAndSwapItemCompleteEventData {
    /// Correlation id of the item.
    pub id: u64,
    /// The value the key held before the swap.
    pub previous: Hash,
    /// The outcome for the item.
    pub error: LoreErrorDetail,
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
/// item. `error.error_code == 0` once the listing completes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreStorageMutableListItemCompleteEventData {
    /// Correlation id of the listing item.
    pub id: u64,
    /// The outcome for the item.
    pub error: LoreErrorDetail,
}
