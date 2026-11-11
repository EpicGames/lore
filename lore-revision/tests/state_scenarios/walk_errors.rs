// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A block a node operation must load fails to, through the test store's evictions and failures:
//! the clears' loads, walks and climbs, the rewrites', restores', modifies' and delete levels'
//! loads and climbs, the slot of an add, an attach and a build, and the finds of a scan's and
//! `lore dirty`'s directory chains. Each answers the error, its directory left dirty where it was,
//! closure kept but where a climb failed part way (C12), a modify or a delete level whose climb
//! began noting an unfinished change, a slot it grabbed freed again, and a chain adding no second
//! directory of a name. The scenarios beside an eviction evict the blocks and fail the store at
//! every point of the operation, and name the stage each run failed at, so each shows it reached
//! the arm it is there for.
//!
//! Loads that cannot fail, so have no test, each of a block the operation, or a change it
//! follows, wrote, which stays in memory, dirty, until a save, and no save runs beside a node
//! operation (Serialize's tier is its own). A served State is built from a clean revision and
//! never saved, so a dirty flag in it was written in memory, its block dirty.
//! - The served rewrite's load of the parent it follows a moved node to
//!   (`ServedNodes::node_rewrite`): the move wrote the new parent's block. The copy-up's retry load
//!   of the directory that refused it (`ServedNodes::node_copy_up`): the copy-up's mark wrote it,
//!   or it refused only for a pending clear, and so was dirty already.
//! - The restore's parent load on the attempt after it found its node moved
//!   (`State::restore_below`), and the modify's climb from the parent a move took its file to
//!   (`State::node_modify_staged`): the move wrote that parent's block, and the one mover beside a
//!   restore or a modify, the C API move (`node_move_staged`), marks it first, so the climb stops
//!   there.
//! - The slot's release in every `release_on_error` (`State::release_node`): the grab wrote the
//!   slot's block.
//! - A build's load of the directory it adds into (`TreeBuild::add`): a node the build wrote.
//! - The served clear's load of the directory it clears (`ServedNodes::node_clear`), dirty by the
//!   clear's precondition, though not its names.
//! - An attach's slot loads (`State::prepare_child`) on a State taking new nodes from fresh
//!   blocks, whose blocks are dirty and hold their names and no stored file metadata.
//!
//! Loads whose failure answers the error with nothing left to undo, reached by the scenarios but
//! named by no stage of their own:
//! - The unserved clear's re-mark after its second check of the children (`State::node_clear`)
//!   writes the directory before it loads its parent, so a load failing there leaves closure.
//! - The copy-up's read of its file once its mark finds the directory gone
//!   (`ServedNodes::node_copy_up`), which fails only where the file stays below a directory
//!   detached with its subtree: its answer and a retry's are the same `NodeNotFound`, or the
//!   store's error; the copy-up beside a detach reaches it.
//! - The grab's load of the unused chain's head block (`State::grab_node_slot`), before any slot
//!   is taken: the two-block build's `add` stage.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore_base::types::Address;
use lore_base::types::Hash;
use lore_revision::node;
use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFileMetadataBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::AttachCheck;
use lore_revision::state::AttachOutcome;
use lore_revision::state::Clear;
use lore_revision::state::Content;
use lore_revision::state::Ext;
use lore_revision::state::LevelMark;
use lore_revision::state::Mark;
use lore_revision::state::Rewrite;
use lore_revision::state::State;
use lore_revision::state::StateError;
use lore_revision::state::TreeAttach;
use lore_storage::hash::hash_string;

use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::ops;
use crate::harness::catalogue::ops::file;
use crate::harness::faults::FailingStore;
use crate::harness::fixtures::Tree;
use crate::harness::invariants::Report;
use crate::harness::invariants::Rules;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;

/// The invariants of a State, its dirty closure included.
async fn closure_holds(tree: &Tree) -> Report {
    crate::holding(
        &tree.state,
        &tree.repository,
        Rules {
            dirty_closure: true,
            ..Rules::default()
        },
    )
    .await
}

/// `node`'s record, its block loaded where it is not in memory.
async fn record(tree: &Tree, node: NodeID) -> Node {
    tree.state
        .node(tree.repository.clone(), node)
        .await
        .expect("the node reads")
}

/// Whether `node`'s block is in memory.
fn resident(tree: &Tree, node: NodeID) -> bool {
    tree.state.resident_block(NodeBlock::index(node)).is_some()
}

/// Loads `held`'s block and asserts `other`'s is not in memory, so only `other`'s load is left
/// to fail.
async fn only_resident(tree: &Tree, held: NodeID, other: NodeID) {
    let _held = record(tree, held).await;
    assert!(
        !resident(tree, other),
        "the block of node {other} is not in memory"
    );
}

/// `node`'s record where its block is in memory, read with the store failing as it is.
fn in_memory(tree: &Tree, node: NodeID) -> Option<Node> {
    tree.state
        .resident_block(NodeBlock::index(node))
        .map(|block| block.node(Node::index(node)))
}

/// `p/a` in the first block and `p/b` in the second, so `p`'s chain, headed by `p/b`, runs from
/// the second block into the first; loaded afresh through a failing store, no block in memory.
async fn spread_tree() -> (Tree, Arc<FailingStore>) {
    Tree::spread(&["p/a"], "fill", &["p/b"])
        .await
        .failing()
        .await
}

/// `q` in the first block, `q/r` with `q/r/f` and `q/r/g` in the second, so a climb from `q/r`
/// leaves the second block for the first; `dirty` marked dirty before the tree is saved and
/// loaded afresh through a failing store, no block in memory.
async fn deep_tree(dirty: &[&str]) -> (Tree, Arc<FailingStore>) {
    let tree = Tree::spread(&["q/"], "fill", &["q/r/f", "q/r/g"]).await;
    for path in dirty {
        ops::mark_dirty(&tree, tree.id(path)).await;
    }
    tree.failing().await
}

/// Content for a rewrite, restore or modify of `path`, other than the content it was built with.
fn content_of(path: &str) -> Content {
    Content {
        mode: 0,
        size: 7,
        address: Address::zero_context_hash(Hash::from_u64(hash_string(&format!("{path}'")))),
    }
}

/// `node` staged for deletion below `parent` through the C API delete's level mark.
async fn delete_level(tree: &Tree, node: NodeID, parent: NodeID) {
    let marked = tree
        .state
        .node_delete_level(&tree.repository, node, parent)
        .await
        .expect("the delete marks its level");
    assert!(matches!(marked, LevelMark::Marked(_)), "{marked:?}");
}

/// `deep_tree` with `q` marked dirty, so its block stays in memory through an eviction, taking
/// new nodes from fresh blocks, so an add into `q` publishes without a load, and `q/r`'s block
/// alone not in memory: a find of `r` in `q` is left to fail, and an add of a second `r` to
/// succeed.
async fn chain_tree() -> (Tree, Arc<FailingStore>) {
    let (tree, store) = deep_tree(&[]).await;
    tree.state
        .node_mark(
            &tree.repository,
            tree.id("q"),
            Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
        )
        .await
        .expect("'q' is marked");
    tree.state
        .take_new_nodes_from_fresh_blocks(tree.repository.clone())
        .await
        .expect("the State takes fresh blocks");
    only_resident(&tree, tree.id("q"), tree.id("q/r")).await;
    (tree, store)
}

/// Asserts `tree`'s dirty closure, and that no directory holds two children of one name.
async fn closed_and_unique(tree: &Tree) {
    crate::holding(
        &tree.state,
        &tree.repository,
        Rules {
            dirty_closure: true,
            unique_names: true,
            ..Rules::default()
        },
    )
    .await;
}

/// A scan's chain whose find of a directory the State holds fails to load answers the error and
/// adds no second directory of the name.
#[tokio::test]
async fn a_scan_chain_whose_find_fails_to_load_adds_no_second_directory() {
    crate::in_context(async {
        let (tree, store) = chain_tree().await;

        store.fail(true);
        let chained = lore_revision::state::os_diff::ensure_scan_dir_chain(
            tree.repository.clone(),
            tree.state.clone(),
            tree.id("q"),
            "r",
        )
        .await;
        store.fail(false);

        let error = chained.expect_err("the find's error is answered");
        assert!(!error.is_node_not_found(), "the store's error: {error}");
        closed_and_unique(&tree).await;
    })
    .await;
}

