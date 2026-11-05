// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The compare (`StateModel::compare`) names each way a State and its model differ: a State
//! changed after its model started, with no record of the change, differs in each aspect the
//! change touched.

use lore_base::types::Hash;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::GuardEvent;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_storage::hash::hash_string;

use crate::harness::catalogue::ops::file;
use crate::harness::fixtures::Tree;
use crate::harness::model;
use crate::harness::model::Model;
use crate::harness::model::Start;

/// `tree`'s model started and nothing replayed, equal to its State; then `change` made to the
/// State, with no scheduler installed and so no record: what the compare names, and the model.
fn compared_after(
    tree: impl Future<Output = Tree>,
    change: impl AsyncFnOnce(&Tree),
) -> (Vec<String>, Model, Tree) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the runtime starts");
    runtime.block_on(lore_base::runtime::LORE_CONTEXT.scope(
        crate::harness::fixtures::execution(),
        async move {
            let tree = tree.await;
            let model = Start::read(&tree).await.model(&tree, &[]).await;
            let before = model::compared(&model, &tree);
            assert!(before.is_empty(), "equal before the change: {before:#?}");
            change(&tree).await;
            (model::compared(&model, &tree), model, tree)
        },
    ))
}

/// A tree of `d/f` and `d/g`, and the link `d/l`.
async fn linked() -> Tree {
    let mut tree = Tree::new(&["d/f", "d/g"]).await;
    let link = Node {
        flags: NodeFlags::Link.bits(),
        name_hash: hash_string("l"),
        child: 40,
        ..Node::default()
    };
    let id = tree
        .state
        .node_add(tree.repository.clone(), tree.id("d"), link, "l")
        .await
        .expect("the link adds");
    tree.nodes.push(("d/l".to_string(), id));
    tree
}

/// The block holding `path`'s node.
async fn block_of(tree: &Tree, path: &str) -> std::sync::Arc<NodeBlock> {
    crate::harness::invariants::block_of(&tree.state, &tree.repository, tree.id(path)).await
}

/// Asserts `differences` holds one saying each of `what`.
fn names(differences: &[String], what: &[&str]) {
    for what in what {
        assert!(
            differences
                .iter()
                .any(|difference| difference.contains(what)),
            "a difference saying '{what}': {differences:#?}"
        );
    }
}

