// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The node operations the bench drives, as one thin layer over `State`, so that the same
//! workloads run against each revision of the node interface.

use std::sync::Arc;

use lore_base::types::Address;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::AttachCheck;
use lore_revision::state::AttachOutcome;
use lore_revision::state::Built;
use lore_revision::state::Clear;
use lore_revision::state::ClearAgainst;
use lore_revision::state::ClearOutcome;
use lore_revision::state::ClearScratch;
use lore_revision::state::Content;
use lore_revision::state::Detach;
use lore_revision::state::Ext;
use lore_revision::state::IfUnchanged;
use lore_revision::state::Mark;
use lore_revision::state::ReattachCheck;
use lore_revision::state::Rewrite;
use lore_revision::state::ServedClear;
use lore_revision::state::State;
use lore_revision::state::TreeAttach;
use lore_revision::state::TreeBuild;
use lore_storage::hash::hash_string;

pub type Repo = Arc<RepositoryContext>;

#[inline(never)]
pub fn directory(name: &str) -> Node {
    Node {
        name_hash: hash_string(name),
        ..Default::default()
    }
}

#[inline(never)]
pub fn file(name: &str) -> Node {
    Node {
        flags: NodeFlags::File.bits(),
        name_hash: hash_string(name),
        ..Default::default()
    }
}

/// An add whose name the caller guarantees is free.
#[inline(never)]
pub async fn add(state: &State, repo: &Repo, parent: NodeID, node: Node, name: &str) -> NodeID {
    state
        .node_add(repo.clone(), parent, node, name)
        .await
        .expect("an add succeeds")
}

/// An attach refusing a name already held; the node where it attached.
#[inline(never)]
pub async fn attach(
    state: &State,
    repo: &Repo,
    parent: NodeID,
    node: Node,
    name: &str,
) -> Option<NodeID> {
    match state
        .node_attach(repo.clone(), parent, node, name)
        .await
        .expect("an attach answers")
    {
        AttachOutcome::Attached(id) => Some(id),
        AttachOutcome::Exists(_) => None,
    }
}

#[inline(never)]
pub async fn detach(state: &State, repo: &Repo, node: NodeID) {
    state
        .node_detach(repo.clone(), node, Detach::Always)
        .await
        .expect("a detach succeeds");
}

/// A checked move; true where it moved.
#[inline(never)]
pub async fn reattach(
    state: &State,
    repo: &Repo,
    node: NodeID,
    parent: NodeID,
    name: &str,
) -> bool {
    matches!(
        state
            .node_reattach(repo.clone(), node, parent, name)
            .await
            .expect("a reattach answers"),
        AttachOutcome::Attached(_)
    )
}

/// A move with no name check, recorded as a staged move, for a caller that serializes moves.
#[inline(never)]
pub async fn move_staged(
    state: &Arc<State>,
    repo: &Repo,
    node: NodeID,
    parent: NodeID,
    name: &str,
) {
    state
        .node_move_staged(
            repo.clone(),
            node,
            parent,
            (name, hash_string(name)),
            &state.moves_permit().await.expect("the moves permit"),
            false,
        )
        .await
        .expect("a move succeeds");
}

#[inline(never)]
pub async fn rewrite(state: &State, repo: &Repo, node: NodeID, size: u64) {
    state
        .node_rewrite(
            repo,
            node,
            &Content {
                mode: 0o644,
                size,
                address: Address::default(),
            },
            Rewrite::Plain,
        )
        .await
        .expect("a rewrite succeeds");
}

#[inline(never)]
pub async fn mark_dirty(state: &State, repo: &Repo, node: NodeID) {
    state
        .node_mark(
            repo,
            node,
            Mark::dirty(
                NodeFlags::DirtyModify,
                Ext::Set(NodeExtFlags::NoFlags),
                false,
            ),
        )
        .await
        .expect("a mark succeeds");
}

#[inline(never)]
pub async fn clear_dirty(state: &State, repo: &Repo, node: NodeID) {
    state
        .node_clear(repo, node, Clear::Dirty)
        .await
        .expect("a clear succeeds");
}

#[inline(never)]
pub async fn mark_staged(state: &State, repo: &Repo, node: NodeID) {
    state
        .node_mark(
            repo,
            node,
            Mark::staged_and_dirty(NodeFlags::StagedModify, NodeFlags::DirtyModify),
        )
        .await
        .expect("a staged mark succeeds");
}

/// The live children of `dir` a name-carrying walk yields.
#[inline(never)]
pub async fn walk(state: &Arc<State>, repo: &Repo, dir: NodeID) -> usize {
    let mut children = lore_revision::state::StateNodeChildrenWithNameIterator::new(
        state.clone(),
        repo.clone(),
        dir,
    )
    .await
    .expect("the walk starts");
    let mut count = 0;
    while let Some(item) = children.next().await.expect("the walk steps") {
        drop(item);
        count += 1;
    }
    count
}