/// The same for `lore dirty`'s chain to the directory of a path it adds.
#[tokio::test]
async fn a_dirty_chain_whose_find_fails_to_load_adds_no_second_directory() {
    crate::in_context(async {
        let (tree, store) = chain_tree().await;

        store.fail(true);
        let chained = lore_revision::file::dirty::ensure_dirty_parent_dirs(
            &tree.repository,
            &tree.state,
            tree.id("q"),
            "r",
        )
        .await;
        store.fail(false);

        let error = chained.expect_err("the find's error is answered");
        assert!(!error.is_node_not_found(), "the store's error: {error}");
        closed_and_unique(&tree).await;
    })
    .await;
}

/// An unserved clear of a dirty file whose block fails to load clears nothing.
#[tokio::test]
async fn an_unserved_clear_whose_block_fails_to_load_clears_nothing() {
    crate::in_context(async {
        let (tree, store) = deep_tree(&["q/r/f"]).await;
        let f = tree.id("q/r/f");
        assert!(!resident(&tree, f), "the file's block is not in memory");

        store.fail(true);
        let cleared = tree
            .state
            .node_clear(&tree.repository, f, Clear::Dirty)
            .await;
        store.fail(false);

        assert!(cleared.is_err(), "the file's block fails to load");
        assert!(record(&tree, f).await.is_dirty(), "'q/r/f' stays dirty");
        closure_holds(&tree).await;
    })
    .await;
}

/// An unserved clear of a file whose directory's chain reaches a block that fails to load: the
/// file is cleared, the walk over the directory's children answers the error, and the directory
/// stays dirty above its clean file.
#[tokio::test]
async fn an_unserved_clear_whose_walk_fails_to_load_keeps_its_directory_dirty() {
    crate::in_context(async {
        let (tree, store) = spread_tree().await;
        let (p, a, b) = (tree.id("p"), tree.id("p/a"), tree.id("p/b"));
        ops::mark_dirty(&tree, a).await;
        assert!(!resident(&tree, b), "the block of 'p/b' is not in memory");

        store.fail(true);
        let cleared = tree
            .state
            .node_clear(&tree.repository, a, Clear::Dirty)
            .await;
        store.fail(false);

        assert!(cleared.is_err(), "the walk fails to load 'p/b''s block");
        assert!(!record(&tree, a).await.is_dirty(), "'p/a' is cleared");
        assert!(record(&tree, p).await.is_dirty(), "'p' stays dirty");
        closure_holds(&tree).await;
    })
    .await;
}

/// An unserved clear whose climb reaches an ancestor in a block that fails to load: the levels
/// below are cleared, and the ancestor stays dirty above them.
#[tokio::test]
async fn an_unserved_clear_whose_climb_fails_to_load_keeps_the_ancestor_dirty() {
    crate::in_context(async {
        let (tree, store) = deep_tree(&["q/r/f"]).await;
        let (q, r, f) = (tree.id("q"), tree.id("q/r"), tree.id("q/r/f"));
        let _f = record(&tree, f).await;
        assert!(!resident(&tree, q), "the block of 'q' is not in memory");

        store.fail(true);
        let cleared = tree
            .state
            .node_clear(&tree.repository, f, Clear::Dirty)
            .await;
        store.fail(false);

        assert!(cleared.is_err(), "the climb fails to load 'q''s block");
        assert!(!record(&tree, f).await.is_dirty(), "'q/r/f' is cleared");
        assert!(!record(&tree, r).await.is_dirty(), "'q/r' is cleared");
        assert!(record(&tree, q).await.is_dirty(), "'q' stays dirty");
        closure_holds(&tree).await;
    })
    .await;
}

/// `tree` served with `p` marked dirty by a served mark, as a local change leaves it, and, with
/// `names`, the names of `p`'s block in memory, which a served clear loads before anything else.
async fn served_with_p_dirty(tree: &Tree, names: bool) {
    tree.state.mark_served();
    let p = tree.id("p");
    assert!(ops::served_mark_dir(tree, p).await);
    if names {
        let _names = tree
            .state
            .block_with_nametable(tree.repository.clone(), NodeBlock::index(p))
            .await
            .expect("'p''s names load");
    }
}

/// Asserts `p`, which a served clear failed on, stays dirty with no clear pending.
async fn kept_dirty_with_no_clear_pending(tree: &Tree) {
    let p = tree.id("p");
    assert!(record(tree, p).await.is_dirty(), "'p' stays dirty");
    let block = tree
        .state
        .resident_block(NodeBlock::index(p))
        .expect("'p''s block is in memory");
    assert!(
        !block.clear_pending(Node::index(p)),
        "no clear of 'p' stays pending"
    );
}

/// A served clear whose directory's names fail to load, the first thing it loads: the
/// directory stays dirty.
#[tokio::test]
async fn a_served_clear_whose_names_fail_to_load_keeps_its_directory_dirty() {
    crate::in_context(async {
        let (tree, store) = spread_tree().await;
        served_with_p_dirty(&tree, false).await;
        let block = tree
            .state
            .resident_block(NodeBlock::index(tree.id("p")))
            .expect("'p''s block is in memory");
        assert!(
            !block.is_nametable_deserialized(),
            "'p''s names are not in memory"
        );

        store.fail(true);
        let cleared = ops::served_clear(&tree, tree.id("p")).await;
        store.fail(false);

        assert!(cleared.is_err(), "'p''s names fail to load");
        assert!(
            !block.is_nametable_deserialized(),
            "the clear failed loading 'p''s names"
        );
        kept_dirty_with_no_clear_pending(&tree).await;
        closure_holds(&tree).await;
    })
    .await;
}

/// A served clear whose walk over the directory's own chain reaches a block that fails to load:
/// its pending clear goes, and the directory stays dirty, the action bits its first phase drops
/// gone whatever the outcome (H20).
#[tokio::test]
async fn a_served_clear_whose_walk_fails_to_load_keeps_its_directory_dirty() {
    crate::in_context(async {
        let (tree, store) = spread_tree().await;
        served_with_p_dirty(&tree, true).await;
        let p = tree.id("p");
        ops::mark_added(&tree, p).await;
        assert!(
            !resident(&tree, tree.id("p/b")),
            "the block of 'p/b' is not in memory"
        );

        store.fail(true);
        let cleared = ops::served_clear(&tree, p).await;
        store.fail(false);

        assert!(cleared.is_err(), "the walk fails to load 'p/b''s block");
        kept_dirty_with_no_clear_pending(&tree).await;
        closure_holds(&tree).await;
        assert!(
            !record(&tree, p).await.is_dirty_add(),
            "the first phase dropped the action bits"
        );
    })
    .await;
}

/// A served clear whose committed directory's block fails to load: it answers the error before
/// it begins, and the directory stays dirty.
#[tokio::test]
async fn a_served_clear_whose_committed_directory_fails_to_load_keeps_it_dirty() {
    crate::in_context(async {
        let (mut tree, store) = spread_tree().await;
        tree.reload_committed().await;
        served_with_p_dirty(&tree, true).await;
        let _b = record(&tree, tree.id("p/b")).await;
        assert!(
            tree.committed
                .resident_block(NodeBlock::index(tree.committed_id("p")))
                .is_none(),
            "the committed directory's block is not in memory"
        );

        store.fail(true);
        let cleared = ops::served_clear(&tree, tree.id("p")).await;
        store.fail(false);

        assert!(
            cleared.is_err(),
            "the committed directory's block fails to load"
        );
        assert!(
            tree.committed
                .resident_block(NodeBlock::index(tree.committed_id("p")))
                .is_none(),
            "the clear failed loading the committed directory's block"
        );
        kept_dirty_with_no_clear_pending(&tree).await;
        closure_holds(&tree).await;
    })
    .await;
}

