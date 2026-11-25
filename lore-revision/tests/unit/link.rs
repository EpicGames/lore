// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
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
use lore_revision::state::LinkPin;
use lore_revision::state::LinkReference;
use lore_revision::state::Mark;
use lore_revision::state::NodeMapping;
use lore_revision::state::State;
use lore_revision::state::allow_all_repositories;
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
            let top_level = RepositoryContext::new_null_context(
                immutable_store,
                mutable_store,
                allow_all_repositories(),
            );
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

/// The first error starts no more link work and is returned once the work in flight finishes.
#[tokio::test]
async fn link_work_failing_finishes_the_work_in_flight_and_starts_no_more() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    let (started, finished) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let work = (0..MAX_CONCURRENT_LINK_TASKS * 2)
        .map(|index| {
            let (started, finished) = (&started, &finished);
            async move {
                started.fetch_add(1, Ordering::SeqCst);
                if index == 0 {
                    return Err(index);
                }
                tokio::task::yield_now().await;
                finished.fetch_add(1, Ordering::SeqCst);
                Ok(index)
            }
        })
        .collect();

    assert_eq!(try_join_links(work).await, Err(0));
    assert_eq!(started.load(Ordering::SeqCst), MAX_CONCURRENT_LINK_TASKS);
    assert_eq!(
        finished.load(Ordering::SeqCst),
        MAX_CONCURRENT_LINK_TASKS - 1
    );
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

/// A merge brings in each following link of the merged side at a path where neither the side it
/// merges into nor the base links the same repository, grouped by repository.
#[tokio::test]
async fn a_merge_brings_in_the_following_links_only_the_merged_side_holds() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let other = RepositoryId::from([2; 16]);
            let (target, _) = state_linking(
                &repository,
                &[
                    ("libs/kept", merged(), tracking(), false),
                    ("libs/replaced", merged(), tracking(), false),
                ],
            )
            .await;
            let (base, _) = state_linking(
                &repository,
                &[
                    ("libs/kept", merged(), tracking(), false),
                    ("libs/removed", merged(), tracking(), false),
                ],
            )
            .await;
            let (source, _) = state_linking(
                &repository,
                &[
                    ("libs/kept", merged(), tracking(), false),
                    ("libs/removed", merged(), tracking(), false),
                    ("libs/replaced", other, tracking(), false),
                    ("libs/pinned", merged(), branch_id(), false),
                    ("libs/new", merged(), tracking(), false),
                    ("vendor/new", merged(), tracking(), false),
                ],
            )
            .await;
            let libs = node_at(&repository, &source, "libs").await;
            let vendored = add(
                &source,
                repository.clone(),
                libs,
                link_node("vendored", merged(), Hash::from_u64(0x11)),
                "vendored",
            )
            .await;
            source
                .link_add(
                    repository.clone(),
                    merged(),
                    tracking(),
                    Hash::from_u64(0x11),
                    vendored,
                    LinkFlags::DisableAutoFollow,
                )
                .await
                .expect("registering the link");

            let mut incoming: Vec<(RepositoryId, Vec<String>)> =
                incoming_following_links(&repository, &target, &base, &source)
                    .await
                    .expect("finding the incoming links")
                    .into_iter()
                    .map(|(link_id, mounts)| {
                        let mut paths: Vec<String> =
                            mounts.into_iter().map(|mount| mount.link_path).collect();
                        paths.sort();
                        (link_id, paths)
                    })
                    .collect();
            incoming.sort();

            let mut expected = vec![
                (
                    merged(),
                    vec!["libs/new".to_string(), "vendor/new".to_string()],
                ),
                (other, vec!["libs/replaced".to_string()]),
            ];
            expected.sort();
            assert_eq!(incoming, expected);
        })
        .await;
}

/// A merge keeps each repository the side it merges into links, at the pin it holds there, but at a
/// path where the base links it and the merged side removed it.
#[tokio::test]
async fn a_merge_keeps_the_repositories_linked_but_where_the_merged_side_removes_them() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let (removed, added) = (RepositoryId::from([2; 16]), RepositoryId::from([3; 16]));
            let (target, nodes) = state_linking(
                &repository,
                &[
                    ("libs/kept", merged(), tracking(), false),
                    ("libs/removed", removed, tracking(), false),
                    ("libs/added", added, tracking(), false),
                ],
            )
            .await;
            target
                .link_update(
                    repository.clone(),
                    merged(),
                    tracking(),
                    Hash::from_u64(0x22),
                    nodes[0],
                )
                .await
                .expect("repinning the kept link");
            let (base, _) = state_linking(
                &repository,
                &[
                    ("libs/kept", merged(), tracking(), false),
                    ("libs/removed", removed, tracking(), false),
                ],
            )
            .await;
            let (source, _) =
                state_linking(&repository, &[("libs/kept", merged(), tracking(), false)]).await;

            let kept = repositories_the_merge_keeps(&repository, &target, &base, &source)
                .await
                .expect("finding the repositories the merge keeps");

            assert_eq!(
                kept,
                [
                    (merged(), Hash::from_u64(0x22)),
                    (added, Hash::from_u64(0x11))
                ]
                .into()
            );
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

