// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Tests for the served and swap tiers of the node operations: the served mark, the served
//! chain rule on attach, detach, reattach and rewrite, and the swap's unmarked changes.

use std::sync::Arc;

use lore_base::types::Address;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::Absent;
use lore_revision::state::AttachCheck;
use lore_revision::state::AttachOutcome;
use lore_revision::state::ClearAgainst;
use lore_revision::state::ClearOutcome;
use lore_revision::state::ClearScratch;
use lore_revision::state::Detach;
use lore_revision::state::Ext;
use lore_revision::state::IfUnchanged;
use lore_revision::state::Mark;
use lore_revision::state::ReattachCheck;
use lore_revision::state::Reattached;
use lore_revision::state::ServedClear;
use lore_revision::state::State;
use lore_revision::state::StateError;
use lore_revision::state::TreeAttach;
use lore_storage::hash::hash_string;

use super::InMemoryMarker;
use super::add;
use super::directory;
use super::file;
use super::record;
use super::reload;
use super::with_repository;
use super::with_state;

/// A file as a served create attaches it: added, holding local content.
fn local_file(name: &str) -> Node {
    Node {
        flags: (NodeFlags::File | NodeFlags::DirtyAdd).bits(),
        ext_flags: NodeExtFlags::LocalContent.bits(),
        name_hash: hash_string(name),
        ..Default::default()
    }
}

/// A chain of clean directories `names` below `parent`, outermost first.
async fn chain(
    state: &State,
    repository: &Arc<RepositoryContext>,
    parent: NodeID,
    names: &[&str],
) -> Vec<NodeID> {
    let mut ids = Vec::with_capacity(names.len());
    let mut at = parent;
    for name in names {
        at = add(state, repository, at, directory(name), name).await;
        ids.push(at);
    }
    ids
}

async fn is_dirty(state: &State, repository: &Arc<RepositoryContext>, node: NodeID) -> bool {
    record(state, repository, node).await.is_dirty()
}

async fn clear_pending(state: &State, repository: &Arc<RepositoryContext>, node: NodeID) -> bool {
    state
        .block(repository.clone(), NodeBlock::index(node))
        .await
        .expect("the node's block")
        .clear_pending(Node::index(node))
}

/// Sets a settle's pending clear on directory `node`, under its block's write lock, as the
/// settle's first phase does.
async fn set_clear_pending(state: &State, repository: &Arc<RepositoryContext>, node: NodeID) {
    let block = state
        .block(repository.clone(), NodeBlock::index(node))
        .await
        .expect("the node's block");
    let writer = block.write_for_tests();
    writer.set_clear_pending(Node::index(node));
}

/// Clears `node`'s dirty flags in place, a file's as a commit's flip does and a directory's as
/// only a fixture does, leaving its ancestors as they are.
async fn clean(state: &State, repository: &Arc<RepositoryContext>, node: NodeID) {
    let block = state
        .block(repository.clone(), NodeBlock::index(node))
        .await
        .expect("the node's block");
    state.node_edit_for_tests(&block, node, Node::clear_dirty_flags);
}

/// Points `node` at `parent`, as only a corrupt State holds it.
async fn corrupt_parent(
    state: &State,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
    parent: NodeID,
) {
    let block = state
        .block(repository.clone(), NodeBlock::index(node))
        .await
        .expect("the node's block");
    block.write_for_tests().node(Node::index(node)).parent = parent;
}

/// A clean tree across two blocks: `top`, in block 0, holds the fillers that fill it, and `x`,
/// in block 1, holds directory `p` and file `f`, both in block 0, moved there.
struct Spread {
    top: NodeID,
    x: NodeID,
    p: NodeID,
    f: NodeID,
}

async fn spread(state: &State, repository: &Arc<RepositoryContext>) -> Spread {
    let top = add(state, repository, ROOT_NODE, directory("top"), "top").await;
    let p = add(state, repository, top, directory("p"), "p").await;
    let f = add(state, repository, top, file("f"), "f").await;
    for index in 0..BLOCK_NODE_COUNT {
        let name = format!("filler{index}");
        add(state, repository, top, file(&name), &name).await;
    }
    let x = add(state, repository, top, directory("x"), "x").await;
    for (node, name) in [(p, "p"), (f, "f")] {
        state
            .node_reattach(repository.clone(), node, x, name)
            .await
            .expect("the move succeeds");
    }
    for node in [top, x, p, f] {
        clean(state, repository, node).await;
    }
    assert_eq!(NodeBlock::index(top), 0);
    assert_eq!(NodeBlock::index(p), 0);
    assert_eq!(NodeBlock::index(f), 0);
    assert_eq!(NodeBlock::index(x), 1);
    Spread { top, x, p, f }
}

/// A served rewrite of file `node`, found below `dir`, to `size` bytes of local content.
async fn rewrite(
    state: &State,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
    dir: NodeID,
    size: u64,
) -> Result<Option<NodeID>, StateError> {
    state
        .served()
        .node_rewrite(
            repository,
            node,
            dir,
            &lore_revision::state::Content {
                mode: 0o644,
                size,
                address: Address::default(),
            },
            (NodeFlags::DirtyModify, NodeExtFlags::LocalContent),
        )
        .await
}

/// Marks `node` dirty with `flags` and `ext` and its ancestors, as a local change does.
async fn mark(
    state: &State,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
    flags: NodeFlags,
    ext: NodeExtFlags,
) {
    state
        .node_mark(repository, node, Mark::dirty(flags, Ext::Set(ext), true))
        .await
        .expect("the mark succeeds");
}

#[tokio::test]
async fn a_served_mark_dirties_the_directory_and_every_clean_ancestor() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b", "c", "d"]).await;

        state
            .served()
            .node_mark(&repository, dirs[3])
            .await
            .expect("the mark succeeds");

        for &dir in &dirs {
            let node = record(&state, &repository, dir).await;
            assert!(node.is_dirty(), "directory {dir} is dirty");
            assert_eq!(
                node.flags & NodeFlags::DirtyBits.bits(),
                NodeFlags::Dirty.bits(),
                "directory {dir} takes the base dirty bit alone"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn a_served_mark_keeps_the_flags_of_a_directory_already_dirty() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b", "c"]).await;
        mark(
            &state,
            &repository,
            dirs[0],
            NodeFlags::DirtyAdd,
            NodeExtFlags::LocalContent,
        )
        .await;

        state
            .served()
            .node_mark(&repository, dirs[2])
            .await
            .expect("the mark succeeds");

        let a = record(&state, &repository, dirs[0]).await;
        assert!(a.is_dirty_add(), "the dirty ancestor keeps its action");
        assert_eq!(a.ext_flags(), NodeExtFlags::LocalContent);
        assert!(is_dirty(&state, &repository, dirs[1]).await);
        assert!(is_dirty(&state, &repository, dirs[2]).await);

        // The target already dirty takes the fast path and keeps its flags too.
        state
            .served()
            .node_mark(&repository, dirs[0])
            .await
            .expect("the mark succeeds");
        let a = record(&state, &repository, dirs[0]).await;
        assert!(a.is_dirty_add());
        assert_eq!(a.ext_flags(), NodeExtFlags::LocalContent);
    })
    .await;
}

#[tokio::test]
async fn a_served_mark_drops_the_pending_clear_of_every_level_it_marks() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b", "c"]).await;
        mark(
            &state,
            &repository,
            dirs[1],
            NodeFlags::Dirty,
            NodeExtFlags::NoFlags,
        )
        .await;
        set_clear_pending(&state, &repository, dirs[0]).await;
        set_clear_pending(&state, &repository, dirs[1]).await;

        state
            .served()
            .node_mark(&repository, dirs[2])
            .await
            .expect("the mark succeeds");

        for &dir in &dirs {
            assert!(is_dirty(&state, &repository, dir).await);
            assert!(
                !clear_pending(&state, &repository, dir).await,
                "directory {dir} loses its pending clear"
            );
        }

        // A dirty target with a pending clear is no early out.
        set_clear_pending(&state, &repository, dirs[2]).await;
        state
            .served()
            .node_mark(&repository, dirs[2])
            .await
            .expect("the mark succeeds");
        assert!(!clear_pending(&state, &repository, dirs[2]).await);
    })
    .await;
}

#[tokio::test]
async fn a_served_mark_below_or_of_a_detached_directory_is_not_found() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b", "c"]).await;
        state
            .node_detach(repository.clone(), dirs[0], Detach::Always)
            .await
            .expect("the detach succeeds");

        for &dir in &dirs {
            let marked = state.served().node_mark(&repository, dir).await;
            assert!(
                marked
                    .as_ref()
                    .is_err_and(|error| error.is_node_not_found()),
                "a mark of {dir} answers NodeNotFound, got {marked:?}"
            );
        }
        assert!(
            !is_dirty(&state, &repository, dirs[2]).await,
            "nothing below the detached directory is marked"
        );
    })
    .await;
}