/// A served clear whose walk over the committed directory's chain reaches a block that fails to
/// load: the directory, which has more children than a block holds, stays dirty.
#[tokio::test]
async fn a_served_clear_whose_committed_walk_fails_to_load_keeps_its_directory_dirty() {
    crate::in_context(async {
        let names: Vec<String> = (0..BLOCK_NODE_COUNT + 8)
            .map(|index| format!("p/f{index}"))
            .collect();
        let paths: Vec<&str> = names.iter().map(String::as_str).collect();
        let (mut tree, store) = Tree::new(&paths).await.failing().await;
        tree.reload_committed().await;
        served_with_p_dirty(&tree, true).await;
        for path in &paths {
            let _child = record(&tree, tree.id(path)).await;
        }
        assert!(
            paths.iter().all(|path| resident(&tree, tree.id(path))),
            "the served chain's blocks are in memory"
        );
        let committed_head = tree
            .committed
            .node(tree.repository.clone(), tree.committed_id("p"))
            .await
            .expect("the committed directory reads")
            .child();
        let head_block = NodeBlock::index(committed_head.expect("a committed child"));
        assert_ne!(
            head_block,
            NodeBlock::index(tree.committed_id("p")),
            "the committed chain starts in another block than the directory's"
        );
        assert!(
            tree.committed.resident_block(head_block).is_none(),
            "the committed chain's first block is not in memory"
        );

        store.fail(true);
        let cleared = ops::served_clear(&tree, tree.id("p")).await;
        store.fail(false);

        assert!(
            cleared.is_err(),
            "the committed chain's block fails to load"
        );
        assert!(
            tree.committed.resident_block(head_block).is_none(),
            "the clear failed loading the committed chain's block"
        );
        kept_dirty_with_no_clear_pending(&tree).await;
        closure_holds(&tree).await;
    })
    .await;
}

/// A served clear whose children match but whose committed name fails to load: the last phase
/// would compare the name, so the clear answers the error and the directory stays dirty.
#[tokio::test]
async fn a_served_clear_whose_committed_name_fails_to_load_keeps_its_directory_dirty() {
    crate::in_context(async {
        let (mut tree, store) = spread_tree().await;
        tree.reload_committed().await;
        served_with_p_dirty(&tree, true).await;
        let _b = record(&tree, tree.id("p/b")).await;
        let committed_p = tree.committed_id("p");
        let _committed = tree
            .committed
            .node(tree.repository.clone(), committed_p)
            .await
            .expect("the committed directory reads");
        let committed_block = tree
            .committed
            .resident_block(NodeBlock::index(committed_p))
            .expect("the committed directory's block is in memory");
        assert!(
            !committed_block.is_nametable_deserialized(),
            "the committed names are not in memory"
        );

        store.fail(true);
        let cleared = ops::served_clear(&tree, tree.id("p")).await;
        store.fail(false);

        assert!(cleared.is_err(), "the committed name fails to load");
        kept_dirty_with_no_clear_pending(&tree).await;
        closure_holds(&tree).await;
    })
    .await;
}

/// An unserved rewrite of a file whose block fails to load writes nothing.
#[tokio::test]
async fn an_unserved_rewrite_whose_block_fails_to_load_writes_nothing() {
    crate::in_context(async {
        let (tree, store) = spread_tree().await;
        let b = tree.id("p/b");
        let content = content_of("p/b");

        store.fail(true);
        let rewritten = tree
            .state
            .node_rewrite(&tree.repository, b, &content, Rewrite::IfClean)
            .await;
        store.fail(false);

        assert!(rewritten.is_err(), "the file's block fails to load");
        let held = record(&tree, b).await;
        assert_ne!(
            held.address.hash, content.address.hash,
            "nothing is written"
        );
        assert!(!held.is_dirty(), "the file stays clean");
    })
    .await;
}

/// A served copy-up whose file's block, or whose directory's block, fails to load writes
/// nothing, and leaves the directory as it was.
#[tokio::test]
async fn a_copy_up_whose_block_fails_to_load_writes_nothing() {
    crate::in_context(async {
        for directory_resident in [true, false] {
            let (tree, store) = spread_tree().await;
            tree.state.mark_served();
            let (p, b) = (tree.id("p"), tree.id("p/b"));
            if directory_resident {
                only_resident(&tree, p, b).await;
            } else {
                only_resident(&tree, b, p).await;
            }

            store.fail(true);
            let copied = tree
                .state
                .served()
                .node_copy_up(
                    &tree.repository,
                    b,
                    p,
                    (0, 3),
                    (NodeFlags::DirtyModify, NodeExtFlags::LocalContent),
                )
                .await;
            store.fail(false);

            assert!(copied.is_err(), "a block fails to load");
            let held = record(&tree, b).await;
            assert!(
                !held.is_dirty() && held.size != 3,
                "nothing is written to the file"
            );
            assert!(
                !record(&tree, p).await.is_dirty(),
                "the directory stays clean"
            );
            closure_holds(&tree).await;
        }
    })
    .await;
}

/// A copy-up whose mark of the directory refusing it fails to load an ancestor's block answers
/// the store's error at once, not a retry's `SlowDown`: the file unwritten, the directory clean.
#[tokio::test]
async fn a_copy_up_whose_mark_fails_to_load_answers_its_error() {
    crate::in_context(async {
        let (tree, store) = deep_tree(&[]).await;
        tree.state.mark_served();
        let (q, r, f) = (tree.id("q"), tree.id("q/r"), tree.id("q/r/f"));
        // Held, so the rewrite's own loads, of the file and its directory, cannot fail.
        let _held = tree
            .state
            .block(tree.repository.clone(), NodeBlock::index(f))
            .await
            .expect("the file's block loads");
        assert!(!resident(&tree, q), "the block of 'q' is not in memory");

        store.fail(true);
        let copied = tree
            .state
            .served()
            .node_copy_up(
                &tree.repository,
                f,
                r,
                (0, 3),
                (NodeFlags::DirtyModify, NodeExtFlags::LocalContent),
            )
            .await;
        store.fail(false);

        let error = copied.expect_err("the mark fails to load 'q''s block");
        assert!(
            !matches!(error, StateError::SlowDown(_)),
            "the store's error, answered at once: {error}"
        );
        let held = record(&tree, f).await;
        assert!(
            !held.is_dirty() && held.size != 3,
            "nothing is written to the file"
        );
        assert!(
            !record(&tree, r).await.is_dirty(),
            "the directory stays clean"
        );
        closure_holds(&tree).await;
    })
    .await;
}

/// A restore of a file staged for deletion whose block, or whose directory's block, fails to
/// load restores nothing.
#[tokio::test]
async fn a_restore_whose_block_fails_to_load_restores_nothing() {
    crate::in_context(async {
        for directory_resident in [true, false] {
            let tree = Tree::spread(&["p/a"], "fill", &["p/b"]).await;
            let (p, b) = (tree.id("p"), tree.id("p/b"));
            delete_level(&tree, b, p).await;
            let (tree, store) = tree.failing().await;
            if directory_resident {
                only_resident(&tree, p, b).await;
            } else {
                only_resident(&tree, b, p).await;
            }

            store.fail(true);
            let restored = tree
                .state
                .node_restore(&tree.repository, b, &content_of("p/b"))
                .await;
            store.fail(false);

            assert!(restored.is_err(), "a block fails to load");
            assert!(
                record(&tree, b).await.is_staged_delete(),
                "'p/b' stays staged for deletion"
            );
            closure_holds(&tree).await;
        }
    })
    .await;
}

/// A modify whose file's block, or whose directory's block, fails to load, before any climb,
/// writes nothing and notes no unfinished change.
#[tokio::test]
async fn a_modify_whose_block_fails_to_load_writes_nothing() {
    crate::in_context(async {
        for directory_resident in [true, false] {
            let (tree, store) = spread_tree().await;
            let (p, b) = (tree.id("p"), tree.id("p/b"));
            if directory_resident {
                only_resident(&tree, p, b).await;
            } else {
                only_resident(&tree, b, p).await;
            }

            store.fail(true);
            let modified = tree
                .state
                .node_modify_staged(&tree.repository, b, &content_of("p/b"))
                .await;
            store.fail(false);

            assert!(modified.is_err(), "a block fails to load");
            assert!(
                !record(&tree, b).await.is_staged(),
                "nothing is written to 'p/b'"
            );
            assert!(
                !tree.state.has_unfinished_change(),
                "no climb began, so nothing is unfinished"
            );
            closure_holds(&tree).await;
        }
    })
    .await;
}

