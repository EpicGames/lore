// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::change::FileAction;
use lore_revision::change::NodeChange;
use lore_revision::change::NodeChangeState;
use lore_revision::link::*;
use lore_revision::lore::RepositoryId;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::diff3_collect;
use lore_revision::state::Mark;
use lore_revision::state::NodeMapping;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;

use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::in_memory;
use crate::helper::in_memory::add;
use crate::helper::in_memory::branch_id;
use crate::helper::in_memory::directory;
use crate::helper::in_memory::link_node;
use crate::helper::in_memory::linked_revision;

fn link(link_repository_id: RepositoryId, link_node_id: NodeID) -> LinkContext {
    LinkContext {
        link_repository_id,
        link_node_id,
        parent_repository_id: RepositoryId::from([9; 16]),
        link_state: State::new(),
    }
}

/// A node change marks the links into the linked repository it is in, and
/// nothing when it is in the top-level repository, even for a link whose
/// target has the top-level repository's id.
#[tokio::test]
async fn only_a_node_change_in_a_linked_repository_marks_links() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable_store, mutable_store, _execution) =
                test_store_create().await.expect("making test stores");
            let top_level = RepositoryContext::new_null_context(immutable_store, mutable_store);
            let linked = top_level.to_link_context(RepositoryId::from([1; 16])).await;

            let tracker = LinkTracker::new();
            tracker.add_link(link(top_level.id, 10));
            tracker.add_link(link(linked.id, 20));

            tracker.on_node_changed(&top_level);
            assert!(
                !tracker.has_modifications(),
                "a change in the top-level repository marked a link"
            );

            tracker.on_node_changed(&linked);
            assert_eq!(
                tracker.get_links_needing_rehash(),
                vec![link(linked.id, 20)]
            );
        })
        .await;
}

/// Per-link work past the limit runs at most the limit at a time, and every result comes back in
/// order.
#[tokio::test]
async fn link_work_past_the_limit_runs_the_limit_at_a_time() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    let in_flight = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let count = MAX_CONCURRENT_LINK_TASKS * 2 + 1;
    let work = (0..count)
        .map(|index| {
            let (in_flight, peak) = (&in_flight, &peak);
            async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::task::yield_now().await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, ()>(index)
            }
        })
        .collect();

    let results = try_join_links(work).await.expect("no work fails");

    assert_eq!(results, (0..count).collect::<Vec<_>>());
    assert_eq!(peak.load(Ordering::SeqCst), MAX_CONCURRENT_LINK_TASKS);
}

/// The repository the merged link points at.
fn merged() -> RepositoryId {
    RepositoryId::from([1; 16])
}

/// The branch a row following the parent's branch stores.
fn tracking() -> BranchId {
    BranchId::default()
}

/// A state holding each of `links`: its path, the repository it links, the branch its row
/// stores, and whether the merge staged its delete, which leaves the node without a row. Answers
/// the state and each link's node.
async fn state_linking(
    repository: &Arc<RepositoryContext>,
    links: &[(&str, RepositoryId, BranchId, bool)],
) -> (Arc<State>, Vec<NodeID>) {
    let state = State::new();
    let mut nodes = Vec::with_capacity(links.len());
    for &(path, link_repository, branch, deleted) in links {
        let (directory_name, name) = path.split_once('/').expect("a link below a directory");
        let parent = match state
            .find_node_link(repository.clone(), directory_name)
            .await
        {
            Ok(found) if found.is_valid() => found.node,
            _ => {
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    directory(directory_name),
                    directory_name,
                )
                .await
            }
        };
        let node = link_node(name, link_repository, Hash::from_u64(0x11));
        if deleted {
            let node = state
                .node_add(repository.clone(), parent, node, name)
                .await
                .expect("adding the link node");
            state
                .node_mark(
                    repository,
                    node,
                    Mark::staged(NodeFlags::StagedDelete, true),
                )
                .await
                .expect("staging the delete of the link node");
            nodes.push(node);
            continue;
        }
        let node = add(&state, repository.clone(), parent, node, name).await;
        state
            .link_add(
                repository.clone(),
                link_repository,
                branch,
                Hash::from_u64(0x11),
                node,
                LinkFlags::NoFlags,
            )
            .await
            .expect("registering the link");
        nodes.push(node);
    }
    (state, nodes)
}

