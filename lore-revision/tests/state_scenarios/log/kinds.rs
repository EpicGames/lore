// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Each record kind at the site that makes it, with its fields: one operation run alone, or a
//! context its setup prepared so the site is reached deterministically.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use lore_base::types::Hash;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::AttachOutcome;
use lore_revision::state::Clear;
use lore_revision::state::ClearOutcome;
use lore_revision::state::Content;
use lore_revision::state::Detach;
use lore_revision::state::Ext;
use lore_revision::state::LevelMark;
use lore_revision::state::LinkPin;
use lore_revision::state::Mark;
use lore_revision::state::NameChange;
use lore_revision::state::OwnFlags;
use lore_revision::state::Rewrite;
use lore_revision::state::State;
use lore_revision::state::StateNodeChildrenIterator;
use lore_revision::state::TreeAttach;
use lore_revision::state::WalkDiscards;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::log;
use lore_revision::state::node::hooks::log::Arm;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Expected;
use lore_revision::state::node::hooks::log::GuardEvent;
use lore_revision::state::node::hooks::log::How;
use lore_revision::state::node::hooks::log::Kind;
use lore_revision::state::node::hooks::log::Read;
use lore_revision::state::node::hooks::log::Reason;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_storage::hash::hash_string;

use super::logged;
use super::logged_as;
use super::logged_runs;
use super::of;
use super::one;
use super::section_of;
use super::word_records;
use super::words_of;
use crate::current_thread;
use crate::harness::catalogue::modelled::Labels;
use crate::harness::catalogue::ops::Settle;
use crate::harness::catalogue::ops::file;
use crate::harness::faults::FailingStore;
use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Exploration;
use crate::in_context;

#[test]
fn an_add_logs_its_grab_its_initialization_and_its_publish_in_its_task() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let d = tree.id("d");
                tree.state
                    .node_add(tree.repository.clone(), d, file("f"), "f")
                    .await
                    .expect("'d/f' adds");
            })
        }),
    );
    assert!(
        records.iter().all(|record| record.task == Some(0)),
        "every record names the operation's task: {records:#?}"
    );
    let records = of(&records, &tree.state);
    let d = tree.id("d");
    let publish = one(&records, "publish", |effect| {
        matches!(effect, Effect::Publish { parent, name, name_hash, kind: Kind::File, check: log::Check::Live, expected: Expected::Nothing, .. }
            if *parent == d && name == "f" && *name_hash == hash_string("f"))
    });
    let Effect::Publish { node: f, .. } = publish.effect else {
        unreachable!()
    };
    assert_eq!(publish.section, Section::Publish);
    let grab = one(
        &records,
        "grab",
        |effect| matches!(effect, Effect::Grab { node, .. } if *node == f),
    );
    assert_eq!(grab.section, Section::Grab);
    // The grab's own change is the model's: the slot's first word is its initialization's.
    let words = word_records(&records, f);
    assert_eq!(words.len(), 2, "initialized, then published: {words:#?}");
    assert_eq!(words[0].section, Section::Grab);
    assert!(
        grab.seq < words[0].seq,
        "initialized after the grab's section"
    );
    let (initialized, published) = (words_of(&records, f)[0], words_of(&records, f)[1]);
    assert!(initialized.is_discarded() && initialized.is_file());
    assert!(!published.is_discarded() && published.parent == d);
    assert_eq!(
        words[1].seq, publish.seq,
        "the publish's word is in its section"
    );
}

#[test]
fn a_detach_logs_its_unlink_and_its_words_in_one_section() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f", "d/g"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let detached = tree
                    .state
                    .node_detach(tree.repository.clone(), tree.id("d/f"), Detach::Always)
                    .await
                    .expect("the detach runs");
                assert!(detached);
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (d, f) = (tree.id("d"), tree.id("d/f"));
    let unlink = one(
        &records,
        "unlink",
        |effect| matches!(effect, Effect::Detach { node, parent, how: How::Always, .. } if *node == f && *parent == d),
    );
    let section = section_of(&records, unlink);
    assert!(
        section
            .iter()
            .all(|record| record.section == Section::Detach),
        "the detach's section is labelled: {section:#?}"
    );
    assert!(
        words_of(&section, f)
            .last()
            .is_some_and(|node| node.is_discarded()),
        "the section leaves 'd/f' Discarded"
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.effect, Effect::Pend { .. })),
        "only the C API delete's detach queues its root"
    );
}

#[test]
fn an_empty_directory_detached_if_empty_logs_how() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/e/"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let detached = tree
                    .state
                    .node_detach(tree.repository.clone(), tree.id("d/e"), Detach::IfEmpty)
                    .await
                    .expect("the detach runs");
                assert!(detached);
            })
        }),
    );
    let records = of(&records, &tree.state);
    let e = tree.id("d/e");
    one(&records, "unlink", |effect| {
        matches!(effect, Effect::Detach { node, how: How::IfEmpty, expected: Expected::Head(head), .. }
            if *node == e && !head.is_valid_node_id())
    });
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.effect, Effect::PreTest { .. })),
        "only a served detach logs its pre-test"
    );
}

/// A refusal decided under a section's locks; one decided on a copy read before them, as an
/// unserved detach's pre-test is, records nothing.
#[test]
fn a_refusal_in_a_section_logs_the_word_it_decided_on() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f", "d/g"]).await;
                tree.state
                    .node_mark(
                        &tree.repository,
                        tree.id("d/g"),
                        Mark::dirty(NodeFlags::DirtyModify, Ext::Keep, false),
                    )
                    .await
                    .expect("'d/g' is marked");
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let detached = tree
                    .state
                    .node_detach(tree.repository.clone(), tree.id("d/g"), Detach::IfClean)
                    .await
                    .expect("the detach runs");
                assert!(!detached, "the detach of a dirty node is refused");
                let restored = tree
                    .state
                    .node_restore(&tree.repository, tree.id("d/f"), &Content::default())
                    .await;
                assert!(
                    restored.is_err(),
                    "the restore of a node not deleted is refused"
                );
            })
        }),
    );
    let records = of(&records, &tree.state);
    let f = tree.id("d/f");
    let refused = one(&records, "refusal", |effect| {
        matches!(effect, Effect::Refused { node, parent, reason: Reason::Restore, .. }
            if *node == f && *parent == f)
    });
    assert_eq!(refused.section, Section::Restore);
    let Effect::Refused {
        read: Read::Word(words),
        ..
    } = &refused.effect
    else {
        panic!("the refusal reads the node's word: {refused:?}");
    };
    assert!(
        !words.0.is_staged_delete(),
        "the word decided on holds no delete"
    );
    assert!(
        words_of(&records, f).is_empty(),
        "the refused section wrote nothing"
    );
    assert_eq!(
        section_of(&records, refused).len(),
        1,
        "the refusal alone in its section"
    );
    let g = tree.id("d/g");
    assert!(
        !records.iter().any(|record| matches!(
            record.effect,
            Effect::Refused { node, .. } | Effect::Detach { node, .. } if node == g
        )),
        "the pre-test's refusal records nothing"
    );
}

/// A restore refused for its parent reads the parent's word, and names it.
#[test]
fn a_restore_below_a_deleted_parent_logs_the_parents_word() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                let (d, f) = (tree.id("d"), tree.id("d/f"));
                for (node, parent) in [(d, ROOT_NODE), (f, d)] {
                    tree.state
                        .node_delete_level(&tree.repository, node, parent)
                        .await
                        .expect("the level marks");
                }
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let _ = tree
                    .state
                    .node_restore(&tree.repository, tree.id("d/f"), &Content::default())
                    .await;
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (d, f) = (tree.id("d"), tree.id("d/f"));
    let refused = one(&records, "refusal", |effect| {
        matches!(effect, Effect::Refused { node, parent, reason: Reason::Restore, read: Read::Word(words), .. }
            if *node == f && *parent == d && words.0.is_staged_delete())
    });
    assert_eq!(refused.section, Section::Restore);
}

