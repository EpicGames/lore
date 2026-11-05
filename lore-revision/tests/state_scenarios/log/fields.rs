// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The fields a model reads from the structural and decision records: the block instances they
//! name, each publish's check and what it compared against, a conditional detach's expectation,
//! and the refusal reasons one operation reaches.

use std::sync::Arc;
use std::sync::Mutex;

use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::AttachOutcome;
use lore_revision::state::Detach;
use lore_revision::state::IfUnchanged;
use lore_revision::state::LevelMark;
use lore_revision::state::LinkPin;
use lore_revision::state::Mark;
use lore_revision::state::NameChange;
use lore_revision::state::TreeAttach;
use lore_revision::state::node::hooks::log;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Expected;
use lore_revision::state::node::hooks::log::How;
use lore_revision::state::node::hooks::log::Kind;
use lore_revision::state::node::hooks::log::Read;
use lore_revision::state::node::hooks::log::Reason;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_storage::hash::hash_string;

use super::logged;
use super::of;
use super::one;
use crate::harness::catalogue::ops::file;
use crate::harness::fixtures::Tree;

/// A directory node named `name`.
fn directory(name: &str) -> Node {
    Node {
        name_hash: hash_string(name),
        ..Node::default()
    }
}

/// A tree with `a/x`, `a/y` and `a/z` in block 1 and `b` in block 2, blocks 0 and 1 full, saved
/// and loaded again, so each block's install is logged when first read.
async fn across_blocks() -> Tree {
    let mut tree = Tree::spread(&[], "fill", &["a/x", "a/y", "a/z"]).await;
    let repository = tree.repository.clone();
    let filler = tree
        .state
        .node_add(repository.clone(), ROOT_NODE, directory("fill2"), "fill2")
        .await
        .expect("the second filler adds");
    for index in 0.. {
        let name = format!("k{index}");
        let id = tree
            .state
            .node_add(repository.clone(), filler, file(&name), &name)
            .await
            .expect("a fill file adds");
        if Node::index(id) == BLOCK_NODE_COUNT - 1 {
            break;
        }
    }
    let b = tree
        .state
        .node_add(repository, ROOT_NODE, directory("b"), "b")
        .await
        .expect("'b' adds");
    tree.nodes.push(("b".to_string(), b));
    tree.reloaded().await
}

/// The instance the log's install of `block` names, its one install in `records`.
fn installed(records: &[&Record], block: usize) -> u64 {
    let installs: Vec<u64> = records
        .iter()
        .filter_map(|record| match record.effect {
            Effect::Loaded {
                block: loaded,
                instance,
                ..
            } if loaded == block => Some(instance),
            _ => None,
        })
        .collect();
    assert_eq!(installs.len(), 1, "one install of block {block}");
    installs[0]
}