/// How a side of a merge changes the link at `libs/old` of a base linking it.
#[derive(Debug, Clone, Copy)]
enum LinkEdit {
    /// Adds a file at the root and leaves the link.
    Keep,
    Remove,
    /// Deletes `libs`, the directory holding the link.
    RemoveDirectory,
    /// Removes the link and links its repository at `libs/new`.
    MoveElsewhere,
    /// Pins the link to another revision of its repository.
    Repin,
    /// Has the link's row name another branch, at the same pin.
    ChangeBranch,
    /// Has the link's row stop following its parent's branch.
    ChangeFlags,
}

/// Commits `edit` on a revision of `base`, where `link` and `libs` are the link and the directory
/// holding it.
async fn commit_link_edit(
    repository: &Arc<RepositoryContext>,
    base: Hash,
    (link, libs): (NodeID, NodeID),
    (merged, pin): (RepositoryId, Hash),
    edit: LinkEdit,
) -> Hash {
    let state = in_memory::revision_state(repository, base).await;
    let remove = async |node| {
        state
            .node_delete(repository.clone(), node)
            .await
            .expect("deleting the node");
        state
            .link_remove(repository.clone(), merged, link)
            .await
            .expect("removing the link's row");
    };
    match edit {
        LinkEdit::Keep => {
            add(
                &state,
                repository.clone(),
                ROOT_NODE,
                in_memory::file("beside.txt"),
                "beside.txt",
            )
            .await;
        }
        LinkEdit::Remove => remove(link).await,
        LinkEdit::RemoveDirectory => remove(libs).await,
        LinkEdit::MoveElsewhere => {
            remove(link).await;
            let moved = add(
                &state,
                repository.clone(),
                libs,
                link_node("new", merged, pin),
                "new",
            )
            .await;
            state
                .link_add(
                    repository.clone(),
                    merged,
                    tracking(),
                    pin,
                    moved,
                    LinkFlags::NoFlags,
                )
                .await
                .expect("registering the moved link");
        }
        LinkEdit::Repin => {
            let linked = repository.to_link_context(merged).await;
            let linked_state = in_memory::revision_state(&linked, pin).await;
            add(
                &linked_state,
                linked.clone(),
                ROOT_NODE,
                in_memory::file("m.txt"),
                "m.txt",
            )
            .await;
            let repin = in_memory::commit(&linked, &linked_state, pin, branch_id()).await;
            state
                .node_set_link_target(
                    repository,
                    link,
                    LinkPin::Whole {
                        address: Address {
                            hash: repin,
                            context: Context::from(merged),
                        },
                        child: ROOT_NODE,
                    },
                )
                .await
                .expect("repinning the link");
            state
                .link_update(repository.clone(), merged, tracking(), repin, link)
                .await
                .expect("repinning the link's row");
        }
        LinkEdit::ChangeBranch => {
            state
                .link_update(repository.clone(), merged, branch_id(), pin, link)
                .await
                .expect("naming another branch in the link's row");
        }
        LinkEdit::ChangeFlags => {
            let row = state
                .link_find(repository.clone(), merged, link)
                .await
                .expect("the link's row");
            state
                .link_replace(
                    repository.clone(),
                    LinkReference {
                        flags: LinkFlags::DisableAutoFollow.bits(),
                        ..row
                    },
                )
                .await
                .expect("fixing the link's row");
        }
    }
    in_memory::commit(repository, &state, base, branch_id()).await
}

