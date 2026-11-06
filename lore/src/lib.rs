// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod args;
pub mod auth;
pub mod branch;
#[cfg(not(feature = "test-util"))]
pub(crate) mod call;
#[cfg(feature = "test-util")]
pub mod call;
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
pub use lore_base::allocator::memory_stats_requested;
pub use lore_base::allocator::memory_summary;
pub use lore_base::lore_spawn;
pub use lore_base::lore_spawn_blocking;
use lore_base::runtime::NetConnections;
pub use lore_base::version::LORE_LIBRARY_VERSION;
/// Whole crate rather than a prelude: `#[error_set]` expands to paths rooted at the crate, so a
/// consumer aliases this into scope as `lore_error_set`.
pub use lore_error_set as error_set;

/// Time allowed for each wait shutdown drives from a synchronous caller: the drains before the
/// runtime shutdown, whose timeout in `lore_revision::interface::shutdown` it matches, and the
/// reads of calls cut short after it.
const SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Shuts the library down, returning whether this call was the one that did it.
///
/// Only the first caller runs the teardown. Every other gets `false`, so a
/// concurrent caller can report the library as already shut down rather than
/// racing a second teardown against the first.
///
/// Claiming the shutdown also closes admission, so calls that arrive while the
/// drains below are still running fail instead of being admitted onto runtimes
/// that are about to go away.
///
/// Safe only once every call has completed. Calls still executing are cancelled at best effort: one
/// not stopped in time may use the memory passed to it after this returns.
pub fn shutdown() -> bool {
    if !lore_base::runtime::claim_runtime_shutdown() {
        lore_base::lore_warn!("Shutdown was already called");
        return false;
    }

    // Garbage collection stops alongside the drains rather than before them, so neither
    // takes the other's share of the budget. A tree writes through the stores its parent
    // owns, so trees drain before storage handles. The storage close sequence (mark
    // invalid, drain in-flight, spawn flush) must run inside an async context
    // to await the per-handle drains, and this function is synchronous wherever it is called
    // from — see `shutdown_block_on` for the three cases and why a `current_thread` caller
    // can only be served with a bound rather than a guarantee.
    if !lore_base::runtime::shutdown_block_on(
        async {
            tokio::join!(lore_revision::repository::stop_store_gc(), async {
                revision_tree::close_all_handles().await;
                storage::close_all_handles().await;
            });
        },
        SHUTDOWN_WAIT,
    ) {
        lore_base::lore_warn!(
            "Timed out draining during shutdown; in-flight edits or writes may be incomplete"
        );
    }

    lore_revision::interface::drop_connections();

    lore_revision::interface::shutdown();

    storage::lent::await_abandoned(SHUTDOWN_WAIT);

    // Services this process started are otherwise collected when the next
    // service call comes, and after a shutdown none will. A program whose
    // service has already exited — stopped by someone else, or died — would
    // hold that child unreaped for however long it outlives its Lore use.
    remote::service_process::collect_exited_services();

    true
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

/// Whether calls will be carried out by the Lore service rather than in this
/// process.
///
/// Answered without a runtime, so a caller that builds one can ask first — see
/// [`size_threads_for_relaying`]. Decided once per process and cached, so asking
/// costs one config read however often it is asked.
pub fn will_use_service() -> bool {
    remote::service_process::service_in_use_blocking()
}

/// Sizes this process's thread pools for relaying its calls to the service, when
/// that is what it will do. A no-op otherwise, and a no-op once a runtime exists.
///
/// Call it before the first Lore operation, and before building a runtime of your
/// own. A relaying process writes a request to a socket and reads events back
/// while the service does the work, so pools sized for that work are threads a
/// whole machine's worth of clients pays for and none of them uses.
///
/// A program that runs the service itself must not call this: it does the work
/// rather than relaying it, whatever this machine's clients do.
pub fn size_threads_for_relaying() {
    if !will_use_service() {
        return;
    }
    lore_base::runtime::runtime_with_settings(
        Some(lore_base::runtime::TokioSettings::relay_only()),
    );
}

/// Sizes this process's thread pools for a command that opens `connections` storage
/// connections to its remote, 0 for the default of one: a network thread for each connection, up
/// to half of the async and network threads together, taken from the async threads. A configured
/// network thread count takes precedence. Returns false, and changes nothing, once a runtime
/// exists, as in a process that relays its calls, or once a count was set.
///
/// Call it before the first Lore operation, and before building a runtime of your own.
pub fn size_threads_for_connections(connections: u32) -> bool {
    size_threads_for(NetConnections::Opened(
        lore_transport::connection::storage_connection_count(connections as usize),
    ))
}

/// Sizes this process's thread pools for running the service: as many network threads as
/// [`size_threads_for_connections`] gives the most storage connections a command opens to a
/// remote, added to the async threads rather than taken from them. The service carries the
/// commands of many callers at once, and can't know in advance how many connections they will ask
/// for.
///
/// Call it before the first Lore operation, and before building a runtime of your own.
pub fn size_threads_for_service() -> bool {
    size_threads_for(NetConnections::Served(
        lore_transport::connection::MAX_STORAGE_CONNECTIONS,
    ))
}

fn size_threads_for(connections: NetConnections) -> bool {
    let sized = lore_base::runtime::set_net_connections(connections);
    if !sized {
        lore_base::lore_debug!("Thread pools already sized; not sizing them for {connections:?}");
    }
    sized
}

pub fn log_file_path() -> LoreString {
    log::get_logs_path().into()
}
