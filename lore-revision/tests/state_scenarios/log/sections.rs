// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Sections of several locks take one `seq` for all their writers, and every run of a race keeps
//! the log's rules: each section's records numbered in order, by one task, labelled.

use std::sync::Arc;

use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::AttachOutcome;
use lore_revision::state::Content;
use lore_revision::state::Detach;
use lore_revision::state::TreeAttach;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_storage::hash::hash_string;

use super::logged;
use super::logged_runs;
use super::of;
use super::one;
use super::section_of;
use super::sections_labelled;
use super::word_records;
use super::words_of;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Refusal;
use crate::harness::catalogue::call::call;
use crate::harness::catalogue::modelled::Labels;
use crate::harness::catalogue::ops::file;
use crate::harness::fixtures::Tree;
use crate::harness::scheduler::Exploration;

/// A publish into the root, in block 0, of a node in block 1: its two writers share the
/// publish's one section, which holds the publish and the node's word.
#[test]
fn a_publish_across_two_blocks_takes_one_section() {
    let (records, tree) = logged(
        || Box::pin(Tree::spread(&[], "fill", &[])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                tree.state
                    .node_add(tree.repository.clone(), ROOT_NODE, file("n"), "n")
                    .await
                    .expect("'n' adds");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let publish = one(&records, "publish", |effect| {
        matches!(
            effect,
            Effect::Publish {
                parent: ROOT_NODE,
                ..
            }
        )
    });
    let Effect::Publish { node: n, .. } = publish.effect else {
        unreachable!()
    };
    assert_eq!(NodeBlock::index(n), 1, "'n' lies in block 1");
    assert_eq!(
        sections_labelled(&records, Section::Publish),
        [publish.seq],
        "one section for both writers"
    );
    let section = section_of(&records, publish);
    assert!(
        word_records(&section, n).len() == 1,
        "the node's word in the publish's section: {section:#?}"
    );
}

/// A served rewrite of a file in block 1 below a directory in block 0: the pair lock's section
/// holds the file's word.
#[test]
fn a_served_rewrite_across_two_blocks_takes_one_section() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::spread(&["d/"], "fill", &["d/f"]).await;
                tree.state.mark_served();
                tree.state
                    .served()
                    .node_mark(&tree.repository, tree.id("d"))
                    .await
                    .expect("'d' is marked");
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let content = Content {
                    mode: 0o644,
                    size: 3,
                    ..Content::default()
                };
                let refusing = tree
                    .state
                    .served()
                    .node_rewrite(
                        &tree.repository,
                        tree.id("d/f"),
                        tree.id("d"),
                        &content,
                        (NodeFlags::DirtyModify, NodeExtFlags::NoFlags),
                    )
                    .await
                    .expect("the rewrite runs");
                assert_eq!(refusing, None, "'d' admits the rewrite");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (d, f) = (tree.id("d"), tree.id("d/f"));
    assert_ne!(NodeBlock::index(d), NodeBlock::index(f));
    let rewrites = sections_labelled(&records, Section::Rewrite);
    assert_eq!(rewrites.len(), 1, "one section: {records:#?}");
    let words = word_records(&records, f);
    assert_eq!(words.len(), 1);
    assert_eq!(words[0].seq, rewrites[0]);
}

/// A detach and a reattach of one file raced through every interleaving: each run's log keeps the
/// rules, and some run takes a race arm of one of them.
#[test]
fn a_detach_racing_a_reattach_logs_its_race_arms() {
    let runs = logged_runs(
        || Box::pin(Tree::new(&["d/f", "e/"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    tree.state
                        .node_detach(tree.repository.clone(), tree.id("d/f"), Detach::Always)
                        .await
                        .expect("the detach runs");
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_reattach(tree.repository.clone(), tree.id("d/f"), tree.id("e"), "g")
                        .await;
                })
            }),
        ],
        vec![],
        Labels::Required,
        // Every schedule fits the default bound, so its one pass is whole.
        Exploration {
            deepening: false,
            sleep_sets: false,
            ..Exploration::default()
        },
    );
    assert!(runs.len() > 1, "the race interleaves");
    // A one-child chain has no link for the other to move under the walk: no `Moving` arm.
    assert_eq!(
        arms_taken(&runs),
        [
            "ReattachChanged",
            "ReattachMissing",
            "UnservedDetachChanged",
            "UnservedDetachMissing",
        ],
        "the race arms over {} runs",
        runs.len()
    );
    for (log, tree) in &runs {
        let records = of(log, &tree.state);
        let f = tree.id("d/f");
        let unlinked = records
            .iter()
            .filter(|record| matches!(record.effect, Effect::Detach { node, .. } if node == f))
            .count();
        assert_eq!(unlinked, 1, "the file is detached once in every run");
    }
}

/// The arms the runs took, each named once, in name order: the set, whatever order the
/// exploration found them in.
fn arms_taken<C>(runs: &[(Vec<Record>, Arc<C>)]) -> Vec<String> {
    let mut taken: Vec<String> = runs
        .iter()
        .flat_map(|(log, _)| log)
        .filter_map(|record| match record.effect {
            Effect::Arm(arm) => Some(format!("{arm:?}")),
            _ => None,
        })
        .collect();
    taken.sort_unstable();
    taken.dedup();
    taken
}

