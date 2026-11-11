// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The OS provider's diff of the working tree against a state.
//!
//! Reads the live filesystem through [`crate::util::fs`] and the global I/O driver, which is what
//! backing an operation with the OS means. Only [`crate::fs::os::OsOperation`] reaches
//! [`diff_os_filesystem`]: another provider implements
//! [`changes_from_filesystem_to_state`](crate::fs::filesystem_provider::InstanceOperation::changes_from_filesystem_to_state)
//! over whatever it is backed by, and routing that here would read the OS behind its own snapshot.
//!
//! A child of [`crate::state`], so the state internals a diff walks stay that module's own rather
//! than becoming the crate's to reach.

use core::str;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_io::StatKind;
use tokio::sync::Semaphore;
use tokio::task::JoinError;
use tokio::task::JoinSet;

use super::*;
use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::change;
use crate::change::FileAction;
use crate::change::NodeChange;
use crate::change::NodeChangeState;
use crate::errors::NodeNotFound;
use crate::filter::FilterMode;
use crate::filter::FilterStates;
use crate::filter::WalkPath;
use crate::fs::filesystem_provider::ChainRoot;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::FilesystemDiffContext;
use crate::fs::filesystem_provider::FilesystemDiffIntent;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::StageIntent;
use crate::fs::os::FileListItem;
use crate::lore::*;
use crate::lore_drain_tasks;
use crate::lore_trace;
use crate::node::*;
use crate::repository::BASE_SUFFIX;
use crate::repository::RepositoryContext;
use crate::repository::TEMP_FILE_EXTENSION;
use crate::repository::THEIRS_SUFFIX;
use crate::repository::is_reserved_node_name;
use crate::state::ChangeSender;
use crate::state::DiffSide;
use crate::state::diff::NodeMatch;
use crate::state::diff::get_filtered_node_and_path;
use crate::state::diff::get_node_match;
use crate::state::stream::emit;
use crate::util::path::EntryPath;
use crate::util::path::RelativePath;

/// Find-or-create the directory node chain from `start` down to `path`, relative
/// to it, marking newly created segments as dirty-add and the directory above the
/// first of them dirty, with its ancestors. A path-filtered scan can
/// enter a directory (or a file's parent) present on disk but absent from
/// `state_from`; creating the chain lets adds discovered inside resolve their
/// parent node. Returns the node for the final path segment. A find that fails
/// other than for a missing name returns its error, adding nothing for that name.
///
/// A reserved name anywhere in `path` answers as no node, as does a chain that would add below a
/// file or a link, or end at one. A name the tree holds is taken as it holds it, so one stored
/// before a rule refused it still leads on; the names from the first one missing onward are
/// checked as an add checks them before any is added, so a refusal adds nothing.
///
/// `lore dirty`'s chain, `ensure_dirty_parent_dirs` in `file/dirty.rs`, walks the same way but adds
/// each directory with no flags, then marks it `DirtyAdd`, climbing from it, and checks no name
/// before its add.
///
/// `path` is generic so a caller spawning this future can hand over an owned path.
#[lore_macro::test_pub]
pub(crate) async fn ensure_scan_dir_chain(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    start: NodeID,
    path: impl AsRef<str>,
) -> Result<NodeID, StateError> {
    if path.as_ref().split('/').any(is_reserved_node_name) {
        return Err(NodeNotFound.into());
    }
    let mut current_node = start;
    let mut marked = false;
    // Split by hand: a `split('/')` iterator held across the awaits below costs this future 56
    // bytes more than the rest of the path does.
    let mut rest = path.as_ref();
    while !rest.is_empty() {
        let (segment, tail) = rest.split_once('/').unwrap_or((rest, ""));
        rest = tail;
        if segment.is_empty() {
            continue;
        }
        let name_hash = crate::hash::hash_string(segment);
        // Only a name the directory lacks is added: a find that failed otherwise, as on a block
        // that did not load, says nothing of the name, and the add checks none. The add follows
        // the match, so the find's answer is not held across its awaits.
        match state
            .find_subnode(repository.clone(), current_node, name_hash)
            .await
        {
            Ok(child_id) => {
                current_node = child_id;
                continue;
            }
            Err(error) if !error.is_node_not_found() => return Err(error),
            Err(_) => {}
        }
        if !marked {
            refuse_below_non_directory(&repository, &state, current_node).await?;
            for name in std::iter::once(segment).chain(rest.split('/')) {
                if !name.is_empty() {
                    validate_node_name_for_store(name)
                        .forward::<StateError>("scan add: entry directory name not storable")?;
                }
            }
        }
        let dir_node = Node {
            flags: NodeFlags::DirtyAdd.bits(),
            name_hash,
            ..Default::default()
        };
        let parent = current_node;
        current_node = state
            .node_add(repository.clone(), parent, dir_node, segment)
            .await
            .forward::<StateError>("scan add: failed to create entry directory node")?;
        // The first add's directory marked at once, before a later add may fail; the adds after
        // it go under directories added dirty.
        if !marked {
            marked = true;
            state
                .node_mark(
                    &repository,
                    parent,
                    Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
                )
                .await?;
        }
    }
    if !marked {
        refuse_below_non_directory(&repository, &state, current_node).await?;
    }
    Ok(current_node)
}

/// Refuses a chain that adds below `node`, or ends at it, where the tree holds a file or a link
/// there: a file holds no children, and a link's child is the node it exposes in its linked tree.
async fn refuse_below_non_directory(
    repository: &Arc<RepositoryContext>,
    state: &State,
    node: NodeID,
) -> Result<(), StateError> {
    if state.node(repository.clone(), node).await?.is_directory() {
        Ok(())
    } else {
        Err(NodeNotFound.into())
    }
}

/// What a walk over a path a caller names does with the path, once it has read what the working
/// tree holds there.
///
/// `Walk` stays unboxed: it is the common answer, moved once into the walk, and boxing it would add
/// a heap allocation per named path.
#[allow(clippy::large_enum_variant)]
enum NamedRoot {
    /// Walk the path, from the filter verdict at it, through what the working tree holds there.
    Walk(FilterStates, crate::fs::os::PathListingResult),
    /// Replace the file the tree holds there with the directory the working tree holds, from the
    /// verdict at it.
    Replaced(FilterStates, lore_io::DirStream),
    /// Report the node the tree holds there missing, from the verdict at it.
    Missing(FilterStates),
    /// Leave the path out.
    Excluded,
}

/// `ctx`'s mode for its path asked about as a directory where `is_directory` says, where the from
/// tree holds `node` there: past the ignore slot where that tree tracks the node as that kind.
///
/// Boxed by its callers: a named path asks it once, and only where the filter would leave the path
/// out.
async fn tracked_mode(
    ctx: &FilesystemDiffContext,
    node: Option<&Node>,
    is_directory: bool,
) -> Result<FilterMode, StateError> {
    let tracked = match node {
        Some(node) => {
            ctx.from
                .state
                .tracks_as(
                    &ctx.from.repository,
                    ctx.from.node,
                    NodeFlags::from_bits_retain(node.flags),
                    is_directory,
                )
                .await?
        }
        None => false,
    };
    Ok((ctx.filter_mode | FilterMode::KeepTracked).at(tracked))
}

/// [`NamedRoot::Replaced`] for a directory listing the working tree holds where the tree holds a
/// file.
fn replaced(states: FilterStates, listing: crate::fs::os::PathListingResult) -> NamedRoot {
    match listing {
        crate::fs::os::PathListingResult::Directory { listing } => {
            NamedRoot::Replaced(states, listing)
        }
        listing => NamedRoot::Walk(states, listing),
    }
}

/// [`NamedRoot`] for `ctx`'s path, read from the working tree at `absolute_path`.
///
/// The path is asked about as what the working tree holds there, else as the node the tree holds,
/// a link as a directory, and as a directory where neither holds anything. Where the filter leaves
/// out what the working tree holds and the tree holds a node of the other kind, that node is missing
/// from the working tree and is asked about as what it is, as the walk over the parent asks about a
/// node its listing claims nothing for. The tree's node is read where the listing leaves the
/// answer open: where the working tree holds nothing, or the filter leaves out what it holds and
/// keeps a node of the other kind. A node the tree tracks is past the ignore slot as the kind it
/// is, so the tree's node is read too where the filter would leave out what the working tree holds,
/// and where it holds a directory, which replaces a file the tree holds there.
///
/// The listing is read here and handed back to walk, so the caller's future holds it across no
/// await of its own.
async fn named_root(
    ctx: &FilesystemDiffContext,
    absolute_path: std::path::PathBuf,
) -> Result<NamedRoot, StateError> {
    let listing = crate::fs::os::list_path(absolute_path)
        .await
        .forward::<StateError>("Failed to list the path")?;
    let filter = &ctx.from.repository.filter;
    let path = &ctx.filesystem_path;
    let held = async || {
        ctx.from
            .state
            .try_node(ctx.from.repository.clone(), ctx.from.node)
            .await
    };
    let listed = match &listing {
        crate::fs::os::PathListingResult::Directory { .. } => true,
        crate::fs::os::PathListingResult::File { .. } => false,
        crate::fs::os::PathListingResult::NotFound => {
            let node = held().await;
            let is_directory = Node::asked_as_directory(node.as_ref());
            let (states, excluded) = filter.child_emit_excludes(
                ctx.states,
                path,
                is_directory,
                Box::pin(tracked_mode(ctx, node.as_ref(), is_directory)).await?,
            );
            return Ok(if excluded {
                NamedRoot::Excluded
            } else {
                NamedRoot::Missing(states)
            });
        }
    };
    let (states, excluded) = filter.child_excludes_tree(ctx.states, path, listed, ctx.filter_mode);
    if !excluded {
        if listed && held().await.is_some_and(|node| node.is_file()) {
            return Ok(replaced(states, listing));
        }
        return Ok(NamedRoot::Walk(states, listing));
    }
    let node = held().await;
    let (states, excluded) = filter.child_emit_excludes(
        ctx.states,
        path,
        listed,
        Box::pin(tracked_mode(ctx, node.as_ref(), listed)).await?,
    );
    if !excluded {
        return Ok(NamedRoot::Walk(states, listing));
    }
    drop(listing);
    let (states, excluded) = filter.child_excludes_tree(
        ctx.states,
        path,
        !listed,
        Box::pin(tracked_mode(ctx, node.as_ref(), !listed)).await?,
    );
    if excluded {
        return Ok(NamedRoot::Excluded);
    }
    Ok(match node {
        Some(node) if Node::asked_as_directory(Some(&node)) != listed => NamedRoot::Missing(states),
        _ => NamedRoot::Excluded,
    })
}

async fn diff_filesystem_subtree_impl(
    mut ctx: FilesystemDiffContext,
    changes: &ChangeSender,
) -> Result<FilesystemDiffStats, StateError> {
    let absolute_path = ctx
        .filesystem_path
        .to_absolute_path(ctx.from.repository.require_path()?)
        .forward::<StateError>("placing the walked directory below the root")?;
    // Replaced below rather than in the match: an await inside an arm of it keeps the finished
    // `named_root` future alive, and this future is one per directory the walk visits.
    let mut replaced = false;
    let listing = if ctx.named_path.is_some() {
        match named_root(&ctx, absolute_path).await? {
            NamedRoot::Walk(states, listing) => {
                ctx.states = states;
                ctx.from_states = states;
                listing
            }
            NamedRoot::Missing(states) => {
                return diff_filesystem_missing(
                    ctx.from,
                    ctx.filesystem_path,
                    states,
                    ctx.filter_mode,
                    ctx.intent,
                    changes,
                    &ctx.link_mounts,
                )
                .await;
            }
            NamedRoot::Replaced(states, listing) => {
                ctx.states = states;
                ctx.from_states = states;
                replaced = true;
                crate::fs::os::PathListingResult::Directory { listing }
            }
            NamedRoot::Excluded => return Ok(FilesystemDiffStats::default()),
        }
    } else {
        crate::fs::os::list_path(absolute_path)
            .await
            .forward::<StateError>("Failed to list the path")?
    };

    match listing {
        crate::fs::os::PathListingResult::Directory { listing } if replaced => {
            Box::pin(diff_named_type_change(ctx, listing, changes)).await
        }
        crate::fs::os::PathListingResult::Directory { listing } => {
            // A path-filtered scan can enter a directory present on disk but
            // absent from state_from (an untracked add). Create its dirty-add
            // node chain so adds discovered inside resolve their parent node.
            let chain = ctx.named_path.unwrap_or(ChainRoot::ROOT);
            if ctx.intent.marks_dirty()
                && !ctx.from.node.is_valid_or_root_node_id()
                && let Some(below) = ctx.filesystem_path.as_str().get(chain.skip as usize..)
                && !below.is_empty()
            {
                let entry_node = ensure_scan_dir_chain(
                    ctx.from.repository.clone(),
                    ctx.from.state.clone(),
                    chain.node,
                    below,
                )
                .await?;
                ctx.from.node = entry_node;
            }
            diff_filesystem_directory(ctx, listing, changes).await
        }
        crate::fs::os::PathListingResult::File { item } => {
            // A path-filtered scan of a new file: ensure its parent directory
            // chain exists so the add resolves its parent node.
            let chain = ctx.named_path.unwrap_or(ChainRoot::ROOT);
            let mut parent_node = None;
            if ctx.intent.marks_dirty()
                && !ctx.from.node.is_valid_node_id()
                && let Some(parent) = ctx.filesystem_path.parent()
            {
                parent_node = Some(
                    ensure_scan_dir_chain(
                        ctx.from.repository.clone(),
                        ctx.from.state.clone(),
                        chain.node,
                        parent.get(chain.skip as usize..).unwrap_or_default(),
                    )
                    .await?,
                );
            }
            diff_filesystem_single_file(ctx, item, parent_node, changes).await
        }
        crate::fs::os::PathListingResult::NotFound => {
            // Path doesn't exist on filesystem - everything in state is deleted
            diff_filesystem_missing(
                ctx.from,
                ctx.filesystem_path,
                ctx.states,
                ctx.filter_mode,
                ctx.intent,
                changes,
                &ctx.link_mounts,
            )
            .await
        }
    }
}

