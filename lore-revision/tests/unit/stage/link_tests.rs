// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What a stage by name of paths inside a link, below a directory it creates first, commits in the
//! linked repository.

use std::sync::Arc;

use lore_revision::link::LinkTracker;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::ROOT_NODE;
use lore_revision::stage::StageCaseChange;
use lore_revision::stage::StageOptions;
use lore_revision::state::NodeMapping;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;

use super::staged_as_marked;
use super::staged_with;
use crate::dirty_move_fixture::committed;
use crate::dirty_move_fixture::dirtied;
use crate::dirty_move_fixture::removed;
use crate::dirty_move_fixture::rename;
use crate::dirty_move_fixture::with_instance;
use crate::dirty_move_fixture::written;
use crate::helper::TestRepository;
use crate::helper::mount_intermediate_at;
use crate::helper::next_tree;
use crate::helper::test_commit;
use crate::helper::test_file_write;

/// Commits `other.txt`, then a link mounted at `vendor` whose tree holds the file
/// `src/Intermediate`.
async fn committed_with_link(instance: &TestRepository) {
    committed(instance, &["other.txt"]).await;
    mount_intermediate_at(
        &instance.repository,
        "vendor",
        RepositoryId::from(uuid::Uuid::now_v7()),
        true,
    )
    .await;
    written(instance, &["vendor/src/Intermediate"]);
    test_commit(instance, "Linked").await;
}

/// Commits `other.txt` and `vendor/keep.txt`, then a link at each of `mounts`, directories of
/// `vendor`, whose trees each hold the file `src/Intermediate` and so give `src` one node ID.
async fn committed_with_links_in_vendor(instance: &TestRepository, mounts: &[&str]) {
    committed(instance, &["other.txt", "vendor/keep.txt"]).await;
    for mount in mounts {
        mount_intermediate_at(
            &instance.repository,
            mount,
            RepositoryId::from(uuid::Uuid::now_v7()),
            true,
        )
        .await;
        written(instance, &[format!("{mount}/src/Intermediate").as_str()]);
    }
    test_commit(instance, "Linked").await;
}

/// The record of the node `state` holds at `path`, read in the State of the link the path crosses,
/// or `None` where it holds none; a lookup failing other than for a missing name fails the test.
async fn linked_node(instance: &TestRepository, state: &Arc<State>, path: &str) -> Option<Node> {
    let link = match state
        .find_node_link(instance.repository.clone(), path)
        .await
    {
        Ok(link) if link.is_valid() => link,
        Ok(_) => return None,
        Err(error) if error.is_node_not_found() => return None,
        Err(error) => panic!("{path}: {error}"),
    };
    let (repository, held) = link
        .resolve(instance.repository.clone(), state.clone())
        .await
        .expect("the State holding the node");
    Some(held.node(repository, link.node).await.expect("the node"))
}

/// A file of a linked repository replaced by a directory holding a file, the path marked dirty and
/// staged by name, is committed in the linked repository as the directory with its file and the
/// file's content, as on an ordinary instance, and nothing is left marked.
#[tokio::test]
async fn a_stage_in_a_link_of_a_directory_replacing_a_file_commits_the_file_in_it() {
    with_instance(|instance| async move {
        committed_with_link(&instance).await;
        removed(&instance, "vendor/src/Intermediate");
        let file = "vendor/src/Intermediate/x.txt";
        written(&instance, &[file]);
        dirtied(&instance, "vendor/src/Intermediate").await;

        staged_as_marked(&instance, &["vendor/src/Intermediate"]).await;
        let revision = test_commit(&instance, "Replaced").await;

        let committed = linked_node(&instance, &revision, file)
            .await
            .expect("the linked revision holds the file");
        assert!(
            committed.is_file() && committed.size == file.len() as u64,
            "flags {:#x}, size {}",
            committed.flags,
            committed.size
        );
        assert!(!committed.address.hash.is_zero(), "the file's content");
        let (_, staged, _) = State::deserialize_current_and_staged(instance.repository.clone())
            .await
            .expect("the states");
        if let Some(staged) = staged
            && let Some(marked) = linked_node(&instance, &staged, file).await
        {
            assert!(!marked.is_dirty(), "flags {:#x}", marked.flags);
        }
    })
    .await;
}

/// Two files of a linked repository in one directory, staged by name, are both committed in the
/// linked repository with their content, new files in a new directory and edited files in a
/// committed one alike.
#[tokio::test]
async fn a_stage_of_two_files_named_in_a_directory_of_a_link_commits_both() {
    let files = ["vendor/src/new/a.txt", "vendor/src/new/b.txt"];
    for edited in [false, true] {
        with_instance(|instance| async move {
            committed_with_link(&instance).await;
            written(&instance, &files);
            if edited {
                staged_as_marked(&instance, &files).await;
                test_commit(&instance, "Added").await;
                for file in files {
                    test_file_write(&instance.path.join(file), b"edited");
                }
            }

            staged_as_marked(&instance, &files).await;
            let revision = test_commit(&instance, "Two").await;

            for file in files {
                let committed = linked_node(&instance, &revision, file).await;
                let size = if edited { 6 } else { file.len() as u64 };
                assert!(
                    committed.is_some_and(|node| node.size == size && !node.address.hash.is_zero()),
                    "edited {edited}: {file}"
                );
            }
        })
        .await;
    }
}

