// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod cert_metrics;
pub(crate) mod hook_recorder;
mod local_store_monitor;
pub(crate) mod test_support;

use lore_server::util::*;

#[test]
fn setup_execution_stores_server_execution_state() {
    let span = tracing::info_span!("test_request");
    let _guard = span.enter();

    let ctx = setup_execution("test", "test-corr".to_string(), "test-user".to_string());

    let state = ctx
        .caller_state()
        .expect("caller_state should be set")
        .clone();
    let downcasted =
        std::sync::Arc::downcast::<lore_server::execution_state::ServerExecutionState>(state);
    assert!(downcasted.is_ok());
}
