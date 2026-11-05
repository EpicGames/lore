// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The decisions sections make under their locks, each with what it read and what it compared
//! against: a detach's conditions and its parent gone, a publish and a move checked against an
//! absence or a log, a served rewrite's refusals, the in-place and owned changes' and a checked
//! move's. Those only a race reaches run as two operations through every interleaving.

use std::sync::Arc;
use std::sync::Mutex;

use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::Absent;
use lore_revision::state::AttachOutcome;
use lore_revision::state::Content;
use lore_revision::state::Detach;
use lore_revision::state::Ext;
use lore_revision::state::IfUnchanged;
use lore_revision::state::LevelMark;
use lore_revision::state::Mark;
use lore_revision::state::NameChange;
use lore_revision::state::OwnFlags;
use lore_revision::state::TreeAttach;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::log;
use lore_revision::state::node::hooks::log::Arm;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Expected;
use lore_revision::state::node::hooks::log::Read;
use lore_revision::state::node::hooks::log::Reason;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_storage::hash::hash_string;

use super::InWindow;
use super::logged;
use super::logged_runs;
use super::of;
use super::one;
use crate::current_thread;
use crate::harness::catalogue::modelled::Labels;
use crate::harness::catalogue::ops::file;
use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::harness::log::Collector;
use crate::harness::scheduler::Exploration;
use crate::in_context;

/// The node the one publish of `name` published.
fn published(records: &[&Record], name: &str) -> NodeID {
    let publish = one(
        records,
        "publish",
        |effect| matches!(effect, Effect::Publish { name: named, .. } if named == name),
    );
    let Effect::Publish { node, .. } = publish.effect else {
        unreachable!()
    };
    node
}

/// Two operations through every interleaving, each run once: one pass at the default bound,
/// which every schedule of these races fits, so the pass is whole, with sleep sets off, as they
/// are sound only for an exploration known to be whole.
fn raced(setup: super::Setup<Tree>, ops: Vec<super::Body<Tree>>) -> Vec<(Vec<Record>, Arc<Tree>)> {
    logged_runs(
        setup,
        ops,
        vec![],
        Labels::Required,
        Exploration {
            deepening: false,
            sleep_sets: false,
            ..Exploration::default()
        },
    )
}

/// Whether some run logged a refusal `matches` picks, in `section`.
fn refused_in_some_run(
    runs: &[(Vec<Record>, Arc<Tree>)],
    section: Section,
    matches: impl Fn(&Tree, &Effect) -> bool,
) -> bool {
    runs.iter().any(|(log, tree)| {
        log.iter().any(|record| {
            record.state == tree.state.hooks().id()
                && record.section == section
                && matches(tree, &record.effect)
        })
    })
}

#[test]
fn a_detach_below_a_detached_parent_logs_not_found() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let repository = &tree.repository;
                tree.state
                    .node_detach(repository.clone(), tree.id("d"), Detach::Always)
                    .await
                    .expect("'d' detaches");
                let _ = tree
                    .state
                    .swap()
                    .node_detach(repository.clone(), tree.id("d/f"), Detach::Always)
                    .await;
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (d, f) = (tree.id("d"), tree.id("d/f"));
    let refused = one(&records, "refusal", |effect| {
        matches!(effect, Effect::Refused { node, parent, reason: Reason::NotFound, read: Read::Word(words), .. }
            if *node == f && *parent == d && words.0.is_discarded())
    });
    assert_eq!(refused.section, Section::Detach);
}

