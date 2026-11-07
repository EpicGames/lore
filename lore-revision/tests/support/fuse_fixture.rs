// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Shared fixture for FUSE provider tests: a committed, disk-backed `State`
//! built from a small file tree, using the `stage` + `commit_boxed` pattern
//! at `tests/path_allocations.rs`.
//!
//! Mounted by path into the unit binary and into the FUSE binaries that need a process of their
//! own; each mounts `tests/helper.rs` as `crate::helper`, whose fixtures this re-exports.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::State;

pub use crate::helper::*;

/// A committed, disk-backed fixture. `state` is the result of staging and
/// committing everything written under the fixture's working tree; the
/// temporary directory itself is not kept alive past `committed` returning,
/// since every test built on this reads through `state` and `repository`,
/// never the filesystem again.
pub struct FuseFixture {
    pub repository: Arc<RepositoryContext>,
    pub write_token: RepositoryWriteToken,
    pub path: PathBuf,
    pub state: Arc<State>,
}

/// Writes each `(relative path, contents)` pair under a fresh repository's
/// working tree, stages and commits the whole tree, and answers the fixture
/// holding the committed state.
pub async fn committed(files: &[(&str, &[u8])]) -> FuseFixture {
    committed_revisions(&[files]).await
}

/// As [`committed`], one commit per entry of `revisions`, each writing its
/// files over the tree the one before left. The fixture holds the last state.
/// Every commit after the first records file history, which only then fills
/// the tree's file metadata blocks.
pub async fn committed_revisions(revisions: &[&[(&str, &[u8])]]) -> FuseFixture {
    let (immutable_store, mutable_store, execution) = test_store_create()
        .await
        .expect("test store creation must succeed");

    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async move {
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
            let instance =
                test_repository_create(immutable_store, mutable_store, repository_id).await;

            let mut state = None;
            for files in revisions {
                for (name, contents) in *files {
                    let full_path = instance.path.join(name);
                    if let Some(parent) = full_path.parent() {
                        std::fs::create_dir_all(parent).unwrap_or_else(|_| {
                            panic!("failed to create parent directory for {name}")
                        });
                    }
                    test_file_write(&full_path, contents);
                }
                state = Some(test_commit_tree(&instance, "fuse fixture").await);
            }
            let state = state.expect("at least one revision");

            FuseFixture {
                repository: instance.repository.clone(),
                write_token: instance.write_token,
                path: instance.path.clone(),
                state,
            }
        })
        .await
}

/// A committed fixture that holds no write token, as a mounted instance is between commands,
/// with its branch metadata stored as a clone stores it.
pub struct ReleasedFixture {
    /// The instance's context, without a write token.
    pub repository: Arc<RepositoryContext>,
    pub immutable: Arc<dyn lore_storage::ImmutableStore>,
    pub mutable: Arc<dyn lore_storage::MutableStore>,
    pub path: PathBuf,
    pub state: Arc<State>,
    /// The instance's directory, `path`, kept while the fixture lives: commands through the
    /// fixture read the filesystem clock from a probe under its `.lore`.
    pub directory: TempDir,
}

/// As [`committed`], then releases the write token, so a provider over the fixture takes the
/// instance's token for itself.
pub async fn released(files: &[(&str, &[u8])]) -> ReleasedFixture {
    released_revisions(&[files]).await
}

/// As [`released`], one commit per entry of `revisions`, as [`committed_revisions`] makes them:
/// every commit after the first fills the tree's file metadata blocks.
pub async fn released_revisions(revisions: &[&[(&str, &[u8])]]) -> ReleasedFixture {
    released_deleting(revisions, &[]).await
}

/// As [`released_revisions`], with the files `deleted` removed before the last commit, which
/// frees their node slots and puts each block it frees one in on the tree's unused chain. A path
/// ending in `/` names a directory, removed with everything below it.
pub async fn released_deleting(
    revisions: &[&[(&str, &[u8])]],
    deleted: &[&str],
) -> ReleasedFixture {
    Box::pin(released_with(revisions, deleted, &[])).await
}

