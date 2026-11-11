// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The node operations the scenarios drive, in one thin layer over `State`, so a scenario stays as
//! it is while the node interface changes under it. Each op a production caller makes is a kind of
//! the catalogue (`super::KINDS`), and records its call in a recording run: its interval and what
//! it answered ([`super::call`]). The helpers no production caller makes, such as the fixtures'
//! edits, say so and record none.

use std::sync::Arc;

use lore_revision::node::LogMark;
use lore_revision::node::Newest;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::node::NodeLink;
use lore_revision::node::PatchKind;
use lore_revision::node::ROOT_NODE;
use lore_revision::node::SiblingCycleGuard;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::is_reserved_node_name;
use lore_revision::stage::DirectoryChildren;
use lore_revision::state::Absent;
use lore_revision::state::AddedSince;
use lore_revision::state::AttachCheck;
use lore_revision::state::AttachOutcome;
use lore_revision::state::ChainMark;
use lore_revision::state::Clear;
use lore_revision::state::ClearAgainst;
use lore_revision::state::ClearOutcome;
use lore_revision::state::ClearScratch;
use lore_revision::state::Content;
use lore_revision::state::Detach;
use lore_revision::state::DiscardCounts;
use lore_revision::state::Ext;
use lore_revision::state::IfUnchanged;
use lore_revision::state::LevelMark;
use lore_revision::state::LinkPin;
use lore_revision::state::Mark;
use lore_revision::state::NameChange;
use lore_revision::state::OwnFlags;
use lore_revision::state::ReattachCheck;
use lore_revision::state::Reattached;
use lore_revision::state::Rewrite;
use lore_revision::state::ServedClear;
use lore_revision::state::State;
use lore_revision::state::StateChildrenNodes;
use lore_revision::state::StateError;
use lore_revision::state::StateNodeChildrenIterator;
use lore_revision::state::StateNodeChildrenWithNameIterator;
use lore_revision::state::TreeAttach;
use lore_revision::state::TreeBuild;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Point;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;

use super::Kind;
use super::call::Answer;
use super::call::Filter;
use super::call::Leaves;
use super::call::Lookup;
use super::call::On;
use super::call::Outcome;
use super::call::Over;
use super::call::Refusal;
use super::call::Repeats;
use super::call::SelfReported;
use super::call::Sweeping;
use super::call::Walk;
use super::call::Yielded;
use super::call::call;
use super::call::call_on;
use super::call::collected;
use super::call::leaving;
use super::call::looked_up;
use super::call::open_call;
use super::call::path_spelled;
use super::call::resolving_names;
use super::call::spelling;
use super::call::sweeping;
use super::call::taken;
use super::call::walked;
use super::call::window;
use crate::harness::fixtures::Tree;

/// The refusal of an op that found its node or directory gone.
const GONE: Outcome = Outcome::Refused(Refusal::NotFound);

/// Whether `error` says the directory an add went to was gone: not found, or `Discarded` under the
/// add's lock, which the unserved add answers as an internal error (attach.rs, `add_child`).
fn parent_gone(error: &StateError) -> bool {
    error.is_node_not_found()
        || error
            .to_string()
            .contains("cannot add a child to a discarded node")
}

/// `step` on `node`, after the step its call answers by, with `node` taken by another task's
/// detach since, it or an ancestor, passed over and noted on the call from where `step` began:
/// the answer stands, as the node was there when the call decided.
async fn taken_since<T>(
    state: &State,
    node: NodeID,
    step: impl Future<Output = Result<T, StateError>>,
) -> Result<(), StateError> {
    ran_unless_taken(state, node, step).await.map(|_| ())
}