#[test]
fn the_delete_levels_answers_log_their_refusals() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f", "e/"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let (d, e, f) = (tree.id("d"), tree.id("e"), tree.id("d/f"));
                let repository = &tree.repository;
                let mut marked = Vec::new();
                for parent in [e, d, d] {
                    let level = tree.state.node_delete_level(repository, f, parent).await;
                    marked.push(level.expect("the level runs"));
                }
                assert_eq!(
                    marked,
                    [LevelMark::Other, LevelMark::Marked(None), LevelMark::Staged]
                );
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (e, f) = (tree.id("e"), tree.id("d/f"));
    let other = one(&records, "other", |effect| {
        matches!(effect, Effect::Refused { node, reason: Reason::DeleteLevelOther, expected: Expected::At { parent, .. }, .. }
            if *node == f && *parent == e)
    });
    // The level that marks arms its guard around the climb above it.
    let guards: Vec<(u64, GuardEvent)> = records
        .iter()
        .filter_map(|record| match record.effect {
            Effect::Guard { event } => Some((record.seq, event)),
            _ => None,
        })
        .collect();
    let events: Vec<GuardEvent> = guards.iter().map(|(_, event)| *event).collect();
    assert_eq!(events, [GuardEvent::Armed, GuardEvent::Finished]);
    let d = tree.id("d");
    let climb = one(
        &records,
        "climb",
        |effect| matches!(effect, Effect::Word { node, .. } if *node == d),
    );
    assert!(
        guards[0].0 < climb.seq && climb.seq < guards[1].0,
        "armed, the climb, finished"
    );
    let staged = one(&records, "staged", |effect| {
        matches!(effect, Effect::Refused { node, reason: Reason::DeleteLevelStaged, read: Read::Word(words), .. }
            if *node == f && words.0.is_staged_delete())
    });
    assert_eq!(other.section, Section::DeleteLevel);
    assert_eq!(staged.section, Section::DeleteLevel);
}

#[test]
fn the_rewrites_refusals_log_their_reasons() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f", "d/h"]).await;
                tree.state
                    .node_mark(
                        &tree.repository,
                        tree.id("d/f"),
                        Mark::dirty(NodeFlags::DirtyModify, Ext::Keep, false),
                    )
                    .await
                    .expect("'d/f' is marked");
                let marked = tree
                    .state
                    .node_delete_level(&tree.repository, tree.id("d/h"), tree.id("d"))
                    .await;
                assert_eq!(marked.expect("'d/h' is deleted"), LevelMark::Marked(None));
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let (d, f, h) = (tree.id("d"), tree.id("d/f"), tree.id("d/h"));
                let repository = &tree.repository;
                let content = Content {
                    size: 5,
                    ..Content::default()
                };
                for (node, rewrite, refused) in [
                    (d, Rewrite::Plain, "the rewrite of a directory"),
                    (f, Rewrite::IfClean, "the clean rewrite of a dirty file"),
                    (
                        h,
                        Rewrite::Plain,
                        "the rewrite of a file staged for deletion",
                    ),
                ] {
                    let rewritten = tree
                        .state
                        .node_rewrite(repository, node, &content, rewrite)
                        .await;
                    assert!(
                        !rewritten.is_ok_and(|written| written),
                        "{refused} is refused"
                    );
                }
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (d, f, h) = (tree.id("d"), tree.id("d/f"), tree.id("d/h"));
    let staged = one(
        &records,
        "rewrite of a file staged for deletion",
        |effect| {
            matches!(effect, Effect::Refused { node, reason: Reason::Rewrite, read: Read::Word(words), .. }
            if *node == h && words.0.is_staged_delete())
        },
    );
    assert_eq!(staged.section, Section::Rewrite);
    let plain = one(&records, "rewrite", |effect| {
        matches!(effect, Effect::Refused { node, reason: Reason::Rewrite, read: Read::Word(words), .. }
            if *node == d && !words.0.is_file())
    });
    let clean = one(&records, "if clean", |effect| {
        matches!(effect, Effect::Refused { node, reason: Reason::IfClean, read: Read::Word(words), .. }
            if *node == f && words.0.is_dirty())
    });
    assert_eq!(plain.section, Section::Rewrite);
    assert_eq!(clean.section, Section::Rewrite);
}

#[test]
fn a_served_rewrite_of_a_directory_logs_its_refusal() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/e/"]).await;
                tree.state.mark_served();
                // Its parent admitting the change, so the kind alone refuses it.
                assert!(crate::harness::catalogue::ops::served_mark_dir(&tree, tree.id("d")).await);
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let _ = tree
                    .state
                    .served()
                    .node_rewrite(
                        &tree.repository,
                        tree.id("d/e"),
                        tree.id("d"),
                        &Content::default(),
                        (NodeFlags::DirtyModify, NodeExtFlags::NoFlags),
                    )
                    .await;
            })
        }),
    );
    let records = of(&records, &tree.state);
    let e = tree.id("d/e");
    let refused = one(&records, "refusal", |effect| {
        matches!(effect, Effect::Refused { node, parent, reason: Reason::ServedRewrite, .. }
            if *node == e && *parent == e)
    });
    assert_eq!(refused.section, Section::Rewrite);
}

#[test]
fn a_rename_logs_its_rename_in_its_section() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                tree.state
                    .node_rename(
                        tree.repository.clone(),
                        tree.id("d/f"),
                        NameChange::Rename("g"),
                    )
                    .await
                    .expect("'d/f' renames");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (d, f) = (tree.id("d"), tree.id("d/f"));
    let rename = one(&records, "rename", |effect| {
        matches!(effect, Effect::Rename { node, parent, name_hash, name, .. }
            if *node == f && *parent == d && *name_hash == hash_string("g") && name == "g")
    });
    assert_eq!(rename.section, Section::RenameInPlace);
    assert_eq!(
        section_of(&records, rename).len(),
        1,
        "a rename writes no word"
    );
}

#[test]
fn a_directory_reattach_logs_its_move_inside_the_moves_permit() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["a/x/", "b/"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                tree.state
                    .node_reattach(tree.repository.clone(), tree.id("a/x"), tree.id("b"), "y")
                    .await
                    .expect("'a/x' moves");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (a, b, x) = (tree.id("a"), tree.id("b"), tree.id("a/x"));
    let moved = one(&records, "move", |effect| {
        matches!(effect, Effect::Move { node, from, to, name, replaced: None, check: log::Check::Walk, expected: Expected::Count { .. }, .. }
            if *node == x && *from == a && *to == b && name.as_ref().is_some_and(|name| name == "y"))
    });
    assert_eq!(moved.section, Section::Reattach);
    let permit = one(&records, "permit", |effect| *effect == Effect::Permit);
    let unpermit = one(&records, "release", |effect| *effect == Effect::Unpermit);
    assert!(
        permit.seq < moved.seq && moved.seq < unpermit.seq,
        "the move lies inside the permit"
    );
}