#[tokio::test]
async fn a_served_mark_more_levels_deep_than_one_pass_marks_every_level() {
    with_state(|repository, state| async move {
        let names: Vec<String> = (0..150).map(|level| format!("level{level}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let dirs = chain(&state, &repository, ROOT_NODE, &names).await;

        state
            .served()
            .node_mark(&repository, *dirs.last().expect("a leaf"))
            .await
            .expect("the mark succeeds");

        for &dir in &dirs {
            assert!(is_dirty(&state, &repository, dir).await, "{dir} is dirty");
        }
    })
    .await;
}

#[tokio::test]
async fn a_served_mark_across_blocks_and_of_the_root() {
    with_state(|repository, state| async move {
        let top = add(&state, &repository, ROOT_NODE, directory("top"), "top").await;
        // Fill the first block, so the directories below land in another.
        for index in 0..BLOCK_NODE_COUNT {
            let name = format!("filler{index}");
            add(&state, &repository, top, file(&name), &name).await;
        }
        let dirs = chain(&state, &repository, top, &["x", "y"]).await;
        assert_ne!(NodeBlock::index(top), NodeBlock::index(dirs[1]));

        state
            .served()
            .node_mark(&repository, dirs[1])
            .await
            .expect("the mark succeeds");
        assert!(is_dirty(&state, &repository, top).await);
        assert!(is_dirty(&state, &repository, dirs[0]).await);
        assert!(is_dirty(&state, &repository, dirs[1]).await);

        state
            .served()
            .node_mark(&repository, ROOT_NODE)
            .await
            .expect("a mark of the root succeeds");
    })
    .await;
}

#[tokio::test]
async fn a_served_mark_loads_the_blocks_it_climbs_through() {
    with_repository(|repository| async move {
        let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
        let first = State::new();
        let top = add(&first, &repository, ROOT_NODE, directory("top"), "top").await;
        for index in 0..BLOCK_NODE_COUNT {
            let name = format!("filler{index}");
            add(&first, &repository, top, file(&name), &name).await;
        }
        let dirs = chain(&first, &repository, top, &["x", "y", "z"]).await;
        let signature = first
            .serialize(repository.clone(), &token)
            .await
            .expect("the State serializes");
        let state = State::deserialize(repository.clone(), signature)
            .await
            .expect("the State loads");
        assert!(state.resident_block(NodeBlock::index(dirs[2])).is_none());

        state
            .served()
            .node_mark(&repository, dirs[2])
            .await
            .expect("the mark succeeds");
        assert!(is_dirty(&state, &repository, top).await);
        for &dir in &dirs {
            assert!(is_dirty(&state, &repository, dir).await);
        }
    })
    .await;
}

#[tokio::test]
async fn a_served_attach_dirties_its_directory_and_ancestors_first() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b"]).await;
        mark(
            &state,
            &repository,
            dirs[0],
            NodeFlags::Dirty,
            NodeExtFlags::NoFlags,
        )
        .await;
        set_clear_pending(&state, &repository, dirs[0]).await;

        let attached = state
            .served()
            .node_attach(
                repository.clone(),
                dirs[1],
                local_file("f"),
                "f",
                AttachCheck::Walk,
            )
            .await
            .expect("the attach succeeds");
        let Some(AttachOutcome::Attached(f)) = attached else {
            panic!("the file attaches, got {attached:?}");
        };
        assert_eq!(record(&state, &repository, f).await.parent, dirs[1]);
        assert!(is_dirty(&state, &repository, dirs[0]).await);
        assert!(is_dirty(&state, &repository, dirs[1]).await);
        assert!(!clear_pending(&state, &repository, dirs[0]).await);

        let again = state
            .served()
            .node_attach(
                repository.clone(),
                dirs[1],
                local_file("f"),
                "f",
                AttachCheck::Resident(None),
            )
            .await
            .expect("the attach answers");
        assert_eq!(again, Some(AttachOutcome::Exists(f)));
    })
    .await;
}

#[tokio::test]
async fn a_served_attach_below_a_detached_directory_is_not_found() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b"]).await;
        state
            .node_detach(repository.clone(), dirs[0], Detach::Always)
            .await
            .expect("the detach succeeds");

        let attached = state
            .served()
            .node_attach(
                repository.clone(),
                dirs[1],
                local_file("f"),
                "f",
                AttachCheck::Walk,
            )
            .await;
        assert!(
            attached
                .as_ref()
                .is_err_and(|error| error.is_node_not_found()),
            "got {attached:?}"
        );
        assert!(!is_dirty(&state, &repository, dirs[1]).await);
    })
    .await;
}

#[tokio::test]
async fn a_served_detach_dirties_its_directory_first() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b"]).await;
        let f = add(&state, &repository, dirs[1], file("f"), "f").await;
        set_clear_pending(&state, &repository, dirs[1]).await;

        let detached = state
            .served()
            .node_detach(repository.clone(), f, Detach::Always)
            .await
            .expect("the detach succeeds");
        assert!(detached);
        assert!(record(&state, &repository, f).await.is_discarded());
        assert!(is_dirty(&state, &repository, dirs[0]).await);
        assert!(is_dirty(&state, &repository, dirs[1]).await);
        assert!(!clear_pending(&state, &repository, dirs[1]).await);

        let again = state
            .served()
            .node_detach(repository.clone(), f, Detach::Always)
            .await;
        assert!(again.as_ref().is_err_and(|error| error.is_node_not_found()));
        for node in [ROOT_NODE, INVALID_NODE] {
            let refused = state
                .served()
                .node_detach(repository.clone(), node, Detach::Always)
                .await;
            assert!(
                refused
                    .as_ref()
                    .is_err_and(|error| error.is_invalid_arguments()),
                "node {node} is not detachable, got {refused:?}"
            );
        }
    })
    .await;
}

/// H2 and H11: an RMDIR's detach goes ahead only while the directory's chain holds what its
/// emptiness check read, compared under the directory's lock. One refused, as a change that marks
/// nothing (a swap's) moved the chain, marks nothing either: the directory above stays clean.
#[tokio::test]
async fn a_served_detach_if_unchanged_is_refused_by_a_changed_chain_and_marks_nothing() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["p", "d"]).await;
        let x = add(&state, &repository, dirs[1], file("x"), "x").await;
        let block = state
            .block(repository.clone(), NodeBlock::index(dirs[1]))
            .await
            .expect("d's block");
        let (read, _) = IfUnchanged::read(&block, dirs[1]);
        assert!(
            state
                .swap()
                .node_detach(repository.clone(), x, Detach::Always)
                .await
                .expect("the swap's detach succeeds")
        );
        assert_eq!(read.changes_since(), 1);
        let refused = state
            .served()
            .node_detach(repository.clone(), dirs[1], Detach::IfUnchanged(&read))
            .await
            .expect("the detach answers");
        assert!(
            !refused,
            "a chain changed since its read refuses the detach"
        );
        assert!(!record(&state, &repository, dirs[1]).await.is_discarded());
        assert!(
            !is_dirty(&state, &repository, dirs[0]).await,
            "p stays clean"
        );
        assert!(
            !is_dirty(&state, &repository, dirs[1]).await,
            "d stays clean"
        );

        let (read, _) = IfUnchanged::read(&block, dirs[1]);
        // Two untouched directories in one block count alike: only the directory tells them apart.
        let (q, r) = (
            chain(&state, &repository, ROOT_NODE, &["q"]).await[0],
            chain(&state, &repository, ROOT_NODE, &["r"]).await[0],
        );
        assert_eq!(NodeBlock::index(q), NodeBlock::index(r));
        let siblings = state
            .block(repository.clone(), NodeBlock::index(q))
            .await
            .expect("q's block");
        let (of_q, _) = IfUnchanged::read(&siblings, q);
        assert_eq!(
            siblings.chain_changes(Node::index(q)),
            siblings.chain_changes(Node::index(r))
        );
        let elsewhere = state
            .served()
            .node_detach(repository.clone(), r, Detach::IfUnchanged(&of_q))
            .await
            .expect("the detach answers");
        assert!(
            !elsewhere,
            "a read of another directory's chain refuses the detach"
        );
        let detached = state
            .served()
            .node_detach(repository.clone(), dirs[1], Detach::IfUnchanged(&read))
            .await
            .expect("the detach succeeds");
        assert!(detached, "an unchanged chain lets the detach go ahead");
        assert!(record(&state, &repository, dirs[1]).await.is_discarded());
        assert!(
            is_dirty(&state, &repository, dirs[0]).await,
            "the detach marks p first"
        );
    })
    .await;
}

/// A chain count read from a block the State has loaded since counts from where that block began,
/// so a detach given it is refused even where the counts agree.
#[tokio::test]
async fn a_detach_if_unchanged_read_from_another_load_of_the_block_is_refused() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["d"]).await;
        let first = reload(&state, &repository).await;
        let second = reload(&first, &repository).await;
        let block = first
            .block(repository.clone(), NodeBlock::index(dirs[0]))
            .await
            .expect("d's block in the first load");
        let (read, _) = IfUnchanged::read(&block, dirs[0]);
        let held = second
            .block(repository.clone(), NodeBlock::index(dirs[0]))
            .await
            .expect("d's block in the second load");
        assert_eq!(
            held.chain_changes(Node::index(dirs[0])),
            block.chain_changes(Node::index(dirs[0])),
            "both loads count from the same start"
        );
        for refused in [
            second
                .node_detach(repository.clone(), dirs[0], Detach::IfUnchanged(&read))
                .await,
            second
                .served()
                .node_detach(repository.clone(), dirs[0], Detach::IfUnchanged(&read))
                .await,
        ] {
            assert!(!refused.expect("the detach answers"));
        }
        assert!(!record(&second, &repository, dirs[0]).await.is_discarded());
    })
    .await;
}

