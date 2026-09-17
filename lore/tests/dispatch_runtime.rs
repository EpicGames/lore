// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Sizing the runtime from a library entry point, which is what an embedder
//! that never calls `size_threads_for_relaying` itself relies on.
//!
//! `relay_sizing.rs` covers the sizing when a caller asks for it outright. This
//! covers the call entry points asking for it before they build the runtime,
//! which is the ordering an embedder gets for free and nothing else asserts: a
//! dispatch that reached the runtime first would build the full one and the
//! sizing would be a no-op from then on.
//!
//! A test target of its own because the runtime is built once per process.

/// The first FFI call an embedder makes has to size the runtime, so the pools a
/// relaying process never uses are not built on its behalf.
///
/// `lore_service_set_use_automatically` is the call used because it runs where
/// it was called whatever the settings say, so it touches no socket and starts
/// no service — what is measured is the entry point, not the verb.
#[test]
fn the_first_library_call_sizes_the_runtime_for_relaying() {
    let sandbox =
        std::env::temp_dir().join(format!("lore-dispatch-runtime-{}", std::process::id()));
    std::fs::create_dir_all(&sandbox).expect("the sandbox must be creatable");

    // Safety: single-threaded, before any runtime exists, and nothing else in
    // this target reads the environment concurrently.
    unsafe {
        // Both, since relaying needs both. Named through the environment so this
        // reads nothing of the machine it runs on.
        std::env::set_var("LORE_USE_SERVICE", "1");
        std::env::set_var("LORE_SERVICE_EXECUTABLE", sandbox.join("lore"));
        // The settings this call writes, and the socket nothing here contacts,
        // kept away from the machine's own.
        std::env::set_var("LORE_GLOBAL_PATH", &sandbox);
        std::env::set_var("LORE_AUTH_PATH", &sandbox);
        std::env::set_var(
            "LORE_SERVICE_SOCKET",
            format!("lore_service-dispatch-{}", std::process::id()),
        );
    }

    assert!(
        lore::will_use_service(),
        "both settings are named, so this process relays"
    );

    // Nothing has built a runtime yet, which is the state an embedder's first
    // call arrives in and the only state the sizing can act from.
    let globals = lore::interface::LoreGlobalArgs::default();
    let args = lore::interface::LoreServiceSetUseAutomaticallyArgs { enabled: 1 };
    let callback = lore::interface::LoreEventCallbackConfig {
        user_context: 0,
        func: None,
    };
    let status = lore::interface::lore_service_set_use_automatically(&globals, &args, callback);
    assert_eq!(status, 0, "the call must succeed against its own settings");

    let expected = lore_base::runtime::TokioSettings::relay_only()
        .worker_threads
        .expect("sizing for relaying asks for a worker count");
    assert_eq!(
        lore::runtime().metrics().num_workers(),
        expected,
        "the entry point must have sized the runtime before building it"
    );
}