/// [`taken_since`], answering whether `step` ran: `false` where it found `node` taken.
async fn ran_unless_taken<T>(
    state: &State,
    node: NodeID,
    step: impl Future<Output = Result<T, StateError>>,
) -> Result<bool, StateError> {
    // Read before the step, as one that found the node gone may yield before it answers.
    let from = state.hooks().seq_now();
    match step.await {
        Ok(_) => Ok(true),
        Err(error) if error.is_node_not_found() => {
            taken(state, node, from);
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// On `target`, moving it into `destination` under `name`.
fn moving(target: NodeID, destination: NodeID, name: &str) -> On {
    On {
        target: Some(target),
        destination: Some(destination),
        name: Some(hash_string(name)),
        spelling: Some(spelling(name.as_bytes())),
        ..On::default()
    }
}

/// On directory `dir`, attaching under `name`.
fn naming(dir: NodeID, name: &str) -> On {
    On {
        target: Some(dir),
        name: Some(hash_string(name)),
        ..On::default()
    }
}

/// The outcome of `error`, one a run can provoke: self-reported where the State asked its caller
/// to slow down or a name table had no room, otherwise a failure, which only a failed read makes
/// legal.
pub(crate) fn failed(error: &StateError) -> Outcome {
    if error.is_slow_down() {
        return Outcome::SelfReported(SelfReported::SlowDown);
    }
    let text = error.to_string();
    if names_full(&text) {
        Outcome::SelfReported(SelfReported::NameTableFull)
    } else {
        Outcome::Failed(text)
    }
}

/// Whether an error's `text` says a name table had no room: as the name store answers a table it
/// cannot grow (state/block.rs, `node_name_store`), or the staged move one it found full before its
/// marks (relocate.rs, `node_move_staged`).
fn names_full(text: &str) -> bool {
    text.contains("node name store would grow") || text.contains("name table has no room")
}

/// The outcome of a change answering done or `error`.
fn done_or_failed(result: Result<(), StateError>) -> Outcome {
    match result {
        Ok(()) => Outcome::Done(Answer::None),
        Err(error) => gone_or_failed(&error),
    }
}

/// The outcome of `error`: the node or directory gone where it was not found, as a concurrent
/// removal leaves it, else as [`failed`] says.
fn gone_or_failed(error: &StateError) -> Outcome {
    if error.is_node_not_found() {
        GONE
    } else {
        failed(error)
    }
}

/// A served directory mark, as a swap's restore marks a directory holding a name it restores, one
/// at a time with the provider's settles and swaps; whether it marked. A directory a concurrent removal took is no error.
pub async fn served_mark_dir(tree: &Tree, dir: NodeID) -> bool {
    call(&tree.state, (&super::SERVED_MARK, Some(dir)), async {
        match tree.state.served().node_mark(&tree.repository, dir).await {
            Ok(()) => (true, Outcome::Done(Answer::None)),
            Err(error) if error.is_node_not_found() => (false, GONE),
            Err(error) => (false, failed(&error)),
        }
    })
    .await
}

/// A served create of file `name` in `dir`, as the provider's `attach_local` makes one: the node
/// added and holding local content. The node where it attached; `None` where the name is held or
/// the directory is gone, which the provider answers with an error.
pub async fn served_create(tree: &Tree, dir: NodeID, name: &str) -> Option<NodeID> {
    call_on(
        &tree.state,
        (&super::SERVED_CREATE, naming(dir, name)),
        async {
            let attached = tree
                .state
                .served()
                .node_attach(
                    tree.repository.clone(),
                    dir,
                    local_file(name),
                    name,
                    AttachCheck::Walk,
                )
                .await;
            attach_answered(attached)
        },
    )
    .await
}

/// A file named `name` added and holding local content, as a served create makes one.
fn local_file(name: &str) -> Node {
    Node {
        flags: (NodeFlags::File | NodeFlags::DirtyAdd).bits(),
        ext_flags: NodeExtFlags::LocalContent.bits(),
        name_hash: hash_string(name),
        ..Node::default()
    }
}

/// A served attach's answer, and its outcome: a refusal where the name was held or the directory
/// was gone.
fn attach_answered(
    attached: Result<Option<AttachOutcome>, StateError>,
) -> (Option<NodeID>, Outcome) {
    match attached {
        Ok(Some(AttachOutcome::Attached(id))) => (Some(id), Outcome::Done(Answer::Node(id))),
        Ok(Some(AttachOutcome::Exists(_))) => (None, Outcome::Refused(Refusal::Exists)),
        // Only a resident attach answers `None`.
        Ok(None) => (
            None,
            Outcome::Failed("a fetching attach answered no node".into()),
        ),
        Err(error) => (None, gone_or_failed(&error)),
    }
}

/// A served create of file `name` in `dir` after a listing reader's build of `dir`, as FUSE's
/// `create_node` makes one: its `resolve_to_attach` looks the name up in the directory's listing
/// (`Listing::lookup_to_attach`), and `attach_local` attaches checked against the directory's log
/// since the lookup where it found the name absent (`AttachCheck::Absent`). Where the lookup found
/// the name held, `create_node` answers it exists with no attach; this attaches by walking the
/// chain, which a holder gone since lets through. The node where it attached; `None` where the
/// name is held or the directory is gone.
pub async fn listing_create(tree: &Tree, dir: NodeID, name: &str) -> Option<NodeID> {
    listing_create_waiting(tree, dir, name, (async {}, async {})).await
}

/// [`listing_create`], awaiting each of `windows`, windows of its call: the first once the build
/// marked the directory's log, as the provider's build runs its hook there, the second after the
/// lookup, before the attach, as a request's other steps run there.
pub async fn listing_create_waiting(
    tree: &Tree,
    dir: NodeID,
    name: &str,
    windows: (impl Future<Output = ()>, impl Future<Output = ()>),
) -> Option<NodeID> {
    call_on(
        &tree.state,
        (&super::LISTING_CREATE, naming(dir, name)),
        async {
            let absent = match listing_absence(tree, dir, name, windows).await {
                Ok(absent) => absent,
                Err(outcome) => return (None, outcome),
            };
            let check = absent
                .as_ref()
                .map_or(AttachCheck::Walk, AttachCheck::Absent);
            let attached = tree
                .state
                .served()
                .node_attach(tree.repository.clone(), dir, local_file(name), name, check)
                .await;
            attach_answered(attached)
        },
    )
    .await
}

/// A served rename of `node` to `name` in `dir` after a listing reader's build of `dir`, as FUSE's
/// `rename_in_state` makes one onto a name its `resolve_to_attach` looked up in the directory's
/// listing: checked against the directory's log since the lookup where it found the name absent
/// (`ReattachCheck::Absent`). Where the lookup found the name held, FUSE replaces its holder; this
/// moves by walking the chain, refused where the name is still held. Whether it moved, as
/// [`served_rename`] answers.
pub async fn listing_rename(tree: &Tree, node: NodeID, dir: NodeID, name: &str) -> bool {
    listing_rename_waiting(tree, node, dir, name, (async {}, async {})).await
}

/// [`listing_rename`], awaiting each of `windows` as [`listing_create_waiting`] does.
pub async fn listing_rename_waiting(
    tree: &Tree,
    node: NodeID,
    dir: NodeID,
    name: &str,
    windows: (impl Future<Output = ()>, impl Future<Output = ()>),
) -> bool {
    call_on(
        &tree.state,
        (&super::LISTING_RENAME, moving(node, dir, name)),
        async {
            leaving(Leaves::Placed);
            let absent = match listing_absence(tree, dir, name, windows).await {
                Ok(absent) => absent,
                Err(outcome) => return (false, outcome),
            };
            let check = absent
                .as_ref()
                .map_or(ReattachCheck::Walk, ReattachCheck::Absent);
            let renamed = tree
                .state
                .served()
                .node_reattach_local(tree.repository.clone(), node, dir, name, check)
                .await;
            reattached(renamed)
        },
    )
    .await
}

/// What a listing reader's build of a directory leaves for its lookups: the directory's block, the
/// mark of its log read before the walk, each child it listed with its name's hash, the blocks
/// holding them by index, and whether it left a child out.
struct ListingBuild {
    dir_block: Arc<NodeBlock>,
    mark: LogMark,
    listed: Vec<(NodeID, u64)>,
    blocks: Vec<(usize, Arc<NodeBlock>)>,
    left_out: bool,
}

/// A listing reader's build of directory `dir`, awaiting the first of `windows` once it marked the
/// log; its lookup of `name` to attach ([`absence`]); the second of `windows`; then the reader's
/// point where it checks the absence.
async fn listing_absence(
    tree: &Tree,
    dir: NodeID,
    name: &str,
    (marked, between): (impl Future<Output = ()>, impl Future<Output = ()>),
) -> Result<Option<Absent>, Outcome> {
    let built = listing_built(tree, dir, marked).await?;
    let absent = absence(&tree.state, &built, dir, hash_string(name))?;
    window(&tree.state, between).await;
    hooks::reached_reader(Point::AbsentCheck).await;
    Ok(absent)
}

/// A listing reader's build of directory `dir`, as the provider's `Listing::build_with` makes one:
/// the directory's log marked before the walk, so a patch landing during it is found after, then
/// the reader's point there and `marked`, a window of its call; the children's name tables loaded;
/// the children walked by name, one staged for deletion left out, the walk noted; the blocks of the
/// children listed held. No fixture holds a link or a name the provider cannot represent, which its
/// build refuses or leaves out.
async fn listing_built(
    tree: &Tree,
    dir: NodeID,
    marked: impl Future<Output = ()>,
) -> Result<ListingBuild, Outcome> {
    let (state, repository) = (&tree.state, &tree.repository);
    let dir_block = node_block(tree, dir).await?;
    let (mark, _) = dir_block.watch_chain(Node::index(dir), None);
    hooks::reached_reader(Point::ListingLog).await;
    window(state, marked).await;
    (state
        .load_children_with_nametables(repository.clone(), dir)
        .await)
        .map_err(|error| gone_or_failed(&error))?;
    let from = state.hooks().seq_now();
    let (walk, left_out) = (named_walk(tree, (dir, from), Filter::UNDELETED).await)
        .map_err(|error| gone_or_failed(&error))?;
    let listed: Vec<(NodeID, u64)> = (walk.yielded.iter())
        .map(|child| {
            let record = child.record.expect("a named walk reads each record");
            (child.node, record.name_hash)
        })
        .collect();
    walked(state, walk);
    let mut blocks: Vec<(usize, Arc<NodeBlock>)> = Vec::new();
    for &(node, _) in &listed {
        let index = NodeBlock::index(node);
        if !blocks.iter().any(|&(held, _)| held == index) {
            blocks.push((index, node_block(tree, node).await?));
        }
    }
    Ok(ListingBuild {
        dir_block,
        mark,
        listed,
        blocks,
        left_out,
    })
}

/// The absence of `name_hash` from `dir` a listing reader's lookup to attach finds after `built`,
/// as the provider's `Listing::lookup_to_attach` reads it: none where the build left a child out,
/// or where an entry listed under the hash still holds its node, read under its block's read lock
/// alone; then, under the directory block's read lock, none where the log was respelled since the
/// build, no longer covers it, or holds an attach of the hash since; `GONE` where the directory is
/// `Discarded`.
fn absence(
    state: &State,
    built: &ListingBuild,
    dir: NodeID,
    name_hash: u64,
) -> Result<Option<Absent>, Outcome> {
    let holds_entry = |&(node, _): &(NodeID, u64)| {
        let index = NodeBlock::index(node);
        (built.blocks.iter())
            .find(|&&(held, _)| held == index)
            .is_some_and(|(_, block)| {
                let record = block.node(Node::index(node));
                !record.is_discarded() && record.parent == dir && record.name_hash == name_hash
            })
    };
    let mut listed = built.listed.iter().filter(|&&(_, hash)| hash == name_hash);
    if built.left_out || listed.any(holds_entry) {
        return Ok(None);
    }
    let reader = built.dir_block.read();
    let at = Node::index(dir);
    if reader.node(at).is_discarded() {
        return Err(GONE);
    }
    let Some(log) = (reader.chain_log(at)).filter(|log| log.respells() == built.mark.respells)
    else {
        return Ok(None);
    };
    let free = match log.newest(name_hash, built.mark.seq) {
        Newest::Nothing => true,
        Newest::Patch(patch) => matches!(patch.kind, PatchKind::Detached),
        Newest::Uncovered => false,
    };
    Ok(free.then(|| Absent::new(state, log.seq(), built.dir_block.clone(), dir, name_hash)))
}

/// A walk of `dir`'s children by name begun at `from`, as the provider's listing walks one, to its
/// end, each child it keeps noted with its record and its name's spelling: the walk under
/// `filter`, and whether it left a child out, for the filter or for a name it could not read.
async fn named_walk(
    tree: &Tree,
    (dir, from): (NodeID, u64),
    filter: Filter,
) -> Result<(Walk, bool), StateError> {
    let repository = tree.repository.clone();
    let mut walk =
        StateNodeChildrenWithNameIterator::new(tree.state.clone(), repository, dir).await?;
    let (mut yielded, mut left_out) = (Vec::new(), false);
    while let Some((node, record, name)) = walk.next().await? {
        let spelling = Some(spelling(name.as_bytes()));
        // The name holds its block's read lock, dropped before the next step.
        drop(name);
        if !filter.keeps(&record) {
            left_out = true;
            continue;
        }
        yielded.push(Yielded {
            node,
            record: Some(record),
            name: None,
            spelling,
        });
    }
    let skipped = walk.skipped_invalid();
    let walk = Walk {
        dir,
        from,
        yielded,
        repeats: Repeats::reported(walk.may_repeat()),
        skipped,
        filter,
        // An iterator walks a directory's chain alone: `Node::child` asserts a directory.
        over: Over::Chain,
    };
    Ok((walk, left_out || skipped != 0))
}

/// The provider diff's checked attach of file `name` in `dir`, as its `settle_add` makes one where
/// its tree does not list `dir`: the file attached dirty-added where no live child holds the name;
/// where one does, a delete staged for it taken back and the holder marked modified unless it is
/// an add; then `dir` marked. `None` where the directory is gone.
pub async fn checked_create(tree: &Tree, dir: NodeID, name: &str) -> Option<AttachOutcome> {
    call_on(
        &tree.state,
        (&super::CHECKED_CREATE, naming(dir, name)),
        async {
            match checked_attach(tree, dir, name).await {
                Ok(Some(AttachOutcome::Attached(id))) => (
                    Some(AttachOutcome::Attached(id)),
                    Outcome::Done(Answer::Node(id)),
                ),
                Ok(Some(AttachOutcome::Exists(held))) => (
                    Some(AttachOutcome::Exists(held)),
                    Outcome::Done(Answer::Held(held)),
                ),
                Ok(None) => (None, GONE),
                Err(error) => (None, failed(&error)),
            }
        },
    )
    .await
}

/// The node [`checked_create`] attached; `None` where it found the name held, or the directory
/// gone.
pub fn attached(created: Option<AttachOutcome>) -> Option<NodeID> {
    match created {
        Some(AttachOutcome::Attached(node)) => Some(node),
        _ => None,
    }
}

/// [`checked_create`]'s composition: `None` where the directory is gone.
async fn checked_attach(
    tree: &Tree,
    dir: NodeID,
    name: &str,
) -> Result<Option<AttachOutcome>, StateError> {
    let (state, repository) = (&tree.state, &tree.repository);
    let node = Node {
        flags: (NodeFlags::File | NodeFlags::DirtyAdd).bits(),
        name_hash: hash_string(name),
        ..Node::default()
    };
    let attached = match state.node_attach(repository.clone(), dir, node, name).await {
        Ok(attached) => attached,
        Err(error) if error.is_node_not_found() => return Ok(None),
        Err(error) => return Err(error),
    };
    if let AttachOutcome::Exists(held) = attached {
        let settled = async {
            let holder = state.node(repository.clone(), held).await?;
            if holder.is_staged_delete() {
                state.node_clear(repository, held, Clear::Staged).await?;
            }
            if !holder.is_dirty_add() {
                let modified = Mark::dirty(
                    NodeFlags::DirtyModify,
                    Ext::Set(NodeExtFlags::NoFlags),
                    true,
                );
                state.node_mark(repository, held, modified).await?;
            }
            Ok(())
        };
        taken_since(state, held, settled).await?;
    }
    let marked = state.node_mark(
        repository,
        dir,
        Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
    );
    taken_since(state, dir, marked).await?;
    Ok(Some(attached))
}

/// A served rename of `node` to `name` in `dir`, as FUSE's `rename_in_state` makes one onto a
/// free name (`ServedNodes::node_reattach_local`): a directory moves to another place only while
/// it holds local content, and a file or a respell moves as any reattach does. Whether it moved;
/// a directory refused, a held name, and a node or directory a concurrent removal took are no
/// error.
pub async fn served_rename(tree: &Tree, node: NodeID, dir: NodeID, name: &str) -> bool {
    matches!(
        served_rename_outcome(tree, node, dir, name).await,
        Outcome::Done(_)
    )
}

/// [`served_rename`]'s outcome, as its call records it.
pub async fn served_rename_outcome(tree: &Tree, node: NodeID, dir: NodeID, name: &str) -> Outcome {
    call_on(
        &tree.state,
        (&super::SERVED_RENAME, moving(node, dir, name)),
        async {
            leaving(Leaves::Placed);
            let renamed = tree
                .state
                .served()
                .node_reattach_local(
                    tree.repository.clone(),
                    node,
                    dir,
                    name,
                    ReattachCheck::Walk,
                )
                .await;
            let (_, outcome) = reattached(renamed);
            (outcome.clone(), outcome)
        },
    )
    .await
}

/// A local reattach's answer, and its outcome: a refusal where the name was held or the directory
/// held no local content, or where a concurrent removal took the node or the directory.
fn reattached(answer: Result<Reattached, StateError>) -> (bool, Outcome) {
    match answer {
        Ok(Reattached::Moved { added }) => (true, Outcome::Done(Answer::Moved { added })),
        Ok(Reattached::Exists(_)) => (false, Outcome::Refused(Refusal::Exists)),
        Ok(Reattached::NotLocal) => (false, Outcome::Refused(Refusal::NotLocal)),
        Err(error) if error.is_node_not_found() => (false, GONE),
        // A rename into the node's own subtree, as a concurrent rename of a directory makes one,
        // or under a node that is no directory.
        Err(error) if error.is_invalid_arguments() => (false, Outcome::Refused(Refusal::Subtree)),
        Err(error) => (false, failed(&error)),
    }
}

/// A served reattach of directory `node` to `name` in `dir`, another place, moving any directory,
/// which no caller makes: the plain `ServedNodes::node_reattach`, kept for the scenarios pinned on
/// directory moves, so no kind. Whether it moved, a held name and a node or directory a concurrent
/// removal took being no error; `None` where the subtree check refused a move into its own subtree.
pub async fn served_reattach_any(
    tree: &Tree,
    node: NodeID,
    dir: NodeID,
    name: &str,
) -> Option<bool> {
    match tree
        .state
        .served()
        .node_reattach(
            tree.repository.clone(),
            node,
            dir,
            name,
            ReattachCheck::Walk,
        )
        .await
    {
        Ok(AttachOutcome::Attached(_)) => Some(true),
        Ok(AttachOutcome::Exists(_)) => Some(false),
        Err(error) if error.is_node_not_found() => Some(false),
        Err(error) if error.is_invalid_arguments() => None,
        Err(error) => panic!("a directory reattach fails: {error}"),
    }
}

/// Gives directory `node` local content, as a directory a create made under `write/` holds it: a
/// fixture's edit, no kind.
pub async fn hold_local(tree: &Tree, node: NodeID) {
    let (state, repository) = (&tree.state, &tree.repository);
    let block = state
        .block(repository.clone(), NodeBlock::index(node))
        .await
        .expect("the directory's block");
    state.node_edit_for_tests(&block, node, |held| {
        held.ext_flags |= NodeExtFlags::LocalContent.bits();
    });
}

/// A served rename of `node` to `name` in `dir` over `holder`, the node holding the name, as
/// FUSE's `rename_in_state` makes one onto a held name, under the local rule
/// [`served_rename`] states; whether it moved. A directory refused and a node or directory a
/// concurrent removal took are no error.
pub async fn served_rename_replacing(
    tree: &Tree,
    node: NodeID,
    (dir, name): (NodeID, &str),
    holder: NodeID,
) -> bool {
    call_on(
        &tree.state,
        (
            &super::SERVED_RENAME_REPLACING,
            On {
                holder: Some(holder),
                ..moving(node, dir, name)
            },
        ),
        async {
            leaving(Leaves::Placed);
            let renamed = tree
                .state
                .served()
                .node_reattach_local(
                    tree.repository.clone(),
                    node,
                    dir,
                    name,
                    ReattachCheck::Replacing(holder),
                )
                .await;
            reattached(renamed)
        },
    )
    .await
}

/// A served removal of `node`, as FUSE's `detach_node` makes one; whether it detached.
pub async fn served_detach(tree: &Tree, node: NodeID) -> bool {
    call(&tree.state, (&super::SERVED_DETACH, Some(node)), async {
        match tree
            .state
            .served()
            .node_detach(tree.repository.clone(), node, Detach::Always)
            .await
        {
            Ok(true) => (true, Outcome::Done(Answer::None)),
            Ok(false) => panic!("a detach that refuses nothing refused"),
            Err(error) if error.is_node_not_found() => (false, GONE),
            Err(error) => (false, failed(&error)),
        }
    })
    .await
}

/// How a served RMDIR ended ([`served_rmdir`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct Rmdir {
    /// Whether it detached the directory.
    pub removed: bool,
    /// The detaches a changed chain refused under the lock.
    pub refused: usize,
    /// The walks after which it found the chain moved, before a detach.
    pub rechecked: usize,
}

/// A served RMDIR of directory `dir`, as FUSE's makes one: on each attempt the directory's block
/// read, its record with its chain's count, its children walked by name, a child staged for
/// deletion passed over, and the directory detached only while its chain holds what the walk read
/// ([`Detach::IfUnchanged`]); a chain that moved during the walk is checked again before the
/// detach, as FUSE checks it before it touches the host. A directory holding a child, or one a
/// concurrent removal took, is not detached.
pub async fn served_rmdir(tree: &Tree, dir: NodeID) -> Rmdir {
    call(&tree.state, (&super::SERVED_RMDIR, Some(dir)), async {
        let mut ended = Rmdir::default();
        let outcome = rmdir(tree, dir, &mut ended).await;
        (ended, outcome)
    })
    .await
}

thread_local! {
    /// The attempts a served RMDIR makes before it answers its retries spent: FUSE's 64, lowered
    /// by a scenario that reaches the answer ([`set_rmdir_attempts_for_tests`]).
    static RMDIR_ATTEMPTS: std::cell::Cell<u32> = const { std::cell::Cell::new(64) };
}

/// Sets the attempts a served RMDIR on this thread makes, as FUSE's are 64.
pub fn set_rmdir_attempts_for_tests(attempts: u32) {
    RMDIR_ATTEMPTS.with(|set| set.set(attempts));
}

/// [`served_rmdir`]'s check loop, counting in `ended` what it met, and its outcome.
async fn rmdir(tree: &Tree, dir: NodeID, ended: &mut Rmdir) -> Outcome {
    for _ in 0..RMDIR_ATTEMPTS.with(std::cell::Cell::get) {
        let block = match tree
            .state
            .block(tree.repository.clone(), NodeBlock::index(dir))
            .await
        {
            Ok(block) => block,
            Err(error) => return failed(&error),
        };
        let (record, changes) = block.node_with_chain_changes(Node::index(dir));
        if record.is_discarded() {
            return GONE;
        }
        {
            let mut children = match StateNodeChildrenWithNameIterator::new(
                tree.state.clone(),
                tree.repository.clone(),
                dir,
            )
            .await
            {
                Ok(children) => children,
                Err(error) => return failed(&error),
            };
            loop {
                let child = match children.next().await {
                    Ok(Some((_, child, name))) => {
                        // The name holds its block's read lock, dropped before the next step.
                        drop(name);
                        child
                    }
                    Ok(None) => break,
                    Err(error) => return failed(&error),
                };
                if !child.is_staged_delete() {
                    return Outcome::Refused(Refusal::NotEmpty);
                }
            }
        }
        let read = IfUnchanged::new(block, dir, changes);
        if read.changes_since() != 0 {
            ended.rechecked += 1;
            continue;
        }
        match tree
            .state
            .served()
            .node_detach(tree.repository.clone(), dir, Detach::IfUnchanged(&read))
            .await
        {
            Ok(true) => {
                ended.removed = true;
                return Outcome::Done(Answer::None);
            }
            Ok(false) => ended.refused += 1,
            Err(error) if error.is_node_not_found() => return GONE,
            Err(error) => return failed(&error),
        }
    }
    // As FUSE's RMDIR answers `EAGAIN` once its retries are spent.
    Outcome::SelfReported(SelfReported::RetriesSpent)
}

/// A swap's copy of file `name` into `dir`, as its `copy_subtree` copies a file: a build of the
/// file alone, published, clean, needing only a live directory. Whether it attached; a name held
/// or a directory gone is no error.
pub async fn swap_attach(tree: &Tree, dir: NodeID, name: &str) -> bool {
    call_on(
        &tree.state,
        (&super::SWAP_ATTACH, naming(dir, name)),
        async {
            let node = file(name);
            let attach = tree
                .state
                .swap()
                .node_attach_tree(tree.repository.clone(), dir, &node, name)
                .await;
            let build = match building(attach) {
                Ok(build) => build,
                Err(refused) => return (false, refused),
            };
            let (copied, outcome) = published(build.publish().await);
            (copied.is_some(), outcome)
        },
    )
    .await
}

/// A swap build begun, or the outcome of one that could not begin: a refusal where the name was
/// held or the directory gone.
fn building(attach: Result<TreeAttach<'_>, StateError>) -> Result<TreeBuild<'_>, Outcome> {
    match attach {
        Ok(TreeAttach::Building(build)) => Ok(build),
        Ok(TreeAttach::Exists(_)) => Err(Outcome::Refused(Refusal::Exists)),
        Err(error) if error.is_node_not_found() => Err(GONE),
        Err(error) => Err(failed(&error)),
    }
}

/// A swap build's publish: the copy where it attached, and its outcome.
fn published(publish: Result<AttachOutcome, StateError>) -> (Option<NodeID>, Outcome) {
    match publish {
        Ok(AttachOutcome::Attached(id)) => (Some(id), Outcome::Done(Answer::Node(id))),
        Ok(AttachOutcome::Exists(_)) => (None, Outcome::Refused(Refusal::Exists)),
        Err(error) if error.is_node_not_found() => (None, GONE),
        Err(error) => (None, failed(&error)),
    }
}

/// A swap's copy of directory `name` holding files `children` into `dir`, as its `copy_subtree`
/// makes one ([`lore_revision::state::SwapNodes::node_attach_tree`]): built clean out of every
/// chain, then published whole, needing only a live directory. The copy where it attached;
/// `None` where the name is held or the directory is gone.
pub async fn swap_copy_tree(
    tree: &Tree,
    dir: NodeID,
    name: &str,
    children: &[&str],
) -> Option<NodeID> {
    swap_copy_paced(tree, (dir, name), children, || async {}).await
}

/// The directory at `path`, found by name from the root as a request resolves it, each hop noted
/// as a find; `None` where none stands there.
pub async fn find_dir(tree: &Tree, path: &str) -> Option<NodeID> {
    call(&tree.state, (&super::FIND_PATH, Some(ROOT_NODE)), async {
        let mut dir = ROOT_NODE;
        for name in path.split('/') {
            dir = match find_noted(tree, dir, hash_string(name)).await {
                Ok(Some(found)) => found,
                Ok(None) => return (None, Outcome::Answer(Answer::Found(None))),
                Err(outcome) => return (None, outcome),
            };
        }
        (Some(dir), Outcome::Answer(Answer::Found(Some(dir))))
    })
    .await
}

/// The node at `path` below `root`, as a resolution entering links on the way finds it, as the
/// commit resolves a staged path (`State::find_relative_node_link`): each hop the State records,
/// each name's hash noted; the node and where it lies, in the call's State or in the revision a
/// link it crossed pins, of another repository or at another revision than the call's State's;
/// `None` where no node stands there. The path names one name at least and none empty or
/// reserved, which the product's own split would read otherwise than this one or refuse before
/// any hop; and no link it crosses pins the call's own repository at the State's own revision,
/// which this would take for an answer in the State.
pub async fn find_linked(tree: &Tree, root: NodeID, path: &str) -> Option<NodeLink> {
    assert!(
        (path.split('/')).all(|name| !name.is_empty() && !is_reserved_node_name(name)),
        "a resolution of '{path}', with a name empty or reserved"
    );
    call(&tree.state, (&super::FIND_LINKED, Some(root)), async {
        resolving_names(&tree.state, path.split('/').map(hash_string).collect());
        let found = (tree.state)
            .find_relative_node_link(tree.repository.clone(), root, path)
            .await;
        match found {
            Ok(found)
                if found.repository == tree.repository.id
                    && found.revision == tree.state.revision() =>
            {
                (
                    Some(found),
                    Outcome::Answer(Answer::Found(Some(found.node))),
                )
            }
            Ok(found) => {
                let answer = Answer::Linked {
                    node: found.node,
                    repository: found.repository,
                    revision: found.revision,
                };
                (Some(found), Outcome::Answer(answer))
            }
            Err(error) if error.is_node_not_found() => (None, Outcome::Answer(Answer::Found(None))),
            Err(error) => (None, failed(&error)),
        }
    })
    .await
}

/// A find of `name` in directory `dir`, as a request resolves one name: the node found, or `None`
/// where no child carries it.
pub async fn find(tree: &Tree, dir: NodeID, name: &str) -> Option<NodeID> {
    call(&tree.state, (&super::FIND, Some(dir)), async {
        answered(find_noted(tree, dir, hash_string(name)).await)
    })
    .await
}

/// A find of `name` in directory `dir` through the blocks in memory alone, as FUSE's worker resolves
/// a name it must not read the store for (`find_subnode_resident`): the node found; `None` where no
/// child carries it, or, self-reported, where a block it needed was not in memory. It has no
/// scheduling point, so it runs whole between other tasks' steps.
pub async fn find_resident(tree: &Tree, dir: NodeID, name: &str) -> Option<NodeID> {
    call(&tree.state, (&super::FIND_RESIDENT, Some(dir)), async {
        let name = hash_string(name);
        let from = tree.state.hooks().seq_now();
        let Some(found) = tree.state.find_subnode_resident(dir, name).transpose() else {
            return (None, Outcome::SelfReported(SelfReported::NeedLoad));
        };
        answered(noted(&tree.state, (dir, name, from), found))
    })
    .await
}

/// The children of directory `dir` as a children iterator begun from the directory yields them,
/// as the scan and the provider diff walk one, the walk noted: the nodes, in order; `None` where
/// it failed or slowed down.
pub async fn children(tree: &Tree, dir: NodeID) -> Option<Vec<NodeID>> {
    call(&tree.state, (&super::CHILDREN, Some(dir)), async {
        let from = tree.state.hooks().seq_now();
        let repository = tree.repository.clone();
        match StateNodeChildrenIterator::new(tree.state.clone(), repository, dir).await {
            Ok(mut walk) => walk_noted(&tree.state, (dir, from), &mut walk).await,
            Err(error) => (None, failed(&error)),
        }
    })
    .await
}

/// [`children`], the walk begun from the parent's record, read with the head it begins at, as a
/// caller inspecting the parent too begins one.
pub async fn children_from_parent(tree: &Tree, dir: NodeID) -> Option<Vec<NodeID>> {
    call(
        &tree.state,
        (&super::CHILDREN_FROM_PARENT, Some(dir)),
        async {
            let from = tree.state.hooks().seq_now();
            let repository = tree.repository.clone();
            match StateNodeChildrenIterator::with_parent(tree.state.clone(), repository, dir).await
            {
                Ok((_, mut walk)) => walk_noted(&tree.state, (dir, from), &mut walk).await,
                Err(error) => (None, failed(&error)),
            }
        },
    )
    .await
}

/// [`children`] through the iterator yielding each child's name too, as the provider's listing
/// walks one, each name's spelling noted.
pub async fn named_children(tree: &Tree, dir: NodeID) -> Option<Vec<NodeID>> {
    call(&tree.state, (&super::NAMED_CHILDREN, Some(dir)), async {
        let from = tree.state.hooks().seq_now();
        match named_walk(tree, (dir, from), Filter::EVERY).await {
            Ok((walk, _)) => walk_answered(&tree.state, walk),
            Err(error) => (None, failed(&error)),
        }
    })
    .await
}

/// The children of directory `dir` as its listing by id answers them, as the provider's rename of
/// a directory reads them, the walk noted: each child by its id alone, its repeats dropped; `None`
/// where it failed.
pub async fn listed_children(tree: &Tree, dir: NodeID) -> Option<Vec<NodeID>> {
    call(&tree.state, (&super::LISTED_CHILDREN, Some(dir)), async {
        let from = tree.state.hooks().seq_now();
        let listed = tree.state.node_children(tree.repository.clone(), dir).await;
        let yielded = listed.map(|children| {
            let yielded = children.iter().map(|&node| by_id(node, None));
            yielded.collect()
        });
        listing_answered(tree, (dir, from), yielded, (Filter::EVERY, Links::Follow))
    })
    .await
}

/// [`listed_children`] with each child's name's hash, as the provider's index of a directory reads
/// them, each hash noted.
pub async fn listed_name_hashes(tree: &Tree, dir: NodeID) -> Option<Vec<NodeID>> {
    call(
        &tree.state,
        (&super::LISTED_NAME_HASHES, Some(dir)),
        async {
            let from = tree.state.hooks().seq_now();
            let listed = tree
                .state
                .node_children_with_name_hash(tree.repository.clone(), dir)
                .await;
            let yielded = listed.map(|children| {
                let yielded = children.iter().map(|&(node, name)| by_id(node, Some(name)));
                yielded.collect()
            });
            listing_answered(tree, (dir, from), yielded, (Filter::EVERY, Links::Follow))
        },
    )
    .await
}

/// A listing's mark of directory `dir`, with the id of the listing's call where it recorded one,
/// and the prepends to the directory its holder counted in it, as the stage keeps a mark beside
/// its listing (`DirectoryChildren`).
pub struct Marked {
    dir: NodeID,
    mark: ChainMark,
    listing: Option<u64>,
    prepends: u32,
}

impl Marked {
    /// Counts a prepend to the directory its holder made itself, as the stage counts an add of its
    /// own that landed ([`ChainMark::count_own_prepend`]).
    pub fn count_own_prepend(&mut self) {
        self.mark.count_own_prepend();
        self.prepends += 1;
    }

    /// On the marked directory, by the mark, searching for the name hash `name` where it names one.
    fn on(&self, name: Option<u64>) -> On {
        On {
            target: Some(self.dir),
            name,
            listing: self.listing,
            prepends: self.prepends,
            ..On::default()
        }
    }
}

/// [`listed_name_hashes`] with the mark of the chain its walk began from, as the stage lists a
/// directory before it claims the directory's entries (`stage_directory`): the nodes and the mark;
/// `None` where it failed, or where `dir` is no directory, whose listing carries no mark.
pub async fn listed_marked(tree: &Tree, dir: NodeID) -> Option<(Vec<NodeID>, Marked)> {
    call(&tree.state, (&super::LISTED_MARKED, Some(dir)), async {
        let from = tree.state.hooks().seq_now();
        let listed = tree
            .state
            .node_children_with_name_hash_marked(tree.repository.clone(), dir)
            .await;
        let (yielded, mark) = match listed {
            Ok((listed, mark)) => {
                let yielded = listed.iter().map(|&(node, name)| by_id(node, Some(name)));
                (Ok(yielded.collect()), mark)
            }
            Err(error) => (Err(error), None),
        };
        let (nodes, outcome) =
            listing_answered(tree, (dir, from), yielded, (Filter::EVERY, Links::Follow));
        let marked = mark.map(|mark| Marked {
            dir,
            mark,
            listing: open_call(),
            prepends: 0,
        });
        (nodes.zip(marked), outcome)
    })
    .await
}

/// A search of the marked directory for `name` since `marked`, as the stage's claim of an entry
/// its listing does not hold makes one (`claim_child`): a child carrying it ahead of the mark's
/// head, none, or stale; `None` where it failed.
pub async fn added_since(tree: &Tree, marked: &Marked, name: &str) -> Option<AddedSince> {
    let name = hash_string(name);
    call_on(
        &tree.state,
        (&super::ADDED_SINCE, marked.on(Some(name))),
        async {
            let searched = tree
                .state
                .find_subnode_added_since(tree.repository.clone(), &marked.mark, name)
                .await;
            match searched {
                Ok(searched) => (Some(searched), Outcome::Answer(Answer::since(searched))),
                Err(error) => (None, failed(&error)),
            }
        },
    )
    .await
}

/// Whether `marked` found its directory reattached since, as the stage asks after a search found a
/// child, to claim it in its listing too (`claim_child`).
pub async fn reattached_since(tree: &Tree, marked: &Marked) -> bool {
    call_on(
        &tree.state,
        (&super::REATTACHED_SINCE, marked.on(None)),
        async {
            let reattached = marked.mark.reattached();
            (reattached, Outcome::Answer(Answer::Reattached(reattached)))
        },
    )
    .await
}

/// How a name read reads a node's name: copied out or under its block's read lock, either with a
/// name the read path refuses skipped or not.
#[derive(Clone, Copy, Debug)]
pub enum NameRead {
    Clone,
    Ref,
    RefOrSkip,
    CloneOrSkip,
}

impl NameRead {
    pub const EVERY: [Self; 4] = [Self::Clone, Self::Ref, Self::RefOrSkip, Self::CloneOrSkip];

    /// The kind of a name read by this read.
    pub fn kind(self) -> &'static Kind {
        match self {
            Self::Clone => &super::NAME_CLONE,
            Self::Ref => &super::NAME_REF,
            Self::RefOrSkip => &super::NAME_REF_OR_SKIP,
            Self::CloneOrSkip => &super::NAME_CLONE_OR_SKIP,
        }
    }
}

/// The name of `node` as `read` reads it, as the provider, the stage and the hold read a node's
/// name, its spelling the answer: the name; `None` where it failed or skipped it.
pub async fn name_read(tree: &Tree, node: NodeID, read: NameRead) -> Option<String> {
    call(&tree.state, (read.kind(), Some(node)), async {
        let (state, repository) = (&tree.state, tree.repository.clone());
        // A name under its block's read lock is copied out at once, the lock dropped with it.
        let name = match read {
            NameRead::Clone => state.node_name_clone(repository, node).await.map(Some),
            NameRead::Ref => {
                (state.node_name_ref(repository, node).await).map(|name| Some(name.to_string()))
            }
            NameRead::RefOrSkip => (state.node_name_ref_or_skip(repository, node).await)
                .map(|name| name.map(|name| name.to_string())),
            NameRead::CloneOrSkip => state.node_name_clone_or_skip(repository, node).await,
        };
        match name {
            Ok(name) => {
                let spelled = name.as_deref().map(|name| spelling(name.as_bytes()));
                (name, Outcome::Answer(Answer::Name(spelled)))
            }
            Err(error) => (None, failed(&error)),
        }
    })
    .await
}

/// Whether `node`'s stored name is `name`, as the C API's move compares one (`move_node`): `None`
/// where it failed.
pub async fn name_is(tree: &Tree, node: NodeID, name: &str) -> Option<bool> {
    let on = On {
        target: Some(node),
        spelling: Some(spelling(name.as_bytes())),
        ..On::default()
    };
    call_on(&tree.state, (&super::NAME_IS, on), async {
        match tree
            .state
            .node_name_is(tree.repository.clone(), node, name)
            .await
        {
            Ok(is) => (Some(is), Outcome::Answer(Answer::NameIs(is))),
            Err(error) => (None, failed(&error)),
        }
    })
    .await
}

/// The path of `node` from the root, or from `ancestor` where given, as links, layers and the
/// commit spell a node's path (`node_path`, `node_path_below`), its spelling the answer: the path,
/// `None` within where `ancestor` does not hold the node; `None` where it failed.
pub async fn node_path(
    tree: &Tree,
    node: NodeID,
    ancestor: Option<NodeID>,
) -> Option<Option<String>> {
    let kind = match ancestor {
        Some(_) => &super::NODE_PATH_BELOW,
        None => &super::NODE_PATH,
    };
    let on = On {
        target: Some(node),
        destination: Some(ancestor.unwrap_or(ROOT_NODE)),
        ..On::default()
    };
    call_on(&tree.state, (kind, on), async {
        let (state, repository) = (&tree.state, tree.repository.clone());
        let path = match ancestor {
            Some(ancestor) => (state.node_path_below(repository, node, ancestor).await)
                .map(|path| path.map(|path| path.as_str().to_string())),
            None => state.node_path(repository, node).await.map(Some),
        };
        match path {
            Ok(path) => {
                let spelled = path.as_deref().map(|path| spelling(path.as_bytes()));
                (Some(path), Outcome::Answer(Answer::Path(spelled)))
            }
            Err(error) => (None, failed(&error)),
        }
    })
    .await
}

/// The paths of the dirty nodes at or below directory `root`, at `base` in the tree, as the stage
/// collects them to stage a directory (`State::collect_dirty_paths`), in one State: `None` where it
/// failed. Each path's spelling relative to `root` noted on the call, sorted. `base` is empty for
/// the State's root alone, as the product takes an empty path for its root's.
pub async fn dirty_paths(tree: &Tree, root: NodeID, base: &str) -> Option<Vec<RelativePath>> {
    assert_eq!(
        base.is_empty(),
        root == ROOT_NODE,
        "a collection's base is empty for the State's root alone"
    );
    call(&tree.state, (&super::DIRTY_PATHS, Some(root)), async {
        let base_path = RelativePath::new_from_initial_path(base).expect("a tree path");
        let paths = (tree.state)
            .collect_dirty_paths(tree.repository.clone(), root, base_path)
            .await;
        match paths {
            Ok(paths) => {
                let mut spelled: Vec<u64> = (paths.iter())
                    .map(|path| below_root(path.as_str(), base))
                    .collect();
                spelled.sort_unstable();
                collected(&tree.state, spelled);
                (Some(paths), Outcome::Answer(Answer::Collected))
            }
            Err(error) => (None, failed(&error)),
        }
    })
    .await
}

/// The spelling of `path` below `base`, the empty path for `base` itself, and the whole of a path
/// not below it, which no node below it spells.
fn below_root(path: &str, base: &str) -> u64 {
    let below = match path.strip_prefix(base) {
        Some(rest) if base.is_empty() => rest,
        Some("") => "",
        Some(rest) => rest.strip_prefix('/').unwrap_or(path),
        None => path,
    };
    path_spelled(below)
}

/// The flags a has-children read asks a directory's children for.
#[derive(Clone, Copy, Debug)]
pub enum Flagged {
    Staged,
    Dirty,
    StagedOrDirty,
}

impl Flagged {
    pub const EVERY: [Self; 3] = [Self::Staged, Self::Dirty, Self::StagedOrDirty];

    /// The kind of a has-children read asking for these flags.
    pub fn kind(self) -> &'static Kind {
        match self {
            Self::Staged => &super::HAS_STAGED_CHILDREN,
            Self::Dirty => &super::HAS_DIRTY_CHILDREN,
            Self::StagedOrDirty => &super::HAS_STAGED_OR_DIRTY_CHILDREN,
        }
    }

    /// The flags a has-children read of `kind` asks for, where it is one.
    pub fn of(kind: &Kind) -> Option<Self> {
        Self::EVERY
            .into_iter()
            .find(|flagged| std::ptr::eq(flagged.kind(), kind))
    }

    /// Whether `node` carries these flags, as the read tests each child.
    pub fn carries(self, node: &Node) -> bool {
        match self {
            Self::Staged => node.is_staged(),
            Self::Dirty => node.is_dirty(),
            Self::StagedOrDirty => node.is_staged() || node.is_dirty(),
        }
    }
}

/// Whether directory `dir` has a child carrying `flagged`'s flags, as the commit, the stage and
/// status ask whether a tree holds work (`node_has_*_children`): `None` where it failed. No link,
/// which no caller passes: the product answers none for one, as for any node that is no
/// directory.
pub async fn has_children(tree: &Tree, dir: NodeID, flagged: Flagged) -> Option<bool> {
    call(&tree.state, (flagged.kind(), Some(dir)), async {
        let (state, repository) = (&tree.state, tree.repository.clone());
        let has = match flagged {
            Flagged::Staged => state.node_has_staged_children(repository, dir).await,
            Flagged::Dirty => state.node_has_dirty_children(repository, dir).await,
            Flagged::StagedOrDirty => {
                state
                    .node_has_staged_or_dirty_children(repository, dir)
                    .await
            }
        };
        match has {
            Ok(has) => (Some(has), Outcome::Answer(Answer::Has(has))),
            Err(error) => (None, failed(&error)),
        }
    })
    .await
}

/// The children of directory `dir` as its collection answers them, leaving those staged for
/// deletion out, as the diff against a revision collects them, the walk noted: each child with
/// its name's hash; `None` where it failed.
pub async fn collected_children(tree: &Tree, dir: NodeID) -> Option<Vec<NodeID>> {
    call(
        &tree.state,
        (&super::COLLECTED_CHILDREN, Some(dir)),
        async {
            let from = tree.state.hooks().seq_now();
            let collected = tree
                .state
                .collect_children_unsorted(tree.repository.clone(), dir, false, true)
                .await;
            let yielded = collected.map(|collected| collected_by_id(&collected));
            listing_answered(
                tree,
                (dir, from),
                yielded,
                (Filter::UNDELETED, Links::Follow),
            )
        },
    )
    .await
}

/// [`collected_children`] keeping those staged for deletion and leaving symbolic links out, as a
/// VFS provider's diff collects the children a working tree versions; `os_diff` collects every
/// child (`State::collect_children_unsorted`).
pub async fn collected_versioned_children(tree: &Tree, dir: NodeID) -> Option<Vec<NodeID>> {
    call(
        &tree.state,
        (&super::COLLECTED_VERSIONED_CHILDREN, Some(dir)),
        async {
            let from = tree.state.hooks().seq_now();
            let filter = Filter::VERSIONED;
            let collected = tree
                .state
                .collect_children_unsorted_where(
                    tree.repository.clone(),
                    dir,
                    filter.deleted,
                    true,
                    filter.keep,
                )
                .await;
            let yielded = collected.map(|collected| collected_by_id(&collected));
            listing_answered(tree, (dir, from), yielded, (filter, Links::Follow))
        },
    )
    .await
}

/// [`collected_children`] with each child's name, following no link, as the projected file
/// system's enumeration of a directory collects them, each name's spelling noted.
pub async fn collected_named_children(tree: &Tree, dir: NodeID) -> Option<Vec<NodeID>> {
    call(
        &tree.state,
        (&super::COLLECTED_NAMED_CHILDREN, Some(dir)),
        async {
            let from = tree.state.hooks().seq_now();
            let collected = tree
                .state
                .collect_named_children_unsorted(tree.repository.clone(), dir, false, false)
                .await;
            let yielded = collected.map(|collected| {
                let yielded = collected.children.iter().map(|child| Yielded {
                    spelling: Some(spelling(child.name_string.as_bytes())),
                    ..by_id(child.node, Some(child.name))
                });
                yielded.collect()
            });
            listing_answered(tree, (dir, from), yielded, (Filter::UNDELETED, Links::Skip))
        },
    )
    .await
}

/// A node a listing answered by its id, and the hash of its name where it answers one.
fn by_id(node: NodeID, name: Option<u64>) -> Yielded {
    Yielded {
        node,
        record: None,
        name,
        spelling: None,
    }
}

/// The children `collected` answered, each by its id and its name's hash.
fn collected_by_id(collected: &StateChildrenNodes) -> Vec<Yielded> {
    (collected.children.iter())
        .map(|child| by_id(child.node, Some(child.name)))
        .collect()
}

/// Whether a listing follows a link into the State its pin names, or answers none over it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Links {
    Follow,
    Skip,
}

