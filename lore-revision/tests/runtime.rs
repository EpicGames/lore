// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod test {
    use lore_base::lore_drain_tasks;
    use lore_base::lore_spawn;
    use lore_base::lore_spawn_blocking;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::branch::BranchError;
    use lore_revision::interface::LoreGlobalArgs;
    use tokio::task::JoinSet;

    pub fn setup_test_execution() -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
        std::sync::Arc::new(lore_revision::interface::ExecutionContext::new_client(
            LoreGlobalArgs::default(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
        ))
    }

    #[tokio::test]
    async fn test_spawn() {
        let execution = setup_test_execution();

        LORE_CONTEXT
            .scope(execution, async {
                assert_eq!(
                    lore_spawn!(async { "finished".to_string() })
                        .await
                        .expect("Task failed"),
                    "finished".to_string()
                );
                assert_eq!(
                    lore_spawn!("test-task", async { "finished".to_string() })
                        .await
                        .expect("Task failed"),
                    "finished".to_string()
                );

                let mut tasks = JoinSet::new();
                let _ = lore_spawn!(tasks, async { "finished".to_string() });
                let _ = lore_spawn!(tasks, "test-task", async { "finished".to_string() });

                let results = tasks.join_all().await;
                assert!(results.len() == 2);
                assert_eq!(results[0], "finished");
                assert_eq!(results[1], "finished");
            })
            .await;
    }

    #[tokio::test]
    async fn test_spawn_blocking() {
        assert_eq!(
            lore_spawn_blocking!(|| { "finished".to_string() })
                .await
                .expect("Task failed"),
            "finished"
        );
        assert_eq!(
            lore_spawn_blocking!("test-task", || { "finished".to_string() })
                .await
                .expect("Task failed"),
            "finished"
        );

        let mut tasks = JoinSet::new();
        let _ = lore_spawn_blocking!(tasks, || { "finished".to_string() });
        let _ = lore_spawn_blocking!(tasks, "test-task", || { "finished".to_string() });

        let results = tasks.join_all().await;
        assert!(results.len() == 2);
        assert_eq!(results[0], "finished");
        assert_eq!(results[1], "finished");
    }

    #[tokio::test]
    async fn test_spawn_drain() {
        let mut tasks = JoinSet::new();

        #[allow(clippy::unused_async)]
        async fn success_task() -> Result<String, BranchError> {
            Ok("result".to_string())
        }
        #[allow(clippy::disallowed_methods)]
        tasks.spawn(success_task());

        #[allow(clippy::unused_async)]
        async fn failure_task() -> Result<String, BranchError> {
            Err(BranchError::internal("Invalid parent"))
        }
        #[allow(clippy::disallowed_methods)]
        tasks.spawn(failure_task());

        let result = lore_drain_tasks!(tasks, BranchError::internal("Task failed"));
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.is_internal());
    }
}