/// Where a merge leaving `staged` holds the link to [`merged`] at the node of `staged`'s first
/// link, merged on `branch`, or on the parent's branch where `None`, once the merge renames the
/// node at `moved.0` to `moved.1`, where given.
async fn merged_link_stands(
    staged: &[(&str, RepositoryId, BranchId, bool)],
    branch: Option<BranchId>,
    moved: Option<(&str, &str)>,
) -> Option<String> {
    let (immutable, mutable, _execution) = test_store_create().await.expect("making test stores");
    let repository = in_memory::repository(immutable, mutable);
    let parent_branch = branch_id();
    let (state, nodes) = state_linking(&repository, staged).await;
    if let Some((path, name)) = moved {
        let node = node_at(&repository, &state, path).await;
        let parent = state
            .node(repository.clone(), node)
            .await
            .expect("loading the moved node")
            .parent;
        let permit = state.moves_permit().await.expect("the moves permit");
        state
            .node_move_staged(
                repository.clone(),
                node,
                parent,
                (name, hash_string(name)),
                &permit,
                false,
            )
            .await
            .expect("moving the node");
    }
    merged_link_path(
        &repository,
        &LinkRows::read(&state, repository.clone())
            .await
            .expect("listing the links"),
        nodes[0],
        merged(),
        branch.unwrap_or(parent_branch),
        parent_branch,
    )
    .await
    .expect("looking up the merged link")
    .map(|path| path.as_str().to_string())
}

/// A merged link, tracking or pinned, that the merge leaves at its path stands there, and one the
/// merge renames, or carries along with the directory above it, stands where the move put it.
#[tokio::test]
async fn a_merged_link_stands_where_the_merge_leaves_its_node() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let pinned = branch_id();
            for (moved, path) in [
                (None, "libs/old"),
                (Some(("libs/old", "Old")), "libs/Old"),
                (Some(("libs", "vendor")), "vendor/old"),
            ] {
                for (stored, branch) in [(tracking(), None), (pinned, Some(pinned))] {
                    assert_eq!(
                        merged_link_stands(
                            &[
                                ("libs/old", merged(), stored, false),
                                ("libs/new", merged(), tracking(), false),
                            ],
                            branch,
                            moved,
                        )
                        .await
                        .as_deref(),
                        Some(path),
                        "a merged link on {branch:?} must stand at {path}"
                    );
                }
            }
        })
        .await;
}

/// A merged link whose delete the merge staged, whose row the merge left on another branch,
/// tracking or pinned, or whose node now links another repository, stands nowhere.
#[tokio::test]
async fn a_merged_link_the_merge_removes_or_replaces_stands_nowhere() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let pinned = branch_id();
            for (staged, branch) in [
                (vec![("libs/old", merged(), tracking(), true)], None),
                (vec![("libs/old", merged(), pinned, true)], Some(pinned)),
                (vec![("libs/old", merged(), branch_id(), false)], None),
                (
                    vec![("libs/old", merged(), tracking(), false)],
                    Some(pinned),
                ),
                (
                    vec![("libs/old", RepositoryId::from([2; 16]), tracking(), false)],
                    None,
                ),
            ] {
                assert_eq!(
                    merged_link_stands(&staged, branch, None).await,
                    None,
                    "a merged link on {branch:?} must stand nowhere in {staged:?}"
                );
            }
        })
        .await;
}

