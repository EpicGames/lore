// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;

use lore_error_set::WrapInternal;
use lore_macro::LoreArgs;
use lore_revision::error::LoreErrorExt;
use lore_revision::global::GlobalConfig;
use lore_revision::shared_store::LoreSharedStoreInfoEventData;
use lore_revision::shared_store::SharedStoreError;
use lore_revision::shared_store::find_existing_shared_store_in_dir;
use serde::Deserialize;
use serde::Serialize;

use crate::call::no_repository_call;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreArray;
use crate::interface::LoreEvent;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::interface::LoreString;

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(create_local)]
/// Arguments for creating a new shared store.
pub struct LoreSharedStoreCreateArgs {
    /// Remote URL backing the store
    pub remote_url: LoreString,

    /// Path where the store will be created; empty string uses the default location
    pub path: LoreString,

    /// Set this as the default shared store in the global config
    pub make_default: u8,
}

/// Creates a new shared store at the specified path
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Shared Store Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::GlobalStoreCreate`](crate::interface::LoreEvent::GlobalStoreCreate) | Emitted on success with the path of the newly created store |
pub async fn create(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreCreateArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, create_local).await
}

async fn create_local(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreCreateArgs,
    callback: LoreEventCallback,
) -> i32 {
    no_repository_call(globals, callback, args, create, async move |args| {
        let path = if args.path.as_str() == "" {
            None
        } else {
            Some(PathBuf::from(args.path.to_string()))
        };

        let raw_remote_url = args.remote_url.as_str();
        let remote_url = raw_remote_url
            .strip_suffix("/")
            .unwrap_or(raw_remote_url)
            .to_owned();

        match lore_revision::shared_store::create_shared_store(
            path,
            remote_url,
            args.make_default != 0,
        )
        .await
        {
            Ok(result) => Ok(result),
            Err(e) => e.emit(),
        }
    })
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(info_local)]
/// Arguments for querying the configured default shared store (no parameters).
pub struct LoreSharedStoreInfoArgs {}

/// Returns information about the configured default shared store
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Shared Store Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::SharedStoreInfo`](crate::interface::LoreEvent::SharedStoreInfo) | Emitted on success with the path of the configured default shared store |
pub async fn info(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, info_local).await
}

async fn info_local(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    let command = async move |_args| -> Result<(), SharedStoreError> {
        let config = GlobalConfig::load()
            .await
            .internal("loading global config")?;

        let mut remote_urls = Vec::new();
        let mut shared_store_paths = Vec::new();
        let mut shared_store_exists = Vec::new();
        for (remote_url, config) in config.all_default_shared_stores() {
            let (path, exists) = match find_existing_shared_store_in_dir(&config.path_to_store) {
                Ok(path) => (path, true),
                Err(SharedStoreError::SharedStoreNotFound(e)) => {
                    (PathBuf::from(e.path.clone()), false)
                }
                Err(err) => {
                    return Err(err);
                }
            };
            remote_urls.push(remote_url.clone().into());
            shared_store_paths.push(path.into());
            shared_store_exists.push(exists as u8);
        }
        LoreEvent::SharedStoreInfo(LoreSharedStoreInfoEventData {
            use_automatically: config.use_shared_store_automatically() as u8,
            remote_urls: LoreArray::from_vec(remote_urls),
            paths: LoreArray::from_vec(shared_store_paths),
            exists: LoreArray::from_vec(shared_store_exists),
        })
        .send();
        Ok(())
    };
    no_repository_call(globals, callback, args, info, command).await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(set_use_automatically_local)]
/// Arguments for setting whether to automatically use the shared store.
pub struct LoreSharedStoreSetUseAutomaticallyArgs {
    /// Automatically use the shared store
    pub enabled: u8,
}

/// Sets whether to automatically use the shared store
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`UrcEvent::Log`](crate::interface::UrcEvent::Log) | Diagnostic messages throughout execution |
/// | [`UrcEvent::Complete`](crate::interface::UrcEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`UrcEvent::End`](crate::interface::UrcEvent::End) | Always emitted after `Complete` to signal callback termination |
pub async fn set_use_automatically(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreSetUseAutomaticallyArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, set_use_automatically_local).await
}

async fn set_use_automatically_local(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreSetUseAutomaticallyArgs,
    callback: LoreEventCallback,
) -> i32 {
    let command = async move |_args| -> Result<(), SharedStoreError> {
        let (mut config, lock) = GlobalConfig::load_locked()
            .await
            .internal("loading global config")?;
        if args.enabled != 0 {
            config.use_shared_store_automatically = Some(true);
        } else {
            config.use_shared_store_automatically = None;
        }
        config.save(lock).await.internal("saving global config")?;
        Ok(())
    };
    no_repository_call(globals, callback, args, info, command).await
}
