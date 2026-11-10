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
    use std::sync::Mutex;
    use std::time::SystemTime;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Context;
    use lore_revision::branch;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::file::reset;
    use lore_revision::file::reset::LoreFileResetCountData;
    use lore_revision::file::reset::ResetOptions;
    use lore_revision::filter::FilterMode;
    use lore_revision::fs::filesystem_provider::FilesystemDiffIntent;
    use lore_revision::fs::filesystem_provider::FilesystemDiffTree;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEvent;
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

    /// A reset puts back a directory the working tree turned into a file, which a dry run leaves
    /// alone.
    #[tokio::test]
    async fn a_reset_puts_back_a_directory_turned_file_a_dry_run_leaves_alone() {
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

            let after = assert_dry_run_reset_writes_nothing(&fixture, &["directory"]).await;
            assert_eq!(
                after.get("directory/was_directory/file.txt"),
                Some(&Some(b"committed".to_vec()))
            );
        })
        .await;
    }

    /// Resets `paths`, relative to the fixture's root, through `repository`, to `revision`
    /// where one is named, purging what the revision does not hold.
    async fn reset_purging_through(
        fixture: &TestRepository,
        repository: Arc<RepositoryContext>,
        paths: &[&str],
        revision: &str,
    ) {
        reset::reset(
            repository,
            &fixture.write_token,
            LoreArray::from_vec(
                paths
                    .iter()
                    .map(|path| {
                        LoreString::from(fixture.path.join(path).to_string_lossy().as_ref())
                    })
                    .collect(),
            ),
            LoreString::from(revision),
            ResetOptions {
                purge: true,
                ..Default::default()
            },
        )
        .await
        .expect("A reset must succeed");
    }

    /// Resets `paths` through `repository` with a purge, as [`reset_purging_through`] does, under
    /// an execution holding `globals`, and answers the counts the reset reports at its end.
    async fn reset_purging_under(
        fixture: &TestRepository,
        repository: Arc<RepositoryContext>,
        paths: &[&str],
        globals: LoreGlobalArgs,
    ) -> LoreFileResetCountData {
        let counts = Arc::new(Mutex::new(None));
        let recorded = counts.clone();
        let execution = Arc::new(ExecutionContext::new_client(
            globals,
            EventDispatcher::new(Some(Box::new(move |event: &LoreEvent| {
                if let LoreEvent::FileResetEnd(end) = event {
                    *recorded.lock().expect("Failed to record the counts") =
                        Some(end.count.clone());
                }
            }))),
        ));
        LORE_CONTEXT
            .scope(
                execution.clone(),
                reset_purging_through(fixture, repository, paths, ""),
            )
            .await;
        execution.dispatcher.drain().await;
        counts
            .lock()
            .expect("Failed to read the counts")
            .take()
            .expect("A reset reports its counts at its end")
    }

    /// Asserts what each of `expected` holds below the fixture's root, `None` for a path the
    /// working tree must not hold.
    fn assert_working_files(fixture: &TestRepository, expected: &[(&str, Option<&[u8]>)]) {
        for (relative, content) in expected {
            let path = fixture.path.join(relative);
            assert_eq!(
                std::fs::read(&path).ok().as_deref(),
                *content,
                "{relative} after the reset"
            );
            assert!(
                content.is_some() || !path.exists(),
                "{relative} is still there"
            );
        }
    }

    /// A purge deletes everything the revision's view-filtered tree lacks, whatever the ignore
    /// rules say: untracked files whether a rule matches them or not, one below a committed
    /// directory a rule excludes, a temporary file and merge sides among them, and a committed
    /// directory the view excludes. It keeps and resets the committed files a rule matches.
    #[tokio::test]
    async fn a_purge_deletes_all_the_view_filtered_revision_lacks_whatever_the_ignore_rules_say() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_directory_create(&path("saved"));
            test_directory_create(&path("hidden"));
            for relative in ["cache.tmp", "saved/config.ini", "hidden/a.txt"] {
                test_file_write(&path(relative), b"committed");
            }
            test_commit_tree(&fixture, "Initial").await;
            test_file_write(&path("cache.tmp"), b"local");
            test_file_write(&path("saved/config.ini"), b"local");
            test_directory_create(&path("saved/logs"));
            for relative in [
                "build.tmp",
                "plain.txt",
                "saved/logs/run.log",
                "write.~loretemp",
                "a.txt~theirs",
                "saved/config.ini~base",
            ] {
                test_file_write(&path(relative), b"untracked");
            }

            let filtered = test_filter_context(
                &fixture,
                fixture.repository.immutable_store(),
                fixture.repository.mutable_store.clone(),
                &["*.tmp", "saved/"],
                &["hidden/"],
            );
            reset_purging_through(&fixture, filtered, &[""], "").await;

            assert_working_files(
                &fixture,
                &[
                    ("cache.tmp", Some(b"committed")),
                    ("saved/config.ini", Some(b"committed")),
                    ("write.~loretemp", None),
                    ("a.txt~theirs", None),
                    ("saved/config.ini~base", None),
                    ("build.tmp", None),
                    ("plain.txt", None),
                    ("saved/logs", None),
                    ("hidden", None),
                ],
            );
        })
        .await;
    }

    /// A purge counts and removes nothing for a committed file the view excludes that the working
    /// tree does not hold.
    #[tokio::test]
    async fn a_purge_counts_no_delete_for_an_excluded_file_the_working_tree_lacks() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_file_write(&path("art.psd"), b"committed");
            test_file_write(&path("kept.txt"), b"committed");
            test_commit_tree(&fixture, "Initial").await;
            std::fs::remove_file(path("art.psd")).expect("Failed to delete file");

            let filtered = test_filter_context(
                &fixture,
                fixture.repository.immutable_store(),
                fixture.repository.mutable_store.clone(),
                &[],
                &["*.psd"],
            );
            let counts =
                reset_purging_under(&fixture, filtered, &[""], LoreGlobalArgs::default()).await;

            assert_eq!(
                (counts.directory_delete_count, counts.file_delete_count),
                (0, 0),
                "the purge counted a delete of a file the working tree does not hold"
            );
        })
        .await;
    }

    /// A forced purge applies no view, so it restores a committed file the view excludes rather
    /// than deleting it.
    #[tokio::test]
    async fn a_forced_purge_restores_a_committed_file_the_view_excludes() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_directory_create(&path("hidden"));
            test_file_write(&path("hidden/a.txt"), b"committed");
            test_commit_tree(&fixture, "Initial").await;
            test_file_write(&path("hidden/a.txt"), b"local");

            let filtered = test_filter_context(
                &fixture,
                fixture.repository.immutable_store(),
                fixture.repository.mutable_store.clone(),
                &[],
                &["hidden/"],
            );
            reset_purging_under(
                &fixture,
                filtered,
                &[""],
                LoreGlobalArgs {
                    force: 1,
                    ..Default::default()
                },
            )
            .await;

            assert_working_files(&fixture, &[("hidden/a.txt", Some(b"committed"))]);
        })
        .await;
    }

    /// A purge keeps the temporary file a write in progress holds beside a file the revision
    /// holds, named from the file's extension or for a file without one, and deletes one beside
    /// nothing the revision holds.
    #[tokio::test]
    async fn a_purge_keeps_only_the_temporary_files_of_files_the_revision_holds() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_file_write(&path("a.bin"), b"committed");
            test_file_write(&path("plain"), b"committed");
            test_commit_tree(&fixture, "Initial").await;
            for relative in ["a.bin.~loretemp", "plain..~loretemp", "gone.bin.~loretemp"] {
                test_file_write(&path(relative), b"partial");
            }

            reset_purging_through(&fixture, fixture.repository.clone(), &[""], "").await;

            assert_working_files(
                &fixture,
                &[
                    ("a.bin.~loretemp", Some(b"partial")),
                    ("plain..~loretemp", Some(b"partial")),
                    ("gone.bin.~loretemp", None),
                ],
            );
        })
        .await;
    }

    /// A purge leaves an entry spelled in another case than a child the revision holds only where
    /// it is that child's entry, as on a file system that folds case. Elsewhere it is untracked: a
    /// file variant, edited, goes beside the file the reset restores, a directory variant goes with
    /// what it holds, and so does the temporary file of a write in progress spelled after a variant.
    #[tokio::test]
    async fn a_purge_leaves_a_case_variant_only_where_it_names_the_child() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            std::fs::create_dir_all(path("Dir")).expect("Failed to create the directory");
            for committed in ["Foo.txt", "Bar.txt", "Dir/a.txt"] {
                test_file_write(&path(committed), b"committed");
            }
            test_commit_tree(&fixture, "Initial").await;
            let folds_case = case_insensitive(&fixture.path);
            std::fs::rename(path("Foo.txt"), path("foo.txt")).expect("Failed to rename the file");
            test_file_write(&path("foo.txt"), b"edited");
            test_file_write(&path("bar.txt.~loretemp"), b"partial");
            std::fs::create_dir_all(path("dir")).expect("Failed to create the variant");
            test_file_write(&path("dir/b.txt"), b"untracked");

            reset_purging_through(&fixture, fixture.repository.clone(), &[""], "").await;

            assert_working_files(
                &fixture,
                &[
                    ("Foo.txt", Some(b"committed")),
                    ("foo.txt", folds_case.then_some(b"committed".as_slice())),
                    ("Bar.txt", Some(b"committed")),
                    (
                        "bar.txt.~loretemp",
                        folds_case.then_some(b"partial".as_slice()),
                    ),
                    ("Dir/a.txt", Some(b"committed")),
                    ("dir/b.txt", None),
                ],
            );
        })
        .await;
    }

    /// A purge leaves a nested repository in place, one an ignore rule matches, whether it reaches
    /// the repository from the root or names it.
    #[tokio::test]
    async fn a_purge_leaves_an_ignored_nested_repository_in_place() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_file_write(&path("kept.txt"), b"committed");
            test_commit_tree(&fixture, "Initial").await;
            test_directory_create(&path("nested/.lore"));

            for purged in ["", "nested"] {
                test_file_write(&path("nested/inner.txt"), b"inner");
                test_file_write(&path("junk.txt"), b"untracked");
                let ignoring = test_filter_context(
                    &fixture,
                    fixture.repository.immutable_store(),
                    fixture.repository.mutable_store.clone(),
                    &["nested/"],
                    &[],
                );
                reset_purging_through(&fixture, ignoring, &[purged, "junk.txt"], "").await;

                assert_working_files(
                    &fixture,
                    &[
                        ("nested/inner.txt", Some(b"inner")),
                        ("kept.txt", Some(b"committed")),
                        ("junk.txt", None),
                    ],
                );
                assert!(
                    path("nested/.lore").is_dir(),
                    "a purge of {purged:?} deleted the nested repository"
                );
            }
        })
        .await;
    }

    /// A purge deletes an untracked directory an ignore rule matches around the nested repository
    /// below it, keeping the repository whole and the directories leading to it, whether it
    /// reaches the directory from the root or names it.
    #[tokio::test]
    async fn a_purge_keeps_a_nested_repository_below_an_ignored_directory() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_file_write(&path("kept.txt"), b"committed");
            test_commit_tree(&fixture, "Initial").await;

            for purged in ["", "external"] {
                test_directory_create(&path("external/libs/nested/.lore"));
                test_directory_create(&path("external/cache"));
                for relative in [
                    "external/top.txt",
                    "external/cache/blob.bin",
                    "external/libs/beside.txt",
                    "external/libs/nested/inner.txt",
                ] {
                    test_file_write(&path(relative), b"untracked");
                }
                let ignoring = test_filter_context(
                    &fixture,
                    fixture.repository.immutable_store(),
                    fixture.repository.mutable_store.clone(),
                    &["external/"],
                    &[],
                );
                reset_purging_through(&fixture, ignoring, &[purged], "").await;

                assert_working_files(
                    &fixture,
                    &[
                        ("kept.txt", Some(b"committed")),
                        ("external/libs/nested/inner.txt", Some(b"untracked")),
                        ("external/top.txt", None),
                        ("external/cache", None),
                        ("external/libs/beside.txt", None),
                    ],
                );
                assert!(
                    path("external/libs/nested/.lore").is_dir(),
                    "a purge of {purged:?} deleted the nested repository"
                );
            }
        })
        .await;
    }

    /// A purge naming an untracked directory leaves a legacy `.urc` in it, as a purge listing the
    /// directory does, and deletes the files beside it.
    #[tokio::test]
    async fn a_purge_naming_a_directory_leaves_a_legacy_control_directory_in_it() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_file_write(&path("kept.txt"), b"committed");
            test_commit_tree(&fixture, "Initial").await;
            test_directory_create(&path("external/.urc"));
            test_directory_create(&path("external/sub"));
            for relative in [
                "external/.urc/state",
                "external/a.txt",
                "external/sub/b.txt",
            ] {
                test_file_write(&path(relative), b"untracked");
            }

            reset_purging_through(&fixture, fixture.repository.clone(), &["external"], "").await;

            assert_working_files(
                &fixture,
                &[
                    ("kept.txt", Some(b"committed")),
                    ("external/.urc/state", Some(b"untracked")),
                    ("external/a.txt", None),
                    ("external/sub", None),
                ],
            );
        })
        .await;
    }

    /// A reset without `--purge` to a revision that lacks a committed directory it names deletes
    /// the directory around the nested repository below it, keeping the repository whole.
    #[tokio::test]
    async fn a_reset_keeps_a_nested_repository_below_a_committed_directory_the_target_lacks() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_file_write(&path("kept.txt"), b"committed");
            let first = test_commit_tree(&fixture, "First").await.revision();
            test_directory_create(&path("tools"));
            test_file_write(&path("tools/build.sh"), b"committed");
            test_commit_tree(&fixture, "Second").await;
            test_directory_create(&path("tools/vendor/sdk/.lore"));
            test_file_write(&path("tools/vendor/sdk/inner.txt"), b"nested");
            test_file_write(&path("tools/vendor/beside.txt"), b"untracked");

            reset::reset(
                fixture.repository.clone(),
                &fixture.write_token,
                LoreArray::from_vec(vec![LoreString::from(
                    path("tools").to_string_lossy().as_ref(),
                )]),
                LoreString::from(first.to_string().as_str()),
                ResetOptions::default(),
            )
            .await
            .expect("A reset must succeed");

            assert_working_files(
                &fixture,
                &[
                    ("kept.txt", Some(b"committed")),
                    ("tools/vendor/sdk/inner.txt", Some(b"nested")),
                    ("tools/build.sh", None),
                    ("tools/vendor/beside.txt", None),
                ],
            );
            assert!(
                path("tools/vendor/sdk/.lore").is_dir(),
                "the reset deleted the nested repository"
            );
        })
        .await;
    }

    /// A purge to another revision deletes the files the current revision commits that the target
    /// lacks, one an ignore rule matches among them, as a reset sync to it does.
    #[tokio::test]
    async fn a_purge_to_another_revision_deletes_the_ignored_committed_files_it_lacks() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_file_write(&path("a.txt"), b"committed");
            let first = test_commit_tree(&fixture, "First").await.revision();
            test_file_write(&path("cfg.tmp"), b"committed");
            test_file_write(&path("plain.txt"), b"committed");
            test_commit_tree(&fixture, "Second").await;

            let ignoring = test_filter_context(
                &fixture,
                fixture.repository.immutable_store(),
                fixture.repository.mutable_store.clone(),
                &["*.tmp"],
                &[],
            );
            reset_purging_through(&fixture, ignoring, &[""], &first.to_string()).await;

            assert_working_files(
                &fixture,
                &[
                    ("a.txt", Some(b"committed")),
                    ("cfg.tmp", None),
                    ("plain.txt", None),
                ],
            );
        })
        .await;
    }

    /// A purge naming a committed file the view excludes deletes it, as one reaching it from a
    /// directory above does, and leaves the excluded file beside it alone.
    #[tokio::test]
    async fn a_purge_naming_a_committed_file_the_view_excludes_deletes_it() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_directory_create(&path("hidden"));
            test_file_write(&path("hidden/a.txt"), b"committed");
            test_file_write(&path("hidden/b.txt"), b"committed");
            test_commit_tree(&fixture, "Initial").await;

            let filtered = test_filter_context(
                &fixture,
                fixture.repository.immutable_store(),
                fixture.repository.mutable_store.clone(),
                &[],
                &["hidden/"],
            );
            reset_purging_through(&fixture, filtered, &["hidden/a.txt"], "").await;

            assert_working_files(
                &fixture,
                &[("hidden/a.txt", None), ("hidden/b.txt", Some(b"committed"))],
            );
        })
        .await;
    }

    /// A reset naming a committed file the view excludes, to a revision that lacks it, leaves the
    /// file in place without `--purge`, and deletes it with `--purge` or with `--force`, which
    /// applies no view.
    #[tokio::test]
    async fn a_reset_naming_an_excluded_file_the_target_lacks_deletes_it_only_purging_or_forced() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_file_write(&path("kept.txt"), b"committed");
            let first = test_commit_tree(&fixture, "First").await.revision();
            test_directory_create(&path("docs"));
            test_file_write(&path("docs/x.txt"), b"committed");
            test_commit_tree(&fixture, "Second").await;

            for (purge, force) in [(false, false), (true, false), (false, true)] {
                test_file_write(&path("docs/x.txt"), b"committed");
                let filtered = test_filter_context(
                    &fixture,
                    fixture.repository.immutable_store(),
                    fixture.repository.mutable_store.clone(),
                    &[],
                    &["docs/"],
                );
                let execution = Arc::new(ExecutionContext::new_client(
                    LoreGlobalArgs {
                        force: u8::from(force),
                        ..Default::default()
                    },
                    EventDispatcher::no_dispatch(),
                ));
                LORE_CONTEXT
                    .scope(
                        execution,
                        reset::reset(
                            filtered,
                            &fixture.write_token,
                            LoreArray::from_vec(vec![LoreString::from(
                                path("docs/x.txt").to_string_lossy().as_ref(),
                            )]),
                            LoreString::from(first.to_string().as_str()),
                            ResetOptions {
                                purge,
                                ..Default::default()
                            },
                        ),
                    )
                    .await
                    .expect("A reset must succeed");

                assert_eq!(
                    path("docs/x.txt").exists(),
                    !purge && !force,
                    "a reset with purge {purge} and force {force} of an excluded file the target \
                     lacks"
                );
            }
        })
        .await;
    }

    /// A reset of a directory puts back a committed file an ignore rule matches where a directory
    /// of build output replaced it, deleting the directory.
    #[tokio::test]
    async fn a_reset_puts_back_an_ignored_committed_file_a_directory_replaced() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_directory_create(&path("game"));
            test_file_write(&path("game/cache.tmp"), b"committed");
            test_commit_tree(&fixture, "Initial").await;
            std::fs::remove_file(path("game/cache.tmp")).expect("Failed to delete file");
            test_directory_create(&path("game/cache.tmp"));
            test_file_write(&path("game/cache.tmp/out.bin"), b"output");

            let ignoring = test_filter_context(
                &fixture,
                fixture.repository.immutable_store(),
                fixture.repository.mutable_store.clone(),
                &["*.tmp"],
                &[],
            );
            reset::reset(
                ignoring,
                &fixture.write_token,
                LoreArray::from_vec(vec![LoreString::from(
                    path("game").to_string_lossy().as_ref(),
                )]),
                LoreString::default(),
                ResetOptions::default(),
            )
            .await
            .expect("A reset must succeed");

            assert_eq!(
                std::fs::read(path("game/cache.tmp")).ok().as_deref(),
                Some(&b"committed"[..])
            );
        })
        .await;
    }

    /// A reset that fails at a later path still discards the dirty add an earlier path's reset
    /// replaced, so the staged state it stores holds the committed node under the name.
    #[tokio::test]
    async fn a_failing_reset_discards_the_dirty_add_an_earlier_path_replaced() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            let paths = |relative: &[&str]| {
                LoreArray::from_vec(
                    relative
                        .iter()
                        .map(|relative| LoreString::from(path(relative).to_string_lossy().as_ref()))
                        .collect(),
                )
            };
            test_file_write(&path("entry"), b"committed");
            test_file_write(&path("staged.txt"), b"committed");
            test_commit_tree(&fixture, "Initial").await;

            std::fs::remove_file(path("entry")).expect("Failed to delete file");
            test_directory_create(&path("entry"));
            test_file_write(&path("entry/inner.txt"), b"inner");
            let scan = StageOptions {
                scan: true,
                ..Default::default()
            };
            file::stage::stage(
                fixture.repository.clone(),
                &fixture.write_token,
                paths(&["entry"]),
                scan,
            )
            .await
            .expect("Stage failed");
            file::unstage::unstage(
                fixture.repository.clone(),
                &fixture.write_token,
                paths(&["."]),
                Default::default(),
            )
            .await
            .expect("Unstage failed");
            test_file_write(&path("staged.txt"), b"modified");
            file::stage::stage(
                fixture.repository.clone(),
                &fixture.write_token,
                paths(&["staged.txt"]),
                scan,
            )
            .await
            .expect("Stage failed");

            reset::reset(
                fixture.repository.clone(),
                &fixture.write_token,
                paths(&["entry", "staged.txt"]),
                LoreString::default(),
                ResetOptions::default(),
            )
            .await
            .expect_err("A reset of a staged file must fail");

            let (current, staged, _) =
                state::State::deserialize_current_and_staged(fixture.repository.clone())
                    .await
                    .expect("Failed to deserialize the states");
            let staged = staged.expect("The staged file keeps a staged state");
            let node = |state: Arc<state::State>| {
                let repository = fixture.repository.clone();
                async move {
                    state
                        .find_node_link(repository, "entry")
                        .await
                        .expect("The states hold entry")
                        .node
                }
            };
            assert_eq!(node(staged).await, node(current).await);
        })
        .await;
    }

    /// A reset naming an empty committed directory below a directory an ignore rule excludes
    /// creates it, as a reset consults the view alone.
    #[tokio::test]
    async fn a_reset_creates_an_empty_committed_directory_below_an_ignored_one() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_directory_create(&path("saved/empty"));
            test_commit_tree(&fixture, "Initial").await;
            std::fs::remove_dir(path("saved/empty")).expect("Failed to delete directory");

            let ignoring = test_filter_context(
                &fixture,
                fixture.repository.immutable_store(),
                fixture.repository.mutable_store.clone(),
                &["saved/"],
                &[],
            );
            reset::reset(
                ignoring,
                &fixture.write_token,
                LoreArray::from_vec(vec![LoreString::from(
                    path("saved/empty").to_string_lossy().as_ref(),
                )]),
                LoreString::default(),
                ResetOptions::default(),
            )
            .await
            .expect("A reset must succeed");

            assert!(
                path("saved/empty").is_dir(),
                "the reset did not create the empty committed directory"
            );
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

                // The purge deletes the untracked files whatever the ignore file says
                for ignored in [file_ignored_path, root_file_ignored_path] {
                    assert!(
                        !std::fs::exists(&ignored)
                            .expect("Could not check the existence of a file"),
                        "{} was not purged.",
                        ignored.display()
                    );
                }

                // No more changes expected after reset with purge
                assert!(changes.is_empty());
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