#[tokio::test]
async fn a_served_reattach_dirties_both_directories_first() {
    with_state(|repository, state| async move {
        let from = chain(&state, &repository, ROOT_NODE, &["a", "b"]).await;
        let to = chain(&state, &repository, ROOT_NODE, &["x", "y"]).await;
        let f = add(&state, &repository, from[1], file("f"), "f").await;

        let moved = state
            .served()
            .node_reattach(repository.clone(), f, to[1], "g", ReattachCheck::Walk)
            .await
            .expect("the move succeeds");
        assert_eq!(moved, AttachOutcome::Attached(f));
        assert_eq!(record(&state, &repository, f).await.parent, to[1]);
        for &dir in from.iter().chain(&to) {
            assert!(is_dirty(&state, &repository, dir).await, "{dir} is dirty");
        }

        // A move onto a held name replaces the holder in the same critical section.
        let holder = add(&state, &repository, from[1], file("h"), "h").await;
        let moved = state
            .served()
            .node_reattach(
                repository.clone(),
                f,
                from[1],
                "h",
                ReattachCheck::Replacing(holder),
            )
            .await
            .expect("the replacing move succeeds");
        assert_eq!(moved, AttachOutcome::Attached(f));
        assert!(record(&state, &repository, holder).await.is_discarded());
        assert_eq!(record(&state, &repository, f).await.parent, from[1]);

        // A same-directory rename marks the directory once.
        set_clear_pending(&state, &repository, from[1]).await;
        let moved = state
            .served()
            .node_reattach(repository.clone(), f, from[1], "i", ReattachCheck::Walk)
            .await
            .expect("the rename succeeds");
        assert_eq!(moved, AttachOutcome::Attached(f));
        assert!(!clear_pending(&state, &repository, from[1]).await);
    })
    .await;
}

/// A served respell of a clean directory dirties it and drops its pending clear; one of a clean
/// file leaves the file clean, as every served rename copies a file up first; an unserved
/// respell dirties neither.
#[tokio::test]
async fn a_served_respell_dirties_a_clean_directory_and_no_file() {
    with_state(|repository, state| async move {
        let d = chain(&state, &repository, ROOT_NODE, &["d"]).await[0];
        let e = add(&state, &repository, d, directory("e"), "e").await;
        let f = add(&state, &repository, d, file("f"), "f").await;

        for (node, name) in [(e, "E"), (f, "F")] {
            state
                .node_reattach(repository.clone(), node, d, name)
                .await
                .expect("an unserved respell");
            assert!(
                !is_dirty(&state, &repository, node).await,
                "an unserved respell dirtied {name}"
            );
        }
        set_clear_pending(&state, &repository, e).await;
        for (node, name) in [(e, "e"), (f, "f")] {
            let respelled = state
                .served()
                .node_reattach(repository.clone(), node, d, name, ReattachCheck::Walk)
                .await
                .expect("a served respell");
            assert_eq!(respelled, AttachOutcome::Attached(node));
        }
        assert!(
            is_dirty(&state, &repository, e).await,
            "the directory is clean"
        );
        assert!(!clear_pending(&state, &repository, e).await);
        assert!(!is_dirty(&state, &repository, f).await, "the file is dirty");
    })
    .await;
}

/// A served reattach reports whether the node counted as added as it moved, read before its own
/// change: a mover taking over a path the revision holds, a holder with no add action, reports
/// its add and loses it, a file turning modified. One meeting a held name reports no add. A
/// directory moves to another name only while it holds local content, and a respell moves any;
/// the plain served reattach moves a tracked directory too.
#[tokio::test]
async fn a_served_reattach_reports_its_add_and_moves_only_local_directories() {
    with_state(|repository, state| async move {
        let d = chain(&state, &repository, ROOT_NODE, &["d"]).await[0];
        let a = add(&state, &repository, d, local_file("a"), "a").await;
        let held = add(&state, &repository, d, file("held"), "held").await;

        let moved = state
            .served()
            .node_reattach_local(repository.clone(), a, d, "b", ReattachCheck::Walk)
            .await
            .expect("the rename");
        assert_eq!(moved, Reattached::Moved { added: true });
        let taken = state
            .served()
            .node_reattach_local(
                repository.clone(),
                a,
                d,
                "held",
                ReattachCheck::Replacing(held),
            )
            .await
            .expect("the rename over a tracked file");
        assert_eq!(taken, Reattached::Moved { added: true });
        let a_now = record(&state, &repository, a).await;
        assert!(
            !a_now.is_dirty_add(),
            "the mover over a tracked path kept its add"
        );
        assert!(
            a_now.is_dirty_modify(),
            "the mover over a tracked path is not modified"
        );
        let other = add(&state, &repository, d, local_file("other"), "other").await;
        let met = state
            .served()
            .node_reattach_local(repository.clone(), other, d, "held", ReattachCheck::Walk)
            .await
            .expect("the rename onto a held name");
        assert_eq!(met, Reattached::Exists(a));

        // Refused before any mark: the clean directory above stays clean.
        let [p, tracked] = chain(&state, &repository, ROOT_NODE, &["p", "t"]).await[..] else {
            unreachable!("two directories")
        };
        let refused = state
            .served()
            .node_reattach_local(repository.clone(), tracked, p, "u", ReattachCheck::Walk)
            .await
            .expect("the refusal");
        assert_eq!(refused, Reattached::NotLocal);
        let unmoved = record(&state, &repository, tracked).await;
        assert_eq!((unmoved.parent, unmoved.name_hash), (p, hash_string("t")));
        assert!(
            !is_dirty(&state, &repository, p).await,
            "the refusal marked p"
        );
        assert!(!unmoved.is_dirty(), "the refusal marked t");
        let elsewhere = add(&state, &repository, d, directory("v"), "v").await;
        let moved = state
            .served()
            .node_reattach(
                repository.clone(),
                elsewhere,
                ROOT_NODE,
                "w",
                ReattachCheck::Walk,
            )
            .await
            .expect("a tracked directory's move admitting any");
        assert_eq!(moved, AttachOutcome::Attached(elsewhere));
        assert!(
            is_dirty(&state, &repository, elsewhere).await,
            "the clean directory moved to a path its revision may lack is clean"
        );
        let respelled = state
            .served()
            .node_reattach_local(repository.clone(), tracked, p, "T", ReattachCheck::Walk)
            .await
            .expect("the respell of a tracked directory");
        assert_eq!(respelled, Reattached::Moved { added: false });
        let local = add(
            &state,
            &repository,
            d,
            Node {
                flags: NodeFlags::DirtyAdd.bits(),
                ext_flags: NodeExtFlags::LocalContent.bits(),
                ..directory("l")
            },
            "l",
        )
        .await;
        let moved = state
            .served()
            .node_reattach_local(
                repository.clone(),
                local,
                ROOT_NODE,
                "m",
                ReattachCheck::Walk,
            )
            .await
            .expect("the move of a local directory");
        assert_eq!(moved, Reattached::Moved { added: true });
    })
    .await;
}

#[tokio::test]
async fn a_served_rewrite_answers_a_clean_directory_and_follows_a_moved_file() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b"]).await;
        let other = add(&state, &repository, ROOT_NODE, directory("o"), "o").await;
        let f = add(&state, &repository, dirs[1], file("f"), "f").await;

        // Below a clean directory the rewrite changes nothing and answers the directory to mark.
        let answered = rewrite(&state, &repository, f, dirs[1], 7)
            .await
            .expect("the rewrite answers");
        assert_eq!(answered, Some(dirs[1]));
        let node = record(&state, &repository, f).await;
        assert_eq!(node.size, 0);
        assert!(!node.is_dirty());
        state
            .served()
            .node_mark(&repository, dirs[1])
            .await
            .expect("the mark succeeds");
        let answered = rewrite(&state, &repository, f, dirs[1], 7)
            .await
            .expect("the rewrite succeeds");
        assert_eq!(answered, None);
        let node = record(&state, &repository, f).await;
        assert_eq!(node.size, 7);
        assert!(node.is_dirty_modify());
        assert_eq!(node.ext_flags(), NodeExtFlags::LocalContent);
        assert!(is_dirty(&state, &repository, dirs[0]).await);

        // A file moved since the caller found it is followed below the directory it went to, which
        // is answered while clean, and rewritten there once it is marked.
        state
            .node_reattach(repository.clone(), f, other, "f")
            .await
            .expect("the move succeeds");
        clean(&state, &repository, other).await;
        let answered = rewrite(&state, &repository, f, dirs[1], 9)
            .await
            .expect("the rewrite answers");
        assert_eq!(answered, Some(other));
        assert_eq!(record(&state, &repository, f).await.size, 7);
        state
            .served()
            .node_mark(&repository, other)
            .await
            .expect("the mark succeeds");
        let answered = rewrite(&state, &repository, f, dirs[1], 9)
            .await
            .expect("the rewrite succeeds");
        assert_eq!(answered, None);
        assert_eq!(record(&state, &repository, f).await.size, 9);

        // A directory, the root and no node at all are refused; a detached file is not found.
        let refused = rewrite(&state, &repository, dirs[1], dirs[0], 0).await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|error| error.is_invalid_arguments()),
            "a directory has no content, got {refused:?}"
        );
        let link = add(
            &state,
            &repository,
            dirs[1],
            Node {
                flags: NodeFlags::Link.bits(),
                name_hash: hash_string("l"),
                ..Default::default()
            },
            "l",
        )
        .await;
        let refused = rewrite(&state, &repository, link, dirs[1], 0).await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|error| error.is_invalid_arguments()),
            "a link's address is its target, got {refused:?}"
        );
        state
            .node_detach(repository.clone(), f, Detach::Always)
            .await
            .expect("the detach succeeds");
        let refused = rewrite(&state, &repository, f, other, 0).await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|error| error.is_node_not_found()),
            "a detached file is not found, got {refused:?}"
        );
        for node in [ROOT_NODE, INVALID_NODE] {
            let refused = rewrite(&state, &repository, node, ROOT_NODE, 0).await;
            assert!(
                refused
                    .as_ref()
                    .is_err_and(|error| error.is_invalid_arguments()),
                "node {node} is refused, got {refused:?}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn a_swap_attach_and_detach_mark_no_directory() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b"]).await;

        let begun = state
            .swap()
            .node_attach_tree(repository.clone(), dirs[1], &file("f"), "f")
            .await
            .expect("the copy begins");
        let TreeAttach::Building(build) = begun else {
            panic!("the name is free");
        };
        let attached = build.publish().await.expect("the copy publishes");
        let AttachOutcome::Attached(f) = attached else {
            panic!("the copy attaches, got {attached:?}");
        };
        let again = state
            .swap()
            .node_attach_tree(repository.clone(), dirs[1], &file("f"), "f")
            .await
            .expect("the copy answers");
        assert!(matches!(again, TreeAttach::Exists(held) if held == f));
        assert!(!is_dirty(&state, &repository, dirs[1]).await);

        let swap = state.swap();
        // A non-empty directory and a dirty node are refused, and nothing moves.
        assert!(
            !swap
                .node_detach(repository.clone(), dirs[1], Detach::IfEmpty)
                .await
                .expect("the detach answers")
        );
        let g = add(&state, &repository, dirs[1], file("g"), "g").await;
        mark(
            &state,
            &repository,
            g,
            NodeFlags::DirtyModify,
            NodeExtFlags::NoFlags,
        )
        .await;
        assert!(
            !swap
                .node_detach(repository.clone(), g, Detach::IfClean)
                .await
                .expect("the detach answers")
        );
        assert!(
            !swap
                .node_detach(repository.clone(), g, Detach::At(dirs[0], hash_string("g")))
                .await
                .expect("the detach answers"),
            "a node no longer at the place is refused"
        );
        assert!(
            swap.node_detach(repository.clone(), g, Detach::At(dirs[1], hash_string("g")))
                .await
                .expect("the detach answers")
        );

        let clean = chain(&state, &repository, ROOT_NODE, &["p", "q"]).await;
        let h = add(&state, &repository, clean[1], file("h"), "h").await;
        assert!(
            swap.node_detach(repository.clone(), h, Detach::IfClean)
                .await
                .expect("the detach answers")
        );
        assert!(
            swap.node_detach(repository.clone(), f, Detach::Always)
                .await
                .expect("the detach answers")
        );
        assert!(
            swap.node_detach(repository.clone(), clean[1], Detach::IfEmpty)
                .await
                .expect("the detach answers")
        );
        assert!(
            !is_dirty(&state, &repository, clean[0]).await,
            "the swap's detaches leave their directory clean"
        );
        assert!(record(&state, &repository, h).await.is_discarded());
        assert!(record(&state, &repository, clean[1]).await.is_discarded());
    })
    .await;
}