/// A sweep of the State's detached slots, as the stage and the commits run one before they save.
#[inline(never)]
pub async fn sweep(state: &State, repo: &Repo) {
    state
        .node_sweep_detached(repo)
        .await
        .expect("a sweep succeeds");
}

/// Block `index` loaded, as a session's reads load the blocks they meet.
#[inline(never)]
pub async fn load_block(state: &State, repo: &Repo, index: usize) {
    state
        .block(repo.clone(), index)
        .await
        .expect("the block loads");
}

#[inline(never)]
pub fn retries(state: &State) -> u64 {
    state.attach_retries()
}

/// A directory mark as the served provider makes one: top-down from the first ancestor already
/// marked, each directory keeping its action and extension flags.
#[inline(never)]
pub async fn served_mark_dir(state: &State, repo: &Repo, dir: NodeID) {
    state
        .served()
        .node_mark(repo, dir)
        .await
        .expect("a directory mark succeeds");
}

/// A create as the served provider makes one: a checked attach under the served chain rule; the
/// node where it attached.
#[inline(never)]
pub async fn served_create(
    state: &State,
    repo: &Repo,
    dir: NodeID,
    node: Node,
    name: &str,
) -> Option<NodeID> {
    match state
        .served()
        .node_attach(repo.clone(), dir, node, name, AttachCheck::Walk)
        .await
        .expect("an attach answers")
    {
        Some(AttachOutcome::Attached(node)) => Some(node),
        Some(AttachOutcome::Exists(_)) | None => None,
    }
}

/// A copy-up's State step as the served provider makes one (`ServedNodes::node_copy_up`): the
/// content rewritten with the dirty marks under the served chain rule, and where the directory
/// refuses it, the directory marked and the rewrite tried again.
#[inline(never)]
pub async fn served_copy_up(state: &State, repo: &Repo, dir: NodeID, node: NodeID, size: u64) {
    state
        .served()
        .node_copy_up(
            repo,
            node,
            dir,
            (0o644, size),
            (NodeFlags::DirtyModify, NodeExtFlags::NoFlags),
        )
        .await
        .expect("a copy-up succeeds");
}

/// A directory settle as a commit makes one, through the served clear against the same State,
/// which stands in for the committed one with the same chains: its action bits dropped and its
/// clear pending, its children compared, then cleared where they match and no change landed
/// meanwhile; whether it cleared. The buffers carry over between settles on a thread, as a
/// commit's settle pass reuses them.
#[inline(never)]
pub async fn served_settle(state: &State, repo: &Repo, dir: NodeID) -> bool {
    thread_local! {
        static SCRATCH: std::cell::Cell<ClearScratch> = std::cell::Cell::default();
    }
    let record = state
        .node(repo.clone(), dir)
        .await
        .expect("the directory reads");
    let against = ClearAgainst {
        state,
        repository: repo,
        node: dir,
    };
    let mut scratch = SCRATCH.take();
    let cleared = state
        .served()
        .node_clear(
            repo,
            dir,
            ServedClear {
                place: (record.parent, record.name_hash),
                against,
                spelled: against,
                scratch: &mut scratch,
            },
            async {},
        )
        .await
        .expect("the clear runs");
    SCRATCH.set(scratch);
    cleared == ClearOutcome::Cleared
}

/// An RMDIR as the served provider makes one: the directory's chain read from its block in
/// memory, as the provider's first, resident check reads it, the directory checked empty, then
/// detached while its chain holds what the check read.
#[inline(never)]
pub async fn served_rmdir(state: &State, repo: &Repo, dir: NodeID) -> bool {
    let block = state
        .resident_block(NodeBlock::index(dir))
        .expect("the directory's block is in memory");
    let (_, count) = block.node_with_chain_changes(Node::index(dir));
    let children = state
        .node_children(repo.clone(), dir)
        .await
        .expect("the children read");
    if !children.is_empty() {
        return false;
    }
    let read = IfUnchanged::new(block, dir, count);
    served_detach(state, repo, dir, Detach::IfUnchanged(&read)).await;
    true
}

/// A removal as the served provider's `detach_node` makes one: the node detached under the
/// served chain rule where `when` allows, then its detached record read for its flags.
async fn served_detach(state: &State, repo: &Repo, node: NodeID, when: Detach<'_>) -> Node {
    assert!(
        state
            .served()
            .node_detach(repo.clone(), node, when)
            .await
            .expect("a detach succeeds"),
        "the detach goes ahead"
    );
    state
        .node(repo.clone(), node)
        .await
        .expect("the detached record reads")
}

