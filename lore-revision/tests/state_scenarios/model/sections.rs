// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Real runs of the sections whose writes the log tests leave out, each replayed through the model
//! (`log::check_model`): its run holds the section's set at least as wide as the section writes,
//! and its every `Word`, altered field by field, to the model's judgement. Each run writes the
//! field its test names, so the set is met where the code writes it.

use std::sync::Arc;

use lore_base::types::Address;
use lore_revision::node::Node;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::Clear;
use lore_revision::state::Content;
use lore_revision::state::Mark;
use lore_revision::state::NameChange;
use lore_revision::state::OwnFlags;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_storage::hash::hash_string;

use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::log::logged;
use crate::log::of;

/// The records each `Word` of `node` in a section labelled `section` holds, in order.
fn written(records: &[&Record], section: Section, node: NodeID) -> Vec<Node> {
    written_with_bits(records, section, node)
        .into_iter()
        .map(|(record, _)| record)
        .collect()
}

/// [`written`], each with its pending clear bit.
fn written_with_bits(records: &[&Record], section: Section, node: NodeID) -> Vec<(Node, bool)> {
    records
        .iter()
        .filter(|record| record.section == section)
        .filter_map(|record| match &record.effect {
            Effect::Word {
                node: of,
                record: words,
                clear_pending,
            } if *of == node => Some((words.0, *clear_pending)),
            _ => None,
        })
        .collect()
}

/// Sets `flags` on `path`'s node, as a setup does before the run records.
async fn flagged(tree: &Tree, path: &str, flags: NodeFlags) {
    let block = invariants::block_of(&tree.state, &tree.repository, tree.id(path)).await;
    let flags = flags.bits();
    tree.state
        .node_edit_for_tests(&block, tree.id(path), |node| node.flags |= flags);
}

#[test]
fn a_restore_writes_its_content_and_drops_its_staged_flags() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let (d, f) = (tree.id("d"), tree.id("d/f"));
                tree.state
                    .node_delete_level(&tree.repository, f, d)
                    .await
                    .expect("'d/f' is staged for deletion");
                let content = Content {
                    mode: 1,
                    size: 3,
                    address: Address::default(),
                };
                tree.state
                    .node_restore(&tree.repository, f, &content)
                    .await
                    .expect("'d/f' is restored");
            })
        }),
    );
    let restored = written(&of(&records, &tree.state), Section::Restore, tree.id("d/f"));
    assert!(
        restored
            .last()
            .is_some_and(|node| node.size == 3 && !node.is_staged_delete()),
        "{restored:?}"
    );
}

#[test]
fn an_in_place_change_writes_content_and_flags() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                ops::mark_dirty(&tree, tree.id("d/f")).await;
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let (d, f) = (tree.id("d"), tree.id("d/f"));
                let block = invariants::block_of(&tree.state, &tree.repository, f).await;
                assert!(tree.state.node_change_in_place(
                    &block,
                    f,
                    (d, hash_string("f")),
                    |node| {
                        node.set_content(Address::default(), 9, 1);
                        node.clear_dirty_flags()
                    }
                ));
            })
        }),
    );
    let changed = written(&of(&records, &tree.state), Section::InPlace, tree.id("d/f"));
    assert!(
        changed
            .last()
            .is_some_and(|node| node.size == 9 && !node.is_dirty()),
        "{changed:?}"
    );
}

#[test]
fn an_owned_change_and_clear_write_their_fields() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                ops::mark_dirty(&tree, tree.id("d/f")).await;
                let f = tree.id("d/f");
                let block = invariants::block_of(&tree.state, &tree.repository, f).await;
                tree.state.node_edit_for_tests(&block, f, |node| {
                    node.ext_flags = NodeExtFlags::LocalContent.bits();
                });
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let f = tree.id("d/f");
                let block = invariants::block_of(&tree.state, &tree.repository, f).await;
                assert!(tree.state.node_change_owned(&block, f, |node| {
                    node.set_size(5);
                    true
                }));
                assert!(tree.state.node_clear_own(&block, f, OwnFlags::Dirty));
            })
        }),
    );
    let owned = written(&of(&records, &tree.state), Section::Owned, tree.id("d/f"));
    assert_eq!(owned.len(), 2, "{owned:?}");
    assert!(owned[0].size == 5 && owned[1].ext_flags == 0 && !owned[1].is_dirty());
}