/// A listing of `dir` begun at `from` under `filter`, which answered `yielded`, its repeats
/// dropped, noted with what it walked ([`over`]) by how it takes `links`, and its answer; where it
/// failed, the failure. It notes no child passed over for its name, as no listing says it passed
/// one, and no fixture holds a name the read path refuses.
fn listing_answered(
    tree: &Tree,
    (dir, from): (NodeID, u64),
    yielded: Result<Vec<Yielded>, StateError>,
    (filter, links): (Filter, Links),
) -> (Option<Vec<NodeID>>, Outcome) {
    match yielded {
        Ok(yielded) => {
            let walk = Walk {
                dir,
                from,
                yielded,
                repeats: Repeats::Dropped,
                skipped: 0,
                filter,
                over: over(tree, dir, links),
            };
            walk_answered(&tree.state, walk)
        }
        Err(error) => (None, failed(&error)),
    }
}

/// What a listing of `dir` walked, by its kind, read with no record from its block, which the
/// State's cache keeps since the listing read it: no change of a node alters its kind while its
/// slot is used, and no caller lists a node another task may free, or runs an eviction beside a
/// listing. A link, the State its pin names where the listing follows `links`, else nothing, as a
/// listing following none answers empty over it. A slot read free walks as a chain, held by the
/// chain's rules.
fn over(tree: &Tree, dir: NodeID, links: Links) -> Over {
    let block = (tree.state.resident_block(NodeBlock::index(dir)))
        .expect("the State's cache keeps the block a listing read");
    let node = block.node(Node::index(dir));
    if node.is_link() && links == Links::Follow {
        Over::Link
    } else if node.is_link() || node.is_file() {
        Over::Leaf
    } else {
        Over::Chain
    }
}

