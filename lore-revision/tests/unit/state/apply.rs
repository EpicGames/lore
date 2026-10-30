// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::BTreeMap;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::diff3_collect;
use lore_revision::state::State;
use lore_revision::state::apply_tree_changes;
use lore_storage::hash::hash_string;

use crate::helper::in_memory;
use crate::helper::in_memory::add;
use crate::helper::in_memory::branch_id;
use crate::helper::in_memory::commit;
use crate::helper::in_memory::directory;
use crate::helper::in_memory::file;
use crate::helper::in_memory::revision_state;
use crate::helper::test_store_create;

const KELVIN: &str = "\u{212A}.txt";

/// The repository the link at `Mount` points into.
const MOUNT_REPOSITORY: uuid::Uuid =
    uuid::Uuid::from_u128(0x0192_6f3c_5d10_7a2b_8c4e_1f2a_3b4c_5d6e);

/// The address hash, size and mode of a node, and for a link its address context and mount
/// source: the repository and the node it points into.
type Content = (Hash, u64, u16, Option<(Context, NodeID)>);

/// A file holding content `version`, which its address hash and its size both name.
fn file_at(name: &str, version: u64) -> Node {
    let mut node = file(name);
    node.address.hash = Hash::from_u64(version);
    node.size = version;
    node
}

/// What [`file_at`] holds, with `mode`.
fn content(version: u64, mode: u16) -> Content {
    (Hash::from_u64(version), version, mode, None)
}

/// An edit a revision makes on the base.
enum Edit {
    /// Moves the node at the first path to the second.
    Move(&'static str, &'static str),
    /// Gives the file at the path the content of the version.
    Write(&'static str, u64),
    /// Gives the file at the path the mode, at version 1.
    Chmod(&'static str, u16),
}

/// The node at `path` in `state`.
async fn node_at(repository: &Arc<RepositoryContext>, state: &State, path: &str) -> NodeID {
    state
        .find_node_link(repository.clone(), path)
        .await
        .expect("resolving the path")
        .node
}

/// A revision holding `File.txt`, `Other.txt`, a file named with the Kelvin sign, `Dir/a.txt` and
/// `Dir/b.txt`, each at version 1, and a link at `Mount`.
async fn base(repository: &Arc<RepositoryContext>) -> Hash {
    let state = State::new();
    for name in ["File.txt", "Other.txt", KELVIN] {
        add(
            &state,
            repository.clone(),
            ROOT_NODE,
            file_at(name, 1),
            name,
        )
        .await;
    }
    let parent = add(
        &state,
        repository.clone(),
        ROOT_NODE,
        directory("Dir"),
        "Dir",
    )
    .await;
    for name in ["a.txt", "b.txt"] {
        add(&state, repository.clone(), parent, file_at(name, 1), name).await;
    }
    let link = Node {
        flags: NodeFlags::Link.bits(),
        mode: 0o755,
        name_hash: hash_string("Mount"),
        child: 7,
        address: Address {
            hash: Hash::from_u64(0xabcd),
            context: Context::from(MOUNT_REPOSITORY),
        },
        ..Default::default()
    };
    add(&state, repository.clone(), ROOT_NODE, link, "Mount").await;
    commit(repository, &state, Hash::default(), branch_id()).await
}

/// The revision making `edits` on `base`.
async fn edited(repository: &Arc<RepositoryContext>, base: Hash, edits: &[Edit]) -> Hash {
    let state = revision_state(repository, base).await;
    for edit in edits {
        let (path, mode, version) = match *edit {
            Edit::Move(from, to) => {
                let (parent, name) = to.rsplit_once('/').unwrap_or(("", to));
                let parent = node_at(repository, &state, parent).await;
                let node = node_at(repository, &state, from).await;
                state
                    .move_node(repository.clone(), node, parent, name)
                    .await
                    .expect("moving the node");
                continue;
            }
            Edit::Write(path, version) => (path, 0o644, version),
            Edit::Chmod(path, mode) => (path, mode, 1),
        };
        let node = node_at(repository, &state, path).await;
        state
            .node_modify(
                repository.clone(),
                node,
                mode,
                version,
                Address::zero_context_hash(Hash::from_u64(version)),
            )
            .await
            .expect("modifying the file");
        state
            .node_mark_staged(
                repository.clone(),
                node,
                NodeFlags::StagedModify,
                NodeFlags::DirtyModify,
            )
            .await
            .expect("staging the modification");
    }
    commit(repository, &state, base, branch_id()).await
}

/// The content of every node in `state` not staged for deletion, by path. A node staged as a
/// move fails the test: the state is a stored revision, which records no move of its own.
async fn contents(repository: &Arc<RepositoryContext>, state: &State) -> BTreeMap<String, Content> {
    let mut contents = BTreeMap::new();
    let mut pending = vec![(ROOT_NODE, String::new())];
    while let Some((parent, prefix)) = pending.pop() {
        for child in state
            .node_children(repository.clone(), parent)
            .await
            .expect("listing the children")
        {
            let node = state
                .node(repository.clone(), child)
                .await
                .expect("reading a child");
            if node.is_staged_delete() {
                continue;
            }
            let name = state
                .node_name_clone(repository.clone(), child)
                .await
                .expect("reading a name");
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            assert!(!node.is_staged_move(), "{path} is stored staged as a move");
            if node.is_directory() {
                pending.push((child, path.clone()));
            }
            let link = node
                .mount_source()
                .map(|source| (node.address.context, source));
            contents.insert(path, (node.address.hash, node.size, node.mode, link));
        }
    }
    contents
}

/// Applies the changes `incoming` makes on the base to the head making `head` on it, as a
/// fast-forward merge does, stores the result, and answers the stored revision's contents.
async fn merge(
    repository: &Arc<RepositoryContext>,
    incoming: &[Edit],
    head: &[Edit],
) -> BTreeMap<String, Content> {
    let base = base(repository).await;
    let incoming = edited(repository, base, incoming).await;
    let head = edited(repository, base, head).await;

    let diff = diff3_collect(repository.clone(), base, incoming, head, None, false)
        .await
        .expect("diffing the revisions");
    assert!(diff.conflicts.is_empty(), "{:?}", diff.conflicts);
    let merged = revision_state(repository, head).await;
    apply_tree_changes(repository.clone(), merged.clone(), &diff.changes)
        .await
        .expect("applying the changes");
    let stored = merged
        .serialize(repository.clone(), &in_memory::token())
        .await
        .expect("storing the merged revision");
    let stored = revision_state(repository, stored).await;
    contents(repository, &stored).await
}

/// Runs `test` against a repository of its own.
async fn run<F, Fut>(test: F)
where
    F: FnOnce(Arc<RepositoryContext>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        test(in_memory::repository(immutable, mutable)).await;
    }))
    .await
    .expect("Task failed");
}