#[test]
fn a_reattach_replacing_a_holder_logs_the_holder() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["a/f", "b/g"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let (f, b, g) = (tree.id("a/f"), tree.id("b"), tree.id("b/g"));
                tree.state
                    .node_reattach_replacing(tree.repository.clone(), f, b, "g", g)
                    .await
                    .expect("'a/f' replaces 'b/g'");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (f, g) = (tree.id("a/f"), tree.id("b/g"));
    let moved = one(&records, "move", |effect| {
        matches!(effect, Effect::Move { node, name_hash, name, replaced: Some(holder), check: log::Check::Replacing, .. }
            if *node == f && *holder == g && *name_hash == hash_string("g")
                && name.as_ref().is_some_and(|name| name == "g"))
    });
    assert!(
        words_of(&section_of(&records, moved), g)
            .last()
            .is_some_and(Node::is_discarded),
        "the holder is Discarded in the move's section"
    );
}

#[test]
fn a_move_to_another_directory_logs_its_move_in_its_section() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["a/f", "b/"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let (f, b) = (tree.id("a/f"), tree.id("b"));
                let own = Mark::staged_and_dirty(NodeFlags::StagedModify, NodeFlags::DirtyModify);
                tree.state
                    .node_move(tree.repository.clone(), f, b, NameChange::Keep, own)
                    .await
                    .expect("'a/f' moves");
                // The destination's climb, as `lore dirty` makes it after the move.
                tree.state
                    .node_mark_destination(&tree.repository, b, own)
                    .await
                    .expect("'b' is marked");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (a, b, f) = (tree.id("a"), tree.id("b"), tree.id("a/f"));
    let moved = one(&records, "move", |effect| {
        matches!(effect, Effect::Move { node, from, to, name: None, check: log::Check::Unchecked, .. }
            if *node == f && *from == a && *to == b)
    });
    assert_eq!(moved.section, Section::MoveNamed);
    assert!(
        words_of(&section_of(&records, moved), f)
            .last()
            .is_some_and(Node::is_staged),
        "the move's own mark is in its section"
    );
}

#[test]
fn a_discard_logs_its_unlink_and_its_free_in_one_section() {
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
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (d, f) = (tree.id("d"), tree.id("d/f"));
    let unlink = one(
        &records,
        "unlink",
        |effect| matches!(effect, Effect::Detach { node, parent, how: How::Discard, .. } if *node == f && *parent == d),
    );
    assert_eq!(unlink.section, Section::DiscardMarks);
    let free = one(
        &records,
        "free",
        |effect| matches!(effect, Effect::Free { node, .. } if *node == f),
    );
    assert_eq!(
        (free.seq, free.sub, free.section),
        (unlink.seq, unlink.sub + 1, Section::DiscardMarks),
        "the free follows the unlink in its section"
    );
    assert!(
        words_of(&records, f).is_empty(),
        "a freed slot's own words are the model's"
    );
}

/// A discard of a subtree marks each directory and frees each file in tasks of their own, which
/// run as the operation's (`subtask`).
#[test]
fn a_subtree_discard_logs_its_marks_and_frees_in_inherited_tasks() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/e/x"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                lore_revision::state::node_discard_fused(
                    tree.state.clone(),
                    tree.repository.clone(),
                    tree.id("d"),
                )
                .await
                .expect("'d' is discarded");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (e, x) = (tree.id("d/e"), tree.id("d/e/x"));
    let marked = one(
        &records,
        "mark",
        |effect| matches!(effect, Effect::Discarding { node } if *node == e),
    );
    assert_eq!(marked.section, Section::DiscardMarks);
    let freed = one(
        &records,
        "free",
        |effect| matches!(effect, Effect::Free { node, .. } if *node == x),
    );
    assert!(
        freed.task.is_some_and(|task| task != 0),
        "freed in a task the operation spawned: {freed:?}"
    );
}

#[test]
fn a_fresh_block_logs_its_install_and_the_grab_names_its_instance() {
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
    let allocated = one(&records, "install", |effect| {
        matches!(effect, Effect::Allocated { block: 1, .. })
    });
    let grab = one(
        &records,
        "grab",
        |effect| matches!(effect, Effect::Grab { node, .. } if NodeBlock::index(*node) == 1),
    );
    assert!(allocated.seq < grab.seq, "the block is installed first");
    let (
        Effect::Allocated { instance, .. },
        Effect::Grab {
            instance: grabbed, ..
        },
    ) = (&allocated.effect, &grab.effect)
    else {
        unreachable!()
    };
    assert_eq!(instance, grabbed, "the grab names the block's instance");
}

#[test]
fn a_block_read_from_the_store_logs_its_install_and_a_reload_a_new_instance() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                Tree::spread(&["a/"], "fill", &["b/"])
                    .await
                    .reloaded()
                    .await
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let b = tree.id("b");
                tree.state
                    .node(tree.repository.clone(), b)
                    .await
                    .expect("'b' reads");
                tree.state.evict_blocks_for_tests();
                tree.state
                    .node(tree.repository.clone(), b)
                    .await
                    .expect("'b' reads again");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let loads: Vec<u64> = records
        .iter()
        .filter_map(|record| match record.effect {
            Effect::Loaded {
                block: 1,
                instance,
                zeroed: false,
            } => Some(instance),
            _ => None,
        })
        .collect();
    assert_eq!(
        loads.len(),
        2,
        "loaded, evicted, loaded again: {records:#?}"
    );
    assert_ne!(loads[0], loads[1], "a reload is another instance");
}

#[test]
fn a_new_states_block_logs_its_zeroed_install() {
    let made: Arc<Mutex<Option<u64>>> = Arc::default();
    let into = made.clone();
    let (records, _) = logged(
        || Box::pin(Tree::new(&["d/"])),
        Box::new(move |tree: Arc<Tree>| {
            let into = into.clone();
            Box::pin(async move {
                let fresh = State::new();
                fresh
                    .node_add(tree.repository.clone(), ROOT_NODE, file("f"), "f")
                    .await
                    .expect("'f' adds");
                *into.lock().unwrap() = Some(fresh.hooks().id());
            })
        }),
    );
    let fresh = made.lock().unwrap().expect("the operation ran");
    let seq_of = |what: &str, matches: fn(&Effect) -> bool| {
        records
            .iter()
            .find(|record| record.state == fresh && matches(&record.effect))
            .map_or_else(
                || panic!("no {what} of the new State: {records:#?}"),
                |record| record.seq,
            )
    };
    let created = seq_of("creation", |effect| {
        matches!(effect, Effect::Created { from: None })
    });
    // Instances count from 1, 0 naming no block.
    let installed = seq_of("zeroed install of block 0", |effect| {
        matches!(
            effect,
            Effect::Loaded {
                block: 0,
                zeroed: true,
                instance: 1,
            }
        )
    });
    assert!(created < installed, "the State made before its first block");
}

#[test]
fn a_pending_discard_logs_its_detach_its_queue_and_its_marks() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let marked = tree
                    .state
                    .node_discard_pending(
                        tree.repository.clone(),
                        tree.id("d"),
                        ROOT_NODE,
                        hash_string("d"),
                    )
                    .await
                    .expect("'d' is discarded");
                assert_eq!(marked, Some(2));
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (d, f) = (tree.id("d"), tree.id("d/f"));
    let unlink = one(&records, "unlink", |effect| {
        matches!(effect, Effect::Detach { node, how: How::At, expected: Expected::At { parent: ROOT_NODE, name_hash }, .. }
            if *node == d && *name_hash == hash_string("d"))
    });
    let pend = one(
        &records,
        "queue",
        |effect| matches!(effect, Effect::Pend { root } if *root == d),
    );
    assert_eq!(
        (pend.seq, pend.sub),
        (unlink.seq, unlink.sub + 1),
        "queued in the detach's section"
    );
    let marked = one(
        &records,
        "mark",
        |effect| matches!(effect, Effect::Discarding { node } if *node == f),
    );
    assert_eq!(marked.section, Section::DiscardMarks);
    assert!(
        words_of(&section_of(&records, marked), f)
            .last()
            .is_some_and(Node::is_discarded),
        "the mark's word is in its section"
    );
}