/// A modify whose climb reaches an ancestor in a block that fails to load: it answers the error
/// with the file unwritten and an unfinished change noted, the levels it marked standing.
#[tokio::test]
async fn a_modify_whose_climb_fails_to_load_notes_an_unfinished_change() {
    crate::in_context(async {
        let (tree, store) = deep_tree(&[]).await;
        let (q, r, f) = (tree.id("q"), tree.id("q/r"), tree.id("q/r/f"));
        let _f = record(&tree, f).await;
        assert!(!resident(&tree, q), "the block of 'q' is not in memory");

        store.fail(true);
        let modified = tree
            .state
            .node_modify_staged(&tree.repository, f, &content_of("q/r/f"))
            .await;
        store.fail(false);

        assert!(modified.is_err(), "the climb fails to load 'q''s block");
        assert!(
            !record(&tree, f).await.is_staged(),
            "nothing is written to 'q/r/f'"
        );
        assert!(record(&tree, r).await.is_staged(), "'q/r' is marked");
        assert!(
            tree.state.has_unfinished_change(),
            "the modify notes an unfinished change"
        );
        crate::holding(&tree.state, &tree.repository, Rules::default()).await;
    })
    .await;
}

/// A modify whose climb ends at the root from outside the root's block, which it dirties last,
/// and whose load of that block fails: it answers the error with the file unwritten and an
/// unfinished change noted, the directory it marked standing.
#[tokio::test]
async fn a_modify_whose_climb_fails_to_load_the_root_block_notes_an_unfinished_change() {
    crate::in_context(async {
        let (tree, store) = Tree::spread(&[], "fill", &["e/x"]).await.failing().await;
        let (e, x) = (tree.id("e"), tree.id("e/x"));
        let _x = record(&tree, x).await;
        assert_ne!(NodeBlock::index(e), 0, "'e' lies outside the root's block");
        assert!(
            tree.state.resident_block(0).is_none(),
            "the root's block is not in memory"
        );

        store.fail(true);
        let modified = tree
            .state
            .node_modify_staged(&tree.repository, x, &content_of("e/x"))
            .await;
        store.fail(false);

        assert!(
            modified.is_err(),
            "the climb fails to load the root's block"
        );
        assert!(
            !record(&tree, x).await.is_staged(),
            "nothing is written to 'e/x'"
        );
        assert!(record(&tree, e).await.is_staged(), "'e' is marked");
        assert!(
            tree.state.has_unfinished_change(),
            "the modify notes an unfinished change"
        );
        crate::holding(&tree.state, &tree.repository, Rules::default()).await;
    })
    .await;
}

/// A delete's level mark whose node's block, or whose directory's block, fails to load marks
/// nothing and notes no unfinished change.
#[tokio::test]
async fn a_delete_level_whose_block_fails_to_load_marks_nothing() {
    crate::in_context(async {
        for directory_resident in [true, false] {
            let (tree, store) = spread_tree().await;
            let (p, b) = (tree.id("p"), tree.id("p/b"));
            if directory_resident {
                only_resident(&tree, p, b).await;
            } else {
                only_resident(&tree, b, p).await;
            }

            store.fail(true);
            let marked = tree.state.node_delete_level(&tree.repository, b, p).await;
            store.fail(false);

            assert!(marked.is_err(), "a block fails to load");
            assert!(
                !record(&tree, b).await.is_staged_delete(),
                "'p/b' is not marked"
            );
            assert!(
                !tree.state.has_unfinished_change(),
                "no climb began, so nothing is unfinished"
            );
            closure_holds(&tree).await;
        }
    })
    .await;
}

/// A delete of a file whose block fails to load stages nothing. One whose climb, marking each
/// node before its parent as the unserved mark does, reaches an ancestor in a block that fails to
/// load answers the error with the file and the levels below that ancestor marked (C12).
#[tokio::test]
async fn a_delete_whose_loads_fail_answers_the_error() {
    crate::in_context(async {
        for file_resident in [false, true] {
            let (tree, store) = deep_tree(&[]).await;
            let (q, r, f) = (tree.id("q"), tree.id("q/r"), tree.id("q/r/f"));
            if file_resident {
                only_resident(&tree, f, q).await;
            } else {
                assert!(!resident(&tree, f), "the file's block is not in memory");
            }

            store.fail(true);
            let deleted = tree.state.node_delete(tree.repository.clone(), f).await;
            store.fail(false);

            assert!(deleted.is_err(), "a block fails to load");
            assert_eq!(
                record(&tree, f).await.is_staged_delete(),
                file_resident,
                "the file is marked where its climb began"
            );
            if file_resident {
                assert!(record(&tree, r).await.is_staged(), "'q/r' is marked");
                assert!(!record(&tree, q).await.is_staged(), "'q' is not");
            }
            crate::holding(&tree.state, &tree.repository, Rules::default()).await;
        }
    })
    .await;
}

/// A delete's level mark whose climb reaches an ancestor in a block that fails to load: it
/// answers the error with the node unmarked and an unfinished change noted, the levels it marked
/// standing.
#[tokio::test]
async fn a_delete_level_whose_climb_fails_to_load_notes_an_unfinished_change() {
    crate::in_context(async {
        let (tree, store) = deep_tree(&[]).await;
        let (q, r, f) = (tree.id("q"), tree.id("q/r"), tree.id("q/r/f"));
        let _f = record(&tree, f).await;
        assert!(!resident(&tree, q), "the block of 'q' is not in memory");

        store.fail(true);
        let marked = tree.state.node_delete_level(&tree.repository, f, r).await;
        store.fail(false);

        assert!(marked.is_err(), "the climb fails to load 'q''s block");
        assert!(
            !record(&tree, f).await.is_staged_delete(),
            "'q/r/f' is not marked"
        );
        assert!(record(&tree, r).await.is_staged(), "'q/r' is marked");
        assert!(
            tree.state.has_unfinished_change(),
            "the delete notes an unfinished change"
        );
        crate::holding(&tree.state, &tree.repository, Rules::default()).await;
    })
    .await;
}

/// The index of the file metadata block holding `slot`'s.
fn metadata_index(slot: NodeID) -> usize {
    NodeFileMetadataBlock::index(node::node_to_file_metadata(slot))
}

/// Asserts, with the store failing, whether `slot`'s file metadata block is stored: a check
/// reads the store for a stored one, and answers none from memory for a tree that stores none.
async fn assert_metadata_stored(tree: &Tree, slot: NodeID, stored: bool) {
    let checked = tree
        .state
        .try_block_file_metadata_existing(tree.repository.clone(), metadata_index(slot))
        .await;
    if stored {
        assert!(checked.is_err(), "the stored block is read");
    } else {
        assert!(matches!(checked, Ok(None)), "no file metadata stored");
    }
}

/// Asserts `added` answered an error with `freed`'s slot freed again: the grab wrote the slot's
/// block, nothing is left in flight, and the next add takes the same slot.
async fn freed_again<T: std::fmt::Debug>(tree: &Tree, freed: NodeID, added: Result<T, StateError>) {
    assert!(added.is_err(), "the slot fails to load: {added:?}");
    let block = tree
        .state
        .resident_block(NodeBlock::index(freed))
        .expect("the slot's block is in memory");
    assert!(block.is_registered(), "the grab wrote the slot's block");
    let report = closure_holds(tree).await;
    assert_eq!(report.detached, 0, "nothing is left in flight");
    let again = tree
        .state
        .node_add(tree.repository.clone(), tree.id("d"), file("h"), "h")
        .await
        .expect("the next add");
    assert_eq!(again, freed, "the next add takes the freed slot again");
}

/// An add whose grabbed slot's file metadata, or, where the tree stores none, names fail to load
/// frees the slot.
#[tokio::test]
async fn an_add_whose_slot_fails_to_load_frees_its_slot() {
    crate::in_context(async {
        for metadata in [Some("d/f"), None] {
            let (tree, store, freed) = Tree::with_a_free_slot(metadata).await;

            store.fail(true);
            assert_metadata_stored(&tree, freed, metadata.is_some()).await;
            let added = tree
                .state
                .node_add(tree.repository.clone(), tree.id("d"), file("h"), "h")
                .await;
            store.fail(false);

            freed_again(&tree, freed, added).await;
        }
    })
    .await;
}