/// A children iterator's walk of `dir`, begun at `from`, to its end, noted, and its answer.
async fn walk_noted(
    state: &State,
    (dir, from): (NodeID, u64),
    walk: &mut StateNodeChildrenIterator,
) -> (Option<Vec<NodeID>>, Outcome) {
    let mut yielded = Vec::new();
    loop {
        match walk.next().await {
            Ok(Some((node, record))) => yielded.push(Yielded {
                node,
                record: Some(record),
                name: None,
                spelling: None,
            }),
            Ok(None) => break,
            Err(error) => return (None, failed(&error)),
        }
    }
    let walk = Walk {
        dir,
        from,
        yielded,
        repeats: Repeats::reported(walk.may_repeat()),
        skipped: 0,
        filter: Filter::EVERY,
        // An iterator walks a directory's chain alone: `Node::child` asserts a directory.
        over: Over::Chain,
    };
    walk_answered(state, walk)
}

/// `walk`, which reached its end, noted: its nodes, and its answer.
fn walk_answered(state: &State, walk: Walk) -> (Option<Vec<NodeID>>, Outcome) {
    let nodes = walk.yielded.iter().map(|yielded| yielded.node).collect();
    walked(state, walk);
    (Some(nodes), Outcome::Answer(Answer::None))
}

/// A find of `name` in `dir`, noted as looked up from the `seq` it began at.
async fn find_noted(tree: &Tree, dir: NodeID, name: u64) -> Result<Option<NodeID>, Outcome> {
    let from = tree.state.hooks().seq_now();
    let found = tree
        .state
        .find_subnode(tree.repository.clone(), dir, name)
        .await;
    noted(&tree.state, (dir, name, from), found)
}

/// A find's answer of `name` in `dir`, begun at `from`, noted as looked up: the node found, or none
/// where no child carries it; the outcome of an error a run can provoke otherwise, which looks up
/// nothing.
fn noted(
    state: &State,
    (dir, name, from): (NodeID, u64, u64),
    found: Result<NodeID, StateError>,
) -> Result<Option<NodeID>, Outcome> {
    let found = match found {
        Ok(node) => Some(node),
        Err(error) if error.is_node_not_found() => None,
        Err(error) => return Err(failed(&error)),
    };
    looked_up(
        state,
        Lookup::Find {
            dir,
            name,
            found,
            from,
        },
    );
    Ok(found)
}

/// A one-name find's answer and outcome.
fn answered(found: Result<Option<NodeID>, Outcome>) -> (Option<NodeID>, Outcome) {
    match found {
        Ok(found) => (found, Outcome::Answer(Answer::Found(found))),
        Err(outcome) => (None, outcome),
    }
}

/// A served copy-up of file `node` in directory `dir`, as FUSE's `mark_local_file` makes one
/// ([`lore_revision::state::ServedNodes::node_copy_up`]): the file rewritten dirty with local
/// content; whether it did. A file or directory a concurrent removal took is no error.
pub async fn served_copy_up(tree: &Tree, node: NodeID, dir: NodeID) -> bool {
    call(&tree.state, (&super::SERVED_COPY_UP, Some(node)), async {
        leaving(Leaves::LocalCopy);
        match tree
            .state
            .served()
            .node_copy_up(
                &tree.repository,
                node,
                dir,
                (0o644, 0),
                (NodeFlags::DirtyModify, NodeExtFlags::LocalContent),
            )
            .await
        {
            Ok(()) => (true, Outcome::Done(Answer::None)),
            Err(error) if error.is_node_not_found() => (false, GONE),
            Err(error) => (false, failed(&error)),
        }
    })
    .await
}

/// A swap's rewrite of file `node` to its committed content where the file is clean, as the swap
/// rewrites a file the target changed ([`lore_revision::state::State::node_rewrite`] with
/// `Rewrite::IfClean`): whether it rewrote. A file a concurrent removal took is no error.
pub async fn swap_rewrite_if_clean(tree: &Tree, node: NodeID) -> bool {
    let built = tree.nodes.iter().find(|(_, id)| *id == node).map_or_else(
        || panic!("file {node} is not in the tree"),
        |(path, _)| path.as_str(),
    );
    call(&tree.state, (&super::SWAP_REWRITE, Some(node)), async {
        // Inside the call, as the swap reads its target's content in its own steps.
        let stored = match tree
            .committed
            .node(tree.repository.clone(), tree.committed_id(built))
            .await
        {
            Ok(stored) => stored,
            Err(error) => return (false, failed(&error)),
        };
        let content = Content {
            mode: stored.mode,
            size: stored.size,
            address: stored.address,
        };
        leaving(Leaves::Content(content));
        match tree
            .state
            .node_rewrite(&tree.repository, node, &content, Rewrite::IfClean)
            .await
        {
            Ok(true) => (true, Outcome::Done(Answer::None)),
            Ok(false) => (false, Outcome::Refused(Refusal::IfClean)),
            Err(error) if error.is_node_not_found() => (false, GONE),
            // `Discarded` since the swap found it, or not a file, under its lock.
            Err(error) if error.is_invalid_arguments() => {
                (false, Outcome::Refused(Refusal::Modify))
            }
            Err(error) => (false, failed(&error)),
        }
    })
    .await
}

/// A settle of directory `dir` as a commit's makes one, through the served clear
/// ([`lore_revision::state::ServedNodes::node_clear`]) against the tree's committed State, at the
/// place the tree built `dir` at: its action bits dropped and its clear pending, its children
/// compared with the committed directory's, then cleared where they match, the pending clear
/// survived, the count is unchanged and `dir` still stands there under its committed name.
pub async fn settle(tree: &Tree, dir: NodeID) -> Settle {
    settle_waiting(tree, dir, async {}).await
}

/// [`settle`], its clear running `between` after comparing and before its last phase: a window
/// the changes beside it land in.
pub async fn settle_waiting(tree: &Tree, dir: NodeID, between: impl Future<Output = ()>) -> Settle {
    call(&tree.state, (&super::SETTLE, Some(dir)), async {
        leaving(Leaves::Clean);
        settle_once(tree, dir, between).await
    })
    .await
}

/// [`settle_waiting`]'s composition, and its outcome.
async fn settle_once(
    tree: &Tree,
    dir: NodeID,
    between: impl Future<Output = ()>,
) -> (Settle, Outcome) {
    let (state, repository) = (&tree.state, &tree.repository);
    let not_begun = (Settle::NotBegun, Outcome::Refused(Refusal::NotBegun));
    let record = match state.node(repository.clone(), dir).await {
        Ok(record) => record,
        Err(error) if error.is_node_not_found() => return not_begun,
        Err(error) => return (Settle::NotBegun, failed(&error)),
    };
    if record.is_discarded() || !record.is_dirty() {
        return not_begun;
    }
    let overtaken = state.clears_overtaken();
    let cleared = match served_clear_waiting(tree, dir, window(state, between)).await {
        Ok(cleared) => cleared,
        Err(error) => return (Settle::NotBegun, failed(&error)),
    };
    match cleared {
        ClearOutcome::Cleared => (Settle::Cleared, Outcome::Done(Answer::None)),
        ClearOutcome::Moved => not_begun,
        ClearOutcome::Kept => {
            let refusal = match kept(tree, dir, overtaken).await {
                Ok(refusal) => refusal,
                Err(error) => return (Settle::KeptUnequal, failed(&error)),
            };
            let settled = if refusal == Refusal::Overtaken {
                Settle::KeptAtFinish
            } else {
                Settle::KeptUnequal
            };
            (settled, Outcome::Refused(refusal))
        }
    }
}

/// Why a settle's clear kept directory `dir`: overtaken where the State counted an overtaken clear
/// since `overtaken`, which tells its own apart as one settle at a time runs on a State, and which
/// a move of `dir` before the clear counts too; otherwise by where `dir` stands now, inside the
/// call: gone, moved since the clear, or there and unequal.
async fn kept(tree: &Tree, dir: NodeID, overtaken: u64) -> Result<Refusal, StateError> {
    if tree.state.clears_overtaken() > overtaken {
        return Ok(Refusal::Overtaken);
    }
    let (place, _) = built_place(tree, dir);
    Ok(match tree.state.node(tree.repository.clone(), dir).await {
        Ok(record) if record.is_discarded() => Refusal::NotFound,
        Ok(record) if (record.parent, record.name_hash) == place => Refusal::Unequal,
        Ok(_) => Refusal::Displaced,
        Err(error) if error.is_node_not_found() => Refusal::NotFound,
        Err(error) => return Err(error),
    })
}

/// [`settle`]'s served clear of directory `dir` against the tree's committed State, at the place
/// the tree built it at, its answer as it is.
pub async fn served_clear(tree: &Tree, dir: NodeID) -> Result<ClearOutcome, StateError> {
    call(&tree.state, (&super::SETTLE, Some(dir)), async {
        leaving(Leaves::Clean);
        let overtaken = tree.state.clears_overtaken();
        let cleared = served_clear_waiting(tree, dir, async {}).await;
        // In a settle's terms: a directory moved from its built place is one it does not begin.
        let outcome = match &cleared {
            Ok(ClearOutcome::Cleared) => Outcome::Done(Answer::None),
            Ok(ClearOutcome::Kept) => match kept(tree, dir, overtaken).await {
                Ok(refusal) => Outcome::Refused(refusal),
                Err(error) => failed(&error),
            },
            Ok(ClearOutcome::Moved) => Outcome::Refused(Refusal::NotBegun),
            // Gone where `dir` is: a walk's not-found may be a child's.
            Err(error) if error.is_node_not_found() && gone(tree, dir).await => GONE,
            Err(error) => failed(error),
        };
        (cleared, outcome)
    })
    .await
}

/// Whether `node` is gone: Discarded, or no node of the State's.
async fn gone(tree: &Tree, node: NodeID) -> bool {
    match tree.state.node(tree.repository.clone(), node).await {
        Ok(record) => record.is_discarded(),
        Err(error) => error.is_node_not_found(),
    }
}

/// [`served_clear`], running `between` before its last phase.
async fn served_clear_waiting(
    tree: &Tree,
    dir: NodeID,
    between: impl Future<Output = ()>,
) -> Result<ClearOutcome, StateError> {
    let repository = &tree.repository;
    let (place, path) = built_place(tree, dir);
    let against = ClearAgainst {
        state: &tree.committed,
        repository,
        node: tree.committed_id(path),
    };
    let mut scratch = ClearScratch::default();
    tree.state
        .served()
        .node_clear(
            repository,
            dir,
            ServedClear {
                place,
                against,
                spelled: against,
                scratch: &mut scratch,
            },
            between,
        )
        .await
}

/// How a [`settle`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settle {
    /// It did not begin: the directory was gone, clean, or not at its built place.
    NotBegun,
    /// The children were not the committed ones: kept.
    KeptUnequal,
    /// The children matched, but a change below landed before the clear: kept.
    KeptAtFinish,
    /// It cleared the directory.
    Cleared,
}

/// The parent and name hash the tree built node `dir` at, and its path.
fn built_place(tree: &Tree, dir: NodeID) -> ((NodeID, u64), &str) {
    let path = tree.nodes.iter().find(|(_, id)| *id == dir).map_or_else(
        || panic!("node {dir} is not in the tree"),
        |(path, _)| path.as_str(),
    );
    let place = match path.rsplit_once('/') {
        Some((parent, name)) => (tree.id(parent), hash_string(name)),
        None => (lore_revision::node::ROOT_NODE, hash_string(path)),
    };
    (place, path)
}

/// Directory `dir`, dirty, recorded added, as a local mkdir leaves it until a commit holds it: a
/// fixture's edit, no kind.
pub async fn mark_added(tree: &Tree, dir: NodeID) {
    let (state, repository) = (&tree.state, &tree.repository);
    let block = state
        .block(repository.clone(), NodeBlock::index(dir))
        .await
        .expect("the directory's block");
    let record = block.node(Node::index(dir));
    assert!(
        record.is_dirty(),
        "directory {dir} is marked added while clean"
    );
    assert!(
        state.node_change_in_place(&block, dir, (record.parent, record.name_hash), |held| {
            held.replace_dirty_action(NodeFlags::DirtyAdd)
        }),
        "directory {dir} is marked added"
    );
}

/// A commit's flip of file `node`, one the tree was built with, to the content it stored, which
/// the committed State holds at the file's built path: the file takes that content, clean,
/// served from the store, where it is still live at its built place, the place the commit stored.
pub async fn flip(tree: &Tree, node: NodeID) -> bool {
    call(&tree.state, (&super::FLIP, Some(node)), async {
        let outcome = flip_in_place(tree, node).await;
        (outcome == Outcome::Done(Answer::None), outcome)
    })
    .await
}

/// [`flip`]'s change in place, and its outcome: a refusal where the file was gone, no file, or
/// no longer at its place.
async fn flip_in_place(tree: &Tree, node: NodeID) -> Outcome {
    let (state, repository) = (&tree.state, &tree.repository);
    let (place, built) = built_place(tree, node);
    let stored = match tree
        .committed
        .node(repository.clone(), tree.committed_id(built))
        .await
    {
        Ok(stored) => stored,
        Err(error) => return failed(&error),
    };
    leaving(Leaves::Flipped(Content {
        mode: stored.mode,
        size: stored.size,
        address: stored.address,
    }));
    match state.node(repository.clone(), node).await {
        Ok(record) if record.is_discarded() => return GONE,
        Ok(_) => {}
        Err(error) if error.is_node_not_found() => return GONE,
        Err(error) => return failed(&error),
    }
    let block = match state
        .block(repository.clone(), NodeBlock::index(node))
        .await
    {
        Ok(block) => block,
        Err(error) => return failed(&error),
    };
    // The file read live, then changed under its lock at the place its commit stored, where a
    // rename or a removal may land between them. The VFS's caller tests the place on its copy
    // too, which the op leaves to the lock, so a rename at any point reaches it.
    lore_revision::state::node::hooks::reached(lore_revision::state::node::hooks::Point::Named(
        "flip's read",
    ))
    .await;
    let flipped = state.node_change_in_place(&block, node, place, |held| {
        if !held.is_file() {
            return false;
        }
        held.set_content(stored.address, stored.size, stored.mode);
        held.clear_dirty_flags()
    });
    changed(flipped)
}

