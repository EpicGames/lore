// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::collections::BTreeMap;
    use std::fs::File;
    use std::future::Future;
    use std::io::Seek;
    use std::io::Write;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::SystemTime;

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
    use lore_revision::fs::filesystem_provider::FilesystemDiffIntent;
    use lore_revision::fs::filesystem_provider::FilesystemDiffTree;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;
    use lore_revision::relay::EventDispatcher;
    use lore_revision::repository;
    use lore_revision::repository::DOT_LOREIGNORE;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::load_filter;
    use lore_revision::stage::StageOptions;
    use lore_revision::state;

    /// The changes the working tree shows against `state`, reported without marking
    /// anything, which is what these tests check a reset by.
    async fn filesystem_changes(
        repository: &Arc<repository::RepositoryContext>,
        state: &Arc<state::State>,
    ) -> Vec<lore_revision::change::NodeChange> {
        let operation = repository
            .file_system()
            .begin_operation()
            .await
            .expect("Failed to start filesystem operation");
        let changes = state::diff_filesystem(
            &operation,
            FilesystemDiffTree {
                repository: repository.clone(),
                state: state.clone(),
            },
            FilesystemDiffTree {
                repository: repository.clone(),
                state: state.clone(),
            },
            None, /* No subpath */
            FilterMode::Full,
            FilesystemDiffIntent::Report,
            Arc::new(Vec::new()),
        )
        .await
        .expect("Failed to diff filesystem")
        .collect()
        .await
        .expect("Failed to diff filesystem");
        operation
            .finalize()
            .await
            .expect("Failed to finish filesystem operation");
        changes
    }

    include!("helper.rs");

    /// The repository's own directory is held nowhere, so a reset naming it, in any case and with
    /// `--purge`, resets nothing and removes nothing. The path never reaches the walk that would
    /// otherwise take a path the revision holds no node for as an untracked one and delete it.
    #[tokio::test]
    async fn reset_purge_of_the_control_directory_path_leaves_it_in_place() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let path = fixture.path.clone();

                test_file_write(&path.join("kept.txt"), b"kept");
                test_commit_tree(&fixture, "Initial").await;

                let control = path.join(".lore");
                assert!(
                    control.join("id").is_file(),
                    "the fixture's control directory holds the repository id"
                );

                for spelling in [".lore", ".LORE", ".Lore/id"] {
                    reset::reset(
                        repository.clone(),
                        &fixture.write_token,
                        LoreArray::from_vec(vec![LoreString::from(
                            path.join(spelling).to_string_lossy().as_ref(),
                        )]),
                        LoreString::default(),
                        ResetOptions {
                            purge: true,
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap_or_else(|err| {
                        panic!("a reset naming {spelling} must succeed, got {err}")
                    });
                    assert!(
                        control.join("id").is_file(),
                        "{spelling}: the reset removed the control directory"
                    );
                }
                assert!(
                    path.join("kept.txt").is_file(),
                    "the content was left alone"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Every path under `root` but the control directory, with a file's content and `None` for a
    /// directory, which is what a dry run reset must leave as it found it.
    fn working_tree(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
        fn walk(root: &Path, directory: &Path, tree: &mut BTreeMap<String, Option<Vec<u8>>>) {
            for entry in std::fs::read_dir(directory).expect("Failed to list directory") {
                let path = entry.expect("Failed to read directory entry").path();
                let relative = path
                    .strip_prefix(root)
                    .expect("Entry outside the root")
                    .to_string_lossy()
                    .replace('\\', "/");
                if relative == ".lore" {
                    continue;
                }
                if path.is_dir() {
                    tree.insert(relative, None);
                    walk(root, &path, tree);
                } else {
                    tree.insert(
                        relative,
                        Some(std::fs::read(&path).expect("Failed to read file")),
                    );
                }
            }
        }
        let mut tree = BTreeMap::new();
        walk(root, root, &mut tree);
        tree
    }

    fn test_directory_create(path: &Path) {
        std::fs::create_dir_all(path)
            .unwrap_or_else(|_| panic!("Failed to create test directory at {}", path.display()));
    }

    /// Resets `paths`, relative to the fixture's root, purging what the revision does not hold.
    async fn reset_purging(
        fixture: &TestRepository,
        paths: &[&str],
    ) -> Result<(), lore_revision::file::reset::ResetError> {
        reset::reset(
            fixture.repository.clone(),
            &fixture.write_token,
            LoreArray::from_vec(
                paths
                    .iter()
                    .map(|path| {
                        LoreString::from(fixture.path.join(path).to_string_lossy().as_ref())
                    })
                    .collect(),
            ),
            LoreString::default(),
            ResetOptions {
                purge: true,
                ..Default::default()
            },
        )
        .await
    }

    /// Resets `paths` in a dry run, which must leave the working tree as it found it.
    async fn assert_dry_run_reset_leaves_the_working_tree(
        fixture: &TestRepository,
        paths: &[&str],
    ) {
        let before = working_tree(&fixture.path);
        let dry_run = Arc::new(ExecutionContext::new_client(
            LoreGlobalArgs {
                dry_run: 1,
                ..Default::default()
            },
            EventDispatcher::no_dispatch(),
        ));
        LORE_CONTEXT
            .scope(dry_run, reset_purging(fixture, paths))
            .await
            .expect("A dry run reset must succeed");
        assert_eq!(
            working_tree(&fixture.path),
            before,
            "a dry run reset of {paths:?} changed the working tree"
        );
    }

    /// Resets `paths` in a dry run, which must leave the working tree as it found it, and then
    /// without one, which must change it: a reset with nothing to do would pass the first check
    /// without testing anything. Returns the working tree the second reset leaves.
    async fn assert_dry_run_reset_writes_nothing(
        fixture: &TestRepository,
        paths: &[&str],
    ) -> BTreeMap<String, Option<Vec<u8>>> {
        let before = working_tree(&fixture.path);
        assert_dry_run_reset_leaves_the_working_tree(fixture, paths).await;

        reset_purging(fixture, paths)
            .await
            .expect("A reset must succeed");
        let after = working_tree(&fixture.path);
        assert_ne!(after, before, "a reset of {paths:?} had nothing to change");
        after
    }

    /// Runs `body` on a fresh repository fixture, in the test store's execution context.
    async fn with_fixture<F>(body: impl FnOnce(TestRepository) -> F + Send + 'static)
    where
        F: Future<Output = ()> + Send,
    {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                body(test_repository_create(immutable_store, mutable_store, repository_id).await)
                    .await;
            }))
            .await
            .expect("Test task failed");
    }

    /// A dry run reset of the root restores no modified or deleted file and purges no untracked
    /// one.
    #[tokio::test]
    async fn a_dry_run_reset_leaves_the_working_tree_alone() {
        with_fixture(async |fixture| {
            let path = fixture.path.clone();
            test_file_write(&path.join("modified.txt"), b"committed");
            test_file_write(&path.join("deleted.txt"), b"committed");
            test_commit_tree(&fixture, "Initial").await;

            test_file_write(&path.join("modified.txt"), b"local");
            std::fs::remove_file(path.join("deleted.txt")).expect("Failed to delete file");
            test_file_write(&path.join("untracked.txt"), b"local");

            assert_dry_run_reset_writes_nothing(&fixture, &[""]).await;
        })
        .await;
    }

    /// A dry run reset of files named inside a subdirectory leaves them alone, where the reset
    /// restores them.
    #[tokio::test]
    async fn a_dry_run_reset_of_files_in_a_subdirectory_leaves_them_alone() {
        with_fixture(async |fixture| {
            let path = fixture.path.clone();
            let directory = path.join("outer").join("inner");
            test_directory_create(&directory);
            test_file_write(&directory.join("modified.txt"), b"committed");
            test_file_write(&directory.join("deleted.txt"), b"committed");
            test_commit_tree(&fixture, "Initial").await;

            test_file_write(&directory.join("modified.txt"), b"local");
            std::fs::remove_file(directory.join("deleted.txt")).expect("Failed to delete file");

            let after = assert_dry_run_reset_writes_nothing(
                &fixture,
                &["outer/inner/modified.txt", "outer/inner/deleted.txt"],
            )
            .await;
            assert_eq!(
                after.get("outer/inner/modified.txt"),
                Some(&Some(b"committed".to_vec()))
            );
            assert_eq!(
                after.get("outer/inner/deleted.txt"),
                Some(&Some(b"committed".to_vec()))
            );
        })
        .await;
    }

    /// A dry run reset of a subdirectory recreates no deleted directory and purges no untracked
    /// file or directory under it. The reset itself leaves what is outside the subdirectory alone.
    #[tokio::test]
    async fn a_dry_run_reset_of_a_subdirectory_leaves_it_alone() {
        with_fixture(async |fixture| {
            let path = fixture.path.clone();
            let directory = path.join("directory");
            test_directory_create(&directory.join("deleted"));
            test_file_write(&directory.join("modified.txt"), b"committed");
            test_file_write(&directory.join("deleted").join("file.txt"), b"committed");
            test_file_write(&path.join("outside.txt"), b"committed");
            test_commit_tree(&fixture, "Initial").await;

            test_file_write(&directory.join("modified.txt"), b"local");
            std::fs::remove_dir_all(directory.join("deleted")).expect("Failed to delete directory");
            test_file_write(&directory.join("untracked.txt"), b"local");
            test_directory_create(&directory.join("untracked"));
            test_file_write(&directory.join("untracked").join("file.txt"), b"local");
            test_file_write(&path.join("outside.txt"), b"local");

            let after = assert_dry_run_reset_writes_nothing(&fixture, &["directory"]).await;
            assert_eq!(
                after.get("directory/deleted/file.txt"),
                Some(&Some(b"committed".to_vec()))
            );
            assert!(!after.contains_key("directory/untracked"));
            assert!(!after.contains_key("directory/untracked.txt"));
            assert_eq!(after.get("outside.txt"), Some(&Some(b"local".to_vec())));
        })
        .await;
    }

    /// A dry run reset leaves alone a file the working tree turned into a directory, where the
    /// reset puts the file back.
    #[tokio::test]
    async fn a_dry_run_reset_of_a_file_turned_directory_leaves_it_alone() {
        with_fixture(async |fixture| {
            let path = fixture.path.clone();
            let directory = path.join("directory");
            test_directory_create(&directory);
            test_file_write(&directory.join("was_file"), b"committed");
            test_commit_tree(&fixture, "Initial").await;

            std::fs::remove_file(directory.join("was_file")).expect("Failed to delete file");
            test_directory_create(&directory.join("was_file"));
            test_file_write(&directory.join("was_file").join("file.txt"), b"local");

            let after = assert_dry_run_reset_writes_nothing(&fixture, &["directory"]).await;
            assert_eq!(
                after.get("directory/was_file"),
                Some(&Some(b"committed".to_vec()))
            );
        })
        .await;
    }

    /// A dry run reset leaves alone a directory the working tree turned into a file. Checked in a
    /// dry run only: the reset itself fails to put the directory back, writing its files under
    /// the file that holds its path.
    #[tokio::test]
    async fn a_dry_run_reset_of_a_directory_turned_file_leaves_it_alone() {
        with_fixture(async |fixture| {
            let path = fixture.path.clone();
            let directory = path.join("directory");
            test_directory_create(&directory.join("was_directory"));
            test_file_write(
                &directory.join("was_directory").join("file.txt"),
                b"committed",
            );
            test_commit_tree(&fixture, "Initial").await;

            std::fs::remove_dir_all(directory.join("was_directory"))
                .expect("Failed to delete directory");
            test_file_write(&directory.join("was_directory"), b"local");

            assert_dry_run_reset_leaves_the_working_tree(&fixture, &["directory"]).await;
        })
        .await;
    }

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
                        default_repository_creation_args(
                            immutable_store.clone(),
                            mutable_store.clone(),
                        )
                        .with_path(&path)
                        .with_id(repository_id)
                        .with_instance_id(created_repo.instance_id)
                        .with_filter(load_filter(&path).expect("Failed to load filter")),
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
                };
                let _signature = commit::commit_boxed(repository.clone(), &write_token, options)
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
                std::fs::rename(path.join("dir_untouched"), path.join("dir_renamed"))
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
                        default_repository_creation_args(
                            immutable_store.clone(),
                            mutable_store.clone(),
                        )
                        .with_path(path.as_path())
                        .with_id(repository_id)
                        .with_instance_id(created_repo.instance_id)
                        .with_filter(load_filter(&path).expect("Failed to load filter")),
                    )
                    .with_write_token(write_token.share()),
                );

                let (current_revision, _current_branch) =
                    lore_revision::instance::load_current_anchor_boxed(&repository)
                        .await
                        .expect("Failed to load current anchor");

                let state_current = state::State::deserialize(repository.clone(), current_revision)
                    .await
                    .expect("Failed to deserialize current state");

                // Check the current filesystem status
                let changes = filesystem_changes(&repository, &state_current).await;

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
                let changes = filesystem_changes(&repository, &state_current).await;

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
                let changes = filesystem_changes(&repository, &state_current).await;

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
                        default_repository_creation_args(
                            immutable_store.clone(),
                            mutable_store.clone(),
                        )
                        .with_path(&path)
                        .with_id(repository_id)
                        .with_instance_id(created_repo.instance_id),
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
                };
                let _signature = commit::commit_boxed(repository.clone(), &write_token, options)
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
                        default_repository_creation_args(
                            immutable_store.clone(),
                            mutable_store.clone(),
                        )
                        .with_path(path.as_path())
                        .with_id(repository_id)
                        .with_instance_id(created_repo.instance_id),
                    )
                    .with_write_token(write_token.share()),
                );

                let (current_revision, _current_branch) =
                    lore_revision::instance::load_current_anchor_boxed(&repository)
                        .await
                        .expect("Failed to load current anchor");

                let state_current = state::State::deserialize(repository.clone(), current_revision)
                    .await
                    .expect("Failed to deserialize current state");

                // Check the current filesystem status
                let changes = filesystem_changes(&repository, &state_current).await;

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
                let changes = filesystem_changes(&repository, &state_current).await;

                // We expect no changes
                assert_eq!(changes.len(), 0);
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
                        default_repository_creation_args(
                            immutable_store.clone(),
                            mutable_store.clone(),
                        )
                        .with_path(&path)
                        .with_id(repository_id)
                        .with_instance_id(created_repo.instance_id),
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
                };
                let _signature = commit::commit_boxed(repository.clone(), &write_token, options)
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
                        default_repository_creation_args(
                            immutable_store.clone(),
                            mutable_store.clone(),
                        )
                        .with_path(path.as_path())
                        .with_id(repository_id)
                        .with_instance_id(created_repo.instance_id),
                    )
                    .with_write_token(write_token.share()),
                );

                let (current_revision, _current_branch) =
                    lore_revision::instance::load_current_anchor_boxed(&repository)
                        .await
                        .expect("Failed to load current anchor");

                let state_current = state::State::deserialize(repository.clone(), current_revision)
                    .await
                    .expect("Failed to deserialize current state");

                // Check the current filesystem status
                let changes = filesystem_changes(&repository, &state_current).await;

                // We expect one change
                assert_eq!(changes.len(), 1);
            }))
            .await
            .expect("Test task failed");
    }
}
