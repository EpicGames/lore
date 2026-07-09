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
use lore_revision::event::EventError;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreString;
use lore_revision::lore::execution_context;
use lore_revision::repository;
use lore_revision::store::event::LoreStorageOpenedEventData;
use lore_revision::util::path::make_absolute;
use lore_storage::MutableStore;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use lore_storage::local::immutable_store::ImmutableStoreSettings;
use lore_storage::local::immutable_store::create as create_immutable;
use lore_storage::local::mutable_store::LocalMutableStore;
use lore_storage::local::mutable_store::MutableStoreSettings;
use serde::Deserialize;
use serde::Serialize;

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
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
pub struct LoreStorageRemoteConfig {
    /// gRPC endpoint of the peer storage service; authenticated with the open call's `globals.identity`
    pub remote_url: LoreString,
}

/// Arguments for `lore_storage_open`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
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
//   has_remote_config: u8 + 7-byte tail pad               →  8 bytes
//   cache_target_bytes: u64                               →  8 bytes
//   cache_target_fragments: u64                           →  8 bytes
//                                                  total  → 64 bytes
const _: () = assert!(std::mem::size_of::<LoreStorageOpenArgs>() == 64);

/// Default soft cap on total bytes held in the immutable store when `gc=1` and the caller
/// passes `cache_target_bytes = 0`.
const DEFAULT_CACHE_TARGET_BYTES: usize = 1 << 30;

/// Default soft cap on fragment count when `gc=1` and the caller passes
/// `cache_target_fragments = 0`.
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

#[error_set]
enum OpenError {
    InvalidArguments,
}

impl EventError for OpenError {
    fn translated(&self) -> LoreError {
        match self {
            OpenError::InvalidArguments(_) => LoreError::InvalidArguments,
            OpenError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
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

async fn open_local(
    globals: LoreGlobalArgs,
    args: LoreStorageOpenArgs,
    callback: LoreEventCallback,
) -> i32 {
    no_repository_call(globals, callback, args, open, async move |args| {
        let path = args.repository_path.as_str();
        let in_memory = args.in_memory != 0;

        let bound_flags = BoundFlags::try_from_globals(execution_context().globals())?;
        // Bound `remote=1` without a `remote_config` produces a silently-broken handle —
        // every read misses local then finds no remote. Reject up front.
        if bound_flags.remote && args.has_remote_config == 0 {
            return Err(OpenError::from(InvalidArguments {
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
                .internal("creating in-memory immutable store")?;
                lore_storage::maintenance::spawn_gc(&immutable, &create_options);
                let mutable: Arc<dyn MutableStore> = Arc::new(
                    LocalMutableStore::new(
                        Option::<&std::path::Path>::None,
                        MutableStoreSettings::default(),
                        immutable.clone(),
                    )
                    .await
                    .internal("creating in-memory mutable store")?,
                );
                (immutable, mutable)
            }
            (false, false) => {
                // Canonicalize for cache-key consistency, but fall back to the raw path on
                // canonicalize failure so the dotpath check below surfaces the real error.
                let absolute = make_absolute(path).unwrap_or_else(|_| PathBuf::from(path));
                let dot_dir = repository::RepositoryFormat::detect(&absolute).dot_dir();
                let dotpath = absolute.join(dot_dir);
                // Without this guard, `load_repository_config` would return defaults and
                // `LocalImmutableStore` would create the directory tree, silently fabricating
                // a fresh repo on any path.
                if !dotpath.is_dir() {
                    return Err(OpenError::from(InvalidArguments {
                        reason: format!(
                            "no lore repository at {} (missing {})",
                            absolute.display(),
                            dot_dir
                        ),
                    }));
                }
                let config = repository::load_repository_config(&absolute)
                    .internal("loading repository config")?;
                let immutable = repository::create_client_immutable_store(
                    &config,
                    &dotpath,
                    create_options,
                    false,
                )
                .await
                .internal("opening immutable store")?;
                let mutable: Arc<dyn MutableStore> =
                    repository::create_client_mutable_store(&config, &dotpath, immutable.clone())
                        .await
                        .internal("opening mutable store")?;
                (immutable, mutable)
            }
            _ => {
                return Err(OpenError::from(InvalidArguments {
                    reason: "`repository_path` non-empty requires `in_memory == 0`; \
                             `repository_path` empty requires `in_memory == 1`"
                        .into(),
                }));
            }
        };

        let remote = if args.has_remote_config != 0 {
            let url = args.remote_config.remote_url.as_str();
            if url.is_empty() {
                return Err(OpenError::from(InvalidArguments {
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
        ));
        let handle = handle::register(store);
        LoreEvent::StorageOpened(LoreStorageOpenedEventData {
            handle_id: handle.handle_id,
        })
        .send();
        Ok::<(), OpenError>(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_zero_targets_yield_no_evictor_or_compactor() {
        let options = build_create_options(0, 0);
        assert!(options.max_capacity.is_none());
        assert!(options.max_size.is_none());
    }

    #[test]
    fn explicit_targets_pass_through() {
        let options = build_create_options(512, 16);
        assert_eq!(options.max_size, Some(512));
        assert_eq!(options.max_capacity, Some(16));
    }

    #[test]
    fn one_zero_field_only_defaults_that_field() {
        let bytes_only = build_create_options(4096, 0);
        assert_eq!(bytes_only.max_size, Some(4096));
        assert_eq!(
            bytes_only.max_capacity,
            Some(DEFAULT_CACHE_TARGET_FRAGMENTS),
        );
        let frags_only = build_create_options(0, 32);
        assert_eq!(frags_only.max_size, Some(DEFAULT_CACHE_TARGET_BYTES));
        assert_eq!(frags_only.max_capacity, Some(32));
    }

    /// Sub-floor `cache_target_fragments` must surface a warn-level log so the operator can
    /// see the misconfiguration. This is the smallest behavioral observable proving the
    /// target reaches the evictor wiring; deterministic eviction would require driving the
    /// evictor's internal floor (`1 << 20` fragments), which is infeasible in a unit test.
    /// The test installs a `fn`-pointer log callback that toggles a static flag when the
    /// expected message lands.
    #[test]
    fn below_floor_emits_warn() {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        static SAW_WARN: AtomicBool = AtomicBool::new(false);

        fn capture(level: lore_base::log::LoreLogLevel, _location: &str, message: &str) {
            if level == lore_base::log::LoreLogLevel::Warn
                && message.contains("below the evictor's internal floor")
            {
                SAW_WARN.store(true, Ordering::Release);
            }
        }

        let prev_level = lore_base::log::log_level();
        lore_base::log::set_log_level(lore_base::log::LoreLogLevel::Warn);
        lore_base::log::set_log_callback(Some(capture));
        SAW_WARN.store(false, Ordering::Release);

        let options = build_create_options(0, 4);

        // Restore the previous logger state regardless of the assert outcome.
        lore_base::log::set_log_callback(None);
        lore_base::log::set_log_level(prev_level);

        assert_eq!(options.max_capacity, Some(4));
        assert!(
            SAW_WARN.load(Ordering::Acquire),
            "sub-floor cache_target_fragments must emit a warn log",
        );
    }
}
