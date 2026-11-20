// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The 3-way diff of a folder `shared` that each side replaced with a link, or changed below.

use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::change::NodeChange;
use lore_revision::lore::RepositoryId;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::DiffItem;
use lore_revision::revision::LinkSettlement;
use lore_revision::revision::MergesAcross;
use lore_revision::revision::diff3_with_source_cap;
use lore_revision::state::State;
use lore_revision::util::collect_stream::collect_stream_with_summary;
use lore_storage::hash::hash_string;

use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::in_memory;
use crate::helper::in_memory::add;
use crate::helper::in_memory::branch_id;
use crate::helper::in_memory::directory;
use crate::helper::in_memory::file;
use crate::helper::in_memory::link_node;
use crate::helper::in_memory::linked_revision;

/// How one side changes the base.
#[derive(Debug, Clone, Copy)]
enum Side {
    /// Puts a link to the repository at the pin at `shared`, in place of the folder where the
    /// base holds it.
    Link(RepositoryId, Hash),
    /// Deletes `shared/a.txt`.
    DeleteA,
    /// Moves `shared/a.txt` to `a.txt`, then replaces `shared` with a link to the repository at
    /// the pin.
    MoveOutAndLink(RepositoryId, Hash),
}

async fn node_at(repository: &Arc<RepositoryContext>, state: &State, path: &str) -> NodeID {
    state
        .find_node_link(repository.clone(), path)
        .await
        .expect("resolving the path")
        .node
}

async fn link_shared(
    repository: &Arc<RepositoryContext>,
    state: &State,
    linked: RepositoryId,
    pin: Hash,
) {
    if let Ok(folder) = state.find_node_link(repository.clone(), "shared").await
        && folder.is_valid()
    {
        state
            .node_delete(repository.clone(), folder.node)
            .await
            .expect("deleting the folder");
    }
    add(
        state,
        repository.clone(),
        ROOT_NODE,
        link_node("shared", linked, pin),
        "shared",
    )
    .await;
}

async fn commit_side(repository: &Arc<RepositoryContext>, base: Hash, side: Side) -> Hash {
    let state = in_memory::revision_state(repository, base).await;
    match side {
        Side::Link(linked, pin) => link_shared(repository, &state, linked, pin).await,
        Side::DeleteA => {
            let a = node_at(repository, &state, "shared/a.txt").await;
            state
                .node_delete(repository.clone(), a)
                .await
                .expect("deleting the file");
        }
        Side::MoveOutAndLink(linked, pin) => {
            let a = node_at(repository, &state, "shared/a.txt").await;
            let permit = state.moves_permit().await.expect("the moves permit");
            state
                .node_move_staged(
                    repository.clone(),
                    a,
                    ROOT_NODE,
                    ("a.txt", hash_string("a.txt")),
                    &permit,
                    false,
                )
                .await
                .expect("moving the file");
            drop(permit);
            link_shared(repository, &state, linked, pin).await;
        }
    }
    in_memory::commit(repository, &state, base, branch_id()).await
}

/// The changes and the conflicts of a 3-way diff, each as its action and path.
type Summary = (Vec<String>, Vec<String>);

/// The 3-way diff, which includes the changes both sides make alike and leaves two links to
/// `settlement`, of the sides `sides` gives, reading the linked repositories it makes, on a base
/// holding `shared/a.txt` and `shared/b.txt` where `folder` says and nothing at `shared` otherwise.
async fn diff(
    folder: bool,
    settlement: LinkSettlement,
    sides: impl AsyncFnOnce(&Arc<RepositoryContext>) -> (Side, Side),
) -> Summary {
    let (immutable, mutable, _execution) = test_store_create().await.expect("making test stores");
    let repository = in_memory::repository(immutable, mutable);
    let base_state = State::new();
    if folder {
        let shared = add(
            &base_state,
            repository.clone(),
            ROOT_NODE,
            directory("shared"),
            "shared",
        )
        .await;
        for name in ["a.txt", "b.txt"] {
            add(&base_state, repository.clone(), shared, file(name), name).await;
        }
    } else {
        add(
            &base_state,
            repository.clone(),
            ROOT_NODE,
            file("root.txt"),
            "root.txt",
        )
        .await;
    }
    let base = in_memory::commit(&repository, &base_state, Hash::default(), branch_id()).await;
    let (source, target) = sides(&repository).await;
    let source = commit_side(&repository, base, source).await;
    let target = commit_side(&repository, base, target).await;
    let (_, items) = collect_stream_with_summary(|tx| {
        diff3_with_source_cap(
            repository.clone(),
            base,
            source,
            target,
            None,
            true,
            None,
            None,
            None,
            MergesAcross::Unknown,
            settlement,
            tx,
        )
    })
    .await
    .expect("diffing the revisions");
    let (mut changes, mut conflicts) = (Vec::new(), Vec::new());
    for item in items {
        match item {
            DiffItem::Change(change) => changes.push(describe(&change)),
            DiffItem::Conflict(pair) => {
                let (source, target) = *pair;
                conflicts.push(format!("{} | {}", describe(&source), describe(&target)));
            }
        }
    }
    changes.sort();
    (changes, conflicts)
}

