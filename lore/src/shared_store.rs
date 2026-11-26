// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;
use std::time::Duration;

use futures::future::Either;
use lore_base::log::LoreLogLevel;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::global::GlobalConfig;
use lore_revision::global::external_dir::locks::Joining;
use lore_revision::global::external_dir::locks::Taken;
use lore_revision::global::external_dir::locks::try_lock_stores;
use lore_revision::instance::InstanceId;
use lore_revision::instance::list_instances;
use lore_revision::repository::RepositoryError;
use lore_revision::repository::StoreConfig;
use lore_revision::shared_store::LoreSharedStoreInfoEventData;
use lore_revision::shared_store::LoreSharedStoreListEventData;
use lore_revision::shared_store::LoreSharedStoreListItem;
use lore_revision::shared_store::SharedStoreError;
use lore_revision::shared_store::find_existing_shared_store_in_dir;
use lore_revision::shared_store::registry::SharedStoreRegistry;
use lore_revision::shared_store::registry::SharedStoreRegistryEntry;
use lore_revision::util::config::SaveableConfig;
use tokio::time::Instant;

use crate::call::no_repository_call;
use crate::call_delegation::dispatch_call;
use crate::call_delegation::on_running_service_or_here;
use crate::interface::LoreArray;
use crate::interface::LoreEvent;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::interface::LoreString;

#[repr(C)]
#[derive(
    Debug,
    Clone,
    PartialEq,
    Default,
    LoreArgs,
    bitcode::Encode,
    bitcode::Decode,
    lore_macro::WireSchema,
)]
#[handler(create_local)]
/// Arguments for creating a new shared store.
pub struct LoreSharedStoreCreateArgs {
    /// Remote URL backing the store
    pub remote_url: LoreString,

    /// Path where the store will be created; empty string uses the default location
    pub path: LoreString,

    /// Set this as the default shared store in the global config
    pub make_default: u8,

    /// Bytes the store holds before compaction reclaims space; 0 uses the default of 10 GiB
    pub max_size: u64,

    /// Fragments the store holds before eviction removes some; 0 uses the default of 2000000. The
    /// periodic eviction pass treats a value below 1048576 as 1048576
    pub max_capacity: u64,
}

// Locks the C ABI layout, as `LoreStorageOpenArgs` does: remote_url and path (16 bytes each),
// make_default with 7 bytes of padding, max_size and max_capacity.
const _: () = assert!(std::mem::size_of::<LoreSharedStoreCreateArgs>() == 56);

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
pub fn create(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreCreateArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    dispatch_call(globals, args, callback, create_local)
}

fn create_local(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreCreateArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
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

        lore_revision::shared_store::create_shared_store(
            path,
            remote_url,
            args.make_default != 0,
            StoreConfig::client_default_with_limits(args.max_capacity, args.max_size),
        )
        .await
    })
}

#[repr(C)]
#[derive(
    Debug,
    Clone,
    PartialEq,
    Default,
    LoreArgs,
    bitcode::Encode,
    bitcode::Decode,
    lore_macro::WireSchema,
)]
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
pub fn info(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreInfoArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    dispatch_call(globals, args, callback, info_local)
}

async fn info_local(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    let command = async move |_args| -> Result<(), SharedStoreError> {
        let config = GlobalConfig::load()
            .await
            .forward::<SharedStoreError>("loading global config")?;

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
#[derive(
    Debug,
    Clone,
    PartialEq,
    Default,
    LoreArgs,
    bitcode::Encode,
    bitcode::Decode,
    lore_macro::WireSchema,
)]
#[handler(list_here_or_on_the_service)]
/// Arguments for listing the registry of shared stores.
pub struct LoreSharedStoreListArgs {
    /// Whether to load each shared store to search for each instance using it. A store another
    /// process still holds after the listing's wait is listed without its instances.
    pub include_instances: u8,
}

/// Returns information about all registered shared stores
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
/// | [`LoreEvent::SharedStoreList`](crate::interface::LoreEvent::SharedStoreList) | Emitted on success with every registered shared store, and the instances using each when `include_instances` is set. A store another process still holds once the listing has waited 3 seconds in all is listed without its instances, and a warning names it. With `include_instances` set and a service running in another process, the listing runs in the service, so this includes a store the calling process holds |
pub fn list(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreListArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    dispatch_call(globals, args, callback, list_here_or_on_the_service)
}

/// How long a listing waits in all for the shared stores other processes hold, before it lists
/// each store still held without its instances.
#[lore_macro::test_pub]
const HELD_STORE_WAIT: Duration = Duration::from_secs(3);

/// Lists the shared stores as [`list_local`] does, on the Lore service where `args` include
/// instances and a service runs in another process: a service holds the stores of the virtual
/// instances it serves for as long as it serves them. In the service, a store the calling process
/// holds is one another process holds, which the listing lists without its instances.
fn list_here_or_on_the_service(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreListArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    if args.include_instances == 0 {
        Either::Left(list_local(globals, args, callback))
    } else {
        Either::Right(on_running_service_or_here(
            globals, args, callback, list_local,
        ))
    }
}

