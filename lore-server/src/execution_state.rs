// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub use lore_telemetry::execution_state::ServerExecutionState;

#[cfg(test)]
mod tests {
    use lore_base::lore_spawn;

    use super::*;

    #[test]
    fn server_execution_state_holds_span() {
        let span = tracing::info_span!("test_span");
        let state = ServerExecutionState {
            span,
            context_label: "test",
        };
        assert!(!state.span.is_none());
    }

    #[test]
    fn server_execution_state_stores_in_caller_state() {
        let span = tracing::info_span!("test_span");
        let state = std::sync::Arc::new(ServerExecutionState {
            span,
            context_label: "test",
        });

        let mut ctx = lore_revision::interface::ExecutionContext::default();
        ctx.set_caller_state(state);

        let retrieved = ctx.caller_state().unwrap().clone();
        let downcasted = std::sync::Arc::downcast::<ServerExecutionState>(retrieved);
        assert!(downcasted.is_ok());
        assert!(!downcasted.unwrap().span.is_none());
    }

    #[lore_macro::lore_instrument]
    async fn instrumented_async_fn() -> u64 {
        42
    }

    #[lore_macro::lore_instrument]
    fn instrumented_sync_fn() -> u64 {
        42
    }

    #[lore_macro::lore_instrument]
    async fn get_current_span_id() -> Option<tracing::span::Id> {
        tracing::Span::current().id()
    }

    #[lore_macro::lore_instrument]
    fn get_current_span_id_sync() -> Option<tracing::span::Id> {
        tracing::Span::current().id()
    }

    #[test]
    fn lore_instrument_async_compiles_and_runs_without_context() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(instrumented_async_fn());
        assert_eq!(result, 42);
    }

    #[test]
    fn lore_instrument_sync_compiles_and_runs_without_context() {
        let result = instrumented_sync_fn();
        assert_eq!(result, 42);
    }

    #[test]
    fn span_propagates_through_lore_spawn_async() {
        use lore_base::runtime::LORE_CONTEXT;

        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = tracing::info_span!("request_span");
        let expected_id = span.id();

        let mut ctx = lore_revision::interface::ExecutionContext::default();
        ctx.set_caller_state(std::sync::Arc::new(ServerExecutionState {
            span: span.clone(),
            context_label: "test",
        }));
        let execution = std::sync::Arc::new(ctx);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let observed_id = rt.block_on(LORE_CONTEXT.scope(execution, async {
            let handle = lore_spawn!(
                LORE_CONTEXT.scope(lore_revision::runtime::execution_context(), async {
                    get_current_span_id().await
                }),
            );
            handle.await.unwrap()
        }));

        assert!(expected_id.is_some(), "test span should have an id");
        assert_eq!(
            observed_id, expected_id,
            "span inside lore_instrument-annotated fn should match the request span"
        );
    }

    #[test]
    fn no_span_when_no_caller_state() {
        use lore_base::runtime::LORE_CONTEXT;

        let ctx = lore_revision::interface::ExecutionContext::default();
        let execution = std::sync::Arc::new(ctx);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let observed_id =
            rt.block_on(LORE_CONTEXT.scope(execution, async { get_current_span_id().await }));

        assert!(
            observed_id.is_none(),
            "should have no active span when caller_state is not set"
        );
    }
}