/// How the merged branch changes the revision holding a link at `libs/old`.
#[derive(Debug, Clone, Copy)]
enum SourceEdit {
    /// Deletes the link.
    DeleteLink,
    /// Deletes the link and links another repository at its path.
    ReplaceLink,
    /// Deletes `libs`.
    DeleteDirectory,
    /// Moves `libs` to `vendor`.
    MoveDirectory,
    /// Renames `libs` to `Libs`.
    RenameDirectoryInCase,
    /// Renames the link to `libs/Old`.
    RenameLinkInCase,
    /// Adds a file beside the link.
    AddFile,
}

/// The node at `path` in `state`.
async fn node_at(repository: &Arc<RepositoryContext>, state: &State, path: &str) -> NodeID {
    state
        .find_node_link(repository.clone(), path)
        .await
        .expect("resolving the path")
        .node
}

/// Whether the diff of a merge whose merged branch makes `edit` on a base linking a repository at
/// `libs/old`, and whose current branch links it again at `libs/new`, deletes the link at `path`.
///
/// The linked repositories are new to each call: content committed to the same repository and
/// address from two stores at once shares one upload, which the store that didn't take it lacks.
async fn diff_deletes(edit: SourceEdit, path: &'static str) -> bool {
    let (immutable, mutable, _execution) = test_store_create().await.expect("making test stores");
    let repository = in_memory::repository(immutable, mutable);
    let merged = RepositoryId::from(uuid::Uuid::now_v7());
    let other = RepositoryId::from(uuid::Uuid::now_v7());
    let pin = linked_revision(&repository, merged).await;
    let base_state = State::new();
    let libs = add(
        &base_state,
        repository.clone(),
        ROOT_NODE,
        directory("libs"),
        "libs",
    )
    .await;
    add(
        &base_state,
        repository.clone(),
        libs,
        link_node("old", merged, pin),
        "old",
    )
    .await;
    let base = in_memory::commit(&repository, &base_state, Hash::default(), branch_id()).await;

    let source_state = in_memory::revision_state(&repository, base).await;
    let link = node_at(&repository, &source_state, "libs/old").await;
    let libs = node_at(&repository, &source_state, "libs").await;
    match edit {
        SourceEdit::DeleteLink | SourceEdit::ReplaceLink => {
            source_state
                .node_delete(repository.clone(), link)
                .await
                .expect("deleting the link");
            if let SourceEdit::ReplaceLink = edit {
                let other_pin = linked_revision(&repository, other).await;
                add(
                    &source_state,
                    repository.clone(),
                    libs,
                    link_node("old", other, other_pin),
                    "old",
                )
                .await;
            }
        }
        SourceEdit::DeleteDirectory => {
            source_state
                .node_delete(repository.clone(), libs)
                .await
                .expect("deleting the directory");
        }
        SourceEdit::MoveDirectory
        | SourceEdit::RenameDirectoryInCase
        | SourceEdit::RenameLinkInCase => {
            let (node, parent, name) = match edit {
                SourceEdit::MoveDirectory => (libs, ROOT_NODE, "vendor"),
                SourceEdit::RenameDirectoryInCase => (libs, ROOT_NODE, "Libs"),
                _ => (link, libs, "Old"),
            };
            let permit = source_state.moves_permit().await.expect("the moves permit");
            source_state
                .node_move_staged(
                    repository.clone(),
                    node,
                    parent,
                    (name, hash_string(name)),
                    &permit,
                    false,
                )
                .await
                .expect("moving the node");
        }
        SourceEdit::AddFile => {
            add(
                &source_state,
                repository.clone(),
                libs,
                in_memory::file("beside.txt"),
                "beside.txt",
            )
            .await;
        }
    }
    let source = in_memory::commit(&repository, &source_state, base, branch_id()).await;

    let target_state = in_memory::revision_state(&repository, base).await;
    let libs = node_at(&repository, &target_state, "libs").await;
    add(
        &target_state,
        repository.clone(),
        libs,
        link_node("new", merged, pin),
        "new",
    )
    .await;
    let target = in_memory::commit(&repository, &target_state, base, branch_id()).await;

    let mut changes = diff3_collect(repository.clone(), base, source, target, None, true)
        .await
        .expect("diffing the revisions")
        .changes;
    lore_revision::change::sort_by_path(&mut changes);
    merge_deletes_link(&changes, path, merged)
}