/// Each structural record names the instance of the block it read its parent's count in: an
/// add's publish, a rename, a move (both parents', and the new parent's count its walk read), a
/// detach and a discard.
#[test]
fn the_structural_records_name_their_parents_block_instances() {
    let counted: Arc<Mutex<Option<u32>>> = Arc::default();
    let into = counted.clone();
    let (records, tree) = logged(
        || Box::pin(across_blocks()),
        Box::new(move |tree: Arc<Tree>| {
            let into = into.clone();
            Box::pin(async move {
                let repository = &tree.repository;
                let (a, b) = (tree.id("a"), tree.id("b"));
                let b_block = tree
                    .state
                    .block(repository.clone(), NodeBlock::index(b))
                    .await
                    .expect("'b''s block");
                let n = tree
                    .state
                    .node_add(repository.clone(), a, file("n"), "n")
                    .await
                    .expect("'a/n' adds");
                assert_eq!(NodeBlock::index(n), 2, "'a/n' lies in block 2");
                tree.state
                    .node_rename(repository.clone(), tree.id("a/x"), NameChange::Rename("w"))
                    .await
                    .expect("'a/x' renames");
                // A count moved from where the block's load started it.
                tree.state
                    .node_add(repository.clone(), b, file("q"), "q")
                    .await
                    .expect("'b/q' adds");
                *into.lock().unwrap() = Some(b_block.node_with_chain_changes(Node::index(b)).1);
                tree.state
                    .node_reattach(repository.clone(), tree.id("a/y"), b, "y")
                    .await
                    .expect("'a/y' moves");
                tree.state
                    .node_detach(repository.clone(), tree.id("a/z"), Detach::Always)
                    .await
                    .expect("'a/z' detaches");
                lore_revision::state::node_discard_fused(tree.state.clone(), repository.clone(), n)
                    .await
                    .expect("'a/n' is discarded");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (one_, two) = (installed(&records, 1), installed(&records, 2));
    assert_ne!(one_, two);
    let (a, b) = (tree.id("a"), tree.id("b"));
    one(
        &records,
        "publish",
        |effect| matches!(effect, Effect::Publish { parent, instance, .. } if *parent == a && *instance == one_),
    );
    one(
        &records,
        "rename",
        |effect| matches!(effect, Effect::Rename { parent, instance, .. } if *parent == a && *instance == one_),
    );
    let before = counted.lock().unwrap().expect("the operation ran");
    assert_ne!(before, 0, "a count its load did not start");
    one(&records, "move", |effect| {
        matches!(effect, Effect::Move {
            from,
            to,
            instances,
            check: log::Check::Walk,
            expected: Expected::Count { changes, instance },
            ..
        } if *from == a && *to == b && *instances == (one_, two) && *changes == before
            && *instance == two)
    });
    let z = tree.id("a/z");
    one(&records, "detach", |effect| {
        matches!(effect, Effect::Detach { node, how: How::Always, instance, .. }
            if *node == z && *instance == one_)
    });
    one(&records, "discard", |effect| {
        matches!(effect, Effect::Detach { parent, how: How::Discard, instance, .. }
            if *parent == a && *instance == one_)
    });
}

/// A staged add publishes `Unstaged`; a build's root publishes `Unchanged` against the count its
/// lookup read, with its block's instance.
#[test]
fn the_publishes_log_their_checks() {
    let counted: Arc<Mutex<Option<u32>>> = Arc::default();
    let into = counted.clone();
    let (records, tree) = logged(
        || Box::pin(across_blocks()),
        Box::new(move |tree: Arc<Tree>| {
            let into = into.clone();
            Box::pin(async move {
                let (repository, b) = (&tree.repository, tree.id("b"));
                tree.state
                    .node_add_staged(repository.clone(), tree.id("a"), file("t"), "t")
                    .await
                    .expect("'a/t' adds");
                let b_block = tree
                    .state
                    .block(repository.clone(), NodeBlock::index(b))
                    .await
                    .expect("'b''s block");
                // A count moved from where the block's load started it.
                tree.state
                    .node_add(repository.clone(), b, file("q"), "q")
                    .await
                    .expect("'b/q' adds");
                *into.lock().unwrap() = Some(b_block.node_with_chain_changes(Node::index(b)).1);
                let build = match tree
                    .state
                    .swap()
                    .node_attach_tree(repository.clone(), b, &directory("c"), "c")
                    .await
                    .expect("the build begins")
                {
                    TreeAttach::Building(build) => build,
                    TreeAttach::Exists(_) => panic!("'b/c' is free"),
                };
                let published = build.publish().await.expect("the publish runs");
                assert!(matches!(published, AttachOutcome::Attached(_)));
            })
        }),
    );
    let before = counted.lock().unwrap().expect("the operation ran");
    assert_ne!(before, 0, "a count its load did not start");
    let records = of(&records, &tree.state);
    let (one_, two) = (installed(&records, 1), installed(&records, 2));
    let (a, b) = (tree.id("a"), tree.id("b"));
    one(&records, "staged publish", |effect| {
        matches!(effect, Effect::Publish { parent, check: log::Check::Unstaged, expected: Expected::Nothing, instance, .. }
            if *parent == a && *instance == one_)
    });
    one(&records, "build publish", |effect| {
        matches!(effect, Effect::Publish {
            parent,
            check: log::Check::Unchanged,
            expected: Expected::Count { changes, instance: counted },
            instance,
            ..
        } if *parent == b && *changes == before && *counted == two && *instance == two)
    });
}

/// RMDIR's detach expects the count it read; a child added after the read refuses its pre-test,
/// which names the count it expected.
#[test]
fn an_rmdir_logs_the_count_it_expects() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                // 'd/e' in block 1, read from the store in the operation.
                let tree = Tree::spread(&["d/"], "fill", &["d/e/"])
                    .await
                    .reloaded()
                    .await;
                tree.state.mark_served();
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let e = tree.id("d/e");
                let block = tree
                    .state
                    .block(tree.repository.clone(), NodeBlock::index(e))
                    .await
                    .expect("the directory's block");
                let (read, _) = IfUnchanged::read(&block, e);
                let x = crate::harness::catalogue::ops::served_create(&tree, e, "x")
                    .await
                    .expect("'d/e/x' is created");
                let detached = tree
                    .state
                    .served()
                    .node_detach(tree.repository.clone(), e, Detach::IfUnchanged(&read))
                    .await
                    .expect("the detach runs");
                assert!(!detached, "the detach expecting the count read is refused");
                assert!(crate::harness::catalogue::ops::served_detach(&tree, x).await);
                let ended = crate::harness::catalogue::ops::served_rmdir(&tree, e).await;
                assert!(ended.removed && ended.refused == 0, "{ended:?}");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let e = tree.id("d/e");
    let refused = one(
        &records,
        "refused pre-test",
        |effect| matches!(effect, Effect::PreTest { node, refused: true, .. } if *node == e),
    );
    let unlink = one(
        &records,
        "unlink",
        |effect| matches!(effect, Effect::Detach { node, how: How::IfUnchanged, .. } if *node == e),
    );
    let (
        Effect::PreTest {
            expected:
                Expected::Count {
                    changes: before,
                    instance: tested,
                },
            ..
        },
        Effect::Detach {
            expected:
                Expected::Count {
                    changes: after,
                    instance: detached,
                },
            ..
        },
    ) = (&refused.effect, &unlink.effect)
    else {
        panic!("both expect a count: {refused:?} {unlink:?}");
    };
    assert_eq!(
        after.wrapping_sub(*before),
        2,
        "the add and the detach of 'x' moved the count by one each"
    );
    assert_eq!(
        (*tested, *detached),
        (installed(&records, 1), installed(&records, 1)),
        "'d/e''s block's instance"
    );
}

/// A trusted move that renames its node names the new name and its hash, and a publish names its
/// node's kind.
#[test]
fn a_renaming_move_and_each_kinds_publish_log_their_names_and_kinds() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["a/f", "b/"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let repository = &tree.repository;
                let own = Mark::staged_and_dirty(NodeFlags::StagedMove, NodeFlags::DirtyMove);
                tree.state
                    .node_move(
                        repository.clone(),
                        tree.id("a/f"),
                        tree.id("b"),
                        NameChange::Rename("g"),
                        own,
                    )
                    .await
                    .expect("'a/f' moves");
                // The destination's climb, as `lore dirty` makes it after the move.
                tree.state
                    .node_mark_destination(repository, tree.id("b"), own)
                    .await
                    .expect("'b' is marked");
                tree.state
                    .node_add(repository.clone(), tree.id("b"), directory("e"), "e")
                    .await
                    .expect("'b/e' adds");
                let link = Node {
                    flags: NodeFlags::Link.bits(),
                    name_hash: hash_string("l"),
                    ..Node::default()
                };
                tree.state
                    .node_add(repository.clone(), tree.id("b"), link, "l")
                    .await
                    .expect("'b/l' adds");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (a, b, f) = (tree.id("a"), tree.id("b"), tree.id("a/f"));
    one(&records, "move", |effect| {
        matches!(effect, Effect::Move { node, from, to, name_hash, name, check: log::Check::Unchecked, .. }
            if *node == f && *from == a && *to == b && *name_hash == hash_string("g")
                && name.as_ref().is_some_and(|name| name == "g"))
    });
    for (name, kind) in [("e", Kind::Directory), ("l", Kind::Link)] {
        one(&records, "publish", |effect| {
            matches!(effect, Effect::Publish { parent, name: named, name_hash, kind: of, .. }
                if *parent == b && named == name && *name_hash == hash_string(name) && *of == kind)
        });
    }
}

/// An add after a discard takes the slot the discard freed, from the head of its block's free
/// list, in the same instance.
#[test]
fn a_grab_after_a_free_logs_the_freed_slot() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                lore_revision::state::node_discard_fused(
                    tree.state.clone(),
                    tree.repository.clone(),
                    tree.id("d/f"),
                )
                .await
                .expect("'d/f' is discarded");
                tree.state
                    .node_add(tree.repository.clone(), tree.id("d"), file("g"), "g")
                    .await
                    .expect("'d/g' adds");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let f = tree.id("d/f");
    let freed = one(
        &records,
        "free",
        |effect| matches!(effect, Effect::Free { node, .. } if *node == f),
    );
    let Effect::Free { instance, .. } = freed.effect else {
        unreachable!()
    };
    let grab = one(
        &records,
        "grab",
        |effect| matches!(effect, Effect::Grab { node, instance: of } if *node == f && *of == instance),
    );
    assert!(freed.seq < grab.seq);
}

/// A build's add into one of its files is refused, the file's word read.
#[test]
fn a_build_add_into_a_file_logs_its_refusal() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let mut build = match tree
                    .state
                    .swap()
                    .node_attach_tree(tree.repository.clone(), tree.id("d"), &directory("c"), "c")
                    .await
                    .expect("the build begins")
                {
                    TreeAttach::Building(build) => build,
                    TreeAttach::Exists(_) => panic!("'d/c' is free"),
                };
                let x = build
                    .add(build.root(), &file("x"), "x")
                    .await
                    .expect("'x' adds to the copy");
                let refused = build.add(x, &file("y"), "y").await;
                assert!(refused.is_err(), "an add into a file is refused");
                build.abandon().await;
            })
        }),
    );
    let records = of(&records, &tree.state);
    let added = one(
        &records,
        "add",
        |effect| matches!(effect, Effect::BuildAdd { name, .. } if name == "x"),
    );
    let Effect::BuildAdd { node: x, .. } = added.effect else {
        unreachable!()
    };
    let refused = one(&records, "refusal", |effect| {
        matches!(effect, Effect::Refused { parent, reason: Reason::NotDirectory, read: Read::Word(words), .. }
            if *parent == x && words.0.is_file())
    });
    assert_eq!(refused.section, Section::BuildAdd);
}