#[tokio::test]
async fn a_swap_detach_of_the_root_or_below_a_detached_directory_is_refused() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b"]).await;
        let f = add(&state, &repository, dirs[1], file("f"), "f").await;
        let swap = state.swap();
        for node in [ROOT_NODE, INVALID_NODE] {
            let refused = swap
                .node_detach(repository.clone(), node, Detach::Always)
                .await;
            assert!(
                refused
                    .as_ref()
                    .is_err_and(|error| error.is_invalid_arguments()),
                "node {node} is not detachable, got {refused:?}"
            );
        }

        state
            .node_detach(repository.clone(), dirs[1], Detach::Always)
            .await
            .expect("the detach succeeds");
        for when in [
            Detach::Always,
            Detach::At(dirs[1], hash_string("f")),
            Detach::IfClean,
            Detach::IfEmpty,
        ] {
            let refused = swap.node_detach(repository.clone(), f, when).await;
            assert!(
                refused
                    .as_ref()
                    .is_err_and(|error| error.is_node_not_found()),
                "{when:?} below a detached directory is not found, got {refused:?}"
            );
        }
        let refused = state
            .served()
            .node_detach(repository.clone(), f, Detach::Always)
            .await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|error| error.is_node_not_found()),
            "got {refused:?}"
        );
        assert!(!record(&state, &repository, f).await.is_discarded());
    })
    .await;
}

/// The pair lock takes its blocks in index order whichever of parent and child is first: a
/// directory in block 1 above a directory and a file in block 0.
#[tokio::test]
async fn served_changes_below_a_directory_in_a_later_block_than_the_node() {
    with_state(|repository, state| async move {
        let tree = spread(&state, &repository).await;

        state
            .served()
            .node_mark(&repository, tree.p)
            .await
            .expect("the mark succeeds");
        for dir in [tree.top, tree.x, tree.p] {
            assert!(is_dirty(&state, &repository, dir).await, "{dir} is dirty");
        }

        clean(&state, &repository, tree.x).await;
        let answered = rewrite(&state, &repository, tree.f, tree.x, 5)
            .await
            .expect("the rewrite answers");
        assert_eq!(answered, Some(tree.x));
        state
            .served()
            .node_mark(&repository, tree.x)
            .await
            .expect("the mark succeeds");
        let answered = rewrite(&state, &repository, tree.f, tree.x, 5)
            .await
            .expect("the rewrite succeeds");
        assert_eq!(answered, None);
        assert_eq!(record(&state, &repository, tree.f).await.size, 5);
    })
    .await;
}

/// Each block a served change writes is recorded changed and the State marked dirty, so a save
/// writes it: a load finds every mark and the rewrite.
#[tokio::test]
async fn served_changes_record_their_blocks_so_a_reload_keeps_them() {
    with_state(|repository, first| async move {
        let tree = spread(&first, &repository).await;
        let state = reload(&first, &repository).await;
        assert!(!state.is_dirty());

        state
            .served()
            .node_mark(&repository, tree.p)
            .await
            .expect("the mark succeeds");
        assert!(state.is_dirty(), "the mark marks the State dirty");
        let answered = rewrite(&state, &repository, tree.f, tree.x, 11)
            .await
            .expect("the rewrite succeeds");
        assert_eq!(answered, None);

        let reloaded = reload(&state, &repository).await;
        for dir in [tree.top, tree.x, tree.p] {
            assert!(
                is_dirty(&reloaded, &repository, dir).await,
                "{dir} is dirty"
            );
        }
        let f = record(&reloaded, &repository, tree.f).await;
        assert_eq!(f.size, 11);
        assert!(f.is_dirty_modify());
        assert_eq!(f.ext_flags(), NodeExtFlags::LocalContent);

        // A mark of the root dirties block 0 and the State.
        let state = reload(&first, &repository).await;
        assert!(!state.is_dirty());
        state
            .served()
            .node_mark(&repository, ROOT_NODE)
            .await
            .expect("a mark of the root succeeds");
        assert!(state.is_dirty(), "a mark of the root marks the State dirty");
    })
    .await;
}

/// A mark whose target's block is not in memory reads it in its first pass: a dirty target is
/// marked already, and a detached one is not found.
#[tokio::test]
async fn a_served_mark_of_a_directory_not_in_memory() {
    with_state(|repository, first| async move {
        let tree = spread(&first, &repository).await;
        let dirty = add(&first, &repository, tree.x, directory("dirty"), "dirty").await;
        let detached = add(&first, &repository, tree.x, directory("gone"), "gone").await;
        mark(
            &first,
            &repository,
            dirty,
            NodeFlags::DirtyAdd,
            NodeExtFlags::LocalContent,
        )
        .await;
        first
            .node_detach(repository.clone(), detached, Detach::Always)
            .await
            .expect("the detach succeeds");
        assert_eq!(NodeBlock::index(dirty), 1);
        assert_eq!(NodeBlock::index(detached), 1);

        let state = reload(&first, &repository).await;
        assert!(state.resident_block(1).is_none());
        state
            .served()
            .node_mark(&repository, dirty)
            .await
            .expect("the mark succeeds");
        let node = record(&state, &repository, dirty).await;
        assert!(node.is_dirty_add(), "the dirty target keeps its action");
        assert_eq!(node.ext_flags(), NodeExtFlags::LocalContent);

        let state = reload(&first, &repository).await;
        assert!(state.resident_block(1).is_none());
        let marked = state.served().node_mark(&repository, detached).await;
        assert!(
            marked
                .as_ref()
                .is_err_and(|error| error.is_node_not_found()),
            "got {marked:?}"
        );
    })
    .await;
}

/// A parent chain only a corrupt State holds fails the mark, which marks nothing: a parent
/// missing is not found, a short cycle reads as a move under way until the attempts run out,
/// and a cycle longer than a pass climbs past any path's depth.
#[tokio::test]
async fn a_served_mark_up_a_corrupt_parent_chain_fails_and_marks_nothing() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b"]).await;
        corrupt_parent(&state, &repository, dirs[0], INVALID_NODE).await;
        let marked = state.served().node_mark(&repository, dirs[1]).await;
        assert!(
            marked
                .as_ref()
                .is_err_and(|error| error.is_node_not_found()),
            "got {marked:?}"
        );
        assert!(!is_dirty(&state, &repository, dirs[1]).await);

        let state = State::new();
        let own = add(&state, &repository, ROOT_NODE, directory("own"), "own").await;
        corrupt_parent(&state, &repository, own, own).await;
        let marked = state.served().node_mark(&repository, own).await;
        assert!(
            marked.as_ref().is_err_and(|error| error.is_slow_down()),
            "got {marked:?}"
        );
        assert!(!is_dirty(&state, &repository, own).await);

        let state = State::new();
        let names: Vec<String> = (0..200).map(|level| format!("level{level}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let dirs = chain(&state, &repository, ROOT_NODE, &names).await;
        let last = *dirs.last().expect("a leaf");
        corrupt_parent(&state, &repository, dirs[0], last).await;
        let marked = state.served().node_mark(&repository, last).await;
        assert!(
            marked.as_ref().is_err_and(|error| error.is_internal()),
            "got {marked:?}"
        );
        for &dir in &dirs {
            assert!(!is_dirty(&state, &repository, dir).await, "{dir} is clean");
        }
    })
    .await;
}

