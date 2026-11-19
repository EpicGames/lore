// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The service state's hold on the background recovery of a move of the external root.

use lore_revision::service_state::ServiceStateImpl;

/// Ending the background recovery aborts the task it holds and waits for it to end.
#[tokio::test]
async fn ending_the_background_recovery_aborts_its_task() {
    let state = ServiceStateImpl::new();
    let task = lore_base::lore_spawn!(std::future::pending::<()>());
    let watched = task.abort_handle();
    state.hold_background_recovery(task);

    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.end_background_recovery(),
    )
    .await
    .expect("the recovery ends");

    assert!(watched.is_finished());
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.end_background_recovery(),
    )
    .await
    .expect("a second end finds nothing to end");
}