#[test]
fn a_rename_in_place_writes_its_own_mark() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let own = Mark::staged_and_dirty(NodeFlags::StagedMove, NodeFlags::DirtyMove);
                tree.state
                    .node_move(
                        tree.repository.clone(),
                        tree.id("d/f"),
                        tree.id("d"),
                        NameChange::Rename("h"),
                        own,
                    )
                    .await
                    .expect("'d/f' renames in place");
                // The destination's climb, as `lore dirty` makes it after the move.
                tree.state
                    .node_mark_destination(&tree.repository, tree.id("d"), own)
                    .await
                    .expect("'d' is marked");
            })
        }),
    );
    let records = of(&records, &tree.state);
    assert!(
        records
            .iter()
            .any(|record| matches!(record.effect, Effect::Rename { .. })),
        "the rename is in place"
    );
    let marked = written(&records, Section::RenameInPlace, tree.id("d/f"));
    assert!(
        marked.last().is_some_and(Node::is_staged_move),
        "{marked:?}"
    );
}

/// A respell keeps the name's hash, and counts no change: the compare holds the count.
#[test]
fn a_respell_counts_nothing() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                tree.state
                    .node_rename(
                        tree.repository.clone(),
                        tree.id("d/f"),
                        NameChange::Rename("F"),
                    )
                    .await
                    .expect("'d/f' respells");
            })
        }),
    );
    assert!(
        of(&records, &tree.state).iter().any(|record| matches!(
            record.effect,
            Effect::Rename { name_hash, .. } if name_hash == hash_string("f")
        )),
        "the rename keeps the hash"
    );
}

/// Merge bits written where a section may write them: a mark setting merge state, an unserved
/// clear of the staged flags dropping them, an in-place resolve.
#[test]
fn merge_bits_change_where_their_sections_write_them() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f", "d/g", "d/h"]).await;
                flagged(&tree, "d/f", NodeFlags::StagedMerge).await;
                flagged(&tree, "d/g", NodeFlags::StagedMergeConflict).await;
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let (d, g) = (tree.id("d"), tree.id("d/g"));
                tree.state
                    .node_clear(&tree.repository, tree.id("d/f"), Clear::Staged)
                    .await
                    .expect("the clear runs");
                let block = invariants::block_of(&tree.state, &tree.repository, g).await;
                assert!(tree.state.node_change_in_place(
                    &block,
                    g,
                    (d, hash_string("g")),
                    |node| { node.set_merge_resolved(true) }
                ));
                tree.state
                    .node_mark(
                        &tree.repository,
                        tree.id("d/h"),
                        Mark::staged(NodeFlags::StagedMergeTheirs, true),
                    )
                    .await
                    .expect("the mark runs");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let merge = NodeFlags::MergeBits.bits();
    let cleared = written(&records, Section::UnservedClear, tree.id("d/f"));
    assert!(cleared.last().is_some_and(|node| node.flags & merge == 0));
    let resolved = written(&records, Section::InPlace, tree.id("d/g"));
    assert!(resolved.last().is_some_and(Node::is_staged_merge_resolved));
    let marked = written(&records, Section::UnservedMark, tree.id("d/h"));
    assert!(marked.first().is_some_and(Node::is_staged_merge_theirs));
}