/// An attach, whose slot `prepare_child` takes, whose slot's file metadata, or, where the tree
/// stores none, names fail to load frees the slot.
#[tokio::test]
async fn an_attach_whose_slot_fails_to_load_frees_its_slot() {
    crate::in_context(async {
        for metadata in [Some("d/f"), None] {
            let (tree, store, freed) = Tree::with_a_free_slot(metadata).await;

            store.fail(true);
            assert_metadata_stored(&tree, freed, metadata.is_some()).await;
            let attached = tree
                .state
                .node_attach(tree.repository.clone(), tree.id("d"), file("h"), "h")
                .await;
            store.fail(false);

            freed_again(&tree, freed, attached).await;
        }
    })
    .await;
}

/// A tree whose unused chain holds two blocks of one free slot each, and stores no file metadata:
/// `d`'s, which `k`'s files fill to its last slot, and `e`'s, freed `e/f`'s slot free. A build into
/// `d`, holding `d`'s block, takes its root's slot there and its first child's in `e`'s block,
/// which the child's grab may have to load and whose names the root's attach did not read.
/// Loaded afresh through a failing store, with `d`'s block in memory.
async fn tree_with_free_slots_in_two_blocks() -> (Tree, Arc<FailingStore>) {
    let tree = Tree::spread(&["e/f", "e/g"], "fill", &["d/", "k/"]).await;
    tree.fill_block_but_one(tree.id("k"), "k").await;
    let f = tree.id("e/f");
    lore_revision::state::node_discard_fused(tree.state.clone(), tree.repository.clone(), f)
        .await
        .expect("'e/f' is discarded");
    let (tree, store) = tree.failing().await;
    let _d = record(&tree, tree.id("d")).await;
    (tree, store)
}

/// The blocks of `tree_with_free_slots_in_two_blocks`' two free slots, `e`'s first.
fn slot_blocks(tree: &Tree) -> [Option<Arc<NodeBlock>>; 2] {
    [tree.id("e/f"), tree.id("d")].map(|node| tree.state.resident_block(NodeBlock::index(node)))
}

/// A build on a served State taking slots from its stored blocks, whose child's slot fails to
/// load, its file metadata once the root's attach hold is evicted, or the names of a block the
/// root took no slot from: the slot is freed, and the build, abandoned, leaves nothing in flight.
#[tokio::test]
async fn a_build_whose_child_slot_fails_to_load_frees_it() {
    crate::in_context(async {
        for names in [false, true] {
            let (tree, store) = if names {
                let (tree, store) = tree_with_free_slots_in_two_blocks().await;
                // Both slots' blocks in memory, their names not, so only the names of the block the
                // root's attach did not take a slot from are left to fail.
                let _e = record(&tree, tree.id("e")).await;
                assert!(
                    slot_blocks(&tree).iter().all(|block| block
                        .as_ref()
                        .is_some_and(|block| !block.is_nametable_deserialized())),
                    "both slots' blocks are in memory, their names not"
                );
                store.fail(true);
                assert_metadata_stored(&tree, tree.id("e/f"), false).await;
                store.fail(false);
                (tree, store)
            } else {
                // The metadata of `d/g`'s slot, not the freed one, so the root's attach writes
                // nothing to the file metadata block, which stays clean, held only for reuse.
                let (tree, store, _) = Tree::with_a_free_slot(Some("d/g")).await;
                (tree, store)
            };
            tree.state.mark_served();
            let root = Node {
                name_hash: hash_string("n"),
                ..Node::default()
            };
            let Ok(TreeAttach::Building(mut build)) = tree
                .state
                .swap()
                .node_attach_tree(tree.repository.clone(), tree.id("d"), &root, "n")
                .await
            else {
                panic!("the build begins");
            };

            if !names {
                tree.state.evict_blocks_for_tests();
            }
            store.fail(true);
            let added = build.add(build.root(), &file("x"), "x").await;
            store.fail(false);
            assert!(added.is_err(), "the child's slot fails to load");
            if names {
                let blocks = slot_blocks(&tree).map(|block| block.expect("in memory"));
                assert!(
                    blocks.iter().all(|block| block.is_registered()),
                    "the root's grab wrote one block, the child's the other"
                );
                assert_eq!(
                    blocks
                        .iter()
                        .filter(|block| block.is_nametable_deserialized())
                        .count(),
                    1,
                    "only the root's block holds its names"
                );
            }
            build.abandon().await;

            let report = closure_holds(&tree).await;
            assert_eq!(report.detached, 0, "nothing is left in flight");
        }
    })
    .await;
}

/// What a scenario's eviction acts on.
trait Evicts: Send + Sync + 'static {
    fn tree(&self) -> &Tree;
    fn store(&self) -> &FailingStore;
}

/// An op that evicts the State's and the committed State's blocks and fails the store.
fn evict<C: Evicts>() -> Op<C> {
    Box::new(|context: Arc<C>| {
        Box::pin(async move {
            context.tree().state.evict_blocks_for_tests();
            context.tree().committed.evict_blocks_for_tests();
            context.store().fail(true);
        })
    })
}

/// A scenario's tree beside an eviction, and where its op failed, if it did.
struct Evicting {
    tree: Tree,
    store: Arc<FailingStore>,
    /// The stage the op failed at.
    failed: OnceLock<&'static str>,
}

impl Evicts for Evicting {
    fn tree(&self) -> &Tree {
        &self.tree
    }

    fn store(&self) -> &FailingStore {
        &self.store
    }
}

impl Modelled for Evicting {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }

    fn mend(&self) {
        self.store.fail(false);
    }

    fn faulted(&self) -> bool {
        self.store.failed_outside_calls() > 0
    }
}

type Build = fn() -> Pin<Box<dyn Future<Output = (Tree, Arc<FailingStore>)>>>;
/// An op: where it failed, by a stage its scenario names, or `None` where it did not.
type Run = fn(Arc<Evicting>) -> Pin<Box<dyn Future<Output = Option<&'static str>> + Send>>;
type Judge = fn(Arc<Evicting>) -> Pin<Box<dyn Future<Output = ()>>>;

/// Explores `run` on the tree `build` makes, beside an op that evicts the blocks and fails the
/// store, at every point of `run`; `judge` checks each run with the store reading again. Asserts
/// the exploration complete, and that some run failed at each of `stages`: the arms the scenario
/// is there for.
fn beside_an_eviction(
    name: &'static str,
    build: Build,
    run: Run,
    judge: Judge,
    stages: &'static [&'static str],
) {
    beside(name, build, run, judge, stages, evict());
}

/// [`beside_an_eviction`], beside `other` in place of the eviction: an op that evicts the
/// blocks and fails the store after a change of its own, or one that changes the tree alone.
fn beside(
    name: &'static str,
    build: Build,
    run: Run,
    judge: Judge,
    stages: &'static [&'static str],
    other: Op<Evicting>,
) {
    // A bit per stage of `stages` some run failed at.
    let reached = Arc::new(AtomicU64::new(0));
    let seen = reached.clone();
    let op: Op<Evicting> = Box::new(move |context: Arc<Evicting>| {
        Box::pin(async move {
            if let Some(stage) = run(context.clone()).await {
                context.failed.set(stage).expect("one run of the op");
            }
        })
    });
    let scenario = Scenario::<Evicting> {
        name,
        setup: Box::new(move || {
            Box::pin(async move {
                let (tree, store) = build().await;
                Evicting {
                    tree,
                    store,
                    failed: OnceLock::new(),
                }
            })
        }),
        ops: vec![op, other],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Evicting>| {
            let seen = seen.clone();
            Box::pin(async move {
                context.store.fail(false);
                if let Some(at) = context
                    .failed
                    .get()
                    .and_then(|failed| stages.iter().position(|stage| stage == failed))
                {
                    seen.fetch_or(1 << at, Ordering::Relaxed);
                }
                judge(context).await;
            })
        }),
    };
    crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            sleep_sets: false,
            ..Exploration::default()
        },
    );
    let reached = reached.load(Ordering::Relaxed);
    let missed: Vec<&str> = (0..stages.len())
        .filter(|at| reached & (1 << at) == 0)
        .map(|at| stages[at])
        .collect();
    assert!(missed.is_empty(), "'{name}': no run failed at {missed:?}");
}

