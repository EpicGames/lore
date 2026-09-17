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

/// Points this crate's unit tests at a global config, credential store and
/// service socket of their own, rather than the machine's.
///
/// `cfg!(test)` keeps a unit test from relaying but does not stop one binding a
/// socket, and `remote::network`'s round trip binds whatever name it is given.
/// `.cargo/config.toml` names a fixed socket for everything cargo runs, which
/// leaves a concurrent run of this crate, and a test binary run directly.
///
/// A constructor because the environment is process-wide: this runs before the
/// harness starts the threads that would make writing to it a data race.
#[cfg(test)]
#[ctor::ctor]
fn sandbox_machine_settings() {
    let sandbox = std::env::temp_dir().join(format!("lore-unit-tests-{}", std::process::id()));
    std::fs::create_dir_all(&sandbox)
        .unwrap_or_else(|error| panic!("creating the sandbox at {}: {error}", sandbox.display()));

    // Safety: constructors run before `main`, so this is the single-threaded
    // window where writing to the environment has no reader to race.
    unsafe {
        std::env::set_var("LORE_GLOBAL_PATH", &sandbox);
        std::env::set_var("LORE_AUTH_PATH", &sandbox);
        std::env::set_var(
            "LORE_SERVICE_SOCKET",
            format!("lore_service-unit-{}", std::process::id()),
        );
    }
}

/// Time allowed for the shutdown work that has to be driven from a synchronous caller.
/// Matches the runtime shutdown timeout in `lore_revision::interface::shutdown`, which
/// runs immediately after it.
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

pub fn log_file_path() -> LoreString {
    log::get_logs_path().into()
}

#[cfg(test)]
mod sandbox_tests {
    /// Asserted on what the library resolves rather than on the variable, since
    /// a name that did not take effect is the failure worth catching.
    #[test]
    fn the_global_config_resolves_inside_the_sandbox() {
        let configured =
            std::env::var("LORE_GLOBAL_PATH").expect("the constructor names the sandbox");
        let resolved = lore_revision::global::get_global_config_dir()
            .expect("the global config directory resolves");

        assert!(
            resolved.starts_with(&configured),
            "the library resolved {}, outside the sandbox at {configured}",
            resolved.display()
        );
    }

    /// On the default name, the round trip in `remote::network` binds the socket
    /// a developer's own service answers on.
    #[test]
    fn the_service_socket_is_this_processs_own() {
        assert_ne!(
            crate::remote::service_socket_name(),
            crate::remote::LORE_SERVICE_SOCKET_NAME,
            "the default socket is the one every service on the machine answers on"
        );
    }
}