/// The diff, at `path` or of the whole trees, of a merge whose merged branch makes `source` and
/// whose current branch makes `target` on a base linking a repository at `libs/old`. Answers the
/// diff and the linked repository.
async fn diff_of_link_edits(
    source: LinkEdit,
    target: LinkEdit,
    path: Option<&str>,
) -> (lore_revision::revision::DiffResult, RepositoryId) {
    let (immutable, mutable, _execution) = test_store_create().await.expect("making test stores");
    let repository = in_memory::repository(immutable, mutable);
    let merged = RepositoryId::from(uuid::Uuid::now_v7());
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
    let link = add(
        &base_state,
        repository.clone(),
        libs,
        link_node("old", merged, pin),
        "old",
    )
    .await;
    add(
        &base_state,
        repository.clone(),
        libs,
        in_memory::file("beside.txt"),
        "beside.txt",
    )
    .await;
    base_state
        .link_add(
            repository.clone(),
            merged,
            tracking(),
            pin,
            link,
            LinkFlags::NoFlags,
        )
        .await
        .expect("registering the link");
    let base = in_memory::commit(&repository, &base_state, Hash::default(), branch_id()).await;
    let nodes = (link, libs);
    let source = commit_link_edit(&repository, base, nodes, (merged, pin), source).await;
    let target = commit_link_edit(&repository, base, nodes, (merged, pin), target).await;

    let path = path.map(|path| RelativePath::from_str(path).expect("a valid path"));
    let diff = diff3_collect(repository.clone(), base, source, target, path, true)
        .await
        .expect("diffing the revisions");
    (diff, merged)
}

/// A link one side removes, also with the directory holding it, conflicts at its mount where the
/// other side repinned it or changed its row, whichever side removes it, within the path the diff
/// covers. Otherwise it is removed, and moved where the removing side links its repository
/// elsewhere. Removing the directory still deletes what it holds beside the conflict.
#[tokio::test]
async fn a_link_removed_on_one_side_conflicts_where_the_other_side_changed_it() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            use FileAction::Add;
            use FileAction::Delete;
            use LinkEdit::*;
            for (source, target, path, conflicted, changed) in [
                (Remove, Repin, None, &["libs/old"][..], &[][..]),
                (Repin, Remove, None, &["libs/old"], &[]),
                (Remove, ChangeBranch, None, &["libs/old"], &[]),
                (Remove, ChangeFlags, None, &["libs/old"], &[]),
                (
                    RemoveDirectory,
                    ChangeBranch,
                    None,
                    &["libs/old"],
                    &[(Delete, "libs/beside.txt")],
                ),
                (Remove, Repin, Some("libs"), &["libs/old"], &[]),
                (Remove, Repin, Some("other"), &[], &[]),
                (
                    MoveElsewhere,
                    Repin,
                    None,
                    &[],
                    &[(Add, "libs/new"), (Delete, "libs/old")],
                ),
                (Remove, Remove, None, &[], &[(Delete, "libs/old")]),
                (Remove, Keep, None, &[], &[(Delete, "libs/old")]),
            ] {
                let (diff, merged) = diff_of_link_edits(source, target, path).await;
                let case = format!("{source:?} against {target:?} at {path:?}");
                let paths: Vec<_> = diff
                    .conflicts
                    .iter()
                    .map(|(source, _)| source.path().as_str())
                    .collect();
                assert_eq!(paths, conflicted, "{case}");
                let mut changes = diff.changes;
                lore_revision::change::sort_by_path(&mut changes);
                let under_libs: Vec<_> = changes
                    .iter()
                    .filter(|change| change.path().as_str().starts_with("libs"))
                    .map(|change| (change.action, change.path().as_str()))
                    .collect();
                assert_eq!(under_libs, changed, "{case}");
                if !conflicted.is_empty() {
                    continue;
                }
                assert_eq!(
                    merge_deletes_link(&changes, "libs/old", merged),
                    !changed.is_empty(),
                    "{case}"
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

/// Reversing a conflicted delete the staged state holds adds back the node the from side holds,
/// and adds nothing where the merge staged a node the from side never held, added and deleted at
/// once.
#[tokio::test]
async fn reversing_a_conflicted_delete_adds_back_only_a_node_the_from_side_holds() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            for (held, reversed) in [(true, FileAction::Add), (false, FileAction::Delete)] {
                let mut change = change_of(
                    &repository,
                    FileAction::Delete,
                    "a.txt",
                    NodeFlags::File,
                    NodeFlags::File | NodeFlags::StagedDelete | NodeFlags::StagedMergeConflict,
                );
                change.flags = lore_revision::change::Flags::Conflict;
                if !held {
                    change.from = change.from.invalid(change.from.mapping.path.clone());
                }
                change.reverse();
                assert_eq!(change.action, reversed, "held by the from side: {held}");
            }
        })
        .await;
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
            assert_eq!(spelled(&at_link.paths), ["lib/d", "lib/d/f.txt"]);
            assert_eq!(spelled(&at_root.paths), spelled(&at_link.paths));
            assert_eq!(spelled(&at_root.directories), spelled(&at_link.directories));
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

