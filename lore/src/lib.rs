// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod args;
pub mod auth;
pub mod branch;
pub(crate) mod call;
pub mod call_delegation;
pub mod dependency;
pub mod file;
pub mod interface;
pub mod layer;
pub mod link;
pub mod lock;
pub mod log;
pub mod notification;
pub mod remote;
pub mod repository;
pub mod revision;
pub mod revision_tree;
pub mod service;
pub mod shared_store;
pub mod storage;
mod util;

use interface::LoreString;
pub use lore_base::version::LORE_LIBRARY_VERSION;

pub fn shutdown() {
    // Close every outstanding storage handle before connections drop and the runtime tears
    // down. The close sequence (mark invalid, drain in-flight, spawn flush) must run inside
    // an async context to await the per-handle drains. Three runtime contexts are possible:
    //   1. No tokio runtime on the calling thread (typical FFI entry from C): use the
    //      shared multi-thread runtime via `block_on`.
    //   2. A multi-thread runtime is current: `block_in_place` lets us block this worker
    //      thread while the runtime keeps other workers running.
    //   3. A single-thread runtime is current (`#[tokio::test]`, embedders, etc.):
    //      `block_in_place` would panic. Spawn the close into the current handle and run a
    //      best-effort wait via `futures::executor::block_on`-style polling — but tokio
    //      offers no clean primitive for that, so we instead spawn into the shared
    //      multi-thread runtime and block there.
    let close_future = async {
        storage::close_all_handles().await;
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(move || {
                handle.block_on(close_future);
            });
        }
        Ok(_) => {
            // Single-threaded current runtime: bouncing into the shared multi-thread runtime
            // would deadlock if the caller's runtime is the one driving this thread. The
            // safest option is to drive the close on the shared runtime via a fresh worker.
            let shared = lore_base::runtime::runtime();
            std::thread::scope(|s| {
                s.spawn(|| shared.block_on(close_future));
            });
        }
        Err(_) => {
            // No tokio context: drive on the shared runtime directly.
            lore_base::runtime::runtime().block_on(close_future);
        }
    }

    lore_revision::interface::drop_connections();

    lore_revision::interface::shutdown();
}

pub fn runtime() -> tokio::runtime::Handle {
    lore_base::runtime::runtime()
}

/// Caps the total number of threads Lore sizes its pools for. Pass `0` for "no
/// limit". Must be called before the first Lore operation; overridden by the
/// `LORE_MAX_THREADS` env var when that is set above zero. Returns `true` if
/// applied, `false` if a limit was already set.
pub fn set_thread_limit(count: usize) -> bool {
    lore_base::runtime::set_thread_limit(count)
}

/// Whether calls will be executed by the Lore service process rather than in
/// this one. Safe to call before a runtime exists, so a caller can size its
/// threading before doing any work. See [`size_threads_for_relaying`].
pub fn will_use_service() -> bool {
    call_delegation::will_use_service()
}

/// Sizes the shared runtime for a process that only relays its work to the Lore
/// service, rather than performing it. Creates the runtime, so it must be
/// called before the first Lore operation and only by a process that does no
/// work of its own; the service process itself must never call it.
pub fn size_threads_for_relaying() {
    drop(lore_base::runtime::runtime_with_settings(Some(
        lore_base::runtime::TokioSettings::relay_only(),
    )));
}

pub fn log_file_path() -> LoreString {
    log::get_logs_path().into()
}