/// A resident-only attach whose directory's block is not in memory answers `None`, reading
/// nothing and changing nothing.
#[tokio::test]
async fn a_resident_served_attach_answers_none_where_the_directory_is_not_in_memory() {
    with_state(|repository, first| async move {
        let tree = spread(&first, &repository).await;
        let state = reload(&first, &repository).await;
        assert!(state.resident_block(NodeBlock::index(tree.x)).is_none());

        let attached = state
            .served()
            .node_attach(
                repository.clone(),
                tree.x,
                local_file("n"),
                "n",
                AttachCheck::Resident(None),
            )
            .await
            .expect("the attach answers");
        assert_eq!(attached, None);
        assert!(state.resident_block(NodeBlock::index(tree.x)).is_none());
        assert!(!state.is_dirty());
    })
    .await;
}

/// An attach or a reattach handed an absence checks the directory's log since it in place of
/// walking: a name attached since is held, and one still absent attaches.
#[tokio::test]
async fn served_attach_and_reattach_against_an_absence() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b"]).await;
        let g = add(&state, &repository, ROOT_NODE, file("g"), "g").await;
        let block = state
            .block(repository.clone(), NodeBlock::index(dirs[1]))
            .await
            .expect("the directory's block");
        let absence = |name: &str| {
            let (mark, _) = block.watch_chain(Node::index(dirs[1]), None);
            Absent::new(&state, mark.seq, block.clone(), dirs[1], hash_string(name))
        };
        let f_absent = absence("f");
        let g_absent = absence("g");

        let attached = state
            .served()
            .node_attach(
                repository.clone(),
                dirs[1],
                local_file("f"),
                "f",
                AttachCheck::Absent(&f_absent),
            )
            .await
            .expect("the attach succeeds");
        let Some(AttachOutcome::Attached(f)) = attached else {
            panic!("the file attaches, got {attached:?}");
        };
        assert!(is_dirty(&state, &repository, dirs[0]).await);
        assert!(is_dirty(&state, &repository, dirs[1]).await);
        let again = state
            .served()
            .node_attach(
                repository.clone(),
                dirs[1],
                local_file("f"),
                "f",
                AttachCheck::Absent(&f_absent),
            )
            .await
            .expect("the attach answers");
        assert_eq!(again, Some(AttachOutcome::Exists(f)));

        clean(&state, &repository, dirs[1]).await;
        let moved = state
            .served()
            .node_reattach(
                repository.clone(),
                g,
                dirs[1],
                "g",
                ReattachCheck::Absent(&g_absent),
            )
            .await
            .expect("the move succeeds");
        assert_eq!(moved, AttachOutcome::Attached(g));
        assert_eq!(record(&state, &repository, g).await.parent, dirs[1]);
        assert!(is_dirty(&state, &repository, dirs[1]).await);
        let moved = state
            .served()
            .node_reattach(
                repository.clone(),
                f,
                dirs[1],
                "g",
                ReattachCheck::Absent(&g_absent),
            )
            .await
            .expect("the move answers");
        assert_eq!(moved, AttachOutcome::Exists(g));
    })
    .await;
}

/// A settle's pending clear is one bit per slot, apart from every other, across the words of
/// the bitmap.
#[test]
fn the_pending_clears_of_a_block_stand_apart() {
    let block = NodeBlock::new_zeroed();
    let probes = [
        0,
        1,
        62,
        63,
        64,
        65,
        127,
        128,
        BLOCK_NODE_COUNT - 2,
        BLOCK_NODE_COUNT - 1,
    ];
    for index in [0, 63, 64, BLOCK_NODE_COUNT - 1] {
        let writer = block.write_for_tests();
        writer.set_clear_pending(index);
        for probe in probes {
            assert_eq!(
                block.clear_pending(probe),
                probe == index,
                "slot {probe} with slot {index} pending"
            );
        }
        writer.drop_clear_pending(index);
        writer.drop_clear_pending(index);
        assert!(probes.iter().all(|&probe| !block.clear_pending(probe)));
    }
}

/// A level of the descent below which a directory already dirty with an action and a pending
/// clear stands keeps that directory's action and extension flags, dropping only its pending
/// clear.
#[tokio::test]
async fn a_served_mark_keeps_the_flags_of_a_dirty_directory_it_descends_through() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b", "c"]).await;
        mark(
            &state,
            &repository,
            dirs[1],
            NodeFlags::DirtyAdd,
            NodeExtFlags::LocalContent,
        )
        .await;
        set_clear_pending(&state, &repository, dirs[1]).await;

        state
            .served()
            .node_mark(&repository, dirs[2])
            .await
            .expect("the mark succeeds");
        let b = record(&state, &repository, dirs[1]).await;
        assert!(b.is_dirty_add(), "the directory keeps its action");
        assert_eq!(b.ext_flags(), NodeExtFlags::LocalContent);
        assert!(!clear_pending(&state, &repository, dirs[1]).await);
        assert!(is_dirty(&state, &repository, dirs[2]).await);
    })
    .await;
}

/// A rewrite whose hint names a directory the file left follows it into a directory in another
/// block, and answers that directory while it is clean.
#[tokio::test]
async fn a_served_rewrite_follows_a_file_into_a_directory_in_another_block() {
    with_state(|repository, state| async move {
        let tree = spread(&state, &repository).await;

        let answered = rewrite(&state, &repository, tree.f, tree.top, 3)
            .await
            .expect("the rewrite answers");
        assert_eq!(answered, Some(tree.x));
        state
            .served()
            .node_mark(&repository, tree.x)
            .await
            .expect("the mark succeeds");
        let answered = rewrite(&state, &repository, tree.f, tree.top, 3)
            .await
            .expect("the rewrite succeeds");
        assert_eq!(answered, None);
        assert_eq!(record(&state, &repository, tree.f).await.size, 3);
    })
    .await;
}

/// A rewrite of a file still below a detached directory is not found: the file is in a detached
/// subtree.
#[tokio::test]
async fn a_served_rewrite_below_a_detached_directory_is_not_found() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b"]).await;
        let f = add(&state, &repository, dirs[1], file("f"), "f").await;
        state
            .node_detach(repository.clone(), dirs[1], Detach::Always)
            .await
            .expect("the detach succeeds");

        let refused = rewrite(&state, &repository, f, dirs[1], 1).await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|error| error.is_node_not_found()),
            "got {refused:?}"
        );
    })
    .await;
}

/// A mark of the root, or of no node, dirties block 0 and the State only, as the unserved marks
/// do; a mark reaching the root from a child in another block dirties block 0 too.
#[tokio::test]
async fn a_served_mark_reaching_the_root_dirties_block_zero() {
    with_state(|repository, first| async move {
        let fill = add(&first, &repository, ROOT_NODE, directory("fill"), "fill").await;
        for index in 0..BLOCK_NODE_COUNT {
            let name = format!("filler{index}");
            add(&first, &repository, fill, file(&name), &name).await;
        }
        let z = add(&first, &repository, ROOT_NODE, directory("z"), "z").await;
        assert_ne!(NodeBlock::index(z), 0);

        let state = reload(&first, &repository).await;
        state
            .served()
            .node_mark(&repository, z)
            .await
            .expect("the mark succeeds");
        assert!(state.block_recorded_changed(NodeBlock::index(z)));
        assert!(
            state.block_recorded_changed(0),
            "the root's block is recorded changed"
        );

        let state = reload(&first, &repository).await;
        state
            .served()
            .node_mark(&repository, INVALID_NODE)
            .await
            .expect("a mark of no node succeeds");
        assert!(state.block_recorded_changed(0));
        assert!(state.is_dirty());
    })
    .await;
}

/// A swap attaches only clean copies from its target: a dirty one is a bad argument.
#[tokio::test]
async fn a_swap_copy_of_a_dirty_node_is_refused() {
    with_state(|repository, state| async move {
        let error = state
            .swap()
            .node_attach_tree(repository.clone(), ROOT_NODE, &local_file("f"), "f")
            .await
            .err()
            .expect("a dirty copy is refused");
        assert!(error.is_invalid_arguments(), "{error}");
    })
    .await;
}

/// A served change landing between the comparison and the clear keeps the directory, its
/// pending clear gone: a mark of a subdirectory, which drops the pending clear alone; a served
/// detach, which drops it and moves the chain count; an unserved add, which moves the count
/// alone. Each counts as a clear overtaken.
#[tokio::test]
async fn a_served_clear_keeps_a_directory_a_change_reached() {
    with_repository(|repository| async move {
        for change in ["subdirectory mark", "served detach", "unserved add"] {
            let pair = committed_pair(&repository).await;
            let [d, f, _, e] = pair.nodes;
            let overtaken = pair.served.clears_overtaken();
            let served = pair.served.clone();
            let repo = repository.clone();
            let cleared = settle_against(
                &pair,
                &repository,
                d,
                (pair.committed_d, pair.committed_d),
                async move {
                    match change {
                        "subdirectory mark" => {
                            served.served().node_mark(&repo, e).await.expect("the mark");
                        }
                        "served detach" => {
                            assert!(
                                served
                                    .served()
                                    .node_detach(repo.clone(), f, Detach::Always)
                                    .await
                                    .expect("the detach")
                            );
                        }
                        _ => {
                            add(&served, &repo, d, holding("h", true, 5), "h").await;
                        }
                    }
                },
            )
            .await;
            assert_eq!(cleared, ClearOutcome::Kept, "{change}");
            assert!(is_dirty(&pair.served, &repository, d).await, "{change}");
            assert!(
                !clear_pending(&pair.served, &repository, d).await,
                "{change}"
            );
            assert_eq!(pair.served.clears_overtaken(), overtaken + 1, "{change}");
        }
    })
    .await;
}

