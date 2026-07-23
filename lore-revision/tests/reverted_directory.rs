// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Working-tree scan handling of a reverted uncommitted directory add.
//!
//! When a directory (and its contents) is indexed as an uncommitted add and
//! then removed from disk before any commit, the next scan must discard the
//! stale node rather than report a delete. The parent has no committed base the
//! directory could be a deletion of, so a delete entry would be an unremovable
//! "zombie" — the same treatment already given to a reverted single-file add.

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use std::sync::Arc;

    use lore_base::error::NoRemote;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Context;
    use lore_revision::branch;
    use lore_revision::change::FileAction;
    use lore_revision::filter::FilterMode;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::repository::load_filter;
    use lore_revision::state;
    use lore_transport::ProtocolError;

    include!("helper.rs");

    /// Create (or truncate) a read/write file at `path` and write `contents` to
    /// it, returning the open handle. Panics if the file cannot be created or
    /// written, since a failed fixture setup invalidates the test.
    fn create_file(path: &Path, contents: &[u8]) -> File {
        let mut file = File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)
            .unwrap_or_else(|_| panic!("Failed to create test file at {}", path.display()));
        file.write_all(contents)
            .unwrap_or_else(|_| panic!("Failed to write test file at {}", path.display()));
        file
    }

    /// Build a fresh on-disk repository at `path` with no commits (revision 0)
    /// and return a write-capable [`RepositoryContext`] for it.
    async fn create_repository(
        path: &Path,
        repository_id: RepositoryId,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
    ) -> Arc<RepositoryContext> {
        std::fs::create_dir_all(path).expect("Create repository directory failed");
        let default_branch = Context::from(uuid::Uuid::now_v7());
        let write_token = repository::RepositoryWriteToken::acquire(path).await;
        let created_repo = repository::create_local(
            path,
            &write_token,
            repository_id,
            default_branch,
            branch::DEFAULT_DEFAULT_NAME.to_string(),
            repository::RepositoryConfig::default(),
            false,
        )
        .await
        .expect("Failed to create repository");

        let repository = Arc::new(
            RepositoryContext::new(
                Some(path.to_path_buf()),
                immutable_store,
                mutable_store,
                repository_id,
                created_repo.instance_id,
                Err(ProtocolError::from(NoRemote)),
                load_filter(path).expect("Failed to load filter"),
                RepositoryFormat::Lore,
            )
            .with_write_token(write_token.share()),
        );
        lore_revision::instance::store_current_anchor_branch(&repository, default_branch)
            .await
            .expect("Failed to store anchor branch");
        repository
    }

    /// Reconcile the working tree against the staged state, mutating `state_staged`
    /// in place exactly as `lore status --scan` does, and return the detected
    /// changes.
    async fn scan(
        repository: Arc<RepositoryContext>,
        state_staged: Arc<state::State>,
        state_current: Arc<state::State>,
    ) -> Vec<lore_revision::change::NodeChange> {
        let (changes, _stats) = state::diff_filesystem_ex(
            repository.clone(),
            state_staged,
            repository,
            state_current,
            None, /* full tree */
            FilterMode::Full,
            true, /* scan_dirty */
            Arc::new(Vec::new()),
        )
        .await
        .expect("Failed to diff filesystem");
        changes
    }

    /// A directory indexed as an uncommitted add (along with its contents) and
    /// then removed from disk must be discarded on the next scan rather than
    /// reported as a delete: with no committed base there is nothing to delete,
    /// and a delete entry would be an unremovable "zombie".
    #[tokio::test]
    async fn removed_uncommitted_directory_is_discarded_not_deleted() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                let repository = create_repository(
                    path.as_path(),
                    repository_id,
                    immutable_store.clone(),
                    mutable_store.clone(),
                )
                .await;

                // A directory with content that gets indexed as an uncommitted
                // add (the directory node plus its child file).
                std::fs::create_dir(path.join("ghost").as_path())
                    .expect("Create ghost directory failed");
                let _ = create_file(path.join("ghost").join("inner.txt").as_path(), &[7, 7, 7]);

                let (current_revision, _branch) =
                    lore_revision::instance::load_current_anchor(&repository)
                        .await
                        .expect("Failed to load current anchor");
                let state_current = state::State::deserialize(repository.clone(), current_revision)
                    .await
                    .expect("Failed to deserialize current state");
                let state_staged = state::State::deserialize(repository.clone(), current_revision)
                    .await
                    .expect("Failed to deserialize staged state");

                // First scan indexes the directory as an add.
                let changes = scan(
                    repository.clone(),
                    state_staged.clone(),
                    state_current.clone(),
                )
                .await;
                assert!(
                    changes
                        .iter()
                        .any(|c| c.path.as_str() == "ghost" && c.action == FileAction::Add),
                    "expected the new directory to be indexed as an add, found: {:?}",
                    changes
                        .iter()
                        .map(|c| (c.path.as_str().to_string(), c.action))
                        .collect::<Vec<_>>()
                );
                assert!(
                    changes.iter().any(|c| c.path.as_str() == "ghost/inner.txt"),
                    "expected the directory's contents to be indexed too, found: {:?}",
                    changes
                        .iter()
                        .map(|c| (c.path.as_str().to_string(), c.action))
                        .collect::<Vec<_>>()
                );

                // Remove it from disk and rescan against the same staged state.
                std::fs::remove_dir_all(path.join("ghost"))
                    .expect("Failed to remove ghost directory");
                let changes = scan(
                    repository.clone(),
                    state_staged.clone(),
                    state_current.clone(),
                )
                .await;
                assert!(
                    changes
                        .iter()
                        .all(|c| !c.path.as_str().starts_with("ghost")),
                    "removed uncommitted directory must be discarded, not reported, found: {:?}",
                    changes
                        .iter()
                        .map(|c| (c.path.as_str().to_string(), c.action))
                        .collect::<Vec<_>>()
                );

                // A further scan stays clean — the node was discarded, not merely
                // hidden, so it cannot resurface.
                let changes = scan(
                    repository.clone(),
                    state_staged.clone(),
                    state_current.clone(),
                )
                .await;
                assert!(
                    changes
                        .iter()
                        .all(|c| !c.path.as_str().starts_with("ghost")),
                    "discarded directory must not resurface on a later scan, found: {:?}",
                    changes
                        .iter()
                        .map(|c| (c.path.as_str().to_string(), c.action))
                        .collect::<Vec<_>>()
                );
            }))
            .await
            .expect("Test task panicked");
    }
}