#[test]
fn a_staged_modify_logs_its_guard_around_its_climb_and_its_edit() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let content = Content {
                    mode: 0o644,
                    size: 7,
                    ..Content::default()
                };
                tree.state
                    .node_modify_staged(&tree.repository, tree.id("d/f"), &content)
                    .await
                    .expect("'d/f' is modified");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (d, f) = (tree.id("d"), tree.id("d/f"));
    let armed = one(&records, "arm", |effect| {
        *effect
            == Effect::Guard {
                event: GuardEvent::Armed,
            }
    });
    let finished = one(&records, "finish", |effect| {
        *effect
            == Effect::Guard {
                event: GuardEvent::Finished,
            }
    });
    let climb = one(
        &records,
        "climb",
        |effect| matches!(effect, Effect::Word { node, .. } if *node == d),
    );
    let edit = one(
        &records,
        "edit",
        |effect| matches!(effect, Effect::Word { node, .. } if *node == f),
    );
    assert_eq!(climb.section, Section::UnservedMark);
    assert_eq!(edit.section, Section::StagedModify);
    assert!(
        armed.seq < climb.seq && climb.seq < edit.seq && edit.seq < finished.seq,
        "armed, the climb, the edit, finished"
    );
}

/// A tree whose blocks the store fails to read once told to.
struct Failing {
    tree: Tree,
    store: Arc<FailingStore>,
}

impl Modelled for Failing {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }

    fn mend(&self) {
        self.store.fail(false);
        self.store.stall(false);
    }

    fn faulted(&self) -> bool {
        self.store.failed_outside_calls() > 0
    }
}

/// A modify whose climb fails to load a block drops its guard armed, which notes the change
/// unfinished.
#[test]
fn a_modify_failing_its_climb_logs_its_guard_dropped_armed() {
    let (records, failing) = logged(
        || {
            Box::pin(async {
                let tree = Tree::spread(&["a/"], "fill", &["a/b/f"]).await;
                let (tree, store) = tree.failing().await;
                Failing { tree, store }
            })
        },
        Box::new(|failing: Arc<Failing>| {
            Box::pin(async move {
                let tree = &failing.tree;
                let f = tree.id("a/b/f");
                tree.state
                    .node(tree.repository.clone(), f)
                    .await
                    .expect("'a/b/f' reads");
                failing.store.fail(true);
                let modified = tree
                    .state
                    .node_modify_staged(&tree.repository, f, &Content::default())
                    .await;
                assert!(modified.is_err(), "the climb fails to load 'a'");
            })
        }),
    );
    let records = of(&records, &failing.tree.state);
    let dropped = one(&records, "drop", |effect| {
        *effect
            == Effect::Guard {
                event: GuardEvent::DroppedArmed,
            }
    });
    let noted = one(&records, "note", |effect| {
        *effect == Effect::Unfinished { set: true }
    });
    assert!(dropped.seq < noted.seq, "the drop notes the change");
}

#[test]
fn a_served_detach_logs_its_pre_tests_only_where_conditional() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f", "d/g", "d/h"]).await;
                tree.state
                    .node_mark(
                        &tree.repository,
                        tree.id("d/g"),
                        Mark::dirty(NodeFlags::DirtyModify, Ext::Keep, false),
                    )
                    .await
                    .expect("'d/g' is marked");
                tree.state.mark_served();
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let served = tree.state.served();
                let repository = &tree.repository;
                for (node, when, detaches) in [
                    ("d/g", Detach::IfClean, false),
                    ("d/f", Detach::IfClean, true),
                    ("d/h", Detach::Always, true),
                ] {
                    let detached = served
                        .node_detach(repository.clone(), tree.id(node), when)
                        .await
                        .expect("the detach runs");
                    assert_eq!(detached, detaches, "the detach of '{node}'");
                }
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (f, g, h) = (tree.id("d/f"), tree.id("d/g"), tree.id("d/h"));
    one(
        &records,
        "refused pre-test",
        |effect| matches!(effect, Effect::PreTest { node, attempt: 0, refused: true, .. } if *node == g),
    );
    let tested = one(
        &records,
        "pre-test",
        |effect| matches!(effect, Effect::PreTest { node, attempt: 0, refused: false, .. } if *node == f),
    );
    let unlink = one(
        &records,
        "unlink",
        |effect| matches!(effect, Effect::Detach { node, how: How::IfClean, .. } if *node == f),
    );
    assert!(tested.seq < unlink.seq, "tested before the detach");
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.effect, Effect::PreTest { node, .. } if node == h)),
        "an unconditional detach tests nothing"
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.effect, Effect::Detach { node, .. } if node == g)),
        "the kept file is not unlinked"
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.effect, Effect::Pend { .. })),
        "only the C API delete's detach queues its root"
    );
}

#[test]
fn a_states_life_is_logged() {
    let made: Arc<Mutex<Option<(u64, u64, Hash)>>> = Arc::default();
    let into = made.clone();
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f"])),
        Box::new(move |tree: Arc<Tree>| {
            let into = into.clone();
            Box::pin(async move {
                let fresh = State::new();
                let signature = tree.save().await.expect("the State saves");
                let loaded = tree.load(signature).await;
                *into.lock().unwrap() = Some((fresh.hooks().id(), loaded.hooks().id(), signature));
                tree.state.mark_served();
                tree.state.note_unfinished_change();
                tree.state.clear_unfinished_change();
            })
        }),
    );
    let (fresh, loaded, signature) = made.lock().unwrap().expect("the operation ran");
    let created = |state: u64| {
        let found: Vec<_> = records
            .iter()
            .filter(|record| record.state == state)
            .filter_map(|record| match record.effect {
                Effect::Created { from } => Some(from),
                _ => None,
            })
            .collect();
        assert_eq!(found.len(), 1, "one creation of State {state}");
        found[0]
    };
    assert_eq!(created(fresh), None);
    assert_eq!(
        created(loaded),
        Some((Arc::as_ptr(&tree.repository) as usize, signature)),
        "a State read from the store names where it came from"
    );
    let records = of(&records, &tree.state);
    let served = one(&records, "serving", |effect| *effect == Effect::Served);
    let set = one(&records, "note", |effect| {
        *effect == Effect::Unfinished { set: true }
    });
    let cleared = one(&records, "clear", |effect| {
        *effect == Effect::Unfinished { set: false }
    });
    assert!(served.seq < set.seq && set.seq < cleared.seq);
}

/// A write logs a word for a node whose flags, extension flags, mode, size, address or, for a
/// link, `child` changed, each on its own; none for one handed out unchanged or changed only in
/// its structure.
#[test]
fn a_write_logs_words_only_for_the_nodes_it_changed() {
    let (records, tree) = logged_as(
        || {
            Box::pin(async {
                let mut tree = Tree::new(&["d/f", "d/g", "d/m", "d/s", "d/a", "d/x"]).await;
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
                let block =
                    invariants::block_of(&tree.state, &tree.repository, tree.id("d/f")).await;
                let edit = |path: &str, edit: fn(&mut Node)| {
                    tree.state.node_edit_for_tests(&block, tree.id(path), edit);
                };
                // Handed out and left as it was, and handed out with only its name's hash
                // changed: structure, which the structural records tell.
                edit("d/g", |_| {});
                edit("d/g", |node| node.name_hash = hash_string("h"));
                edit("d/f", |node| node.flags |= NodeFlags::Dirty.bits());
                edit("d/x", |node| {
                    node.ext_flags = NodeExtFlags::LocalContent.bits();
                });
                edit("d/m", |node| node.mode = 0o600);
                edit("d/s", |node| node.size = 41);
                edit("d/a", |node| node.address.hash = Hash::from_u64(5));
                edit("d/l", |node| node.child = 7);
            })
        }),
        Labels::Free,
    );
    let records = of(&records, &tree.state);
    assert!(
        words_of(&records, tree.id("d/g")).is_empty(),
        "'d/g' did not change"
    );
    let word = |path: &str| {
        let words = words_of(&records, tree.id(path));
        assert_eq!(words.len(), 1, "one word of '{path}': {records:#?}");
        words[0]
    };
    assert!(word("d/f").is_dirty());
    assert_eq!(word("d/x").ext_flags, NodeExtFlags::LocalContent.bits());
    assert_eq!(word("d/m").mode, 0o600);
    assert_eq!(word("d/s").size, 41);
    assert_eq!(word("d/a").address.hash, Hash::from_u64(5));
    assert_eq!(word("d/l").child, 7);
}

