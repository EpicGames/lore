// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Tests for a directory's chain marks and chain log: what a search since a listing's mark
//! answers, and the patches and counts each chain change writes to the directory's log.

use std::cell::Cell;
use std::sync::Arc;

use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::AddedSince;
use lore_revision::state::AttachOutcome;
use lore_revision::state::ChainMark;
use lore_revision::state::REATTACH_CHANGES;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

use super::with_state;
use crate::node::log_state;

/// Adds a node of `flags` named `name` under `parent`.
async fn add_node(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    parent: NodeID,
    name: &str,
    flags: NodeFlags,
) -> NodeID {
    let node = Node {
        flags: flags.bits(),
        name_hash: hash_string(name),
        ..Default::default()
    };
    super::add(state, repository, parent, node, name).await
}

/// The mark of `parent`'s chain as a listing of it begins now.
async fn mark(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    parent: NodeID,
) -> ChainMark {
    state
        .node_children_with_name_hash_marked(repository.clone(), parent)
        .await
        .expect("Failed to list the children")
        .1
        .expect("a directory's listing is marked")
}

/// The search of the marked directory's children for `name` since `mark`.
async fn since(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    mark: &ChainMark,
    name: &str,
) -> AddedSince {
    state
        .find_subnode_added_since(repository.clone(), mark, hash_string(name))
        .await
        .expect("Failed to search the children added since")
}

/// A child is prepended, so the search for what was linked in since a head
/// was seen covers everything ahead of that head and stops there: it finds
/// the new children and never the ones the caller already holds.
#[tokio::test]
async fn a_search_since_a_head_covers_what_was_linked_in_ahead_of_it() {
    with_state(|repository, state| async move {
        add_node(&repository, &state, ROOT_NODE, "held", NodeFlags::File).await;
        let listed = mark(&repository, &state, ROOT_NODE).await;
        let added = add_node(&repository, &state, ROOT_NODE, "added", NodeFlags::File).await;
        assert!(!listed.reattached(), "an add shows no reattach");

        assert_eq!(
            since(&repository, &state, &listed, "added").await,
            AddedSince::Found(added),
            "a child linked in after the head was seen is ahead of it"
        );
        assert_eq!(
            since(&repository, &state, &listed, "held").await,
            AddedSince::Absent,
            "the head itself is where the search stops, so what it holds is not reported"
        );
        assert_eq!(
            since(&repository, &state, &listed, "never").await,
            AddedSince::Absent,
            "a name no child carries is not reported"
        );
    })
    .await;
}

/// A listing of an empty directory marks a chain with no head: everything is linked in
/// since, so the search covers the whole chain and a miss stands.
#[tokio::test]
async fn a_search_since_an_empty_listing_covers_the_whole_chain() {
    with_state(|repository, state| async move {
        let listed = mark(&repository, &state, ROOT_NODE).await;
        let added = add_node(&repository, &state, ROOT_NODE, "added", NodeFlags::File).await;
        assert_eq!(
            since(&repository, &state, &listed, "added").await,
            AddedSince::Found(added)
        );
        assert_eq!(
            since(&repository, &state, &listed, "never").await,
            AddedSince::Absent
        );
    })
    .await;
}

/// A detached head leaves the chain, so a search since it never meets it and walks the
/// whole chain, whose answer stands: with no reattach since the mark, nothing passed
/// over a node.
#[tokio::test]
async fn a_search_since_a_detached_head_walks_the_whole_chain() {
    with_state(|repository, state| async move {
        let kept = add_node(&repository, &state, ROOT_NODE, "kept", NodeFlags::File).await;
        let head = add_node(&repository, &state, ROOT_NODE, "head", NodeFlags::File).await;
        let listed = mark(&repository, &state, ROOT_NODE).await;
        add_node(&repository, &state, ROOT_NODE, "added", NodeFlags::File).await;
        state
            .node_detach(
                repository.clone(),
                head,
                lore_revision::state::Detach::Always,
            )
            .await
            .expect("detach");
        assert!(!listed.reattached(), "a detach shows no reattach");
        assert_eq!(
            since(&repository, &state, &listed, "never").await,
            AddedSince::Absent
        );
        assert_eq!(
            since(&repository, &state, &listed, "kept").await,
            AddedSince::Found(kept),
            "the walk passes where the head stood"
        );
    })
    .await;
}