/// A pin on a link detached meanwhile is refused as not found.
#[test]
fn a_pin_on_a_detached_link_logs_not_found() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let mut tree = Tree::new(&["d/"]).await;
                let link = Node {
                    flags: NodeFlags::Link.bits(),
                    name_hash: hash_string("l"),
                    ..Node::default()
                };
                let id = tree
                    .state
                    .node_add(tree.repository.clone(), tree.id("d"), link, "l")
                    .await
                    .expect("the link adds");
                tree.nodes.push(("d/l".to_string(), id));
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let l = tree.id("d/l");
                tree.state
                    .node_detach(tree.repository.clone(), l, Detach::Always)
                    .await
                    .expect("the link detaches");
                let pinned = tree
                    .state
                    .node_set_link_target(
                        &tree.repository,
                        l,
                        LinkPin::Revision(lore_base::types::Hash::from_u64(7)),
                    )
                    .await;
                assert!(
                    pinned.is_err_and(|error| error.is_node_not_found()),
                    "the repin of a detached link is refused"
                );
            })
        }),
    );
    let records = of(&records, &tree.state);
    let l = tree.id("d/l");
    let refused = one(&records, "refusal", |effect| {
        matches!(effect, Effect::Refused { node, reason: Reason::NotFound, read: Read::Word(words), .. }
            if *node == l && words.0.is_discarded())
    });
    assert_eq!(refused.section, Section::Repin);
}