/// A link at `path` of [`merged`] pinned at `signature` with `flags`.
fn link_at(path: &str, signature: u64, flags: LinkFlags) -> LinkAtPath {
    LinkAtPath {
        path: path.to_string(),
        reference: LinkReference {
            repository: merged(),
            signature: Hash::from_u64(signature),
            local_node: 2,
            flags: flags.bits(),
            ..Default::default()
        },
        mount_source: None,
    }
}

/// A link whose row changed only its flags is touched.
#[test]
fn a_link_with_other_flags_is_touched() {
    let from = [link_at("libs/a", 1, LinkFlags::NoFlags)];
    let to = [link_at("libs/a", 1, LinkFlags::DisableAutoFollow)];
    assert_eq!(
        touched_link_repositories(&from, &to).collect::<Vec<_>>(),
        [merged(), merged()]
    );
}

/// The same links in another order touch nothing, as a link removed and added back moves its row
/// to the end of the registry.
#[test]
fn the_same_links_in_another_order_touch_nothing() {
    let from = [
        link_at("libs/a", 1, LinkFlags::NoFlags),
        link_at("libs/b", 2, LinkFlags::DisableAutoFollow),
    ];
    let to = [
        link_at("libs/b", 2, LinkFlags::DisableAutoFollow),
        link_at("libs/a", 1, LinkFlags::NoFlags),
    ];
    assert_eq!(touched_link_repositories(&from, &to).count(), 0);
}

/// A revision of `holder` linking each of `links`, as (mount, repository, pin), at its root.
async fn revision_linking(
    root: &Arc<RepositoryContext>,
    holder: RepositoryId,
    links: &[(&str, RepositoryId, Hash)],
) -> Hash {
    let context = root.to_link_context(holder).await;
    let state = State::new();
    for &(name, repository, pin) in links {
        let node = add(
            &state,
            context.clone(),
            ROOT_NODE,
            link_node(name, repository, pin),
            name,
        )
        .await;
        state
            .link_add(
                context.clone(),
                repository,
                BranchId::default(),
                pin,
                node,
                LinkFlags::NoFlags,
            )
            .await
            .expect("registering the link");
    }
    in_memory::commit(&context, &state, Hash::default(), branch_id()).await
}

fn first_reach(repository: RepositoryId, pin: Hash, mount_path: &str) -> LinkReach {
    LinkReach {
        repository,
        pin,
        branch: branch_id(),
        mount_path: mount_path.to_string(),
        depth: 1,
        leads: true,
    }
}

/// A walk reaches a repository through a cycle once, reads each distinct pin of it, and never lists
/// the root, even where a linked revision links it back.
#[tokio::test]
async fn a_link_walk_reads_each_pin_once_and_skips_the_root() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable_store, mutable_store, _execution) =
                test_store_create().await.expect("making test stores");
            let root = Arc::new(RepositoryContext::new_null_context(
                immutable_store,
                mutable_store,
                allow_all_repositories(),
            ));
            let (a, b) = (RepositoryId::from([1; 16]), RepositoryId::from([2; 16]));
            let a_unlinked = revision_linking(&root, a, &[]).await;
            let b_pin = revision_linking(&root, b, &[("a", a, a_unlinked)]).await;
            let a_pin = revision_linking(
                &root,
                a,
                &[("b", b, b_pin), ("up", root.id, Hash::from_u64(7))],
            )
            .await;

            let reached = collect_with_context(&root, vec![first_reach(a, a_pin, "a")])
                .await
                .expect("walking the links");

            let listed: Vec<_> = reached.iter().map(|linked| linked.repository).collect();
            assert_eq!(listed, [a, b]);
            let a_paths: Vec<_> = reached[0]
                .reaches
                .iter()
                .map(|r| r.mount_path.as_str())
                .collect();
            assert_eq!(a_paths, ["a", "a/b/a"]);
            let a_pins: Vec<_> = reached[0].pins.iter().map(|walked| walked.pin).collect();
            assert_eq!(a_pins, [a_pin, a_unlinked]);
            assert_eq!(reached[0].pins[0].linked, [b]);
            assert_eq!(reached[1].reaches[0].depth, 2);
        })
        .await;
}