/// How the file at `file_path` compares to `from_node`: a type change, a modification, or
/// neither. Reports what it found and leaves both trees alone.
///
/// A file is modified where its executable bit differs from the node's or its content does.
/// The bit settles the answer on its own, so it is tested before anything is read.
///
/// `current_node` is what the working copy last held, which is what says whether a recorded
/// modification time can answer for `from_node`.
async fn compare_single_file_against_state(
    operation: &InstanceOperationImpl,
    repository: Arc<RepositoryContext>,
    from_node: Option<&Node>,
    current_node: Option<&Node>,
    observed: &FileInfo,
    file_path: &impl WalkPath,
    stats: &FilesystemDiffStats,
) -> Result<SingleFileCompareResult, StateError> {
    let Some(from_node) = from_node else {
        // No state node - this is a new file
        return Ok(SingleFileCompareResult::NewFile);
    };

    let state_is_file = from_node.is_file();
    let _state_is_directory = from_node.is_directory();
    let _state_is_link = from_node.is_link();

    // Handle type changes
    let filesystem_is_file = observed.is_file();
    if filesystem_is_file && !state_is_file {
        // Filesystem has file, state has directory or link
        return Ok(SingleFileCompareResult::TypeChangedToFile);
    }

    if !filesystem_is_file && state_is_file {
        // Filesystem has directory, state has file
        return Ok(SingleFileCompareResult::TypeChangedToDirectory);
    }

    // At this point, both are files - check for modifications
    if state_is_file && filesystem_is_file {
        if observed.mode_differs_from(from_node.mode) {
            return Ok(SingleFileCompareResult::Modified);
        }

        // Force hash check if the from state doesn't match current state
        // (timestamp tracking only tells us if file matches what was last written,
        // which is the current state)
        let force_hash_check =
            current_node.is_none_or(|n| n.address.hash != from_node.address.hash);

        let modification = file_modified_against_node(
            repository,
            from_node,
            observed.mtime(),
            observed.size(),
            file_path,
            !force_hash_check,
            operation,
            &lore_storage::ContentHashes::default(),
        )
        .await?;
        stats.classify(&modification);

        if modification.is_modified() {
            return Ok(SingleFileCompareResult::Modified);
        }
    }

    Ok(SingleFileCompareResult::Unmodified)
}

/// Settle a delete for `node_id` and every node below it, which is what a commit needs to
/// drop a subtree the file system no longer holds.
///
/// A node already settled is left alone along with its subtree, so a second pass over the
/// same replacement costs one node read. A link's subtree lives in the linked state and is
/// not walked here, as staging a delete does not.
async fn settle_subtree_delete(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node_id: NodeID,
    intent: FilesystemDiffIntent,
) -> Result<(), StateError> {
    let mut stack = vec![node_id];
    while let Some(node_id) = stack.pop() {
        let node = state.node(repository.clone(), node_id).await?;
        if node.is_staged_delete() {
            continue;
        }
        mark_settled(state, repository, node_id, SettledAction::Delete, intent).await?;
        if !node.is_directory() {
            continue;
        }
        let mut child = node.child();
        let mut cycle = SiblingCycleGuard::new(node_id);
        while let Some(child_id) = child {
            stack.push(child_id);
            let child_node = state.node(repository.clone(), child_id).await?;
            child_node.walk_step(child_id, node_id, &mut cycle)?;
            child = child_node.sibling();
        }
    }
    Ok(())
}

/// Settle a modification for a node whose content the file system already matches, which is
/// what force stages and what carries a merge's flags onto a node needing no other change.
async fn settle_insisted_modification(
    ctx: &FileDiffContext,
    file_path: &RelativePath,
    changes: &ChangeSender,
    filter_mode: FilterMode,
) -> Result<(), StateError> {
    mark_settled(
        &ctx.state_from,
        &ctx.repository_from,
        ctx.from_node_id,
        SettledAction::Modify,
        ctx.intent,
    )
    .await?;
    emit_change(
        ctx.create_from_change_state(file_path.clone()),
        ctx.new_file_change_state(file_path.clone()),
        FileAction::Keep,
        change::Flags::Modify,
        changes,
        filter_mode,
    )
    .await
}

/// Settle a modification for a directory node force or a merge asked for.
///
/// A directory holds no content to compare, so it is settled on being asked rather than on
/// anything the walk found. The descent below it runs either way.
///
/// The keep it emits stands between the revision and the file system, so its `to` side states
/// `observed` rather than a node.
#[allow(clippy::too_many_arguments)]
async fn settle_insisted_directory(
    node_list: &StateChildrenNodes,
    node_id: NodeID,
    node: &Node,
    path: &RelativePath,
    observed: FileInfo,
    changes: &ChangeSender,
    intent: FilesystemDiffIntent,
    filter_mode: FilterMode,
) -> Result<(), StateError> {
    mark_settled(
        &node_list.state,
        &node_list.repository,
        node_id,
        SettledAction::Modify,
        intent,
    )
    .await?;
    let from = NodeChangeState {
        mapping: NodeMapping {
            repository: node_list.repository.clone(),
            state: node_list.state.clone(),
            path: path.clone(),
            node: node_id,
        },
        observed: None,
        flags: NodeFlags::from_bits_retain(node.flags),
        address: node.address,
        mode: node.mode,
    };
    let to = NodeChangeState {
        observed: Some(observed),
        ..from.invalid(path.clone())
    };
    emit_change(
        from,
        to,
        FileAction::Keep,
        change::Flags::Modify,
        changes,
        filter_mode,
    )
    .await
}

/// Report the delete of the node a type change displaced and the add of what replaced it,
/// settling both where the intent stages.
///
/// The displaced node keeps its place beside the replacement and carries the staged delete,
/// which is how a commit drops the old content and records the new.
///
/// Returns the replacement node, [`INVALID_NODE`] where the intent minted none, so a walk
/// knows whether to descend against the node or against an empty listing.
async fn emit_type_replacement(
    ctx: &FileDiffContext,
    file_path: &RelativePath,
    is_directory: bool,
    changes: &ChangeSender,
    filter_mode: FilterMode,
) -> Result<NodeID, StateError> {
    let staging = ctx.intent.stage().is_some();
    if staging {
        settle_subtree_delete(
            &ctx.state_from,
            &ctx.repository_from,
            ctx.from_node_id,
            ctx.intent,
        )
        .await?;
    }

    add_change(
        ctx.create_from_change_state(file_path.clone()),
        ctx.invalid_change_state(file_path.clone()),
        FileAction::Delete,
        change::Flags::None,
        changes,
        filter_mode,
        ctx.states,
    )
    .await?;

    let replacement = if staging {
        ctx.add_new_node(file_path, is_directory).await?
    } else {
        INVALID_NODE
    };
    let to_state = if replacement.is_valid_node_id() {
        ctx.settled_change_state(replacement, file_path.clone())
            .await?
    } else if is_directory {
        ctx.new_directory_change_state(file_path.clone())
    } else {
        ctx.new_file_change_state(file_path.clone())
    };

    add_change(
        ctx.invalid_change_state(file_path.clone()),
        to_state,
        FileAction::Add,
        change::Flags::None,
        changes,
        filter_mode,
        ctx.states,
    )
    .await?;
    Ok(replacement)
}