/// An edit of a node's record, by its path.
type Edit = (&'static str, fn(&mut Node));

#[test]
fn each_word_of_a_record_differs() {
    let edits: [Edit; 8] = [
        ("d/f", |node| node.flags |= NodeFlags::Dirty.bits()),
        ("d/f", |node| {
            node.ext_flags = NodeExtFlags::LocalContent.bits();
        }),
        ("d/f", |node| node.address.hash = Hash::from_u64(7)),
        ("d/f", |node| node.size = 41),
        ("d/f", |node| node.mode = 0o600),
        ("d/f", |node| node.parent = 0),
        ("d/f", |node| node.name_hash ^= 1),
        ("d/l", |node| node.child = 41),
    ];
    for (path, edit) in edits {
        let (differences, _, tree) = compared_after(linked(), async |tree: &Tree| {
            let block = block_of(tree, path).await;
            tree.state.node_edit_for_tests(&block, tree.id(path), edit);
        });
        names(&differences, &[&format!("{} is Node", tree.id(path))]);
    }
}

#[test]
fn a_chain_and_a_free_list_differ() {
    let (differences, _, _) = compared_after(Tree::new(&["d/f"]), async |tree: &Tree| {
        tree.state
            .node_add(tree.repository.clone(), tree.id("d"), file("n"), "n")
            .await
            .expect("the file adds");
    });
    names(&differences, &["'s chain", "node_count", "chain count"]);
    let (differences, _, _) = compared_after(Tree::new(&["d/f", "d/g"]), async |tree: &Tree| {
        lore_revision::state::node_discard_fused(
            tree.state.clone(),
            tree.repository.clone(),
            tree.id("d/f"),
        )
        .await
        .expect("the discard runs");
    });
    names(&differences, &["free list", "'s chain"]);
}

#[test]
fn a_count_and_a_pending_clear_differ() {
    let (differences, _, _) = compared_after(Tree::new(&["d/f"]), async |tree: &Tree| {
        let block = block_of(tree, "d").await;
        let index = Node::index(tree.id("d"));
        let writer = block.write_for_tests();
        writer.note_chain_changes(index, 1);
        writer.set_clear_pending(index);
    });
    names(&differences, &["chain count", "pending clear"]);
}

#[test]
fn the_states_flags_differ() {
    let (differences, _, _) = compared_after(Tree::new(&["d/f"]), async |tree: &Tree| {
        tree.state.mark_served();
        tree.state.note_unfinished_change();
    });
    names(&differences, &["served true", "an unfinished change true"]);
}

#[test]
fn a_reloaded_blocks_instance_differs() {
    let loaded = async {
        let tree = Tree::new(&["d/f"]).await.reloaded().await;
        tree.state
            .node(tree.repository.clone(), tree.id("d/f"))
            .await
            .expect("'d/f' reads");
        tree
    };
    let (differences, _, _) = compared_after(loaded, async |tree: &Tree| {
        tree.state.evict_blocks_for_tests();
        tree.state
            .node(tree.repository.clone(), tree.id("d/f"))
            .await
            .expect("'d/f' reads again");
    });
    names(&differences, &["instance"]);
}

#[test]
fn a_guard_left_armed_differs() {
    let (_, mut model, tree) = compared_after(Tree::new(&["d/f"]), async |_: &Tree| {});
    model.replay(&[Record {
        seq: 0,
        sub: 0,
        section: Section::Unlabelled,
        state: tree.state.hooks().id(),
        task: Some(3),
        effect: Effect::Guard {
            event: GuardEvent::Armed,
        },
    }]);
    names(&model::compared(&model, &tree), &["guards armed"]);
}

#[test]
fn a_free_slot_not_discarded_and_a_block_with_no_image_differ() {
    let freed = async {
        let tree = Tree::new(&["d/f", "d/g"]).await;
        lore_revision::state::node_discard_fused(
            tree.state.clone(),
            tree.repository.clone(),
            tree.id("d/f"),
        )
        .await
        .expect("'d/f' is discarded");
        tree
    };
    let (differences, _, _) = compared_after(freed, async |tree: &Tree| {
        let block = block_of(tree, "d").await;
        let free = tree.id("d/f");
        tree.state.node_edit_for_tests(&block, free, |node| {
            node.flags = 0;
        });
    });
    names(&differences, &["is free in the model, not Discarded"]);
    let (differences, _, _) = compared_after(Tree::new(&["d/f"]), async |tree: &Tree| {
        tree.state
            .take_new_nodes_from_fresh_blocks(tree.repository.clone())
            .await
            .expect("new nodes come from fresh blocks");
        tree.state
            .node_add(tree.repository.clone(), tree.id("d"), file("n"), "n")
            .await
            .expect("the file adds");
    });
    names(&differences, &["the model has no image of it"]);
}

/// A tree whose `d/f` was discarded, its slot heading the free list; the State's slot then given
/// a grabbed slot's words, `size` among them, under a parent and a name; the model told of a grab
/// of the slot with no `Word`, as a directory with no content makes none, and of its build's
/// abandon, so it never learns the parent and the name: what the compare names, and the slot.
fn abandoned_unplaced(size: u64) -> (Vec<String>, NodeID) {
    let freed = async {
        let tree = Tree::new(&["d/f", "d/g"]).await;
        lore_revision::state::node_discard_fused(
            tree.state.clone(),
            tree.repository.clone(),
            tree.id("d/f"),
        )
        .await
        .expect("'d/f' is discarded");
        tree
    };
    let (_, mut model, tree) = compared_after(freed, async move |tree: &Tree| {
        let block = block_of(tree, "d").await;
        let parent = tree.id("d");
        tree.state
            .node_edit_for_tests(&block, tree.id("d/f"), |node| {
                *node = Node {
                    flags: NodeFlags::Discarded.bits(),
                    parent,
                    name_hash: hash_string("r"),
                    size,
                    sibling: node.sibling,
                    ..Node::default()
                };
            });
    });
    let slot = tree.id("d/f");
    let instance = tree
        .state
        .resident_block(NodeBlock::index(slot))
        .expect("the slot's block is in memory")
        .hook_instance();
    let (state, task) = (tree.state.hooks().id(), Some(0));
    model.replay(&[
        Record {
            seq: 0,
            sub: 0,
            section: Section::Grab,
            state,
            task,
            effect: Effect::Grab {
                node: slot,
                instance,
            },
        },
        Record {
            seq: 1,
            sub: 0,
            section: Section::Abandon,
            state,
            task,
            effect: Effect::Abandon { root: slot },
        },
    ]);
    assert!(
        model.violations().is_empty(),
        "the grab and the abandon apply"
    );
    (model::compared(&model, &tree), slot)
}

#[test]
fn a_slot_the_model_never_placed_is_compared_by_its_words() {
    let (differences, slot) = abandoned_unplaced(0);
    names(&differences, &["free list"]);
    assert!(
        !differences
            .iter()
            .any(|difference| difference.contains(&format!("{slot} is Node"))),
        "a parent and a name the model never learnt differ in no record: {differences:#?}"
    );
    let (differences, slot) = abandoned_unplaced(7);
    names(&differences, &[&format!("{slot} is Node")]);
}