/// A swap's detach of a directory it finds clean; whether it detached. A directory already
/// gone is not detached again.
pub async fn swap_detach_if_clean(tree: &Tree, node: NodeID) -> bool {
    call(
        &tree.state,
        (&super::SWAP_DETACH_IF_CLEAN, Some(node)),
        async {
            match tree
                .state
                .swap()
                .node_detach(tree.repository.clone(), node, Detach::IfClean)
                .await
            {
                Ok(true) => (true, Outcome::Done(Answer::None)),
                Ok(false) => (false, Outcome::Refused(Refusal::IfClean)),
                Err(error) if error.is_node_not_found() => (false, GONE),
                Err(error) => (false, failed(&error)),
            }
        },
    )
    .await
}

/// An `os_diff` mark of a file it found modified: forced, as `mark_settled` makes it.
pub async fn scan_mark_modified(tree: &Tree, node: NodeID) {
    call(
        &tree.state,
        (&super::SCAN_MARK_MODIFIED, Some(node)),
        async {
            leaving(Leaves::Dirty);
            let marked = tree
                .state
                .node_mark(
                    &tree.repository,
                    node,
                    Mark::dirty(
                        NodeFlags::DirtyModify,
                        Ext::Set(NodeExtFlags::NoFlags),
                        true,
                    ),
                )
                .await;
            ((), done_or_failed(marked))
        },
    )
    .await;
}

/// An unforced dirty mark of a file, in no call: no kind's, as the callers' modify marks are
/// forced ([`scan_mark_modified`]). It writes the same on a clean file, and nothing on one already
/// marked modified with the same extended flags, where the forced one writes it again and climbs
/// to the first dirty ancestor.
pub async fn mark_dirty(tree: &Tree, node: NodeID) {
    mark_dirty_as(tree, node, NodeExtFlags::NoFlags).await;
}

/// [`mark_dirty`] setting the file's extended flags to `ext`, as a written symbolic link carries
/// `Symlink`.
pub async fn mark_dirty_as(tree: &Tree, node: NodeID, ext: NodeExtFlags) {
    tree.state
        .node_mark(
            &tree.repository,
            node,
            Mark::dirty(NodeFlags::DirtyModify, Ext::Set(ext), false),
        )
        .await
        .expect("a mark succeeds");
}

/// A forced dirty mark of a deletion on `node` alone, in no call, its ancestors marked dirty: as
/// `lore dirty` marks a node it finds deleted (file/dirty.rs, `dirty_delete`), which also marks
/// each node below it the filter keeps, so a clean node below one this marks stands for one the
/// filter leaves out.
pub async fn mark_deleted(tree: &Tree, node: NodeID) {
    let mark = Mark::dirty(
        NodeFlags::DirtyDelete,
        Ext::Set(NodeExtFlags::NoFlags),
        true,
    );
    tree.state
        .node_mark(&tree.repository, node, mark)
        .await
        .expect("a mark succeeds");
}

/// An `os_diff` clear of a file it found unchanged, and of each ancestor nothing below keeps dirty.
pub async fn scan_clear(tree: &Tree, node: NodeID) {
    call(&tree.state, (&super::SCAN_CLEAR, Some(node)), async {
        leaving(Leaves::Clean);
        let cleared = tree
            .state
            .node_clear(&tree.repository, node, Clear::Dirty)
            .await;
        ((), done_or_failed(cleared))
    })
    .await;
}

/// A reset's clear of directory `dir`, which a dirty add it discarded left, and of each ancestor
/// nothing below keeps dirty.
pub async fn vacated_clear(tree: &Tree, dir: NodeID) {
    call(&tree.state, (&super::VACATED_CLEAR, Some(dir)), async {
        let cleared = tree
            .state
            .node_clear(&tree.repository, dir, Clear::Vacated)
            .await;
        ((), done_or_failed(cleared))
    })
    .await;
}

/// An `os_diff` add of directory `name` in `dir`: the node published dirty-added, then `dir`
/// marked, as the scan adds a directory it finds new.
pub async fn scan_add(tree: &Tree, dir: NodeID, name: &str) {
    call(&tree.state, (&super::SCAN_ADD, Some(dir)), async {
        let (_, outcome) = added_then_marked(tree, dir, NodeFlags::DirtyAdd, name).await;
        ((), outcome)
    })
    .await;
}

/// The provider diff's add of `name` in `dir`, a directory its walk listed without the name, as
/// its `settle_add` makes one (state/diff.rs): the node published dirty-added without a name
/// check, a file where `is_file`, then `dir` marked. `os_diff`'s add
/// (`FileDiffContext::add_new_node`) makes the same writes, and also a settled mark under a
/// staging intent and `dir`'s mark where the add failed, which this leaves out. Nothing else
/// writes the diff's State, and its settles run one at a time; the explorations beside clears and
/// detaches run it beside more than production does. The node where it was added; `None` where
/// `dir` is gone or the add failed.
pub async fn diff_add(tree: &Tree, dir: NodeID, name: &str, is_file: bool) -> Option<NodeID> {
    let flags = if is_file {
        NodeFlags::File | NodeFlags::DirtyAdd
    } else {
        NodeFlags::DirtyAdd
    };
    call(
        &tree.state,
        (&super::DIFF_ADD, Some(dir)),
        added_then_marked(tree, dir, flags, name),
    )
    .await
}

/// [`scan_add`]'s and [`diff_add`]'s composition: `name` added in `dir` with `flags`, unchecked,
/// then `dir` marked dirty. The node where it was added; `None` where `dir` is gone or the add
/// failed.
async fn added_then_marked(
    tree: &Tree,
    dir: NodeID,
    flags: NodeFlags,
    name: &str,
) -> (Option<NodeID>, Outcome) {
    let node = Node {
        flags: flags.bits(),
        name_hash: hash_string(name),
        ..Node::default()
    };
    let unchained = Chain {
        at: dir,
        added: None,
    };
    match added_in(tree, dir, &unchained, node, name).await {
        Ok(added) => {
            let mark = Mark::dirty(NodeFlags::Dirty, Ext::Keep, false);
            answered_after(tree, added, (dir, mark)).await
        }
        Err(outcome) => (None, outcome),
    }
}

/// The answer of a call that added `added`, after its last step, `mark` on `node`: the node
/// added, `node` noted where it was taken since, or the mark's error.
async fn answered_after(
    tree: &Tree,
    added: NodeID,
    (node, mark): (NodeID, Mark),
) -> (Option<NodeID>, Outcome) {
    let marked = tree.state.node_mark(&tree.repository, node, mark);
    let outcome = match taken_since(&tree.state, node, marked).await {
        Ok(()) => Outcome::Done(Answer::Node(added)),
        Err(error) => failed(&error),
    };
    (Some(added), outcome)
}

/// How a chain marks the directories it adds.
#[derive(Clone, Copy)]
enum ChainMarks {
    /// The directory above the first added, dirty, as `ensure_scan_dir_chain` (`os_diff.rs`) marks
    /// it.
    AboveFirst,
    /// Each added, dirty-added, forced, with its ancestors, as `lore dirty`'s
    /// `ensure_dirty_parent_dirs` (file/dirty.rs) marks them.
    EachForced,
}

/// Where a chain ended: its last directory, and the last it added, if any.
struct Chain {
    at: NodeID,
    added: Option<NodeID>,
}

impl Chain {
    /// The answer of a call whose last publish was this chain's last add.
    fn answer(&self) -> Outcome {
        Outcome::Done(self.added.map_or(Answer::None, Answer::Node))
    }
}

/// The directories of `names` below `start`, as `ensure_scan_dir_chain` (`os_diff.rs`) and
/// `ensure_dirty_parent_dirs` (file/dirty.rs) walk them: each found, its find noted, or, where it is
/// missing, added with `flags` without a name check and marked as `marks` says. The names are the
/// scenarios' own, none reserved, each storable, which the callers check before they add. Where
/// the chain stops, as its callers stop at an error, the outcome of its call: the error's; `start`
/// gone, refused; a directory below `start` gone or taken, noted, with what the chain published
/// last.
async fn chain(
    tree: &Tree,
    start: NodeID,
    names: &str,
    flags: NodeFlags,
    marks: ChainMarks,
) -> Result<Chain, Outcome> {
    let mut chain = Chain {
        at: start,
        added: None,
    };
    for segment in names.split('/').filter(|segment| !segment.is_empty()) {
        assert!(
            !is_reserved_node_name(segment),
            "a chain through the reserved name '{segment}'"
        );
        let name_hash = hash_string(segment);
        if let Some(found) = find_noted(tree, chain.at, name_hash).await? {
            chain.at = found;
            continue;
        }
        let node = Node {
            flags: flags.bits(),
            name_hash,
            ..Node::default()
        };
        let parent = chain.at;
        let added = added_in(tree, start, &chain, node, segment).await?;
        let marked = match marks {
            ChainMarks::AboveFirst if chain.added.is_none() => {
                Some((parent, Mark::dirty(NodeFlags::Dirty, Ext::Keep, false)))
            }
            ChainMarks::AboveFirst => None,
            ChainMarks::EachForced => Some((
                added,
                Mark::dirty(NodeFlags::DirtyAdd, Ext::Set(NodeExtFlags::NoFlags), true),
            )),
        };
        chain = Chain {
            at: added,
            added: Some(added),
        };
        if let Some((node, mark)) = marked {
            let mark = tree.state.node_mark(&tree.repository, node, mark);
            let ran = ran_unless_taken(&tree.state, node, mark).await;
            if !ran.map_err(|error| failed(&error))? {
                return Err(chain.answer());
            }
        }
    }
    Ok(chain)
}

/// `node`, named `name`, added without a name check in the last directory of `chain`, which its
/// call began at `start`. Where that directory is gone, the outcome of the call: refused where it
/// is `start`; otherwise the directory noted taken, with what the chain published last.
async fn added_in(
    tree: &Tree,
    start: NodeID,
    chain: &Chain,
    node: Node,
    name: &str,
) -> Result<NodeID, Outcome> {
    let from = tree.state.hooks().seq_now();
    let added = tree
        .state
        .node_add(tree.repository.clone(), chain.at, node, name)
        .await;
    match added {
        Ok(added) => Ok(added),
        Err(error) if !parent_gone(&error) => Err(failed(&error)),
        Err(_) if chain.at == start => Err(GONE),
        Err(_) => {
            taken(&tree.state, chain.at, from);
            Err(chain.answer())
        }
    }
}

/// `ensure_scan_dir_chain` (`os_diff.rs`), as the provider diff runs it before the checked attach
/// of a name whose parent its walk did not list, a path-filtered scan before it walks a directory
/// the State lacks, and status for the directories its untracked paths share, a level of them at
/// once (`create_shared_untracked_ancestors`): each directory of `names` below `start` found, or,
/// where it is missing, added dirty without a name check, the directory above the first added
/// marked. Answers the last directory added, or none where it added none. The last directory;
/// `None` where the chain stopped short of it.
pub async fn scan_chain(tree: &Tree, start: NodeID, names: &str) -> Option<NodeID> {
    call(&tree.state, (&super::SCAN_CHAIN, Some(start)), async {
        let chained = chain(
            tree,
            start,
            names,
            NodeFlags::DirtyAdd,
            ChainMarks::AboveFirst,
        );
        match chained.await {
            Ok(chain) => (Some(chain.at), chain.answer()),
            Err(outcome) => (None, outcome),
        }
    })
    .await
}

/// The provider diff's settle of an add whose parent its walk did not list (state/diff.rs,
/// `settle_add`): the directories to the parent found or added ([`scan_chain`]), then the checked
/// attach of file `name` in it ([`checked_create`]), two calls, as the settle makes them in turn.
/// `None` where the chain stopped short or the parent is gone.
pub async fn chain_checked_create(
    tree: &Tree,
    start: NodeID,
    names: &str,
    name: &str,
) -> Option<AttachOutcome> {
    let dir = scan_chain(tree, start, names).await?;
    checked_create(tree, dir, name).await
}

/// `lore dirty`'s add of `name` below the directories `names` reaches from `start` (file/dirty.rs,
/// `ensure_dirty_parent_dirs`, then `dirty_add` or `dirty_add_directory`): each missing directory
/// added clean and marked dirty-added, forced, with its ancestors; then the node, a file where
/// `is_file`, added clean without a name check, as `lore dirty` adds only a name its lookup found
/// untracked and runs its paths one at a time, and marked dirty-added, forced, with its ancestors.
/// Nothing else writes the staged State; the explorations beside clears and detaches run it beside
/// more than production does. The node where it was added; `None` where the chain stopped short,
/// the directory is gone or the add failed.
pub async fn dirty_add(
    tree: &Tree,
    start: NodeID,
    names: &str,
    name: &str,
    is_file: bool,
) -> Option<NodeID> {
    call(&tree.state, (&super::DIRTY_ADD, Some(start)), async {
        let chained = chain(
            tree,
            start,
            names,
            NodeFlags::empty(),
            ChainMarks::EachForced,
        );
        let chain = match chained.await {
            Ok(chain) => chain,
            Err(outcome) => return (None, outcome),
        };
        let node = if is_file { file(name) } else { directory(name) };
        match added_in(tree, start, &chain, node, name).await {
            Ok(added) => {
                let mark = Mark::dirty(NodeFlags::DirtyAdd, Ext::Set(NodeExtFlags::NoFlags), true);
                answered_after(tree, added, (added, mark)).await
            }
            Err(outcome) => (None, outcome),
        }
    })
    .await
}

/// The most entries a stage's claims in a directory take, within the 31 claims past its answer's
/// the model watches: two its listing's, three an entry's at most, a stale search, its find through
/// the chain and a reattach read, or two, a search answering none and its mark's head kept, and one
/// node taken where the claims stop.
const MOST_ENTRIES: usize = 9;

/// What the stage's claims in a directory came to ([`stage_claim`]): what its listing held, in
/// sibling order; each entry's node, in order, claimed from its listing, found since its mark or
/// added; and the listed children no entry claimed, in sibling order, each a path the tree holds
/// and the entries do not.
#[derive(Debug)]
pub struct Claimed {
    pub listed: Vec<NodeID>,
    pub nodes: Vec<NodeID>,
    pub unclaimed: Vec<NodeID>,
}

/// The stage's listing of directory `dir` and its claims of `entries`, each a name and whether it
/// names a file, as `stage_directory` (stage.rs) makes them for the directory's entries on disk:
/// the listing marked; each entry claimed from it or searched for since its mark (`claim_child`),
/// each read noted; an entry the directory holds none of added clean and marked staged and dirty
/// added, forced, with its ancestors (`stage_node_from_metadata`), counted in the mark once marked.
/// One call, beside nothing that frees a published slot while its mark lives (the mark's
/// precondition), which nothing drops, as `lore stage` runs under `block_on_command`. Scheduling
/// points follow the listing, as the stage reads the directory on disk there, and each entry
/// claimed, as the stage reads an existing node's block there (`Point::StageWindow`). The entries
/// are the scenarios' own: distinct names, none reserved, [`MOST_ENTRIES`] at most. Names sharing
/// a hash reach the claims, as the stage's default case mode leaves them; beyond the product, the
/// claims go on past a second claim of a hash answering a node spelled otherwise, where that mode
/// stops with an error. What it claimed; `None` where a step failed, the directory was gone, or a
/// node it added was taken.
pub async fn stage_claim(tree: &Tree, dir: NodeID, entries: &[(&str, bool)]) -> Option<Claimed> {
    stage_claim_windowed(tree, dir, entries, async {}, |_, _| async {}).await
}

/// [`stage_claim`], awaiting `between`, a window of its call, after its listing's scheduling point.
pub async fn stage_claim_waiting(
    tree: &Tree,
    dir: NodeID,
    entries: &[(&str, bool)],
    between: impl Future<Output = ()>,
) -> Option<Claimed> {
    stage_claim_windowed(tree, dir, entries, between, |_, _| async {}).await
}

/// [`stage_claim`], awaiting `after(index, node)`, a window of its call, once each entry's claim or
/// add answered `node`, as the stage descends into a directory entry there inline where no permit
/// is free, a child's claims there calls of their own.
pub async fn stage_claim_descending<F: Future<Output = ()>>(
    tree: &Tree,
    dir: NodeID,
    entries: &[(&str, bool)],
    after: impl FnMut(usize, NodeID) -> F,
) -> Option<Claimed> {
    stage_claim_windowed(tree, dir, entries, async {}, after).await
}

/// [`stage_claim`], awaiting both windows: `between` as [`stage_claim_waiting`] does, `after` as
/// [`stage_claim_descending`] does.
pub async fn stage_claim_windowed<F: Future<Output = ()>>(
    tree: &Tree,
    dir: NodeID,
    entries: &[(&str, bool)],
    between: impl Future<Output = ()>,
    after: impl FnMut(usize, NodeID) -> F,
) -> Option<Claimed> {
    assert!(
        entries.len() <= MOST_ENTRIES,
        "a stage's claims of {} entries, past the {MOST_ENTRIES} whose claims the model watches",
        entries.len()
    );
    for (at, &(name, _)) in entries.iter().enumerate() {
        assert!(
            !is_reserved_node_name(name),
            "a stage's claim of the reserved name '{name}'"
        );
        assert!(
            !entries[..at].iter().any(|&(other, _)| other == name),
            "a stage's claims of '{name}' twice"
        );
    }
    call(&tree.state, (&super::STAGE_CLAIM, Some(dir)), async {
        match claims_in(tree, dir, entries, between, after).await {
            Ok(claimed) => (Some(claimed), Outcome::Done(Answer::None)),
            Err(outcome) => (None, outcome),
        }
    })
    .await
}

