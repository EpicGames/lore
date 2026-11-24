// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod check_cancelled {
    use std::sync::Arc;

    use lore_base::error::Cancelled;
    use lore_base::lore_spawn;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::runtime::Cancellation;
    use lore_revision::runtime::check_cancelled;

    /// A client context, which nothing can cancel.
    fn client_execution() -> Arc<ExecutionContext> {
        Arc::new(ExecutionContext::new_client(
            LoreGlobalArgs::default(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
        ))
    }

    fn context_with(token: &Cancellation) -> Arc<ExecutionContext> {
        Arc::new(
            ExecutionContext::new_server(
                LoreGlobalArgs::default(),
                lore_revision::relay::EventDispatcher::no_dispatch(),
                String::default(),
            )
            .with_cancellation(token.clone()),
        )
    }

    #[tokio::test]
    async fn passes_outside_any_context() {
        check_cancelled().expect("no context means nothing to cancel");
    }

    #[tokio::test]
    async fn passes_in_a_context_without_a_token() {
        LORE_CONTEXT
            .scope(client_execution(), async {
                check_cancelled().expect("a context without a token is never cancelled");
            })
            .await;
    }

    #[tokio::test]
    async fn fails_once_the_token_is_cancelled() {
        let token = Cancellation::new();
        LORE_CONTEXT
            .scope(context_with(&token), async {
                check_cancelled().expect("the token is not cancelled yet");
                token.cancel();
                let Cancelled = check_cancelled().expect_err("the token is cancelled");
            })
            .await;
    }

    #[tokio::test]
    async fn a_spawned_task_sees_its_spawner_cancelled() {
        let token = Cancellation::new();
        token.cancel();
        let result = LORE_CONTEXT
            .scope(context_with(&token), async {
                lore_spawn!(async { check_cancelled() })
                    .await
                    .expect("the task runs to its end")
            })
            .await;
        let Cancelled = result.expect_err("the task inherits the cancelled token");
    }
}

mod cancellation {
    use lore_revision::runtime::Cancellation;

    mod child {
        use super::*;

        #[test]
        fn a_child_is_cancelled_with_its_parent_and_not_the_other_way() {
            let parent = Cancellation::new();
            let child = parent.child();
            child.cancel();
            assert!(child.is_cancelled());
            assert!(!parent.is_cancelled());

            let other = parent.child();
            parent.cancel();
            assert!(other.is_cancelled());
        }
    }

    mod is_parent_cancelled {
        use super::*;

        #[test]
        fn tells_a_cancelled_parent_from_a_child_cancelled_alone() {
            let parent = Cancellation::new();
            let child = parent.child();
            child.cancel();
            assert!(!child.is_parent_cancelled());

            let grandchild = parent.child().child();
            parent.cancel();
            assert!(grandchild.is_parent_cancelled());
        }

        #[test]
        fn a_root_has_no_parent_to_be_cancelled() {
            let root = Cancellation::new();
            root.cancel();
            assert!(!root.is_parent_cancelled());
        }
    }

    mod cancel_on_drop {
        use super::*;

        #[test]
        fn dropping_the_guard_cancels() {
            let cancellation = Cancellation::new();
            drop(cancellation.clone().cancel_on_drop());
            assert!(cancellation.is_cancelled());
        }
    }

    mod cancelled {
        use std::time::Duration;

        use super::*;

        #[tokio::test]
        async fn waiting_on_a_child_ends_when_its_parent_is_cancelled() {
            let parent = Cancellation::new();
            let waiting = parent.child().cancelled();
            parent.cancel();
            tokio::time::timeout(Duration::from_secs(5), waiting)
                .await
                .expect("the wait ends once the parent is cancelled");
        }

        #[tokio::test]
        async fn waiting_after_a_cancel_ends_at_once() {
            let cancellation = Cancellation::new();
            cancellation.cancel();
            tokio::time::timeout(Duration::from_secs(5), cancellation.cancelled())
                .await
                .expect("the wait ends at once");
        }

        #[tokio::test]
        async fn waiting_on_a_child_after_its_parent_is_cancelled_ends_at_once() {
            let parent = Cancellation::new();
            let child = parent.child();
            parent.cancel();
            tokio::time::timeout(Duration::from_secs(5), child.cancelled())
                .await
                .expect("the wait ends at once");
        }
    }
}