/// A conditional detach whose condition fails only under its locks, a race having changed the
/// node or its chain since the copy it tested: each condition's reason, read and expectation.
#[test]
fn a_detach_refused_under_its_locks_logs_what_its_condition_read() {
    let marked = raced(
        || Box::pin(Tree::new(&["d/f"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_detach(tree.repository.clone(), tree.id("d/f"), Detach::IfClean)
                        .await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_mark(
                            &tree.repository,
                            tree.id("d/f"),
                            Mark::dirty(NodeFlags::DirtyModify, Ext::Keep, false),
                        )
                        .await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&marked, Section::Detach, |tree, effect| {
            let f = tree.id("d/f");
            matches!(effect, Effect::Refused { node, parent, reason: Reason::IfClean, read: Read::Word(words), .. }
                if *node == f && *parent == f && words.0.is_dirty())
        }),
        "no run refused a file marked dirty since its test"
    );
    let filled = raced(
        || Box::pin(Tree::new(&["d/e/"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_detach(tree.repository.clone(), tree.id("d/e"), Detach::IfEmpty)
                        .await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_add(tree.repository.clone(), tree.id("d/e"), file("x"), "x")
                        .await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&filled, Section::Detach, |tree, effect| {
            let e = tree.id("d/e");
            matches!(effect, Effect::Refused { node, parent, reason: Reason::IfEmpty, read: Read::Head(head), expected: Expected::Head(expected), .. }
                if *node == e && *parent == e && head.is_valid_node_id() && head == expected)
        }),
        "no run refused a directory filled since its test"
    );
    let changed = raced(
        || Box::pin(Tree::new(&["d/e/"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let e = tree.id("d/e");
                    let block = tree
                        .state
                        .block(tree.repository.clone(), NodeBlock::index(e))
                        .await
                        .expect("the directory's block");
                    let (read, _) = IfUnchanged::read(&block, e);
                    let _ = tree
                        .state
                        .node_detach(tree.repository.clone(), e, Detach::IfUnchanged(&read))
                        .await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_add(tree.repository.clone(), tree.id("d/e"), file("x"), "x")
                        .await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&changed, Section::Detach, |tree, effect| {
            let e = tree.id("d/e");
            matches!(effect, Effect::Refused {
                node,
                parent,
                reason: Reason::IfUnchanged,
                read: Read::Count { changes, instance },
                expected: Expected::Count { changes: counted, instance: of },
            } if *node == e && *parent == e && changes != counted && instance == of)
        }),
        "no run refused a directory whose chain changed since its read"
    );
    let renamed = raced(
        || Box::pin(Tree::new(&["d/f"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let at = Detach::At(tree.id("d"), hash_string("f"));
                    let _ = tree
                        .state
                        .node_detach(tree.repository.clone(), tree.id("d/f"), at)
                        .await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_rename(
                            tree.repository.clone(),
                            tree.id("d/f"),
                            NameChange::Rename("g"),
                        )
                        .await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&renamed, Section::Detach, |tree, effect| {
            let (d, f) = (tree.id("d"), tree.id("d/f"));
            matches!(effect, Effect::Refused {
                node,
                parent,
                reason: Reason::At,
                read: Read::Word(words),
                expected: Expected::At { parent: at, name_hash },
            } if *node == f && *parent == f && *at == d && *name_hash == hash_string("f")
                && words.0.name_hash == hash_string("g"))
        }),
        "no run refused a file renamed since its test"
    );
}

/// A conditional detach whose node a race renamed and detached since its test finds it not found
/// under its locks, though its condition refuses the detached record.
#[test]
fn a_detach_of_a_node_detached_since_its_test_logs_not_found() {
    let gone = raced(
        || Box::pin(Tree::new(&["d/f"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let at = Detach::At(tree.id("d"), hash_string("f"));
                    let _ = tree
                        .state
                        .node_detach(tree.repository.clone(), tree.id("d/f"), at)
                        .await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let (repository, f) = (&tree.repository, tree.id("d/f"));
                    let _ = tree
                        .state
                        .node_rename(repository.clone(), f, NameChange::Rename("g"))
                        .await;
                    let _ = tree
                        .state
                        .node_detach(repository.clone(), f, Detach::Always)
                        .await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&gone, Section::Detach, |tree, effect| {
            let f = tree.id("d/f");
            matches!(effect, Effect::Refused { node, parent, reason: Reason::NotFound, read: Read::Word(words), .. }
                if *node == f && *parent == f && words.0.is_discarded())
        }),
        "no run found a file renamed and detached since its test not found"
    );
    assert!(
        !refused_in_some_run(&gone, Section::Detach, |_, effect| {
            matches!(effect, Effect::Refused { reason: Reason::At, read: Read::Word(words), .. }
                if words.0.is_discarded())
        }),
        "a run refused a detached file on its condition"
    );
}

/// An add's publish refused under its locks: a staged add below a directory staged for deletion
/// meanwhile, and below one detached meanwhile.
#[test]
fn a_publish_refused_under_its_locks_logs_its_parents_word() {
    let deleted = raced(
        || Box::pin(Tree::new(&["s/"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_add_staged(tree.repository.clone(), tree.id("s"), file("t"), "t")
                        .await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_delete_level(&tree.repository, tree.id("s"), ROOT_NODE)
                        .await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&deleted, Section::Publish, |tree, effect| {
            let s = tree.id("s");
            matches!(effect, Effect::Refused { parent, reason: Reason::StagedDelete, read: Read::Word(words), .. }
                if *parent == s && words.0.is_staged_delete())
        }),
        "no run refused a staged add below a deleted directory"
    );
    let detached = raced(
        || Box::pin(Tree::new(&["s/"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_add_staged(tree.repository.clone(), tree.id("s"), file("t"), "t")
                        .await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_detach(tree.repository.clone(), tree.id("s"), Detach::Always)
                        .await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&detached, Section::Publish, |tree, effect| {
            let s = tree.id("s");
            matches!(effect, Effect::Refused { parent, reason: Reason::NotFound, read: Read::Word(words), .. }
                if *parent == s && words.0.is_discarded())
        }),
        "no run refused a staged add below a detached directory"
    );
    let live = raced(
        || Box::pin(Tree::new(&["s/"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_add(tree.repository.clone(), tree.id("s"), file("t"), "t")
                        .await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_detach(tree.repository.clone(), tree.id("s"), Detach::Always)
                        .await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&live, Section::Publish, |tree, effect| {
            let s = tree.id("s");
            matches!(effect, Effect::Refused { parent, reason: Reason::NotFound, read: Read::Word(words), .. }
                if *parent == s && words.0.is_discarded())
        }),
        "no run refused an add below a detached directory"
    );
}

/// The log positions two absences were read at.
type Positions = Arc<Mutex<Option<(u32, u32)>>>;

/// An attach checked against an absence: one a later add made stale finds the holder, and its slot
/// is released; a fresh one publishes, naming the position it checked from.
#[test]
fn an_attach_checked_against_an_absence_logs_its_check_or_the_holder() {
    let positions: Positions = Arc::default();
    let into = positions.clone();
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/"])),
        Box::new(move |tree: Arc<Tree>| {
            let into = into.clone();
            Box::pin(async move {
                let (repository, d) = (&tree.repository, tree.id("d"));
                let block = tree
                    .state
                    .block(repository.clone(), NodeBlock::index(d))
                    .await
                    .expect("the directory's block");
                let (stale_at, _) = block.watch_chain(Node::index(d), None);
                let stale = Absent::new(
                    &tree.state,
                    stale_at.seq,
                    block.clone(),
                    d,
                    hash_string("x"),
                );
                let _ = tree
                    .state
                    .node_add(repository.clone(), d, file("x"), "x")
                    .await
                    .expect("'d/x' adds");
                let attached = tree
                    .state
                    .node_attach_absent(repository.clone(), d, file("x"), "x", Some(&stale))
                    .await
                    .expect("the attach runs");
                assert!(matches!(attached, AttachOutcome::Exists(_)), "{attached:?}");
                let (fresh_at, _) = block.watch_chain(Node::index(d), None);
                let fresh = Absent::new(
                    &tree.state,
                    fresh_at.seq,
                    block.clone(),
                    d,
                    hash_string("y"),
                );
                let attached = tree
                    .state
                    .node_attach_absent(repository.clone(), d, file("y"), "y", Some(&fresh))
                    .await
                    .expect("the attach runs");
                assert!(matches!(attached, AttachOutcome::Attached(_)));
                *into.lock().unwrap() = Some((stale_at.seq, fresh_at.seq));
            })
        }),
    );
    let (stale, fresh) = positions.lock().unwrap().expect("the operation ran");
    let records = of(&records, &tree.state);
    let d = tree.id("d");
    let x = published(&records, "x");
    let refused = one(&records, "refusal", |effect| {
        matches!(effect, Effect::Refused { parent, reason: Reason::Exists, read: Read::Holder { holder, name_hash }, expected: Expected::Log { seq }, .. }
            if *parent == d && *holder == x && *seq == stale && *name_hash == hash_string("x"))
    });
    assert_eq!(refused.section, Section::Publish);
    let Effect::Refused { node: slot, .. } = refused.effect else {
        unreachable!()
    };
    let released = one(
        &records,
        "release",
        |effect| matches!(effect, Effect::Free { node, .. } if *node == slot),
    );
    assert_eq!(released.section, Section::Release);
    assert!(refused.seq < released.seq);
    one(&records, "publish", |effect| {
        matches!(effect, Effect::Publish { parent, name, check: log::Check::Absent, expected: Expected::Log { seq }, .. }
            if *parent == d && name == "y" && *seq == fresh)
    });
}

/// A build begun on a directory a listing watches publishes against the log from its lookup.
#[test]
fn a_build_on_a_watched_directory_logs_its_logged_check() {
    let positions: Positions = Arc::default();
    let into = positions.clone();
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/"])),
        Box::new(move |tree: Arc<Tree>| {
            let into = into.clone();
            Box::pin(async move {
                let d = tree.id("d");
                let block = tree
                    .state
                    .block(tree.repository.clone(), NodeBlock::index(d))
                    .await
                    .expect("the directory's block");
                let (watched, _) = block.watch_chain(Node::index(d), None);
                *into.lock().unwrap() = Some((watched.seq, watched.seq));
                let root = Node {
                    name_hash: hash_string("c"),
                    ..Node::default()
                };
                let build = match tree
                    .state
                    .swap()
                    .node_attach_tree(tree.repository.clone(), d, &root, "c")
                    .await
                    .expect("the build begins")
                {
                    TreeAttach::Building(build) => build,
                    TreeAttach::Exists(_) => panic!("'d/c' is free"),
                };
                let published = build.publish().await.expect("the publish runs");
                assert!(matches!(published, AttachOutcome::Attached(_)));
            })
        }),
    );
    let (watched, _) = positions.lock().unwrap().expect("the operation ran");
    let records = of(&records, &tree.state);
    let d = tree.id("d");
    one(&records, "publish", |effect| {
        matches!(effect, Effect::Publish { parent, name, check: log::Check::Logged, expected: Expected::Log { seq }, .. }
            if *parent == d && name == "c" && *seq == watched)
    });
}

/// A reattach checked against an absence: a fresh one moves, naming the position; one a later
/// add made stale finds the holder.
#[test]
fn a_reattach_checked_against_an_absence_logs_its_check_or_the_holder() {
    let positions: Positions = Arc::default();
    let into = positions.clone();
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/", "e/f", "e/h"])),
        Box::new(move |tree: Arc<Tree>| {
            let into = into.clone();
            Box::pin(async move {
                let (repository, d) = (&tree.repository, tree.id("d"));
                let block = tree
                    .state
                    .block(repository.clone(), NodeBlock::index(d))
                    .await
                    .expect("the directory's block");
                let (fresh_at, _) = block.watch_chain(Node::index(d), None);
                let fresh = Absent::new(
                    &tree.state,
                    fresh_at.seq,
                    block.clone(),
                    d,
                    hash_string("g"),
                );
                let moved = tree
                    .state
                    .node_reattach_absent(repository.clone(), tree.id("e/f"), d, "g", Some(&fresh))
                    .await
                    .expect("the reattach runs");
                assert!(matches!(moved, AttachOutcome::Attached(_)));
                // Again onto its own name: the log since the absence holds its own attach alone.
                let moved = tree
                    .state
                    .node_reattach_absent(repository.clone(), tree.id("e/f"), d, "g", Some(&fresh))
                    .await
                    .expect("the reattach runs");
                assert!(matches!(moved, AttachOutcome::Attached(_)));
                let (stale_at, _) = block.watch_chain(Node::index(d), None);
                let stale = Absent::new(
                    &tree.state,
                    stale_at.seq,
                    block.clone(),
                    d,
                    hash_string("k"),
                );
                tree.state
                    .node_add(repository.clone(), d, file("k"), "k")
                    .await
                    .expect("'d/k' adds");
                let moved = tree
                    .state
                    .node_reattach_absent(repository.clone(), tree.id("e/h"), d, "k", Some(&stale))
                    .await
                    .expect("the reattach runs");
                assert!(matches!(moved, AttachOutcome::Exists(_)), "{moved:?}");
                *into.lock().unwrap() = Some((fresh_at.seq, stale_at.seq));
            })
        }),
    );
    let (fresh, stale) = positions.lock().unwrap().expect("the operation ran");
    let records = of(&records, &tree.state);
    let (d, f, h) = (tree.id("d"), tree.id("e/f"), tree.id("e/h"));
    one(&records, "move", |effect| {
        matches!(effect, Effect::Move { node, to, check: log::Check::Absent, expected: Expected::Log { seq }, .. }
            if *node == f && *to == d && *seq == fresh)
    });
    one(&records, "own-name move", |effect| {
        matches!(effect, Effect::Move { node, from, to, name, check: log::Check::Logged, expected: Expected::Log { seq }, .. }
            if *node == f && *from == d && *to == d && *seq == fresh
                && name.as_ref().is_some_and(|name| name == "g"))
    });
    let k = published(&records, "k");
    let refused = one(&records, "refusal", |effect| {
        matches!(effect, Effect::Refused { node, parent, reason: Reason::Exists, read: Read::Holder { holder, name_hash }, expected: Expected::Log { seq } }
            if *node == h && *parent == d && *holder == k && *seq == stale && *name_hash == hash_string("k"))
    });
    assert_eq!(refused.section, Section::Reattach);
}

/// A served rewrite of a detached file, and of a file below a detached directory.
#[test]
fn a_served_rewrite_of_a_detached_file_or_below_one_logs_its_refusal() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f", "e/h"]).await;
                for path in ["d/f", "e"] {
                    tree.state
                        .node_detach(tree.repository.clone(), tree.id(path), Detach::Always)
                        .await
                        .expect("the node detaches");
                }
                tree.state.mark_served();
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                for (node, dir) in [("d/f", "d"), ("e/h", "e")] {
                    let _ = tree
                        .state
                        .served()
                        .node_rewrite(
                            &tree.repository,
                            tree.id(node),
                            tree.id(dir),
                            &Content::default(),
                            (NodeFlags::DirtyModify, NodeExtFlags::NoFlags),
                        )
                        .await;
                }
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (e, f, h) = (tree.id("e"), tree.id("d/f"), tree.id("e/h"));
    let detached = one(&records, "detached file", |effect| {
        matches!(effect, Effect::Refused { node, parent, reason: Reason::ServedRewrite, read: Read::Word(words), .. }
            if *node == f && *parent == f && words.0.is_discarded())
    });
    let below = one(&records, "below a detached directory", |effect| {
        matches!(effect, Effect::Refused { node, parent, reason: Reason::ServedRewrite, read: Read::Word(words), .. }
            if *node == h && *parent == e && words.0.is_discarded())
    });
    assert_eq!(detached.section, Section::Rewrite);
    assert_eq!(below.section, Section::Rewrite);
}

/// An in-place change its closure declines, and an owned change and clear of a detached node.
#[test]
fn a_declined_change_and_an_owned_clear_of_a_detached_node_log_their_refusals() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f", "d/g"]).await;
                // Dirty, so its owned clear would write.
                tree.state
                    .node_mark(
                        &tree.repository,
                        tree.id("d/g"),
                        Mark::dirty(NodeFlags::DirtyModify, Ext::Keep, false),
                    )
                    .await
                    .expect("'d/g' is marked");
                tree.state
                    .node_detach(tree.repository.clone(), tree.id("d/g"), Detach::Always)
                    .await
                    .expect("'d/g' detaches");
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let (d, f, g) = (tree.id("d"), tree.id("d/f"), tree.id("d/g"));
                let block = invariants::block_of(&tree.state, &tree.repository, f).await;
                let changed =
                    tree.state
                        .node_change_in_place(&block, f, (d, hash_string("f")), |_| false);
                assert!(!changed, "the declined change writes nothing");
                let owned = tree.state.node_change_owned(&block, g, |held| {
                    held.set_size(9);
                    true
                });
                assert!(!owned, "the owned change of a detached node is refused");
                let cleared = tree.state.node_clear_own(&block, g, OwnFlags::Dirty);
                assert!(!cleared, "the owned clear of a detached node is refused");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (d, f, g) = (tree.id("d"), tree.id("d/f"), tree.id("d/g"));
    let declined = one(&records, "declined", |effect| {
        matches!(effect, Effect::Refused { node, reason: Reason::InPlace, read: Read::Word(words), expected: Expected::At { parent, .. }, .. }
            if *node == f && *parent == d && !words.0.is_discarded())
    });
    assert_eq!(declined.section, Section::InPlace);
    let owned: Vec<_> = records
        .iter()
        .filter(|record| {
            matches!(record.effect, Effect::Refused { node, reason: Reason::OwnedGone, read: Read::Word(words), .. }
                if node == g && words.0.is_discarded())
        })
        .collect();
    assert_eq!(owned.len(), 2, "the owned change and the owned clear");
    assert!(owned.iter().all(|record| record.section == Section::Owned));
}

/// A staged move into a directory a delete stages after the move's checks is refused under its
/// locks, on that directory's word.
#[test]
fn a_staged_move_into_a_directory_a_delete_stages_logs_unmovable() {
    let runs = raced(
        || Box::pin(Tree::new(&["a/f", "b/"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let permit = tree.state.moves_permit().await.expect("the permit");
                    let _ = tree
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
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_delete_level(
                            &tree.repository,
                            tree.id("b"),
                            lore_revision::node::ROOT_NODE,
                        )
                        .await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&runs, Section::MoveNamed, |tree, effect| {
            matches!(effect, Effect::Refused { node, parent, reason: Reason::Unmovable, read: Read::Word(words), .. }
                if *node == tree.id("a/f") && *parent == tree.id("b") && words.0.is_staged_delete())
        }),
        "no run staged 'b' for deletion between the move's checks and its locks"
    );
}

/// A staged move out of a directory a detach takes after the move's marks is refused under its
/// locks, on that directory's word.
#[test]
fn a_staged_move_out_of_a_directory_a_detach_takes_logs_unmovable() {
    let runs = raced(
        || Box::pin(Tree::new(&["c/g", "e/"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let permit = tree.state.moves_permit().await.expect("the permit");
                    let _ = tree
                        .state
                        .node_move_staged(
                            tree.repository.clone(),
                            tree.id("c/g"),
                            tree.id("e"),
                            ("g", hash_string("g")),
                            &permit,
                            false,
                        )
                        .await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_detach(tree.repository.clone(), tree.id("c"), Detach::Always)
                        .await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&runs, Section::MoveNamed, |tree, effect| {
            matches!(effect, Effect::Refused { node, parent, reason: Reason::Unmovable, read: Read::Word(words), .. }
                if *node == tree.id("c/g") && *parent == tree.id("c") && words.0.is_discarded())
        }),
        "no run detached 'c' between the move's marks and its locks"
    );
}

/// A rename by a trusted move whose node a detach takes before its locks is refused on the node's
/// own word, Discarded, in a run the model replays.
#[test]
fn a_trusted_rename_of_a_node_detached_meanwhile_logs_not_found() {
    let runs = raced(
        || Box::pin(Tree::new(&["d/f"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let moved = tree
                        .state
                        .node_move(
                            tree.repository.clone(),
                            tree.id("d/f"),
                            tree.id("d"),
                            NameChange::Rename("g"),
                            Mark::dirty(NodeFlags::DirtyMove, Ext::Keep, true),
                        )
                        .await;
                    assert!(
                        moved.is_ok() || moved.is_err_and(|error| error.is_node_not_found()),
                        "the rename lands, or finds 'd/f' gone"
                    );
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    tree.state
                        .node_detach(tree.repository.clone(), tree.id("d/f"), Detach::Always)
                        .await
                        .expect("'d/f' detaches");
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&runs, Section::RenameInPlace, |tree, effect| {
            matches!(effect, Effect::Refused { node, parent, reason: Reason::NotFound, read: Read::Word(words), .. }
                if *node == tree.id("d/f") && parent == node && words.0.is_discarded())
        }),
        "no run detached 'd/f' before the rename's locks"
    );
}

/// A C API modify whose file a delete stages after its first check is refused under its lock, on
/// the file's own word.
#[test]
fn a_modify_overtaken_by_a_delete_logs_its_refusal() {
    let runs = raced(
        || Box::pin(Tree::new(&["d/f"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_modify_staged(&tree.repository, tree.id("d/f"), &Content::default())
                        .await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let marked = tree
                        .state
                        .node_delete_level(&tree.repository, tree.id("d/f"), tree.id("d"))
                        .await
                        .expect("the level marks");
                    assert!(matches!(marked, LevelMark::Marked(_)));
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&runs, Section::StagedModify, |tree, effect| {
            matches!(effect, Effect::Refused { node, parent, reason: Reason::Rewrite, read: Read::Word(words), .. }
                if *node == tree.id("d/f") && parent == node && words.0.is_staged_delete())
        }),
        "no run staged 'd/f' for deletion between the modify's check and its lock"
    );
}

/// A staged rename in place of a node a delete stages after the move's checks is refused under
/// its locks, on the node's own word.
#[test]
fn a_staged_rename_of_a_node_a_delete_stages_logs_unmovable() {
    let runs = raced(
        || Box::pin(Tree::new(&["a/f"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let permit = tree.state.moves_permit().await.expect("the permit");
                    let _ = tree
                        .state
                        .node_move_staged(
                            tree.repository.clone(),
                            tree.id("a/f"),
                            tree.id("a"),
                            ("g", hash_string("g")),
                            &permit,
                            false,
                        )
                        .await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_delete_level(&tree.repository, tree.id("a/f"), tree.id("a"))
                        .await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&runs, Section::RenameInPlace, |tree, effect| {
            matches!(effect, Effect::Refused { node, parent, reason: Reason::Unmovable, read: Read::Word(words), .. }
                if *node == tree.id("a/f") && parent == node && words.0.is_staged_delete())
        }),
        "no run staged 'a/f' for deletion between the rename's checks and its locks"
    );
}

/// A rename whose node a detach on another thread takes in its window, between its read and its
/// locks, which no await separates, is refused on the node's own word, Discarded.
#[test]
fn a_rename_of_a_node_detached_in_its_window_logs_not_found() {
    let tree = Arc::new(current_thread().block_on(in_context(Tree::new(&["d/f"]))));
    let log = Collector::new();
    log.set_recording(true);
    let (scheduler, in_window, resume) = InWindow::stalling(log.clone(), 1);
    let renaming = tree.clone();
    let renamed = std::thread::spawn(move || {
        let installed = hooks::install(scheduler);
        let renamed = current_thread().block_on(in_context(hooks::as_task(
            0,
            renaming.state.node_rename(
                renaming.repository.clone(),
                renaming.id("d/f"),
                NameChange::Rename("g"),
            ),
        )));
        drop(installed);
        renamed
    });
    in_window.recv().expect("the rename reaches its window");
    current_thread()
        .block_on(in_context(tree.state.node_detach(
            tree.repository.clone(),
            tree.id("d/f"),
            Detach::Always,
        )))
        .expect("'d/f' detaches");
    resume.send(()).expect("the rename waits");
    let renamed = renamed.join().expect("the renaming thread ends");
    assert!(
        renamed.is_err_and(|error| error.is_node_not_found()),
        "the rename finds 'd/f' gone"
    );
    let records = log.drain();
    let f = tree.id("d/f");
    let refused = one(&records.iter().collect::<Vec<_>>(), "refusal", |effect| {
        matches!(effect, Effect::Refused { node, parent, reason: Reason::NotFound, read: Read::Word(words), .. }
            if *node == f && *parent == f && words.0.is_discarded())
    });
    assert_eq!(refused.section, Section::RenameInPlace);
}

/// A served rename of a directory holding local content, whose settle drops it between the
/// rename's first check and its locks, is refused under them, naming where it moved to.
#[test]
fn a_local_rename_overtaken_by_a_clear_logs_not_local() {
    let runs = raced(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/s/", "e/"]).await;
                tree.state.mark_served();
                crate::harness::catalogue::ops::hold_local(&tree, tree.id("d/s")).await;
                assert!(
                    crate::harness::catalogue::ops::served_mark_dir(&tree, tree.id("d/s")).await
                );
                tree
            })
        },
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let (s, e) = (tree.id("d/s"), tree.id("e"));
                    let _ = crate::harness::catalogue::ops::served_rename(&tree, s, e, "s").await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    crate::harness::catalogue::ops::settle(&tree, tree.id("d/s")).await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&runs, Section::Reattach, |tree, effect| {
            matches!(effect, Effect::Refused { node, reason: Reason::NotLocal, expected: Expected::At { parent, name_hash }, .. }
                if *node == tree.id("d/s") && *parent == tree.id("e") && *name_hash == hash_string("s"))
        }),
        "no run cleared 'd/s' between the rename's checks"
    );
}

/// A staged move of a node a delete stages after the move's checks is refused under its locks,
/// on the node's own word.
#[test]
fn a_staged_move_of_a_node_a_delete_stages_logs_unmovable() {
    let runs = raced(
        || Box::pin(Tree::new(&["a/f", "b/"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let permit = tree.state.moves_permit().await.expect("the permit");
                    let _ = tree
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
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = tree
                        .state
                        .node_delete_level(&tree.repository, tree.id("a/f"), tree.id("a"))
                        .await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&runs, Section::MoveNamed, |tree, effect| {
            matches!(effect, Effect::Refused { node, parent, reason: Reason::Unmovable, read: Read::Word(words), .. }
                if *node == tree.id("a/f") && parent == node && words.0.is_staged_delete())
        }),
        "no run staged 'a/f' for deletion between the move's checks and its locks"
    );
}

/// The served tree `paths` with `marked` dirty, as local changes leave them.
async fn served_marked(paths: &[&str], marked: &[&str]) -> Tree {
    serve(Tree::new(paths).await, marked).await
}

/// `tree` served, with `marked` dirty.
async fn serve(tree: Tree, marked: &[&str]) -> Tree {
    tree.state.mark_served();
    for dir in marked {
        assert!(crate::harness::catalogue::ops::served_mark_dir(&tree, tree.id(dir)).await);
    }
    tree
}

/// A served mark's level refused under its lock, its node or the parent it hangs from gone since
/// its descent read them, logs the word it found Discarded: the directory an RMDIR took, and the
/// directory a removal took above the node marked.
#[test]
fn a_served_marks_level_refused_under_its_lock_logs_the_word_gone() {
    let removed = raced(
        || Box::pin(served_marked(&["a/d/"], &["a"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    crate::harness::catalogue::ops::served_mark_dir(&tree, tree.id("a/d")).await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    crate::harness::catalogue::ops::served_rmdir(&tree, tree.id("a/d")).await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&removed, Section::ServedMark, |tree, effect| {
            let d = tree.id("a/d");
            matches!(effect, Effect::Refused { node, parent, reason: Reason::NotFound, read: Read::Word(words), .. }
                if *node == d && *parent == d && words.0.is_discarded())
        }),
        "no run's level found the directory it marks gone under its lock"
    );
    // The parent in the node's block, and in a block of its own, which the level read-locks apart.
    let setups: [super::Setup<Tree>; 2] = [
        || Box::pin(async { serve(Tree::new(&["a/d/e/"]).await, &["a", "a/d"]).await }),
        || {
            Box::pin(async {
                let tree = Tree::spread(&["a/d/"], "fill", &["a/d/e/"]).await;
                serve(tree, &["a", "a/d"]).await
            })
        },
    ];
    for (spread, setup) in setups.into_iter().enumerate() {
        let above = raced(
            setup,
            vec![
                Box::new(|tree: Arc<Tree>| {
                    Box::pin(async move {
                        crate::harness::catalogue::ops::served_mark_dir(&tree, tree.id("a/d/e"))
                            .await;
                    })
                }),
                Box::new(|tree: Arc<Tree>| {
                    Box::pin(async move {
                        crate::harness::catalogue::ops::served_detach(&tree, tree.id("a/d")).await;
                    })
                }),
            ],
        );
        assert!(
            refused_in_some_run(&above, Section::ServedMark, |tree, effect| {
                let (d, e) = (tree.id("a/d"), tree.id("a/d/e"));
                matches!(effect, Effect::Refused { node, parent, reason: Reason::NotFound, read: Read::Word(words), .. }
                    if *node == e && *parent == d && words.0.is_discarded())
            }),
            "spread {spread}: no run's level found the parent it marks below gone under its lock"
        );
    }
}

/// A served rename into a directory a create changes between the rename's walk and its locks
/// finds the count moved under the locks, walks again, and moves: the reattach's count decides.
#[test]
fn a_served_rename_into_a_directory_a_create_changes_walks_again() {
    let runs = raced(
        || Box::pin(served_marked(&["a/x", "e/"], &["a", "e"])),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    crate::harness::catalogue::ops::served_rename(
                        &tree,
                        tree.id("a/x"),
                        tree.id("e"),
                        "y",
                    )
                    .await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    crate::harness::catalogue::ops::served_create(&tree, tree.id("e"), "n").await;
                })
            }),
        ],
    );
    let walked_again = runs.iter().any(|(log, tree)| {
        let (id, x) = (tree.state.hooks().id(), tree.id("a/x"));
        let on: Vec<&Record> = log.iter().filter(|record| record.state == id).collect();
        on.iter()
            .position(|record| record.effect == Effect::Arm(Arm::ReattachChanged))
            .is_some_and(|at| {
                on[at..]
                    .iter()
                    .any(|record| matches!(record.effect, Effect::Move { node, .. } if node == x))
            })
    });
    assert!(
        walked_again,
        "no run's rename found the count moved and walked again"
    );
}

/// A commit's flip of a file in place refused under its lock, a removal of the file landing
/// between its read and its lock, logs the word it found Discarded.
#[test]
fn a_flip_refused_under_its_lock_logs_the_file_gone() {
    let removed = raced(
        || {
            Box::pin(async {
                let tree = served_marked(&["a/f"], &[]).await;
                assert!(
                    crate::harness::catalogue::ops::served_copy_up(
                        &tree,
                        tree.id("a/f"),
                        tree.id("a")
                    )
                    .await
                );
                tree
            })
        },
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    crate::harness::catalogue::ops::flip(&tree, tree.id("a/f")).await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    crate::harness::catalogue::ops::served_detach(&tree, tree.id("a/f")).await;
                })
            }),
        ],
    );
    assert!(
        refused_in_some_run(&removed, Section::InPlace, |tree, effect| {
            let f = tree.id("a/f");
            matches!(effect, Effect::Refused { node, reason: Reason::InPlaceAway, read: Read::Word(words), .. }
                if *node == f && words.0.is_discarded())
        }),
        "no run's flip found its file gone under its lock"
    );
}

/// H2's producer: a served RMDIR's detach refused at its pre-test, its parent's clear pending in a
/// settle's window so the parent admits no change, ends there and marks no parent. A child created
/// since the RMDIR read its directory moves the count it expects. Run through its call, so the
/// model judges the order of its pre-test and its marks.
#[test]
fn a_detach_refused_at_its_pre_test_below_a_pending_clear_marks_no_parent() {
    use crate::harness::catalogue;
    use crate::harness::catalogue::call::Answer;
    use crate::harness::catalogue::call::Outcome;
    use crate::harness::catalogue::call::Refusal;
    use crate::harness::catalogue::call::call;
    use crate::harness::catalogue::ops;

    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/e/"]).await;
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
                ops::served_create(&tree, e, "x")
                    .await
                    .expect("'d/e/x' is created");
                // Refused on the count 'x' moved, which leaves the directory not empty.
                let rmdir = call(&tree.state, (&catalogue::SERVED_RMDIR, Some(e)), async {
                    let detached = tree
                        .state
                        .served()
                        .node_detach(tree.repository.clone(), e, Detach::IfUnchanged(&read))
                        .await
                        .expect("the detach runs");
                    let outcome = if detached {
                        Outcome::Done(Answer::None)
                    } else {
                        Outcome::Refused(Refusal::NotEmpty)
                    };
                    ((), outcome)
                });
                ops::settle_waiting(&tree, tree.id("d"), rmdir).await;
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (d, e) = (tree.id("d"), tree.id("d/e"));
    let refused = one(
        &records,
        "refused pre-test",
        |effect| matches!(effect, Effect::PreTest { node, refused: true, .. } if *node == e),
    );
    let pending = records.iter().any(|record| {
        record.seq < refused.seq
            && record.section == Section::SettlePhase1
            && matches!(record.effect, Effect::Word { node, clear_pending: true, .. } if node == d)
    });
    assert!(
        pending,
        "the settle set 'd's clear pending before the pre-test"
    );
    assert!(
        !records.iter().any(|record| record.seq > refused.seq
            && record.section == Section::ServedMark
            && matches!(record.effect, Effect::Word { node, .. } if node == d)),
        "the refused detach marked no parent"
    );
}
