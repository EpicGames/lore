// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The warning a listing of the shared stores gives its caller for a store another process holds,
//! in a process that never installs the process-wide log callback, as an embedder that doesn't
//! call `lore_log_configure` never does.
//!
//! A test target of its own because unit tests install and remove that callback process-wide, so no
//! unit test can rely on its absence.

mod test_util;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use lore::interface::LoreEvent;
use lore::interface::LoreEventCallback;
use lore::shared_store::LoreSharedStoreListArgs;
use lore_base::log::LoreLogLevel;
use lore_revision::interface::LoreGlobalArgs;
use test_util::TempDir;
use test_util::hold_store;
use test_util::listed_path;
use test_util::register;

/// A listing with instances, in a process without the process-wide log callback, delivers the
/// warning naming a store another process holds to the call's own callback.
#[test]
fn a_listing_warns_its_caller_of_a_held_store_without_the_log_callback() {
    let sandbox = TempDir::new("lore-held-store-warning-");
    // Safety: no runtime exists yet, and this target's one test is the only reader of the
    // environment.
    unsafe {
        std::env::set_var("LORE_USE_SERVICE", "0");
        std::env::remove_var("LORE_SERVICE_EXECUTABLE");
        std::env::set_var("LORE_GLOBAL_PATH", sandbox.path());
        std::env::set_var("LORE_AUTH_PATH", sandbox.path());
        std::env::set_var(
            "LORE_SERVICE_SOCKET",
            format!("lore_service-held-store-warning-{}", std::process::id()),
        );
    }
    let store = sandbox.child("store");
    register(sandbox.path(), &[&store]);

    let warnings = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&warnings);
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::Log(data) = event
            && data.level == LoreLogLevel::Warn
        {
            recorder
                .lock()
                .expect("the collector lock is not poisoned")
                .push(data.message.to_string());
        }
    }));
    let code = lore::runtime().block_on(async {
        let _hold = hold_store(&store).await;
        tokio::time::timeout(
            Duration::from_secs(30),
            lore::shared_store::list(
                LoreGlobalArgs::default(),
                LoreSharedStoreListArgs {
                    include_instances: 1,
                },
                callback,
            ),
        )
        .await
        .expect("the listing doesn't wait for the store held")
    });

    assert_eq!(code, 0);
    let path = listed_path(&store);
    let warnings = warnings.lock().expect("the collector lock is not poisoned");
    assert!(
        warnings.iter().any(|warning| warning.contains(&path)),
        "a warning names the store {path} held: {warnings:?}"
    );
}