/// Emit an Add+Dirty reconciliation change for a file whose node exists in
/// `state_from` (staged) but not in the current state. The file's presence
/// on disk is the add, and the node carries the `DirtyAdd` flag (re-marked
/// here if it was cleared by stale reconciliation). The compare framework
/// is bypassed because comparing the filesystem hash against the staged
/// node's zero address is meaningless for an add.
///
/// A dirty-move destination is exempt: it only looks like a node missing from
/// the current state because the walk matches by name, while the same node is
/// present in the current revision under its source path. Emitting an add for
/// it would drop the move provenance and overwrite `DirtyMove` with `DirtyAdd`,
/// so the move is left to the state diff, which coalesces it by file context.
#[allow(clippy::too_many_arguments)]
async fn emit_unstaged_add(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    from_node_id: NodeID,
    from_node: Node,
    file_path: &RelativePath,
    observed: &FileInfo,
    changes: &ChangeSender,
    stats: &FilesystemDiffStats,
    filter_mode: FilterMode,
    states: FilterStates,
    intent: FilesystemDiffIntent,
) -> Result<(), StateError> {
    if from_node.is_dirty_move() {
        lore_trace!("File {file_path} is a dirty move destination, not an unstaged add");
        return Ok(());
    }
    let staging = intent.stage().is_some();
    if staging {
        record_observed_size(&state, &repository, from_node_id, observed).await?;
    }
    if staging || !from_node.is_dirty_add() {
        mark_settled(
            &state,
            &repository,
            from_node_id,
            SettledAction::Add,
            intent,
        )
        .await?;
    }
    let block_index = NodeBlock::index(from_node_id);
    let node_index = Node::index(from_node_id);
    let block = state.block(repository.clone(), block_index).await?;
    let node = block.node(node_index);
    add_change(
        NodeChangeState {
            mapping: NodeMapping {
                repository: repository.clone(),
                state: state.clone(),
                path: file_path.clone(),
                node: INVALID_NODE,
            },
            observed: None,
            flags: NodeFlags::NoFlags,
            address: Address::default(),
            mode: 0,
        },
        NodeChangeState {
            mapping: NodeMapping {
                repository: repository.clone(),
                state: state.clone(),
                path: file_path.clone(),
                node: from_node_id,
            },
            observed: None,
            flags: NodeFlags::from_bits_retain(node.flags),
            address: node.address,
            mode: node.mode,
        },
        change::FileAction::Add,
        change::Flags::None,
        changes,
        filter_mode,
        states,
    )
    .await?;
    stats.file_add.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Record on `node_id` the size the walk measured at its path.
///
/// The mode is not recorded. A commit reads the file's mode and compares it with the node's to
/// see whether the revision has to carry a change to it, so a mode recorded here would be
/// compared against itself and a change to the executable bit alone would be lost.
///
/// The node is a file already: a path whose type the tree disagrees with is replaced rather
/// than recorded, which is [`emit_type_replacement`]'s.
async fn record_observed_size(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node_id: NodeID,
    observed: &FileInfo,
) -> Result<(), StateError> {
    let block = state
        .block(repository.clone(), NodeBlock::index(node_id))
        .await?;
    state.node_change_owned(&block, node_id, |held| {
        held.set_size(observed.size());
        true
    });
    Ok(())
}

/// Emit a single Add change for `node_id` without recursing into its subtree —
/// the caller's walk recursion surfaces the children. Used to report a dirty-add
/// directory exactly once per scan (unlike `add_change`, which walks the whole
/// hierarchy for a directory add and would double-count against the recursion).
async fn emit_dirty_add_node_single(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    path: &RelativePath,
    changes: &ChangeSender,
    stats: &FilesystemDiffStats,
    intent: FilesystemDiffIntent,
) -> Result<(), StateError> {
    let block = state
        .block(repository.clone(), NodeBlock::index(node_id))
        .await?;
    let node = block.node(Node::index(node_id));
    if intent.stage().is_some() || !node.is_dirty_add() {
        mark_settled(&state, &repository, node_id, SettledAction::Add, intent).await?;
    }
    emit_add_node_single(repository, state, node_id, path, changes, stats).await
}

/// Emit a single Add change for `node_id` as it stands, without marking it and without
/// recursing into its subtree.
async fn emit_add_node_single(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    path: &RelativePath,
    changes: &ChangeSender,
    stats: &FilesystemDiffStats,
) -> Result<(), StateError> {
    let block = state
        .block(repository.clone(), NodeBlock::index(node_id))
        .await?;
    let node = block.node(Node::index(node_id));
    emit(changes, || NodeChange {
        action: change::FileAction::Add,
        flags: compute_change_flags(&node),
        from: NodeChangeState {
            mapping: NodeMapping {
                repository: repository.clone(),
                state: state.clone(),
                path: path.clone(),
                node: INVALID_NODE,
            },
            observed: None,
            flags: NodeFlags::NoFlags,
            address: Address::default(),
            mode: 0,
        },
        to: NodeChangeState {
            mapping: NodeMapping {
                repository: repository.clone(),
                state: state.clone(),
                path: path.clone(),
                node: node_id,
            },
            observed: None,
            flags: NodeFlags::from_bits_retain(node.flags),
            address: node.address,
            mode: node.mode,
        },
    })
    .await?;
    stats.file_add.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Handle the result of a single file comparison and create appropriate changes.
///
/// This is the unified code path for handling single file node changes in both
/// `diff_filesystem_single_file` and `diff_filesystem_directory`.
///
/// # Arguments
/// * `ctx` - Context containing state references for creating changes
/// * `compare_result` - Result of the file comparison
/// * `file_path` - Path to the file (relative)
/// * `from_path` - Original path for rename detection (None if not a rename)
/// * `is_filesystem_directory` - True if the filesystem item is a directory
/// * `changes` - Where the changes are emitted
/// * `stats` - Statistics to update
///
/// # Rename Handling
/// When `from_path` is Some, this indicates the file was renamed. The function handles
/// renames for both modified and unmodified content:
/// - Unmodified + Rename: Generates a Move action (file content matches but name changed)
/// - Modified + Rename: Generates a Move action with modified content
///
/// # Returns
/// The node a staged type replacement minted, [`INVALID_NODE`] otherwise, so a walk
/// descends into a directory that replaced a file.
#[allow(clippy::too_many_arguments)]
async fn handle_single_file_compare_result(
    ctx: &FileDiffContext,
    compare_result: SingleFileCompareResult,
    file_path: &impl WalkPath,
    from_path: Option<&RelativePath>,
    is_filesystem_directory: bool,
    changes: &ChangeSender,
    stats: &FilesystemDiffStats,
    filter_mode: FilterMode,
) -> Result<NodeID, StateError> {
    match compare_result {
        SingleFileCompareResult::Unmodified => {
            // Handle rename case: content is unchanged but filename differs
            if let Some(original_path) = from_path {
                lore_trace!(
                    "File {} renamed from {}, content unmodified, add move change",
                    file_path,
                    original_path
                );
                // Settled before the change is recorded, so `compute_change_flags` loads
                // the marked node, as the modified arm below does.
                if ctx.intent.stage().is_some() && ctx.from_node_id.is_valid_node_id() {
                    mark_settled(
                        &ctx.state_from,
                        &ctx.repository_from,
                        ctx.from_node_id,
                        SettledAction::Move,
                        ctx.intent,
                    )
                    .await?;
                }
                let item_path = file_path.to_path();
                add_change(
                    ctx.create_from_change_state(
                        from_path.cloned().unwrap_or_else(|| item_path.clone()),
                    ),
                    ctx.new_file_change_state(item_path.clone()),
                    change::FileAction::Move,
                    change::Flags::None,
                    changes,
                    filter_mode,
                    ctx.states,
                )
                .await?;
                stats.file_replace.fetch_add(1, Ordering::Relaxed);
            } else if ctx.insists {
                lore_trace!("File {} unmodified, staged anyway", file_path);
                settle_insisted_modification(ctx, &file_path.to_path(), changes, filter_mode)
                    .await?;
                stats.file_replace.fetch_add(1, Ordering::Relaxed);
            } else {
                lore_trace!("File {} unmodified, retain", file_path);
                stats.file_retain.fetch_add(1, Ordering::Relaxed);

                // Scan: clear stale Dirty on retained file
                if ctx.intent.marks_dirty()
                    && ctx.from_node_id.is_valid_node_id()
                    && ctx.from_node.is_some_and(|n| n.is_dirty())
                {
                    ctx.state_from
                        .node_clear(&ctx.repository_from, ctx.from_node_id, Clear::Dirty)
                        .await?;
                }
            }
        }
        SingleFileCompareResult::Modified => {
            let action = if from_path.is_some() {
                lore_trace!("File {} renamed and modified, add move change", file_path);
                change::FileAction::Move
            } else {
                lore_trace!("File {} modified, add change", file_path);
                change::FileAction::Keep
            };

            // Scan: persist Dirty on the modified node before recording the change so
            // compute_change_flags loads the dirty node and includes Dirty in the event.
            if ctx.intent.marks_dirty() && ctx.from_node_id.is_valid_node_id() {
                if ctx.intent.stage().is_some() {
                    record_observed_size(
                        &ctx.state_from,
                        &ctx.repository_from,
                        ctx.from_node_id,
                        &ctx.observed,
                    )
                    .await?;
                }
                let settled = if action == change::FileAction::Move {
                    SettledAction::Move
                } else {
                    SettledAction::Modify
                };
                mark_settled(
                    &ctx.state_from,
                    &ctx.repository_from,
                    ctx.from_node_id,
                    settled,
                    ctx.intent,
                )
                .await?;
            }

            let item_path = file_path.to_path();
            add_change(
                ctx.create_from_change_state(
                    from_path.cloned().unwrap_or_else(|| item_path.clone()),
                ),
                ctx.new_file_change_state(item_path.clone()),
                action,
                change::Flags::Modify,
                changes,
                filter_mode,
                ctx.states,
            )
            .await?;

            stats.file_replace.fetch_add(1, Ordering::Relaxed);
        }
        SingleFileCompareResult::NewFile => {
            lore_trace!("File {} is new (not in state)", file_path);

            // Scan: create the Dirty+Add node in state first, then route add_change
            // through its NodeID so compute_change_flags loads it and sets Dirty.
            let to_state = if !is_filesystem_directory && ctx.intent.marks_dirty() {
                NodeChangeState {
                    mapping: NodeMapping {
                        repository: ctx.repository_from.clone(),
                        state: ctx.state_from.clone(),
                        path: file_path.to_path(),
                        node: ctx.add_new_node(&file_path.to_path(), false).await?,
                    },
                    observed: None,
                    flags: NodeFlags::File | NodeFlags::DirtyAdd,
                    address: Address::default(),
                    mode: 0,
                }
            } else if is_filesystem_directory {
                ctx.new_directory_change_state(file_path.to_path())
            } else {
                ctx.new_file_change_state(file_path.to_path())
            };

            add_change(
                ctx.invalid_change_state(file_path.to_path()),
                to_state,
                FileAction::Add,
                change::Flags::None,
                changes,
                filter_mode,
                ctx.states,
            )
            .await?;

            stats.file_add.fetch_add(1, Ordering::Relaxed);
        }
        SingleFileCompareResult::TypeChangedToFile => {
            lore_trace!(
                "Type changed at {} - state has directory/link, filesystem has file, delete + add",
                file_path
            );
            return emit_type_replacement(ctx, &file_path.to_path(), false, changes, filter_mode)
                .await;
        }
        SingleFileCompareResult::TypeChangedToDirectory => {
            lore_trace!(
                "Type changed at {} - state has file, filesystem has directory, delete + add",
                file_path
            );
            return emit_type_replacement(ctx, &file_path.to_path(), true, changes, filter_mode)
                .await;
        }
    }
    Ok(INVALID_NODE)
}

/// Handle diff for a directory path.
/// All items from the listing are children of `node_path`.
#[allow(clippy::too_many_arguments)]
async fn diff_filesystem_directory(
    ctx: FilesystemDiffContext,
    file_listing: lore_io::DirStream,
    changes: &ChangeSender,
) -> Result<FilesystemDiffStats, StateError> {
    /// A staging walk includes the children staged for delete, so a path the file system
    /// still holds takes its delete back rather than being added a second time beside it.
    async fn collect_node_list(
        traversal: &NodeMapping,
        include_deleted: bool,
    ) -> Result<StateChildrenNodes, StateError> {
        let NodeMapping {
            repository,
            state,
            node: node_id,
            ..
        } = traversal;
        Ok(if node_id.is_valid_or_root_node_id() {
            let node = state.node(repository.clone(), *node_id).await?;
            if node.is_directory() {
                state
                    .collect_children_unsorted(
                        repository.clone(),
                        *node_id,
                        include_deleted,
                        true, /* Traverse links */
                    )
                    .await?
            } else {
                // State has a file where filesystem has directory - treat as delete + add
                // Return state node as single item for delete comparison
                StateChildrenNodes {
                    repository: repository.clone(),
                    state: state.clone(),
                    children: vec![StateNamedNode::new(
                        *node_id,
                        0,
                        node.name_hash,
                        NodeFlags::from_bits_retain(node.flags),
                    )],
                }
            }
        } else {
            StateChildrenNodes {
                repository: repository.clone(),
                state: state.clone(),
                children: vec![],
            }
        })
    }
    // Collect state node lists (always directory mode here)
    let staging = ctx.intent.stage().is_some();
    let mut node_list = collect_node_list(&ctx.from, staging).await?;

    let mut current_node_list = collect_node_list(&ctx.current, false).await?;

    let mut tasks = JoinSet::new();
    let mut stats = FilesystemDiffStats::default();
    let mut pending_discards: Vec<NodeID> = Vec::new();

    // TODO(mjansson) Use (radix) sorter on name for scaling to directories with many entries
    named_node_sort(&mut node_list.children);
    named_node_sort(&mut current_node_list.children);

    let mut node_list_found = vec![false; node_list.children.len()];

    // Run the walk in a helper so any `?` early-out still hits the
    // drain below — otherwise the JoinSet drops with subtree-recursion
    // tasks still running, leaking the Arc<RepositoryContext> clones.
    let work_result = diff_filesystem_directory_walk(
        &ctx,
        file_listing,
        &node_list,
        &current_node_list,
        &mut node_list_found,
        &mut tasks,
        changes,
        &mut stats,
        &mut pending_discards,
    )
    .await;
    let drain_result = lore_drain_tasks!(tasks, StateError::internal("Task failure"));
    work_result?;
    drain_result?;
    if let Some(discards) = &ctx.discards {
        discards.queue(
            node_list.state.clone(),
            node_list.repository.clone(),
            pending_discards,
        );
    }
    Ok(stats)
}

/// Emit a single `Delete` change for one node, reloading it so any dirty flags
/// just persisted by [`State::node_mark`] are reflected in the record.
async fn emit_single_delete(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    path: &RelativePath,
    changes: &ChangeSender,
) -> Result<(), StateError> {
    let block = state
        .block(repository.clone(), NodeBlock::index(node_id))
        .await?;
    let node = block.node(Node::index(node_id));
    emit(changes, move || {
        let from = NodeChangeState {
            mapping: NodeMapping {
                repository,
                state,
                path: path.clone(),
                node: node_id,
            },
            observed: None,
            flags: NodeFlags::from_bits_retain(node.flags),
            address: node.address,
            mode: node.mode,
        };
        let to = from.invalid(path.clone());
        NodeChange {
            action: FileAction::Delete,
            flags: compute_change_flags(&node),
            from,
            to,
        }
    })
    .await
}

/// A mount stays absent while the linked repository cannot be read, so the working tree not
/// holding one says nothing about the link. `link remove` takes a link out of the tree.
fn keeps_absent_link(node: &Node, path: &RelativePath) -> bool {
    if !node.is_link() {
        return false;
    }
    lore_trace!("Link mount {path} is absent from the filesystem, keeping the link");
    true
}

/// Emit the buffered ancestor-directory deletes, outermost first, and clear the
/// buffer so sibling subtrees don't re-emit them. When the intent marks dirty each
/// directory is marked `DirtyDelete` first so a later bare `stage` (which walks
/// dirty flags rather than rescanning) picks up the directory deletion.
async fn flush_pending_dir_deletes(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    changes: &ChangeSender,
    pending: &mut Vec<(NodeID, RelativePath)>,
    intent: FilesystemDiffIntent,
) -> Result<(), StateError> {
    for (node_id, path) in std::mem::take(pending) {
        if intent.marks_dirty() {
            mark_settled(state, repository, node_id, SettledAction::Delete, intent).await?;
        }
        emit_single_delete(state.clone(), repository.clone(), node_id, &path, changes).await?;
    }
    Ok(())
}

/// Walk a revision subtree that is absent from the filesystem and emit `Delete`
/// changes for only the portion that was actually materialized on disk under the
/// active filter, returning whether anything materialized.
///
/// Materialization mirrors clone/checkout discovery: excluded children are
/// pruned (never written), a non-excluded file or link materializes, and a
/// directory materializes when a descendant does or — when it has no children —
/// when the empty directory itself is not excluded.
///
/// A directory can evaluate as "not excluded" only because the filter let the
/// diff descend through it (a view re-inclusion's generated traversal rules, or a
/// glob matching the directory but not content deeper inside it) while nothing
/// under it is in view. Such a directory is never written, so its delete record
/// is buffered in `pending` and emitted only once a materializing descendant
/// proves it existed on disk; if none does, the buffered entry is dropped. This
/// keeps the report from claiming a delete for a directory that was never there.
///
/// When the intent marks dirty, every node a delete is emitted for — the
/// materializing leaf and each flushed ancestor directory — is marked
/// `DirtyDelete` so the persisted dirty-tracking state records the deletion at
/// the granularity it is reported. An unforced `node_mark` short-circuits on a node
/// already carrying the base `Dirty` bit (which `DirtyDelete` includes), so a
/// sibling's upward propagation never clobbers a directory's `DirtyDelete`.
///
/// Under a staging intent [`stage_absent_subtree`] settles the subtree, and the answer is whether
/// it deleted the subtree whole.
///
/// A directory holding a link is never buffered: the link stays, so the directory does too.
#[allow(clippy::too_many_arguments)]
async fn emit_filesystem_subtree_deletes(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    node: &Node,
    path: &RelativePath,
    states: FilterStates,
    filter_mode: FilterMode,
    intent: FilesystemDiffIntent,
    changes: &ChangeSender,
    pending: &mut Vec<(NodeID, RelativePath)>,
    link_mounts: &[LinkMountInfo],
) -> Result<bool, StateError> {
    if intent.stage().is_some() {
        return Box::pin(stage_absent_subtree(
            state,
            repository,
            node_id,
            node,
            path,
            states,
            filter_mode,
            intent,
            changes,
            link_mounts,
        ))
        .await
        .map(|kept| !kept);
    }

    // Caller guarantees `node` is not filter-excluded.
    if keeps_absent_link(node, path) {
        return Ok(false);
    }

    if node.is_file() {
        flush_pending_dir_deletes(&state, &repository, changes, pending, intent).await?;
        if intent.marks_dirty() {
            mark_settled(&state, &repository, node_id, SettledAction::Delete, intent).await?;
        }
        emit_single_delete(state, repository, node_id, path, changes).await?;
        return Ok(true);
    }

    let holds_link = LinkMountInfo::any_under(link_mounts, path);
    if !holds_link {
        pending.push((node_id, path.clone()));
    }
    let depth = pending.len();

    let mut children =
        StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node_id).await?;
    let mut had_child = false;
    let mut any_materialized = false;
    while let Some((child_id, child_node, child_name)) = children.next().await? {
        had_child = true;
        let child_path = path.push_into_buf(&child_name).freeze();
        // Release the block read lock before recursing (see NodeNameLock docs).
        drop(child_name);
        let (child_states, excluded) = repository.filter.child_excludes_tree(
            states,
            &child_path,
            child_node.is_directory(),
            filter_mode,
        );
        if excluded {
            continue;
        }
        if Box::pin(emit_filesystem_subtree_deletes(
            state.clone(),
            repository.clone(),
            child_id,
            &child_node,
            &child_path,
            child_states,
            filter_mode,
            intent,
            changes,
            pending,
            link_mounts,
        ))
        .await?
        {
            any_materialized = true;
        }
    }

    if any_materialized {
        // The first materializing descendant already flushed this directory.
        return Ok(true);
    }

    if !had_child && !states.excluded(filter_mode) {
        // Empty in-view directory: clone/checkout writes it, so its absence is a
        // real deletion. It is the materializing leaf here, and its own buffered
        // entry (pushed above) is flushed and marked along with its ancestors.
        flush_pending_dir_deletes(&state, &repository, changes, pending, intent).await?;
        return Ok(true);
    }

    // Nothing under this directory materialized: drop its buffered entry.
    if !holds_link {
        pending.truncate(depth - 1);
    }
    Ok(false)
}

/// Settle the deletion of the absent subtree at `node` for a staging walk, as `stage` stages an
/// absent directory, answering whether part of it is kept.
///
/// A child the filter excludes stays, with every directory on its path: the working tree lacks it
/// because the filter left it out, not because it was deleted. A link stays too, and so does a
/// directory the filter excludes that holds no children, which nothing materializes. Each node
/// deleted is marked and reported once its children are settled.
#[allow(clippy::too_many_arguments)]
async fn stage_absent_subtree(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    node: &Node,
    path: &RelativePath,
    states: FilterStates,
    filter_mode: FilterMode,
    intent: FilesystemDiffIntent,
    changes: &ChangeSender,
    link_mounts: &[LinkMountInfo],
) -> Result<bool, StateError> {
    if keeps_absent_link(node, path) {
        return Ok(true);
    }
    if node.is_directory() {
        let mut kept = LinkMountInfo::any_under(link_mounts, path);
        let mut had_child = false;
        let mut children =
            StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node_id)
                .await?;
        while let Some((child_id, child_node, child_name)) = children.next().await? {
            had_child = true;
            let child_path = path.push_into_buf(&child_name).freeze();
            // Release the block read lock before recursing (see NodeNameLock docs).
            drop(child_name);
            let (child_states, excluded) = repository.filter.child_excludes_tree(
                states,
                &child_path,
                !child_node.is_file(),
                filter_mode,
            );
            kept |= excluded
                || Box::pin(stage_absent_subtree(
                    state.clone(),
                    repository.clone(),
                    child_id,
                    &child_node,
                    &child_path,
                    child_states,
                    filter_mode,
                    intent,
                    changes,
                    link_mounts,
                ))
                .await?;
        }
        if kept || (!had_child && states.excluded(filter_mode)) {
            return Ok(true);
        }
    }
    mark_settled(&state, &repository, node_id, SettledAction::Delete, intent).await?;
    emit_single_delete(state, repository, node_id, path, changes).await?;
    Ok(false)
}

