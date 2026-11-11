// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_methods)] // Test fixtures read and edit the working tree directly.

//! A repository on a working tree of its own, for tests that commit, sync, switch branches and
//! merge through it and compare what the working tree holds.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::branch::merge::MergeError;
use lore_revision::commit;
use lore_revision::commit::CommitError;
use lore_revision::commit::CommitOptions;
use lore_revision::file;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::MetadataInherit;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository;
use lore_revision::repository::BranchSwitchOptions;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::revision::sync;
use lore_revision::revision::sync::SyncError;
use lore_revision::revision::sync::SyncOptions;
use lore_revision::stage::StageCaseChange;
use lore_revision::stage::StageOptions;
use lore_revision::state::State;

use crate::helper::create_branch_on_instance;
use crate::helper::test_file_write;
use crate::helper::test_store_create;

/// A repository on a working tree of its own, its stores in the tree's `.lore` directory.
pub(super) struct Instance {
    pub(super) repository: Arc<RepositoryContext>,
    pub(super) write_token: RepositoryWriteToken,
    pub(super) path: PathBuf,
    pub(super) main: BranchId,
    _tempdir: TempDir,
}

/// Runs `test` offline against an instance holding `tree` committed on the main branch, each file
/// holding its own path. The instance's directory is named for `name`, the test's.
pub(super) async fn run<F, Fut>(name: &str, tree: &'static [&'static str], test: F)
where
    F: FnOnce(Instance) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    test_store_create().await.expect("Failed to create stores");
    let execution = Arc::new(ExecutionContext::new_client(
        LoreGlobalArgs::default().set_offline(),
        EventDispatcher::no_dispatch(),
    ));
    execution.set_user_id("operator@example.com").await;
    let prefix = format!("{name}-");
    lore_base::lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let tempdir = TempDir::new(&prefix);
        let path = tempdir.to_path_buf();
        let main = BranchId::from(uuid::Uuid::now_v7());
        let write_token = RepositoryWriteToken::acquire(&path).await;
        let repository = repository::create_local(
            &path,
            &write_token,
            RepositoryId::from(uuid::Uuid::now_v7()),
            main,
            branch::DEFAULT_DEFAULT_NAME.to_string(),
            repository::RepositoryConfig::default(),
            false,
        )
        .await
        .expect("Failed to create the repository");
        let instance = Instance {
            repository,
            write_token,
            path,
            main,
            _tempdir: tempdir,
        };
        for path in tree {
            write(&instance, path, path);
        }
        stage_and_commit(&instance, &instance.path, "First").await;
        test(instance).await;
    }))
    .await
    .expect("Test task failed");
}

/// Writes `content` to the file at `path` in the working tree, creating its directory.
pub(super) fn write(instance: &Instance, path: &str, content: &str) {
    let absolute = instance.path.join(path);
    std::fs::create_dir_all(absolute.parent().expect("A parent directory"))
        .expect("Failed to create the directory");
    test_file_write(&absolute, content.as_bytes());
}

/// Every file under `root` by its path, spelled as the file system holds it, with its content.
pub(super) fn working_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((directory, prefix)) = pending.pop() {
        for entry in std::fs::read_dir(&directory).expect("Failed to list a directory") {
            let entry = entry.expect("Failed to read a directory entry");
            let name = entry.file_name().into_string().expect("A UTF-8 name");
            if prefix.is_empty() && name == repository::DOT_LORE {
                continue;
            }
            let path = format!("{prefix}{name}");
            if entry.file_type().expect("A file type").is_dir() {
                pending.push((entry.path(), format!("{path}/")));
            } else {
                files.insert(
                    path,
                    std::fs::read(entry.path()).expect("Failed to read a file"),
                );
            }
        }
    }
    files
}

/// `files` with `to` in place of `from` at the start of every path below or at `from`.
pub(super) fn renamed(
    files: &BTreeMap<String, Vec<u8>>,
    from: &str,
    to: &str,
) -> BTreeMap<String, Vec<u8>> {
    files
        .iter()
        .map(|(path, content)| {
            let path = match path.strip_prefix(from) {
                Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{to}{rest}"),
                _ => path.clone(),
            };
            (path, content.clone())
        })
        .collect()
}

/// Stages `path` and everything below it, taking a name that differs from the tree's only in case
/// as a rename.
pub(super) async fn stage(instance: &Instance, path: &Path) {
    file::stage::stage(
        instance.repository.clone(),
        &instance.write_token,
        LoreArray::from_vec(vec![LoreString::from(path)]),
        StageOptions {
            case_change: StageCaseChange::Rename,
            scan: true,
            ..Default::default()
        },
    )
    .await
    .expect("Failed to stage");
}

/// Commits what is staged without forcing, answering the revision.
pub(super) async fn commit(instance: &Instance, message: &str) -> Result<Hash, CommitError> {
    commit::commit_boxed(
        instance.repository.clone(),
        &instance.write_token,
        CommitOptions::new(message.to_string()),
    )
    .await
}

/// [`stage`]s `path` and [`commit`]s it, answering the revision.
pub(super) async fn stage_and_commit(instance: &Instance, path: &Path, message: &str) -> Hash {
    stage(instance, path).await;
    commit(instance, message)
        .await
        .expect("The commit succeeds without --force")
}