/// A walk covering the whole chain, as a search since an empty listing's mark makes, answers
/// a miss from itself where no reattach landed during it, though one moved the count since
/// the mark.
#[tokio::test]
async fn a_search_covering_the_whole_chain_answers_a_miss_after_an_earlier_reattach() {
    with_state(|repository, state| async move {
        let listed = mark(&repository, &state, ROOT_NODE).await;
        let renamed = add_node(&repository, &state, ROOT_NODE, "added", NodeFlags::File).await;
        state
            .node_reattach(repository.clone(), renamed, ROOT_NODE, "renamed")
            .await
            .expect("the rename");
        assert!(
            listed.reattached(),
            "the rename moved the count since the mark"
        );
        assert_eq!(
            since(&repository, &state, &listed, "never").await,
            AddedSince::Absent
        );
        assert_eq!(
            since(&repository, &state, &listed, "renamed").await,
            AddedSince::Found(renamed)
        );
    })
    .await;
}

/// A holder counting its own prepends in its mark keeps the mark fresh past
/// `REATTACH_CHANGES` of them, as the stage counts its adds into a directory it listed:
/// a miss stands and no reattach shows, where the uncounted adds would have made it stale.
/// A reattach after them still shows.
#[tokio::test]
async fn own_prepends_counted_in_a_mark_never_make_it_stale() {
    with_state(|repository, state| async move {
        let kept = add_node(&repository, &state, ROOT_NODE, "kept", NodeFlags::File).await;
        let mut listed = mark(&repository, &state, ROOT_NODE).await;
        for index in 0..REATTACH_CHANGES {
            add_node(
                &repository,
                &state,
                ROOT_NODE,
                &format!("own-{index}"),
                NodeFlags::File,
            )
            .await;
            listed.count_own_prepend();
        }
        assert!(!listed.reattached(), "the counted adds show no reattach");
        assert_eq!(
            since(&repository, &state, &listed, "never").await,
            AddedSince::Absent
        );
        state
            .node_reattach(repository.clone(), kept, ROOT_NODE, "renamed")
            .await
            .expect("the rename");
        assert!(listed.reattached(), "a reattach after them shows");
        assert_eq!(
            since(&repository, &state, &listed, "never").await,
            AddedSince::Stale
        );
    })
    .await;
}

/// H9: a reattach puts a node at the head, the head seen among them, which leaves what was
/// prepended since behind it. A search since that head finds what was added before the
/// reattach, or answers that it cannot tell; it never answers that nothing was added.
#[tokio::test]
async fn a_search_since_a_head_a_rename_put_back_in_front_misses_nothing_added() {
    with_state(|repository, state| async move {
        add_node(&repository, &state, ROOT_NODE, "kept", NodeFlags::File).await;
        let head = add_node(&repository, &state, ROOT_NODE, "head", NodeFlags::File).await;
        let listed = mark(&repository, &state, ROOT_NODE).await;
        let added = add_node(&repository, &state, ROOT_NODE, "added", NodeFlags::File).await;
        state
            .node_reattach(repository.clone(), head, ROOT_NODE, "renamed")
            .await
            .expect("the rename");
        let answer = since(&repository, &state, &listed, "added").await;
        assert!(
            matches!(answer, AddedSince::Found(found) if found == added)
                || answer == AddedSince::Stale,
            "a child added before the rename put the head back in front is found or the \
             search is stale: {answer:?}"
        );
    })
    .await;
}

/// A reattach since the mark makes a miss stale, not a find: a child the walk meets
/// carrying the name is the answer, wherever the reattach put what it moved.
#[tokio::test]
async fn a_search_since_a_mark_a_reattach_moved_finds_what_it_meets() {
    with_state(|repository, state| async move {
        let moved = add_node(&repository, &state, ROOT_NODE, "moved", NodeFlags::File).await;
        add_node(&repository, &state, ROOT_NODE, "head", NodeFlags::File).await;
        let listed = mark(&repository, &state, ROOT_NODE).await;
        let added = add_node(&repository, &state, ROOT_NODE, "added", NodeFlags::File).await;
        state
            .node_reattach(repository.clone(), moved, ROOT_NODE, "renamed")
            .await
            .expect("the rename");
        assert_eq!(
            since(&repository, &state, &listed, "added").await,
            AddedSince::Found(added),
            "the walk meets the child added since"
        );
        assert_eq!(
            since(&repository, &state, &listed, "renamed").await,
            AddedSince::Found(moved),
            "the walk meets the node the reattach put at the head"
        );
        assert_eq!(
            since(&repository, &state, &listed, "never").await,
            AddedSince::Stale,
            "a miss after a reattach cannot tell what the reattach left behind the head"
        );
    })
    .await;
}