/// A rename onto an empty directory as the served provider makes one: a replacing move under
/// the served chain rule.
#[inline(never)]
pub async fn served_rename_over(
    state: &State,
    repo: &Repo,
    node: NodeID,
    parent: NodeID,
    name: &str,
    holder: NodeID,
) -> bool {
    matches!(
        state
            .served()
            .node_reattach(
                repo.clone(),
                node,
                parent,
                name,
                ReattachCheck::Replacing(holder)
            )
            .await
            .expect("a replacing move answers"),
        AttachOutcome::Attached(_)
    )
}

/// An UNLINK as the served provider makes one: the node removed as `detach_node` removes it.
#[inline(never)]
pub async fn served_unlink(state: &State, repo: &Repo, node: NodeID) {
    served_detach(state, repo, node, Detach::Always).await;
}

/// A fresh mark of a leaf followed by its clear, which climbs back up the path the mark dirtied.
#[inline(never)]
pub async fn mark_fresh(state: &State, repo: &Repo, node: NodeID) {
    state
        .node_mark(
            repo,
            node,
            Mark::dirty(
                NodeFlags::DirtyModify,
                Ext::Set(NodeExtFlags::NoFlags),
                false,
            ),
        )
        .await
        .expect("a mark succeeds");
    state
        .node_clear(repo, node, Clear::Dirty)
        .await
        .expect("a clear succeeds");
}

/// A moved directory's descendant marked as `mark_subtree_moved` marks one: its staged move and
/// its dirty move.
#[inline(never)]
pub async fn mark_moved(state: &State, repo: &Repo, node: NodeID) {
    let mark = Mark {
        staged: Some((NodeFlags::StagedMove, false)),
        dirty: Some((NodeFlags::DirtyMove, Ext::Set(NodeExtFlags::NoFlags), false)),
    };
    state
        .node_mark(repo, node, mark)
        .await
        .expect("a mark succeeds");
}

/// A new local directory a mount adds under `parent`, as `add_local_node` adds one.
#[inline(never)]
pub async fn mount_add_dir(state: &State, repo: &Repo, parent: NodeID, name: &str) -> NodeID {
    let node_id = state
        .node_add(repo.clone(), parent, directory(name), name)
        .await
        .expect("an add succeeds");
    state
        .node_mark(
            repo,
            node_id,
            Mark::dirty(
                NodeFlags::DirtyAdd,
                Ext::Set(NodeExtFlags::LocalContent),
                false,
            ),
        )
        .await
        .expect("a mark succeeds");
    node_id
}

/// A local file a mount adds under `parent`, which holds no node of its name, as
/// `add_local_node` adds a new one.
#[inline(never)]
pub async fn mount_add(state: &State, repo: &Repo, parent: NodeID, (name, name_hash): (&str, u64)) {
    let node = Node {
        flags: NodeFlags::File.bits(),
        size: 1,
        name_hash,
        ..Node::default()
    };
    let node_id = state
        .node_add(repo.clone(), parent, node, name)
        .await
        .expect("an add succeeds");
    state
        .node_mark(
            repo,
            node_id,
            Mark::dirty(
                NodeFlags::DirtyAdd,
                Ext::Set(NodeExtFlags::LocalContent),
                false,
            ),
        )
        .await
        .expect("a mark succeeds");
}

/// A mount's pass over `dir`, a tracked directory, for the local files `entries`, as a VFS
/// provider's mount would make one: the directory's children read once into an index by name hash,
/// then each entry's file taking the local content in place where the directory holds its name,
/// and added where it does not.
#[inline(never)]
pub async fn mount_tracked(
    state: &Arc<State>,
    repo: &Repo,
    dir: NodeID,
    entries: &[(String, u64)],
) {
    let mut held: Vec<(u64, NodeID, Node)> = Vec::new();
    let mut children =
        lore_revision::state::StateNodeChildrenIterator::new(state.clone(), repo.clone(), dir)
            .await
            .expect("the walk starts");
    while let Some((node_id, node)) = children.next().await.expect("the walk steps") {
        held.push((node.name_hash, node_id, node));
    }
    held.sort_unstable_by_key(|(name_hash, _, _)| *name_hash);
    for (name, name_hash) in entries {
        let existing = held
            .binary_search_by_key(name_hash, |(name_hash, _, _)| *name_hash)
            .ok()
            .map(|index| held[index].1);
        let (node_id, flags) = if let Some(node_id) = existing {
            state
                .node_rewrite(
                    repo,
                    node_id,
                    &Content {
                        mode: 0,
                        size: 1,
                        address: Address::default(),
                    },
                    Rewrite::Plain,
                )
                .await
                .expect("a rewrite succeeds");
            (node_id, NodeFlags::DirtyModify)
        } else {
            let node = Node {
                flags: NodeFlags::File.bits(),
                size: 1,
                name_hash: *name_hash,
                ..Node::default()
            };
            let node_id = state
                .node_add(repo.clone(), dir, node, name)
                .await
                .expect("an add succeeds");
            (node_id, NodeFlags::DirtyAdd)
        };
        state
            .node_mark(
                repo,
                node_id,
                Mark::dirty(flags, Ext::Set(NodeExtFlags::LocalContent), false),
            )
            .await
            .expect("a mark succeeds");
    }
}