/// An unserved clear beside an eviction: where the walk that checks the directory again after
/// its clear fails to load, the clear marks the directory again before it answers the error;
/// wherever a walk fails, the directory is dirty at the end.
#[test]
fn an_unserved_clear_whose_recheck_fails_to_load_marks_its_directory_again() {
    beside_an_eviction(
        "an unserved clear whose re-check fails to load",
        || {
            Box::pin(async {
                let (tree, store) = spread_tree().await;
                ops::mark_dirty(&tree, tree.id("p/a")).await;
                (tree, store)
            })
        },
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                let rechecked = tree.state.clears_rechecked();
                let cleared = tree
                    .state
                    .node_clear(&tree.repository, tree.id("p/a"), Clear::Dirty)
                    .await;
                cleared.err().map(|_| {
                    if tree.state.clears_rechecked() > rechecked {
                        "re-check"
                    } else {
                        "walk"
                    }
                })
            })
        },
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                if context.failed.get().is_some() {
                    assert!(
                        record(tree, tree.id("p")).await.is_dirty(),
                        "'p' is dirty after a failed walk"
                    );
                } else {
                    assert!(
                        !record(tree, tree.id("p/a")).await.is_dirty(),
                        "'p/a' is cleared"
                    );
                }
                closure_holds(tree).await;
            })
        },
        &["re-check"],
    );
}

/// A served clear beside an eviction, failing before or after its first phase: wherever it fails,
/// its directory stays dirty with no clear pending, and the served closure holds.
#[test]
fn a_served_clear_beside_an_eviction() {
    beside_an_eviction(
        "a served clear beside an eviction",
        || {
            Box::pin(async {
                let (mut tree, store) = spread_tree().await;
                tree.reload_committed().await;
                served_with_p_dirty(&tree, true).await;
                ops::mark_added(&tree, tree.id("p")).await;
                (tree, store)
            })
        },
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                let cleared = ops::served_clear(tree, tree.id("p")).await;
                // The first phase drops the action bits of `p`, dirty, so in memory.
                cleared.err().map(|_| {
                    if in_memory(tree, tree.id("p")).is_some_and(|p| !p.is_dirty_add()) {
                        "begun"
                    } else {
                        "before"
                    }
                })
            })
        },
        |context| {
            Box::pin(async move {
                if context.failed.get().is_some() {
                    kept_dirty_with_no_clear_pending(&context.tree).await;
                }
                closure_holds(&context.tree).await;
            })
        },
        &["begun"],
    );
}

/// A served clear's tree beside an eviction and a served create, and how the two ops ended.
struct ClearAndCreate {
    tree: Tree,
    store: Arc<FailingStore>,
    /// The clear answered an error.
    failed: AtomicBool,
    /// The create began with the clear pending.
    pending: AtomicBool,
    /// The create attached its file.
    created: AtomicBool,
}

impl Evicts for ClearAndCreate {
    fn tree(&self) -> &Tree {
        &self.tree
    }

    fn store(&self) -> &FailingStore {
        &self.store
    }
}

impl Modelled for ClearAndCreate {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }

    fn mend(&self) {
        self.store.fail(false);
    }

    fn faulted(&self) -> bool {
        self.store.failed_outside_calls() > 0
    }
}

/// A served clear beside an eviction and a served create in its directory, which marks the
/// directory again where it finds the clear pending, under at most four preemptions: wherever
/// the clear fails, its pending clear goes and the directory stays dirty, a file created kept;
/// the served closure holds, with nothing left in flight. Asserts some run had the create begin
/// with the clear pending and attach, and the clear fail.
#[test]
fn a_served_clear_beside_an_eviction_and_a_create() {
    const PREEMPTIONS: usize = 4;
    let raced = Arc::new(AtomicU64::new(0));
    let seen = raced.clone();
    let clear: Op<ClearAndCreate> = Box::new(|context: Arc<ClearAndCreate>| {
        Box::pin(async move {
            let failed = ops::served_clear(&context.tree, context.tree.id("p"))
                .await
                .is_err();
            context.failed.store(failed, Ordering::Relaxed);
        })
    });
    let create: Op<ClearAndCreate> = Box::new(|context: Arc<ClearAndCreate>| {
        Box::pin(async move {
            let tree = &context.tree;
            let p = tree.id("p");
            // `p` is dirty, so its block is in memory.
            let pending = tree
                .state
                .resident_block(NodeBlock::index(p))
                .is_some_and(|block| block.clear_pending(Node::index(p)));
            context.pending.store(pending, Ordering::Relaxed);
            let node = Node {
                flags: (NodeFlags::File | NodeFlags::DirtyAdd).bits(),
                ext_flags: NodeExtFlags::LocalContent.bits(),
                ..Node::default()
            };
            // Its walk of the directory's chain may meet the failing store too.
            let attached = tree
                .state
                .served()
                .node_attach(tree.repository.clone(), p, node, "c", AttachCheck::Walk)
                .await;
            let created = matches!(attached, Ok(Some(AttachOutcome::Attached(_))));
            context.created.store(created, Ordering::Relaxed);
        })
    });
    let scenario = Scenario::<ClearAndCreate> {
        name: "a served clear beside an eviction and a create",
        setup: Box::new(|| {
            Box::pin(async {
                let (mut tree, store) = spread_tree().await;
                tree.reload_committed().await;
                served_with_p_dirty(&tree, true).await;
                ClearAndCreate {
                    tree,
                    store,
                    failed: AtomicBool::new(false),
                    pending: AtomicBool::new(false),
                    created: AtomicBool::new(false),
                }
            })
        }),
        ops: vec![clear, evict(), create],
        cancellable: vec![],
        check: Box::new(move |context: Arc<ClearAndCreate>| {
            let seen = seen.clone();
            Box::pin(async move {
                context.store.fail(false);
                let tree = &context.tree;
                let [failed, pending, created] =
                    [&context.failed, &context.pending, &context.created]
                        .map(|flag| flag.load(Ordering::Relaxed));
                if failed {
                    kept_dirty_with_no_clear_pending(tree).await;
                }
                if created {
                    let found = tree
                        .state
                        .find_subnode(tree.repository.clone(), tree.id("p"), hash_string("c"))
                        .await;
                    assert!(found.is_ok(), "the created file stays in the directory");
                    assert!(
                        record(tree, tree.id("p")).await.is_dirty(),
                        "the directory stays dirty over its created file"
                    );
                }
                if failed && pending && created {
                    seen.fetch_add(1, Ordering::Relaxed);
                }
                let report = closure_holds(tree).await;
                assert_eq!(report.detached, 0, "nothing is left in flight");
            })
        }),
    };
    crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            sleep_sets: false,
            preemptions: PREEMPTIONS,
            ..Exploration::default()
        },
    );
    assert!(
        raced.load(Ordering::Relaxed) > 0,
        "some run had the create begin with the clear pending and attach, and the clear fail"
    );
}

/// A copy-up beside an eviction, failing before or after its mark of the directory: wherever it
/// fails, the file is unwritten; where it does not, the file holds its local content; the served
/// closure holds.
#[test]
fn a_copy_up_beside_an_eviction() {
    beside_an_eviction(
        "a copy-up beside an eviction",
        || {
            Box::pin(async {
                let (tree, store) = spread_tree().await;
                tree.state.mark_served();
                (tree, store)
            })
        },
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                let copied = tree
                    .state
                    .served()
                    .node_copy_up(
                        &tree.repository,
                        tree.id("p/b"),
                        tree.id("p"),
                        (0, 3),
                        (NodeFlags::DirtyModify, NodeExtFlags::LocalContent),
                    )
                    .await;
                // A directory the mark wrote is dirty, so in memory.
                copied.err().map(|_| {
                    if in_memory(tree, tree.id("p")).is_some_and(|p| p.is_dirty()) {
                        "after its mark"
                    } else {
                        "before its mark"
                    }
                })
            })
        },
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                let held = record(tree, tree.id("p/b")).await;
                if context.failed.get().is_some() {
                    assert!(
                        !held.is_dirty() && held.size != 3,
                        "a failed copy-up writes nothing to the file"
                    );
                } else {
                    assert!(
                        held.is_dirty() && held.size == 3,
                        "the copy-up wrote the file"
                    );
                }
                closure_holds(tree).await;
            })
        },
        &["before its mark", "after its mark"],
    );
}

