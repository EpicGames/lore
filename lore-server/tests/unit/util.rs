// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod cert_metrics;
mod core_hop;
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

mod setup_execution {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::runtime::Cancellation;
    use lore_server::util::core_hop::with_request_cancellation;
    use lore_server::util::setup_execution;

    #[tokio::test]
    async fn carries_the_cancellation_of_the_request_being_served() {
        let request = Cancellation::new();
        let execution = with_request_cancellation(request.clone(), async {
            setup_execution("test", String::default(), String::default())
        })
        .await;
        request.cancel();
        let token = execution
            .cancellation()
            .expect("the context carries the request's token");
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn carries_the_cancellation_of_the_context_it_runs_under() {
        let request = Cancellation::new();
        let outer = with_request_cancellation(request.clone(), async {
            setup_execution("outer", String::default(), String::default())
        })
        .await;
        let inner = LORE_CONTEXT
            .scope(outer, async {
                setup_execution("inner", String::default(), String::default())
            })
            .await;
        request.cancel();
        let token = inner
            .cancellation()
            .expect("the nested context carries the outer token");
        assert!(token.is_cancelled());
    }

    #[test]
    fn has_no_cancellation_outside_a_request() {
        let execution = setup_execution("test", String::default(), String::default());
        assert!(execution.cancellation().is_none());
    }
}