/// [`stage_claim_windowed`]'s steps in its call, as `stage_directory` (stage.rs) claims its
/// entries: what it claimed, or the outcome of the step that stopped it, as the stage stops at an
/// entry's error. Its directory gone, or a node it added taken
/// before its mark, once an add of its own landed, the add stands: the directory or the node
/// noted taken, the call done.
async fn claims_in<F: Future<Output = ()>>(
    tree: &Tree,
    dir: NodeID,
    entries: &[(&str, bool)],
    between: impl Future<Output = ()>,
    mut after: impl FnMut(usize, NodeID) -> F,
) -> Result<Claimed, Outcome> {
    let state = &tree.state;
    let from = state.hooks().seq_now();
    let (listed, mark) = (state
        .node_children_with_name_hash_marked(tree.repository.clone(), dir)
        .await)
        .map_err(|error| failed(&error))?;
    let mark = mark.expect("the stage lists a directory, whose listing carries a mark");
    let listed_nodes = listed.iter().map(|&(node, _)| node).collect();
    let yielded = listed.iter().map(|&(node, name)| by_id(node, Some(name)));
    let walk = Walk {
        dir,
        from,
        yielded: yielded.collect(),
        repeats: Repeats::Dropped,
        skipped: 0,
        filter: Filter::EVERY,
        // A listing carrying a mark walked a directory's chain.
        over: Over::Chain,
    };
    walked(state, walk);
    hooks::reached(Point::Named("listed")).await;
    let mut marked = Marked {
        dir,
        mark,
        listing: None,
        prepends: 0,
    };
    let mut children = DirectoryChildren::new(listed, None);
    window(state, between).await;
    let (mut nodes, mut landed) = (Vec::with_capacity(entries.len()), false);
    for (index, &(name, is_file)) in entries.iter().enumerate() {
        let claimed = claim_child(tree, &mut children, &marked, hash_string(name)).await;
        if let Some(claimed) = after_adds(claimed, landed)? {
            hooks::reached(Point::StageWindow).await;
            nodes.push(claimed);
            window(state, after(index, claimed)).await;
            continue;
        }
        let node = if is_file { file(name) } else { directory(name) };
        let from = state.hooks().seq_now();
        let added = match state
            .node_add(tree.repository.clone(), dir, node, name)
            .await
        {
            Ok(added) => added,
            Err(error) if !parent_gone(&error) => return after_adds(Err(failed(&error)), landed),
            Err(_) if !landed => return Err(GONE),
            Err(_) => {
                taken(state, dir, from);
                return Err(Outcome::Done(Answer::None));
            }
        };
        landed = true;
        let mark = Mark::staged_and_dirty(NodeFlags::StagedAdd, NodeFlags::DirtyAdd);
        let mark = state.node_mark(&tree.repository, added, mark);
        let ran = ran_unless_taken(state, added, mark).await;
        if !after_adds(ran.map_err(|error| failed(&error)), landed)? {
            return Err(Outcome::Done(Answer::None));
        }
        marked.count_own_prepend();
        nodes.push(added);
        window(state, after(index, added)).await;
    }
    Ok(Claimed {
        listed: listed_nodes,
        nodes,
        unclaimed: children.unclaimed().collect(),
    })
}

/// `step`'s answer in a stage's claims, passed on; panics where it self-reported once an add of the
/// claims' own `landed`, as the model takes a call's self-report for one that changed nothing, the
/// add another step's. No run reaches one: a `SlowDown` needs the chain to change at every attempt
/// the product's retry bound allows, and no scenario fills a name table beside the kind.
fn after_adds<T>(step: Result<T, Outcome>, landed: bool) -> Result<T, Outcome> {
    if landed && let Err(Outcome::SelfReported(reported)) = &step {
        panic!(
            "a stage's claims self-reported {reported:?} after an add of their own landed, which \
             the model would take for a change of the self-reporting step's"
        );
    }
    step
}

/// The child of `marked`'s directory `name_hash` belongs to, as `claim_child` (stage.rs) answers,
/// each read by the mark noted with the prepends counted in it: the listed child carrying it that
/// nothing claimed yet; else a search since the mark, a stale one followed by a find through the
/// whole chain, then, for a node either found, the mark's reattach read, a reattach claiming the
/// node in the listing too; else the listed child carrying it, claimed or not. None where the
/// directory holds none; the outcome of a read that failed.
async fn claim_child(
    tree: &Tree,
    children: &mut DirectoryChildren,
    marked: &Marked,
    name_hash: u64,
) -> Result<Option<NodeID>, Outcome> {
    if let Some(node) = children.claim(name_hash) {
        return Ok(Some(node));
    }
    let (state, dir, prepends) = (&tree.state, marked.dir, marked.prepends);
    let from = state.hooks().seq_now();
    let searched = (state
        .find_subnode_added_since(tree.repository.clone(), &marked.mark, name_hash)
        .await)
        .map_err(|error| failed(&error))?;
    let since = Lookup::Since {
        dir,
        name: name_hash,
        searched,
        prepends,
        from,
        to: state.hooks().seq_now(),
    };
    looked_up(state, since);
    let found = match searched {
        AddedSince::Found(node) => node,
        AddedSince::Absent => return Ok(children.holds(name_hash)),
        AddedSince::Stale => match find_noted(tree, dir, name_hash).await? {
            Some(node) => node,
            None => return Ok(children.holds(name_hash)),
        },
    };
    let from = state.hooks().seq_now();
    let reattached = marked.mark.reattached();
    let read = Lookup::Reattached {
        dir,
        reattached,
        prepends,
        from,
        to: state.hooks().seq_now(),
    };
    looked_up(state, read);
    if reattached {
        children.claim_node(found);
    }
    Ok(Some(found))
}

/// An unserved detach of `node`, as the pre-serving writers remove a node the local tree replaced;
/// whether it detached. A node already gone is not detached again.
pub async fn unserved_detach(tree: &Tree, node: NodeID) -> bool {
    call(&tree.state, (&super::UNSERVED_DETACH, Some(node)), async {
        match tree
            .state
            .node_detach(tree.repository.clone(), node, Detach::Always)
            .await
        {
            Ok(true) => (true, Outcome::Done(Answer::None)),
            Ok(false) => panic!("an unserved detach that refuses nothing refused"),
            Err(error) if error.is_node_not_found() => (false, GONE),
            Err(error) => (false, failed(&error)),
        }
    })
    .await
}

/// A file node named `name`, clean.
pub fn file(name: &str) -> Node {
    Node {
        flags: NodeFlags::File.bits(),
        name_hash: hash_string(name),
        ..Node::default()
    }
}

/// A directory node named `name`, clean.
pub fn directory(name: &str) -> Node {
    Node {
        name_hash: hash_string(name),
        ..Node::default()
    }
}

/// An add of file `name` below `dir`, then its fused discard, as a reset discards what it added:
/// in a directory nothing else changes, as a discard runs where nothing writes or walks the chain
/// it unlinks from.
pub async fn add_discard(tree: &Tree, dir: NodeID, name: &str) {
    call(&tree.state, (&super::ADD_DISCARD, Some(dir)), async {
        let (state, repository) = (&tree.state, &tree.repository);
        let added = match state
            .node_add(repository.clone(), dir, file(name), name)
            .await
        {
            Ok(added) => added,
            Err(error) if parent_gone(&error) => return ((), GONE),
            Err(error) => return ((), failed(&error)),
        };
        ((), discarded(tree, added).await)
    })
    .await;
}

/// An add of directory `name` below `dir`, holding file `f` and directory `s` of file `g`, then
/// the directory's fused discard, which frees `s` in a task of its own.
pub async fn add_discard_tree(tree: &Tree, dir: NodeID, name: &str) {
    call(&tree.state, (&super::ADD_DISCARD_TREE, Some(dir)), async {
        let (state, repository) = (&tree.state, &tree.repository);
        let add = async |into: NodeID, node: Node, name: &str| {
            state.node_add(repository.clone(), into, node, name).await
        };
        let added = match add(dir, directory(name), name).await {
            Ok(added) => added,
            Err(error) if parent_gone(&error) => return ((), GONE),
            Err(error) => return ((), failed(&error)),
        };
        let filled = async {
            add(added, file("f"), "f").await?;
            let below = add(added, directory("s"), "s").await?;
            add(below, file("g"), "g").await
        };
        if let Err(error) = filled.await {
            return ((), failed(&error));
        }
        ((), discarded(tree, added).await)
    })
    .await;
}

/// The outcome of the fused discard of `node`, which nothing else changes: what it freed.
async fn discarded(tree: &Tree, node: NodeID) -> Outcome {
    let state = &tree.state;
    match lore_revision::state::node_discard_fused(state.clone(), tree.repository.clone(), node)
        .await
    {
        Ok(counts) => Outcome::Done(freed(counts)),
        Err(error) => failed(&error),
    }
}

/// The pre-serving respelling of `node` to `name` where it stands (fs/fuse/filesystem.rs,
/// `respell`): its stored name read and compared, then, where it differs, the rename and the
/// node's own mark, `Dirty`, `Keep`, unforced, with its ancestors (`respelled`). Its callers change
/// only the name's case; a scenario's rename may change its hash, which no caller does, where the
/// scenario says why.
pub async fn rename_then_mark(tree: &Tree, node: NodeID, name: &str) {
    let mark = Mark::dirty(NodeFlags::Dirty, Ext::Keep, false);
    respelled(tree, &super::RENAME, (node, name), mark).await;
}

/// `lore stage --case rename`'s respelling of `node`, which a stage's claim found, to `name`, the
/// entry's spelling of the stored name in another case (stage.rs, the case mismatch under
/// `StageCaseChange::Rename`): the stored name read and compared, then, where it differs, the
/// rename, keeping the node's hash and place, and the node's own staged mark, `StagedMove`, forced,
/// with its ancestors (`respelled`); the stage's node flags are none here, as only a stage keeping
/// the case sets any. Made in the window a stage claim leaves after the node's claim
/// ([`stage_claim_descending`]), as the stage respells there, on a node the claim found, not one
/// it added; the stage's undelete of a node staged for deletion, before the name read, is not the
/// op's.
pub async fn stage_respell(tree: &Tree, node: NodeID, name: &str) {
    let mark = Mark::staged(NodeFlags::StagedMove, true);
    respelled(tree, &super::STAGE_RESPELL, (node, name), mark).await;
}

/// A respelling's call of `kind` on `node`: its stored name read under its block's read lock and
/// compared with `name`; where they differ, [`State::node_rename`] and the node's own `mark`, the
/// node passed over where a detach took it before the mark, which only a dirty half can find, as
/// a staged half marks a `Discarded` node. A node gone before the rename, `GONE`. A stage's
/// respelling keeps the name's hash, which it asserts.
async fn respelled(tree: &Tree, kind: &'static Kind, (node, name): (NodeID, &str), mark: Mark) {
    let on = On {
        target: Some(node),
        name: Some(hash_string(name)),
        spelling: Some(spelling(name.as_bytes())),
        ..On::default()
    };
    call_on(&tree.state, (kind, on), async {
        leaving(Leaves::Named);
        let (state, repository) = (&tree.state, &tree.repository);
        let respelling = async {
            {
                // The name's lock lasts the comparison alone.
                let stored = state.node_name_ref(repository.clone(), node).await?;
                if &*stored == name {
                    return Ok(());
                }
                assert!(
                    !std::ptr::eq(kind, &super::STAGE_RESPELL)
                        || hash_string(&stored) == hash_string(name),
                    "a stage's respelling of '{}' to '{name}' changes its hash",
                    &*stored
                );
            }
            state
                .node_rename(repository.clone(), node, NameChange::Rename(name))
                .await?;
            taken_since(state, node, state.node_mark(repository, node, mark)).await
        };
        ((), done_or_failed(respelling.await))
    })
    .await;
}

/// The provider diff's undelete of `node`, as `os_diff`'s (`staged_entry`): its staged flags
/// cleared, the node alone, leaving out the settled mark that follows it there.
pub async fn staged_clear(tree: &Tree, node: NodeID) {
    call(&tree.state, (&super::STAGED_CLEAR, Some(node)), async {
        leaving(Leaves::Unstaged);
        let cleared = tree
            .state
            .node_clear(&tree.repository, node, Clear::Staged)
            .await;
        ((), done_or_failed(cleared))
    })
    .await;
}

/// The pre-serving writers' rewrite of file `node` to `content`, dirty in the same write, then
/// `parent`'s mark: the ancestors marked after, from the parent up. Unlike theirs, it sets no
/// `LocalContent`, which no unserved fixture holds.
pub async fn dirty_rewrite(tree: &Tree, node: NodeID, parent: NodeID, content: &Content) {
    call(&tree.state, (&super::DIRTY_REWRITE, Some(node)), async {
        leaving(Leaves::Content(*content));
        let dirty = Rewrite::Dirty(NodeFlags::DirtyModify, NodeExtFlags::NoFlags);
        match tree
            .state
            .node_rewrite(&tree.repository, node, content, dirty)
            .await
        {
            // Only a rewrite if clean answers that it did not write.
            Ok(rewrote) => assert!(rewrote, "the dirty rewrite of file {node} writes"),
            Err(error) if error.is_node_not_found() => return ((), GONE),
            // `Discarded`, not a file, or staged for deletion under its lock.
            Err(error) if error.is_invalid_arguments() => {
                return ((), Outcome::Refused(Refusal::Modify));
            }
            Err(error) => return ((), failed(&error)),
        }
        let marked = tree.state.node_mark(
            &tree.repository,
            parent,
            Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
        );
        (
            (),
            done_or_failed(taken_since(&tree.state, parent, marked).await),
        )
    })
    .await;
}

/// The outcome of a change that answers whether it changed: a refusal where its node was gone,
/// moved or renamed, or where it found nothing to change.
fn changed(changed: bool) -> Outcome {
    if changed {
        Outcome::Done(Answer::None)
    } else {
        Outcome::Refused(Refusal::Declined)
    }
}

/// An in-place change of file `node`'s size, where it still stands under `place`'s parent and
/// name; whether it changed.
pub async fn in_place_size(tree: &Tree, node: NodeID, place: (NodeID, u64), size: u64) -> bool {
    call(&tree.state, (&super::IN_PLACE, Some(node)), async {
        leaving(Leaves::Size(size));
        let block = match node_block(tree, node).await {
            Ok(block) => block,
            Err(failed) => return (false, failed),
        };
        let done = tree
            .state
            .node_change_in_place(&block, node, place, |held| {
                held.set_size(size);
                true
            });
        (done, changed(done))
    })
    .await
}

/// An owned change of file `node`'s size, as the scan records a size it observed, on a node no
/// task frees, moves or renames; whether it changed.
pub async fn owned_size(tree: &Tree, node: NodeID, size: u64) -> bool {
    call(&tree.state, (&super::OWNED, Some(node)), async {
        leaving(Leaves::Size(size));
        let block = match node_block(tree, node).await {
            Ok(block) => block,
            Err(failed) => return (false, failed),
        };
        let done = tree.state.node_change_owned(&block, node, |held| {
            held.set_size(size);
            true
        });
        (done, changed(done))
    })
    .await
}

/// An owned clear of `node`'s dirty flags.
pub async fn owned_clear(tree: &Tree, node: NodeID) {
    call(&tree.state, (&super::OWNED_CLEAR, Some(node)), async {
        leaving(Leaves::Clean);
        let block = match node_block(tree, node).await {
            Ok(block) => block,
            Err(failed) => return ((), failed),
        };
        let cleared = tree.state.node_clear_own(&block, node, OwnFlags::Dirty);
        ((), changed(cleared))
    })
    .await;
}

/// The block holding `node`, or the outcome of the error loading it.
async fn node_block(tree: &Tree, node: NodeID) -> Result<Arc<NodeBlock>, Outcome> {
    tree.state
        .block(tree.repository.clone(), NodeBlock::index(node))
        .await
        .map_err(|error| failed(&error))
}

/// A repin of link `node` to `pin`: a whole pin through the link's block, loaded first, as staging,
/// unstaging and merging repin one; a revision's alone as a link's update does.
pub async fn repin(tree: &Tree, node: NodeID, pin: LinkPin) {
    call(&tree.state, (&super::REPIN, Some(node)), async {
        leaving(match pin {
            LinkPin::Whole { address, child } => Leaves::Pinned {
                hash: address.hash,
                child: Some(child),
            },
            LinkPin::Revision(hash) => Leaves::Pinned { hash, child: None },
        });
        let repinned = match pin {
            LinkPin::Whole { .. } => tree
                .state
                .block(tree.repository.clone(), NodeBlock::index(node))
                .await
                .and_then(|block| tree.state.node_set_link_target_in(&block, node, pin)),
            LinkPin::Revision(_) => {
                tree.state
                    .node_set_link_target(&tree.repository, node, pin)
                    .await
            }
        };
        ((), done_or_failed(repinned))
    })
    .await;
}