fn describe(change: &NodeChange) -> String {
    format!("{:?} {:?} {}", change.action, change.flags, change.path())
}

fn new_repository() -> RepositoryId {
    RepositoryId::from(uuid::Uuid::now_v7())
}

/// The 3-way diff [`diff`] makes of links to one repository put at `shared` on both sides, at two
/// pins where `other_pin` says.
async fn same_repository(folder: bool, settlement: LinkSettlement, other_pin: bool) -> Summary {
    diff(folder, settlement, async |repository| {
        let linked = new_repository();
        let pin = linked_revision(repository, linked).await;
        let target_pin = if other_pin {
            linked_revision(repository, linked).await
        } else {
            pin
        };
        (Side::Link(linked, pin), Side::Link(linked, target_pin))
    })
    .await
}

/// Both sides putting a link to one repository in place of a folder, or where nothing stood,
/// diff with no change and no conflict in the tree where the links' rows settle them, at one pin
/// or at two.
#[tokio::test]
async fn the_same_repository_linked_on_both_sides_where_no_link_stood_leaves_the_rows_to_settle() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            for folder in [true, false] {
                for other_pin in [false, true] {
                    assert_eq!(
                        same_repository(folder, LinkSettlement::Rows, other_pin).await,
                        (vec![], vec![]),
                        "folder {folder}, other pin {other_pin}"
                    );
                }
            }
        })
        .await;
}

/// The conflict a 3-way diff reports for the mount `shared`.
const MOUNT_CONFLICT: &str =
    "Keep Flags(Merge | Conflict) shared | Keep Flags(Merge | Conflict) shared";

/// Where the tree settles two links, as for a push, a cherry-pick or a revert, links to one
/// repository at two pins conflict at the mount where they replaced a folder, and at the link
/// where nothing stood.
#[tokio::test]
async fn the_same_repository_linked_at_two_pins_conflicts_where_the_tree_settles_the_links() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            assert_eq!(
                same_repository(true, LinkSettlement::Tree, true).await,
                (vec![], vec![MOUNT_CONFLICT.to_string()]),
            );
            assert_eq!(
                same_repository(false, LinkSettlement::Tree, true).await,
                (
                    vec![],
                    vec![
                        "Add Flags(Merge | Conflict) shared | Add Flags(Merge | Conflict) shared"
                            .to_string()
                    ]
                ),
            );
        })
        .await;
}

/// A folder replaced with links to two repositories, with a link on one side and changed below on
/// the other, or with a link to one repository on each side where one moved a file out of the
/// folder first, conflicts at the mount, whichever side made which change, and leaves no change.
/// The move out conflicts where it put the file too, where the source holds the link alone.
#[tokio::test]
async fn a_folder_replaced_otherwise_on_each_side_conflicts_at_the_mount() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            type Sides = fn(RepositoryId, RepositoryId, Hash, Hash) -> (Side, Side);
            let moved_out =
                "Delete Flags(Merge | Conflict) a.txt | Move Flags(Merge | Conflict) a.txt";
            // Each case with the conflicts besides the mount's, as given and swapped.
            let cases: [(&str, Sides, [&[&str]; 2]); 3] = [
                (
                    "two repositories",
                    |first, second, first_pin, second_pin| {
                        (Side::Link(first, first_pin), Side::Link(second, second_pin))
                    },
                    [&[], &[]],
                ),
                (
                    "a delete below",
                    |first, _, pin, _| (Side::Link(first, pin), Side::DeleteA),
                    [&[], &[]],
                ),
                (
                    "a file moved out",
                    |first, _, pin, _| (Side::Link(first, pin), Side::MoveOutAndLink(first, pin)),
                    [&[moved_out], &[]],
                ),
            ];
            for (case, sides, besides) in cases {
                for (swapped, besides) in [false, true].into_iter().zip(besides) {
                    let summary = diff(true, LinkSettlement::Rows, async |repository| {
                        let (first, second) = (new_repository(), new_repository());
                        let first_pin = linked_revision(repository, first).await;
                        let second_pin = linked_revision(repository, second).await;
                        let (source, target) = sides(first, second, first_pin, second_pin);
                        if swapped {
                            (target, source)
                        } else {
                            (source, target)
                        }
                    })
                    .await;
                    let mut conflicts: Vec<String> = besides
                        .iter()
                        .map(|conflict| conflict.to_string())
                        .collect();
                    conflicts.push(MOUNT_CONFLICT.to_string());
                    assert_eq!(summary, (vec![], conflicts), "{case}, swapped {swapped}");
                }
            }
        })
        .await;
}