#[test]
fn a_settle_logs_its_pending_clear_and_its_clear() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
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
                let cleared = crate::harness::catalogue::ops::served_clear(&tree, tree.id("d"))
                    .await
                    .expect("the clear runs");
                assert_eq!(cleared, lore_revision::state::ClearOutcome::Cleared);
            })
        }),
    );
    let records = of(&records, &tree.state);
    let d = tree.id("d");
    let words = word_records(&records, d);
    let phases: Vec<_> = words
        .iter()
        .map(|record| match record.effect {
            Effect::Word {
                record: ref words,
                clear_pending,
                ..
            } => (record.section, clear_pending, words.0.is_dirty()),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        phases,
        [
            (Section::SettlePhase1, true, true),
            (Section::SettlePhase3, false, false)
        ],
        "phase 1 pends the clear, phase 3 clears: {words:#?}"
    );
}

#[test]
fn a_served_clear_of_a_moved_directory_logs_its_arm() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f", "e/"]).await;
                tree.state
                    .node_reattach(tree.repository.clone(), tree.id("d"), tree.id("e"), "d")
                    .await
                    .expect("'d' moves");
                tree.state.mark_served();
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let cleared = crate::harness::catalogue::ops::served_clear(&tree, tree.id("d"))
                    .await
                    .expect("the clear runs");
                assert!(matches!(cleared, ClearOutcome::Moved), "{cleared:?}");
            })
        }),
    );
    let records = of(&records, &tree.state);
    one(&records, "arm", |effect| {
        *effect == Effect::Arm(Arm::ClearMoved)
    });
    let d = tree.id("d");
    assert!(
        !records.iter().any(|record| {
            matches!(record.effect, Effect::Admitted { node, .. } | Effect::Word { node, .. }
                if node == d)
        }),
        "a settle of a moved directory admits and writes nothing of it"
    );
}

#[test]
fn a_mark_needing_a_load_logs_its_arms_and_its_slow_down() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::spread(&["a/"], "fill", &["b/c/"])
                    .await
                    .reloaded()
                    .await;
                tree.state.set_retry_bound_for_tests(Some(1));
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let marked = tree
                    .state
                    .served()
                    .node_mark(&tree.repository, tree.id("b/c"))
                    .await;
                assert!(marked.is_err(), "one attempt only loads the block");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let load = one(&records, "load", |effect| {
        *effect == Effect::Arm(Arm::MarkLoad)
    });
    let slow = one(&records, "slow-down", |effect| {
        *effect == Effect::Arm(Arm::SlowDown)
    });
    assert!(load.seq < slow.seq);
}

/// A served mark of a path deeper than one pass marks its deep directory first, the target
/// waiting, then resumes the target.
#[test]
fn a_deep_served_mark_logs_its_deep_and_waiting_passes() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["a/b/c/d/e/f/g/"]).await;
                tree.state.mark_served();
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                tree.state
                    .served()
                    .node_mark(&tree.repository, tree.id("a/b/c/d/e/f/g"))
                    .await
                    .expect("the path is marked");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let deep = records
        .iter()
        .position(|record| record.effect == Effect::Arm(Arm::MarkDeep))
        .expect("a deep pass");
    let waiting = records
        .iter()
        .position(|record| record.effect == Effect::Arm(Arm::MarkWaiting))
        .expect("a waiting target resumed");
    assert!(deep < waiting);
    let words: Vec<_> = records
        .iter()
        .filter(|record| matches!(record.effect, Effect::Word { .. }))
        .collect();
    assert!(
        words
            .iter()
            .all(|record| record.section == Section::ServedMark),
        "each level marked in a served mark's section"
    );
    let mut marked: Vec<NodeID> = words
        .iter()
        .filter_map(|record| match record.effect {
            Effect::Word { node, .. } => Some(node),
            _ => None,
        })
        .collect();
    marked.sort_unstable();
    marked.dedup();
    let mut levels: Vec<NodeID> = ["a", "a/b", "a/b/c", "a/b/c/d", "a/b/c/d/e", "a/b/c/d/e/f"]
        .iter()
        .chain(&["a/b/c/d/e/f/g"])
        .map(|path| tree.id(path))
        .collect();
    levels.sort_unstable();
    assert_eq!(marked, levels, "every level marked");
}

#[test]
fn a_links_pin_logs_its_repin_and_its_refusals() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let mut tree = Tree::new(&["d/f"]).await;
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
                let (f, l) = (tree.id("d/f"), tree.id("d/l"));
                let repository = &tree.repository;
                let hash = Hash::from_u64(7);
                tree.state
                    .node_set_link_target(repository, l, LinkPin::Revision(hash))
                    .await
                    .expect("the link pins");
                let refused = tree
                    .state
                    .node_set_link_target(repository, f, LinkPin::Revision(hash))
                    .await;
                assert!(
                    refused.is_err_and(|error| error.is_invalid_arguments()),
                    "the repin of a file is refused"
                );
                let whole = LinkPin::Whole {
                    address: lore_base::types::Address {
                        hash: Hash::from_u64(8),
                        ..Default::default()
                    },
                    child: f,
                };
                tree.state
                    .node_set_link_target(repository, l, whole)
                    .await
                    .expect("the link pins whole");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (f, l) = (tree.id("d/f"), tree.id("d/l"));
    let repin = one(
        &records,
        "repin",
        |effect| matches!(effect, Effect::Repin { node, whole: false, .. } if *node == l),
    );
    assert_eq!(repin.section, Section::Repin);
    assert!(
        words_of(&section_of(&records, repin), l)
            .last()
            .is_some_and(|node| node.address.hash == Hash::from_u64(7)),
        "the pin's word is in its section"
    );
    let refused = one(
        &records,
        "refusal",
        |effect| matches!(effect, Effect::Refused { node, reason: Reason::NotLink, .. } if *node == f),
    );
    assert_eq!(refused.section, Section::Repin);
    let whole = one(
        &records,
        "whole repin",
        |effect| matches!(effect, Effect::Repin { node, child, whole: true } if *node == l && *child == f),
    );
    assert_eq!(whole.section, Section::Repin);
    assert!(
        words_of(&section_of(&records, whole), l)
            .last()
            .is_some_and(|node| node.address.hash == Hash::from_u64(8) && node.child == f),
        "the whole pin's word is in its section"
    );
}

