// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod aws_store_test;
mod common;
mod dynamodb_test;
mod hashicorp;
mod locks_test;
mod presign_test;
mod remote_store_test;
mod replicated_store_test;
mod replication_service_test;
mod revision_tree_test;
mod service_test;
mod shared_store_test;
mod storage_copy_on_write_test;
mod storage_mutable_test;
mod storage_remote_test;
mod storage_test;
mod store_fan_out_test;
mod store_keep_alive_test;

/// Points this test binary's global config and credentials at a directory of
/// its own, the way the Python suite does for every command it runs.
///
/// These tests call the same entry points a user does, and those read the
/// global config: `repository::create` consults `use_shared_store_automatically`
/// even when the call passes no shared store, and every routed call consults
/// `use_service_automatically`. Without this the tests would read — and could
/// write — whatever the developer running them has configured, so a machine
/// with the service or shared stores enabled would fail tests that pass
/// elsewhere, or quietly put test data in a real store.
///
/// A constructor rather than a fixture because the setting is process-wide:
/// this runs before `main`, and so before the test harness starts the threads
/// that would make writing to the environment a data race. The directory is
/// left behind for post-mortem, under the OS temporary directory that the rest
/// of the suite already uses.
#[cfg(test)]
#[ctor::ctor]
fn sandbox_global_config() {
    let sandbox = std::env::temp_dir().join(format!(
        "lore-integration-tests-{}",
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(error) = std::fs::create_dir_all(&sandbox) {
        panic!(
            "creating the global config sandbox at {}: {error}",
            sandbox.display()
        );
    }
    // Safety: constructors run before `main`, so this is the single-threaded
    // window where writing to the environment has no reader to race.
    unsafe {
        std::env::set_var("LORE_GLOBAL_PATH", &sandbox);
        std::env::set_var("LORE_AUTH_PATH", &sandbox);
        // The service socket is one per user under its default name, so tests
        // that start or stop a service would reach whichever one the developer
        // is running. Name one for this binary instead.
        std::env::set_var(
            "LORE_SERVICE_SOCKET",
            format!("lore_service_it_{}", uuid::Uuid::new_v4().simple()),
        );
    }
}

#[cfg(test)]
pub fn setup_execution(
    user_id: String,
) -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    std::sync::Arc::new(lore_revision::interface::ExecutionContext::new_server(
        lore_revision::interface::LoreGlobalArgs::default(),
        lore_revision::relay::EventDispatcher::no_dispatch(),
        user_id,
    ))
}

#[cfg(test)]
mod sandbox_tests {
    /// The sandbox is only worth anything if the config the library resolves is
    /// the one inside it, so assert on the resolved directory rather than on
    /// the variable this crate set.
    #[test]
    fn the_global_config_resolves_inside_the_sandbox() {
        let configured =
            std::env::var("LORE_GLOBAL_PATH").expect("the constructor sets the sandbox");
        let resolved = lore_revision::global::get_global_config_dir()
            .expect("the global config directory resolves");

        assert!(
            resolved.starts_with(&configured),
            "the library resolved {} , which is outside the sandbox at {configured}",
            resolved.display()
        );
    }
}