/// A clear dropped between its phases, as a cancelled settle drops it, leaves no pending clear
/// and the directory dirty.
#[tokio::test]
async fn a_dropped_served_clear_leaves_no_pending_clear() {
    with_repository(|repository| async move {
        let pair = committed_pair(&repository).await;
        let [d, ..] = pair.nodes;
        let reached = std::sync::atomic::AtomicBool::new(false);
        let clearing = settle_against(
            &pair,
            &repository,
            d,
            (pair.committed_d, pair.committed_d),
            async {
                reached.store(true, std::sync::atomic::Ordering::Relaxed);
                std::future::pending::<()>().await;
            },
        );
        tokio::select! {
            biased;
            _ = clearing => unreachable!("a clear waiting on a pending future ended"),
            () = tokio::task::yield_now() => {}
        }
        assert!(
            reached.load(std::sync::atomic::Ordering::Relaxed),
            "the clear was dropped between its phases"
        );
        assert!(is_dirty(&pair.served, &repository, d).await);
        assert!(!clear_pending(&pair.served, &repository, d).await);
    })
    .await;
}

/// A directory moved, renamed, respelled or detached between the comparison and the clear is
/// kept: the clear checks its place and its name under the lock it clears under.
#[tokio::test]
async fn a_served_clear_keeps_a_directory_that_left_its_place_or_name() {
    with_repository(|repository| async move {
        for change in ["moved", "renamed", "respelled", "detached"] {
            let pair = committed_pair(&repository).await;
            let [d, ..] = pair.nodes;
            let other = add(
                &pair.served,
                &repository,
                ROOT_NODE,
                holding("o", false, 6),
                "o",
            )
            .await;
            pair.served
                .served()
                .node_mark(&repository, other)
                .await
                .expect("the mark");
            let served = pair.served.clone();
            let repo = repository.clone();
            let cleared = settle_against(
                &pair,
                &repository,
                d,
                (pair.committed_d, pair.committed_d),
                async move {
                    let (to, name) = match change {
                        "moved" => (other, "d"),
                        "renamed" => (ROOT_NODE, "renamed"),
                        "respelled" => (ROOT_NODE, "D"),
                        _ => {
                            assert!(
                                served
                                    .served()
                                    .node_detach(repo.clone(), d, Detach::Always)
                                    .await
                                    .expect("the detach")
                            );
                            return;
                        }
                    };
                    assert_eq!(
                        served
                            .served()
                            .node_reattach(repo.clone(), d, to, name, ReattachCheck::Walk)
                            .await
                            .expect("the move"),
                        AttachOutcome::Attached(d)
                    );
                },
            )
            .await;
            assert_eq!(cleared, ClearOutcome::Kept, "{change}");
            assert!(is_dirty(&pair.served, &repository, d).await, "{change}");
            assert!(
                !clear_pending(&pair.served, &repository, d).await,
                "{change}"
            );
        }
    })
    .await;
}

/// A clear that clears and one that keeps, dropping the add action, each record the directory's
/// block, so a reload keeps what they wrote, with no pending clear.
#[tokio::test]
async fn a_served_clear_records_its_change_so_a_reload_keeps_it() {
    with_repository(|repository| async move {
        for keeps in [false, true] {
            let mut pair = committed_pair(&repository).await;
            let [d, ..] = pair.nodes;
            mark(
                &pair.served,
                &repository,
                d,
                NodeFlags::DirtyAdd,
                NodeExtFlags::NoFlags,
            )
            .await;
            if keeps {
                add(&pair.served, &repository, d, holding("h", true, 5), "h").await;
            }
            pair.served = reload(&pair.served, &repository).await;
            assert!(!pair.served.is_dirty());
            assert!(record(&pair.served, &repository, d).await.is_dirty_add());

            let cleared = settle_against(
                &pair,
                &repository,
                d,
                (pair.committed_d, pair.committed_d),
                async {},
            )
            .await;
            assert_eq!(cleared == ClearOutcome::Cleared, !keeps);
            assert!(pair.served.is_dirty(), "the clear marks the State dirty");
            assert!(pair.served.block_recorded_changed(NodeBlock::index(d)));

            let reloaded = reload(&pair.served, &repository).await;
            let held = record(&reloaded, &repository, d).await;
            assert_eq!(held.is_dirty(), keeps, "the reload lost the clear");
            assert!(
                !held.is_dirty_add(),
                "the reload lost the dropped add action"
            );
            assert!(!clear_pending(&reloaded, &repository, d).await);
        }
    })
    .await;
}

/// A served attach whose walk finds the name held answers `Exists` and marks nothing.
#[tokio::test]
async fn a_served_attach_of_a_held_name_marks_nothing() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b"]).await;
        let f = add(&state, &repository, dirs[1], file("f"), "f").await;

        let again = state
            .served()
            .node_attach(
                repository.clone(),
                dirs[1],
                local_file("f"),
                "f",
                AttachCheck::Walk,
            )
            .await
            .expect("the attach answers");
        assert_eq!(again, Some(AttachOutcome::Exists(f)));
        assert!(!is_dirty(&state, &repository, dirs[1]).await);
    })
    .await;
}

/// A served reattach whose walk finds the name held answers `Exists` before its pre-tests, so
/// neither directory is marked and the node stays where it was.
#[tokio::test]
async fn a_served_reattach_onto_a_held_name_marks_nothing() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b"]).await;
        let f = add(&state, &repository, dirs[1], file("f"), "f").await;
        let g = add(&state, &repository, dirs[0], file("g"), "g").await;
        for node in [dirs[0], dirs[1], f, g] {
            clean(&state, &repository, node).await;
        }

        let moved = state
            .served()
            .node_reattach(repository.clone(), g, dirs[1], "f", ReattachCheck::Walk)
            .await
            .expect("the reattach answers");
        assert_eq!(moved, AttachOutcome::Exists(f));
        assert!(!is_dirty(&state, &repository, dirs[0]).await);
        assert!(!is_dirty(&state, &repository, dirs[1]).await);
        assert_eq!(record(&state, &repository, g).await.parent, dirs[0]);
    })
    .await;
}

/// A copy-up marks a clean directory and rewrites the file; one of a file in a detached
/// subtree is not found.
#[tokio::test]
async fn a_served_copy_up_marks_its_directory_or_finds_the_file_detached() {
    with_state(|repository, state| async move {
        let dirs = chain(&state, &repository, ROOT_NODE, &["a", "b"]).await;
        let f = add(&state, &repository, dirs[1], file("f"), "f").await;
        let copy_up = |node| {
            state.served().node_copy_up(
                &repository,
                node,
                dirs[1],
                (0o644, 4),
                (NodeFlags::DirtyModify, NodeExtFlags::LocalContent),
            )
        };

        copy_up(f).await.expect("the copy-up succeeds");
        assert_eq!(record(&state, &repository, f).await.size, 4);
        assert!(is_dirty(&state, &repository, dirs[1]).await);

        let g = add(&state, &repository, dirs[1], file("g"), "g").await;
        clean(&state, &repository, dirs[1]).await;
        state
            .node_detach(repository.clone(), dirs[0], Detach::Always)
            .await
            .expect("the detach succeeds");
        let refused = copy_up(g).await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|error| error.is_node_not_found()),
            "got {refused:?}"
        );
    })
    .await;
}

/// A node of kind `file` or directory named `name`, holding content `seed`, a file with mode
/// 0o644.
fn holding(name: &str, file: bool, seed: u64) -> Node {
    let mut node = if file {
        self::file(name)
    } else {
        directory(name)
    };
    node.address.hash = lore_base::types::Hash::from_u64(seed);
    node.size = seed;
    if file {
        node.mode = 0o644;
    }
    node
}

/// A served State and a committed one holding the same tree: directory `d` (content 1) with
/// files `f` (2) and `g` (3) and directory `e` (4); `d` in the served one marked dirty.
struct Committed {
    served: Arc<State>,
    committed: Arc<State>,
    /// `d`, then `f`, `g` and `e`, in the served State.
    nodes: [NodeID; 4],
    /// `d` in the committed State.
    committed_d: NodeID,
}

async fn committed_pair(repository: &Arc<RepositoryContext>) -> Committed {
    let (served, committed) = (State::new(), State::new());
    let mut built = Vec::new();
    for state in [&served, &committed] {
        let d = add(state, repository, ROOT_NODE, holding("d", false, 1), "d").await;
        let f = add(state, repository, d, holding("f", true, 2), "f").await;
        let g = add(state, repository, d, holding("g", true, 3), "g").await;
        let e = add(state, repository, d, holding("e", false, 4), "e").await;
        built.push([d, f, g, e]);
    }
    served
        .served()
        .node_mark(repository, built[0][0])
        .await
        .expect("the mark succeeds");
    Committed {
        served,
        committed,
        nodes: built[0],
        committed_d: built[1][0],
    }
}

/// A settle's clear of served directory `dir` against `against` in the committed State, spelled
/// as `spelled` there, `before` running between the comparison and the clear.
async fn settle_against(
    pair: &Committed,
    repository: &Arc<RepositoryContext>,
    dir: NodeID,
    (against, spelled): (NodeID, NodeID),
    before: impl std::future::Future<Output = ()>,
) -> ClearOutcome {
    let held = record(&pair.served, repository, dir).await;
    let mut scratch = ClearScratch::default();
    let in_committed = |node| ClearAgainst {
        state: &pair.committed,
        repository,
        node,
    };
    pair.served
        .served()
        .node_clear(
            repository,
            dir,
            ServedClear {
                place: (held.parent, held.name_hash),
                against: in_committed(against),
                spelled: in_committed(spelled),
                scratch: &mut scratch,
            },
            before,
        )
        .await
        .expect("the clear runs")
}