/// A served conditional detach and a served reattach of one file raced: the served detach's arms,
/// and a pre-test after the first attempt. The detach runs through a call, the served detach's
/// kind with the test's own condition, so the model judges its pre-test per attempt. `e/h` gives
/// the reattach's name lookup a step, where the detach lands between its read of the file and
/// its walk to it (`ReattachMissing`).
#[test]
fn a_served_detach_racing_a_reattach_logs_its_race_arms() {
    let runs = logged_runs(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f", "e/h"]).await;
                tree.state.mark_served();
                tree
            })
        },
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    call(
                        &tree.state,
                        (&catalogue::SERVED_DETACH, Some(tree.id("d/f"))),
                        async {
                            let detached = tree
                                .state
                                .served()
                                .node_detach(
                                    tree.repository.clone(),
                                    tree.id("d/f"),
                                    Detach::IfClean,
                                )
                                .await;
                            let outcome = match detached {
                                Ok(true) => Outcome::Done(Answer::None),
                                Ok(false) => Outcome::Refused(Refusal::IfClean),
                                Err(error) if error.is_node_not_found() => {
                                    Outcome::Refused(Refusal::NotFound)
                                }
                                Err(error) => Outcome::Failed(error.to_string()),
                            };
                            ((), outcome)
                        },
                    )
                    .await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = crate::harness::catalogue::ops::served_rename(
                        &tree,
                        tree.id("d/f"),
                        tree.id("e"),
                        "g",
                    )
                    .await;
                })
            }),
        ],
        vec![],
        Labels::Required,
        // Every schedule fits the default bound, so its one pass is whole.
        Exploration {
            deepening: false,
            sleep_sets: false,
            ..Exploration::default()
        },
    );
    assert_eq!(
        arms_taken(&runs),
        ["DetachChanged", "ReattachChanged", "ReattachMissing"],
        "the race arms over {} runs",
        runs.len()
    );
    assert!(
        runs.iter().any(|(log, _)| log
            .iter()
            .any(|record| matches!(record.effect, Effect::PreTest { attempt: 1.., .. }))),
        "some run tests again after its first attempt"
    );
}

/// Two directories moved to one parent at once: in every run, the moves permit's takes and
/// releases alternate in the log's order, each move inside its own.
#[test]
fn two_directory_reattaches_log_their_permits_in_turn() {
    let runs = logged_runs(
        || Box::pin(Tree::new(&["a/x/", "a/y/", "b/"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    tree.state
                        .node_reattach(tree.repository.clone(), tree.id("a/x"), tree.id("b"), "x")
                        .await
                        .expect("'a/x' moves");
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    tree.state
                        .node_reattach(tree.repository.clone(), tree.id("a/y"), tree.id("b"), "y")
                        .await
                        .expect("'a/y' moves");
                })
            }),
        ],
        vec![],
        Labels::Required,
        Exploration::default(),
    );
    assert!(runs.len() > 1, "the moves interleave");
    for (log, tree) in &runs {
        let records = of(log, &tree.state);
        let turns: Vec<&str> = records
            .iter()
            .filter_map(|record| match record.effect {
                Effect::Permit => Some("take"),
                Effect::Unpermit => Some("release"),
                Effect::Move { .. } => Some("move"),
                _ => None,
            })
            .collect();
        assert_eq!(
            turns,
            ["take", "move", "release", "take", "move", "release"],
            "each move inside a permit of its own"
        );
    }
}

/// A build's add of a node in the block after its directory's: the section of its two locks holds
/// the add and the node's word, numbered once both are taken.
#[test]
fn a_build_add_across_two_blocks_takes_one_section() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/"]).await;
                // Block 0 full but for its last slot, which the build's root takes.
                tree.fill_block_but_one(ROOT_NODE, "fill").await;
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let root = Node {
                    name_hash: hash_string("c"),
                    ..Node::default()
                };
                let mut build = match tree
                    .state
                    .swap()
                    .node_attach_tree(tree.repository.clone(), tree.id("d"), &root, "c")
                    .await
                    .expect("the build begins")
                {
                    TreeAttach::Building(build) => build,
                    TreeAttach::Exists(_) => panic!("'d/c' is free"),
                };
                build
                    .add(build.root(), &file("x"), "x")
                    .await
                    .expect("'x' adds to the copy");
                let published = build.publish().await.expect("the publish runs");
                assert!(matches!(published, AttachOutcome::Attached(_)));
            })
        }),
    );
    let records = of(&records, &tree.state);
    let added = one(
        &records,
        "add",
        |effect| matches!(effect, Effect::BuildAdd { name, .. } if name == "x"),
    );
    let Effect::BuildAdd {
        node: x,
        parent: root,
        ..
    } = added.effect
    else {
        unreachable!()
    };
    assert_eq!(
        (NodeBlock::index(root), NodeBlock::index(x)),
        (0, 1),
        "the root in block 0, 'x' in block 1"
    );
    let section = section_of(&records, added);
    assert_eq!(section.len(), 2, "the add and the word: {section:#?}");
    let word = word_records(&section, x);
    assert_eq!(word.len(), 1);
    assert_eq!(word[0].sub, added.sub + 1, "the word after the add");
    assert!(
        words_of(&section, x)
            .last()
            .is_some_and(|node| !node.is_discarded()),
        "linked, its Discarded cleared"
    );
}