/// A walk reads the revision a link at the depth limit pins, and goes no deeper.
#[tokio::test]
async fn a_link_walk_stops_at_the_depth_limit() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable_store, mutable_store, _execution) =
                test_store_create().await.expect("making test stores");
            let root = Arc::new(RepositoryContext::new_null_context(
                immutable_store,
                mutable_store,
                allow_all_repositories(),
            ));
            let chain: Vec<RepositoryId> = (1..=lore_revision::state::MAX_LINK_DEPTH as u8 + 1)
                .map(|byte| RepositoryId::from([byte; 16]))
                .collect();
            let mut pin = revision_linking(&root, chain[chain.len() - 1], &[]).await;
            for pair in chain.windows(2).rev() {
                pin = revision_linking(&root, pair[0], &[("next", pair[1], pin)]).await;
            }

            let reached = collect_with_context(&root, vec![first_reach(chain[0], pin, "first")])
                .await
                .expect("walking the links");

            let listed: Vec<_> = reached.iter().map(|linked| linked.repository).collect();
            assert_eq!(listed, chain[..lore_revision::state::MAX_LINK_DEPTH]);
            let deepest = reached.last().expect("the deepest repository");
            assert_eq!(deepest.pins.len(), 1, "the deepest pin must be read");
            assert!(deepest.pins[0].linked.is_empty());
        })
        .await;
}

/// A revision of `linked` holding a directory for each of `names`, added in that order.
async fn revision_with_directories(linked: &Arc<RepositoryContext>, names: &[&str]) -> Hash {
    let state = State::new();
    for &name in names {
        add(&state, linked.clone(), ROOT_NODE, directory(name), name).await;
    }
    in_memory::commit(linked, &state, Hash::default(), branch_id()).await
}

/// A State linking `linked` at `a`, exposing `foo`, and at `b`, exposing `bar`, both of the
/// revision `pin`, with the nodes of the two mounts.
async fn mounting_foo_and_bar(
    repository: &Arc<RepositoryContext>,
    linked: &Arc<RepositoryContext>,
    pin: Hash,
) -> (Arc<State>, NodeID, NodeID) {
    let pinned = in_memory::revision_state(linked, pin).await;
    let state = State::new();
    let mut mounts = Vec::new();
    for (name, source) in [("a", "foo"), ("b", "bar")] {
        let child = node_at(linked, &pinned, source).await;
        let node = add(
            &state,
            repository.clone(),
            ROOT_NODE,
            lore_revision::node::Node {
                child,
                ..link_node(name, linked.id, pin)
            },
            name,
        )
        .await;
        state
            .link_add(
                repository.clone(),
                linked.id,
                BranchId::default(),
                pin,
                node,
                LinkFlags::NoFlags,
            )
            .await
            .expect("registering the mount");
        mounts.push(node);
    }
    (state, mounts[0], mounts[1])
}

/// Repinning a repository rewrites the row and the node of every mount: a mount whose folder keeps
/// its node in the new revision keeps it, and one whose node the new revision gives another folder
/// exposes its folder again.
#[tokio::test]
async fn repinning_a_repository_moves_every_mount_to_the_folder_it_exposed() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let linked = repository
                .to_link_context(RepositoryId::from(uuid::Uuid::now_v7()))
                .await;
            let old = revision_with_directories(&linked, &["foo", "bar"]).await;
            let new = revision_with_directories(&linked, &["other", "bar", "foo"]).await;
            let (old_state, new_state) = (
                in_memory::revision_state(&linked, old).await,
                in_memory::revision_state(&linked, new).await,
            );
            let old_foo = node_at(&linked, &old_state, "foo").await;
            let (new_foo, new_bar) = (
                node_at(&linked, &new_state, "foo").await,
                node_at(&linked, &new_state, "bar").await,
            );
            assert_eq!(node_at(&linked, &new_state, "other").await, old_foo);
            assert_eq!(node_at(&linked, &old_state, "bar").await, new_bar);
            let (state, a, b) = mounting_foo_and_bar(&repository, &linked, old).await;

            let moved = repin_repository(&state, repository.clone(), linked.id, new, branch_id())
                .await
                .expect("repinning the repository");

            assert_eq!(
                moved
                    .iter()
                    .map(|mount| (mount.link_node, mount.child))
                    .collect::<Vec<_>>(),
                [(a, new_foo), (b, new_bar)]
            );
            for (mount, child) in [(a, new_foo), (b, new_bar)] {
                let node = state
                    .node(repository.clone(), mount)
                    .await
                    .expect("reading the mount");
                assert_eq!((node.address.hash, node.child), (new, child));
            }
            let rows = state
                .link_list(repository.clone())
                .await
                .expect("reading the registry");
            assert!(rows.iter().all(|row| row.signature() == new));
        })
        .await;
}

