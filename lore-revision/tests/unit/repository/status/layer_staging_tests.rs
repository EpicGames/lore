// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Whether a layer's staged state still holds staging worth its pin.

use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::layer::Layer;
use lore_revision::layer::LayerState;
use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::status::layer_holds_staging;
use lore_revision::state::Mark;
use lore_revision::state::State;
use lore_revision::state::count_staged_files;
use lore_storage::hash::hash_string;

use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::in_memory;
use crate::helper::in_memory::directory;
use crate::link::state_linking_a_tree;

/// Whether the layer drawing `source_path` of `state` holds staging.
async fn holds_staging(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    source_path: &str,
) -> bool {
    let layer = Layer {
        target_path: "mount".into(),
        source_path: source_path.into(),
        repository: repository.id,
        ..Default::default()
    };
    let layer_state = LayerState {
        repository,
        state_current: state.clone(),
        state_staged: state,
    };
    layer_holds_staging(&layer, &layer_state).await
}

/// A layer whose source path crosses a link holds staging where the tree below its source, in
/// the state the link pins, holds a dirty node, and none where that tree is clean.
#[tokio::test]
async fn a_layer_drawing_through_a_link_holds_staging_where_the_linked_tree_does() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            for dirty in [true, false] {
                let (repository, state, _) = state_linking_a_tree(dirty).await;

                assert_eq!(
                    holds_staging(repository, state, "lib/d").await,
                    dirty,
                    "a linked tree dirty: {dirty}"
                );
            }
        })
        .await;
}

/// A state holding `src/e`, a staged directory whose chain leads into a block the state lacks,
/// and beside it `src/a.txt` staged where `staged_sibling` says. Answers the repository and the
/// state.
async fn state_with_an_unreadable_directory(
    staged_sibling: bool,
) -> (Arc<RepositoryContext>, Arc<State>) {
    let (immutable, mutable, _execution) = test_store_create().await.expect("making test stores");
    let repository = in_memory::repository(immutable, mutable);
    let state = State::new();
    let source = state
        .node_add(repository.clone(), ROOT_NODE, directory("src"), "src")
        .await
        .expect("adding the source");
    let unreadable = state
        .node_add(repository.clone(), source, directory("e"), "e")
        .await
        .expect("adding the directory");
    let mut staged = vec![unreadable];
    if staged_sibling {
        staged.push(
            state
                .node_add(
                    repository.clone(),
                    source,
                    in_memory::file("a.txt"),
                    "a.txt",
                )
                .await
                .expect("adding the file"),
        );
    }
    for node in staged {
        state
            .node_mark(&repository, node, Mark::staged(NodeFlags::StagedAdd, true))
            .await
            .expect("staging the node");
    }
    let block = state
        .block(repository.clone(), NodeBlock::index(unreadable))
        .await
        .expect("the directory's block");
    block.write_for_tests().node(Node::index(unreadable)).child = (1000 * BLOCK_NODE_COUNT) as u32;
    (repository, state)
}

/// A layer whose staged tree can't be read below its source holds staging: a pin dropped on a
/// question that could not be answered loses staged work.
#[tokio::test]
async fn a_layer_whose_tree_cannot_be_read_below_its_source_holds_staging() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (repository, state) = state_with_an_unreadable_directory(false).await;

            assert!(holds_staging(repository, state, "src").await);
        })
        .await;
}

/// A count of staged files beside a directory it can't read keeps the files it read, and is not
/// complete.
#[tokio::test]
async fn a_staged_file_count_beside_an_unreadable_directory_keeps_what_it_read() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (repository, state) = state_with_an_unreadable_directory(true).await;
            let source = state
                .find_subnode(repository.clone(), ROOT_NODE, hash_string("src"))
                .await
                .expect("src");

            let staged = count_staged_files(repository, state, source).await;

            assert_eq!(staged.count, 1);
            assert!(!staged.complete);
        })
        .await;
}