/// A settle of a directory carrying an action drops it in its phase 1.
#[test]
fn a_settle_drops_its_directorys_action() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                tree.state.mark_served();
                assert!(ops::served_mark_dir(&tree, tree.id("d")).await);
                flagged(&tree, "d", NodeFlags::DirtyModify).await;
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                ops::settle(&tree, tree.id("d")).await;
            })
        }),
    );
    let settled = written(
        &of(&records, &tree.state),
        Section::SettlePhase1,
        tree.id("d"),
    );
    assert!(
        settled.first().is_some_and(|node| node.action_bits() == 0),
        "{settled:?}"
    );
}

/// A served rename of an added file over a file the revision holds takes the path over: its add
/// becomes a modify in the reattach's section (H20).
#[test]
fn a_served_rename_over_a_revisions_file_turns_its_add_to_a_modify() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/g"]).await;
                tree.state.mark_served();
                assert!(ops::served_mark_dir(&tree, tree.id("d")).await);
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let (d, g) = (tree.id("d"), tree.id("d/g"));
                let created = ops::served_create(&tree, d, "n")
                    .await
                    .expect("'d/n' is created");
                assert!(ops::served_rename_replacing(&tree, created, (d, "g"), g).await);
            })
        }),
    );
    let records = of(&records, &tree.state);
    let moved = records
        .iter()
        .find_map(|record| match record.effect {
            Effect::Move {
                node,
                replaced: Some(_),
                ..
            } => Some(node),
            _ => None,
        })
        .expect("the replacing move");
    let renamed = written(&records, Section::Reattach, moved);
    assert!(
        renamed.last().is_some_and(Node::is_dirty_modify),
        "{renamed:?}"
    );
}

/// A restore of a file staged for deletion in a merge drops its merge bits with its staged flags.
#[test]
fn a_restore_drops_merge_bits() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                flagged(
                    &tree,
                    "d/f",
                    NodeFlags::StagedDelete | NodeFlags::StagedMerge,
                )
                .await;
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                tree.state
                    .node_restore(&tree.repository, tree.id("d/f"), &Content::default())
                    .await
                    .expect("'d/f' is restored");
            })
        }),
    );
    let restored = written(&of(&records, &tree.state), Section::Restore, tree.id("d/f"));
    assert!(
        restored
            .last()
            .is_some_and(|node| node.flags & NodeFlags::MergeBits.bits() == 0),
        "{restored:?}"
    );
}

/// A restore of a link takes its address: the one write of a link's pin outside a repin.
#[test]
fn a_restore_of_a_link_writes_its_address() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let mut tree = Tree::new(&["d/f"]).await;
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
                flagged(&tree, "d/l", NodeFlags::StagedDelete).await;
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let content = Content {
                    address: Address {
                        hash: lore_base::types::Hash::from_u64(9),
                        ..Address::default()
                    },
                    ..Content::default()
                };
                tree.state
                    .node_restore(&tree.repository, tree.id("d/l"), &content)
                    .await
                    .expect("'d/l' is restored");
            })
        }),
    );
    let restored = written(&of(&records, &tree.state), Section::Restore, tree.id("d/l"));
    assert!(
        restored.last().is_some_and(
            |node| node.child == 40 && node.address.hash == lore_base::types::Hash::from_u64(9)
        ),
        "{restored:?}"
    );
}

/// A served rename of a directory a settle has pending, in the settle's window, drops the
/// pending clear in the reattach's section.
#[test]
fn a_served_reattach_drops_its_directorys_pending_clear() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                tree.state.mark_served();
                assert!(ops::served_mark_dir(&tree, tree.id("d")).await);
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let d = tree.id("d");
                let renamed = async {
                    let moved = ops::served_reattach_any(&tree, d, ROOT_NODE, "e").await;
                    assert_eq!(moved, Some(true));
                };
                ops::settle_waiting(&tree, d, renamed).await;
            })
        }),
    );
    let moved = written_with_bits(&of(&records, &tree.state), Section::Reattach, tree.id("d"));
    assert!(
        moved.last().is_some_and(|(_, pending)| !pending),
        "{moved:?}"
    );
}