/// Whether staging leaves the name alone: a scratch file of the client's own making, or the
/// base and theirs sides a merge resolution has not consumed yet. A marking walk reports
/// them, since the working tree does hold them.
///
/// The mine side is not among them, as `stage` does not skip it either: a resolution that
/// keeps the working copy leaves it beside the resolved file.
fn staging_ignores_name(name: &str) -> bool {
    name.ends_with(TEMP_FILE_EXTENSION)
        || name.ends_with(BASE_SUFFIX)
        || name.ends_with(THEIRS_SUFFIX)
}

/// The index in `node_list` of the child an entry named `name_hash` claims, `None` where the
/// directory holds none.
///
/// Two children can share a folded name: a staged replacement sits beside the node it
/// displaced, and two spellings of one name can both be committed. A staging walk claims one
/// child per entry and prefers one not staged for delete, so a replacement is not replaced
/// again; only a run of more than one child is read to decide. Every other intent reports
/// against whichever the search reached, as it has.
async fn claim_named_child(
    node_list: &StateChildrenNodes,
    claimed: &[bool],
    name_hash: u64,
    staging: bool,
) -> Result<Option<usize>, StateError> {
    let children = node_list.children.as_slice();
    let Ok(found) = children.binary_search_by(|child| child.name.cmp(&name_hash)) else {
        return Ok(None);
    };
    if !staging {
        return Ok(Some(found));
    }

    let mut start = found;
    while start > 0 && children[start - 1].name == name_hash {
        start -= 1;
    }
    let mut end = found + 1;
    while end < children.len() && children[end].name == name_hash {
        end += 1;
    }
    if end - start == 1 {
        return Ok((!claimed[start]).then_some(start));
    }

    let mut displaced = None;
    for index in start..end {
        if claimed[index] {
            continue;
        }
        let node = node_list
            .state
            .node(node_list.repository.clone(), children[index].node)
            .await?;
        if !node.is_staged_delete() {
            return Ok(Some(index));
        }
        displaced = displaced.or(Some(index));
    }
    Ok(displaced)
}

/// Whether the tree and the file system agree on the type at a path. A type change replaces
/// the node rather than settling it, so staging decides only where they agree.
fn types_agree(from_node: &Node, is_file: bool, is_directory: bool) -> bool {
    (from_node.is_file() && is_file)
        || ((from_node.is_directory() || from_node.is_link()) && is_directory)
}

/// How a staging walk treats a node the tree holds, taking back a staged delete before
/// anything is compared.
///
/// A node already carrying a staged action is left as it is, so staging the same tree twice
/// reports the second pass as staging nothing. Force and a merge both ask for the action
/// again regardless, a merge because its own flags have to reach the node.
///
/// Taking back a delete clears the staged flags before marking, which is what drops a merge's
/// own flags: [`State::node_mark`] preserves those, and a node whose delete was taken back
/// carries no merge.
async fn staged_entry(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node_id: NodeID,
    node: &Node,
    stage: StageIntent,
    forced: bool,
) -> Result<StagedEntry, StateError> {
    let insists = forced
        || stage.node_flags.contains(NodeFlags::StagedMerge)
        || node.is_staged_merge_unresolved();
    if node.is_staged_delete() {
        state.node_clear(repository, node_id, Clear::Staged).await?;
        mark_settled(
            state,
            repository,
            node_id,
            SettledAction::Modify,
            FilesystemDiffIntent::Stage(stage),
        )
        .await?;
        return Ok(StagedEntry::Undeleted);
    }
    if node.is_staged() && !insists {
        return Ok(StagedEntry::Settled);
    }
    Ok(StagedEntry::Compare { insisted: insists })
}