/// A RENAME of a file into directory `to` as the served provider makes one: a checked move under
/// the served chain rule; true where it moved.
#[inline(never)]
pub async fn served_rename(
    state: &State,
    repo: &Repo,
    node: NodeID,
    to: NodeID,
    name: &str,
) -> bool {
    matches!(
        state
            .served()
            .node_reattach(repo.clone(), node, to, name, ReattachCheck::Walk)
            .await
            .expect("a move answers"),
        AttachOutcome::Attached(_)
    )
}

/// A swap's copy of a directory of `children` files below `parent`: its name checked, the
/// directory built out of every chain with each child added into it, then published whole; the
/// directory.
#[inline(never)]
pub async fn swap_copy(
    state: &State,
    repo: &Repo,
    parent: NodeID,
    name: &str,
    children: &[String],
) -> NodeID {
    let mut build = copy_root(state, repo, parent, name).await;
    for child in children {
        copy_child(&mut build, child).await;
    }
    copy_publish(build).await
}

/// A swap's copy of one file below `parent`: its name checked, its node taken out of every
/// chain, then published, a build of one node; the file.
#[inline(never)]
pub async fn swap_copy_file(state: &State, repo: &Repo, parent: NodeID, name: &str) -> NodeID {
    match state
        .swap()
        .node_attach_tree(repo.clone(), parent, &file(name), name)
        .await
        .expect("a copy begins")
    {
        TreeAttach::Building(build) => copy_publish(build).await,
        TreeAttach::Exists(_) => panic!("the copy's name is free"),
    }
}

/// Whether `node` is clean.
#[inline(never)]
pub async fn is_clean(state: &State, repo: &Repo, node: NodeID) -> bool {
    !state
        .node(repo.clone(), node)
        .await
        .expect("the node reads")
        .is_dirty()
}

/// Proof the bench's repository is a path-less in-memory one.
struct InMemoryBench;

impl lore_revision::repository::InMemoryContext for InMemoryBench {}

/// `state` written to the store and read back, so its blocks load on demand.
#[inline(never)]
pub async fn cold(state: &Arc<State>, repo: &Repo) -> Arc<State> {
    let token = lore_revision::repository::RepositoryWriteToken::in_memory(&InMemoryBench);
    let signature = state
        .serialize(repo.clone(), &token)
        .await
        .expect("the state serializes");
    State::deserialize(repo.clone(), signature)
        .await
        .expect("the state reads back")
}

/// The build of a swap's subtree copy, its name checked and its root taken.
#[inline(never)]
pub async fn copy_root<'s>(
    state: &'s State,
    repo: &Repo,
    parent: NodeID,
    name: &str,
) -> TreeBuild<'s> {
    match state
        .swap()
        .node_attach_tree(repo.clone(), parent, &directory(name), name)
        .await
        .expect("a copy begins")
    {
        TreeAttach::Building(build) => build,
        TreeAttach::Exists(_) => panic!("the copy's name is free"),
    }
}

/// One child a swap's subtree copy adds into the directory it builds.
#[inline(never)]
pub async fn copy_child(build: &mut TreeBuild<'_>, name: &str) -> Built {
    build
        .add(build.root(), &file(name), name)
        .await
        .expect("a copied child adds")
}

/// A swap's subtree copy published whole; its directory.
#[inline(never)]
pub async fn copy_publish(build: TreeBuild<'_>) -> NodeID {
    match build.publish().await.expect("a copy publishes") {
        AttachOutcome::Attached(dir) => dir,
        AttachOutcome::Exists(_) => panic!("the copy's name is free"),
    }
}

/// A discard of a node and everything below it under an exclusive claim, as the callers that free
/// at once make one (`apply_pending_discards`): the node unlinked from its chain, then its subtree
/// freed with a task per directory, the node last.
#[inline(never)]
pub async fn discard(state: &Arc<State>, repo: &Repo, node: NodeID) {
    lore_revision::state::node_discard_fused(state.clone(), repo.clone(), node)
        .await
        .expect("a discard succeeds");
}

/// A commit's post-pass discard of `roots` under its exclusive claim, as `commit_discard` makes
/// one: one batched free, each root unlinked from its chain, then every slot freed a block at a
/// time. The roots here are files, so the walk collected nothing below them.
#[inline(never)]
pub async fn discard_batch(state: &Arc<State>, repo: &Repo, roots: &[NodeID]) {
    lore_revision::state::node_discard_free(state, repo, roots, Vec::new(), |_, _| {})
        .await
        .expect("a discard succeeds");
}
