// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_open` — acquire a handle to a content-addressed store.
//!
//! Two modes:
//! - **Disk-backed** (`repository_path` non-empty, `in_memory == 0`): constructs backend Arcs
//!   via `lore_revision::repository::create_client_immutable_store` / `create_client_mutable_store`,
//!   routing through the path-keyed cache in `lore_revision`. Two handles on the same path
//!   share one underlying `Arc<dyn ImmutableStore>` and its evictor.
//! - **In-memory** (`repository_path` empty, `in_memory == 1`): calls
//!   `LocalImmutableStore::new(None, ...)` + `LocalMutableStore::new(None, ...)` directly. Each
//!   open produces an independent backend pair — no cache.
//!
//! In either mode, the returned backend Arcs are wrapped in a fresh per-handle `StoreInternal`.
//! When `args.has_remote_config != 0`, a `RemoteEndpoint` keyed by the handle's identity is
//! also constructed and stashed on the `StoreInternal` for ops that consult a peer service.
//! Construction does not open the gRPC connection; the connection is established lazily by the
//! first session lookup. The handle is registered and delivered via `LORE_EVENT_STORAGE_OPENED`
//! before `Complete`.

use std::path::PathBuf;
use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreString;
use lore_revision::lore::execution_context;
use lore_revision::repository;
use lore_revision::repository::get_dot_lore_path;
use lore_revision::store::event::LoreStorageOpenedEventData;
use lore_revision::util::path::make_absolute;
use lore_storage::MutableStore;
use lore_storage::StorageError;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use lore_storage::local::immutable_store::ImmutableStoreSettings;
use lore_storage::local::immutable_store::create as create_immutable;
use lore_storage::local::mutable_store::LocalMutableStore;
use lore_storage::local::mutable_store::MutableStoreSettings;

use crate::call::no_repository_call;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::handle;
use crate::storage::remote::RemoteEndpoint;
use crate::storage::store::BoundFlags;
use crate::storage::store::StoreInternal;

/// Remote endpoint configuration for a storage handle.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, ValidateText, bitcode::Encode, bitcode::Decode)]
pub struct LoreStorageRemoteConfig {
    /// gRPC endpoint of the peer storage service; authenticated with the open call's `globals.identity`
    pub remote_url: LoreString,
}

/// Arguments for `lore_storage_open`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(open_local)]
pub struct LoreStorageOpenArgs {
    /// Path to an existing lore repository; must be empty when `in_memory` is set
    pub repository_path: LoreString,
    /// Open a fresh in-memory store; `repository_path` must then be empty
    pub in_memory: u8,
    /// Remote endpoint binding for ops that consult a peer; honored only when `has_remote_config` is set
    pub remote_config: LoreStorageRemoteConfig,
    /// Activate `remote_config`; otherwise the handle has no remote
    pub has_remote_config: u8,
    /// Skip re-hashing a loaded payload and checking it against the address it was read from.
    ///
    /// Zero keeps the check, which is the default: a store handing back bytes under a content
    /// address should be able to say they are the bytes that address names. A caller whose own
    /// layer already assures integrity - one that scrubs its store on a schedule, say - pays for
    /// the check on every byte of every read and learns nothing new from it, and can set this.
    ///
    /// Applies to every read on the handle.
    pub skip_verify: u8,
    /// Soft cap on total immutable-store bytes (compactor target). A non-zero cache target enables
    /// incremental background GC for the handle; `0` then selects the default. Shared disk backends
    /// inherit the first opener's value
    pub cache_target_bytes: u64,
    /// Soft cap on immutable-store fragment count (evictor target). A non-zero cache target enables
    /// incremental background GC for the handle; `0` then selects the default
    pub cache_target_fragments: u64,
}

