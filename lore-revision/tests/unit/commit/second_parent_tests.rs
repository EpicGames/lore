// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The second parent given to committing a tree in memory: which second parents it refuses as an
//! invalid argument before it writes anything, and which read failures stay the read's own.

use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::commit::InMemoryCommitFailure;
use lore_revision::commit::commit_in_memory_revision;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::in_memory;
use crate::helper::test_store_create;
use crate::state::HoldingStore;

/// A repository reading through a [`HoldingStore`], telling a failed local read apart from a miss
/// ([`RepositoryContext::strict_local_reads`]), with a revision on a branch of its own holding
/// `a.bin` and, from no revision on another branch, the revision `other`. Answers the store, the
/// repository, the first revision and its branch, and `other`.
async fn two_lines() -> (
    Arc<HoldingStore>,
    Arc<RepositoryContext>,
    (Hash, BranchId),
    Hash,
) {
    let (immutable, mutable, _) = test_store_create().await.expect("creating the stores");
    let store = HoldingStore::over(immutable);
    let repository = Arc::new(
        RepositoryContext::new(
            crate::helper::default_repository_creation_args(store.clone(), mutable)
                .with_id(RepositoryId::from(uuid::Uuid::now_v7())),
        )
        .with_write_token(in_memory::token())
        .strict_local_reads(),
    );
    let branch = in_memory::branch_id();
    let state = State::new();
    in_memory::add(
        &state,
        repository.clone(),
        ROOT_NODE,
        in_memory::file("a.bin"),
        "a.bin",
    )
    .await;
    let first = in_memory::commit(&repository, &state, Hash::default(), branch).await;
    let state = State::new();
    in_memory::add(
        &state,
        repository.clone(),
        ROOT_NODE,
        in_memory::file("b.bin"),
        "b.bin",
    )
    .await;
    let other =
        in_memory::commit(&repository, &state, Hash::default(), in_memory::branch_id()).await;
    (store, repository, (first, branch), other)
}

/// Commits on `parent_self`, a tree there holding `c.bin` beside what it holds, with
/// `parent_other` as the second parent.
async fn commit_merge(
    repository: &Arc<RepositoryContext>,
    (parent_self, branch): (Hash, BranchId),
    parent_other: Hash,
) -> Result<Hash, InMemoryCommitFailure> {
    let state = State::deserialize(repository.clone(), parent_self)
        .await
        .expect("the loaded revision");
    in_memory::add(
        &state,
        repository.clone(),
        ROOT_NODE,
        in_memory::file("c.bin"),
        "c.bin",
    )
    .await;
    commit_in_memory_revision(
        repository.clone(),
        &in_memory::token(),
        state,
        in_memory::metadata_on(branch),
        parent_self,
        parent_other,
        branch,
    )
    .await
}

/// A second parent the store doesn't hold is an invalid argument; one whose read fails otherwise,
/// here as the store refusing the caller, fails with the read's own error. Neither writes anything.
#[tokio::test]
async fn a_second_parent_is_an_invalid_argument_only_where_the_store_does_not_hold_it() {
    let (_, _, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let (store, repository, first, other) = two_lines().await;

        store.refuse(other);
        let missing = commit_merge(&repository, first, other)
            .await
            .expect_err("a second parent the store doesn't hold");
        assert!(missing.error.is_invalid_arguments(), "{:?}", missing.error);
        assert!(!missing.tree_mutated());
        assert_on(&repository, first).await;

        let (store, repository, first, other) = two_lines().await;
        store.fail_next(
            other,
            usize::MAX,
            lore_storage::StoreError::from(lore_base::error::NotAuthorized),
        );
        let refused = commit_merge(&repository, first, other)
            .await
            .expect_err("a second parent the store refuses to read");
        assert!(refused.error.is_not_authorized(), "{:?}", refused.error);
        assert!(!refused.tree_mutated());
        assert_on(&repository, first).await;
    }))
    .await
    .expect("the test task");
}

/// A tree loaded from no revision takes no second parent, as a merge revision needs a first parent: the
/// call is refused as an invalid argument before anything is written.
#[tokio::test]
async fn a_tree_loaded_from_no_revision_takes_no_second_parent() {
    let (_, _, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let (_, repository, (first, branch), other) = two_lines().await;

        let refused = commit_merge(&repository, (Hash::default(), branch), other)
            .await
            .expect_err("a second parent on a tree loaded from no revision");
        assert!(refused.error.is_invalid_arguments(), "{:?}", refused.error);
        assert!(!refused.tree_mutated());
        assert_on(&repository, (first, branch)).await;
    }))
    .await
    .expect("the test task");
}

/// Asserts that the branch of `(revision, branch)` is still at `revision`: a refused commit moves
/// no branch latest.
async fn assert_on(repository: &Arc<RepositoryContext>, (revision, branch): (Hash, BranchId)) {
    assert_eq!(
        branch::load_latest(repository.clone(), branch)
            .await
            .expect("the branch latest"),
        revision
    );
}
