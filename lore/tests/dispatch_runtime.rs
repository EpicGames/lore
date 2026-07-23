// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! An embedder's first FFI call, when the service is enabled, must size the
//! shared runtime for relaying rather than build the full runtime it never
//! uses. This runs in its own test binary so the process-wide runtime is built
//! fresh here.

#[test]
fn first_ffi_call_sizes_runtime_for_relaying_when_service_enabled() {
    let global = std::env::temp_dir().join("lore_dispatch_runtime_test_global");
    // Safety: set before any runtime use, and this test binary is
    // single-threaded at this point.
    unsafe {
        std::env::set_var("LORE_USE_SERVICE", "1");
        std::env::set_var("LORE_GLOBAL_PATH", &global);
        std::env::remove_var("LORE_WORKER_THREADS");
    }

    // Any FFI call goes through the synchronous runner, which sizes the runtime
    // before building it. `set-use-automatically` only writes the global config
    // — here the isolated one under LORE_GLOBAL_PATH — so it touches no socket
    // and starts no service.
    let globals = lore::interface::LoreGlobalArgs::default();
    let args = lore::interface::LoreServiceSetUseAutomaticallyArgs { enabled: 1 };
    let callback = lore::interface::LoreEventCallbackConfig {
        user_context: 0,
        func: None,
    };
    lore::interface::lore_service_set_use_automatically(&globals, &args, callback);

    assert_eq!(
        lore::runtime().metrics().num_workers(),
        2,
        "the first FFI call must size the runtime lean when the service is enabled"
    );
}
