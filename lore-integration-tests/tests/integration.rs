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
mod shared_store_test;
mod storage_copy_on_write_test;
mod storage_mutable_test;
mod storage_remote_test;
mod storage_test;
mod store_fan_out_test;
mod store_keep_alive_test;

/// Points this test binary's global config, credentials and service socket at
/// names of its own, rather than the machine's.
///
/// These tests call the entry points a user does, and those read the global
/// config: `repository::create` consults `use_shared_store_automatically` even
/// when the call passes no shared store. Reading the machine's fails tests that
/// pass elsewhere, and writing it puts test data in a real store.
///
/// A constructor because the environment is process-wide: this runs before
/// `main`, and so before the threads that would make writing to it a data race.
#[cfg(test)]
#[ctor::ctor]
fn sandbox_machine_settings() {
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let sandbox = std::env::temp_dir().join(format!("lore-integration-tests-{unique}"));
    std::fs::create_dir_all(&sandbox)
        .unwrap_or_else(|error| panic!("creating the sandbox at {}: {error}", sandbox.display()));

    // Safety: constructors run before `main`, so this is the single-threaded
    // window where writing to the environment has no reader to race.
    unsafe {
        std::env::set_var("LORE_GLOBAL_PATH", &sandbox);
        std::env::set_var("LORE_AUTH_PATH", &sandbox);
        // Short suffix rather than the whole of `unique`: the name joins the
        // temporary directory into a socket address, which holds 108 bytes.
        std::env::set_var(
            "LORE_SERVICE_SOCKET",
            format!("lore_service-it-{}", &unique[..12]),
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
    /// Asserted on the resolved directory rather than the variable, since a
    /// name that did not take effect is the failure worth catching.
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

    /// A `stop` connects to whatever is listening whether or not calls relay, so
    /// on the default name this target would end a developer's own service.
    #[test]
    fn the_service_socket_is_this_binarys_own() {
        let named = std::env::var("LORE_SERVICE_SOCKET").expect("the constructor names the socket");

        assert_eq!(
            lore::remote::service_socket_name(),
            named,
            "this target must act on a socket of its own"
        );
        assert_ne!(
            named,
            lore::remote::LORE_SERVICE_SOCKET_NAME,
            "the default socket is the one every service on the machine answers on"
        );
    }
}