/// Match each filesystem item from `file_receiver` against `node_list` (the
/// `from` state's children) and `current_node_list` (the `current` state's
/// children), emitting changes through `changes`, marking matched entries in
/// `node_list_found`, spawning subtree-recursion tasks into `tasks`, and
/// queueing stale directory nodes into `pending_discards`. Items with no
/// match in `node_list` are buffered and processed as new adds once the
/// receiver is drained. Must only be called from [`diff_filesystem_directory`],
/// which sorts `node_list` and `current_node_list` by name beforehand — the
/// binary searches here assume that ordering.
///
/// A scan reconciles a node the current revision does not hold and no walked
/// entry matched — removed from disk, or a directory the walk declined as a
/// nested working copy — by queueing it for discard rather than reporting a
/// `Delete`: with no committed base there is nothing to delete from, and no
/// mutation verb would clear the entry.
///
/// Each kind of entry other than a file, and each pass after the listing, runs
/// in a function of its own. A local live across several awaits takes space in
/// every state of the future holding it, so kept here those would add to the
/// file comparison, the walk's largest state.
#[allow(clippy::too_many_arguments)]
async fn diff_filesystem_directory_walk(
    ctx: &FilesystemDiffContext,
    mut file_listing: lore_io::DirStream,
    node_list: &StateChildrenNodes,
    current_node_list: &StateChildrenNodes,
    node_list_found: &mut [bool],
    tasks: &mut SubtreeTasks,
    changes: &ChangeSender,
    stats: &mut FilesystemDiffStats,
    pending_discards: &mut Vec<NodeID>,
) -> Result<(), StateError> {
    let repository_root = ctx.from.repository.require_path()?;
    let staging = ctx.intent.stage().is_some();
    let forced = staging && execution_context().globals().force();
    let mut nested_probe: Option<std::path::PathBuf> = None;
    let mut new_file_list = vec![];
    let mut entry_buffer = ctx
        .filesystem_path
        .to_buf_with_capacity(RelativePath::COMPONENT_ROOM);
    while let Some(entry) = file_listing.next().await {
        let Some(item) = crate::fs::os::file_list_item(entry)
            .forward::<StateError>("Unusable directory entry")?
        else {
            continue;
        };
        if is_reserved_node_name(item.name.as_str()) {
            continue;
        }
        if staging && staging_ignores_name(item.name.as_str()) {
            continue;
        }

        let entry = EntryPath::enter(&mut entry_buffer, item.name.as_str());

        // Claimed before the filter is asked: a node the tree tracks as the kind the entry is is
        // past the ignore slot, which the tree is asked only where the rules would leave the entry
        // out. An entry the filter drops is left unclaimed, as before.
        let claimed =
            claim_named_child(node_list, node_list_found, item.name_hash, staging).await?;
        let is_directory = item.metadata.kind == StatKind::Directory;
        let filter = &ctx.from.repository.filter;
        let mode = filter
            .tracked_mode(
                ctx.states,
                entry.path(),
                is_directory,
                ctx.filter_mode | FilterMode::KeepTracked,
                async || match claimed {
                    Some(index) => {
                        let named = &node_list.children[index];
                        node_list
                            .state
                            .tracks_as(
                                &node_list.repository,
                                named.node,
                                named.flags(),
                                is_directory,
                            )
                            .await
                    }
                    None => Ok(false),
                },
            )
            .await?;
        let (item_states, excluded) =
            filter.child_emit_excludes(ctx.states, entry.path(), is_directory, mode);
        if excluded {
            continue;
        }

        let Some(current_index) = claimed else {
            new_file_list.push((item, item_states));
            continue;
        };

        let from_named_node = &node_list.children[current_index];
        node_list_found[current_index] = true;

        let current_match = match current_node_list
            .children
            .as_slice()
            .binary_search_by(|child| child.name.cmp(&item.name_hash))
        {
            Ok(index) => {
                let current_node_id = current_node_list.children[index].node;
                get_node_match(
                    current_node_list,
                    current_node_id,
                    item.name.as_str(),
                    &ctx.current.path,
                )
                .await?
                .map(|matched| (current_node_id, matched))
            }
            Err(_) => None,
        };
        let current_node_id = current_match
            .as_ref()
            .map_or(INVALID_NODE, |(node_id, _)| *node_id);
        let current_node = current_match
            .as_ref()
            .map_or_else(Node::default, |(_, matched)| matched.node);

        let Some(from_match) = get_node_match(
            node_list,
            from_named_node.node,
            item.name.as_str(),
            &ctx.from.path,
        )
        .await?
        else {
            continue;
        };
        let from_node = from_match.node;

        let was_file = from_node.is_file();
        let was_directory = from_node.is_directory();
        let was_link = from_node.is_link();

        let is_directory = item.metadata.kind == StatKind::Directory;
        let is_file = item.metadata.kind == StatKind::File;

        let is_rename = from_match.renamed();

        let staged = match ctx.intent.stage() {
            Some(stage) if types_agree(&from_node, is_file, is_directory) => {
                staged_entry(
                    &node_list.state,
                    &node_list.repository,
                    from_named_node.node,
                    &from_node,
                    stage,
                    forced,
                )
                .await?
            }
            _ => StagedEntry::Compare { insisted: false },
        };

        if was_file && is_file {
            match staged {
                StagedEntry::Settled => continue,
                StagedEntry::Undeleted => {
                    emit_add_node_single(
                        node_list.repository.clone(),
                        node_list.state.clone(),
                        from_named_node.node,
                        &entry.to_path(),
                        changes,
                        stats,
                    )
                    .await?;
                    continue;
                }
                StagedEntry::Compare { .. } => {}
            }

            // A node in state_from but not in state_current is an unstaged
            // add — the file's presence on disk is the add. Comparing the
            // filesystem hash against the staged node's zero address would
            // misclassify, so emit Add+Dirty directly and skip the compare.
            if ctx.intent.marks_dirty() && !current_node_id.is_valid_node_id() {
                emit_unstaged_add(
                    node_list.repository.clone(),
                    node_list.state.clone(),
                    from_named_node.node,
                    from_node,
                    &entry.to_path(),
                    &FileInfo::from_stat(&item.metadata),
                    changes,
                    stats,
                    ctx.filter_mode,
                    item_states,
                    ctx.intent,
                )
                .await?;
                continue;
            }

            let current_node_ref = if current_node_id.is_valid_node_id() {
                Some(&current_node)
            } else {
                None
            };

            let observed = FileInfo::from_stat(&item.metadata);
            let compare_result = compare_single_file_against_state(
                &ctx.operation,
                node_list.repository.clone(),
                Some(&from_node),
                current_node_ref,
                &observed,
                entry.path(),
                stats,
            )
            .await?;

            // Create context for generating changes
            let file_ctx = FileDiffContext {
                repository_from: node_list.repository.clone(),
                state_from: node_list.state.clone(),
                from_node_id: from_named_node.node,
                from_node: Some(from_node),
                parent_node_id: Some(ctx.from.node),
                intent: ctx.intent,
                states: item_states,
                observed,
                insists: matches!(staged, StagedEntry::Compare { insisted: true }),
            };

            handle_single_file_compare_result(
                &file_ctx,
                compare_result,
                entry.path(),
                from_match.renamed_path.as_ref(),
                false, // filesystem item is a file, not directory
                changes,
                stats,
                ctx.filter_mode,
            )
            .await?;
        } else if was_link && is_directory {
            diff_filesystem_linked_directory(
                ctx,
                node_list,
                from_named_node,
                from_node,
                current_node,
                &from_match,
                &current_match,
                &entry,
                &item,
                item_states,
                staged,
                tasks,
                changes,
                stats,
            )
            .await?;
        } else if was_directory && is_directory {
            let descended = diff_filesystem_matched_directory(
                ctx,
                node_list,
                current_node_list,
                from_named_node,
                from_node,
                current_node_id,
                current_node,
                &from_match,
                &current_match,
                &entry,
                &item,
                item_states,
                staged,
                is_rename,
                &mut nested_probe,
                repository_root,
                tasks,
                changes,
                stats,
            )
            .await?;
            if !descended {
                node_list_found[current_index] = false;
            }
        } else {
            diff_filesystem_type_change(
                ctx,
                node_list,
                from_named_node,
                from_node,
                &entry,
                &item,
                item_states,
                is_file,
                is_directory,
                &mut nested_probe,
                repository_root,
                tasks,
                changes,
                stats,
            )
            .await?;
        }
    }

    diff_filesystem_missing_nodes(
        ctx,
        node_list,
        current_node_list,
        node_list_found,
        staging,
        changes,
        pending_discards,
    )
    .await?;

    diff_filesystem_new_entries(
        ctx,
        &mut new_file_list,
        current_node_list,
        &mut nested_probe,
        repository_root,
        staging,
        tasks,
        changes,
        stats,
    )
    .await?;

    while let Some(joined) = tasks.join_next().await {
        diff_filesystem_subtree_merge_task(joined, stats)?;
    }

    Ok(())
}

/// Walks a directory the tree holds as a link, against the linked state, for
/// [`diff_filesystem_directory_walk`].
#[allow(clippy::too_many_arguments)]
async fn diff_filesystem_linked_directory(
    ctx: &FilesystemDiffContext,
    node_list: &StateChildrenNodes,
    from_named_node: &StateNamedNode,
    from_node: Node,
    current_node: Node,
    from_match: &NodeMatch,
    current_match: &Option<(NodeID, NodeMatch)>,
    entry: &EntryPath<'_>,
    item: &FileListItem,
    item_states: FilterStates,
    staged: StagedEntry,
    tasks: &mut SubtreeTasks,
    changes: &ChangeSender,
    stats: &mut FilesystemDiffStats,
) -> Result<(), StateError> {
    let item_path = entry.to_path();
    let from_path = from_match.path(&ctx.from.path, item.name.as_str());
    let current_path = current_match
        .as_ref()
        .map_or_else(RelativePath::new, |(_, matched)| {
            matched.path(&ctx.current.path, item.name.as_str())
        });
    if staged == StagedEntry::Undeleted {
        emit_add_node_single(
            node_list.repository.clone(),
            node_list.state.clone(),
            from_named_node.node,
            &item_path,
            changes,
            stats,
        )
        .await?;
    }
    let current_link = current_node.is_link().then(|| current_node.linked_node());
    let (from, current) = crate::state::cross_links(
        DiffSide {
            repository: ctx.from.repository.clone(),
            state: ctx.from.state.clone(),
            node: from_named_node.node,
        },
        DiffSide {
            repository: ctx.current.repository.clone(),
            state: ctx.current.state.clone(),
            node: current_match
                .as_ref()
                .map_or(INVALID_NODE, |(node, _)| *node),
        },
        Some(&from_node.linked_node()),
        current_link.as_ref(),
    )
    .await?;
    let from_item_states = ctx
        .from
        .repository
        .filter
        .child_excludes_tree(ctx.from_states, &from_path, true, ctx.filter_mode)
        .0;
    // The mounts are spelled from the from-side path the walk below compares against.
    let linked_mounts =
        crate::state::collect_crossed_mounts(&from, from.node, &current, current.node, &from_path)
            .await?;
    diff_filesystem_subtree_dispatch(
        FilesystemDiffContext {
            operation: ctx.operation.clone(),
            from: NodeMapping {
                repository: from.repository,
                state: from.state,
                path: from_path,
                node: from.node,
            },
            current: NodeMapping {
                repository: current.repository,
                state: current.state,
                path: current_path,
                node: current.node,
            },
            filesystem_path: item_path,
            states: item_states,
            from_states: from_item_states,
            named_path: None,
            filter_mode: ctx.filter_mode,
            intent: ctx.intent,
            discards: ctx.discards.clone(),
            layer_mounts: ctx.layer_mounts.clone(),
            link_mounts: Arc::new(linked_mounts),
        },
        tasks,
        changes,
        stats,
    )
    .await?;

    Ok(())
}

/// A path a caller names that the `from` tree holds as a file and the working tree as a
/// directory, replaced and walked as [`diff_filesystem_type_change`] does for an entry of its
/// parent's listing.
async fn diff_named_type_change(
    mut ctx: FilesystemDiffContext,
    listing: lore_io::DirStream,
    changes: &ChangeSender,
) -> Result<FilesystemDiffStats, StateError> {
    let mut parent = ctx
        .filesystem_path
        .parent_path()
        .to_absolute_path(ctx.from.repository.require_path()?)
        .forward::<StateError>("placing the named directory below the root")?;
    let name = ctx.filesystem_path.name();
    parent.push(name);
    let stat = lore_io::IoDriver::global()
        .symlink_stat(parent.as_path())
        .await
        .map_err(crate::fs::filesystem_provider::FsError::from)
        .forward::<StateError>("reading the named directory")?;
    parent.pop();
    // The listing follows a symbolic link at the path, which the walk over its parent skips,
    // leaving the file missing.
    if stat.kind != StatKind::Directory {
        return diff_filesystem_missing(
            ctx.from,
            ctx.filesystem_path,
            ctx.states,
            ctx.filter_mode,
            ctx.intent,
            changes,
            &ctx.link_mounts,
        )
        .await;
    }
    let file = ctx
        .from
        .state
        .node(ctx.from.repository.clone(), ctx.from.node)
        .await?;
    let replacement = emit_type_replacement(
        &FileDiffContext {
            repository_from: ctx.from.repository.clone(),
            state_from: ctx.from.state.clone(),
            from_node_id: ctx.from.node,
            from_node: Some(file),
            parent_node_id: Some(file.parent),
            intent: ctx.intent,
            states: ctx.states,
            observed: FileInfo::from_stat(&stat),
            insists: false,
        },
        &ctx.filesystem_path,
        true,
        changes,
        ctx.filter_mode,
    )
    .await?;
    if !descends_into_replacement(replacement, ctx.intent, &mut parent, name).await {
        return Ok(FilesystemDiffStats::default());
    }
    ctx.from.node = replacement;
    ctx.current = NodeMapping {
        node: INVALID_NODE,
        path: RelativePath::new(),
        ..ctx.current
    };
    ctx.intent = ctx.intent.below_replacement();
    diff_filesystem_directory(ctx, listing, changes).await
}

/// Whether a walk descends into the directory `name` below `parent` that replaced a file,
/// against `replacement`, the node the intent minted for it.
///
/// A walk that only reports mints no node and walks the directory against an empty listing, so
/// its content is reported as added and a reversed diff deletes it before the directory itself,
/// untracked content included, since a reset puts the committed file back. A marking walk without
/// a minted node stays out: its chain would resolve the path to the old file node and queue that
/// node for discard. A nested repository root stays out too, as it does everywhere else in the
/// walk: reported as added, a reversed diff would delete another repository's working files.
async fn descends_into_replacement(
    replacement: NodeID,
    intent: FilesystemDiffIntent,
    parent: &mut std::path::PathBuf,
    name: &str,
) -> bool {
    if replacement.is_valid_node_id() {
        return true;
    }
    if intent.marks_dirty() {
        return false;
    }
    let nested = is_nested_repository_root(parent, name).await;
    if nested {
        lore_trace!("Not descending into nested repository root {name} that replaced a file");
    }
    !nested
}