/// A link deleted, deleted and replaced at its path by a link to another repository, or below a
/// directory deleted or moved elsewhere is one the merge's diff deletes: a move carries no
/// identity of a link below the moved directory, so the diff deletes it and adds another.
#[tokio::test]
async fn a_link_the_merged_branch_deletes_is_deleted_by_the_diff() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            for edit in [
                SourceEdit::DeleteLink,
                SourceEdit::ReplaceLink,
                SourceEdit::DeleteDirectory,
                SourceEdit::MoveDirectory,
            ] {
                assert!(
                    diff_deletes(edit, "libs/old").await,
                    "the diff must delete the link after {edit:?}"
                );
            }
        })
        .await;
}

/// A link the merged branch renames in case, or that moves with a directory it renames in case,
/// moves; one beside which it adds a file stays; and one the current branch added after branching
/// is no change of the merged branch: the diff deletes none of them.
#[tokio::test]
async fn a_link_the_merged_branch_moves_keeps_or_never_held_is_not_deleted_by_the_diff() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            for (edit, path) in [
                (SourceEdit::RenameDirectoryInCase, "libs/old"),
                (SourceEdit::RenameLinkInCase, "libs/old"),
                (SourceEdit::AddFile, "libs/old"),
                (SourceEdit::AddFile, "libs/new"),
            ] {
                assert!(
                    !diff_deletes(edit, path).await,
                    "the diff must not delete the link at {path} after {edit:?}"
                );
            }
        })
        .await;
}

/// A change taking `action` at `path` from a node of `from` to one of `to`, each addressed in
/// [`merged`], in states holding nothing.
fn change_of(
    repository: &Arc<RepositoryContext>,
    action: FileAction,
    path: &str,
    from: NodeFlags,
    to: NodeFlags,
) -> NodeChange {
    let side = |flags: NodeFlags| NodeChangeState {
        mapping: NodeMapping {
            repository: repository.clone(),
            state: State::new(),
            path: RelativePath::new_from_initial_path(path).expect("the path"),
            node: 1,
        },
        observed: None,
        flags,
        address: Address {
            hash: Hash::default(),
            context: Context::from(merged()),
        },
        mode: 0,
    };
    NodeChange {
        action,
        flags: lore_revision::change::Flags::None,
        from: side(from),
        to: side(to),
    }
}

/// A change replacing a link, or a directory above it, with a file takes the link with it, and
/// one leaving a link to the same repository, or a directory, there does not.
#[tokio::test]
async fn a_link_or_directory_replaced_by_a_file_deletes_the_link() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            for (path, from, to, deleted) in [
                ("libs", NodeFlags::NoFlags, NodeFlags::File, true),
                ("libs", NodeFlags::NoFlags, NodeFlags::NoFlags, false),
                ("libs/old", NodeFlags::Link, NodeFlags::File, true),
                ("libs/old", NodeFlags::Link, NodeFlags::Link, false),
            ] {
                assert_eq!(
                    merge_deletes_link(
                        &[change_of(&repository, FileAction::Keep, path, from, to)],
                        "libs/old",
                        merged(),
                    ),
                    deleted,
                    "a change of {path} to {to:?}"
                );
            }
        })
        .await;
}