/// Repinning a repository at a revision lacking the folder a mount exposes fails as an invalid
/// path.
#[tokio::test]
async fn repinning_a_repository_refuses_a_revision_lacking_a_mounted_folder() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let linked = repository
                .to_link_context(RepositoryId::from(uuid::Uuid::now_v7()))
                .await;
            let old = revision_with_directories(&linked, &["foo", "bar"]).await;
            let new = revision_with_directories(&linked, &["foo"]).await;
            let (state, _, _) = mounting_foo_and_bar(&repository, &linked, old).await;

            let refused = repin_repository(&state, repository, linked.id, new, branch_id())
                .await
                .err()
                .expect("bar is gone from the new revision");

            assert!(refused.is_invalid_path(), "{refused:?}");
        })
        .await;
}

/// Repinning a repository whose mounts carry different flags fails before any mount moves.
#[tokio::test]
async fn repinning_a_repository_refuses_mounts_with_different_flags() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let linked = repository
                .to_link_context(RepositoryId::from(uuid::Uuid::now_v7()))
                .await;
            let old = revision_with_directories(&linked, &["foo", "bar"]).await;
            let new = revision_with_directories(&linked, &["foo", "bar", "baz"]).await;
            let (state, a, b) = mounting_foo_and_bar(&repository, &linked, old).await;
            let row = state
                .link_find(repository.clone(), linked.id, b)
                .await
                .expect("finding the mount's row");
            state
                .link_replace(
                    repository.clone(),
                    LinkReference {
                        flags: LinkFlags::DisableAutoFollow.bits(),
                        ..row
                    },
                )
                .await
                .expect("changing one mount's flags");

            let refused = repin_repository(&state, repository.clone(), linked.id, new, branch_id())
                .await
                .err()
                .expect("the mounts carry different flags");

            assert!(refused.is_link_pins_diverge(), "{refused:?}");
            for mount in [a, b] {
                let node = state
                    .node(repository.clone(), mount)
                    .await
                    .expect("reading the mount");
                assert_eq!(node.address.hash, old);
            }
        })
        .await;
}

/// A revision of `linked` staged on `pin`, which no commit holds, with a directory `name` added.
async fn staged_on(linked: &Arc<RepositoryContext>, pin: Hash, name: &str) -> Hash {
    let staged = in_memory::revision_state(linked, pin).await;
    staged.reparent_onto(pin);
    add(&staged, linked.clone(), ROOT_NODE, directory(name), name).await;
    staged
        .serialize(linked.clone(), &in_memory::token())
        .await
        .expect("serializing the staged revision")
}

/// The mounts of one repository in one parent stage into one state, and a mount of it in another
/// parent into another.
#[tokio::test]
async fn tracking_shares_one_linked_state_per_parent_and_repository() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let linked = repository
                .to_link_context(RepositoryId::from(uuid::Uuid::now_v7()))
                .await;
            let other_parent = repository
                .to_link_context(RepositoryId::from(uuid::Uuid::now_v7()))
                .await;
            let pin = revision_with_directories(&linked, &["foo", "bar"]).await;
            let (state, a, b) = mounting_foo_and_bar(&repository, &linked, pin).await;
            let (other_state, other_a, _) = mounting_foo_and_bar(&other_parent, &linked, pin).await;
            let tracker = LinkTracker::new();

            let mut tracked = Vec::new();
            for (parent, mount) in [
                ((&repository, &state), a),
                ((&repository, &state), b),
                ((&other_parent, &other_state), other_a),
            ] {
                tracked.push(
                    tracker
                        .track(parent, mount, linked.id, pin)
                        .await
                        .expect("tracking the mount"),
                );
            }

            assert!(Arc::ptr_eq(&tracked[0], &tracked[1]));
            assert!(!Arc::ptr_eq(&tracked[0], &tracked[2]));
            assert_eq!(tracker.all_links().len(), 3);
        })
        .await;
}

/// A mount at the committed pin, tracked first, stages into the uncommitted revision another
/// mount of its repository pins.
#[tokio::test]
async fn tracking_a_mount_at_the_committed_pin_shares_the_uncommitted_revision() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let linked = repository
                .to_link_context(RepositoryId::from(uuid::Uuid::now_v7()))
                .await;
            let pin = revision_with_directories(&linked, &["foo", "bar"]).await;
            let staged = staged_on(&linked, pin, "baz").await;
            let (state, a, b) = mounting_foo_and_bar(&repository, &linked, pin).await;
            state
                .link_update(
                    repository.clone(),
                    linked.id,
                    BranchId::default(),
                    staged,
                    a,
                )
                .await
                .expect("pinning one mount at the staged revision");

            let tracked = LinkTracker::new()
                .track((&repository, &state), b, linked.id, pin)
                .await
                .expect("tracking the mount at the committed pin");

            assert_eq!(tracked.revision(), staged);
        })
        .await;
}

