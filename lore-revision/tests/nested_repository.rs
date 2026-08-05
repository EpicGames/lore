// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Working-tree scan handling of a nested repository — a child directory that
//! is itself a Lore working copy (it carries its own `.lore/`).
//!
//! A nested repository must be treated as an implicit boundary: its contents
//! belong to the nested repository, not the parent, so the parent scan neither
//! descends into nor indexes it. A child directory that was indexed before the
//! boundary existed and is then removed must not leave an unremovable delete
//! entry behind (the "zombie" entry) — the parent has no committed base it
//! could be a deletion of, so the stale node is discarded on the next scan.

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
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::filter::FilterMode;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository;
    use lore_revision::repository::DOT_LORE;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::repository::load_filter;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;
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
    /// and return a write-capable [`RepositoryContext`] for it, along with the
    /// [`repository::RepositoryWriteToken`] needed to stage/commit into it.
    async fn create_repository(
        path: &Path,
        repository_id: RepositoryId,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
    ) -> (Arc<RepositoryContext>, repository::RepositoryWriteToken) {
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
        (repository, write_token)
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

    /// A child directory carrying its own `.lore/` is a nested repository: the
    /// parent scan must not index it or pull its contents into the parent tree.
    #[tokio::test]
    async fn nested_repository_is_not_indexed() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                let (repository, _write_token) = create_repository(
                    path.as_path(),
                    repository_id,
                    immutable_store.clone(),
                    mutable_store.clone(),
                )
                .await;

                // A tracked file in the parent so the scan has real work to do.
                let _ = create_file(path.join("parent_file.txt").as_path(), &[0, 1, 2, 3]);

                // A nested repository: a child directory with its own `.lore/`
                // control directory and content that belongs to it, not the parent.
                std::fs::create_dir(path.join("nested").as_path())
                    .expect("Create nested directory failed");
                std::fs::create_dir(path.join("nested").join(DOT_LORE).as_path())
                    .expect("Create nested/.lore directory failed");
                let _ = create_file(path.join("nested").join("inner.txt").as_path(), &[9, 9, 9]);

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

                let changes = scan(repository.clone(), state_staged, state_current).await;

                // The parent file is indexed; nothing under the nested repository is.
                assert!(
                    changes.iter().any(|c| c.path.as_str() == "parent_file.txt"),
                    "expected the parent's own file to be indexed"
                );
                assert!(
                    changes
                        .iter()
                        .all(|c| !c.path.as_str().starts_with("nested")),
                    "nested repository contents must not be indexed, found: {:?}",
                    changes
                        .iter()
                        .map(|c| c.path.as_str().to_string())
                        .collect::<Vec<_>>()
                );
            }))
            .await
            .expect("Test task panicked");
    }

    /// A directory already staged as a normal dirty-add that then becomes a
    /// nested repository root (a `.lore/` appears inside it) is a stale
    /// "zombie" entry: the next scan must discard the staged subtree instead
    /// of continuing to index the nested repository's contents.
    #[tokio::test]
    async fn staged_directory_becoming_nested_repository_is_discarded() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                let (repository, _write_token) = create_repository(
                    path.as_path(),
                    repository_id,
                    immutable_store.clone(),
                    mutable_store.clone(),
                )
                .await;

                // A plain child directory with content: the first scan stages
                // it as an ordinary dirty-add subtree.
                std::fs::create_dir(path.join("nested").as_path())
                    .expect("Create nested directory failed");
                let _ = create_file(path.join("nested").join("inner.txt").as_path(), &[9, 9, 9]);

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

                let changes = scan(
                    repository.clone(),
                    state_staged.clone(),
                    state_current.clone(),
                )
                .await;
                assert!(
                    changes
                        .iter()
                        .any(|c| c.path.as_str().starts_with("nested")),
                    "expected the plain directory to be indexed by the first scan"
                );

                // The directory becomes a nested repository root — as when
                // `lore repository create` runs inside a staged directory.
                std::fs::create_dir(path.join("nested").join(DOT_LORE).as_path())
                    .expect("Create nested/.lore directory failed");

                // The rescan discards the stale staged entry instead of
                // continuing to index the nested repository's contents.
                let changes = scan(repository.clone(), state_staged, state_current).await;
                assert!(
                    changes
                        .iter()
                        .all(|c| !c.path.as_str().starts_with("nested")),
                    "zombie entry for a staged directory turned nested repository must be \
                     discarded, found: {:?}",
                    changes
                        .iter()
                        .map(|c| c.path.as_str().to_string())
                        .collect::<Vec<_>>()
                );
            }))
            .await
            .expect("Test task panicked");
    }

    /// A directory that was already *committed* into the parent tree keeps
    /// being tracked, and its contents keep being descended into and indexed,
    /// even after a `.lore/` control directory appears inside it —
    /// mirroring how git does not silently untrack previously committed
    /// content just because a nested `.git` shows up. The auto-discard
    /// ("zombie" cleanup) only applies to never-committed staged entries;
    /// untracking a committed nested repository is left to an explicit user
    /// action, not this scan.
    #[tokio::test]
    async fn committed_directory_becoming_nested_repository_stays_tracked() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                let (repository, write_token) = create_repository(
                    path.as_path(),
                    repository_id,
                    immutable_store.clone(),
                    mutable_store.clone(),
                )
                .await;

                // A plain directory with a file, staged and committed into the
                // parent tree — so it is present in state_current, not just
                // state_staged.
                std::fs::create_dir(path.join("nested").as_path())
                    .expect("Create nested directory failed");
                let _ = create_file(path.join("nested").join("inner.txt").as_path(), &[1, 2, 3]);

                let paths = LoreArray::from_vec(vec![LoreString::from(&path)]);
                file::stage::stage(
                    repository.clone(),
                    &write_token,
                    paths,
                    StageOptions {
                        case_change: stage::StageCaseChange::Error,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: true,
                    },
                )
                .await
                .expect("Stage failed");

                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Commit nested directory".to_string()),
                ))
                .await
                .expect("Commit failed");

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

                // The already-committed directory becomes a nested repository
                // root, as when `lore repository create` runs inside it.
                std::fs::create_dir(path.join("nested").join(DOT_LORE).as_path())
                    .expect("Create nested/.lore directory failed");

                // Modify the already-tracked file so an "unmodified, no diff"
                // scan result can't be mistaken for the boundary having
                // silently swallowed it: if the parent still descends into
                // and indexes `nested/`, this modification must surface.
                let _ = create_file(path.join("nested").join("inner.txt").as_path(), &[4, 5, 6]);

                let changes = scan(repository.clone(), state_staged, state_current).await;
                assert!(
                    changes
                        .iter()
                        .any(|c| c.path.as_str() == "nested/inner.txt"),
                    "a directory committed before becoming a nested repository root \
                     must stay tracked, with its contents still indexed, found: {:?}",
                    changes
                        .iter()
                        .map(|c| c.path.as_str().to_string())
                        .collect::<Vec<_>>()
                );
            }))
            .await
            .expect("Test task panicked");
    }
}