#[test]
fn the_in_place_and_owned_changes_log_their_refusals() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f", "e/"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let (e, f) = (tree.id("e"), tree.id("d/f"));
                let block = invariants::block_of(&tree.state, &tree.repository, f).await;
                let changed =
                    tree.state
                        .node_change_in_place(&block, f, (e, hash_string("f")), |held| {
                            held.set_size(9);
                            true
                        });
                assert!(
                    !changed,
                    "the change in place under another parent is refused"
                );
                assert!(
                    !tree.state.node_change_owned(&block, f, |_| false),
                    "the declined owned change writes nothing"
                );
                assert!(
                    !tree.state.node_clear_own(&block, f, OwnFlags::Dirty),
                    "the owned clear of a clean node writes nothing"
                );
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (e, f) = (tree.id("e"), tree.id("d/f"));
    let in_place = one(&records, "in place", |effect| {
        matches!(effect, Effect::Refused { node, reason: Reason::InPlaceAway, expected: Expected::At { parent, .. }, .. }
            if *node == f && *parent == e)
    });
    assert_eq!(in_place.section, Section::InPlace);
    let owned: Vec<_> = records
        .iter()
        .filter(|record| {
            matches!(record.effect, Effect::Refused { node, reason: Reason::Owned, .. } if node == f)
        })
        .collect();
    assert_eq!(owned.len(), 2, "the owned change and the owned clear");
    assert!(owned.iter().all(|record| record.section == Section::Owned));
}

/// A build: its private adds, then a publish that finds its name taken since its lookup, so it
/// retries and abandons its copy.
#[test]
fn a_build_logs_its_adds_and_its_abandon() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let d = tree.id("d");
                let root = Node {
                    name_hash: hash_string("c"),
                    ..Node::default()
                };
                let mut build = match tree
                    .state
                    .swap()
                    .node_attach_tree(tree.repository.clone(), d, &root, "c")
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
                tree.state
                    .node_add(tree.repository.clone(), d, file("c"), "c")
                    .await
                    .expect("'d/c' adds");
                let published = build.publish().await.expect("the publish runs");
                assert!(matches!(published, AttachOutcome::Exists(_)));
            })
        }),
    );
    let records = of(&records, &tree.state);
    let added = one(&records, "add", |effect| {
        matches!(effect, Effect::BuildAdd { name, name_hash, kind: Kind::File, .. }
            if name == "x" && *name_hash == hash_string("x"))
    });
    assert_eq!(added.section, Section::BuildAdd);
    let Effect::BuildAdd { parent: root, .. } = added.effect else {
        unreachable!()
    };
    one(&records, "retry", |effect| {
        *effect == Effect::Arm(Arm::BuildPublishRetry)
    });
    let abandon = one(
        &records,
        "abandon",
        |effect| matches!(effect, Effect::Abandon { root: abandoned } if *abandoned == root),
    );
    assert_eq!(abandon.section, Section::Abandon);
    let frees: Vec<_> = records
        .iter()
        .filter(|record| {
            matches!(record.effect, Effect::Free { .. })
                && (record.seq, record.sub) > (abandon.seq, abandon.sub)
        })
        .collect();
    assert_eq!(frees.len(), 2, "the copy's root and child are freed");
    assert!(
        frees
            .iter()
            .all(|record| record.section == Section::Release),
        "each handed back as an unpublished slot: {frees:#?}"
    );
}

/// A walk's discards applied once the walks drained: the discard, then the propagation's clear
/// of the parent left with no dirty child.
#[test]
fn applied_discards_log_the_propagations_clear() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/x", "d/y"]).await;
                tree.state
                    .node_mark(
                        &tree.repository,
                        tree.id("d/x"),
                        Mark::dirty(NodeFlags::DirtyAdd, Ext::Keep, false),
                    )
                    .await
                    .expect("'d/x' is marked");
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let discards = WalkDiscards::default();
                discards.queue(
                    tree.state.clone(),
                    tree.repository.clone(),
                    vec![tree.id("d/x")],
                );
                discards.apply().await.expect("the discards apply");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let d = tree.id("d");
    let cleared = one(
        &records,
        "clear",
        |effect| matches!(effect, Effect::Word { node, record, .. } if *node == d && !record.0.is_dirty()),
    );
    assert_eq!(cleared.section, Section::ClearPropagation);
}

/// A settle that finds a child dirty drops its pending clear and leaves the directory as it was:
/// the last phase's word changes the pending clear alone, the directory never handed out.
#[test]
fn a_settle_kept_by_a_dirty_child_logs_its_pending_clear_dropped() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                tree.state.mark_served();
                tree.state
                    .served()
                    .node_mark(&tree.repository, tree.id("d/f"))
                    .await
                    .expect("'d/f' is marked");
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let cleared = crate::harness::catalogue::ops::served_clear(&tree, tree.id("d"))
                    .await
                    .expect("the clear runs");
                assert_eq!(cleared, ClearOutcome::Kept);
            })
        }),
    );
    let records = of(&records, &tree.state);
    let d = tree.id("d");
    let words = word_records(&records, d);
    let phases: Vec<_> = words
        .iter()
        .map(|record| match record.effect {
            Effect::Word { clear_pending, .. } => (record.section, clear_pending),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        phases,
        [
            (Section::SettlePhase1, true),
            (Section::SettlePhase3, false)
        ],
        "phase 1 pends the clear, phase 3 drops it: {words:#?}"
    );
    let records_of_d = words_of(&records, d);
    assert_eq!(
        log::Words(records_of_d[0]),
        log::Words(records_of_d[1]),
        "the directory's words as phase 1 left them"
    );
    assert!(records_of_d[1].is_dirty());
}

/// A settle cancelled while its clear is pending drops the pending clear as it unwinds: some run
/// logs phase 3's word, the pending clear gone and the directory still dirty.
#[test]
fn a_settle_cancelled_while_pending_logs_its_pending_clear_dropped() {
    let runs = logged_runs(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                tree.state.mark_served();
                tree.state
                    .served()
                    .node_mark(&tree.repository, tree.id("d"))
                    .await
                    .expect("'d' is marked");
                tree
            })
        },
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let _ = crate::harness::catalogue::ops::served_clear(&tree, tree.id("d")).await;
            })
        })],
        vec![0],
        Labels::Required,
        Exploration {
            cancellation: true,
            ..Exploration::default()
        },
    );
    let dropped = runs.iter().any(|(log, tree)| {
        let records = of(log, &tree.state);
        let d = tree.id("d");
        word_records(&records, d).last().is_some_and(|record| {
            matches!(record.effect, Effect::Word { record: words, clear_pending: false, .. }
                if record.section == Section::SettlePhase3 && words.0.is_dirty())
        })
    });
    assert!(dropped, "no run of {} dropped a pending clear", runs.len());
}

/// An unserved clear of a file clears it, then the directory left with no dirty child, each in
/// its own section.
#[test]
fn an_unserved_clear_logs_its_clears_up_the_tree() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                tree.state
                    .node_mark(
                        &tree.repository,
                        tree.id("d/f"),
                        Mark::dirty(NodeFlags::DirtyModify, Ext::Keep, false),
                    )
                    .await
                    .expect("'d/f' is marked");
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                tree.state
                    .node_clear(&tree.repository, tree.id("d/f"), Clear::Dirty)
                    .await
                    .expect("the clear runs");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (d, f) = (tree.id("d"), tree.id("d/f"));
    let cleared = |node: NodeID| {
        one(
            &records,
            "clear",
            |effect| matches!(effect, Effect::Word { node: of, record, .. } if *of == node && !record.0.is_dirty()),
        )
    };
    let (file_cleared, directory_cleared) = (cleared(f), cleared(d));
    assert_eq!(file_cleared.section, Section::UnservedClear);
    assert_eq!(directory_cleared.section, Section::UnservedClear);
    assert!(file_cleared.seq < directory_cleared.seq, "the file first");
}