/// The clear compares children as multisets whatever order either chain lists them in: the
/// committed children in another order, or out of order only from some child on, clear; one
/// child's content or name differing past an in-order prefix, a child missing, or one more keeps
/// the directory.
#[tokio::test]
async fn a_served_clear_compares_children_in_any_order() {
    with_repository(|repository| async move {
        let served_order = [("a", 1), ("b", 2), ("c", 3), ("d", 4)];
        for (committed, clears) in [
            (vec![("d", 4), ("c", 3), ("b", 2), ("a", 1)], true),
            (vec![("a", 1), ("b", 2), ("d", 4), ("c", 3)], true),
            (vec![("a", 1), ("b", 2), ("c", 3), ("d", 5)], false),
            (vec![("a", 1), ("b", 2), ("c", 3), ("e", 4)], false),
            (vec![("a", 1), ("b", 2), ("d", 4)], false),
            (
                vec![("a", 1), ("b", 2), ("c", 3), ("d", 4), ("e", 6)],
                false,
            ),
        ] {
            let (served, in_committed) = (State::new(), State::new());
            let dir = add(&served, &repository, ROOT_NODE, holding("x", false, 9), "x").await;
            let against = add(
                &in_committed,
                &repository,
                ROOT_NODE,
                holding("x", false, 9),
                "x",
            )
            .await;
            // Each chain lists its children newest first, so add in reverse to list them in order.
            for (name, seed) in served_order.iter().rev() {
                add(&served, &repository, dir, holding(name, true, *seed), name).await;
            }
            for (name, seed) in committed.iter().rev() {
                add(
                    &in_committed,
                    &repository,
                    against,
                    holding(name, true, *seed),
                    name,
                )
                .await;
            }
            served
                .served()
                .node_mark(&repository, dir)
                .await
                .expect("the mark");
            let pair = Committed {
                served: served.clone(),
                committed: in_committed.clone(),
                nodes: [dir; 4],
                committed_d: against,
            };
            let cleared =
                settle_against(&pair, &repository, dir, (against, against), async {}).await;
            assert_eq!(
                cleared == ClearOutcome::Cleared,
                clears,
                "against {committed:?}: {cleared:?}"
            );
        }
    })
    .await;
}

/// A directory whose children are the committed directory's, clean and holding the committed
/// content, takes the committed address and size, clean, and keeps no pending clear.
#[tokio::test]
async fn a_served_clear_clears_a_directory_holding_the_committed_children() {
    with_repository(|repository| async move {
        let pair = committed_pair(&repository).await;
        let [d, ..] = pair.nodes;
        let committed = record(&pair.committed, &repository, pair.committed_d).await;
        // Content the clear replaces.
        let block = pair
            .served
            .block(repository.clone(), NodeBlock::index(d))
            .await
            .expect("d's block");
        pair.served.node_edit_for_tests(&block, d, |node| {
            node.address.hash = lore_base::types::Hash::from_u64(99);
        });

        let cleared = settle_against(
            &pair,
            &repository,
            d,
            (pair.committed_d, pair.committed_d),
            async {},
        )
        .await;

        assert_eq!(cleared, ClearOutcome::Cleared);
        let d_record = record(&pair.served, &repository, d).await;
        assert!(!d_record.is_dirty());
        assert_eq!(d_record.address.hash, committed.address.hash);
        assert_eq!(d_record.size, committed.size);
        assert!(!clear_pending(&pair.served, &repository, d).await);
        assert!(pair.served.block_recorded_changed(NodeBlock::index(d)));
    })
    .await;
}

/// Each difference the comparison reads keeps the directory dirty, with no pending clear left
/// and its add action dropped: a dirty child, a clean child holding local content, a child
/// missing or extra, other content, another file mode, another kind, and a same-hash sibling
/// counted once against two.
#[tokio::test]
async fn a_served_clear_keeps_a_directory_whose_children_differ() {
    with_repository(|repository| async move {
        let in_place = async |state: &State, node: NodeID, change: &dyn Fn(&mut Node)| {
            let block = state
                .block(repository.clone(), NodeBlock::index(node))
                .await
                .expect("the block");
            state.node_edit_for_tests(&block, node, change);
        };
        for difference in [
            "dirty",
            "local",
            "missing",
            "extra",
            "content",
            "mode",
            "kind",
            "same hash",
        ] {
            let pair = committed_pair(&repository).await;
            let [d, f, g, e] = pair.nodes;
            mark(
                &pair.served,
                &repository,
                d,
                NodeFlags::DirtyAdd,
                NodeExtFlags::NoFlags,
            )
            .await;
            let mut against = pair.committed_d;
            match difference {
                "dirty" => {
                    mark(
                        &pair.served,
                        &repository,
                        f,
                        NodeFlags::DirtyModify,
                        NodeExtFlags::NoFlags,
                    )
                    .await;
                }
                "local" => {
                    in_place(&pair.served, f, &|node| {
                        node.ext_flags = NodeExtFlags::LocalContent.bits();
                    })
                    .await;
                }
                "missing" => {
                    assert!(
                        pair.served
                            .served()
                            .node_detach(repository.clone(), g, Detach::Always)
                            .await
                            .expect("the detach")
                    );
                }
                "extra" => {
                    add(&pair.served, &repository, d, holding("h", true, 5), "h").await;
                }
                "content" => {
                    in_place(&pair.served, g, &|node| {
                        node.address.hash = lore_base::types::Hash::from_u64(7);
                    })
                    .await;
                }
                "mode" => in_place(&pair.served, f, &|node| node.mode = 0o755).await,
                "kind" => {
                    // The committed side holds `e` as a file of the same content.
                    against = add(
                        &pair.committed,
                        &repository,
                        ROOT_NODE,
                        holding("d2", false, 1),
                        "d2",
                    )
                    .await;
                    for (name, file, seed) in [("f", true, 2), ("g", true, 3), ("e", true, 4)] {
                        add(
                            &pair.committed,
                            &repository,
                            against,
                            holding(name, file, seed),
                            name,
                        )
                        .await;
                    }
                }
                "same hash" => {
                    assert_eq!(hash_string("F"), hash_string("f"));
                    add(
                        &pair.committed,
                        &repository,
                        pair.committed_d,
                        holding("F", true, 2),
                        "F",
                    )
                    .await;
                }
                _ => unreachable!(),
            }
            let _ = e;

            let cleared =
                settle_against(&pair, &repository, d, (against, pair.committed_d), async {}).await;

            assert_eq!(cleared, ClearOutcome::Kept, "{difference}");
            let held = record(&pair.served, &repository, d).await;
            assert!(held.is_dirty(), "{difference}");
            assert!(!held.is_dirty_add(), "{difference}: the add action goes");
            assert!(
                !clear_pending(&pair.served, &repository, d).await,
                "{difference}"
            );
        }
    })
    .await;
}

/// The directory must carry the spelled directory's name byte for byte: a name differing in case
/// alone, which keeps the hash, keeps it dirty; the root carries none and is cleared.
#[tokio::test]
async fn a_served_clear_checks_the_name_and_clears_the_root_by_content() {
    with_repository(|repository| async move {
        let pair = committed_pair(&repository).await;
        let [d, ..] = pair.nodes;
        let respelled = add(
            &pair.committed,
            &repository,
            ROOT_NODE,
            holding("D", false, 1),
            "D",
        )
        .await;

        let cleared = settle_against(
            &pair,
            &repository,
            d,
            (pair.committed_d, respelled),
            async {},
        )
        .await;
        assert_eq!(cleared, ClearOutcome::Kept);
        assert!(is_dirty(&pair.served, &repository, d).await);

        let cleared = settle_against(
            &pair,
            &repository,
            d,
            (pair.committed_d, pair.committed_d),
            async {},
        )
        .await;
        assert_eq!(cleared, ClearOutcome::Cleared);

        // The committed root holds `d` and `D`; the served one `d` alone, then `D` too.
        let root = settle_against(
            &pair,
            &repository,
            ROOT_NODE,
            (ROOT_NODE, ROOT_NODE),
            async {},
        )
        .await;
        assert_eq!(root, ClearOutcome::Kept);
        add(
            &pair.served,
            &repository,
            ROOT_NODE,
            holding("D", false, 1),
            "D",
        )
        .await;
        let root = settle_against(
            &pair,
            &repository,
            ROOT_NODE,
            (ROOT_NODE, ROOT_NODE),
            async {},
        )
        .await;
        assert_eq!(root, ClearOutcome::Cleared);
    })
    .await;
}