/// Settles a directory both the tree and the file system hold and walks below it, for
/// [`diff_filesystem_directory_walk`]. `false` when a marking walk finds an uncommitted entry at
/// the root of a nested repository, which the walk discards rather than descends into.
///
/// An uncommitted directory the filter excludes that holds nothing staged is settled as a new one
/// is, by [`diff_filesystem_new_excluded_directory`].
#[allow(clippy::too_many_arguments)]
async fn diff_filesystem_matched_directory(
    ctx: &FilesystemDiffContext,
    node_list: &StateChildrenNodes,
    current_node_list: &StateChildrenNodes,
    from_named_node: &StateNamedNode,
    from_node: Node,
    current_node_id: NodeID,
    current_node: Node,
    from_match: &NodeMatch,
    current_match: &Option<(NodeID, NodeMatch)>,
    entry: &EntryPath<'_>,
    item: &FileListItem,
    item_states: FilterStates,
    staged: StagedEntry,
    is_rename: bool,
    nested_probe: &mut Option<std::path::PathBuf>,
    repository_root: &std::path::Path,
    tasks: &mut SubtreeTasks,
    changes: &ChangeSender,
    stats: &mut FilesystemDiffStats,
) -> Result<bool, StateError> {
    let item_path = entry.to_path();
    let from_path = from_match.path(&ctx.from.path, item.name.as_str());
    let current_path = current_match
        .as_ref()
        .map_or_else(RelativePath::new, |(_, matched)| {
            matched.path(&ctx.current.path, item.name.as_str())
        });
    let uncommitted = ctx.intent.marks_dirty() && !current_node_id.is_valid_node_id();
    if uncommitted {
        let probe = ctx
            .filesystem_path
            .to_absolute_path_cached(repository_root, nested_probe)
            .forward::<StateError>("placing the walked directory below the root")?;
        if is_nested_repository_root(probe, item.name.as_str()).await {
            lore_trace!("Discarding zombie entry for nested repository root {item_path}");
            return Ok(false);
        }
    }
    let excluded_new =
        uncommitted && !from_node.is_staged() && item_states.excluded(ctx.filter_mode);
    if staged == StagedEntry::Undeleted {
        emit_add_node_single(
            node_list.repository.clone(),
            node_list.state.clone(),
            from_named_node.node,
            &item_path,
            changes,
            stats,
        )
        .await?;
    } else if staged == StagedEntry::Settled {
        // The node already carries its staged action; the descent below still runs.
    } else if uncommitted {
        if !excluded_new {
            emit_dirty_add_node_single(
                node_list.repository.clone(),
                node_list.state.clone(),
                from_named_node.node,
                &item_path,
                changes,
                stats,
                ctx.intent,
            )
            .await?;
        }
    } else if is_rename {
        let measured =
            if from_node.address == current_node.address && from_node.mode == current_node.mode {
                change::Flags::None
            } else {
                change::Flags::Modify
            };
        add_change(
            NodeChangeState {
                mapping: NodeMapping {
                    repository: node_list.repository.clone(),
                    state: node_list.state.clone(),
                    path: from_path.clone(),
                    node: from_named_node.node,
                },
                observed: None,
                flags: NodeFlags::from_bits_retain(from_node.flags),
                address: from_node.address,
                mode: from_node.mode,
            },
            NodeChangeState {
                mapping: NodeMapping {
                    repository: current_node_list.repository.clone(),
                    state: current_node_list.state.clone(),
                    path: item_path.clone(),
                    node: current_node_id,
                },
                observed: None,
                flags: NodeFlags::from_bits_retain(current_node.flags),
                address: current_node.address,
                mode: current_node.mode,
            },
            FileAction::Move,
            measured,
            changes,
            ctx.filter_mode,
            item_states,
        )
        .await?;
    } else if matches!(staged, StagedEntry::Compare { insisted: true }) {
        settle_insisted_directory(
            node_list,
            from_named_node.node,
            &from_node,
            &item_path,
            FileInfo::from_stat(&item.metadata),
            changes,
            ctx.intent,
            ctx.filter_mode,
        )
        .await?;
    }
    let from_item_states = ctx
        .from
        .repository
        .filter
        .child_excludes_tree(ctx.from_states, &from_path, true, ctx.filter_mode)
        .0;
    let current_link = current_node.is_link().then(|| current_node.linked_node());
    let (from, current) = crate::state::cross_links(
        DiffSide {
            repository: node_list.repository.clone(),
            state: node_list.state.clone(),
            node: from_named_node.node,
        },
        DiffSide {
            repository: current_node_list.repository.clone(),
            state: current_node_list.state.clone(),
            node: current_node_id,
        },
        None,
        current_link.as_ref(),
    )
    .await?;
    let link_mounts_recurse = match current_link {
        Some(_) => Arc::new(Vec::new()),
        None => ctx.link_mounts.clone(),
    };
    let subtree = FilesystemDiffContext {
        operation: ctx.operation.clone(),
        from: NodeMapping {
            repository: from.repository,
            state: from.state,
            path: from_path,
            node: from.node,
        },
        current: NodeMapping {
            repository: current.repository,
            state: current.state,
            path: current_path,
            node: current.node,
        },
        filesystem_path: item_path,
        states: item_states,
        from_states: from_item_states,
        named_path: None,
        filter_mode: ctx.filter_mode,
        intent: ctx.intent,
        discards: ctx.discards.clone(),
        layer_mounts: ctx.layer_mounts.clone(),
        link_mounts: link_mounts_recurse,
    };
    if excluded_new {
        let parent = NodeMapping {
            repository: node_list.repository.clone(),
            state: node_list.state.clone(),
            path: ctx.from.path.clone(),
            node: from_node.parent,
        };
        Box::pin(diff_filesystem_new_excluded_dispatch(
            subtree, parent, tasks, changes, stats,
        ))
        .await?;
    } else {
        diff_filesystem_subtree_dispatch(subtree, tasks, changes, stats).await?;
    }

    Ok(true)
}

/// Replaces a node whose type the file system changed, file to directory or back, for
/// [`diff_filesystem_directory_walk`].
#[allow(clippy::too_many_arguments)]
async fn diff_filesystem_type_change(
    ctx: &FilesystemDiffContext,
    node_list: &StateChildrenNodes,
    from_named_node: &StateNamedNode,
    from_node: Node,
    entry: &EntryPath<'_>,
    item: &FileListItem,
    item_states: FilterStates,
    is_file: bool,
    is_directory: bool,
    nested_probe: &mut Option<std::path::PathBuf>,
    repository_root: &std::path::Path,
    tasks: &mut SubtreeTasks,
    changes: &ChangeSender,
    stats: &mut FilesystemDiffStats,
) -> Result<(), StateError> {
    // Type change: file <-> directory
    let file_ctx = FileDiffContext {
        repository_from: node_list.repository.clone(),
        state_from: node_list.state.clone(),
        from_node_id: from_named_node.node,
        from_node: Some(from_node),
        parent_node_id: Some(ctx.from.node),
        intent: ctx.intent,
        states: item_states,
        observed: FileInfo::from_stat(&item.metadata),
        insists: false,
    };

    // Determine the type change direction
    let compare_result = if is_file {
        SingleFileCompareResult::TypeChangedToFile
    } else {
        SingleFileCompareResult::TypeChangedToDirectory
    };

    lore_trace!(
        "Filesystem type (file/directory) differs for node {} in path {}, add delete and add changes",
        from_named_node.node,
        entry.path()
    );

    let replacement = handle_single_file_compare_result(
        &file_ctx,
        compare_result,
        entry.path(),
        None,
        is_directory,
        changes,
        stats,
        ctx.filter_mode,
    )
    .await?;

    // A directory that replaced a file is walked against the node the replacement minted, so
    // the content it holds is staged with it. A file that replaced a directory holds none.
    let descends = is_directory && {
        let probe = ctx
            .filesystem_path
            .to_absolute_path_cached(repository_root, nested_probe)
            .forward::<StateError>("placing the walked directory below the root")?;
        descends_into_replacement(replacement, ctx.intent, probe, item.name.as_str()).await
    };
    if descends {
        let item_path = entry.to_path();
        diff_filesystem_subtree_dispatch(
            FilesystemDiffContext {
                operation: ctx.operation.clone(),
                from: NodeMapping {
                    repository: ctx.from.repository.clone(),
                    state: ctx.from.state.clone(),
                    path: item_path.clone(),
                    node: replacement,
                },
                current: NodeMapping {
                    repository: ctx.current.repository.clone(),
                    state: ctx.current.state.clone(),
                    path: RelativePath::new(),
                    node: INVALID_NODE,
                },
                filesystem_path: item_path,
                states: item_states,
                from_states: item_states,
                named_path: None,
                filter_mode: ctx.filter_mode,
                intent: ctx.intent.below_replacement(),
                discards: ctx.discards.clone(),
                layer_mounts: ctx.layer_mounts.clone(),
                link_mounts: ctx.link_mounts.clone(),
            },
            tasks,
            changes,
            stats,
        )
        .await?;
    }

    Ok(())
}

/// Reports the nodes of `node_list` the listing did not match, for
/// [`diff_filesystem_directory_walk`].
async fn diff_filesystem_missing_nodes(
    ctx: &FilesystemDiffContext,
    node_list: &StateChildrenNodes,
    current_node_list: &StateChildrenNodes,
    node_list_found: &[bool],
    staging: bool,
    changes: &ChangeSender,
    pending_discards: &mut Vec<NodeID>,
) -> Result<(), StateError> {
    // Every node here is one the tree holds, which is past the ignore slot: a tracked file the
    // ignore file names is deleted like any other.
    let deletion_mode = ctx.filter_mode & FilterMode::View;
    // Nodes that were not iterated are deleted in file system
    for (index, from_named_node) in node_list.children.iter().enumerate() {
        if node_list_found[index] {
            continue;
        }

        let Some((from_node, from_node_states)) = get_filtered_node_and_path(
            node_list,
            from_named_node.node,
            &ctx.from.path,
            ctx.from_states,
            deletion_mode,
            async |_| Ok(false),
        )
        .await?
        else {
            // A node an earlier scan added that the filter now excludes leaves the tree when it was
            // never committed, as staging drops it. A staged add the filter excludes was forced,
            // so it stays.
            if ctx.intent.marks_dirty() {
                let node = node_list
                    .state
                    .node(node_list.repository.clone(), from_named_node.node)
                    .await?;
                if node.is_dirty_add()
                    && !node.is_staged()
                    && crate::stage::drops_cleanly(
                        &ctx.current.state,
                        &node_list.repository,
                        &node_list.state,
                        from_named_node.node,
                        &node,
                    )
                    .await
                    .forward::<StateError>("Failed to check a filtered node")?
                {
                    pending_discards.push(from_named_node.node);
                }
            }
            continue;
        };

        // A staging walk sees the nodes already staged for delete, and the file system
        // still not holding one says nothing new about it.
        if staging && from_node.node.is_staged_delete() {
            continue;
        }

        if ctx.intent.marks_dirty() && from_node.node.is_directory() {
            let in_current = current_node_list
                .children
                .as_slice()
                .binary_search_by(|child| child.name.cmp(&from_named_node.name))
                .is_ok();
            if !in_current {
                lore_trace!(
                    "Queueing reverted uncommitted directory node {} (no entry at {}, not in current)",
                    from_named_node.node,
                    from_node.path
                );
                pending_discards.push(from_named_node.node);
                continue;
            }
        }

        if keeps_absent_link(&from_node.node, &from_node.path) {
            continue;
        }

        // Emit deletes only for the materialized portion of the subtree,
        // suppressing directories the filter merely descended through but never
        // wrote to disk (see emit_filesystem_subtree_deletes).
        if from_node.node.is_directory() {
            let mut pending = Vec::new();
            emit_filesystem_subtree_deletes(
                node_list.state.clone(),
                node_list.repository.clone(),
                from_named_node.node,
                &from_node.node,
                &from_node.path,
                from_node_states,
                deletion_mode,
                ctx.intent,
                changes,
                &mut pending,
                &ctx.link_mounts,
            )
            .await?;
            continue;
        }

        // A leaf node present in state_from but not in state_current, with
        // no file on disk, is an unstaged add that the user reverted by
        // removing the file. Discard the node so state_staged matches the
        // filesystem rather than emitting a Delete change for a node that
        // shouldn't exist.
        let in_current = current_node_list
            .children
            .as_slice()
            .binary_search_by(|child| child.name.cmp(&from_named_node.name))
            .is_ok();
        if ctx.intent.marks_dirty() && from_node.node.is_file() && !in_current {
            lore_trace!(
                "Queueing reverted-DirtyAdd node {} (no file at {}, not in current)",
                from_named_node.node,
                from_node.path
            );
            pending_discards.push(from_named_node.node);
            continue;
        }

        // Scan: persist Dirty+Delete on the missing node before recording the change
        // so compute_change_flags loads the dirty node and includes Dirty in the event.
        if ctx.intent.marks_dirty() {
            mark_settled(
                &node_list.state,
                &node_list.repository,
                from_named_node.node,
                SettledAction::Delete,
                ctx.intent,
            )
            .await?;
        }

        lore_trace!(
            "Filesystem does not have node {} in path {}, add deleted change",
            from_named_node.node,
            ctx.filesystem_path
        );

        add_change(
            NodeChangeState {
                mapping: NodeMapping {
                    repository: node_list.repository.clone(),
                    state: node_list.state.clone(),
                    path: from_node.path.clone(),
                    node: from_named_node.node,
                },
                observed: None,
                flags: NodeFlags::from_bits_retain(from_node.node.flags),
                address: from_node.node.address,
                mode: from_node.node.mode,
            },
            NodeChangeState {
                mapping: NodeMapping {
                    repository: node_list.repository.clone(),
                    state: node_list.state.clone(),
                    path: from_node.path.clone(),
                    node: INVALID_NODE,
                },
                observed: None,
                flags: NodeFlags::NoFlags,
                address: Address::default(),
                mode: 0,
            },
            FileAction::Delete,
            change::Flags::None,
            changes,
            deletion_mode,
            from_node_states,
        )
        .await?;
    }

    Ok(())
}

