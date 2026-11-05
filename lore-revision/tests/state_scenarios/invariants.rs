// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The invariants checker's own checks: each shape it must report.

use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::Detach;
use lore_storage::hash::hash_string;

use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::harness::invariants::Rules;

/// The violations `tree` holds, which must be some.
async fn violations(tree: &Tree) -> Vec<String> {
    violations_under(tree, Rules::default()).await
}

/// The violations `tree` holds under `rules`, which must be some.
async fn violations_under(tree: &Tree, rules: Rules) -> Vec<String> {
    invariants::check(&tree.state, &tree.repository, rules)
        .await
        .expect_err("the checker reports the shape")
}

/// Asserts some violation of `found` says `what`.
fn reported(found: &[String], what: &str) {
    assert!(
        found.iter().any(|violation| violation.contains(what)),
        "'{what}' in {found:#?}"
    );
}

/// Runs `edit` on the record at `path`, as no primitive would change it.
async fn edited(tree: &Tree, path: &str, edit: impl FnOnce(&mut Node)) {
    let id = tree.id(path);
    let block = invariants::block_of(&tree.state, &tree.repository, id).await;
    tree.state.node_edit_for_tests(&block, id, edit);
}

/// A typed slot in no chain, naming a live directory as its parent: the shape an attach dropped
/// before its publish leaves (H8).
#[tokio::test]
async fn a_typed_slot_outside_every_chain_is_reported() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        let f = tree.id("d/f");
        tree.state
            .node_detach(tree.repository.clone(), f, Detach::Always)
            .await
            .expect("the detach");
        let block = invariants::block_of(&tree.state, &tree.repository, f).await;
        tree.state.node_edit_for_tests(&block, f, |node| {
            node.flags &= !NodeFlags::Discarded.bits();
        });
        let found = violations(&tree).await;
        assert!(
            found
                .iter()
                .any(|violation| violation.contains("is in no chain")),
            "{found:#?}"
        );
    })
    .await;
}

/// A typed node of a detached subtree whose Discarded root was freed before it.
#[tokio::test]
async fn a_typed_node_below_a_free_slot_is_reported() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        let d = tree.id("d");
        tree.state
            .node_detach(tree.repository.clone(), d, Detach::Always)
            .await
            .expect("the detach");
        lore_revision::state::node_discard_free(
            &tree.state,
            &tree.repository,
            &[],
            vec![d],
            |_, _| {},
        )
        .await
        .expect("d is freed");
        let found = violations(&tree).await;
        assert!(
            found
                .iter()
                .any(|violation| violation.contains("a free slot")),
            "{found:#?}"
        );
    })
    .await;
}

/// With no build in flight, as in a C API tree, a typed node below a detached root is reported,
/// where the default rules take it for a build's: every node of a subtree a C API delete
/// detaches is marked Discarded.
#[tokio::test]
async fn a_typed_node_below_a_detached_root_is_reported_with_no_builds() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        let d = tree.id("d");
        tree.state
            .node_detach(tree.repository.clone(), d, Detach::Always)
            .await
            .expect("the detach");
        invariants::check(&tree.state, &tree.repository, Rules::default())
            .await
            .unwrap_or_else(|violations| panic!("a build's shape passes: {violations:#?}"));
        let found = invariants::check(
            &tree.state,
            &tree.repository,
            Rules {
                no_builds: true,
                ..Rules::default()
            },
        )
        .await
        .expect_err("the checker reports the node");
        assert!(
            found
                .iter()
                .any(|violation| violation.contains("no build in flight")),
            "{found:#?}"
        );
    })
    .await;
}

/// A block on the tree's unused chain twice, here following itself: the shape a save threading the
/// chain's full head onto it leaves (H4).
#[tokio::test]
async fn a_block_twice_on_the_unused_chain_is_reported() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        assert_eq!(tree.state.unused_head(), 0, "block 0 heads the chain");
        let block = invariants::block_of(&tree.state, &tree.repository, tree.id("d")).await;
        block.write_for_tests().node_block().block_unused_next = 0;
        let found = violations(&tree).await;
        assert!(
            found
                .iter()
                .any(|violation| violation.contains("on the unused chain twice")),
            "{found:#?}"
        );
    })
    .await;
}

