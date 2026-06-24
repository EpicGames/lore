// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::fs::File;
    use std::io::Seek;
    use std::io::Write;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::SystemTime;

    use lore_base::error::NoRemote;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Context;
    use lore_revision::branch;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::file::reset;
    use lore_revision::file::reset::ResetOptions;
    use lore_revision::filter::FilterMode;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;
    use lore_revision::relay::EventDispatcher;
    use lore_revision::repository;
    use lore_revision::repository::DOT_LOREIGNORE;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::repository::load_filter;
    use lore_revision::stage::StageOptions;
    use lore_revision::state;
    use lore_transport::ProtocolError;

    include!("helper.rs");

    fn create_file(path: &Path) -> File {
        let mut file = File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)
            .unwrap_or_else(|_| panic!("Failed to create test file at {}", path.display()));

        file.write_all(&[0, 1, 2, 3, 4])
            .unwrap_or_else(|_| panic!("Failed to write test file at {}", path.display()));

        file
    }

    #[tokio::test]
    async fn reset_all() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                std::fs::create_dir_all(path.as_path()).expect("Create directory failed");

                // Add loreignore file
                let mut file_loreignore = create_file(path.join(DOT_LOREIGNORE).as_path());

                // Add dir_ignored/ to the loreignore filter
                file_loreignore
                    .seek(std::io::SeekFrom::Start(0))
                    .expect("Failed to seek to beginning of .loreignore");
                file_loreignore
                    .write_all(b"dir_ignored/\nroot_file_ignored.txt\n")
                    .expect("Failed to write to .loreignore");

                let default_branch = Context::from(uuid::Uuid::now_v7());
                let write_token = repository::RepositoryWriteToken::acquire(path.as_path()).await;
                let created_repo = repository::create_local(
                    path.as_path(),
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
                        Some(path.clone()),
                        immutable_store.clone(),
                        mutable_store.clone(),
                        repository_id,
                        created_repo.instance_id,
                        Err(ProtocolError::from(NoRemote)),
                        load_filter(&path).expect("Failed to load filter"),
                        RepositoryFormat::Lore,
                    )
                    .with_write_token(write_token.share()),
                );
                lore_revision::instance::store_current_anchor_branch(&repository, default_branch)
                    .await
                    .expect("Failed to store anchor branch");

                // Create initial test directories
                // - dir_deleted
                // - dir_untouched
                // - dir_modified
                // - dir_modified/dir_notmodified
                // - dir_ignored
                std::fs::create_dir(path.join("dir_deleted").as_path())
                    .expect("Create dir_deleted failed");
                std::fs::create_dir(path.join("dir_untouched").as_path())
                    .expect("Create dir_untouched failed");
                std::fs::create_dir(path.join("dir_modified").as_path())
                    .expect("Create dir_modified failed");
                std::fs::create_dir(path.join("dir_modified/dir_notmodified").as_path())
                    .expect("Create dir_modified failed");
                std::fs::create_dir(path.join("dir_ignored").as_path())
                    .expect("Create dir_ignored failed");

                // Create initial test files
                // - file_unmodified.txt
                // - dir_deleted/file_deleted.txt
                // - dir_untouched/file_unmodified.txt
                // - dir_modified/file_modified.txt
                // - dir_modified/dir_notmodified/file_notmodified.txt
                // - dir_ignored/file_ignored.txt
                // - root_file_ignored.txt

                let _ = create_file(path.join("file_unmodified.txt").as_path());
                let _ = create_file(path.join("dir_deleted/file_deleted.txt").as_path());
                let _ = create_file(path.join("dir_untouched/file_unmodified.txt").as_path());
                let mut file_modified =
                    create_file(path.join("dir_modified/file_modified.txt").as_path());
                let _ = create_file(
                    path.join("dir_modified/dir_notmodified/file_notmodified.txt")
                        .as_path(),
                );

                let file_ignored_path = path.join("dir_ignored/file_ignored.txt");
                let mut file_ignored = create_file(file_ignored_path.as_path());
                let root_file_ignored_path = path.join("root_file_ignored.txt");
                let _ = create_file(root_file_ignored_path.as_path());

                // Stage current state
                let _signature = file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    StageOptions {
                        scan: true,
                        ..StageOptions::default()
                    },
                )
                .await
                .expect("Failed to stage repository");

                // Commit the initial revision
                let options = CommitOptions {
                    message: String::new(),
                    link_messages: std::collections::HashMap::new(),
                    link: None,
                    layer_messages: std::collections::HashMap::new(),
                    layer: None,
                    stats: false,
                };
                let _signature =
                    Box::pin(commit::commit(repository.clone(), &write_token, options))
                        .await
                        .expect("Failed to commit revision");

                // Create a new directory
                // - dir_added
                std::fs::create_dir(path.join("dir_added").as_path())
                    .expect("Create dir_added failed");

                // Create a new file
                // - dir_added/file_added.txt
                let _ = create_file(path.join("dir_added/file_added.txt").as_path());

                // Delete a directory
                // - dir_deleted
                std::fs::remove_dir_all(path.join("dir_deleted"))
                    .expect("Failed to delete dir_deleted directory");

                // Rename a directory
                // - dir_untouched -> dir_renamed
                lore_storage::fs_util::rename_file(
                    path.join("dir_untouched"),
                    path.join("dir_renamed"),
                )
                .expect("Failed to rename dir_untouched directory");

                // Modify a file
                // - dir_modified/file_modified.txt
                file_modified
                    .write_all(&[5, 6, 7, 8, 9])
                    .expect("Failed to modify file_modified.txt");
                let _ = file_modified.set_modified(SystemTime::now());

                // Modify an ignored file
                // - dir_ignored_file_ignored.txt
                file_ignored
                    .write_all(&[5, 6, 7, 8, 9])
                    .expect("Failed to modify file_ignored.txt");
                let _ = file_ignored.set_modified(SystemTime::now());

                let file_ignore_path = path.join("dir_ignored").join("file_ignored.txt");
                let file_ignored_contents = std::fs::read_to_string(file_ignore_path.as_path())
                    .expect("Failed to read file_ignored.txt");

                let repository = Arc::new(
                    RepositoryContext::new(
                        Some(path.as_path().to_path_buf()),
                        immutable_store.clone(),
                        mutable_store.clone(),
                        repository_id,
                        created_repo.instance_id,
                        Err(ProtocolError::from(NoRemote)),
                        load_filter(&path).expect("Failed to load filter"),
                        RepositoryFormat::Lore,
                    )
                    .with_write_token(write_token.share()),
                );

                let (current_revision, _current_branch) =
                    lore_revision::instance::load_current_anchor(&repository)
                        .await
                        .expect("Failed to load current anchor");

                let state_current = state::State::deserialize(repository.clone(), current_revision)
                    .await
                    .expect("Failed to deserialize current state");

                // Check the current filesystem status
                let (changes, _) = state::diff_filesystem(
                    repository.clone(),
                    state_current.clone(),
                    repository.clone(),
                    state_current.clone(),
                    None, /* No subpath */
                    FilterMode::Full,
                    std::sync::Arc::new(Vec::new()),
                )
                .await
                .expect("Failed to diff filesystem");

                // Couple of changes are expected
                assert!(!changes.is_empty());

                // Reset all changes to the repository without purging
                reset::reset(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    LoreString::default(),
                    ResetOptions::default(),
                )
                .await
                .expect("Failed to reset changes");

                // Check the current filesystem status again
                let (changes, _) = state::diff_filesystem(
                    repository.clone(),
                    state_current.clone(),
                    repository.clone(),
                    state_current.clone(),
                    None, /* No subpath */
                    FilterMode::Full,
                    std::sync::Arc::new(Vec::new()),
                )
                .await
                .expect("Failed to diff filesystem");

                // We expect four untracked changes
                assert_eq!(changes.len(), 4);

                // We expect the ignored file to still be changed
                let file_ignored_reset_contents =
                    std::fs::read_to_string(file_ignore_path.as_path())
                        .expect("Could not read from file_ignored.txt");
                assert_eq!(file_ignored_contents, file_ignored_reset_contents);

                // Reset all changes to the repository this time purging untracked files
                reset::reset(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    LoreString::default(),
                    ResetOptions {
                        purge: true,
                        ..Default::default()
                    },
                )
                .await
                .expect("Failed to reset changes");

                // Check the current filesystem status again after reset with purging
                let (changes, _) = state::diff_filesystem(
                    repository.clone(),
                    state_current.clone(),
                    repository.clone(),
                    state_current.clone(),
                    None, /* No subpath */
                    FilterMode::Full,
                    std::sync::Arc::new(Vec::new()),
                )
                .await
                .expect("Failed to diff filesystem");

                // We expect the ignored file to still be changed (and extant)
                let file_ignored_reset_contents =
                    std::fs::read_to_string(file_ignore_path.as_path())
                        .expect("Could not read from file_ignored.txt");
                assert_eq!(file_ignored_contents, file_ignored_reset_contents);

                // We expect the ignored file to still be changed (and extant)
                assert!(
                    std::fs::exists(&root_file_ignored_path)
                        .expect("Could not check the existence of root_file_ignored.txt"),
                    "root_file_ignored.txt was purged."
                );

                // No more changes expected after reset with purge
                assert!(changes.is_empty());

                // Reset all changes to the repository with both purge and force
                let force_context = Arc::new(ExecutionContext::new_client(
                    LoreGlobalArgs {
                        force: 1,
                        ..Default::default()
                    },
                    EventDispatcher::no_dispatch(),
                ));
                LORE_CONTEXT
                    .scope(
                        force_context,
                        reset::reset(
                            repository.clone(),
                            &write_token,
                            LoreArray::from_vec(vec![LoreString::from(&path)]),
                            LoreString::default(),
                            ResetOptions {
                                purge: true,
                                ..Default::default()
                            },
                        ),
                    )
                    .await
                    .expect("Failed to reset changes");

                // We expect the ignored files to be destroyed
                assert!(
                    !std::fs::exists(file_ignored_path)
                        .expect("Could not check the existence of file_ignored.txt"),
                    "file_ignored.txt was not force purged."
                );
                assert!(
                    !std::fs::exists(root_file_ignored_path)
                        .expect("Could not check the existence of root_file_ignored.txt"),
                    "root_file_ignored.txt was not force purged."
                );

                let _ = std::fs::remove_dir_all(path.as_path());
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn reset_single_file_modified() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                std::fs::create_dir_all(path.as_path()).expect("Create directory failed");

                let default_branch_id = Context::from(uuid::Uuid::now_v7());
                let write_token = repository::RepositoryWriteToken::acquire(path.as_path()).await;
                let created_repo = repository::create_local(
                    path.as_path(),
                    &write_token,
                    repository_id,
                    default_branch_id,
                    branch::DEFAULT_DEFAULT_NAME.to_string(),
                    repository::RepositoryConfig::default(),
                    false,
                )
                .await
                .expect("Failed to initialize repository");

                let repository = Arc::new(
                    RepositoryContext::new(
                        Some(path.clone()),
                        immutable_store.clone(),
                        mutable_store.clone(),
                        repository_id,
                        created_repo.instance_id,
                        Err(ProtocolError::from(NoRemote)),
                        Arc::default(),
                        RepositoryFormat::Lore,
                    )
                    .with_write_token(write_token.share()),
                );

                lore_revision::instance::store_current_anchor_branch(
                    &repository,
                    default_branch_id,
                )
                .await
                .expect("Failed to store anchor branch");

                // Create initial test directories
                // - dir_modified/modified/inner
                let inner_path = path.join("dir_modified").join("modified").join("inner");
                std::fs::create_dir_all(inner_path.as_path())
                    .expect("Create modified directory chain failed");

                // Create initial test files
                // - dir_modified/modified/inner/file_modified.txt
                let file_modified_path = inner_path.join("file_modified.txt");
                let mut file_modified = create_file(file_modified_path.as_path());

                // Stage current state
                let _signature = file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    StageOptions {
                        scan: true,
                        ..StageOptions::default()
                    },
                )
                .await
                .expect("Failed to stage repository");

                // Commit the initial revision
                let options = CommitOptions {
                    message: String::new(),
                    link_messages: std::collections::HashMap::new(),
                    link: None,
                    layer_messages: std::collections::HashMap::new(),
                    layer: None,
                    stats: false,
                };
                let _signature =
                    Box::pin(commit::commit(repository.clone(), &write_token, options))
                        .await
                        .expect("Failed to commit revision");

                // Modify a file
                // - dir_modified/modified/inner/file_modified.txt
                file_modified
                    .write_all(&[5, 6, 7, 8, 9])
                    .expect("Failed to modify file_modified.txt");
                let _ = file_modified.set_modified(SystemTime::now());

                let repository = Arc::new(
                    RepositoryContext::new(
                        Some(path.as_path().to_path_buf()),
                        immutable_store.clone(),
                        mutable_store.clone(),
                        repository_id,
                        created_repo.instance_id,
                        Err(ProtocolError::from(NoRemote)),
                        Arc::default(),
                        RepositoryFormat::Lore,
                    )
                    .with_write_token(write_token.share()),
                );

                let (current_revision, _current_branch) =
                    lore_revision::instance::load_current_anchor(&repository)
                        .await
                        .expect("Failed to load current anchor");

                let state_current = state::State::deserialize(repository.clone(), current_revision)
                    .await
                    .expect("Failed to deserialize current state");

                // Check the current filesystem status
                let (changes, _) = state::diff_filesystem(
                    repository.clone(),
                    state_current.clone(),
                    repository.clone(),
                    state_current.clone(),
                    None, /* No subpath */
                    FilterMode::Full,
                    std::sync::Arc::new(Vec::new()),
                )
                .await
                .expect("Failed to diff filesystem");

                // Couple of changes are expected
                assert!(!changes.is_empty());

                // Reset the modified file without purging
                reset::reset(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    LoreString::default(),
                    ResetOptions::default(),
                )
                .await
                .expect("Failed to reset changes");

                // Check the current filesystem status again
                let (changes, _) = state::diff_filesystem(
                    repository.clone(),
                    state_current.clone(),
                    repository.clone(),
                    state_current.clone(),
                    None, /* No subpath */
                    FilterMode::Full,
                    std::sync::Arc::new(Vec::new()),
                )
                .await
                .expect("Failed to diff filesystem");

                // We expect no changes
                assert_eq!(changes.len(), 0);

                let _ = std::fs::remove_dir_all(path.as_path());
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn reset_staged_file() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                std::fs::create_dir_all(path.as_path()).expect("Create directory failed");

                let default_branch_id = Context::from(uuid::Uuid::now_v7());
                let write_token = repository::RepositoryWriteToken::acquire(path.as_path()).await;
                let created_repo = repository::create_local(
                    path.as_path(),
                    &write_token,
                    repository_id,
                    default_branch_id,
                    branch::DEFAULT_DEFAULT_NAME.to_string(),
                    repository::RepositoryConfig::default(),
                    false,
                )
                .await
                .expect("Failed to initialize repository");

                let repository = Arc::new(
                    RepositoryContext::new(
                        Some(path.clone()),
                        immutable_store.clone(),
                        mutable_store.clone(),
                        repository_id,
                        created_repo.instance_id,
                        Err(ProtocolError::from(NoRemote)),
                        Arc::default(),
                        RepositoryFormat::Lore,
                    )
                    .with_write_token(write_token.share()),
                );

                lore_revision::instance::store_current_anchor_branch(
                    &repository,
                    default_branch_id,
                )
                .await
                .expect("Failed to store anchor branch");

                // Create initial test directories
                // - dir_modified/modified/inner
                let inner_path = path.join("dir_modified").join("modified").join("inner");
                std::fs::create_dir_all(inner_path.as_path())
                    .expect("Create modified directory chain failed");

                // Create initial test files
                // - dir_modified/modified/inner/file_modified.txt
                let file_modified_path = inner_path.join("file_modified.txt");
                let mut file_modified = create_file(file_modified_path.as_path());

                // Stage current state
                let _signature = file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    StageOptions {
                        scan: true,
                        ..StageOptions::default()
                    },
                )
                .await
                .expect("Failed to stage repository");

                // Commit the initial revision
                let options = CommitOptions {
                    message: String::new(),
                    link_messages: std::collections::HashMap::new(),
                    link: None,
                    layer_messages: std::collections::HashMap::new(),
                    layer: None,
                    stats: false,
                };
                let _signature =
                    Box::pin(commit::commit(repository.clone(), &write_token, options))
                        .await
                        .expect("Failed to commit revision");

                // Modify a file
                // - dir_modified/modified/inner/file_modified.txt
                file_modified
                    .write_all(&[5, 6, 7, 8, 9])
                    .expect("Failed to modify file_modified.txt");
                let _ = file_modified.set_modified(SystemTime::now());

                // Stage current state again
                let _signature = file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    StageOptions {
                        scan: true,
                        ..StageOptions::default()
                    },
                )
                .await
                .expect("Failed to stage repository");

                // Reset the modified file without purging, expect it to fail
                reset::reset(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    LoreString::default(),
                    ResetOptions::default(),
                )
                .await
                .expect_err("Failed to reset changes");

                let repository = Arc::new(
                    RepositoryContext::new(
                        Some(path.as_path().to_path_buf()),
                        immutable_store.clone(),
                        mutable_store.clone(),
                        repository_id,
                        created_repo.instance_id,
                        Err(ProtocolError::from(NoRemote)),
                        Arc::default(),
                        RepositoryFormat::Lore,
                    )
                    .with_write_token(write_token.share()),
                );

                let (current_revision, _current_branch) =
                    lore_revision::instance::load_current_anchor(&repository)
                        .await
                        .expect("Failed to load current anchor");

                let state_current = state::State::deserialize(repository.clone(), current_revision)
                    .await
                    .expect("Failed to deserialize current state");

                // Check the current filesystem status
                let (changes, _) = state::diff_filesystem(
                    repository.clone(),
                    state_current.clone(),
                    repository.clone(),
                    state_current.clone(),
                    None, /* No subpath */
                    FilterMode::Full,
                    std::sync::Arc::new(Vec::new()),
                )
                .await
                .expect("Failed to diff filesystem");

                // We expect one change
                assert_eq!(changes.len(), 1);

                let _ = std::fs::remove_dir_all(path.as_path());
            }))
            .await
            .expect("Test task failed");
    }
}