async fn list_local(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreListArgs,
    callback: LoreEventCallback,
) -> i32 {
    let command = async move |_args| -> Result<(), RepositoryError> {
        let include_instances = args.include_instances != 0;
        let registry = SharedStoreRegistry::load()
            .await
            .forward::<RepositoryError>("Unable to load registry")?;

        let mut wait = HELD_STORE_WAIT;
        let mut stores = Vec::new();
        for entry in registry.entries() {
            let (ids, paths) = if include_instances {
                instances_in(entry, &mut wait).await?
            } else {
                Default::default()
            };
            let per_store_data = LoreSharedStoreListItem {
                remote_url: entry.remote_url().into(),
                store_path: entry.path().into(),
                instance_paths: LoreArray::from_vec(paths),
                instance_ids: LoreArray::from_vec(ids),
            };
            stores.push(per_store_data);
        }
        LoreEvent::SharedStoreList(LoreSharedStoreListEventData {
            stores: LoreArray::from_vec(stores),
        })
        .send();

        Ok(())
    };
    no_repository_call(globals, callback, args, list, command).await
}

/// The IDs and paths of the instances registered in the shared store of `entry`, read under the
/// store's locks, or none where another process still holds the store after up to `wait` spent
/// waiting for it, with a warning naming it. Takes the time spent waiting off `wait`.
async fn instances_in(
    entry: &SharedStoreRegistryEntry,
    wait: &mut Duration,
) -> Result<(Vec<InstanceId>, Vec<LoreString>), RepositoryError> {
    let Taken::Locked(_locks) = lock_shared_store(entry, wait).await? else {
        warn_of_held_store(entry.path());
        return Ok(Default::default());
    };
    let repository = entry.create_null_repository_context().await?;
    Ok(list_instances(&repository)
        .await
        .forward::<RepositoryError>("Listing instances from repository")?
        .into_iter()
        .map(|metadata| (metadata.instance_id, metadata.path.into()))
        .unzip())
}

/// Locks the stores of the shared store of `entry`, for up to `wait` spent waiting while another
/// process holds them, and takes the time spent waiting off `wait`. Waits only where an attempt
/// that doesn't wait finds the store held, as
/// [`lore_revision::global::external_dir::locks::lock_stores_until`] does, so a store no other
/// process holds takes nothing off `wait`.
#[lore_macro::test_pub]
async fn lock_shared_store(
    entry: &SharedStoreRegistryEntry,
    wait: &mut Duration,
) -> Result<Taken, RepositoryError> {
    let store = entry.store_dir();
    let stores = std::slice::from_ref(&store);
    match try_lock_stores(stores, Joining::OwnProcess, Duration::ZERO)
        .await
        .forward_any::<RepositoryError>("locking a shared store")?
    {
        Taken::Held(_) if !wait.is_zero() => {}
        taken => return Ok(taken),
    }
    let started = Instant::now();
    let taken = try_lock_stores(stores, Joining::OwnProcess, *wait)
        .await
        .forward_any::<RepositoryError>("locking a shared store")?;
    *wait = wait.saturating_sub(started.elapsed());
    Ok(taken)
}

/// Warns the call's callback that the instances of the shared store at `path` aren't listed, as
/// another process holds the store, and writes the warning to the log file where one is configured:
/// through [`crate::log::lore_log_dispatch`] directly, as the process-wide log callback that
/// `lore_warn!` goes through reaches it only once a process installs it.
fn warn_of_held_store(path: &str) {
    let message = format!(
        "Not listing the instances of the shared store at {path}: another process holds it"
    );
    crate::log::lore_log_dispatch(LoreLogLevel::Warn, module_path!(), &message);
}

#[repr(C)]
#[derive(
    Debug,
    Clone,
    PartialEq,
    Default,
    LoreArgs,
    bitcode::Encode,
    bitcode::Decode,
    lore_macro::WireSchema,
)]
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
pub fn set_use_automatically(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreSetUseAutomaticallyArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    dispatch_call(globals, args, callback, set_use_automatically_local)
}

async fn set_use_automatically_local(
    globals: LoreGlobalArgs,
    args: LoreSharedStoreSetUseAutomaticallyArgs,
    callback: LoreEventCallback,
) -> i32 {
    let dry_run = globals.dry_run();
    let command = async move |_args| -> Result<(), SharedStoreError> {
        let (mut config, lock) = GlobalConfig::load_locked()
            .await
            .forward::<SharedStoreError>("loading global config")?;
        if args.enabled != 0 {
            config.use_shared_store_automatically = Some(true);
        } else {
            config.use_shared_store_automatically = None;
        }
        if !dry_run {
            config
                .save(lock)
                .await
                .forward::<SharedStoreError>("saving global config")?;
        }
        Ok(())
    };
    no_repository_call(globals, callback, args, info, command).await
}