/// A commit's discard: the walk collects, then the root is unlinked and the nodes below freed a
/// block at a time, in the frees' sections.
#[test]
fn a_walked_discard_logs_its_frees_in_their_sections() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/e/x", "d/y"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let collected: Arc<Mutex<Vec<NodeID>>> = Arc::default();
                let into = collected.clone();
                lore_revision::state::node_discard_walk(
                    tree.state.clone(),
                    tree.repository.clone(),
                    tree.id("d"),
                    move |node, _| into.lock().unwrap().push(node),
                )
                .await
                .expect("the walk runs");
                let collected = std::mem::take(&mut *collected.lock().unwrap());
                lore_revision::state::node_discard_free(
                    &tree.state,
                    &tree.repository,
                    &[tree.id("d")],
                    collected,
                    |_, _| {},
                )
                .await
                .expect("the frees run");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let d = tree.id("d");
    let unlink = one(
        &records,
        "unlink",
        |effect| matches!(effect, Effect::Detach { node, how: How::Discard, .. } if *node == d),
    );
    assert_eq!(unlink.section, Section::DiscardMarks);
    for path in ["d/e", "d/e/x", "d/y"] {
        let node = tree.id(path);
        let free = one(
            &records,
            "free",
            |effect| matches!(effect, Effect::Free { node: of, .. } if *of == node),
        );
        assert_eq!(free.section, Section::DiscardFree, "'{path}' freed");
    }
}

/// A commit's update of the tree's root hash moves the root's hash and size out of block 0, under
/// the save's label.
#[test]
fn a_root_hash_update_logs_its_root_reset() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/"]).await;
                let block = invariants::block_of(&tree.state, &tree.repository, ROOT_NODE).await;
                tree.state.node_edit_for_tests(&block, ROOT_NODE, |node| {
                    node.address.hash = Hash::from_u64(9);
                    node.size = 3;
                });
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                tree.state
                    .update_tree_root_hash(tree.repository.clone())
                    .await
                    .expect("the root hash updates");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let reset = word_records(&records, ROOT_NODE);
    let last = reset.last().expect("the root's word");
    assert_eq!(last.section, Section::Save);
    let Effect::Word { record: words, .. } = &last.effect else {
        unreachable!()
    };
    assert!(words.0.address.hash.is_zero() && words.0.size == 0);
}

/// A staged modify of a file below the root climbs nothing, so arms no guard.
#[test]
fn a_staged_modify_below_the_root_logs_no_guard() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["f"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                tree.state
                    .node_modify_staged(&tree.repository, tree.id("f"), &Content::default())
                    .await
                    .expect("'f' is modified");
            })
        }),
    );
    let records = of(&records, &tree.state);
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.effect, Effect::Guard { .. })),
        "no guard armed: {records:#?}"
    );
    one(
        &records,
        "edit",
        |effect| matches!(effect, Effect::Word { node, .. } if *node == tree.id("f")),
    );
}

/// The `Admitted` of `node` on `expected` in `labelled`, one, and after it in its section a `Word`
/// of `node` where `writes`, else its `Move` or `Rename`.
fn admitted_in(
    records: &[&Record],
    (node, expected): (NodeID, Expected),
    labelled: Section,
    writes: bool,
) {
    let found: Vec<&&Record> = records
        .iter()
        .filter(|record| {
            record.section == labelled
                && matches!(record.effect, Effect::Admitted { node: of, expected: as_read }
                    if of == node && as_read == expected)
        })
        .collect();
    assert_eq!(
        found.len(),
        1,
        "one admission of {node} in {labelled:?}: {records:#?}"
    );
    let admitted = found[0];
    let after = section_of(records, admitted).iter().any(|record| {
        record.sub > admitted.sub
            && match record.effect {
                Effect::Word { node: of, .. } => writes && of == node,
                Effect::Move { node: of, .. } | Effect::Rename { node: of, .. } => {
                    !writes && of == node
                }
                _ => false,
            }
    });
    assert!(
        after,
        "the change of {node} after its admission in {labelled:?}"
    );
}

/// Each change decided on what no other record of it says logs its `Admitted` in its section,
/// with what it read, before its `Word`: an in-place change its place, an `IfClean` rewrite the
/// node clean, a delete level's mark its place, a staged move and a staged rename their
/// parents.
#[test]
fn the_unserved_admissions_log_what_they_were_decided_on() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f", "d/g", "d/h", "e/"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let (d, e) = (tree.id("d"), tree.id("e"));
                let (f, g, h) = (tree.id("d/f"), tree.id("d/g"), tree.id("d/h"));
                let repository = &tree.repository;
                let block = invariants::block_of(&tree.state, repository, f).await;
                let changed =
                    tree.state
                        .node_change_in_place(&block, f, (d, hash_string("f")), |held| {
                            held.set_size(9);
                            true
                        });
                assert!(changed, "'d/f' changes in place");
                let content = Content {
                    size: 5,
                    ..Content::default()
                };
                let rewritten = tree
                    .state
                    .node_rewrite(repository, g, &content, Rewrite::IfClean)
                    .await;
                assert!(rewritten.expect("the rewrite runs"), "'d/g' is clean");
                let marked = tree.state.node_delete_level(repository, h, d).await;
                assert_eq!(marked.expect("the level runs"), LevelMark::Marked(None));
                let permit = tree.state.moves_permit().await.expect("the permit");
                for (node, to, name) in [(f, e, "f"), (g, d, "k")] {
                    tree.state
                        .node_move_staged(
                            repository.clone(),
                            node,
                            to,
                            (name, hash_string(name)),
                            &permit,
                            false,
                        )
                        .await
                        .expect("the staged move runs");
                }
                drop(permit);
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (d, e) = (tree.id("d"), tree.id("e"));
    let at = |parent: NodeID, name: &str| Expected::At {
        parent,
        name_hash: hash_string(name),
    };
    admitted_in(
        &records,
        (tree.id("d/f"), at(d, "f")),
        Section::InPlace,
        true,
    );
    admitted_in(
        &records,
        (tree.id("d/g"), Expected::Clean),
        Section::Rewrite,
        true,
    );
    admitted_in(
        &records,
        (tree.id("d/h"), Expected::Deletable(d)),
        Section::DeleteLevel,
        true,
    );
    admitted_in(
        &records,
        (tree.id("d/f"), Expected::Movable { from: d, to: e }),
        Section::MoveNamed,
        false,
    );
    admitted_in(
        &records,
        (tree.id("d/g"), Expected::Movable { from: d, to: d }),
        Section::RenameInPlace,
        false,
    );
}

/// The served admissions: a served rewrite its parent, a local-only reattach the name it moves
/// to, a settle's phases the directory's place.
#[test]
fn the_served_admissions_log_what_they_were_decided_on() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f", "e/", "s/t"]).await;
                tree.state.mark_served();
                tree.state
                    .served()
                    .node_mark(&tree.repository, tree.id("s"))
                    .await
                    .expect("'s' is marked");
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let cleared = crate::harness::catalogue::ops::served_clear(&tree, tree.id("s"))
                    .await
                    .expect("the clear runs");
                assert_eq!(cleared, ClearOutcome::Cleared);
                let (d, e, f) = (tree.id("d"), tree.id("e"), tree.id("d/f"));
                assert!(
                    crate::harness::catalogue::ops::served_copy_up(&tree, f, d).await,
                    "'d/f' is copied up"
                );
                assert!(
                    crate::harness::catalogue::ops::served_rename(&tree, f, e, "g").await,
                    "'d/f' moves"
                );
            })
        }),
    );
    let records = of(&records, &tree.state);
    let (d, e, f, s) = (tree.id("d"), tree.id("e"), tree.id("d/f"), tree.id("s"));
    let place = Expected::At {
        parent: ROOT_NODE,
        name_hash: hash_string("s"),
    };
    admitted_in(&records, (s, place), Section::SettlePhase1, true);
    admitted_in(&records, (s, place), Section::SettlePhase3, true);
    admitted_in(&records, (f, Expected::Below(d)), Section::Rewrite, true);
    admitted_in(
        &records,
        (
            f,
            Expected::Local {
                to: e,
                name_hash: hash_string("g"),
            },
        ),
        Section::Reattach,
        false,
    );
}