/// A block with room off the unused chain, not the last block and not marked for the next save
/// to thread: no grab reaches its free slots.
#[tokio::test]
async fn a_block_with_room_no_grab_reaches_is_reported() {
    crate::in_context(async {
        let tree = Tree::spread(&["d/"], "fill", &["e/"]).await;
        assert_eq!(
            tree.state.unused_head(),
            1,
            "block 1 heads the chain, block 0 left it full"
        );
        let fill = tree.id("fill");
        let freed = tree
            .state
            .node_children(tree.repository.clone(), fill)
            .await
            .expect("the fill's children")[0];
        lore_revision::state::node_discard_fused(
            tree.state.clone(),
            tree.repository.clone(),
            freed,
        )
        .await
        .expect("the discard");
        invariants::check(&tree.state, &tree.repository, Rules::default())
            .await
            .expect("a block marked and registered for the next save is reached");
        let block = invariants::block_of(&tree.state, &tree.repository, freed).await;
        assert!(
            block.write_for_tests().take_first_unused_node(),
            "the discard marked block 0 for the next save to thread"
        );
        let found = violations(&tree).await;
        assert!(
            found
                .iter()
                .any(|violation| violation.contains("has room no grab reaches")),
            "{found:#?}"
        );
    })
    .await;
}

/// A Discarded node a chain still reaches, which no detach leaves: discarded in a chain, and live.
#[tokio::test]
async fn a_discarded_node_in_a_chain_is_reported() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        edited(&tree, "d/f", |node| {
            node.flags |= NodeFlags::Discarded.bits();
        })
        .await;
        let found = violations(&tree).await;
        reported(&found, "discarded node");
        reported(&found, "live node");
    })
    .await;
}

/// A node in one chain naming another directory as its parent.
#[tokio::test]
async fn a_node_naming_another_parent_is_reported() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        edited(&tree, "d/f", |node| node.parent = ROOT_NODE).await;
        reported(&violations(&tree).await, "names parent");
    })
    .await;
}

/// A chain looping back to a node it reached.
#[tokio::test]
async fn a_node_reached_twice_is_reported() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f", "d/g"]).await;
        let head = tree.id("d/g");
        edited(&tree, "d/f", |node| node.sibling = head).await;
        reported(&violations(&tree).await, "reached twice");
    })
    .await;
}

/// A node whose name hash is another name's.
#[tokio::test]
async fn a_name_not_matching_its_hash_is_reported() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        edited(&tree, "d/f", |node| node.name_hash = hash_string("not f")).await;
        reported(&violations(&tree).await, "does not match its name hash");
    })
    .await;
}

/// Two siblings of one name hash, under the rule that keeps names unique.
#[tokio::test]
async fn siblings_sharing_a_name_hash_are_reported_where_names_are_unique() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f", "d/g"]).await;
        edited(&tree, "d/g", |node| node.name_hash = hash_string("f")).await;
        let rules = Rules {
            unique_names: true,
            ..Rules::default()
        };
        reported(&violations_under(&tree, rules).await, "hash as");
    })
    .await;
}

/// A node carrying extension flags, under the rule of a State never served.
#[tokio::test]
async fn extension_flags_are_reported_on_a_state_never_served() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        edited(&tree, "d/f", |node| node.ext_flags = 1).await;
        let rules = Rules {
            no_ext_flags: true,
            ..Rules::default()
        };
        reported(&violations_under(&tree, rules).await, "carries ext flags");
    })
    .await;
}

/// A pending clear no settle drops.
#[tokio::test]
async fn a_pending_clear_outliving_its_settle_is_reported() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        let d = tree.id("d");
        let block = invariants::block_of(&tree.state, &tree.repository, d).await;
        let writer = block.write_for_tests();
        writer.set_clear_pending(Node::index(d));
        drop(writer);
        reported(
            &violations(&tree).await,
            "a pending clear outlived its settle",
        );
    })
    .await;
}

/// A block's free list reaching past its node count, through a typed slot, and back onto itself.
#[tokio::test]
async fn a_free_list_past_its_count_through_a_typed_slot_or_looping_is_reported() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        let f = tree.id("d/f");
        let block = invariants::block_of(&tree.state, &tree.repository, f).await;
        let free_list = |head: u32, count: u32| {
            let mut writer = block.write_for_tests();
            let data = writer.node_block();
            data.node_unused = head;
            data.node_unused_count = count;
        };
        let count = block.read().node_block().node_count;
        free_list(count + 1, 1);
        reported(&violations(&tree).await, "past its count");
        free_list(Node::index(f) as u32, 1);
        reported(&violations(&tree).await, "is not discarded");

        let tree = Tree::new(&["d/f"]).await;
        let f = tree.id("d/f");
        tree.state
            .node_detach(tree.repository.clone(), f, Detach::Always)
            .await
            .expect("the detach");
        lore_revision::state::node_discard_free(
            &tree.state,
            &tree.repository,
            &[],
            vec![f],
            |_, _| {},
        )
        .await
        .expect("f is freed");
        let block = invariants::block_of(&tree.state, &tree.repository, f).await;
        {
            let mut writer = block.write_for_tests();
            let data = writer.node_block();
            let head = data.node_unused;
            data.node_unused_count += 1;
            data.node[head as usize].sibling = head;
        }
        reported(&violations(&tree).await, "loops");
    })
    .await;
}