/// The child of the current revision's directory that the entry at `index` of `entries` stands
/// for, where the two are of one kind, or [`INVALID_NODE`] where there is none. `entries` is
/// sorted by name hash.
///
/// A child stands for one entry of its folded name: the one spelled as the child is, or a case
/// variant where it is the only entry of the name, as [`child_standing_for`] decides. Several
/// variants with none spelled as the child stand for nothing. A lone entry of a lone child's
/// folded name stands for it without its name being read.
///
/// Where the current revision holds a file and the disk a directory, the walk below it lists
/// that file as the directory's only child, which no entry stands for.
async fn tracked_child(
    ctx: &FilesystemDiffContext,
    current_node_list: &StateChildrenNodes,
    entries: &[(FileListItem, FilterStates)],
    index: usize,
) -> Result<NodeID, StateError> {
    let item = &entries[index].0;
    let children = current_node_list.children.as_slice();
    let Ok(found) = children.binary_search_by(|child| child.name.cmp(&item.name_hash)) else {
        return Ok(INVALID_NODE);
    };
    let tracked = run_around(children, found, |child| child.name == item.name_hash);
    let listed = run_around(entries, index, |(entry, _)| {
        entry.name_hash == item.name_hash
    });
    let node_id = if tracked.len() == 1 && listed.len() == 1 {
        children[found].node
    } else {
        let standing = Box::pin(child_standing_for(
            current_node_list,
            &children[tracked],
            &entries[listed.clone()],
            index - listed.start,
        ))
        .await?;
        match standing {
            Some(node_id) => node_id,
            None => return Ok(INVALID_NODE),
        }
    };
    if node_id == ctx.current.node {
        return Ok(INVALID_NODE);
    }
    let node = current_node_list
        .state
        .node(current_node_list.repository.clone(), node_id)
        .await?;
    let agrees = types_agree(
        &node,
        item.metadata.kind == StatKind::File,
        item.metadata.kind == StatKind::Directory,
    );
    Ok(if agrees { node_id } else { INVALID_NODE })
}

/// The range of `items` around `at` that `same` holds for.
fn run_around<T>(items: &[T], at: usize, same: impl Fn(&T) -> bool) -> std::ops::Range<usize> {
    let start = items[..at]
        .iter()
        .rposition(|item| !same(item))
        .map_or(0, |before| before + 1);
    let end = items[at..]
        .iter()
        .position(|item| !same(item))
        .map_or(items.len(), |after| at + after);
    start..end
}

/// The child of `children` the entry at `at` of `entries` stands for, all of one folded name:
/// the child it spells, or where it spells none and is the only entry, the first child. A lone
/// case variant thus stands for the child on a filesystem that folds case and after a rename that
/// changes case alone, and a variant beside another entry of the name stands for nothing.
///
/// Boxed by its caller, as it is the cold path: it reads the children's names, which only a
/// directory holding two entries or two children of one folded name asks for.
async fn child_standing_for(
    nodes: &StateChildrenNodes,
    children: &[StateNamedNode],
    entries: &[(FileListItem, FilterStates)],
    at: usize,
) -> Result<Option<NodeID>, StateError> {
    for child in children {
        if spelled_as(nodes, child.node, entries[at].0.name.as_str()).await? {
            return Ok(Some(child.node));
        }
    }
    Ok((entries.len() == 1).then_some(children[0].node))
}

/// Whether the state spells the name of `node_id` as `name`.
async fn spelled_as(
    nodes: &StateChildrenNodes,
    node_id: NodeID,
    name: &str,
) -> Result<bool, StateError> {
    Ok(nodes
        .state
        .node_name_ref_or_skip(nodes.repository.clone(), node_id)
        .await?
        .is_some_and(|held| *held == *name))
}

/// Reports the entries no node matched, each with the filter's verdict for it, walking below new
/// directories, for [`diff_filesystem_directory_walk`]. A
/// [`FilesystemDiffIntent::ReportTracked`] walk leaves out each entry the current revision does
/// not track, as [`tracked_child`] decides, and sorts `new_file_list` by name hash for it.
///
/// A new directory the filter excludes stands on disk only for content below it, so a marking
/// walk walks below it first, and reports it where that walk marked something and discards it
/// where it did not.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn diff_filesystem_new_entries<'a>(
    ctx: &'a FilesystemDiffContext,
    new_file_list: &'a mut [(FileListItem, FilterStates)],
    current_node_list: &'a StateChildrenNodes,
    nested_probe: &'a mut Option<std::path::PathBuf>,
    repository_root: &'a std::path::Path,
    staging: bool,
    tasks: &'a mut SubtreeTasks,
    changes: &'a ChangeSender,
    stats: &'a mut FilesystemDiffStats,
) -> impl Future<Output = Result<(), StateError>> + 'a {
    if matches!(ctx.intent, FilesystemDiffIntent::ReportTracked) {
        new_file_list.sort_unstable_by_key(|(item, _)| item.name_hash);
    }
    let new_file_list: &'a [(FileListItem, FilterStates)] = new_file_list;
    async move {
        // Remaining files/directories are added (all are children of node_path)
        'new_file_iter: for index in 0..new_file_list.len() {
            let (file, child_states) = &new_file_list[index];
            let child_states = *child_states;
            let current = if matches!(ctx.intent, FilesystemDiffIntent::ReportTracked) {
                let current = tracked_child(ctx, current_node_list, new_file_list, index).await?;
                if !current.is_valid_node_id() {
                    continue 'new_file_iter;
                }
                current
            } else {
                INVALID_NODE
            };
            // For directory listing, new items are children
            let child_file_path = ctx
                .filesystem_path
                .push_into_buf(file.name.as_str())
                .freeze();

            let is_directory = file.metadata.kind == StatKind::Directory;

            if is_directory {
                // A directory on disk with no `state_from` node that matches a
                // link in `state_current` is a link add, not a per-file add: the
                // mounted content belongs to the linked repository. Skip the
                // entry; the link node is reported via `state::diff_collect` (in
                // `lore status`) and `link list`, not via `file diff`.
                //
                // The realistic scan-side caller (`lore status` via
                // `diff_filesystem`) stages the link in `state_from` before
                // status runs, so the link is matched in the paired
                // `was_link && is_directory` branch above and never reaches here;
                // the `continue` fires with a dirty-marking intent only in a
                // constructed corner case, where skipping dirty-add is still
                // correct (the link is not new in the working state).
                if ctx
                    .link_mounts
                    .iter()
                    .any(|m| m.mount_path.as_str() == child_file_path.as_str())
                {
                    lore_trace!(
                        "Filesystem path {child_file_path} matches a link in the current state, skipping link-internal content"
                    );
                    continue 'new_file_iter;
                }
                // Layer mount detection: if this directory's parent-relative path
                // matches a configured layer mount, switch comparison context to
                // the layer's repo and state for the recursion. The layer mount
                // itself is NOT emitted as an "add" — its content is owned by the
                // layer's pinned revision, not the parent's tree.
                if let Some(mount) = ctx
                    .layer_mounts
                    .iter()
                    .find(|m| m.mount_path.as_str() == child_file_path.as_str())
                {
                    // A staging walk leaves the mount alone: the layer's own tree is staged by
                    // its own walk against its own state, and staging it from here as well
                    // would settle the same nodes twice.
                    if staging {
                        lore_trace!("Filesystem path {child_file_path} is a layer mount, skipping");
                        continue 'new_file_iter;
                    }
                    lore_trace!(
                        "Filesystem path {child_file_path} is a layer mount, recursing into layer state"
                    );
                    let layer_repository = mount.repository.clone();
                    let layer_state = mount.state.clone();
                    let layer_source_node = mount.source_node;
                    diff_filesystem_subtree_dispatch(
                        FilesystemDiffContext {
                            operation: ctx.operation.clone(),
                            from: NodeMapping {
                                repository: layer_repository.clone(),
                                state: layer_state.clone(),
                                path: child_file_path.clone(),
                                node: layer_source_node,
                            },
                            current: NodeMapping {
                                repository: layer_repository,
                                state: layer_state,
                                path: child_file_path.clone(),
                                node: layer_source_node,
                            },
                            filesystem_path: child_file_path,
                            states: child_states,
                            // The layer is walked at its mount path, which is the
                            // path on disk.
                            from_states: child_states,
                            named_path: None,
                            filter_mode: ctx.filter_mode,
                            intent: ctx.intent,
                            discards: ctx.discards.clone(),
                            // Non-overlapping layers: no nested mounts inside a layer.
                            layer_mounts: Arc::new(vec![]),
                            // Crossing into the layer state; parent's link mounts
                            // are paths in the parent tree and do not apply here.
                            link_mounts: Arc::new(vec![]),
                        },
                        tasks,
                        changes,
                        stats,
                    )
                    .await?;
                    continue 'new_file_iter;
                }
                let probe = ctx
                    .filesystem_path
                    .to_absolute_path_cached(repository_root, nested_probe)
                    .forward::<StateError>("placing the walked directory below the root")?;
                if is_nested_repository_root(probe, file.name.as_str()).await {
                    lore_trace!("Skipping nested repository root {child_file_path}");
                    continue 'new_file_iter;
                }
                lore_trace!("Filesystem has new directory in path {child_file_path}, recursing");

                // Scan: persist a Dirty+Add node for the new directory before
                // recursing, so files inside it resolve their parent and the staged
                // anchor rebase can descend the dirty subtree. Emit it as a single
                // node (the recursion below surfaces the children) and recurse
                // against it so a rescan matches the persisted subtree.
                let mut dir_from_root = INVALID_NODE;
                let mut dir_from_path = RelativePath::new();
                let mut dir_from_states = FilterStates::ROOT;
                if ctx.intent.marks_dirty() {
                    // The new directory is a child of the directory currently being
                    // walked; its node is the correct parent even across link/layer
                    // boundaries (resolving by parent path would not match there).
                    let dir_node = Node {
                        flags: NodeFlags::DirtyAdd.bits(),
                        name_hash: crate::hash::hash_string(file.name.as_str()),
                        ..Default::default()
                    };
                    dir_from_root = ctx
                        .from
                        .state
                        .node_add(
                            ctx.from.repository.clone(),
                            ctx.from.node,
                            dir_node,
                            file.name.as_str(),
                        )
                        .await
                        .forward::<StateError>("scan add: failed to add new directory node")?;
                    // Marked before the walk below, the walk below an excluded directory included,
                    // so a failure there leaves no add under a clean directory. Discarding an
                    // excluded directory the walk added nothing below clears the mark again.
                    ctx.from
                        .state
                        .node_mark(
                            &ctx.from.repository,
                            ctx.from.node,
                            Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
                        )
                        .await?;
                    if !child_states.excluded(ctx.filter_mode) {
                        emit_dirty_add_node_single(
                            ctx.from.repository.clone(),
                            ctx.from.state.clone(),
                            dir_from_root,
                            &child_file_path,
                            changes,
                            stats,
                            ctx.intent,
                        )
                        .await?;
                    }
                    dir_from_path = child_file_path.clone();
                    dir_from_states = child_states;
                }

                let subtree = FilesystemDiffContext {
                    operation: ctx.operation.clone(),
                    from: NodeMapping {
                        repository: ctx.from.repository.clone(),
                        state: ctx.from.state.clone(),
                        path: dir_from_path,
                        node: dir_from_root,
                    },
                    current: NodeMapping {
                        repository: current_node_list.repository.clone(),
                        state: current_node_list.state.clone(),
                        path: child_file_path.clone(),
                        node: current,
                    },
                    filesystem_path: child_file_path.clone(),
                    states: child_states,
                    from_states: dir_from_states,
                    named_path: None,
                    filter_mode: ctx.filter_mode,
                    intent: ctx.intent,
                    discards: ctx.discards.clone(),
                    layer_mounts: ctx.layer_mounts.clone(),
                    // Same parent state; deeper paths may still match a link.
                    link_mounts: ctx.link_mounts.clone(),
                };
                if ctx.intent.marks_dirty() && child_states.excluded(ctx.filter_mode) {
                    Box::pin(diff_filesystem_new_excluded_dispatch(
                        subtree,
                        ctx.from.clone(),
                        tasks,
                        changes,
                        stats,
                    ))
                    .await?;
                    continue 'new_file_iter;
                }
                diff_filesystem_subtree_dispatch(subtree, tasks, changes, stats).await?;

                // The single Dirty+Add directory node emitted above is the scan's
                // report for this new directory; skip the transient change below.
                if ctx.intent.marks_dirty() {
                    continue 'new_file_iter;
                }
            }

            let file_ctx = FileDiffContext {
                repository_from: ctx.from.repository.clone(),
                state_from: ctx.from.state.clone(),
                from_node_id: INVALID_NODE,
                from_node: None,
                parent_node_id: Some(ctx.from.node),
                intent: ctx.intent,
                states: child_states,
                observed: FileInfo::from_stat(&file.metadata),
                insists: false,
            };

            lore_trace!("Filesystem has new item in path {child_file_path}, add add change");

            handle_single_file_compare_result(
                &file_ctx,
                SingleFileCompareResult::NewFile,
                &child_file_path,
                None,
                is_directory,
                changes,
                stats,
                ctx.filter_mode,
            )
            .await?;
        }

        Ok(())
    }
}