/// How many children of the directory at `path` of `state`, read in the State of the link the path
/// crosses, carry `name`.
async fn linked_children_named(
    instance: &TestRepository,
    state: &Arc<State>,
    path: &str,
    name: &str,
) -> usize {
    let link = state
        .find_node_link(instance.repository.clone(), path)
        .await
        .expect("the directory");
    let (repository, held) = link
        .resolve(instance.repository.clone(), state.clone())
        .await
        .expect("the State holding the directory");
    let mut count = 0;
    for child in held
        .node_children(repository.clone(), link.node)
        .await
        .expect("the directory's children")
    {
        let held_name = held
            .node_name_clone(repository.clone(), child)
            .await
            .expect("the child's name");
        count += usize::from(held_name == name);
    }
    count
}

/// Files named in two links, below directories the two linked repositories hold under one node ID
/// of their own trees, are each committed in the directory their own linked repository holds, and
/// no linked directory gains a second of one name: `vendor/a/src/x` is held in one, and
/// `vendor/b/src/y` in the other.
#[tokio::test]
async fn a_stage_in_two_links_commits_each_file_in_the_directory_its_repository_holds() {
    with_instance(|instance| async move {
        committed_with_links_in_vendor(&instance, &["vendor/a", "vendor/b"]).await;
        let kept = ["vendor/a/src/x/keep.txt", "vendor/b/src/y/keep.txt"];
        written(&instance, &kept);
        staged_as_marked(&instance, &kept).await;
        test_commit(&instance, "Kept").await;
        let files = [
            "vendor/a/src/x/1.txt",
            "vendor/a/src/x/2.txt",
            "vendor/b/src/y/1.txt",
            "vendor/b/src/y/2.txt",
        ];
        written(&instance, &files);

        staged_as_marked(&instance, &files).await;
        let revision = test_commit(&instance, "Four").await;

        for (directory, name) in [("vendor/a/src", "x"), ("vendor/b/src", "y")] {
            assert_eq!(
                linked_children_named(&instance, &revision, directory, name).await,
                1,
                "{directory}/{name}"
            );
        }
        for file in files.iter().chain(&kept) {
            assert!(
                linked_node(&instance, &revision, file).await.is_some(),
                "{file}"
            );
        }
    })
    .await;
}

/// Two edited files of a linked repository, named below its directory `src` spelled in another
/// case on disk and staged keeping the tree's case, are committed edited in the linked repository,
/// in the directory the stage renames on disk to the tree's spelling.
#[tokio::test]
async fn a_stage_keeping_the_case_in_a_link_commits_files_below_a_directory_spelled_otherwise() {
    with_instance(|instance| async move {
        committed_with_links_in_vendor(&instance, &["vendor/lib"]).await;
        let files = ["vendor/lib/src/a.txt", "vendor/lib/src/b.txt"];
        written(&instance, &files);
        staged_as_marked(&instance, &files).await;
        test_commit(&instance, "Added").await;
        rename(&instance, "vendor/lib/src", "vendor/lib/SRC");
        let named = ["vendor/lib/SRC/a.txt", "vendor/lib/SRC/b.txt"];
        for file in named {
            test_file_write(&instance.path.join(file), b"edited");
        }

        staged_with(
            &instance,
            &named,
            StageOptions {
                case_change: StageCaseChange::Keep,
                ..Default::default()
            },
        )
        .await;
        let revision = test_commit(&instance, "Edited").await;

        for file in files {
            let committed = linked_node(&instance, &revision, file).await;
            assert!(committed.is_some_and(|node| node.size == 6), "{file}");
        }
    })
    .await;
}

/// Where the walks of `files`, written below a link, start once the directories they share are
/// created first, asserting each starts in the State the link tracker holds for the link, at
/// `path`.
async fn tracked_walk_starts(
    instance: &TestRepository,
    files: &[&str],
    path: &str,
) -> Vec<NodeMapping> {
    written(instance, files);
    let repository = instance.repository.clone();
    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .expect("a filesystem operation");
    let tracker = LinkTracker::new();
    let targets: Vec<RelativePath> = files
        .iter()
        .map(|file| RelativePath::new_from_clean_parts(file, ""))
        .collect();

    let starts = lore_revision::file::stage::test_util::walk_starts(
        operation.clone(),
        repository.clone(),
        next_tree(&repository).await,
        tracker.clone(),
        &targets,
    )
    .await
    .expect("the directories created first");
    operation.finalize().await.expect("the operation finishes");

    let tracked = tracker.all_links();
    let [link] = tracked.as_slice() else {
        panic!("one link tracked, of {}", tracked.len());
    };
    for start in &starts {
        assert_eq!(start.repository.id, link.link_repository_id);
        assert!(Arc::ptr_eq(&start.state, &link.link_state));
        assert_eq!(start.path.as_str(), path);
    }
    starts
}

/// The walks of the targets below a directory created first inside a link start in the State the
/// link tracker holds for the link, at the path the directory stands at.
#[tokio::test]
async fn the_walks_below_a_directory_created_first_in_a_link_start_in_the_tracked_state() {
    with_instance(|instance| async move {
        committed_with_link(&instance).await;
        tracked_walk_starts(
            &instance,
            &["vendor/src/new/a.txt", "vendor/src/new/b.txt"],
            "vendor/src/new",
        )
        .await;
    })
    .await;
}

/// The walks of the targets in the mount directory of a link to a whole repository start at the
/// linked root, in the State the link tracker holds for the link.
#[tokio::test]
async fn the_walks_of_files_in_the_mount_of_a_whole_repository_start_at_its_root() {
    with_instance(|instance| async move {
        committed_with_link(&instance).await;
        let starts =
            tracked_walk_starts(&instance, &["vendor/a.txt", "vendor/b.txt"], "vendor").await;
        for start in starts {
            assert_eq!(start.node, ROOT_NODE);
        }
    })
    .await;
}