/// A change to a shared state moves the mounts holding it and those pinning the uncommitted
/// revision it was opened at, each keeping the node it exposes, and leaves a mount at the
/// committed pin there.
#[tokio::test]
async fn repinning_staged_mounts_moves_the_mounts_sharing_the_change() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let linked = repository
                .to_link_context(RepositoryId::from(uuid::Uuid::now_v7()))
                .await;
            let pin = revision_with_directories(&linked, &["foo", "bar"]).await;
            let staged = staged_on(&linked, pin, "baz").await;
            let restaged = staged_on(&linked, pin, "qux").await;
            let (state, a, b) = mounting_foo_and_bar(&repository, &linked, pin).await;
            state
                .link_update(
                    repository.clone(),
                    linked.id,
                    BranchId::default(),
                    staged,
                    a,
                )
                .await
                .expect("pinning one mount at the staged revision");
            let opened = in_memory::revision_state(&linked, staged).await;
            let children = [a, b].map(async |mount| {
                state
                    .node(repository.clone(), mount)
                    .await
                    .expect("reading the mount")
                    .child
            });
            let children = futures::future::join_all(children).await;

            let moved = repin_staged_mounts(
                &state,
                &repository,
                linked.id,
                BranchId::default(),
                restaged,
                sharing_mounts(&opened, &[]),
            )
            .await
            .expect("repinning the mounts sharing the state");
            assert_eq!(moved, [a]);
            let held = repin_staged_mounts(
                &state,
                &repository,
                linked.id,
                BranchId::default(),
                restaged,
                sharing_mounts(&opened, &[b]),
            )
            .await
            .expect("repinning the mount holding the change");
            assert_eq!(held, [b]);

            for (mount, child) in [a, b].into_iter().zip(children) {
                let node = state
                    .node(repository.clone(), mount)
                    .await
                    .expect("reading the mount");
                assert_eq!((node.address.hash, node.child), (restaged, child));
            }
        })
        .await;
}

/// Mounts pinning two uncommitted revisions, as staging wrote before every mount shared one, each
/// stage into the state of their own revision, which every task tracking that mount shares.
#[tokio::test]
async fn tracking_mounts_on_unrelated_revisions_keeps_a_state_for_each() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let linked = repository
                .to_link_context(RepositoryId::from(uuid::Uuid::now_v7()))
                .await;
            let pin = revision_with_directories(&linked, &["foo", "bar"]).await;
            let (state, a, b) = mounting_foo_and_bar(&repository, &linked, pin).await;
            let mut staged = Vec::new();
            for (mount, name) in [(a, "baz"), (b, "qux")] {
                let revision = staged_on(&linked, pin, name).await;
                state
                    .link_update(
                        repository.clone(),
                        linked.id,
                        BranchId::default(),
                        revision,
                        mount,
                    )
                    .await
                    .expect("pinning the mount at its staged revision");
                staged.push(revision);
            }
            let tracker = LinkTracker::new();

            let mut tracked = Vec::new();
            for (mount, revision) in [(a, staged[0]), (b, staged[1]), (b, staged[1])] {
                tracked.push(
                    tracker
                        .track((&repository, &state), mount, linked.id, revision)
                        .await
                        .expect("tracking the mount"),
                );
            }

            assert_eq!(
                tracked
                    .iter()
                    .map(|state| state.revision())
                    .collect::<Vec<_>>(),
                [staged[0], staged[1], staged[1]]
            );
            assert!(Arc::ptr_eq(&tracked[1], &tracked[2]));
        })
        .await;
}

/// A State of `repository` mounting `linked` at `a`, exposing `foo`, staged at a revision lacking
/// `bar`, and at `b`, exposing `bar`, pinned at the revision before, as mounts that disagree from
/// before every mount shared one pin can hold.
async fn mounting_a_revision_lacking_a_mounted_folder(
    repository: &Arc<RepositoryContext>,
    linked: &Arc<RepositoryContext>,
) -> Arc<State> {
    let old = revision_with_directories(linked, &["foo", "bar"]).await;
    let new = revision_with_directories(linked, &["foo"]).await;
    let state = State::new();
    for (name, source, pin, staged) in [("a", "foo", new, true), ("b", "bar", old, false)] {
        let pinned = in_memory::revision_state(linked, pin).await;
        let node = lore_revision::node::Node {
            child: node_at(linked, &pinned, source).await,
            ..link_node(name, linked.id, pin)
        };
        let node = if staged {
            add(&state, repository.clone(), ROOT_NODE, node, name).await
        } else {
            state
                .node_add(repository.clone(), ROOT_NODE, node, name)
                .await
                .expect("adding the mount")
        };
        state
            .link_add(
                repository.clone(),
                linked.id,
                BranchId::default(),
                pin,
                node,
                LinkFlags::NoFlags,
            )
            .await
            .expect("registering the mount");
    }
    state
}