/// A copy-up beside a served detach of its directory, the file still in it, then an eviction: the
/// copy-up answers `NodeNotFound`, or the store's error where a load after the detach failed, its
/// read of the file once its mark finds the directory gone among them, never `SlowDown`, with
/// nothing written; closure holds.
#[test]
fn a_copy_up_beside_a_detach_of_its_directory() {
    beside(
        "a copy-up beside a detach of its directory",
        || {
            Box::pin(async {
                let (tree, store) = spread_tree().await;
                tree.state.mark_served();
                (tree, store)
            })
        },
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                let copied = tree
                    .state
                    .served()
                    .node_copy_up(
                        &tree.repository,
                        tree.id("p/b"),
                        tree.id("p"),
                        (0, 3),
                        (NodeFlags::DirtyModify, NodeExtFlags::LocalContent),
                    )
                    .await;
                copied.err().map(|error| {
                    if error.is_node_not_found() {
                        "gone"
                    } else if matches!(error, StateError::SlowDown(_)) {
                        "slow down"
                    } else {
                        "store"
                    }
                })
            })
        },
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                let held = record(tree, tree.id("p/b")).await;
                match context.failed.get() {
                    Some(&"slow down") => panic!("a copy-up beside a detach answers no SlowDown"),
                    Some(_) => assert!(
                        !held.is_dirty() && held.size != 3,
                        "a failed copy-up writes nothing to the file"
                    ),
                    None => assert!(
                        held.is_dirty() && held.size == 3,
                        "the copy-up wrote the file"
                    ),
                }
                closure_holds(tree).await;
            })
        },
        &["store"],
        Box::new(|context: Arc<Evicting>| {
            Box::pin(async move {
                ops::served_detach(&context.tree, context.tree.id("p")).await;
                context.tree.state.evict_blocks_for_tests();
                context.store.fail(true);
            })
        }),
    );
}

/// A restore of `p/b`, staged for deletion, beside an eviction, failing at its loads or at its
/// climb to `p`, in another block: where it fails before its write, the file stays staged for
/// deletion; where its climb fails, the file is restored and `p` keeps the delete's marks (H8);
/// closure holds.
#[test]
fn a_restore_beside_an_eviction() {
    beside_an_eviction(
        "a restore beside an eviction",
        || {
            Box::pin(async {
                let tree = Tree::spread(&["p/a"], "fill", &["p/b"]).await;
                delete_level(&tree, tree.id("p/b"), tree.id("p")).await;
                tree.failing().await
            })
        },
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                let b = tree.id("p/b");
                let restored = tree
                    .state
                    .node_restore(&tree.repository, b, &content_of("p/b"))
                    .await;
                // A file the restore wrote is dirty, so in memory.
                restored.err().map(|_| match in_memory(tree, b) {
                    Some(record) if !record.is_staged_delete() => "climb",
                    _ => "below",
                })
            })
        },
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                let staged_delete = record(tree, tree.id("p/b")).await.is_staged_delete();
                assert_eq!(
                    staged_delete,
                    context.failed.get() == Some(&"below"),
                    "the file stays staged for deletion where the restore failed before its write"
                );
                let p = record(tree, tree.id("p")).await;
                assert!(
                    p.is_staged() && p.is_dirty(),
                    "'p' keeps the delete's marks"
                );
                closure_holds(tree).await;
            })
        },
        &["below", "climb"],
    );
}

/// A modify beside an eviction, failing at its loads or at its climb to `q`: wherever it fails,
/// the file is unwritten, and where its climb marked `q/r` an unfinished change is noted, closure
/// kept wherever none is; where it does not fail, the file is staged.
#[test]
fn a_modify_beside_an_eviction() {
    beside_an_eviction(
        "a modify beside an eviction",
        || Box::pin(deep_tree(&[])),
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                let modified = tree
                    .state
                    .node_modify_staged(&tree.repository, tree.id("q/r/f"), &content_of("q/r/f"))
                    .await;
                // A directory the climb marked is dirty, so in memory.
                modified.err().map(|_| {
                    if in_memory(tree, tree.id("q/r")).is_some_and(|r| r.is_staged()) {
                        "climb"
                    } else {
                        "load"
                    }
                })
            })
        },
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                let staged = record(tree, tree.id("q/r/f")).await.is_staged();
                let Some(failed) = context.failed.get() else {
                    assert!(staged, "the modify wrote the file");
                    assert!(
                        !tree.state.has_unfinished_change(),
                        "a modify that wrote leaves nothing unfinished"
                    );
                    closure_holds(tree).await;
                    return;
                };
                assert!(!staged, "a failed modify writes nothing to the file");
                let unfinished = tree.state.has_unfinished_change();
                assert_eq!(
                    unfinished,
                    *failed == "climb",
                    "a modify notes an unfinished change where it fails after its climb marked"
                );
                if unfinished {
                    crate::holding(&tree.state, &tree.repository, Rules::default()).await;
                } else {
                    closure_holds(tree).await;
                }
            })
        },
        &["load", "climb"],
    );
}

/// Where an attach or an add into `d` failed: at its slot, where the grab wrote the free slot's
/// block, or before it.
fn attach_failed(tree: &Tree) -> &'static str {
    let slot = NodeBlock::index(tree.id("d/f"));
    if tree
        .state
        .resident_block(slot)
        .is_some_and(|block| block.is_registered())
    {
        "slot"
    } else {
        "lookup"
    }
}