/// An edit to the working tree.
#[derive(Clone, Copy, Debug)]
pub(super) enum Edit {
    /// Renames the first path to the second.
    Move(&'static str, &'static str),
    /// Writes the content to the file at the path.
    Write(&'static str, &'static str),
    /// Deletes the file or the directory at the path.
    Delete(&'static str),
    /// Gives the file at the path the executable bit, or takes it away.
    #[cfg(unix)]
    Chmod(&'static str, bool),
}

/// Makes `edits` in the working tree, stages each move as one, and commits them, answering the
/// revision.
pub(super) async fn commit_edits(instance: &Instance, edits: &[Edit], message: &str) -> Hash {
    for edit in edits {
        match *edit {
            Edit::Move(from, to) => {
                let (from, to) = (instance.path.join(from), instance.path.join(to));
                std::fs::rename(&from, &to).expect("rename");
                file::stage::stage_move(
                    instance.repository.clone(),
                    &instance.write_token,
                    from.to_string_lossy().into_owned(),
                    to.to_string_lossy().into_owned(),
                    StageOptions {
                        case_change: StageCaseChange::Rename,
                        scan: true,
                        ..Default::default()
                    },
                )
                .await
                .expect("Failed to stage the move");
            }
            Edit::Write(path, content) => write(instance, path, content),
            Edit::Delete(path) => {
                let absolute = instance.path.join(path);
                if absolute.is_dir() {
                    std::fs::remove_dir_all(absolute).expect("Failed to delete");
                } else {
                    std::fs::remove_file(absolute).expect("Failed to delete");
                }
            }
            #[cfg(unix)]
            Edit::Chmod(path, executable) => {
                use std::os::unix::fs::PermissionsExt;
                let mode = if executable { 0o755 } else { 0o644 };
                std::fs::set_permissions(
                    instance.path.join(path),
                    std::fs::Permissions::from_mode(mode),
                )
                .expect("Failed to change the mode");
            }
        }
    }
    stage_and_commit(instance, &instance.path, message).await
}

pub(super) async fn switch_to(instance: &Instance, branch: &str) {
    repository::branch_switch(
        instance.repository.clone(),
        &instance.write_token,
        branch.to_string(),
        BranchSwitchOptions {
            signature: None,
            local: true,
            reset: false,
            search_nearest: false,
            bare: false,
        },
    )
    .await
    .expect("Failed to switch branch");
}

pub(super) async fn create_branch(instance: &Instance, name: &str) {
    create_branch_on_instance(&instance.repository, name, None, "", false)
        .await
        .expect("Failed to create the branch");
}

pub(super) async fn current(instance: &Instance) -> Hash {
    lore_revision::instance::load_current_anchor_boxed(&instance.repository)
        .await
        .expect("Failed to load the current revision")
        .0
}

pub(super) async fn revision_state(instance: &Instance, revision: Hash) -> Arc<State> {
    State::deserialize(instance.repository.clone(), revision)
        .await
        .expect("Failed to read the revision")
}

/// Merges branch `name` into the current one, answering whether the merge committed.
pub(super) async fn merge_branch(instance: &Instance, name: &str) -> bool {
    let before = current(instance).await;
    try_merge_branch(instance, name)
        .await
        .expect("Failed to merge");
    current(instance).await != before
}

/// Syncs the instance to `revision`, answering what the sync answers.
pub(super) async fn sync_to(instance: &Instance, revision: Hash) -> Result<(), SyncError> {
    sync::sync_boxed(
        instance.repository.clone(),
        &instance.write_token,
        SyncOptions {
            revision: Some(revision.to_string()),
            ..Default::default()
        },
    )
    .await
}

/// Merges branch `name` into the current one, answering what the merge answers.
pub(super) async fn try_merge_branch(instance: &Instance, name: &str) -> Result<Hash, MergeError> {
    start_merge(instance, name, false).await
}

/// Merges branch `name` into the current one, committing the merge unless `no_commit` asks to
/// leave it staged, and answers what the merge answers.
pub(super) async fn start_merge(
    instance: &Instance,
    name: &str,
    no_commit: bool,
) -> Result<Hash, MergeError> {
    let source = branch::resolve(instance.repository.clone(), name)
        .await
        .expect("Failed to resolve the branch to merge")
        .id;
    Box::pin(branch::merge::merge_start(
        instance.repository.clone(),
        &instance.write_token,
        source,
        branch::merge::MergeStartOptions {
            message: "Merge".to_string(),
            link_messages: std::collections::HashMap::new(),
            no_commit,
            scope: branch::merge::MergeScope::MainOnly,
            inherit_metadata: MetadataInherit::default(),
        },
    ))
    .await
}

/// The names `revision` stores along `path`, which is looked up ignoring case.
pub(super) async fn names_along(instance: &Instance, revision: Hash, path: &str) -> Vec<String> {
    let state = revision_state(instance, revision).await;
    let mut node: NodeID = state
        .find_node_link(instance.repository.clone(), path)
        .await
        .expect("Failed to find the path")
        .node;
    let mut names = Vec::new();
    while node != ROOT_NODE {
        names.push(
            state
                .node_name_clone(instance.repository.clone(), node)
                .await
                .expect("Failed to read a name"),
        );
        node = state
            .node(instance.repository.clone(), node)
            .await
            .expect("Failed to read a node")
            .parent;
    }
    names.reverse();
    names
}