/// A mark answers for the State it was taken of: a search of another State with it is
/// stale, though that State holds the same directory, its count and its head alike.
#[tokio::test]
async fn a_search_with_another_states_mark_is_stale() {
    with_state(|repository, state| async move {
        add_node(&repository, &state, ROOT_NODE, "head", NodeFlags::File).await;
        let listed = mark(&repository, &state, ROOT_NODE).await;
        let other = State::new();
        add_node(&repository, &other, ROOT_NODE, "head", NodeFlags::File).await;
        add_node(&repository, &other, ROOT_NODE, "added", NodeFlags::File).await;
        assert_eq!(
            since(&repository, &other, &listed, "never").await,
            AddedSince::Stale
        );
    })
    .await;
}

/// Children linked in since a head was seen that do not all fit in the block
/// that head sits in: the search crosses into the block it started from and
/// only then reaches the head it stops at.
#[tokio::test]
async fn a_search_since_a_head_crosses_blocks_before_it_reaches_the_head() {
    with_state(|repository, state| async move {
        for index in 0..BLOCK_NODE_COUNT - 64 {
            add_node(
                &repository,
                &state,
                ROOT_NODE,
                &format!("held-{index:05}"),
                NodeFlags::File,
            )
            .await;
        }
        let listed = mark(&repository, &state, ROOT_NODE).await;
        let mut added = Vec::new();
        for index in 0..128 {
            added.push(
                add_node(
                    &repository,
                    &state,
                    ROOT_NODE,
                    &format!("added-{index:05}"),
                    NodeFlags::File,
                )
                .await,
            );
        }
        assert!(
            state.block_count() > 1,
            "the children added since must reach past the block the head sits in"
        );

        let since_head = |name: String| {
            let (repository, state, listed) = (&repository, &state, &listed);
            async move { since(repository, state, listed, &name).await }
        };

        assert_eq!(
            since_head("added-00127".to_string()).await,
            AddedSince::Found(*added.last().expect("added")),
            "the child linked in last heads the chain"
        );
        assert_eq!(
            since_head("added-00000".to_string()).await,
            AddedSince::Found(*added.first().expect("added")),
            "a child linked in before the block boundary is still reached"
        );
        assert_eq!(
            since_head(format!("held-{:05}", BLOCK_NODE_COUNT - 65)).await,
            AddedSince::Absent,
            "the head is where the search stops"
        );
        assert_eq!(
            since_head("held-00000".to_string()).await,
            AddedSince::Absent,
            "nothing behind the head is reached"
        );
    })
    .await;
}

/// Installs `directory`'s chain log as a listing build does, and answers its block and
/// the mark the install read.
async fn listed(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    directory: NodeID,
) -> (Arc<NodeBlock>, LogMark) {
    let block = state
        .block(repository.clone(), NodeBlock::index(directory))
        .await
        .expect("Failed to load the directory's block");
    let (mark, _) = block.watch_chain(Node::index(directory), None);
    (block, mark)
}

/// `directory`'s patches from sequence `from` to the log's head, oldest first.
fn logged(block: &NodeBlock, directory: NodeID, from: u32) -> Vec<(PatchKind, u64, NodeID)> {
    let reader = block.read();
    let log = reader
        .chain_log(Node::index(directory))
        .expect("the directory has a chain log");
    (from..log.seq())
        .map(|sequence| {
            let patch = log
                .patch(sequence)
                .unwrap_or_else(|| panic!("patch {sequence} is not in the log's window"));
            (patch.kind, patch.name_hash, patch.node)
        })
        .collect()
}

/// `directory`'s patches from sequence `from` to the log's head, oldest first, each with its
/// node and the block its weak reference names, where that block is still in memory.
fn logged_blocks(
    block: &NodeBlock,
    directory: NodeID,
    from: u32,
) -> Vec<(NodeID, Option<Arc<NodeBlock>>)> {
    let reader = block.read();
    let log = reader
        .chain_log(Node::index(directory))
        .expect("the directory has a chain log");
    (from..log.seq())
        .map(|sequence| {
            let patch = log
                .patch(sequence)
                .unwrap_or_else(|| panic!("patch {sequence} is not in the log's window"));
            (patch.node, patch.block.upgrade())
        })
        .collect()
}