// Lock the C ABI layout of `LoreStorageOpenArgs`. The struct is exposed via cbindgen and
// callers must zero-initialize it (`= {0}` in C). Re-ordering existing fields breaks ABI
// compatibility; appending new fields at the tail is safe IF the caller zero-initialized.
// This assertion catches accidental field re-ordering at compile time.
//
// Layout on a 64-bit target with `#[repr(C)]`:
//   repository_path: LoreString { ptr, len }              → 16 bytes
//   in_memory: u8 + 7-byte tail pad                       →  8 bytes
//   remote_config: LoreStorageRemoteConfig { LoreString } → 16 bytes
//   has_remote_config: u8, skip_verify: u8 + 6-byte pad   →  8 bytes
//   cache_target_bytes: u64                               →  8 bytes
//   cache_target_fragments: u64                           →  8 bytes
//                                                  total  → 64 bytes
const _: () = assert!(std::mem::size_of::<LoreStorageOpenArgs>() == 64);

/// Default soft cap on total bytes held in the immutable store when `gc=1` and the caller
/// passes `cache_target_bytes = 0`.
#[lore_macro::test_pub]
const DEFAULT_CACHE_TARGET_BYTES: usize = 1 << 30;

/// Default soft cap on fragment count when `gc=1` and the caller passes
/// `cache_target_fragments = 0`.
#[lore_macro::test_pub]
const DEFAULT_CACHE_TARGET_FRAGMENTS: usize = 1 << 20;

/// Internal floor the evictor enforces in `lore-storage`. Targets below this are silently
/// raised by the evictor; the open path warns the caller so a misconfiguration doesn't go
/// unnoticed.
const EVICTOR_MIN_CAPACITY: usize = 1 << 20;

/// Build the `ImmutableStoreCreateOptions` for a handle from the caller's cache targets.
/// Incremental background GC (evictor + compactor) is opt-in per handle: with both targets
/// `0` the result is `none()` — no evictor or compactor spawn. When either target is non-zero,
/// GC is enabled and a `0` field resolves to the built-in default; a non-zero
/// `cache_target_fragments` below the evictor's internal floor (`EVICTOR_MIN_CAPACITY`) is
/// passed through but logged at `warn` so the caller knows the effective cap is the floor, not
/// the value they asked for.
#[lore_macro::test_pub]
fn build_create_options(
    cache_target_bytes: u64,
    cache_target_fragments: u64,
) -> ImmutableStoreCreateOptions {
    if cache_target_bytes == 0 && cache_target_fragments == 0 {
        return ImmutableStoreCreateOptions::none();
    }
    let max_size = if cache_target_bytes == 0 {
        DEFAULT_CACHE_TARGET_BYTES
    } else {
        cache_target_bytes as usize
    };
    let max_capacity = if cache_target_fragments == 0 {
        DEFAULT_CACHE_TARGET_FRAGMENTS
    } else {
        cache_target_fragments as usize
    };
    if cache_target_fragments != 0 && (cache_target_fragments as usize) < EVICTOR_MIN_CAPACITY {
        lore_base::lore_warn!(
            "cache_target_fragments={cache_target_fragments} is below the evictor's internal floor of {EVICTOR_MIN_CAPACITY}; the effective fragment cap will be the floor"
        );
    }
    ImmutableStoreCreateOptions {
        max_capacity: Some(max_capacity),
        eviction_delay: None,
        max_size: Some(max_size),
        compaction_delay: None,
    }
}

/// Acquire a handle to a content-addressed store.
///
/// On success the caller receives `LORE_EVENT_STORAGE_OPENED` carrying
/// `{handle}` before `Complete` with `status` `0`. On failure, no
/// `STORAGE_OPENED` and no `LORE_EVENT_ERROR` fire; `Complete` carries the
/// error code in `status` and the full detail in its `error` field.
pub async fn open(
    globals: LoreGlobalArgs,
    args: LoreStorageOpenArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, open_local).await
}

