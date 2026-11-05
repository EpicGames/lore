// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Races the model judges in every interleaving the deterministic scheduler explores: two served
//! creates of one name, whose loser releases its slot on a served State; a block reset racing a
//! grab of the block; a staged add racing the pending marks below its root; two restores of one
//! file; staged changes climbing first beside each other. Each run replays through the model with
//! its calls and ends in its checkpoint (`log::check_model`).

use std::sync::Arc;

use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::state::Content;
use lore_revision::state::node::hooks::log::Arm;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Reason;
use lore_revision::state::node::hooks::log::Section;
use lore_storage::hash::hash_string;

use crate::harness::catalogue::modelled::Labels;
use crate::harness::catalogue::ops;
use crate::harness::catalogue::ops::file;
use crate::harness::fixtures::Tree;
use crate::harness::scheduler::Exploration;
use crate::log::Body;
use crate::log::logged_runs;
use crate::log::of;

/// Two served creates of one name: one publishes, the other finds the name held and releases the
/// slot it took, a free on a served State of a slot never published.
#[test]
fn two_served_creates_of_one_name_publish_once() {
    let runs = logged_runs(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                tree.state.mark_served();
                assert!(ops::served_mark_dir(&tree, tree.id("d")).await);
                tree
            })
        },
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    ops::served_create(&tree, tree.id("d"), "n").await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    ops::served_create(&tree, tree.id("d"), "n").await;
                })
            }),
        ],
        vec![],
        Labels::Required,
        Exploration::default(),
    );
    let mut released = false;
    for (records, tree) in &runs {
        let records = of(records, &tree.state);
        let published = records
            .iter()
            .filter(|record| matches!(record.effect, Effect::Publish { .. }))
            .count();
        assert_eq!(published, 1, "one create publishes the name");
        released |= records.iter().any(|record| {
            record.section == Section::Release && matches!(record.effect, Effect::Free { .. })
        });
    }
    assert!(released, "in some run the loser released the slot it took");
}

/// Two restores of one file staged for deletion, each to content of its own, so a second that
/// went through would write: one takes it back, and the other, whichever runs second, finds it
/// restored under its lock and is refused.
#[test]
fn two_restores_of_one_deleted_file_restore_once() {
    let restoring = |size: u64| -> Body<Tree> {
        Box::new(move |tree: Arc<Tree>| {
            Box::pin(async move {
                let content = Content {
                    size,
                    ..Content::default()
                };
                ops::restore(&tree, tree.id("d/f"), &content).await;
            })
        })
    };
    let runs = logged_runs(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                ops::staged_delete_level(&tree, tree.id("d/f"), tree.id("d")).await;
                tree
            })
        },
        vec![restoring(1), restoring(2)],
        vec![],
        Labels::Required,
        Exploration::default(),
    );
    for (records, tree) in &runs {
        let f = tree.id("d/f");
        let refused = of(records, &tree.state)
            .iter()
            .filter(|record| {
                matches!(record.effect, Effect::Refused { node, reason: Reason::Restore, .. }
                    if node == f)
            })
            .count();
        assert_eq!(refused, 1, "one restore refused under its lock");
    }
}

/// A discard freeing the last used slots of a block, beside a create grabbing a slot of it: the
/// grab lands before the block resets, or after, in which case it takes the block's first slot.
#[test]
fn a_block_reset_races_a_grab() {
    let runs = logged_runs(
        || Box::pin(Tree::spread(&["a/x"], "fill", &["p/f"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    lore_revision::state::node_discard_fused(
                        tree.state.clone(),
                        tree.repository.clone(),
                        tree.id("p"),
                    )
                    .await
                    .expect("the discard runs");
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = ops::checked_create(&tree, tree.id("a"), "n").await;
                })
            }),
        ],
        vec![],
        Labels::Required,
        Exploration::default(),
    );
    let mut grabbed: Vec<usize> = Vec::new();
    for (records, tree) in &runs {
        let block = NodeBlock::index(tree.id("p"));
        for record in of(records, &tree.state) {
            if let Effect::Grab { node, .. } = record.effect
                && NodeBlock::index(node) == block
            {
                grabbed.push(Node::index(node));
            }
        }
    }
    grabbed.sort_unstable();
    grabbed.dedup();
    assert!(
        grabbed.contains(&0) && grabbed.len() > 1,
        "the grab landed before the reset in some run and after it in another: {grabbed:?}"
    );
}