/// Each patch names the block that holds its node, which a VFS listing reads an attached node's
/// record from with no State lock: here for children in another block than their directory, an
/// add, a move in and a move in over a holder, and each detach.
#[tokio::test]
async fn a_patch_names_the_block_holding_its_node() {
    with_state(|repository, state| async move {
        let dir = add_node(&repository, &state, ROOT_NODE, "dir", NodeFlags::NoFlags).await;
        let other = add_node(&repository, &state, ROOT_NODE, "other", NodeFlags::NoFlags).await;
        // Fills the directory's block, so every node below lands in another.
        for index in 0..BLOCK_NODE_COUNT {
            add_node(
                &repository,
                &state,
                other,
                &format!("f{index}"),
                NodeFlags::File,
            )
            .await;
        }
        let holder = add_node(&repository, &state, dir, "held.txt", NodeFlags::File).await;
        let mover = add_node(&repository, &state, other, "m.txt", NodeFlags::File).await;
        let replacer = add_node(&repository, &state, other, "r.txt", NodeFlags::File).await;
        let (block, mark) = listed(&repository, &state, dir).await;

        let added = add_node(&repository, &state, dir, "added.txt", NodeFlags::File).await;
        state
            .node_move_staged(
                repository.clone(),
                mover,
                dir,
                ("moved.txt", hash_string("moved.txt")),
                &state.moves_permit().await.expect("the moves permit"),
                false,
            )
            .await
            .expect("Failed to move the child in");
        state
            .node_reattach_replacing(repository.clone(), replacer, dir, "held.txt", holder)
            .await
            .expect("Failed to move over the holder");

        let patches = logged_blocks(&block, dir, mark.seq);
        assert_eq!(
            patches.iter().map(|(node, _)| *node).collect::<Vec<_>>(),
            [added, mover, holder, replacer],
            "the add, the move in, the holder's detach and the replacer's attach"
        );
        for (node, named) in &patches {
            assert_ne!(
                NodeBlock::index(*node),
                NodeBlock::index(dir),
                "node {node} lies in another block than its directory"
            );
            let holding = state
                .block(repository.clone(), NodeBlock::index(*node))
                .await
                .expect("Failed to load the node's block");
            assert!(
                named
                    .as_ref()
                    .is_some_and(|named| Arc::ptr_eq(named, &holding)),
                "node {node}'s patch names the block holding it"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn a_create_appends_an_attach_patch_under_the_directory_log() {
    with_state(|repository, state| async move {
        let dir = add_node(&repository, &state, ROOT_NODE, "dir", NodeFlags::NoFlags).await;
        let (block, mark) = listed(&repository, &state, dir).await;
        let changes = block.chain_changes(Node::index(dir));

        let child = add_node(&repository, &state, dir, "a.txt", NodeFlags::File).await;

        assert_eq!(
            logged(&block, dir, mark.seq),
            vec![(PatchKind::Attached, hash_string("a.txt"), child)],
            "a create appends exactly its attach to the directory's log"
        );
        assert_eq!(
            block.chain_changes(Node::index(dir)),
            changes + 1,
            "the chain change count keeps its weight of 1 per prepend"
        );
        assert_eq!(
            log_state(&block, Node::index(dir)).2,
            mark.detaches,
            "an attach is no detach"
        );
    })
    .await;
}

#[tokio::test]
async fn an_unlink_appends_a_detach_patch_and_counts_it() {
    with_state(|repository, state| async move {
        let dir = add_node(&repository, &state, ROOT_NODE, "dir", NodeFlags::NoFlags).await;
        let child = add_node(&repository, &state, dir, "gone.txt", NodeFlags::File).await;
        let (block, mark) = listed(&repository, &state, dir).await;
        let changes = block.chain_changes(Node::index(dir));

        state
            .node_detach(
                repository.clone(),
                child,
                lore_revision::state::Detach::Always,
            )
            .await
            .expect("Failed to detach the child");

        assert_eq!(
            logged(&block, dir, mark.seq),
            vec![(PatchKind::Detached, hash_string("gone.txt"), child)],
            "an unlink appends exactly its detach to the directory's log"
        );
        assert_eq!(
            log_state(&block, Node::index(dir)).2,
            mark.detaches + 1,
            "the log counts the detach"
        );
        assert_eq!(
            block.chain_changes(Node::index(dir)),
            changes + 1,
            "the chain change count keeps its weight of 1 per detach"
        );
    })
    .await;
}

#[tokio::test]
async fn a_rename_over_a_holder_appends_detach_then_attach() {
    with_state(|repository, state| async move {
        let dir = add_node(&repository, &state, ROOT_NODE, "dir", NodeFlags::NoFlags).await;
        let holder = add_node(&repository, &state, dir, "target", NodeFlags::File).await;
        let mover = add_node(&repository, &state, dir, "temp", NodeFlags::File).await;
        let (block, mark) = listed(&repository, &state, dir).await;

        let outcome = state
            .node_reattach_replacing(repository.clone(), mover, dir, "target", holder)
            .await
            .expect("Failed to rename over the holder");
        assert!(matches!(outcome, AttachOutcome::Attached(node) if node == mover));

        let patches = logged(&block, dir, mark.seq);
        assert!(
            patches.contains(&(PatchKind::Detached, hash_string("temp"), mover)),
            "the mover leaves its old name: {patches:?}"
        );
        let target = hash_string("target");
        let at = patches
            .iter()
            .position(|&patch| patch == (PatchKind::Detached, target, holder))
            .unwrap_or_else(|| panic!("the holder's detach is logged: {patches:?}"));
        assert_eq!(
            patches.get(at + 1),
            Some(&(PatchKind::Attached, target, mover)),
            "the mover's attach follows the holder's detach at the next sequence: {patches:?}"
        );
        assert_eq!(patches.len(), 3, "nothing else is logged: {patches:?}");
        assert_eq!(
            log_state(&block, Node::index(dir)).2,
            mark.detaches + 2,
            "the holder's and the mover's detaches are counted"
        );
    })
    .await;
}

#[tokio::test]
async fn a_cross_directory_replace_logs_the_holder_in_its_own_directory() {
    with_state(|repository, state| async move {
        let from = add_node(&repository, &state, ROOT_NODE, "from", NodeFlags::NoFlags).await;
        let to = add_node(&repository, &state, ROOT_NODE, "to", NodeFlags::NoFlags).await;
        let mover = add_node(&repository, &state, from, "m.txt", NodeFlags::File).await;
        let holder = add_node(&repository, &state, to, "h.txt", NodeFlags::File).await;
        let (from_block, from_mark) = listed(&repository, &state, from).await;
        let (to_block, to_mark) = listed(&repository, &state, to).await;

        state
            .node_reattach_replacing(repository.clone(), mover, to, "h.txt", holder)
            .await
            .expect("Failed to move over the holder");

        assert_eq!(
            logged(&from_block, from, from_mark.seq),
            vec![(PatchKind::Detached, hash_string("m.txt"), mover)],
            "the mover's old directory logs only its departure"
        );
        assert_eq!(
            logged(&to_block, to, to_mark.seq),
            vec![
                (PatchKind::Detached, hash_string("h.txt"), holder),
                (PatchKind::Attached, hash_string("h.txt"), mover),
            ],
            "the holder's directory logs the holder's detach, then the mover's attach"
        );
        assert_eq!(
            log_state(&from_block, Node::index(from)).2,
            from_mark.detaches + 1
        );
        assert_eq!(
            log_state(&to_block, Node::index(to)).2,
            to_mark.detaches + 1
        );
    })
    .await;
}

/// A move to another directory under another name logs the node's departure under its old
/// name in the directory it left, and its arrival under its new name in the one it joined;
/// the old chain's count moves by a reattach, the new one's by one.
#[tokio::test]
async fn a_cross_directory_move_and_rename_logs_each_name_in_its_directory() {
    with_state(|repository, state| async move {
        let from = add_node(&repository, &state, ROOT_NODE, "from", NodeFlags::NoFlags).await;
        let to = add_node(&repository, &state, ROOT_NODE, "to", NodeFlags::NoFlags).await;
        let mover = add_node(&repository, &state, from, "m.txt", NodeFlags::File).await;
        let (from_block, from_mark) = listed(&repository, &state, from).await;
        let (to_block, to_mark) = listed(&repository, &state, to).await;
        let from_changes = from_block.chain_changes(Node::index(from));
        let to_changes = to_block.chain_changes(Node::index(to));

        let left = state
            .node_move(
                repository.clone(),
                mover,
                to,
                lore_revision::state::NameChange::Rename("n.txt"),
                lore_revision::state::Mark::staged(NodeFlags::StagedMove, true),
            )
            .await
            .expect("Failed to move and rename");
        assert_eq!(left, from, "the move answers the directory it left");

        assert_eq!(
            logged(&from_block, from, from_mark.seq),
            vec![(PatchKind::Detached, hash_string("m.txt"), mover)],
            "the old directory logs the departure under the old name"
        );
        assert_eq!(
            logged(&to_block, to, to_mark.seq),
            vec![(PatchKind::Attached, hash_string("n.txt"), mover)],
            "the new directory logs the arrival under the new name"
        );
        assert_eq!(
            log_state(&from_block, Node::index(from)).2,
            from_mark.detaches + 1
        );
        assert_eq!(log_state(&to_block, Node::index(to)).2, to_mark.detaches);
        assert_eq!(
            from_block.chain_changes(Node::index(from)),
            from_changes.wrapping_add(REATTACH_CHANGES)
        );
        assert_eq!(
            to_block.chain_changes(Node::index(to)),
            to_changes.wrapping_add(1)
        );
    })
    .await;
}

/// A replace across directories moves the holder's directory's chain change count by the
/// reattach weight, plus the mover's prepend, so a reader holding a count from before reads
/// the directory again, as after a reattach out of it.
#[tokio::test]
async fn a_cross_directory_replace_moves_the_holders_directory_count_by_the_reattach_weight() {
    with_state(|repository, state| async move {
        let from = add_node(&repository, &state, ROOT_NODE, "from", NodeFlags::NoFlags).await;
        let to = add_node(&repository, &state, ROOT_NODE, "to", NodeFlags::NoFlags).await;
        let mover = add_node(&repository, &state, from, "m.txt", NodeFlags::File).await;
        let holder = add_node(&repository, &state, to, "h.txt", NodeFlags::File).await;
        let to_block = state
            .block(repository.clone(), NodeBlock::index(to))
            .await
            .expect("Failed to load the holder's directory's block");
        let before = to_block.chain_changes(Node::index(to));

        state
            .node_reattach_replacing(repository.clone(), mover, to, "h.txt", holder)
            .await
            .expect("Failed to move over the holder");

        assert_eq!(
            to_block.chain_changes(Node::index(to)),
            before.wrapping_add(REATTACH_CHANGES).wrapping_add(1)
        );
    })
    .await;
}

/// A replace whose mover follows the holder in the chain leaves one chain: the mover's link
/// is the holder's sibling pointer, which the holder's removal moves to the holder's link.
#[tokio::test]
async fn a_rename_over_the_holder_just_ahead_of_it_in_the_chain_leaves_one_chain() {
    with_state(|repository, state| async move {
        let dir = add_node(&repository, &state, ROOT_NODE, "dir", NodeFlags::NoFlags).await;
        let last = add_node(&repository, &state, dir, "last", NodeFlags::File).await;
        let mover = add_node(&repository, &state, dir, "temp", NodeFlags::File).await;
        let holder = add_node(&repository, &state, dir, "target", NodeFlags::File).await;
        assert_eq!(
            state
                .node_children(repository.clone(), dir)
                .await
                .expect("Failed to walk the chain"),
            vec![holder, mover, last],
            "newest first, the mover just behind the holder"
        );

        let outcome = state
            .node_reattach_replacing(repository.clone(), mover, dir, "target", holder)
            .await
            .expect("Failed to rename over the holder");
        assert!(matches!(outcome, AttachOutcome::Attached(node) if node == mover));

        let children = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            state.node_children(repository.clone(), dir),
        )
        .await
        .expect("the walk of the chain ends")
        .expect("Failed to walk the chain");
        assert_eq!(children, vec![mover, last]);
        assert_eq!(
            state
                .find_subnode(repository.clone(), dir, hash_string("target"))
                .await
                .expect("Failed to find the name"),
            mover
        );
    })
    .await;
}

#[tokio::test]
async fn a_rename_in_place_bumps_respells_and_a_replace_does_not() {
    with_state(|repository, state| async move {
        let dir = add_node(&repository, &state, ROOT_NODE, "dir", NodeFlags::NoFlags).await;
        let file = add_node(&repository, &state, dir, "file.txt", NodeFlags::File).await;
        let other = add_node(&repository, &state, dir, "other.txt", NodeFlags::File).await;
        let (block, mark) = listed(&repository, &state, dir).await;
        assert_eq!(
            hash_string("FILE.txt"),
            hash_string("file.txt"),
            "the test needs a respelling that keeps the hash"
        );

        state
            .node_reattach(repository.clone(), file, dir, "FILE.txt")
            .await
            .expect("Failed to respell in place");
        assert_eq!(
            log_state(&block, Node::index(dir)).3,
            mark.respells + 1,
            "a rename in place that keeps the hash is a respelling"
        );

        state
            .node_reattach_replacing(repository.clone(), file, dir, "other.txt", other)
            .await
            .expect("Failed to rename over the holder");
        assert_eq!(
            log_state(&block, Node::index(dir)).3,
            mark.respells + 1,
            "a replacing rename changes the hash and respells nothing"
        );
    })
    .await;
}

#[tokio::test]
async fn move_node_renaming_in_place_logs_detach_and_attach() {
    with_state(|repository, state| async move {
        let dir = add_node(&repository, &state, ROOT_NODE, "dir", NodeFlags::NoFlags).await;
        let child = add_node(&repository, &state, dir, "a.bin", NodeFlags::File).await;
        let (block, mark) = listed(&repository, &state, dir).await;
        let changes = block.chain_changes(Node::index(dir));

        state
            .node_move_staged(
                repository.clone(),
                child,
                dir,
                ("b.bin", hash_string("b.bin")),
                &state.moves_permit().await.expect("the moves permit"),
                false,
            )
            .await
            .expect("Failed to rename in place");
        assert_eq!(
            logged(&block, dir, mark.seq),
            vec![
                (PatchKind::Detached, hash_string("a.bin"), child),
                (PatchKind::Attached, hash_string("b.bin"), child),
            ],
            "a rename in place leaves the old name and takes the new one"
        );
        let (_, _, detaches, respells, _) = log_state(&block, Node::index(dir));
        assert_eq!(detaches, mark.detaches + 1);
        assert_eq!(respells, mark.respells, "a new hash is no respelling");
        assert_eq!(
            block.chain_changes(Node::index(dir)),
            changes.wrapping_add(REATTACH_CHANGES),
            "the chain change count keeps its reattach weight"
        );

        let before = log_state(&block, Node::index(dir)).0;
        state
            .node_move_staged(
                repository.clone(),
                child,
                dir,
                ("B.bin", hash_string("B.bin")),
                &state.moves_permit().await.expect("the moves permit"),
                false,
            )
            .await
            .expect("Failed to respell in place");
        assert_eq!(
            logged(&block, dir, before),
            vec![
                (PatchKind::Detached, hash_string("b.bin"), child),
                (PatchKind::Attached, hash_string("b.bin"), child),
            ]
        );
        assert_eq!(
            log_state(&block, Node::index(dir)).3,
            mark.respells + 1,
            "a rename keeping the hash is a respelling"
        );
    })
    .await;
}

/// A move to another directory moves the source's chain change count by the reattach weight,
/// and the destination's by one prepend.
#[tokio::test]
async fn move_node_to_another_directory_counts_the_reattach_weight_in_the_source() {
    with_state(|repository, state| async move {
        let source = add_node(&repository, &state, ROOT_NODE, "src", NodeFlags::NoFlags).await;
        let destination = add_node(&repository, &state, ROOT_NODE, "dst", NodeFlags::NoFlags).await;
        let child = add_node(&repository, &state, source, "child.bin", NodeFlags::File).await;
        let source_block = state
            .block(repository.clone(), NodeBlock::index(source))
            .await
            .expect("Failed to load the source's block");
        let destination_block = state
            .block(repository.clone(), NodeBlock::index(destination))
            .await
            .expect("Failed to load the destination's block");
        let source_before = source_block.chain_changes(Node::index(source));
        let destination_before = destination_block.chain_changes(Node::index(destination));

        state
            .node_move_staged(
                repository.clone(),
                child,
                destination,
                ("child.bin", hash_string("child.bin")),
                &state.moves_permit().await.expect("the moves permit"),
                false,
            )
            .await
            .expect("Failed to move the child");

        assert_eq!(
            source_block.chain_changes(Node::index(source)),
            source_before.wrapping_add(REATTACH_CHANGES)
        );
        assert_eq!(
            destination_block.chain_changes(Node::index(destination)),
            destination_before.wrapping_add(1)
        );
    })
    .await;
}

#[tokio::test]
async fn an_append_past_capacity_advances_floor() {
    with_state(|repository, state| async move {
        let dir = add_node(&repository, &state, ROOT_NODE, "dir", NodeFlags::NoFlags).await;
        let (block, mark) = listed(&repository, &state, dir).await;
        let capacity = log_state(&block, Node::index(dir)).4;
        assert_eq!(
            capacity, CHAIN_LOG_INITIAL,
            "a log installs at its initial size"
        );

        let mut children = Vec::new();
        for index in 0..capacity {
            children.push(
                add_node(
                    &repository,
                    &state,
                    dir,
                    &format!("f{index}"),
                    NodeFlags::File,
                )
                .await,
            );
        }
        assert_eq!(
            log_state(&block, Node::index(dir)).1,
            mark.seq,
            "a full log has overwritten nothing, so its floor stays"
        );

        let last = add_node(&repository, &state, dir, "overflow", NodeFlags::File).await;
        let (seq, floor, _, _, _) = log_state(&block, Node::index(dir));
        assert_eq!(seq, mark.seq + capacity as u32 + 1);
        assert_eq!(
            floor,
            mark.seq + 1,
            "the floor moves past the one patch overwritten"
        );
        let reader = block.read();
        let log = reader.chain_log(Node::index(dir)).expect("a chain log");
        assert!(
            log.patch(mark.seq).is_none(),
            "the overwritten patch is outside the window"
        );
        assert_eq!(log.patch(floor).map(|patch| patch.node), Some(children[1]));
        assert_eq!(log.patch(seq - 1).map(|patch| patch.node), Some(last));
        assert!(log.patch(seq).is_none(), "nothing is held at the head");
    })
    .await;
}

#[tokio::test]
async fn a_lost_install_race_keeps_the_winners_log() {
    with_state(|repository, state| async move {
        let dir = add_node(&repository, &state, ROOT_NODE, "dir", NodeFlags::NoFlags).await;
        let block = state
            .block(repository.clone(), NodeBlock::index(dir))
            .await
            .expect("Failed to load the directory's block");
        let index = Node::index(dir);
        let fired = Cell::new(0);
        let winner = Cell::new(None);
        let hook = || {
            fired.set(fired.get() + 1);
            let (mark, _) = block.watch_chain(index, None);
            let log = block
                .read()
                .chain_log(index)
                .map(|log| log as *const ChainLog as usize);
            winner.set(Some((mark, log)));
        };

        let (mark, record) = block.watch_chain(index, Some(&hook));

        assert_eq!(fired.get(), 1, "the hook fires once");
        let (winner_mark, winner_log) = winner.get().expect("the hook ran");
        assert_eq!(mark, winner_mark, "the loser answers the winner's mark");
        assert_eq!(
            block
                .read()
                .chain_log(index)
                .map(|log| log as *const ChainLog as usize),
            winner_log,
            "the winner's log is the one installed"
        );
        assert_eq!(
            block.read().chain_log_count(),
            1,
            "the block holds one log, the winner's"
        );
        assert_eq!(
            record.parent, ROOT_NODE,
            "the record read with the mark is the directory's"
        );
    })
    .await;
}

#[tokio::test]
async fn an_install_racing_another_directorys_install_keeps_both() {
    with_state(|repository, state| async move {
        let first = add_node(&repository, &state, ROOT_NODE, "first", NodeFlags::NoFlags).await;
        let second = add_node(&repository, &state, ROOT_NODE, "second", NodeFlags::NoFlags).await;
        let block = state
            .block(repository.clone(), NodeBlock::index(first))
            .await
            .expect("Failed to load the directories' block");
        assert!(
            Arc::ptr_eq(
                &block,
                &state
                    .block(repository.clone(), NodeBlock::index(second))
                    .await
                    .expect("block")
            ),
            "the test needs both directories in one block"
        );
        let fired = Cell::new(0);
        let hook = || {
            fired.set(fired.get() + 1);
            if fired.get() == 1 {
                block.watch_chain(Node::index(second), None);
            }
        };

        block.watch_chain(Node::index(first), Some(&hook));

        assert_eq!(
            fired.get(),
            2,
            "the install saw the slot count move and ran its gap again"
        );
        let reader = block.read();
        assert_eq!(reader.chain_log_count(), 2, "both directories keep a log");
        assert!(reader.chain_log(Node::index(first)).is_some());
        assert!(reader.chain_log(Node::index(second)).is_some());
    })
    .await;
}

#[tokio::test]
async fn a_directory_never_listed_has_no_log() {
    with_state(|repository, state| async move {
        let dir = add_node(&repository, &state, ROOT_NODE, "dir", NodeFlags::NoFlags).await;
        let child = add_node(&repository, &state, dir, "a.txt", NodeFlags::File).await;
        let other = add_node(&repository, &state, dir, "b.txt", NodeFlags::File).await;
        state
            .node_reattach(repository.clone(), child, dir, "c.txt")
            .await
            .expect("Failed to rename");
        state
            .node_move_staged(
                repository.clone(),
                child,
                dir,
                ("d.txt", hash_string("d.txt")),
                &state.moves_permit().await.expect("the moves permit"),
                false,
            )
            .await
            .expect("Failed to rename");
        state
            .node_detach(
                repository.clone(),
                other,
                lore_revision::state::Detach::Always,
            )
            .await
            .expect("Failed to detach");

        let block = state
            .block(repository.clone(), NodeBlock::index(dir))
            .await
            .expect("Failed to load the directory's block");
        let reader = block.read();
        assert!(
            reader.chain_log(Node::index(dir)).is_none(),
            "no write installs a log"
        );
        assert_eq!(reader.chain_log_count(), 0, "the block holds no log at all");
    })
    .await;
}

/// A directory freed empties its chain log, which then covers no position read before: a
/// directory or a build taking the slot again inherits none of its patches.
#[tokio::test]
async fn a_freed_directory_empties_its_log() {
    with_state(|repository, state| async move {
        let dir = add_node(&repository, &state, ROOT_NODE, "dir", NodeFlags::NoFlags).await;
        let (block, mark) = listed(&repository, &state, dir).await;
        add_node(&repository, &state, dir, "a.txt", NodeFlags::File).await;

        lore_revision::state::node_discard_fused(state.clone(), repository.clone(), dir)
            .await
            .expect("the discard");

        let reader = block.read();
        let log = reader
            .chain_log(Node::index(dir))
            .expect("the freed directory's slot keeps its log");
        assert!(
            log.holds_no_patch(),
            "the log holds no patch of the freed directory"
        );
        assert!(
            !log.covers(mark.seq),
            "a position read before the free is stale"
        );
    })
    .await;
}