/// A staged move out of a directory staged for deletion, which its checks do not test, is refused
/// under its locks, the directory's word read; the permit's take and release wrap it.
#[test]
fn a_staged_move_out_of_a_deleted_directory_logs_unmovable() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["a/f", "b/"]).await;
                let marked = tree
                    .state
                    .node_delete_level(&tree.repository, tree.id("a"), ROOT_NODE)
                    .await
                    .expect("the level marks");
                assert!(matches!(marked, LevelMark::Marked(_)));
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let permit = tree.state.moves_permit().await.expect("the permit");
                let moved = tree
                    .state
                    .node_move_staged(
                        tree.repository.clone(),
                        tree.id("a/f"),
                        tree.id("b"),
                        ("f", hash_string("f")),
                        &permit,
                        false,
                    )
                    .await;
                assert!(
                    moved.is_err(),
                    "the move out of a deleted directory is refused"
                );
                drop(permit);
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (a, f): (NodeID, NodeID) = (tree.id("a"), tree.id("a/f"));
    let refused = one(&records, "refusal", |effect| {
        matches!(effect, Effect::Refused { node, parent, reason: Reason::Unmovable, read: Read::Word(words), .. }
            if *node == f && *parent == a && words.0.is_staged_delete())
    });
    assert_eq!(refused.section, Section::MoveNamed);
    let permit = one(&records, "take", |effect| *effect == Effect::Permit);
    let unpermit = one(&records, "release", |effect| *effect == Effect::Unpermit);
    assert!(permit.seq < refused.seq && refused.seq < unpermit.seq);
}