/// A commit through a mount staged at a revision lacking the folder another mount exposes is
/// refused before anything is committed, whether the parent mounts the repository or a repository
/// the parent links does.
#[tokio::test]
async fn a_commit_refuses_a_staged_revision_lacking_a_folder_another_mount_exposes() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let linked = repository
                .to_link_context(RepositoryId::from(uuid::Uuid::now_v7()))
                .await;
            let top_level =
                mounting_a_revision_lacking_a_mounted_folder(&repository, &linked).await;

            let holder = repository
                .to_link_context(RepositoryId::from(uuid::Uuid::now_v7()))
                .await;
            let holder_state = mounting_a_revision_lacking_a_mounted_folder(&holder, &linked).await;
            let staged = holder_state
                .serialize(holder.clone(), &in_memory::token())
                .await
                .expect("storing the holder's staged state");
            let nested = State::new();
            let node = add(
                &nested,
                repository.clone(),
                ROOT_NODE,
                link_node("holder", holder.id, staged),
                "holder",
            )
            .await;
            nested
                .link_add(
                    repository.clone(),
                    holder.id,
                    BranchId::default(),
                    staged,
                    node,
                    LinkFlags::NoFlags,
                )
                .await
                .expect("registering the holder");

            for state in [top_level, nested] {
                let refused =
                    lore_revision::commit::reject_unmovable_link_mounts(&repository, &state, None)
                        .await
                        .expect_err("bar is gone from the staged revision");
                assert!(refused.is_invalid_arguments(), "{refused:?}");
            }
        })
        .await;
}

/// Mounts a link to `link_id` at `<directory_name>/lib` in `state` and registers it.
async fn mount_registered_link(
    state: &State,
    repository: &Arc<RepositoryContext>,
    directory_name: &str,
    link_id: RepositoryId,
) -> NodeID {
    let parent = add(
        state,
        repository.clone(),
        ROOT_NODE,
        directory(directory_name),
        directory_name,
    )
    .await;
    let node = add(
        state,
        repository.clone(),
        parent,
        link_node("lib", link_id, Hash::from_u64(0x11)),
        "lib",
    )
    .await;
    state
        .link_add(
            repository.clone(),
            link_id,
            tracking(),
            Hash::from_u64(0x11),
            node,
            LinkFlags::NoFlags,
        )
        .await
        .expect("registering the link");
    node
}

/// A link staged for add and one staged for delete both count as mounts a path overlaps, the
/// latter staged as `lore link remove` stages it: its node staged for delete and its row gone
/// from the staged registry, so only the current registry still lists it.
#[tokio::test]
async fn overlapping_link_mount_reports_staged_links() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let removed_id = RepositoryId::from([2; 16]);

            // Both states build `gone` alike, so its link node has one ID in each.
            let state_current = State::new();
            mount_registered_link(&state_current, &repository, "gone", removed_id).await;
            let state_staged = State::new();
            let removed =
                mount_registered_link(&state_staged, &repository, "gone", removed_id).await;
            state_staged
                .node_mark(
                    &repository,
                    removed,
                    Mark::staged(NodeFlags::StagedDelete, true),
                )
                .await
                .expect("staging the delete of the link node");
            state_staged
                .link_remove(repository.clone(), removed_id, removed)
                .await
                .expect("removing the link from the staged registry");
            mount_registered_link(&state_staged, &repository, "vendor", merged()).await;

            for (path, expected) in [
                ("vendor", Some("vendor/lib")),
                ("gone/lib/x", Some("gone/lib")),
                ("gone", Some("gone/lib")),
                ("other", None),
            ] {
                let path = RelativePath::new_from_initial_path(path).expect("the path");
                let found = overlapping_link_mount(
                    &state_current,
                    &state_staged,
                    repository.clone(),
                    &path,
                )
                .await
                .expect("listing the link mounts");
                assert_eq!(
                    found.as_ref().map(RelativePath::as_str),
                    expected,
                    "the link mount overlapping {path}"
                );
            }
        })
        .await;
}
