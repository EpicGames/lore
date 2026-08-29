// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::BranchId;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::branch::BranchLatestStatus;
    use lore_revision::repository::RepositoryContext;
    use lore_storage::local::immutable_store::LocalImmutableStore;

    include!("helper.rs");

    async fn test_repository(
        path: &std::path::Path,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
    ) -> Arc<RepositoryContext> {
        let immutable_store = LocalImmutableStore::new(
            None,
            lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
        )
        .await
        .expect("Failed to create store");
        let write_token = lore_revision::repository::RepositoryWriteToken::acquire(path).await;
        Arc::new(
            RepositoryContext::new(
                default_repository_creation_args(immutable_store, mutable_store).with_path(path),
            )
            .with_write_token(write_token.share()),
        )
    }

    fn branch_id() -> BranchId {
        Context::from(uuid::Uuid::now_v7())
    }

    async fn seed(repository: Arc<RepositoryContext>, branch: BranchId, tip: Hash) {
        branch::store_latest(
            repository,
            branch,
            Hash::default(),
            tip,
            BranchLatestStatus::Divergent,
        )
        .await
        .expect("Seeding the first tip must succeed");
    }

    #[tokio::test]
    async fn store_latest_creates_the_pointer_when_the_branch_has_no_tip() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable).await;
                let branch = branch_id();
                let tip = Hash::from_u64(0x11);

                seed(repository.clone(), branch, tip).await;

                assert_eq!(
                    branch::load_latest(repository, branch)
                        .await
                        .expect("Tip must read back"),
                    tip
                );
            }))
            .await
            .expect("Task failed");
    }

    /// The compare-and-swap is only the first of three writes: the divergence flag and
    /// the history chain hang off the same call, and a client that stopped recording
    /// either would lose the next sync's divergence check or the branch's head history.
    #[tokio::test]
    async fn store_latest_records_the_divergence_flag_and_the_history() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable).await;
                let branch = branch_id();
                let first = Hash::from_u64(0x11);
                let second = Hash::from_u64(0x22);

                seed(repository.clone(), branch, first).await;
                assert!(
                    branch::load_latest_divergent(repository.clone(), branch)
                        .await
                        .expect("the divergence flag must read back"),
                    "a Divergent store must record the flag the next sync checks"
                );
                assert_eq!(
                    branch::load_latest_history(repository.clone(), branch, None)
                        .await
                        .expect("the history must read back")
                        .revision,
                    first,
                    "the first tip must head the history chain"
                );

                branch::store_latest(
                    repository.clone(),
                    branch,
                    first,
                    second,
                    BranchLatestStatus::Convergent,
                )
                .await
                .expect("advancing convergently must succeed");

                assert!(
                    !branch::load_latest_divergent(repository.clone(), branch)
                        .await
                        .expect("the divergence flag must read back"),
                    "a Convergent store must clear the flag"
                );
                assert_eq!(
                    branch::load_latest_history(repository, branch, None)
                        .await
                        .expect("the history must read back")
                        .revision,
                    second,
                    "the history chain must follow the tip"
                );
            }))
            .await
            .expect("Task failed");
    }

    #[tokio::test]
    async fn store_latest_advances_the_tip_when_previous_matches() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable).await;
                let branch = branch_id();
                let first = Hash::from_u64(0x11);
                let second = Hash::from_u64(0x22);

                seed(repository.clone(), branch, first).await;

                branch::store_latest(
                    repository.clone(),
                    branch,
                    first,
                    second,
                    BranchLatestStatus::Divergent,
                )
                .await
                .expect("Advancing from the stored tip must succeed");

                assert_eq!(
                    branch::load_latest(repository, branch)
                        .await
                        .expect("Tip must read back"),
                    second
                );
            }))
            .await
            .expect("Task failed");
    }

    #[tokio::test]
    async fn store_latest_rejects_a_previous_that_is_not_the_stored_tip() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable).await;
                let branch = branch_id();
                let stored = Hash::from_u64(0x11);

                seed(repository.clone(), branch, stored).await;

                let error = branch::store_latest(
                    repository.clone(),
                    branch,
                    Hash::from_u64(0x33),
                    Hash::from_u64(0x22),
                    BranchLatestStatus::Divergent,
                )
                .await
                .expect_err("A tip that moved must not be overwritten");
                assert!(
                    error.is_branch_advanced(),
                    "Expected BranchAdvanced, got {error}"
                );

                assert_eq!(
                    branch::load_latest(repository, branch)
                        .await
                        .expect("Tip must read back"),
                    stored,
                    "A rejected compare-and-swap must leave the tip alone"
                );
            }))
            .await
            .expect("Task failed");
    }

    #[tokio::test]
    async fn store_latest_rejects_a_zero_previous_when_the_branch_already_has_a_tip() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable).await;
                let branch = branch_id();
                let stored = Hash::from_u64(0x11);

                seed(repository.clone(), branch, stored).await;

                let error = branch::store_latest(
                    repository.clone(),
                    branch,
                    Hash::default(),
                    Hash::from_u64(0x22),
                    BranchLatestStatus::Divergent,
                )
                .await
                .expect_err("A create against a branch that already has a tip must fail");
                assert!(
                    error.is_branch_advanced(),
                    "Expected BranchAdvanced, got {error}"
                );

                assert_eq!(
                    branch::load_latest(repository, branch)
                        .await
                        .expect("Tip must read back"),
                    stored
                );
            }))
            .await
            .expect("Task failed");
    }

    /// Two committers that both observed the same tip: the compare-and-swap is what
    /// stops the loser from silently orphaning the winner's revision.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_store_latest_from_the_same_previous_publishes_one_tip() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable).await;
                let branch = branch_id();
                let observed = Hash::from_u64(0x11);
                let mine = Hash::from_u64(0x22);
                let theirs = Hash::from_u64(0x33);

                seed(repository.clone(), branch, observed).await;

                let (first, second) = tokio::join!(
                    branch::store_latest(
                        repository.clone(),
                        branch,
                        observed,
                        mine,
                        BranchLatestStatus::Divergent,
                    ),
                    branch::store_latest(
                        repository.clone(),
                        branch,
                        observed,
                        theirs,
                        BranchLatestStatus::Divergent,
                    )
                );

                let winner = match (first, second) {
                    (Ok(()), Err(error)) => {
                        assert!(
                            error.is_branch_advanced(),
                            "The loser must see BranchAdvanced, got {error}"
                        );
                        mine
                    }
                    (Err(error), Ok(())) => {
                        assert!(
                            error.is_branch_advanced(),
                            "The loser must see BranchAdvanced, got {error}"
                        );
                        theirs
                    }
                    (first, second) => panic!(
                        "Exactly one write must win: first {first:?}, second {second:?}, \
                         both observed {observed}"
                    ),
                };

                assert_eq!(
                    branch::load_latest(repository, branch)
                        .await
                        .expect("Tip must read back"),
                    winner
                );
            }))
            .await
            .expect("Task failed");
    }
}
