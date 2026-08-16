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
pub use lore_base::lore_spawn;
pub use lore_base::lore_spawn_blocking;
pub use lore_base::version::LORE_LIBRARY_VERSION;
/// Whole crate rather than a prelude: `#[error_set]` expands to paths rooted at the crate, so a
/// consumer aliases this into scope as `lore_error_set`.
pub use lore_error_set as error_set;

/// Time allowed for each stage of shutdown that has to be driven from a synchronous
/// caller. Matches the runtime shutdown timeout in `lore_revision::interface::shutdown`,
/// which runs immediately after these.
const SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

pub fn shutdown() {
    // Before the storage handles: a tree writes through the stores its parent owns, so
    // draining trees first leaves the storage flush below a quiesced store.
    if !lore_base::runtime::shutdown_block_on(revision_tree::close_all_handles(), SHUTDOWN_WAIT) {
        lore_base::lore_warn!(
            "Timed out closing revision tree handles during shutdown; in-flight edits may be \
             incomplete"
        );
    }

    // Close every outstanding storage handle before connections drop and the runtime tears
    // down. The close sequence (mark invalid, drain in-flight, spawn flush) must run inside
    // an async context to await the per-handle drains, and this function is synchronous
    // wherever it is called from — see `shutdown_block_on` for the three cases and why a
    // `current_thread` caller can only be served with a bound rather than a guarantee.
    if !lore_base::runtime::shutdown_block_on(storage::close_all_handles(), SHUTDOWN_WAIT) {
        lore_base::lore_warn!(
            "Timed out closing storage handles during shutdown; in-flight writes may be \
             incomplete"
        );
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
///
/// The sizing is applied once, when the runtime is built, and cannot be undone:
/// later settings are ignored because the runtime already exists. A long-lived
/// process that turns the service off after this has been called (for example
/// via [`service::set_use_automatically`](crate::service::set_use_automatically))
/// then runs its work locally on this relay-sized runtime, which is correct but
/// slow. Decide routing once at start-up and do not flip it mid-process.
pub fn size_threads_for_relaying() {
    drop(lore_base::runtime::runtime_with_settings(Some(
        lore_base::runtime::TokioSettings::relay_only(),
    )));
}

pub fn log_file_path() -> LoreString {
    log::get_logs_path().into()
}