/// As [`released_revisions`], with the files `executable` made executable before the last
/// commit.
pub async fn released_executable(
    revisions: &[&[(&str, &[u8])]],
    executable: &[&str],
) -> ReleasedFixture {
    Box::pin(released_with(revisions, &[], executable)).await
}

/// As [`released_revisions`], with `deleted` removed ([`released_deleting`]) and `executable`
/// made executable before the last commit.
async fn released_with(
    revisions: &[&[(&str, &[u8])]],
    deleted: &[&str],
    executable: &[&str],
) -> ReleasedFixture {
    let (immutable, mutable, execution) = test_store_create()
        .await
        .expect("test store creation must succeed");
    let stores = (immutable.clone(), mutable.clone());
    let (repository, path, state) = lore_base::runtime::LORE_CONTEXT
        .scope(execution, async move {
            let instance = test_repository_create(
                stores.0,
                stores.1,
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            store_branch_metadata(&instance.repository, &instance.write_token).await;
            let mut state = None;
            for (index, files) in revisions.iter().enumerate() {
                if index + 1 == revisions.len() {
                    for name in deleted {
                        let path = instance.path.join(name.trim_end_matches('/'));
                        // Test fixture writes; not subject to repository write-token discipline.
                        #[allow(clippy::disallowed_methods)]
                        let removed = if name.ends_with('/') {
                            std::fs::remove_dir_all(path)
                        } else {
                            std::fs::remove_file(path)
                        };
                        removed.unwrap_or_else(|_| panic!("failed to delete {name}"));
                    }
                    for name in executable {
                        use std::os::unix::fs::PermissionsExt as _;
                        // Test fixture writes; not subject to repository write-token discipline.
                        #[allow(clippy::disallowed_methods)]
                        std::fs::set_permissions(
                            instance.path.join(name),
                            std::fs::Permissions::from_mode(0o755),
                        )
                        .unwrap_or_else(|_| panic!("failed to make {name} executable"));
                    }
                }
                for (name, contents) in *files {
                    let full_path = instance.path.join(name);
                    if let Some(parent) = full_path.parent() {
                        std::fs::create_dir_all(parent).unwrap_or_else(|_| {
                            panic!("failed to create parent directory for {name}")
                        });
                    }
                    test_file_write(&full_path, contents);
                }
                state = Some(test_commit_tree(&instance, "released fuse fixture").await);
            }
            let TestRepository {
                repository,
                write_token,
                path,
                _tempdir: directory,
            } = instance;
            drop(write_token);
            (
                Arc::new(repository.without_command_locks()),
                (path, directory),
                state.expect("at least one revision"),
            )
        })
        .await;
    let (path, directory) = path;
    ReleasedFixture {
        repository,
        immutable,
        mutable,
        path,
        state,
        directory,
    }
}

/// Stores the metadata of the current branch, which `status` reads and the fixture's commits
/// leave out.
async fn store_branch_metadata(repository: &Arc<RepositoryContext>, token: &RepositoryWriteToken) {
    let (_revision, branch) = lore_revision::instance::load_current_anchor_boxed(repository)
        .await
        .expect("the fixture's anchor");
    lore_revision::branch::create(
        repository.clone(),
        token,
        branch,
        lore_revision::branch::DEFAULT_DEFAULT_NAME,
        lore_revision::branch::default_category(),
        "test-user",
        0,
        vec![],
        false,
        false,
    )
    .await
    .expect("the fixture's branch metadata");
}

/// `files` small files with distinct contents, spread over directories of at most
/// `GENERATED_FILES_PER_DIR` entries, as `(relative path, contents)` in path order.
pub fn generated_tree(files: usize) -> Vec<(String, Vec<u8>)> {
    (0..files)
        .map(|i| {
            let path = format!("d{:05}/f{i:08}.txt", i / GENERATED_FILES_PER_DIR);
            let contents = format!("generated file {i}\n").into_bytes();
            (path, contents)
        })
        .collect()
}

pub const GENERATED_FILES_PER_DIR: usize = 1000;