/// A file renamed only in case takes its new name and the content the rename comes with.
#[tokio::test]
async fn a_case_only_move_of_a_file_takes_its_new_name_and_content() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[
                Edit::Move("File.txt", "file.txt"),
                Edit::Write("file.txt", 2),
            ],
            &[Edit::Write("Other.txt", 3)],
        )
        .await;

        assert_eq!(merged.get("file.txt"), Some(&content(2, 0o644)));
        assert_eq!(merged.get("Other.txt"), Some(&content(3, 0o644)));
        assert!(!merged.contains_key("File.txt"), "{merged:?}");
    })
    .await;
}

/// A directory renamed only in case takes its new name and keeps every child, the ones the
/// rename leaves unchanged included.
#[tokio::test]
async fn a_case_only_move_of_a_directory_keeps_its_children() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[Edit::Move("Dir", "dir"), Edit::Write("dir/a.txt", 2)],
            &[Edit::Write("Other.txt", 3)],
        )
        .await;

        assert!(merged.contains_key("dir"), "{merged:?}");
        assert_eq!(merged.get("dir/a.txt"), Some(&content(2, 0o644)));
        assert_eq!(merged.get("dir/b.txt"), Some(&content(1, 0o644)));
        assert!(!merged.contains_key("Dir"), "{merged:?}");
    })
    .await;
}

/// A rename that only changes case keeps the content the head gave the file meanwhile, which the
/// 3-way diff folds into the rename.
#[tokio::test]
async fn a_case_only_rename_keeps_the_content_the_head_gave_the_file() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[
                Edit::Move("File.txt", "file.txt"),
                Edit::Write("Other.txt", 2),
            ],
            &[Edit::Write("File.txt", 3)],
        )
        .await;

        assert_eq!(merged.get("file.txt"), Some(&content(3, 0o644)));
        assert_eq!(merged.get("Other.txt"), Some(&content(2, 0o644)));
        assert!(!merged.contains_key("File.txt"), "{merged:?}");
    })
    .await;
}

/// A file renamed only in case takes the mode the rename comes with.
#[tokio::test]
async fn a_case_only_move_takes_the_mode_it_comes_with() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[
                Edit::Move("File.txt", "file.txt"),
                Edit::Chmod("file.txt", 0o755),
                Edit::Write("Dir/a.txt", 2),
            ],
            &[Edit::Write("Other.txt", 3)],
        )
        .await;

        assert_eq!(merged.get("file.txt"), Some(&content(1, 0o755)));
        assert!(!merged.contains_key("File.txt"), "{merged:?}");
    })
    .await;
}

/// A rename to the name the Kelvin sign folds to is a rename that only changes case.
#[tokio::test]
async fn a_case_only_move_folds_beyond_ascii() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[Edit::Move(KELVIN, "k.txt"), Edit::Write("k.txt", 2)],
            &[Edit::Write("Other.txt", 3)],
        )
        .await;

        assert_eq!(merged.get("k.txt"), Some(&content(2, 0o644)));
        assert!(!merged.contains_key(KELVIN), "{merged:?}");
    })
    .await;
}

/// A link renamed only in case keeps the revision, the repository and the node it points at.
#[tokio::test]
async fn a_case_only_move_of_a_link_keeps_its_target() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[Edit::Move("Mount", "mount"), Edit::Write("Other.txt", 2)],
            &[Edit::Write("File.txt", 3)],
        )
        .await;

        assert_eq!(
            merged.get("mount"),
            Some(&(
                Hash::from_u64(0xabcd),
                0,
                0o755,
                Some((Context::from(MOUNT_REPOSITORY), 7))
            ))
        );
        assert!(!merged.contains_key("Mount"), "{merged:?}");
    })
    .await;
}

/// A move that changes case and parent both moves the node out of the directory it leaves.
#[tokio::test]
async fn a_move_that_changes_case_and_parent_moves_the_node() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[Edit::Move("Dir/a.txt", "A.txt")],
            &[Edit::Write("Other.txt", 3)],
        )
        .await;

        assert_eq!(merged.get("A.txt"), Some(&content(1, 0o644)));
        assert_eq!(merged.get("Dir/b.txt"), Some(&content(1, 0o644)));
        assert!(!merged.contains_key("Dir/a.txt"), "{merged:?}");
    })
    .await;
}