fn open_local(
    globals: LoreGlobalArgs,
    args: LoreStorageOpenArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    no_repository_call(globals, callback, args, open, async move |args| {
        let path = args.repository_path.as_str();
        let in_memory = args.in_memory != 0;

        let bound_flags = BoundFlags::try_from_globals(execution_context().globals())?;
        // Bound `remote=1` without a `remote_config` produces a silently-broken handle —
        // every read misses local then finds no remote. Reject up front.
        if bound_flags.remote && args.has_remote_config == 0 {
            return Err(StorageError::from(InvalidArguments {
                reason: "`globals.remote=1` requires `has_remote_config != 0`".into(),
            }));
        }
        let create_options = if execution_context().globals().no_gc() {
            ImmutableStoreCreateOptions::none()
        } else {
            build_create_options(args.cache_target_bytes, args.cache_target_fragments)
        };

        let identity = execution_context()
            .globals()
            .identity()
            .unwrap_or("")
            .to_owned();

        let (immutable, mutable) = match (path.is_empty(), in_memory) {
            (true, true) => {
                let immutable = create_immutable(
                    Option::<PathBuf>::None,
                    create_options,
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .forward_any::<StorageError>("creating in-memory immutable store")?;
                lore_storage::maintenance::spawn_gc(&immutable, &create_options);
                let mutable: Arc<dyn MutableStore> = Arc::new(
                    LocalMutableStore::new(
                        Option::<&std::path::Path>::None,
                        MutableStoreSettings::default(),
                        immutable.clone(),
                    )
                    .await
                    .forward::<StorageError>("creating in-memory mutable store")?,
                );
                (immutable, mutable)
            }
            (false, false) => {
                // Canonicalize for cache-key consistency, but fall back to the raw path on
                // canonicalize failure so the dotpath check below surfaces the real error.
                let absolute = make_absolute(path).unwrap_or_else(|_| PathBuf::from(path));
                let dotpath = get_dot_lore_path(&absolute).map_err(|_err| {
                    StorageError::from(InvalidArguments {
                        reason: format!(
                            "unable to find .lore directory for repository at {}",
                            absolute.display(),
                        ),
                    })
                })?;
                // Without this guard, `load_repository_config` would return defaults and
                // `LocalImmutableStore` would create the directory tree, silently fabricating
                // a fresh repo on any path.
                if !dotpath.is_dir() {
                    return Err(StorageError::from(InvalidArguments {
                        reason: format!(
                            "no lore repository at {} (missing {})",
                            absolute.display(),
                            dotpath.display()
                        ),
                    }));
                }
                let config = repository::load_repository_config(&absolute)
                    .forward_any::<StorageError>("loading repository config")?;
                let immutable = repository::create_client_immutable_store(
                    &config,
                    &dotpath,
                    create_options,
                    false,
                )
                .await
                .forward_any::<StorageError>("opening immutable store")?;
                let mutable: Arc<dyn MutableStore> =
                    repository::create_client_mutable_store(&config, &dotpath, immutable.clone())
                        .await
                        .forward_any::<StorageError>("opening mutable store")?;
                (immutable, mutable)
            }
            _ => {
                return Err(StorageError::from(InvalidArguments {
                    reason: "`repository_path` non-empty requires `in_memory == 0`; \
                             `repository_path` empty requires `in_memory == 1`"
                        .into(),
                }));
            }
        };

        let remote = if args.has_remote_config != 0 {
            let url = args.remote_config.remote_url.as_str();
            if url.is_empty() {
                return Err(StorageError::from(InvalidArguments {
                    reason: "`remote_config.remote_url` must be non-empty when \
                             `has_remote_config != 0`"
                        .into(),
                }));
            }
            let identity_for_remote = if identity.is_empty() {
                None
            } else {
                Some(identity.as_str())
            };
            Some(Arc::new(RemoteEndpoint::new(url, identity_for_remote)))
        } else {
            None
        };

        let store = Arc::new(StoreInternal::new(
            identity,
            immutable,
            mutable,
            remote,
            bound_flags,
            args.skip_verify != 0,
        ));
        let handle = handle::register(store);
        LoreEvent::StorageOpened(LoreStorageOpenedEventData {
            handle_id: handle.handle_id,
        })
        .send();
        Ok::<(), StorageError>(())
    })
}