/// A C API add of `node` named `name` into `dir`: the node where it published; `None` where the
/// directory is gone or below a pending delete's root, which refuses it.
pub async fn staged_add(tree: &Tree, dir: NodeID, node: Node, name: &str) -> Option<NodeID> {
    call(&tree.state, (&super::STAGED_ADD, Some(dir)), async {
        match tree
            .state
            .node_add_staged(tree.repository.clone(), dir, node, name)
            .await
        {
            Ok(added) => (Some(added), Outcome::Done(Answer::Node(added))),
            Err(error) if error.is_node_not_found() => (None, GONE),
            Err(error) if error.is_invalid_arguments() => (None, Outcome::Refused(Refusal::Add)),
            Err(error) => (None, failed(&error)),
        }
    })
    .await
}

/// A C API modify of file `node` to `content`; whether it modified: a file a delete staged first
/// refuses it.
pub async fn staged_modify(tree: &Tree, node: NodeID, content: &Content) -> bool {
    call(&tree.state, (&super::STAGED_MODIFY, Some(node)), async {
        leaving(Leaves::Content(*content));
        match tree
            .state
            .node_modify_staged(&tree.repository, node, content)
            .await
        {
            Ok(_) => (true, Outcome::Done(Answer::None)),
            Err(error) if error.is_invalid_arguments() => {
                (false, Outcome::Refused(Refusal::Modify))
            }
            Err(error) => (false, failed(&error)),
        }
    })
    .await
}

/// A C API delete of file `node` below `parent`, under the moves permit.
pub async fn staged_delete_level(tree: &Tree, node: NodeID, parent: NodeID) {
    call(&tree.state, (&super::STAGED_DELETE, Some(node)), async {
        leaving(Leaves::StagedDelete);
        let _permit = tree.state.moves_permit().await.expect("the moves permit");
        let outcome = match tree
            .state
            .node_delete_level(&tree.repository, node, parent)
            .await
        {
            Ok(LevelMark::Marked(_)) => Outcome::Done(Answer::None),
            Ok(LevelMark::Staged) => Outcome::Refused(Refusal::DeleteLevelStaged),
            Ok(LevelMark::Other) => Outcome::Refused(Refusal::DeleteLevelOther),
            Err(error) if error.is_node_not_found() => GONE,
            Err(error) => failed(&error),
        };
        ((), outcome)
    })
    .await;
}

/// A C API restore of `node`, staged for deletion, to `content`; whether it restored: another
/// task's restore may take it back first.
pub async fn restore(tree: &Tree, node: NodeID, content: &Content) -> bool {
    call(&tree.state, (&super::RESTORE, Some(node)), async {
        leaving(Leaves::Restored);
        match tree
            .state
            .node_restore(&tree.repository, node, content)
            .await
        {
            Ok(_) => (true, Outcome::Done(Answer::None)),
            Err(error) if error.is_invalid_arguments() => {
                (false, Outcome::Refused(Refusal::Restore))
            }
            Err(error) => (false, failed(&error)),
        }
    })
    .await
}

/// A C API move of `node` into `to` as `name`, under the moves permit, as the C API moves an entry
/// after one of its batch that failed: the move walks the destination's ancestors itself.
pub async fn staged_move(tree: &Tree, node: NodeID, to: NodeID, name: &str) {
    staged_move_planned(tree, (node, to, name), false).await;
}

/// A C API move of `node` into `to` as `name`, under the moves permit (lore's `move_batch`). Where
/// `walked`, as for an entry whose entries before it all applied: the plan's walk first
/// (`plan_entries`), which, for a directory going to another parent, walks the destination's
/// ancestors and refuses a move into the node's own subtree, then the move told it walked. Where
/// not, the move walks itself.
pub async fn staged_move_planned(
    tree: &Tree,
    (node, to, name): (NodeID, NodeID, &str),
    walked: bool,
) {
    call_on(
        &tree.state,
        (&super::STAGED_MOVE, moving(node, to, name)),
        async {
            leaving(Leaves::Placed);
            let permit = tree.state.moves_permit().await.expect("the moves permit");
            if walked {
                match closes_a_loop(tree, node, to).await {
                    Ok(false) => {}
                    Ok(true) => return ((), Outcome::Refused(Refusal::Move)),
                    Err(error) => return ((), gone_or_failed(&error)),
                }
            }
            let moved = tree
                .state
                .node_move_staged(
                    tree.repository.clone(),
                    node,
                    to,
                    (name, hash_string(name)),
                    &permit,
                    walked,
                )
                .await;
            let outcome = match moved {
                // But a name table with no room, self-reported.
                Err(error) if error.is_invalid_arguments() && !names_full(&error.to_string()) => {
                    Outcome::Refused(Refusal::Move)
                }
                moved => done_or_failed(moved),
            };
            ((), outcome)
        },
    )
    .await;
}

/// The C API plan's walk for a move of `node` into `to` (lore's `closes_a_loop`), with no other
/// entry in its batch: where `node` is a directory going to another parent, whether `to` or an
/// ancestor of it is `node`, read up the parents one at a time; an error for ancestors that loop.
async fn closes_a_loop(tree: &Tree, node: NodeID, to: NodeID) -> Result<bool, StateError> {
    let (state, repository) = (&tree.state, &tree.repository);
    let record = state.node(repository.clone(), node).await?;
    if !record.is_directory() || record.parent == to {
        return Ok(false);
    }
    let mut cycle = SiblingCycleGuard::new(node);
    let mut ancestor = to;
    while ancestor.is_valid_node_id() {
        if ancestor == node {
            return Ok(true);
        }
        if cycle.observe(ancestor).is_err() {
            return Err(StateError::internal(format!(
                "the ancestors of node {to} loop"
            )));
        }
        ancestor = state.node(repository.clone(), ancestor).await?.parent;
    }
    Ok(false)
}

/// A C API delete of directory `node`, added staged, below `parent` with `name_hash`: under the
/// moves permit, which it holds through `holding` first, detached and queued pending, its subtree
/// marked. Whether it detached the directory: one gone, a delete's before it, is no error.
pub async fn pend(
    tree: &Tree,
    (node, parent, name_hash): (NodeID, NodeID, u64),
    holding: impl Future<Output = ()>,
) -> bool {
    let on = On {
        place: Some((parent, name_hash)),
        ..On::at(Some(node))
    };
    call_on(&tree.state, (&super::PEND, on), async {
        let _permit = tree.state.moves_permit().await.expect("the moves permit");
        window(&tree.state, holding).await;
        match tree
            .state
            .node_discard_pending(tree.repository.clone(), node, parent, name_hash)
            .await
        {
            Ok(Some(_)) => (true, Outcome::Done(Answer::None)),
            // Not at its place: gone, or moved or renamed since the delete planned it.
            Ok(None) => (false, GONE),
            Err(error) if error.is_node_not_found() => (false, GONE),
            Err(error) => (false, failed(&error)),
        }
    })
    .await
}

/// The most futures a level of [`delete_batch`], or its detaches, spread over; the C API spreads
/// them over one per processor.
const BATCH_GROUPS: usize = 2;

/// How a step of [`delete_batch`] on an entry ended: done, answering `T`; refused, its node gone,
/// moved or of no name; or failed on an error.
enum Step<T> {
    Done(T),
    Refused,
    Failed,
}

/// How an entry of [`delete_batch`] went: whole where every step on it was done, `Some(false)`
/// where one was refused, `None` where one failed on an error.
fn went_after(went: Option<bool>, step: &Step<impl Sized>) -> Option<bool> {
    match step {
        Step::Done(_) => went,
        Step::Refused => went.map(|_| false),
        Step::Failed => None,
    }
}

/// A C API delete of the subtrees `roots` in one batch (`lore/src/revision_tree/delete.rs`, `plan`
/// and `apply`), under the moves permit its call takes for the whole batch, as the C API's task
/// takes it: each root read, one gone, staged for deletion or of no name refusing its entry; then,
/// in a window of the call, so each step is a call of its own, held to the permit by its batch's
/// rule (C10), the tracked roots staged for deletion a level at a time, each level's nodes spread
/// over at most [`BATCH_GROUPS`] futures it spawns, or marked in place where one would take them
/// all, each node a call of its own, its children read from the chain head its mark read; then
/// every added root an entry reached detached onto the pending queue, grouped by parent, the
/// groups spread the same way, each root a call of its own. A step that fails, or finds its node
/// elsewhere, fails its entry, and the rest go on; nothing is undone. Its roots hold no tracked
/// node below an added one, which the C API's check of a mixed subtree refuses, so the op makes no
/// such check. How each entry went ([`went_after`]).
pub async fn delete_batch(tree: &Tree, roots: &[NodeID]) -> Vec<Option<bool>> {
    call(&tree.state, (&super::DELETE_BATCH, None), async {
        let permit = tree.state.moves_permit().await.expect("the moves permit");
        let mut planned = Vec::with_capacity(roots.len());
        for &root in roots {
            match tree.state.node(tree.repository.clone(), root).await {
                Ok(node) if !node.is_discarded() && !node.is_staged_delete() => {
                    planned.push(Some((
                        root,
                        node.parent,
                        node.name_hash,
                        node.is_staged_add(),
                    )));
                }
                Ok(_) => planned.push(None),
                Err(error) => {
                    drop(permit);
                    return (vec![None; roots.len()], failed(&error));
                }
            }
        }
        let applied = batch_applied(&tree.state, &tree.repository, &planned);
        let went = window(&tree.state, applied).await;
        drop(permit);
        (went, Outcome::Done(Answer::None))
    })
    .await
}

/// A tracked node of a level of [`delete_batch`]: the node, its directory and its entry.
type Tracked = (NodeID, NodeID, usize);

/// An added root of [`delete_batch`]: the node, its directory, its name's hash and its entry.
type AddedRoot = (NodeID, NodeID, u64, usize);

/// [`delete_batch`]'s steps below its reads, on `planned`, each entry's root, its directory, its
/// name's hash and whether it is added, or none where its read refused it.
async fn batch_applied(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    planned: &[Option<(NodeID, NodeID, u64, bool)>],
) -> Vec<Option<bool>> {
    let mut went: Vec<Option<bool>> = planned
        .iter()
        .map(|planned| Some(planned.is_some()))
        .collect();
    let mut level: Vec<Tracked> = Vec::new();
    let mut added: Vec<AddedRoot> = Vec::new();
    for (entry, planned) in planned.iter().enumerate() {
        match *planned {
            Some((node, parent, name_hash, true)) => added.push((node, parent, name_hash, entry)),
            Some((node, parent, _, false)) => level.push((node, parent, entry)),
            None => {}
        }
    }
    while !level.is_empty() {
        let groups: Vec<Vec<Tracked>> = (level.chunks(level.len().div_ceil(BATCH_GROUPS)))
            .map(<[_]>::to_vec)
            .collect();
        let mut next = Vec::new();
        let marked = in_groups(groups, |group| {
            let (state, repository) = (state.clone(), repository.clone());
            async move { levels_marked(&state, &repository, &group).await }
        });
        for steps in marked.await {
            for (entry, step) in steps {
                went[entry] = went_after(went[entry], &step);
                if let Step::Done((tracked, roots)) = step {
                    next.extend(tracked);
                    added.extend(roots);
                }
            }
        }
        level = next;
    }
    added.retain(|&(.., entry)| went[entry] == Some(true));
    // Stable, so each directory's roots keep the order the levels read them in.
    added.sort_by_key(|&(_, parent, ..)| parent);
    let mut groups: Vec<Vec<AddedRoot>> = Vec::new();
    for root in added {
        match groups.last_mut() {
            Some(group) if group[0].1 == root.1 => group.push(root),
            _ => groups.push(vec![root]),
        }
    }
    let per_future = groups.len().div_ceil(BATCH_GROUPS).max(1);
    let batches: Vec<Vec<AddedRoot>> = groups.chunks(per_future).map(<[_]>::concat).collect();
    let detached = in_groups(batches, |roots| {
        let (state, repository) = (state.clone(), repository.clone());
        async move {
            let mut steps = Vec::with_capacity(roots.len());
            for (node, parent, name_hash, entry) in roots {
                let step = detached_added(&state, &repository, (node, parent, name_hash)).await;
                steps.push((entry, step));
            }
            steps
        }
    });
    for steps in detached.await {
        for (entry, step) in steps {
            went[entry] = went_after(went[entry], &step);
        }
    }
    went
}

/// `step` of each of `groups`, in place where there is one, else each in a future spawned for it,
/// its answers in the order the groups end.
async fn in_groups<G, F, T>(groups: Vec<G>, step: impl Fn(G) -> F) -> Vec<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    if groups.len() <= 1 {
        let mut answers = Vec::with_capacity(1);
        for group in groups {
            answers.push(step(group).await);
        }
        return answers;
    }
    let mut tasks = tokio::task::JoinSet::new();
    for group in groups {
        lore_base::lore_spawn!(tasks, lore_revision::subtask!(step(group)));
    }
    let mut answers = Vec::with_capacity(tasks.len());
    while let Some(answer) = tasks.join_next().await {
        answers.push(answer.expect("a batch's future ends"));
    }
    answers
}

/// Each of `group`'s nodes staged for deletion, one after another, each a call of its own
/// ([`level_marked`]), with its entry: where done, its tracked children, making the next level,
/// and its added ones, roots to detach.
async fn levels_marked(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    group: &[Tracked],
) -> Vec<(usize, Step<(Vec<Tracked>, Vec<AddedRoot>)>)> {
    let mut steps = Vec::with_capacity(group.len());
    for &(node, parent, entry) in group {
        let step = match level_marked(state, repository, (node, parent)).await {
            Step::Done((children, roots)) => Step::Done((
                children
                    .into_iter()
                    .map(|child| (child, node, entry))
                    .collect(),
                (roots.into_iter())
                    .map(|(root, name_hash)| (root, node, name_hash, entry))
                    .collect(),
            )),
            Step::Refused => Step::Refused,
            Step::Failed => Step::Failed,
        };
        steps.push((entry, step));
    }
    steps
}

/// A level of a C API delete's batch: tracked `node`, in `parent`, staged for deletion under its
/// batch's moves permit, then its children read from the chain head its mark read, as the C API
/// sorts them (delete.rs, `sort_children`): the tracked ones, and the added ones with their names'
/// hashes; those staged for deletion or discarded already passed over. A node staged for deletion
/// already is done with no child, its subtree staged with it.
async fn level_marked(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    (node, parent): (NodeID, NodeID),
) -> Step<(Vec<NodeID>, Vec<(NodeID, u64)>)> {
    call(state, (&super::DELETE_LEVEL, Some(node)), async {
        leaving(Leaves::StagedDelete);
        let head = match state.node_delete_level(repository, node, parent).await {
            Ok(LevelMark::Marked(head)) => head,
            Ok(LevelMark::Staged) => {
                let step = Step::Done((Vec::new(), Vec::new()));
                return (step, Outcome::Refused(Refusal::DeleteLevelStaged));
            }
            Ok(LevelMark::Other) => {
                return (Step::Refused, Outcome::Refused(Refusal::DeleteLevelOther));
            }
            Err(error) if error.is_node_not_found() => return (Step::Refused, GONE),
            Err(error) => return (Step::Failed, failed(&error)),
        };
        match sorted_children(state, repository, node, head).await {
            Ok(sorted) => (Step::Done(sorted), Outcome::Done(Answer::None)),
            Err(error) => (Step::Failed, failed(&error)),
        }
    })
    .await
}

/// The children of `node` from `head` on, sorted as [`level_marked`] says.
async fn sorted_children(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
    head: Option<NodeID>,
) -> Result<(Vec<NodeID>, Vec<(NodeID, u64)>), StateError> {
    let (mut tracked, mut added) = (Vec::new(), Vec::new());
    let mut cycle = SiblingCycleGuard::new(node);
    let mut next = head;
    while let Some(child_id) = next {
        let index = NodeBlock::index(child_id);
        let block = match state.resident_block(index) {
            Some(block) => block,
            None => state.block(repository.clone(), index).await?,
        };
        let child = block.node(Node::index(child_id));
        child.walk_step(child_id, node, &mut cycle)?;
        next = child.sibling();
        if child.is_discarded() || child.is_staged_delete() {
            continue;
        }
        if child.is_staged_add() {
            added.push((child_id, child.name_hash));
        } else {
            tracked.push(child_id);
        }
    }
    Ok((tracked, added))
}