/// A C API delete of `s/t` beside a C API add into `s/t/u`: the add publishes before the delete
/// detaches `t`; or between the detach and the marks' reaching `u`; or after them, refused as `u`
/// is `Discarded`. Where it published, the marks reach what it published (pending.rs), and no
/// publish lands in `u` once it is marked.
#[test]
fn a_staged_add_races_the_pending_marks_below_its_root() {
    let runs = logged_runs(
        || {
            Box::pin(async {
                let mut tree = Tree::new(&["s/"]).await;
                let directory = |name: &str| Node {
                    name_hash: hash_string(name),
                    ..Node::default()
                };
                let repository = tree.repository.clone();
                let t = tree
                    .state
                    .node_add_staged(repository.clone(), tree.id("s"), directory("t"), "t")
                    .await
                    .expect("'s/t' adds");
                let u = tree
                    .state
                    .node_add_staged(repository, t, directory("u"), "u")
                    .await
                    .expect("'s/t/u' adds");
                tree.nodes
                    .extend([("s/t".to_string(), t), ("s/t/u".to_string(), u)]);
                tree
            })
        },
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let place = (tree.id("s/t"), tree.id("s"), hash_string("t"));
                    assert!(ops::pend(&tree, place, async {}).await, "'s/t' is detached");
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    // Refused where it meets the pending delete's root.
                    let _ = ops::staged_add(&tree, tree.id("s/t/u"), file("n"), "n").await;
                })
            }),
        ],
        vec![],
        Labels::Required,
        Exploration::default(),
    );
    let (mut before, mut between, mut refused) = (false, false, false);
    for (records, tree) in &runs {
        let (t, u) = (tree.id("s/t"), tree.id("s/t/u"));
        let records = of(records, &tree.state);
        let at = |found: &dyn Fn(&Effect) -> bool| {
            records.iter().position(|record| found(&record.effect))
        };
        let unlinked = at(&|effect| matches!(effect, Effect::Detach { node, .. } if *node == t))
            .expect("'s/t' is detached");
        let marked = at(&|effect| matches!(effect, Effect::Discarding { node } if *node == u))
            .expect("'s/t/u' is marked");
        let Some(published) =
            at(&|effect| matches!(effect, Effect::Publish { parent, .. } if *parent == u))
        else {
            refused = true;
            continue;
        };
        assert!(published < marked, "no publish into 'u' once it is marked");
        let Effect::Publish { node: added, .. } = records[published].effect else {
            unreachable!("a publish");
        };
        assert!(
            records[marked..]
                .iter()
                .any(|record| record.effect == Effect::Discarding { node: added }),
            "the marks reach what the add published"
        );
        if published < unlinked {
            before = true;
        } else {
            between = true;
        }
    }
    assert!(
        before && between && refused,
        "the add published before the detach in some run ({before}), between it and the marks \
         in another ({between}), and was refused after them in another ({refused})"
    );
}

/// Climb first (H8) on a clean directory below another: a delete level and a staged add into its
/// parent each climb it, and its parent, before their change, whichever goes first.
#[test]
fn a_delete_level_and_an_add_into_its_directory_each_climb_first() {
    let runs = logged_runs(
        || Box::pin(Tree::new(&["o/p/f"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    ops::staged_delete_level(&tree, tree.id("o/p/f"), tree.id("o/p")).await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    ops::staged_add(&tree, tree.id("o/p"), file("n"), "n").await;
                })
            }),
        ],
        vec![],
        Labels::Required,
        Exploration::default(),
    );
    let mut climbed = [false; 2];
    for (records, tree) in &runs {
        let p = tree.id("o/p");
        let first = of(records, &tree.state).into_iter().find(|record| {
            record.section == Section::UnservedMark
                && matches!(record.effect, Effect::Word { node, .. } if node == p)
        });
        if let Some(seen) = first
            .and_then(|record| record.task)
            .and_then(|task| climbed.get_mut(task))
        {
            *seen = true;
        }
    }
    assert_eq!(climbed, [true, true], "each climbed 'p' first in some run");
}

/// Climb first (H8) where a staged move takes a modify's file out of its directory between the
/// modify's climb and its change: the modify climbs again from where its file went.
#[test]
fn a_modify_and_a_move_of_its_file_each_climb_first() {
    let runs = logged_runs(
        || Box::pin(Tree::new(&["p/f", "q/"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let content = Content {
                        size: 7,
                        ..Content::default()
                    };
                    ops::staged_modify(&tree, tree.id("p/f"), &content).await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    ops::staged_move(&tree, tree.id("p/f"), tree.id("q"), "f").await;
                })
            }),
        ],
        vec![],
        Labels::Required,
        Exploration::default(),
    );
    assert!(
        runs.iter().any(|(records, tree)| of(records, &tree.state)
            .iter()
            .any(|record| record.effect == Effect::Arm(Arm::ModifyMovedClimb))),
        "a modify climbed again from where its file went in some run"
    );
}
