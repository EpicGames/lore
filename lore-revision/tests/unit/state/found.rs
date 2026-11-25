// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::lore::RepositoryId;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::FoundNode;
use lore_revision::state::NodeMapping;
use lore_revision::state::State;
use lore_revision::state::allow_all_repositories;
use lore_revision::util::path::RelativePath;

use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::in_memory::add;
use crate::helper::in_memory::directory;
use crate::helper::in_memory::file;
use crate::helper::in_memory::link_node;
use crate::helper::in_memory::linked_revision_holding;
use crate::helper::in_memory::revision_state;

/// The repository [`parent_linking_a_readme`] links.
fn linked() -> RepositoryId {
    RepositoryId::from([1; 16])
}

/// A tree holding `README.txt` and, at `vendor/lib`, a link to [`linked`] holding a `README.txt`
/// of its own, which both number alike. Answers the tree, the parent's `README.txt` and the pin.
async fn parent_linking_a_readme(
    repository: &Arc<RepositoryContext>,
) -> (Arc<State>, NodeID, Hash) {
    let pin = linked_revision_holding(repository, linked(), &["README.txt"]).await;
    let state = State::new();
    let own = add(
        &state,
        repository.clone(),
        ROOT_NODE,
        file("README.txt"),
        "README.txt",
    )
    .await;
    let vendor = add(
        &state,
        repository.clone(),
        ROOT_NODE,
        directory("vendor"),
        "vendor",
    )
    .await;
    add(
        &state,
        repository.clone(),
        vendor,
        link_node("lib", linked(), pin),
        "lib",
    )
    .await;
    (state, own, pin)
}

/// Runs `test` on the tree [`parent_linking_a_readme`] builds, with its repository.
async fn with_parent_linking_a_readme<F, Fut>(test: F)
where
    F: FnOnce(Arc<RepositoryContext>, Arc<State>, NodeID, Hash) -> Fut,
    Fut: Future<Output = ()>,
{
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable_store, mutable_store, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = Arc::new(RepositoryContext::new_null_context(
                immutable_store,
                mutable_store,
                allow_all_repositories(),
            ));
            let (state, own, pin) = parent_linking_a_readme(&repository).await;
            test(repository, state, own, pin).await;
        })
        .await;
}

/// A path the lookup reaches through a link names the linked node, held by the linked state, even
/// where the tree searched numbers one of its own nodes alike; a path of the tree searched names
/// its own node, held by the pair the lookup ran on.
#[tokio::test]
async fn a_found_node_is_held_by_the_tree_that_numbers_it() {
    with_parent_linking_a_readme(|repository, state, own, pin| async move {
        let searched = (repository.clone(), state.clone());

        let across = state
            .locate_node(repository.clone(), "vendor/lib/README.txt")
            .await
            .expect("the linked file");
        assert_eq!(across.identity().node, own);
        assert!(across.crossed_link());
        assert_eq!(across.own_node(), None);
        let held = across.held(searched.clone());
        assert_eq!(held.repository().id, linked());
        assert_eq!(held.state().revision(), pin);
        let linked_node = revision_state(held.repository(), pin)
            .await
            .node(held.repository().clone(), own)
            .await
            .expect("the node the linked revision holds");
        assert_eq!(
            held.read().await.expect("the linked node").address,
            linked_node.address
        );

        let parent = state
            .locate_node(repository.clone(), "README.txt")
            .await
            .expect("the parent file");
        assert!(!parent.crossed_link());
        assert_eq!(parent.own_node(), Some(own));
        let held = parent.held(searched);
        assert!(Arc::ptr_eq(held.repository(), &repository));
        assert!(Arc::ptr_eq(held.state(), &state));
    })
    .await;
}

/// A node reached through a link is not the tree's own: asked for as its own, it is not found.
#[tokio::test]
async fn a_node_across_a_link_is_not_found_as_the_trees_own() {
    with_parent_linking_a_readme(|repository, state, own, _| async move {
        let own_at = async |path| {
            state
                .locate_node(repository.clone(), path)
                .await
                .and_then(FoundNode::into_own_node)
        };

        assert_eq!(own_at("README.txt").await.expect("the parent file"), own);
        assert!(
            own_at("vendor/lib/README.txt")
                .await
                .expect_err("the linked file is not the parent's")
                .is_node_not_found()
        );
    })
    .await;
}

/// A mapping holding nothing at a path answers an invalid node there, which nothing holds; at a
/// path through a link it answers the linked node, held by the linked state.
#[tokio::test]
async fn a_mapping_answers_an_invalid_node_where_it_holds_none() {
    with_parent_linking_a_readme(|repository, state, _, pin| async move {
        let root = NodeMapping::root(repository, state);
        let missing = RelativePath::new_from_initial_path("missing.txt").expect("a path");
        let across = RelativePath::new_from_initial_path("vendor/lib/README.txt").expect("a path");

        assert_eq!(root.node_at(&missing).await.identity().node, INVALID_NODE);
        assert!(root.held_at(&missing).await.is_none());
        let held = root.held_at(&across).await.expect("the linked file");
        assert_eq!(
            (held.repository().id, held.state().revision()),
            (linked(), pin)
        );
    })
    .await;
}