/// A directory moved away is [`ClearOutcome::Moved`], its flags untouched; a served change
/// landing between the comparison and the clear marks it again, which drops the pending clear,
/// and the clear keeps it.
#[tokio::test]
async fn a_served_clear_leaves_a_moved_directory_and_keeps_one_a_change_reached() {
    with_repository(|repository| async move {
        let pair = committed_pair(&repository).await;
        let [d, _, g, _] = pair.nodes;
        mark(
            &pair.served,
            &repository,
            d,
            NodeFlags::DirtyAdd,
            NodeExtFlags::NoFlags,
        )
        .await;
        let held = record(&pair.served, &repository, d).await;
        let mut scratch = ClearScratch::default();
        let against = ClearAgainst {
            state: &pair.committed,
            repository: &repository,
            node: pair.committed_d,
        };
        let moved = pair
            .served
            .served()
            .node_clear(
                &repository,
                d,
                ServedClear {
                    place: (held.parent, hash_string("elsewhere")),
                    against,
                    spelled: against,
                    scratch: &mut scratch,
                },
                async {},
            )
            .await
            .expect("the clear runs");
        assert_eq!(moved, ClearOutcome::Moved);
        assert!(record(&pair.served, &repository, d).await.is_dirty_add());

        let served = pair.served.clone();
        let detached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let landed = detached.clone();
        let cleared = settle_against(
            &pair,
            &repository,
            d,
            (pair.committed_d, pair.committed_d),
            async {
                let removed = served
                    .served()
                    .node_detach(repository.clone(), g, Detach::Always)
                    .await
                    .expect("the detach");
                landed.store(removed, std::sync::atomic::Ordering::Relaxed);
            },
        )
        .await;
        assert!(detached.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(cleared, ClearOutcome::Kept);
        assert!(is_dirty(&pair.served, &repository, d).await);
        assert!(!clear_pending(&pair.served, &repository, d).await);
    })
    .await;
}

/// A clear against a committed node that is detached or no directory is [`ClearOutcome::Kept`]
/// before the first phase: the directory keeps its action, as nothing committed matches it, and
/// takes no pending clear.
#[tokio::test]
async fn a_served_clear_against_a_detached_or_no_directory_keeps_the_directory_s_action() {
    with_repository(|repository| async move {
        let pair = committed_pair(&repository).await;
        let [d, ..] = pair.nodes;
        mark(
            &pair.served,
            &repository,
            d,
            NodeFlags::DirtyAdd,
            NodeExtFlags::NoFlags,
        )
        .await;
        let file = add(
            &pair.committed,
            &repository,
            ROOT_NODE,
            holding("x", true, 5),
            "x",
        )
        .await;
        let detached = add(
            &pair.committed,
            &repository,
            ROOT_NODE,
            holding("y", false, 6),
            "y",
        )
        .await;
        assert!(
            pair.committed
                .node_detach(repository.clone(), detached, Detach::Always)
                .await
                .expect("detaching y")
        );

        for against in [file, detached] {
            let cleared = settle_against(&pair, &repository, d, (against, against), async {}).await;
            assert_eq!(cleared, ClearOutcome::Kept, "against {against}");
            let held = record(&pair.served, &repository, d).await;
            assert!(
                held.is_dirty_add(),
                "against {against}: the add action stays"
            );
            assert!(
                !clear_pending(&pair.served, &repository, d).await,
                "against {against}"
            );
        }
    })
    .await;
}

/// Same-hash siblings compare as a multiset: each committed child takes the first unmatched
/// served child equal to it, so two names differing in case alone clear where both sides hold
/// both with the same contents, and keep where one content differs.
#[tokio::test]
async fn a_served_clear_counts_same_hash_siblings_as_a_multiset() {
    with_repository(|repository| async move {
        for (committed_seed, clears) in [(2, true), (9, false)] {
            let pair = committed_pair(&repository).await;
            let [d, ..] = pair.nodes;
            add(&pair.served, &repository, d, holding("F", true, 2), "F").await;
            add(
                &pair.committed,
                &repository,
                pair.committed_d,
                holding("F", true, committed_seed),
                "F",
            )
            .await;

            let cleared = settle_against(
                &pair,
                &repository,
                d,
                (pair.committed_d, pair.committed_d),
                async {},
            )
            .await;

            let expected = if clears {
                ClearOutcome::Cleared
            } else {
                ClearOutcome::Kept
            };
            assert_eq!(cleared, expected, "committed F holding {committed_seed}");
        }
    })
    .await;
}

/// Files by name, each with the seed of its content ([`holding`]).
type Files<'a> = &'a [(&'a str, u64)];

/// Directory `x` below the root of `state`, with directory `fill`: the files `before` added to `x`
/// first, then files of `fill` up to the last slot of block 0, then the files `after`, in block 1,
/// so the chain of `x`, newest first, runs from block 1 into block 0.
async fn across_blocks(
    state: &State,
    repository: &Arc<RepositoryContext>,
    (before, after): (Files<'_>, Files<'_>),
) -> NodeID {
    let x = add(state, repository, ROOT_NODE, holding("x", false, 9), "x").await;
    let fill = add(state, repository, ROOT_NODE, directory("fill"), "fill").await;
    for (name, seed) in before {
        let child = add(state, repository, x, holding(name, true, *seed), name).await;
        assert_eq!(NodeBlock::index(child), 0, "'{name}' lies in block 0");
    }
    for index in 0.. {
        let name = format!("fill{index}");
        let filler = add(state, repository, fill, file(&name), &name).await;
        assert_eq!(NodeBlock::index(filler), 0, "the fillers lie in block 0");
        if Node::index(filler) == BLOCK_NODE_COUNT - 1 {
            break;
        }
    }
    for (name, seed) in after {
        let child = add(state, repository, x, holding(name, true, *seed), name).await;
        assert_eq!(NodeBlock::index(child), 1, "'{name}' lies in block 1");
    }
    x
}

/// A settle's clear of directory `x` of `served`, marked dirty first, against directory `against`
/// of `committed`.
async fn settle_across(
    (served, committed): (&Arc<State>, &Arc<State>),
    repository: &Arc<RepositoryContext>,
    (x, against): (NodeID, NodeID),
) -> ClearOutcome {
    served
        .served()
        .node_mark(repository, x)
        .await
        .expect("the mark");
    let pair = Committed {
        served: served.clone(),
        committed: committed.clone(),
        nodes: [x; 4],
        committed_d: against,
    };
    settle_against(&pair, repository, x, (against, against), async {}).await
}

/// The clear compares children whose chains cross a block boundary, on either side or on both:
/// equal children clear wherever each side holds them; a committed child the served directory
/// lacks, before the boundary or past it, keeps the directory, as does a served child past the
/// boundary the committed directory lacks: neither walk stops where its chain leaves a block.
#[tokio::test]
async fn a_served_clear_compares_children_across_blocks() {
    with_repository(|repository| async move {
        let (a, b, c) = (("a", 1), ("b", 2), ("c", 3));
        let cases: [(_, _, bool); 6] = [
            // Both chains crossing.
            ((&[a][..], &[b, c][..]), (&[a][..], &[b, c][..]), true),
            // Only the committed chain crossing, then only the served one.
            ((&[a, b, c][..], &[][..]), (&[a][..], &[b, c][..]), true),
            ((&[a][..], &[b, c][..]), (&[a, b, c][..], &[][..]), true),
            // A committed child missing: the one past the boundary, then one before it.
            ((&[][..], &[b, c][..]), (&[a][..], &[b, c][..]), false),
            ((&[a][..], &[b][..]), (&[a][..], &[b, c][..]), false),
            // A served child past the boundary the committed directory lacks.
            ((&[a][..], &[b, c][..]), (&[][..], &[b, c][..]), false),
        ];
        for (held, stored, clears) in cases {
            let (served, committed) = (State::new(), State::new());
            let x = across_blocks(&served, &repository, held).await;
            let against = across_blocks(&committed, &repository, stored).await;
            let cleared = settle_across((&served, &committed), &repository, (x, against)).await;
            assert_eq!(
                cleared == ClearOutcome::Cleared,
                clears,
                "served {held:?} against {stored:?}: {cleared:?}"
            );
        }
    })
    .await;
}

/// The clear reads a committed State loaded from the store, loading its blocks as its walk reaches
/// them: equal children clear; a committed child in a block not yet in memory that the served
/// directory lacks, or holds with other content, keeps the directory.
#[tokio::test]
async fn a_served_clear_compares_children_against_a_reloaded_committed_state() {
    with_repository(|repository| async move {
        let (a, b, c) = (("a", 1), ("b", 2), ("c", 3));
        let cases: [(_, bool); 4] = [
            ((&[a][..], &[b, c][..]), true),
            // Lacking `c`, in block 1, at the head of the committed chain.
            ((&[a][..], &[b][..]), false),
            // Holding `c` with other content.
            ((&[a][..], &[b, ("c", 4)][..]), false),
            // Lacking `a`, in block 0, past the boundary.
            ((&[][..], &[b, c][..]), false),
        ];
        for (held, clears) in cases {
            let built = State::new();
            let against = across_blocks(&built, &repository, (&[a], &[b, c])).await;
            let committed = reload(&built, &repository).await;
            assert!(
                committed.resident_block(0).is_none() && committed.resident_block(1).is_none(),
                "the load reads no block"
            );
            let served = State::new();
            let x = across_blocks(&served, &repository, held).await;
            let cleared = settle_across((&served, &committed), &repository, (x, against)).await;
            assert_eq!(
                cleared == ClearOutcome::Cleared,
                clears,
                "served {held:?}: {cleared:?}"
            );
            assert!(
                committed.resident_block(1).is_some(),
                "the comparison loaded the committed chain's head block"
            );
        }
    })
    .await;
}

/// A link child compares by its kind and the revision it pins, its content hash: a committed link
/// pinning the same revision clears; one pinning another revision, or a directory of the link's
/// name and content in its place, keeps the directory.
#[tokio::test]
async fn a_served_clear_compares_a_link_child_by_its_kind_and_revision() {
    with_repository(|repository| async move {
        let content = |seed: u8| lore_base::types::Hash::from([seed; 32]);
        let link = |seed: u8| Node {
            flags: NodeFlags::Link.bits(),
            name_hash: hash_string("l"),
            address: Address {
                hash: content(seed),
                ..Address::default()
            },
            child: 5,
            ..Default::default()
        };
        let mut in_place = directory("l");
        in_place.address.hash = content(7);
        for (stored, clears) in [(link(7), true), (link(8), false), (in_place, false)] {
            let pair = committed_pair(&repository).await;
            let [d, ..] = pair.nodes;
            add(&pair.served, &repository, d, link(7), "l").await;
            add(&pair.committed, &repository, pair.committed_d, stored, "l").await;
            let cleared = settle_against(
                &pair,
                &repository,
                d,
                (pair.committed_d, pair.committed_d),
                async {},
            )
            .await;
            assert_eq!(
                cleared == ClearOutcome::Cleared,
                clears,
                "against {stored:?}: {cleared:?}"
            );
        }
    })
    .await;
}