/// The subtree walks a directory has in flight, each reporting what it counted.
///
/// Nothing else comes back: a subtree emits its changes to the caller through a clone of the
/// sender rather than collecting them for the parent to fold in.
type SubtreeTasks = JoinSet<Result<FilesystemDiffStats, StateError>>;

fn diff_filesystem_task_semaphore() -> &'static Arc<Semaphore> {
    DIFF_FILESYSTEM_TASK_SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_TREE_TASKS)))
}

/// Diffs one subtree, spawned while the budget allows and inline once it does not, then folds
/// back whatever has finished.
///
/// Inline rather than a blocking acquire: a parent holds its permit until its children finish,
/// so waiting on one would wait on a descendant that cannot start.
async fn diff_filesystem_subtree_dispatch(
    subtree: FilesystemDiffContext,
    tasks: &mut SubtreeTasks,
    changes: &ChangeSender,
    stats: &mut FilesystemDiffStats,
) -> Result<(), StateError> {
    if let Ok(permit) = diff_filesystem_task_semaphore().clone().try_acquire_owned() {
        let changes = changes.clone();
        lore_spawn!(
            tasks,
            crate::subtask!(async move {
                let _permit = permit;
                diff_filesystem_subtree_recurse(subtree, &changes).await
            })
        );
    } else {
        stats.append(diff_filesystem_subtree_recurse(subtree, changes).await?);
    }
    while let Some(joined) = tasks.try_join_next() {
        diff_filesystem_subtree_merge_task(joined, stats)?;
    }
    Ok(())
}

/// [`diff_filesystem_subtree_dispatch`] for `subtree`, walked by
/// [`diff_filesystem_new_excluded_directory`].
///
/// A cold path, which its caller boxes: a walk meets it only below a rule that re-includes below
/// a directory it excludes.
async fn diff_filesystem_new_excluded_dispatch(
    subtree: FilesystemDiffContext,
    parent: NodeMapping,
    tasks: &mut SubtreeTasks,
    changes: &ChangeSender,
    stats: &mut FilesystemDiffStats,
) -> Result<(), StateError> {
    if let Ok(permit) = diff_filesystem_task_semaphore().clone().try_acquire_owned() {
        let changes = changes.clone();
        lore_spawn!(
            tasks,
            crate::subtask!(async move {
                let _permit = permit;
                diff_filesystem_new_excluded_directory(subtree, parent, &changes).await
            })
        );
    } else {
        stats.append(diff_filesystem_new_excluded_directory(subtree, parent, changes).await?);
    }
    while let Some(joined) = tasks.try_join_next() {
        diff_filesystem_subtree_merge_task(joined, stats)?;
    }
    Ok(())
}

/// The diff of `subtree`, a directory the filter excludes that a marking walk added below the
/// directory `parent` maps, in this walk or an earlier one, and that holds nothing staged. Such a
/// directory stands on disk only for content below it, so it is reported where the walk added
/// something below it and queued for discarding where it did not.
async fn diff_filesystem_new_excluded_directory(
    subtree: FilesystemDiffContext,
    parent: NodeMapping,
    changes: &ChangeSender,
) -> Result<FilesystemDiffStats, StateError> {
    let (node, path) = (subtree.from.node, subtree.filesystem_path.clone());
    let (intent, discards) = (subtree.intent, subtree.discards.clone());
    let stats = diff_filesystem_subtree_recurse(subtree, changes).await?;
    match discards {
        Some(discards) if stats.file_add.load(Ordering::Relaxed) == 0 => {
            discards.queue(parent.state, parent.repository, vec![node]);
        }
        _ => {
            emit_dirty_add_node_single(
                parent.repository,
                parent.state,
                node,
                &path,
                changes,
                &stats,
                intent,
            )
            .await?;
        }
    }
    Ok(stats)
}

/// Folds a joined subtree task's count into the parent directory's.
fn diff_filesystem_subtree_merge_task(
    joined: Result<Result<FilesystemDiffStats, StateError>, JoinError>,
    stats: &mut FilesystemDiffStats,
) -> Result<(), StateError> {
    stats.append(
        joined
            .internal("Task failure")
            .map_err(StateError::from)
            .flatten()?,
    );
    Ok(())
}

/// Handle diff for a single file path.
/// The item is the file at `node_path` (not a child).
///
/// This function uses the unified single-file comparison logic via
/// `compare_single_file_against_state` and `handle_single_file_compare_result`.
///
/// `parent_node` is the node a file the `from` tree lacks is added below, where the walk found or
/// added the directories leading to it; without one, the parent is found by the file's path.
#[allow(clippy::too_many_arguments)]
async fn diff_filesystem_single_file(
    ctx: FilesystemDiffContext,
    file_item: crate::fs::os::FileListItem,
    parent_node: Option<NodeID>,
    changes: &ChangeSender,
) -> Result<FilesystemDiffStats, StateError> {
    let stats = FilesystemDiffStats::default();

    // Path is already correct - file_item represents node_path itself
    // No path manipulation needed!

    // Get the state nodes for comparison
    let from_node = if ctx.from.node.is_valid_node_id() {
        ctx.from
            .state
            .node(ctx.from.repository.clone(), ctx.from.node)
            .await
            .ok()
    } else {
        None
    };

    let current_node = if ctx.current.node.is_valid_node_id() {
        ctx.current
            .state
            .node(ctx.current.repository.clone(), ctx.current.node)
            .await
            .ok()
    } else {
        None
    };

    let observed = FileInfo::from_stat(&file_item.metadata);

    // Staging decides on a node the tree holds before anything is compared, as the directory
    // walk does.
    let mut insisted = false;
    if let Some(stage) = ctx.intent.stage()
        && file_item.metadata.kind == StatKind::File
        && let Some(node) = from_node
        && node.is_file()
    {
        match staged_entry(
            &ctx.from.state,
            &ctx.from.repository,
            ctx.from.node,
            &node,
            stage,
            execution_context().globals().force(),
        )
        .await?
        {
            StagedEntry::Settled => return Ok(stats),
            StagedEntry::Undeleted => {
                emit_add_node_single(
                    ctx.from.repository.clone(),
                    ctx.from.state.clone(),
                    ctx.from.node,
                    &ctx.filesystem_path,
                    changes,
                    &stats,
                )
                .await?;
                return Ok(stats);
            }
            StagedEntry::Compare { insisted: asked } => insisted = asked,
        }
    }

    // A node in state_from but not in state_current is an unstaged add —
    // the file's presence on disk is the add. Skip the compare and emit
    // Add+Dirty directly.
    if ctx.intent.marks_dirty()
        && file_item.metadata.kind == StatKind::File
        && ctx.from.node.is_valid_node_id()
        && !ctx.current.node.is_valid_node_id()
        && let Some(node) = from_node
        && node.is_file()
    {
        emit_unstaged_add(
            ctx.from.repository.clone(),
            ctx.from.state.clone(),
            ctx.from.node,
            node,
            &ctx.filesystem_path,
            &observed,
            changes,
            &stats,
            ctx.filter_mode,
            ctx.states,
            ctx.intent,
        )
        .await?;
        return Ok(stats);
    }

    let compare_result = compare_single_file_against_state(
        &ctx.operation,
        ctx.from.repository.clone(),
        from_node.as_ref(),
        current_node.as_ref(),
        &observed,
        &ctx.filesystem_path,
        &stats,
    )
    .await?;

    // Create the context for generating changes
    let file_ctx = FileDiffContext {
        repository_from: ctx.from.repository.clone(),
        state_from: ctx.from.state.clone(),
        from_node_id: ctx.from.node,
        from_node,
        parent_node_id: parent_node,
        intent: ctx.intent,
        states: ctx.states,
        observed,
        insists: insisted,
    };

    handle_single_file_compare_result(
        &file_ctx,
        compare_result,
        &ctx.filesystem_path,
        None, // No rename detection for single file path
        file_item.metadata.kind == StatKind::Directory,
        changes,
        &stats,
        ctx.filter_mode,
    )
    .await?;

    Ok(stats)
}

/// Handle diff when filesystem path doesn't exist.
/// Everything in state under this path is considered deleted, bar what a link keeps.
async fn diff_filesystem_missing(
    from: NodeMapping,
    filesystem_path: RelativePath,
    states: FilterStates,
    filter_mode: FilterMode,
    intent: FilesystemDiffIntent,
    changes: &ChangeSender,
    link_mounts: &[LinkMountInfo],
) -> Result<FilesystemDiffStats, StateError> {
    let stats = FilesystemDiffStats::default();
    // Everything deleted here is a node the tree holds, which is past the ignore slot.
    let filter_mode = filter_mode & FilterMode::View;

    // Add delete changes for all nodes under the from node
    if from.node.is_valid_node_id() {
        let from_node = from.state.node(from.repository.clone(), from.node).await?;

        if keeps_absent_link(&from_node, &filesystem_path) {
            return Ok(stats);
        }

        if LinkMountInfo::any_under(link_mounts, &from.path) {
            emit_filesystem_subtree_deletes(
                from.state.clone(),
                from.repository.clone(),
                from.node,
                &from_node,
                &from.path,
                states,
                filter_mode,
                intent,
                changes,
                &mut Vec::new(),
                link_mounts,
            )
            .await?;
            return Ok(stats);
        }

        lore_trace!(
            "Filesystem path {} does not exist, marking state node {} as deleted",
            filesystem_path,
            from.node
        );

        // Scan: mark missing file as Dirty+Delete
        if intent.marks_dirty() {
            mark_settled(
                &from.state,
                &from.repository,
                from.node,
                SettledAction::Delete,
                intent,
            )
            .await?;
        }

        add_change(
            NodeChangeState {
                mapping: NodeMapping {
                    repository: from.repository.clone(),
                    state: from.state.clone(),
                    path: from.path.clone(),
                    node: from.node,
                },
                observed: None,
                flags: NodeFlags::from_bits_retain(from_node.flags),
                address: from_node.address,
                mode: from_node.mode,
            },
            NodeChangeState {
                mapping: NodeMapping {
                    repository: from.repository,
                    state: from.state,
                    path: from.path,
                    node: INVALID_NODE,
                },
                observed: None,
                flags: NodeFlags::NoFlags,
                address: Address::default(),
                mode: 0,
            },
            FileAction::Delete,
            change::Flags::None,
            changes,
            filter_mode,
            states,
        )
        .await?;
    }

    Ok(stats)
}

/// Walks one subtree, boxed so the walk can descend into itself.
fn diff_filesystem_subtree_recurse<'a>(
    ctx: FilesystemDiffContext,
    changes: &'a ChangeSender,
) -> Pin<Box<dyn Future<Output = Result<FilesystemDiffStats, StateError>> + Send + 'a>> {
    Box::pin(diff_filesystem_subtree_impl(ctx, changes))
}

/// Diffs the working tree under `diff.filesystem_path` against the trees it names, emitting a
/// change per difference through `changes`.
///
/// The module's entry, and what
/// [`OsOperation`](crate::fs::os::OsOperation)'s
/// `changes_from_filesystem_to_state` answers with.
///
/// A marking walk handed no queue for what it finds stale holds it on one of its own and
/// discards it once the whole walk has drained, so no directory discards while another is
/// still marking. Discarding is boxed as the cold path: most walks find nothing stale.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub(crate) fn diff_os_filesystem(
    mut diff: FilesystemDiffContext,
    changes: &ChangeSender,
) -> impl Future<Output = Result<FilesystemDiffStats, StateError>> + Send + '_ {
    let owned = (diff.discards.is_none() && diff.intent.marks_dirty()).then(|| {
        let discards = Arc::new(WalkDiscards::default());
        diff.discards = Some(discards.clone());
        discards
    });
    async move {
        let stats = diff_filesystem_subtree_recurse(diff, changes).await?;
        if let Some(discards) = owned.filter(|discards| !discards.is_empty()) {
            Box::pin(discards.apply()).await?;
        }
        Ok(stats)
    }
}