/// An unserved attach, as a VFS provider diff's checked attach takes a slot, and an add, each
/// beside an eviction: wherever its slot fails to load, the slot is freed, and nothing is left in
/// flight; where it does not fail, the file is attached.
#[test]
fn an_attach_and_an_add_beside_an_eviction() {
    let stored: Build = || {
        Box::pin(async {
            let (tree, store, _) = Tree::with_a_free_slot(Some("d/g")).await;
            (tree, store)
        })
    };
    let none_stored: Build = || {
        Box::pin(async {
            let (tree, store, _) = Tree::with_a_free_slot(None).await;
            (tree, store)
        })
    };
    let attach: Run = |context| {
        Box::pin(async move {
            let tree = &context.tree;
            let attached = tree
                .state
                .node_attach(tree.repository.clone(), tree.id("d"), file("h"), "h")
                .await;
            attached.err().map(|_| attach_failed(tree))
        })
    };
    let add: Run = |context| {
        Box::pin(async move {
            let tree = &context.tree;
            let added = tree
                .state
                .node_add(tree.repository.clone(), tree.id("d"), file("h"), "h")
                .await;
            added.err().map(|_| attach_failed(tree))
        })
    };
    let cases: [(&'static str, Build, Run); 4] = [
        (
            "an attach into a stored file metadata block, beside an eviction",
            stored,
            attach,
        ),
        (
            "an attach into a tree storing no file metadata, beside an eviction",
            none_stored,
            attach,
        ),
        (
            "an add into a stored file metadata block, beside an eviction",
            stored,
            add,
        ),
        (
            "an add into a tree storing no file metadata, beside an eviction",
            none_stored,
            add,
        ),
    ];
    for (name, build, run) in cases {
        beside_an_eviction(
            name,
            build,
            run,
            |context| {
                Box::pin(async move {
                    let tree = &context.tree;
                    let found = tree
                        .state
                        .find_subnode(tree.repository.clone(), tree.id("d"), hash_string("h"))
                        .await;
                    assert_eq!(
                        found.is_ok(),
                        context.failed.get().is_none(),
                        "the file is attached where the attach did not fail"
                    );
                    let report = closure_holds(tree).await;
                    assert_eq!(report.detached, 0, "nothing is left in flight");
                })
            },
            &["slot"],
        );
    }
}

/// The product's chains, a scan's and `lore dirty`'s, and the scan's chain through its op, to the
/// held `q/r` beside an eviction, failing at their find, the step each takes first: wherever one
/// fails, no second `q/r` is added, closure kept; where it does not, it answers `q/r`. The model
/// holds the op's answer to its reads.
#[test]
fn a_chain_to_a_held_directory_beside_an_eviction() {
    let scan: Run = |context| {
        Box::pin(async move {
            let tree = &context.tree;
            let chained = lore_revision::state::os_diff::ensure_scan_dir_chain(
                tree.repository.clone(),
                tree.state.clone(),
                tree.id("q"),
                "r",
            )
            .await;
            match chained {
                Ok(found) => {
                    assert_eq!(found, tree.id("q/r"), "the chain answers 'q/r'");
                    None
                }
                Err(_) => Some("find"),
            }
        })
    };
    let dirty: Run = |context| {
        Box::pin(async move {
            let tree = &context.tree;
            let chained = lore_revision::file::dirty::ensure_dirty_parent_dirs(
                &tree.repository,
                &tree.state,
                tree.id("q"),
                "r",
            )
            .await;
            match chained {
                Ok(found) => {
                    assert_eq!(
                        found,
                        lore_revision::file::dirty::ParentChain::Reached(tree.id("q/r")),
                        "the chain answers 'q/r'"
                    );
                    None
                }
                Err(_) => Some("find"),
            }
        })
    };
    let op: Run = |context| {
        Box::pin(async move {
            let tree = &context.tree;
            let chained = ops::scan_chain(tree, tree.id("q"), "r").await;
            match chained {
                Some(found) => {
                    assert_eq!(found, tree.id("q/r"), "the chain answers 'q/r'");
                    None
                }
                None => Some("find"),
            }
        })
    };
    let cases = [
        (
            "a scan's chain to a held directory beside an eviction",
            scan,
        ),
        (
            "lore dirty's chain to a held directory beside an eviction",
            dirty,
        ),
        ("a scan chain op to a held directory beside an eviction", op),
    ];
    for (name, run) in cases {
        beside_an_eviction(
            name,
            || Box::pin(chain_tree()),
            run,
            |context| Box::pin(async move { closed_and_unique(&context.tree).await }),
            &["find"],
        );
    }
}

/// A scan's chain from `q` to a new `q/r/n` through its op beside an eviction, failing before its
/// add, at a find or the add's slot or parent, or at its mark's climb from `q/r` to `q`: the model
/// holds its answer to its reads; names stay unique, closure kept but where the mark's climb
/// stopped part way (C12); where it does not fail, `q/r/n` is added. A failed find of `r` is the
/// held directory's case: here `q`'s block goes with the eviction, so an add into `q` would fail
/// too.
#[test]
fn a_scan_chain_to_a_new_directory_beside_an_eviction() {
    beside_an_eviction(
        "a scan chain to a new directory beside an eviction",
        || Box::pin(deep_tree(&[])),
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                let chained = ops::scan_chain(tree, tree.id("q"), "r/n").await;
                // `q/r`'s block holds the add where there was one, dirty, so in memory.
                let added = tree
                    .state
                    .find_subnode_resident(tree.id("q/r"), hash_string("n"));
                chained.is_none().then_some(match added {
                    Ok(Some(_)) => "mark",
                    _ => "before",
                })
            })
        },
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                let failed = context.failed.get().copied();
                let rules = Rules {
                    dirty_closure: failed != Some("mark"),
                    unique_names: true,
                    ..Rules::default()
                };
                crate::holding(&tree.state, &tree.repository, rules).await;
                if failed.is_none() {
                    let found = tree
                        .state
                        .find_subnode(tree.repository.clone(), tree.id("q/r"), hash_string("n"))
                        .await;
                    assert!(found.is_ok(), "the chain adds 'q/r/n'");
                }
            })
        },
        &["before", "mark"],
    );
}

/// An unserved attach beside an unserved detach of its directory: where the detach lands after
/// the attach took its slot, the attach's publish finds the directory changed, its next round
/// finds it detached, and it frees the slot; only the directory is left detached. Asserts some run
/// failed so.
#[test]
fn an_attach_whose_directory_is_detached_after_its_slot_frees_it() {
    beside(
        "an attach beside a detach of its directory",
        || {
            Box::pin(async {
                let (tree, store, _) = Tree::with_a_free_slot(None).await;
                (tree, store)
            })
        },
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                let retries = tree.state.attach_retries();
                let attached = tree
                    .state
                    .node_attach(tree.repository.clone(), tree.id("d"), file("h"), "h")
                    .await;
                attached.err().map(|error| {
                    if error.is_node_not_found() && tree.state.attach_retries() > retries {
                        "after its slot"
                    } else {
                        "before its slot"
                    }
                })
            })
        },
        |context| {
            Box::pin(async move {
                let report = closure_holds(&context.tree).await;
                assert_eq!(report.detached, 1, "only the directory is left detached");
            })
        },
        &["after its slot"],
        Box::new(|context: Arc<Evicting>| {
            Box::pin(async move {
                assert!(ops::unserved_detach(&context.tree, context.tree.id("d")).await);
            })
        }),
    );
}

/// A build of `d/n` holding `d/n/x` and `d/n/y`, published, as the swap's copy builds one, and
/// abandoned on an error: where it failed, or `None` where it attached.
async fn build_two(tree: &Tree) -> Option<&'static str> {
    let root = Node {
        name_hash: hash_string("n"),
        ..Node::default()
    };
    let mut build = match tree
        .state
        .swap()
        .node_attach_tree(tree.repository.clone(), tree.id("d"), &root, "n")
        .await
    {
        Ok(TreeAttach::Building(build)) => build,
        Ok(TreeAttach::Exists(_)) => panic!("'d/n' is not there"),
        Err(_) => return Some("begin"),
    };
    let into = build.root();
    for name in ["x", "y"] {
        if build.add(into, &file(name), name).await.is_err() {
            build.abandon().await;
            return Some("add");
        }
    }
    match build.publish().await {
        Ok(AttachOutcome::Attached(_)) => None,
        _ => Some("publish"),
    }
}

/// Judges a build beside an eviction: the build attached where it did not fail, and nothing is
/// left in flight.
fn judge_build(context: Arc<Evicting>) -> Pin<Box<dyn Future<Output = ()>>> {
    Box::pin(async move {
        let tree = &context.tree;
        let found = tree
            .state
            .find_subnode(tree.repository.clone(), tree.id("d"), hash_string("n"))
            .await;
        assert_eq!(
            found.is_ok(),
            context.failed.get().is_none(),
            "the build is attached where it did not fail"
        );
        let report = closure_holds(tree).await;
        assert_eq!(report.detached, 0, "nothing is left in flight");
    })
}

/// A build on a served State that takes slots from its stored blocks, beside an eviction: a
/// child's slot fails to load its file metadata, or the names of a block the root took no slot
/// from, and over two blocks the child's grab fails to load that block; wherever a load fails,
/// nothing is left in flight, and the served closure holds.
#[test]
fn a_build_beside_an_eviction() {
    beside_an_eviction(
        "a build beside an eviction",
        || {
            Box::pin(async {
                let (tree, store, _) = Tree::with_a_free_slot(Some("d/g")).await;
                tree.state.mark_served();
                (tree, store)
            })
        },
        |context| Box::pin(async move { build_two(&context.tree).await }),
        judge_build,
        &["add"],
    );
    beside_an_eviction(
        "a build over two blocks beside an eviction",
        || {
            Box::pin(async {
                let (tree, store) = tree_with_free_slots_in_two_blocks().await;
                tree.state.mark_served();
                (tree, store)
            })
        },
        |context| {
            Box::pin(async move {
                let tree = &context.tree;
                // Each grab wrote its block, so in memory.
                build_two(tree).await.map(|stage| {
                    let both = slot_blocks(tree)
                        .iter()
                        .all(|block| block.as_ref().is_some_and(|block| block.is_registered()));
                    if stage == "add" && both {
                        "child slot"
                    } else {
                        stage
                    }
                })
            })
        },
        judge_build,
        &["add", "child slot"],
    );
}