/// A delete takes the link with it where it deletes a link to the same repository at its path,
/// or a directory above it, and not where it deletes a node of another kind, as a change of the
/// current branch replacing that node with the link or its directory would.
#[tokio::test]
async fn a_delete_takes_the_link_only_with_its_node_or_a_directory_above_it() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let delete = |path, from| {
                change_of(
                    &repository,
                    FileAction::Delete,
                    path,
                    from,
                    NodeFlags::NoFlags,
                )
            };
            let mut other_link = delete("libs/old", NodeFlags::Link);
            other_link.from.address.context = Context::from(RepositoryId::from([2; 16]));
            let link_added = change_of(
                &repository,
                FileAction::Add,
                "libs/old",
                NodeFlags::NoFlags,
                NodeFlags::Link,
            );
            for (changes, deleted) in [
                (vec![delete("libs/old", NodeFlags::Link)], true),
                (vec![delete("libs", NodeFlags::NoFlags)], true),
                (vec![other_link], false),
                (
                    vec![delete("libs/old", NodeFlags::NoFlags), link_added],
                    false,
                ),
                (vec![delete("libs", NodeFlags::File)], false),
            ] {
                assert_eq!(
                    merge_deletes_link(&changes, "libs/old", merged()),
                    deleted,
                    "the diff {:?}",
                    changes
                        .iter()
                        .map(|change| (change.path().to_string(), change.from.flags))
                        .collect::<Vec<_>>()
                );
            }
        })
        .await;
}

/// A state holding the link `lib` to a new repository, which pins a state of that repository
/// holding `d/f.txt`, staged and dirty as an add where `dirty` says and clean otherwise. Answers
/// the repository, the state and the link.
pub(crate) async fn state_linking_a_tree(
    dirty: bool,
) -> (Arc<RepositoryContext>, Arc<State>, NodeID) {
    let (immutable, mutable, _execution) = test_store_create().await.expect("making test stores");
    let repository = in_memory::repository(immutable, mutable);
    let link_repository = RepositoryId::from(uuid::Uuid::now_v7());
    let linked = repository.to_link_context(link_repository).await;
    let linked_state = State::new();
    let place = async |parent, node, name| {
        if dirty {
            add(&linked_state, linked.clone(), parent, node, name).await
        } else {
            linked_state
                .node_add(linked.clone(), parent, node, name)
                .await
                .expect("adding the node")
        }
    };
    let d = place(ROOT_NODE, directory("d"), "d").await;
    place(d, in_memory::file("f.txt"), "f.txt").await;
    let pin = linked_state
        .serialize(linked, &in_memory::token())
        .await
        .expect("storing the linked state");
    let state = State::new();
    let link = add(
        &state,
        repository.clone(),
        ROOT_NODE,
        link_node("lib", link_repository, pin),
        "lib",
    )
    .await;
    (repository, state, link)
}

/// The dirty paths collected at a link are those of the state it pins, spelled from the link's
/// path, as they are where the collection meets the link below the root.
#[tokio::test]
async fn the_dirty_paths_collected_at_a_link_are_those_of_the_state_it_pins() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (repository, state, link) = state_linking_a_tree(true).await;
            let lib: RelativePath = "lib".parse().expect("a path");

            let at_link = state
                .collect_dirty_paths(repository.clone(), link, lib)
                .await
                .expect("collecting at the link");
            let at_root = state
                .collect_dirty_paths(repository, ROOT_NODE, RelativePath::new())
                .await
                .expect("collecting at the root");

            let spelled = |paths: &[RelativePath]| {
                paths
                    .iter()
                    .map(|path| path.as_str().to_string())
                    .collect::<Vec<_>>()
            };
            assert_eq!(spelled(&at_link), ["lib/d", "lib/d/f.txt"]);
            assert_eq!(spelled(&at_root), spelled(&at_link));
        })
        .await;
}

/// A link has no child in the state holding it, however staged and dirty the tree it pins.
#[tokio::test]
async fn a_link_has_no_child_in_the_state_holding_it() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (repository, state, link) = state_linking_a_tree(true).await;

            for (read, held) in [
                (
                    "staged",
                    state
                        .node_has_staged_children(repository.clone(), link)
                        .await,
                ),
                (
                    "dirty",
                    state
                        .node_has_dirty_children(repository.clone(), link)
                        .await,
                ),
                (
                    "staged or dirty",
                    state
                        .node_has_staged_or_dirty_children(repository.clone(), link)
                        .await,
                ),
            ] {
                assert!(!held.expect("reading the link"), "a {read} child");
            }
        })
        .await;
}