/// An added root of a C API delete's batch, `node` in `parent` by `name_hash`, detached onto the
/// pending queue under its batch's moves permit: refused where it is no longer there.
async fn detached_added(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    (node, parent, name_hash): (NodeID, NodeID, u64),
) -> Step<()> {
    let on = On {
        place: Some((parent, name_hash)),
        ..On::at(Some(node))
    };
    call_on(state, (&super::DETACH_ADDED, on), async {
        match state
            .node_discard_pending(repository.clone(), node, parent, name_hash)
            .await
        {
            Ok(Some(_)) => (Step::Done(()), Outcome::Done(Answer::None)),
            Ok(None) => (Step::Refused, GONE),
            Err(error) if error.is_node_not_found() => (Step::Refused, GONE),
            Err(error) => (Step::Failed, failed(&error)),
        }
    })
    .await
}

/// The C API commit's drain of the pending queue, after its last refusal: every subtree the
/// deletes queued, freed. What it freed.
pub async fn drain(tree: &Tree) -> DiscardCounts {
    call(&tree.state, (&super::DRAIN, None), async {
        match tree.state.node_free_pending(&tree.repository).await {
            Ok(counts) => (counts, Outcome::Done(freed(counts))),
            Err(error) => (DiscardCounts::default(), failed(&error)),
        }
    })
    .await
}

/// The sweep the stage, the CLI commit and the C API commit run before they save, under their
/// exclusive claim with nothing beside it ([`State::node_sweep_detached`]): the pending queue
/// drained, then every slot a flagged block in memory holds detached or leaked in flight freed with
/// the subtree below it (C15). What it freed.
pub async fn sweep(tree: &Tree) -> DiscardCounts {
    call(&tree.state, (&super::SWEEP, None), async {
        sweeping(&tree.state, swept_on(&tree.state, &[]));
        match tree.state.node_sweep_detached(&tree.repository).await {
            Ok(counts) => (counts, Outcome::Done(freed(counts))),
            Err(error) => (DiscardCounts::default(), failed(&error)),
        }
    })
    .await
}

/// A VFS provider's sweep of its served State where it paused its requests, settles and swaps
/// ([`lore_revision::state::ServedNodes::node_sweep_detached`]): [`sweep`]'s frees, but for every
/// detached subtree holding a node of `retained`, the nodes the kernel still references, which it
/// keeps whole. What it freed.
pub async fn served_sweep(tree: &Tree, retained: &[NodeID]) -> DiscardCounts {
    let mut retained = retained.to_vec();
    retained.sort_unstable();
    retained.dedup();
    call(&tree.state, (&super::SERVED_SWEEP, None), async {
        sweeping(&tree.state, swept_on(&tree.state, &retained));
        let held = |node: NodeID| retained.binary_search(&node).is_ok();
        match tree
            .state
            .served()
            .node_sweep_detached(&tree.repository, held)
            .await
        {
            Ok(counts) => (counts, Outcome::Done(freed(counts))),
            Err(error) => (DiscardCounts::default(), failed(&error)),
        }
    })
    .await
}

/// What a sweep of `state` keeping `retained`, sorted, is made on: those nodes, and the blocks in
/// memory now, as it begins.
fn swept_on(state: &State, retained: &[NodeID]) -> Sweeping {
    Sweeping {
        retained: retained.to_vec(),
        resident: (0..state.block_count())
            .filter(|&index| state.resident_block(index).is_some())
            .collect(),
    }
}

/// What a discard answered it freed.
fn freed(counts: DiscardCounts) -> Answer {
    Answer::Freed {
        files: counts.file_count,
        directories: counts.directory_count,
    }
}

/// `lore dirty move`'s move of `node` into `to`, its name as `name` says (file/dirty.rs,
/// `dirty_move`), on a State no other task changes; the command finds both by path first, which
/// the op takes found: the node read first; then [`lore_revision::state::State::node_move`] with
/// the node's own dirty mark, forced, an add staying dirty-added and the
/// rest dirty-moved; the destination's climb; a directory's subtree marked dirty (`marked_below`);
/// then the directory it left, marked dirty. Its error, for the scenarios that count them.
pub async fn dirty_move(
    tree: &Tree,
    node: NodeID,
    to: NodeID,
    name: NameChange<'_>,
) -> Result<(), StateError> {
    let calls = || dirty_move_calls(tree, node, to, name);
    trusted(tree, &super::DIRTY_MOVE, (node, to, name), calls).await
}

/// `lore stage move`'s State calls for a move of `node` into the existing directory `to`, its name
/// as `name` says (file/stage.rs, `stage_move`); the command finds both by path first, which the op
/// takes found: the node read first; then [`lore_revision::state::State::node_move`] with
/// the node's own staged mark, forced, an add staying staged-added and the
/// rest staged-moved; `replacing`, the node the name's lookup found there before the move, staged
/// deleted (`staged_deleted`) but where it is the node itself, as a rename of the case alone finds
/// it; the destination's climb, dirty too where the node is dirty; a directory's subtree marked
/// staged (`marked_below`); then the directory it left, staged, where a commit recorded it (its
/// address not zero), as the stage leaves one only the working tree holds to the commit's prune.
/// The parent the stage stages from disk is not the op's. Its error.
pub async fn stage_move(
    tree: &Tree,
    node: NodeID,
    to: NodeID,
    name: NameChange<'_>,
    replacing: Option<NodeID>,
) -> Result<(), StateError> {
    let calls = || stage_move_calls(tree, node, to, name, replacing);
    trusted(tree, &super::STAGE_MOVE, (node, to, name), calls).await
}

/// A merge's or restore's realized move of `node` into `to`, its name as `name` says
/// (fs/realize.rs, `relink_moved_node`): [`lore_revision::state::State::node_move`],
/// staged moved, forced, with `StagedMerge` where `merge`; the destination's
/// climb; then the directory it left, staged. The realize finds both nodes by path first, which
/// the op takes found. Its error.
pub async fn realize_move(
    tree: &Tree,
    node: NodeID,
    to: NodeID,
    name: NameChange<'_>,
    merge: bool,
) -> Result<(), StateError> {
    let calls = || realize_move_calls(tree, node, to, name, merge);
    trusted(tree, &super::REALIZE_MOVE, (node, to, name), calls).await
}

/// A trusted move's call of `kind`, on `node` moving into `to` as `name` says, `calls` making its
/// steps once the call is open, each answering its error with whether the node had moved: done
/// where they ran; the node gone before it moved, `GONE`; any other error, or one once it moved, a
/// failure.
async fn trusted<F: Future<Output = Result<(), (bool, StateError)>>>(
    tree: &Tree,
    kind: &'static Kind,
    (node, to, name): (NodeID, NodeID, NameChange<'_>),
    calls: impl FnOnce() -> F,
) -> Result<(), StateError> {
    call_on(&tree.state, (kind, moving_as(node, to, name)), async {
        leaving(Leaves::Placed);
        match calls().await {
            Ok(()) => (Ok(()), Outcome::Done(Answer::None)),
            Err((false, error)) if error.is_node_not_found() => (Err(error), GONE),
            Err((_, error)) => {
                let failed = failed(&error);
                (Err(error), failed)
            }
        }
    })
    .await
}

/// On `node`, moving it into `to`, its name as `name` says.
fn moving_as(node: NodeID, to: NodeID, name: NameChange<'_>) -> On {
    match name {
        NameChange::Rename(name) => moving(node, to, name),
        NameChange::Keep => On {
            target: Some(node),
            destination: Some(to),
            ..On::default()
        },
    }
}

/// What a move's caller reads of its node before it moves it.
struct Moving {
    directory: bool,
    added: bool,
    deleted: bool,
    dirty: bool,
}

impl Moving {
    /// `node`'s record, read.
    async fn read(tree: &Tree, node: NodeID) -> Result<Self, StateError> {
        let node = tree.state.node(tree.repository.clone(), node).await?;
        Ok(Self {
            directory: node.is_directory(),
            added: node.is_added(),
            deleted: node.is_deleted(),
            dirty: node.is_dirty(),
        })
    }
}

/// [`dirty_move`]'s steps.
async fn dirty_move_calls(
    tree: &Tree,
    node: NodeID,
    to: NodeID,
    name: NameChange<'_>,
) -> Result<(), (bool, StateError)> {
    let (state, repository) = (&tree.state, &tree.repository);
    let read = Moving::read(tree, node)
        .await
        .map_err(|error| (false, error))?;
    let flag = if read.added {
        NodeFlags::DirtyAdd
    } else {
        NodeFlags::DirtyMove
    };
    let own = Mark::dirty(flag, Ext::Set(NodeExtFlags::NoFlags), true);
    let left = state
        .node_move(repository.clone(), node, to, name, own)
        .await
        .map_err(|error| (false, error))?;
    let marked = async {
        state.node_mark_destination(repository, to, own).await?;
        if read.directory {
            marked_below(tree, node, SubtreeMarks::Dirty(flag), read.deleted).await?;
        }
        let mark = Mark::dirty(NodeFlags::Dirty, Ext::Keep, false);
        state.node_mark_left(repository, (left, to), mark).await
    };
    marked.await.map_err(|error| (true, error))
}

/// [`stage_move`]'s steps.
async fn stage_move_calls(
    tree: &Tree,
    node: NodeID,
    to: NodeID,
    name: NameChange<'_>,
    replacing: Option<NodeID>,
) -> Result<(), (bool, StateError)> {
    let (state, repository) = (&tree.state, &tree.repository);
    let read = Moving::read(tree, node)
        .await
        .map_err(|error| (false, error))?;
    let flag = if read.added {
        NodeFlags::StagedAdd
    } else {
        NodeFlags::StagedMove
    };
    let own = Mark::staged(flag, true);
    let left = state
        .node_move(repository.clone(), node, to, name, own)
        .await
        .map_err(|error| (false, error))?;
    let marked = async {
        if let Some(target) = replacing.filter(|target| *target != node) {
            staged_deleted(tree, target).await?;
        }
        let climb = Mark {
            dirty: read.dirty.then_some((NodeFlags::Dirty, Ext::Keep, false)),
            ..own
        };
        state.node_mark_destination(repository, to, climb).await?;
        if read.directory {
            marked_below(tree, node, SubtreeMarks::Staged(flag), read.deleted).await?;
        }
        let recorded = left != to
            && left != ROOT_NODE
            && !state
                .node(repository.clone(), left)
                .await?
                .address
                .is_zero();
        if recorded {
            let mark = Mark::staged(NodeFlags::Staged, false);
            state.node_mark_left(repository, (left, to), mark).await?;
        }
        Ok(())
    };
    marked.await.map_err(|error| (true, error))
}

/// [`realize_move`]'s steps.
async fn realize_move_calls(
    tree: &Tree,
    node: NodeID,
    to: NodeID,
    name: NameChange<'_>,
    merge: bool,
) -> Result<(), (bool, StateError)> {
    let (state, repository) = (&tree.state, &tree.repository);
    let mut flags = NodeFlags::StagedMove;
    if merge {
        flags |= NodeFlags::StagedMerge;
    }
    let own = Mark::staged(flags, true);
    let left = state
        .node_move(repository.clone(), node, to, name, own)
        .await
        .map_err(|error| (false, error))?;
    let marked = async {
        state.node_mark_destination(repository, to, own).await?;
        let mark = Mark::staged(NodeFlags::Staged, false);
        state.node_mark_left(repository, (left, to), mark).await
    };
    marked.await.map_err(|error| (true, error))
}

/// How a move marks the nodes below a directory it moved, with the flag the directory took: dirty,
/// as `lore dirty move` marks them, or staged, as `lore stage move` does.
#[derive(Clone, Copy)]
enum SubtreeMarks {
    Dirty(NodeFlags),
    Staged(NodeFlags),
}

impl SubtreeMarks {
    /// The mark of a node below, unforced: an add's add, or the directory's flag.
    fn mark(self, added: bool) -> Mark {
        match self {
            Self::Dirty(flag) => {
                let flag = if added { NodeFlags::DirtyAdd } else { flag };
                Mark::dirty(flag, Ext::Set(NodeExtFlags::NoFlags), false)
            }
            Self::Staged(flag) => {
                let flag = if added { NodeFlags::StagedAdd } else { flag };
                Mark::staged(flag, false)
            }
        }
    }
}

/// The nodes below directory `dir` a move took, marked as `marks` says, in the order the moves'
/// walks mark them (file/stage.rs `mark_children_moved`, file/dirty.rs
/// `mark_children_dirty_moved`): each directory's children listed, then each read, marked and, a
/// directory, descended into before the next. A deleted one is left as it is and not descended
/// into, unless the moved directory was deleted itself (`undelete`): then every node below is
/// marked, the deletion taken off each.
async fn marked_below(
    tree: &Tree,
    dir: NodeID,
    marks: SubtreeMarks,
    undelete: bool,
) -> Result<(), StateError> {
    let (state, repository) = (&tree.state, &tree.repository);
    // The nodes still to mark, the next on top.
    let mut pending = state.node_children(repository.clone(), dir).await?;
    pending.reverse();
    while let Some(child) = pending.pop() {
        let (deleted, added, directory) = {
            let node = state.node(repository.clone(), child).await?;
            (node.is_deleted(), node.is_added(), node.is_directory())
        };
        if !undelete && deleted {
            continue;
        }
        state
            .node_mark(repository, child, marks.mark(added))
            .await?;
        if directory {
            let children = state.node_children(repository.clone(), child).await?;
            pending.extend(children.into_iter().rev());
        }
    }
    Ok(())
}

/// The stage's deletion of `target`, the node a move replaces (stage.rs, `stage_delete`): the
/// target deleted ([`staged_deleted_one`]), then each node below it in the order the stage walks
/// them, each directory's children through the children iterator, as the stage's own walk steps
/// through them, each deleted before the next is read.
async fn staged_deleted(tree: &Tree, target: NodeID) -> Result<(), StateError> {
    let mut walks = Vec::new();
    walks.extend(staged_deleted_one(tree, target).await?);
    while let Some(walk) = walks.last_mut() {
        let Some((child, _, name)) = walk.next().await? else {
            walks.pop();
            continue;
        };
        // The name holds its block's read lock, dropped before the child's read and mark.
        drop(name);
        walks.extend(staged_deleted_one(tree, child).await?);
    }
    Ok(())
}

/// One node's deletion in [`staged_deleted`] (stage.rs, `stage_delete_node`): `node` read, then
/// marked staged and dirty deleted, forced, with its ancestors, unless staged for deletion
/// already; a directory it marked, the walk of its children.
async fn staged_deleted_one(
    tree: &Tree,
    node: NodeID,
) -> Result<Option<StateNodeChildrenWithNameIterator>, StateError> {
    let (state, repository) = (&tree.state, &tree.repository);
    let (deleted, directory) = {
        let record = state.node(repository.clone(), node).await?;
        (record.is_staged_delete(), record.is_directory())
    };
    if deleted {
        return Ok(None);
    }
    let mark = Mark {
        staged: Some((NodeFlags::StagedDelete, true)),
        dirty: Some((
            NodeFlags::DirtyDelete,
            Ext::Set(NodeExtFlags::NoFlags),
            true,
        )),
    };
    state.node_mark(repository, node, mark).await?;
    if !directory {
        return Ok(None);
    }
    let walk =
        StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node).await?;
    Ok(Some(walk))
}

/// A swap's rewrite of file `node` to `content` where it is clean; whether it rewrote.
pub async fn swap_rewrite(tree: &Tree, node: NodeID, content: &Content) -> bool {
    call(&tree.state, (&super::SWAP_REWRITE, Some(node)), async {
        leaving(Leaves::Content(*content));
        match tree
            .state
            .node_rewrite(&tree.repository, node, content, Rewrite::IfClean)
            .await
        {
            Ok(true) => (true, Outcome::Done(Answer::None)),
            Ok(false) => (false, Outcome::Refused(Refusal::IfClean)),
            Err(error) if error.is_node_not_found() => (false, GONE),
            // `Discarded` since the swap found it, or not a file, under its lock.
            Err(error) if error.is_invalid_arguments() => {
                (false, Outcome::Refused(Refusal::Modify))
            }
            Err(error) => (false, failed(&error)),
        }
    })
    .await
}

/// [`swap_copy_tree`], `pace` awaited before each file's add, as the swap copies each file's content
/// there. The copy where it attached.
pub async fn swap_copy_paced<P: Future<Output = ()>>(
    tree: &Tree,
    (dir, name): (NodeID, &str),
    children: &[&str],
    pace: impl Fn() -> P,
) -> Option<NodeID> {
    call_on(
        &tree.state,
        (&super::SWAP_COPY_TREE, naming(dir, name)),
        async {
            let attach = tree
                .state
                .swap()
                .node_attach_tree(tree.repository.clone(), dir, &directory(name), name)
                .await;
            let mut build = match building(attach) {
                Ok(build) => build,
                Err(refused) => return (None, refused),
            };
            for child in children {
                window(&tree.state, pace()).await;
                if let Err(error) = build.add(build.root(), &file(child), child).await {
                    // As the swap's fill fails: the build abandoned, which a drop would leak (C12).
                    Box::pin(build.abandon()).await;
                    return (None, failed(&error));
                }
            }
            published(build.publish().await)
        },
    )
    .await
}