/// A checked attach below a file publishes nothing: its read of the parent refuses it before any
/// lock, through the `check_live_directory` its check under the locks shares, which a lookup's
/// count alone would not tell from a directory's. Its outcome is read after the model's replay,
/// which judges a publish below a file first.
#[test]
fn an_attach_below_a_file_publishes_nothing() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let attached = tree
                    .state
                    .node_attach(tree.repository.clone(), tree.id("d/f"), file("n"), "n")
                    .await;
                assert!(attached.is_err(), "an attach below a file is refused");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let f = tree.id("d/f");
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.effect, Effect::Publish { parent, .. } if parent == f)),
        "nothing published below the file"
    );
}

/// A served mark of a directory between its settle's phases drops the pending clear the first
/// phase set, so the last phase keeps the directory dirty: the clear's decision on its pending
/// clear, met on a run the model replays.
#[test]
fn a_settle_overtaken_by_a_mark_between_its_phases_keeps_its_directory() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["a/d/f"]).await;
                tree.state.mark_served();
                for dir in ["a", "a/d"] {
                    assert!(
                        crate::harness::catalogue::ops::served_mark_dir(&tree, tree.id(dir)).await
                    );
                }
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let d = tree.id("a/d");
                let between = async {
                    assert!(
                        crate::harness::catalogue::ops::served_mark_dir(&tree, d).await,
                        "'a/d' is marked"
                    );
                };
                let settled =
                    crate::harness::catalogue::ops::settle_waiting(&tree, d, between).await;
                assert_eq!(settled, Settle::KeptAtFinish);
            })
        }),
    );
    let records = of(&records, &tree.state);
    let d = tree.id("a/d");
    one(&records, "kept", |effect| {
        *effect == Effect::Arm(Arm::ClearKept)
    });
    assert!(
        !records.iter().any(|record| {
            record.section == Section::SettlePhase3
                && matches!(&record.effect, Effect::Word { node, record: words, .. }
                    if *node == d && !words.0.is_dirty())
        }),
        "the directory kept dirty"
    );
}

/// An unserved mark climbing into a detached directory: its staged half marks the directory, its
/// dirty half stops there, writing no `Dirty` into a node no longer in the tree.
#[test]
fn an_unserved_marks_dirty_half_stops_at_a_detached_directory() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                tree.state
                    .node_detach(tree.repository.clone(), tree.id("d"), Detach::Always)
                    .await
                    .expect("'d' detaches");
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                // The outcome read from the records, after the model's replay judged it.
                let _ = tree
                    .state
                    .node_mark(
                        &tree.repository,
                        tree.id("d/f"),
                        Mark::staged_and_dirty(NodeFlags::StagedModify, NodeFlags::DirtyModify),
                    )
                    .await;
            })
        }),
    );
    let records = of(&records, &tree.state);
    let d = tree.id("d");
    let marked = word_records(&records, d);
    assert!(
        !marked.is_empty()
            && marked.iter().all(|record| {
                record.section == Section::UnservedMark
                    && matches!(&record.effect, Effect::Word { record: words, .. }
                        if words.0.is_staged() && !words.0.is_dirty())
            }),
        "the staged half alone reached the directory: {marked:#?}"
    );
}

/// A reader's walk records its read of its directory's head and count, with its block's instance,
/// at a `seq` of its own that no other record shares: the children iterator begun from its
/// parent's record and from the directory alone, and the listing, each once, after an add at the
/// head moved the count; a listing of a file, which walks nothing, records none.
#[test]
fn a_readers_walk_records_its_read_of_the_directorys_head() {
    let read: Arc<Mutex<Option<(u32, u32, u64)>>> = Arc::default();
    let into = read.clone();
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f"])),
        Box::new(move |tree: Arc<Tree>| {
            let into = into.clone();
            Box::pin(async move {
                let (state, repository, d) =
                    (tree.state.clone(), tree.repository.clone(), tree.id("d"));
                let block = state
                    .block(repository.clone(), NodeBlock::index(d))
                    .await
                    .expect("the directory's block");
                let before = block.chain_changes(Node::index(d));
                state
                    .node_add(repository.clone(), d, file("g"), "g")
                    .await
                    .expect("'d/g' adds");
                let _ =
                    StateNodeChildrenIterator::with_parent(state.clone(), repository.clone(), d)
                        .await
                        .expect("a walk from the parent's record");
                let _ = StateNodeChildrenIterator::new(state.clone(), repository.clone(), d)
                    .await
                    .expect("a walk");
                let _ = state
                    .node_children(repository.clone(), d)
                    .await
                    .expect("a listing");
                let _ = state
                    .node_children(repository, tree.id("d/f"))
                    .await
                    .expect("a listing of a file");
                let after = block.chain_changes(Node::index(d));
                *into.lock().unwrap() = Some((before, after, block.hook_instance()));
            })
        }),
    );
    let d = tree.id("d");
    let (before, changes, instance) = read.lock().unwrap().expect("the operation ran");
    assert_eq!(changes, before + 1, "the add counted");
    let records = of(&records, &tree.state);
    let heads: Vec<&Record> = records
        .iter()
        .copied()
        .filter(|record| matches!(record.effect, Effect::Head { .. }))
        .collect();
    assert_eq!(heads.len(), 3, "{heads:#?}");
    for head in &heads {
        assert_eq!(
            head.effect,
            Effect::Head {
                node: d,
                changes,
                instance
            }
        );
        assert_eq!(
            section_of(&records, head).len(),
            1,
            "a seq of its own: {head:?}"
        );
    }
}

/// A scheduler recording on its thread nothing but how many block guards the thread holds as
/// each reader's head read is recorded.
#[derive(Default)]
struct GuardsAtHead {
    held: Mutex<Vec<usize>>,
}

impl hooks::Scheduler for GuardsAtHead {
    fn reached(&self, _point: hooks::Point) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        None
    }

    fn touched(&self, _resource: hooks::Resource, _write: bool) {}

    fn recording(&self) -> bool {
        true
    }

    fn effect(&self, record: Record) {
        if matches!(record.effect, Effect::Head { .. }) {
            self.held.lock().unwrap().push(hooks::guards_held());
        }
    }
}

/// A reader's head read is recorded under its block's read lock, the one guard its thread holds
/// there, so no section counted on the chain falls between the read and its `seq`: the children
/// iterators' and the listing's.
#[test]
fn a_readers_head_read_is_recorded_under_its_blocks_read_lock() {
    let tree = Arc::new(current_thread().block_on(in_context(Tree::new(&["d/f"]))));
    let scheduler = Arc::new(GuardsAtHead::default());
    let installed = hooks::install(scheduler.clone());
    current_thread().block_on(in_context(hooks::as_task(0, async {
        let (state, repository, d) = (tree.state.clone(), tree.repository.clone(), tree.id("d"));
        let _ = StateNodeChildrenIterator::with_parent(state.clone(), repository.clone(), d)
            .await
            .expect("a walk from the parent's record");
        let _ = StateNodeChildrenIterator::new(state.clone(), repository.clone(), d)
            .await
            .expect("a walk");
        let _ = state.node_children(repository, d).await.expect("a listing");
    })));
    drop(installed);
    assert_eq!(*scheduler.held.lock().unwrap(), [1, 1, 1]);
}
