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
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::SystemTime;

    use lore_base::log::LoreLogLevel;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::file::reset;
    use lore_revision::file::reset::LoreFileResetCountData;
    use lore_revision::file::reset::ResetError;
    use lore_revision::file::reset::ResetMergeSide;
    use lore_revision::file::reset::ResetOptions;
    use lore_revision::filter::FilterMode;
    use lore_revision::fs::filesystem_provider::CaseLookup;
    use lore_revision::fs::filesystem_provider::FilesystemDiffIntent;
    use lore_revision::fs::filesystem_provider::FilesystemDiffTree;
    use lore_revision::fs::filesystem_provider::FilesystemProvider;
    use lore_revision::fs::filesystem_provider::FsError;
    use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
    use lore_revision::fs::filesystem_provider::StaticDispatchInstanceOperation;
    use lore_revision::fs::filesystem_provider::test_util::TestOperation;
    use lore_revision::fs::os::OsFilesystem;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEvent;
    use lore_revision::interface::LoreFileAction;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::BranchId;
    use lore_revision::lore::RepositoryId;
    use lore_revision::node::NodeID;
    use lore_revision::node::ROOT_NODE;
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

    fn test_directory_create(path: &Path) {
        std::fs::create_dir_all(path)
            .unwrap_or_else(|_| panic!("Failed to create test directory at {}", path.display()));
    }

    /// Resets `paths`, relative to the fixture's root, purging what the revision does not hold.
    async fn reset_purging(
        fixture: &TestRepository,
        paths: &[&str],
    ) -> Result<(), lore_revision::file::reset::ResetError> {
        reset_paths(
            fixture,
            paths,
            ResetOptions {
                purge: true,
                ..Default::default()
            },
        )
        .await
    }

    /// Resets `paths`, relative to the fixture's root, with `options`.
    async fn reset_paths(
        fixture: &TestRepository,
        paths: &[&str],
        options: ResetOptions,
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
            options,
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

    /// A reset puts back the spelling the revision holds an entry in where the working tree holds
    /// it in another case: a file holding the committed content, an edited file, and a directory,
    /// below which it restores what the revision holds. A reset of the root reaches them through
    /// its walk, purging or not, and a reset naming each of them through the path it names, in
    /// either spelling.
    #[tokio::test]
    async fn a_reset_puts_back_the_spelling_the_revision_holds() {
        for (paths, purge) in [
            (&[""][..], false),
            (&[""], true),
            (&["foo.txt", "Bar.txt", "dir"], false),
        ] {
            with_fixture(async move |fixture| {
                let path = |relative: &str| fixture.path.join(relative);
                std::fs::create_dir_all(path("Dir")).expect("Failed to create the directory");
                for committed in ["Foo.txt", "Bar.txt", "Dir/a.txt"] {
                    test_file_write(&path(committed), b"committed");
                }
                let state = test_commit_tree(&fixture, "Initial").await;
                for (committed, variant) in [("Foo.txt", "foo.txt"), ("Bar.txt", "bar.txt")] {
                    std::fs::rename(path(committed), path(variant))
                        .expect("Failed to spell the file in another case");
                }
                std::fs::rename(path("Dir"), path("dir"))
                    .expect("Failed to spell the directory in another case");
                test_file_write(&path("bar.txt"), b"edited");
                test_file_write(&path("dir/a.txt"), b"edited");

                reset_paths(
                    &fixture,
                    paths,
                    ResetOptions {
                        purge,
                        ..Default::default()
                    },
                )
                .await
                .expect("A reset must succeed");

                let committed = Some(b"committed".to_vec());
                assert_eq!(
                    working_tree(&fixture.path),
                    BTreeMap::from([
                        ("Bar.txt".to_string(), committed.clone()),
                        ("Dir".to_string(), None),
                        ("Dir/a.txt".to_string(), committed.clone()),
                        ("Foo.txt".to_string(), committed),
                    ]),
                    "a reset of {paths:?}, purging {purge}, left the working tree"
                );
                let changes = filesystem_changes(&fixture.repository, &state).await;
                assert!(
                    changes.is_empty(),
                    "a reset of {paths:?}, purging {purge}, left changes: {:?}",
                    test_reported(&changes)
                );
            })
            .await;
        }
    }

    /// A purging reset of a directory of many files the working tree holds in another case renames
    /// each to the revision's spelling and keeps it, the purge of the directory removing none of
    /// them however far the reset of each has got.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_purging_reset_keeps_every_file_it_renames_to_the_revision_spelling() {
        const FILES: usize = 400;
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            for index in 0..FILES {
                test_file_write(&path(&format!("File{index}.txt")), b"committed");
            }
            test_commit_tree(&fixture, "Initial").await;
            for index in 0..FILES {
                std::fs::rename(
                    path(&format!("File{index}.txt")),
                    path(&format!("file{index}.txt")),
                )
                .expect("Failed to spell the file in another case");
            }

            reset_purging(&fixture, &[""])
                .await
                .expect("A reset must succeed");

            let expected: BTreeMap<String, Option<Vec<u8>>> = (0..FILES)
                .map(|index| (format!("File{index}.txt"), Some(b"committed".to_vec())))
                .collect();
            assert_eq!(working_tree(&fixture.path), expected);
        })
        .await;
    }

    /// A reset takes an entry spelled in another case for a node only where it is the one entry of
    /// the name and of the node's kind. It writes the node beside a directory spelled as a file the
    /// revision holds, and beside two entries spelling the name otherwise, leaving them as they
    /// stand, whether it reaches the node from the root or names it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_reset_renames_an_entry_spelled_in_another_case_only_where_it_is_the_one_of_its_kind()
    {
        for paths in [&[""][..], &["Kind", "Twice.txt"]] {
            with_fixture(async move |fixture| {
                let path = |relative: &str| fixture.path.join(relative);
                for committed in ["Kind", "Twice.txt"] {
                    test_file_write(&path(committed), b"committed");
                }
                test_commit_tree(&fixture, "Initial").await;
                std::fs::remove_file(path("Kind")).expect("Failed to remove the file");
                test_directory_create(&path("kind"));
                test_file_write(&path("kind/x.txt"), b"untracked");
                std::fs::rename(path("Twice.txt"), path("twice.txt"))
                    .expect("Failed to spell the file in another case");
                test_file_write(&path("TWICE.txt"), b"untracked");

                reset_paths(&fixture, paths, ResetOptions::default())
                    .await
                    .expect("A reset must succeed");

                let committed = Some(b"committed".to_vec());
                let untracked = Some(b"untracked".to_vec());
                assert_eq!(
                    working_tree(&fixture.path),
                    BTreeMap::from([
                        ("Kind".to_string(), committed.clone()),
                        ("kind".to_string(), None),
                        ("kind/x.txt".to_string(), untracked.clone()),
                        ("TWICE.txt".to_string(), untracked),
                        ("Twice.txt".to_string(), committed.clone()),
                        ("twice.txt".to_string(), committed),
                    ]),
                    "a reset of {paths:?} left the working tree"
                );
            })
            .await;
        }
    }

    /// A filesystem provider whose operations read, write and remove in the working tree at `root`,
    /// refuse every rename, as a file system refuses one of an entry held open, find another case
    /// of a name as `case_lookup` says, and record each recursive removal in `removed` without
    /// removing anything, and each directory read in `directory_reads`.
    struct RefusingRenames {
        root: std::path::PathBuf,
        removed: Arc<parking_lot::Mutex<Vec<String>>>,
        case_lookup: CaseLookup,
        directory_reads: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl FilesystemProvider for RefusingRenames {
        async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
            Ok(Arc::new(InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Test(TestOperation {
                    reads: Some(OsFilesystem::new(&self.root).begin_operation()),
                    removed: self.removed.clone(),
                    case_lookup: self.case_lookup,
                    directory_reads: self.directory_reads.clone(),
                    ..TestOperation::default()
                }),
                false,
            )))
        }
    }

    /// What [`reset_refusing_renames`] answers.
    struct RefusedReset {
        result: Result<(), ResetError>,
        warnings: Vec<String>,
        /// The paths the reset removed recursively.
        #[cfg(target_os = "linux")]
        removed: Vec<String>,
        /// How many directories the reset read.
        directory_reads: usize,
    }

    /// Resets the root of `fixture` with `options` through a context over [`RefusingRenames`],
    /// which finds another case of a name as `case_lookup` says.
    async fn reset_refusing_renames(
        fixture: &TestRepository,
        options: ResetOptions,
        case_lookup: CaseLookup,
    ) -> RefusedReset {
        fn log_to_context(level: LoreLogLevel, _: &str, message: &str) {
            if let Some(execution) = lore_revision::lore::try_execution_context() {
                execution
                    .dispatcher
                    .send(LoreEvent::Log(EventDispatcher::make_log(
                        level,
                        message.to_string(),
                    )));
            }
        }
        let removed = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let directory_reads = Arc::new(AtomicUsize::new(0));
        let refusing = Arc::new(
            RepositoryContext::new(
                default_repository_creation_args(
                    fixture.repository.immutable_store(),
                    fixture.repository.mutable_store.clone(),
                )
                .with_path(&fixture.path)
                .with_id(fixture.repository.id)
                .with_instance_id(fixture.repository.instance_id)
                .with_filesystem_provider(Arc::new(RefusingRenames {
                    root: fixture.path.clone(),
                    removed: removed.clone(),
                    case_lookup,
                    directory_reads: directory_reads.clone(),
                })),
            )
            .with_write_token(fixture.write_token.share()),
        );
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let logged = warnings.clone();
        let execution = Arc::new(ExecutionContext::new_client(
            LoreGlobalArgs::default(),
            EventDispatcher::new(Some(Box::new(move |event: &LoreEvent| {
                if let LoreEvent::Log(log) = event
                    && log.level == LoreLogLevel::Warn
                {
                    logged
                        .lock()
                        .expect("Failed to record a warning")
                        .push(log.message.to_string());
                }
            }))),
        ));
        static LOG_TO_CONTEXT: std::sync::Once = std::sync::Once::new();
        LOG_TO_CONTEXT.call_once(|| lore_base::log::set_log_callback(Some(log_to_context)));
        let result = LORE_CONTEXT
            .scope(execution.clone(), async {
                reset::reset(
                    refusing,
                    &fixture.write_token,
                    LoreArray::from_vec(vec![LoreString::from(&fixture.path)]),
                    LoreString::default(),
                    options,
                )
                .await
            })
            .await;
        execution.dispatcher.drain().await;
        let warnings = std::mem::take(&mut *warnings.lock().expect("Failed to read the warnings"));
        #[cfg(target_os = "linux")]
        let removed = std::mem::take(&mut *removed.lock());
        RefusedReset {
            result,
            warnings,
            #[cfg(target_os = "linux")]
            removed,
            directory_reads: directory_reads.load(Ordering::Relaxed),
        }
    }

    /// A reset whose rename of an entry to the revision's spelling the file system refuses resets
    /// the entry where it stands, a file and a directory with what it holds, an edited file to its
    /// committed content, and warns once for every refusal, naming the first, whether it lists each
    /// directory before its children or, where a stat misses another case, on a child's first miss.
    #[tokio::test]
    async fn a_reset_resets_an_entry_whose_rename_is_refused_where_it_stands() {
        let on_miss = cfg!(target_os = "linux").then_some(CaseLookup::OnMiss);
        for case_lookup in std::iter::once(CaseLookup::Listing).chain(on_miss) {
            with_fixture(async move |fixture| {
                let path = |relative: &str| fixture.path.join(relative);
                std::fs::create_dir_all(path("Dir")).expect("Failed to create the directory");
                for committed in ["Foo.txt", "Dir/a.txt"] {
                    test_file_write(&path(committed), b"committed");
                }
                test_commit_tree(&fixture, "Initial").await;
                for (from, to) in [("Foo.txt", "foo.txt"), ("Dir", "dir")] {
                    std::fs::rename(path(from), path("spelling.tmp"))
                        .expect("Failed to rename the entry");
                    std::fs::rename(path("spelling.tmp"), path(to))
                        .expect("Failed to rename the entry");
                }
                test_file_write(&path("foo.txt"), b"edited");

                let reset =
                    reset_refusing_renames(&fixture, ResetOptions::default(), case_lookup).await;

                reset.result.expect("A reset must succeed");
                let committed = Some(b"committed".to_vec());
                assert_eq!(
                    working_tree(&fixture.path),
                    BTreeMap::from([
                        ("dir".to_string(), None),
                        ("dir/a.txt".to_string(), committed.clone()),
                        ("foo.txt".to_string(), committed),
                    ]),
                    "{case_lookup:?}"
                );
                assert_eq!(
                    reset.warnings.len(),
                    1,
                    "{case_lookup:?}: the reset warned {:?}",
                    reset.warnings
                );
            })
            .await;
        }
    }

    /// A reset that reads a directory only once the stat of a child's spelling misses reads none
    /// where every child stands in the revision's spelling, edited or not.
    #[tokio::test]
    async fn a_reset_reading_directories_on_a_miss_reads_none_without_one() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            std::fs::create_dir_all(path("Dir/Sub")).expect("Failed to create the directories");
            for committed in ["Top.txt", "Dir/a.txt", "Dir/Sub/b.txt"] {
                test_file_write(&path(committed), b"committed");
            }
            test_commit_tree(&fixture, "Initial").await;
            test_file_write(&path("Dir/a.txt"), b"edited");

            let reset =
                reset_refusing_renames(&fixture, ResetOptions::default(), CaseLookup::OnMiss).await;

            reset.result.expect("A reset must succeed");
            assert_eq!(
                std::fs::read(path("Dir/a.txt")).expect("Failed to read the file"),
                b"committed"
            );
            assert_eq!(reset.directory_reads, 0);
        })
        .await;
    }

    /// A reset that reads a directory only once the stat of a child's spelling misses reads each
    /// directory holding a miss once, whichever walk or file task misses first, and pairs every
    /// missed child from that one listing: two files in one directory, and a directory in the root,
    /// each held in another case. Their renames refused, each is reset where it stands, and what a
    /// directory held there stands in the revision's spelling, read no further. Linux alone, where
    /// a stat misses another case.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_reset_reading_directories_on_a_miss_reads_each_once() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            for directory in ["Dir", "Sub"] {
                std::fs::create_dir_all(path(directory)).expect("Failed to create the directory");
            }
            for committed in ["Dir/a.txt", "Dir/b.txt", "Sub/c.txt"] {
                test_file_write(&path(committed), b"committed");
            }
            test_commit_tree(&fixture, "Initial").await;
            for (from, to) in [("Dir/a.txt", "Dir/A.txt"), ("Dir/b.txt", "Dir/B.txt")] {
                std::fs::rename(path(from), path(to)).expect("Failed to rename the file");
            }
            std::fs::rename(path("Sub"), path("sub.tmp")).expect("Failed to rename the directory");
            std::fs::rename(path("sub.tmp"), path("sub")).expect("Failed to rename the directory");
            test_file_write(&path("Dir/A.txt"), b"edited");

            let reset =
                reset_refusing_renames(&fixture, ResetOptions::default(), CaseLookup::OnMiss).await;

            reset.result.expect("A reset must succeed");
            let committed = Some(b"committed".to_vec());
            assert_eq!(
                working_tree(&fixture.path),
                BTreeMap::from([
                    ("Dir".to_string(), None),
                    ("Dir/A.txt".to_string(), committed.clone()),
                    ("Dir/B.txt".to_string(), committed.clone()),
                    ("sub".to_string(), None),
                    ("sub/c.txt".to_string(), committed),
                ])
            );
            assert_eq!(
                reset.directory_reads, 2,
                "the root, for the directory, and the directory holding the files"
            );
            assert_eq!(
                reset.warnings.len(),
                1,
                "the reset warned {:?}",
                reset.warnings
            );
        })
        .await;
    }

    /// A purge whose walk could not read every entry of a directory lists the directory again, and
    /// leaves the entries the walk paired with a child: an entry kept in another spelling, its
    /// rename refused, is not purged as untracked. The purge then fails on the entry it cannot
    /// read, as it fails wherever the reset's walk did not list it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_purge_listing_a_directory_again_leaves_the_entries_paired_with_a_child() {
        use std::os::unix::ffi::OsStrExt;

        const FILES: usize = 50;
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            for index in 0..FILES {
                test_file_write(&path(&format!("File{index}.txt")), b"committed");
            }
            test_commit_tree(&fixture, "Initial").await;
            test_file_write(
                &fixture
                    .path
                    .join(std::ffi::OsStr::from_bytes(b"not-text-\xff.bin")),
                b"untracked",
            );
            for index in 0..FILES {
                std::fs::rename(
                    path(&format!("File{index}.txt")),
                    path(&format!("file{index}.txt")),
                )
                .expect("Failed to spell the file in another case");
            }

            let reset = reset_refusing_renames(
                &fixture,
                ResetOptions {
                    purge: true,
                    ..Default::default()
                },
                CaseLookup::OnMiss,
            )
            .await;

            assert!(
                reset.result.is_err(),
                "the purge must fail on the entry it cannot read"
            );
            assert!(
                reset.removed.is_empty(),
                "the purge removed the entries the walk kept: {:?}",
                reset.removed
            );
        })
        .await;
    }

    /// Resets the root of `fixture` with `globals`, answering each path the reset reports and the
    /// action it reports for it, by path.
    async fn reset_reports(
        fixture: &TestRepository,
        globals: LoreGlobalArgs,
    ) -> Vec<(String, LoreFileAction)> {
        let reported = Arc::new(Mutex::new(Vec::new()));
        let recorded = reported.clone();
        let execution = Arc::new(ExecutionContext::new_client(
            globals,
            EventDispatcher::new(Some(Box::new(move |event: &LoreEvent| {
                if let LoreEvent::FileResetFile(file) = event {
                    recorded
                        .lock()
                        .expect("Failed to record a file")
                        .push((file.path.to_string(), file.action));
                }
            }))),
        ));
        LORE_CONTEXT
            .scope(
                execution.clone(),
                reset_paths(fixture, &[""], ResetOptions::default()),
            )
            .await
            .expect("A reset must succeed");
        execution.dispatcher.drain().await;
        let mut reported = std::mem::take(&mut *reported.lock().expect("Failed to read the files"));
        reported.sort_by(|a, b| a.0.cmp(&b.0));
        reported
    }

    /// A dry run reset of entries the working tree holds in another case reports what the reset
    /// itself does: the move of each, and the restore of an edited file at the revision's
    /// spelling, whether the file is respelled or stands in a respelled directory. It leaves the
    /// working tree as it found it.
    #[tokio::test]
    async fn a_dry_run_reset_reports_a_respelled_entry_as_the_reset_does() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            std::fs::create_dir_all(path("Dir")).expect("Failed to create the directory");
            for committed in ["Dir/a.txt", "Dir/b.txt", "Dir/c.txt", "Foo.txt"] {
                test_file_write(&path(committed), b"committed");
            }
            test_commit_tree(&fixture, "Initial").await;
            std::fs::rename(path("Dir"), path("dir.tmp")).expect("Failed to rename the directory");
            std::fs::rename(path("dir.tmp"), path("dir")).expect("Failed to rename the directory");
            std::fs::rename(path("Foo.txt"), path("foo.tmp")).expect("Failed to rename the file");
            std::fs::rename(path("foo.tmp"), path("foo.txt")).expect("Failed to rename the file");
            for edited in ["dir/a.txt", "foo.txt"] {
                test_file_write(&path(edited), b"edited");
            }
            let before = working_tree(&fixture.path);

            let dry_run = reset_reports(
                &fixture,
                LoreGlobalArgs {
                    dry_run: 1,
                    ..Default::default()
                },
            )
            .await;
            assert_eq!(working_tree(&fixture.path), before);
            let reset = reset_reports(&fixture, LoreGlobalArgs::default()).await;

            assert_eq!(
                reset,
                [
                    ("Dir".to_string(), LoreFileAction::Move),
                    ("Dir/a.txt".to_string(), LoreFileAction::Keep),
                    ("Foo.txt".to_string(), LoreFileAction::Move),
                    ("Foo.txt".to_string(), LoreFileAction::Keep),
                ]
            );
            assert_eq!(dry_run, reset);
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

    /// Resets `paths` below `root` through `repository` to the revision of `target`.
    async fn reset_to_revision(
        repository: &Arc<RepositoryContext>,
        root: &Path,
        paths: &[&str],
        target: &state::State,
    ) -> Result<(), lore_revision::file::reset::ResetError> {
        reset::reset(
            repository.clone(),
            repository
                .try_write_token()
                .expect("The repository was opened for writing"),
            path_arguments(root, paths),
            LoreString::from(target.revision().to_string().as_str()),
            ResetOptions::default(),
        )
        .await
    }

    /// The node `state` holds at `path`.
    async fn node_at(
        repository: &Arc<RepositoryContext>,
        state: &state::State,
        path: &str,
    ) -> NodeID {
        state
            .find_node_link(repository.clone(), path)
            .await
            .unwrap_or_else(|_| panic!("The state holds {path}"))
            .node
    }

    /// A tree numbers its nodes apart from another revision's, so a reset to an earlier revision
    /// can find a file it restores at a slot the staged tree gives another file. Here the current
    /// revision deleted `old.txt`, and `new.txt` took its slot, committed or marked as added. The
    /// reset of `old.txt` leaves `new.txt` marked as it was, and still succeeds once `new.txt` is
    /// staged.
    #[tokio::test]
    async fn a_reset_to_another_revision_leaves_the_file_holding_the_restored_files_slot() {
        for committed in [true, false] {
            with_fixture(async move |fixture| {
                let path = |relative: &str| fixture.path.join(relative);
                test_file_write(&path("kept.txt"), b"kept");
                test_file_write(&path("old.txt"), b"old");
                let first = test_commit_tree(&fixture, "First").await;
                std::fs::remove_file(path("old.txt")).expect("Failed to delete file");
                test_commit_tree(&fixture, "Second").await;
                test_file_write(&path("new.txt"), b"new");
                if committed {
                    test_commit_tree(&fixture, "Third").await;
                    test_file_write(&path("new.txt"), b"edited");
                }
                file::dirty::dirty(
                    fixture.repository.clone(),
                    path_arguments(&fixture.path, &["new.txt"]),
                )
                .await
                .expect("Dirty failed");
                assert_eq!(
                    node_at(&fixture.repository, &first, "old.txt").await,
                    node_at(
                        &fixture.repository,
                        &*next_tree(&fixture.repository).await,
                        "new.txt"
                    )
                    .await,
                    "committed {committed}: new.txt takes the slot old.txt held"
                );

                reset_to_revision(&fixture.repository, &fixture.path, &["old.txt"], &first)
                    .await
                    .expect("A reset must succeed");
                let new = next_node(&fixture.repository, "new.txt").await;
                assert!(
                    new.is_dirty() && new.is_dirty_add() != committed,
                    "committed {committed}: the reset changed the marks of new.txt"
                );

                stage_paths(
                    &fixture.repository,
                    &fixture.path,
                    &["new.txt"],
                    false,
                    false,
                )
                .await;
                reset_to_revision(&fixture.repository, &fixture.path, &["old.txt"], &first)
                    .await
                    .expect("A reset of a path nothing staged holds must succeed");
                assert!(next_node(&fixture.repository, "new.txt").await.is_staged());
                let new: &[u8] = if committed { b"edited" } else { b"new" };
                assert_working_files(
                    &fixture,
                    &[("old.txt", Some(b"old")), ("new.txt", Some(new))],
                );
            })
            .await;
        }
    }

    /// Stages the whole working tree below `root` and commits it through `repository`, answering
    /// the state of the revision that produced, as [`test_commit_tree`] does for a fixture.
    async fn commit_tree(
        repository: &Arc<RepositoryContext>,
        root: &Path,
        message: &str,
    ) -> Arc<state::State> {
        stage_paths(repository, root, &["."], true, false).await;
        let revision = commit::commit_boxed(
            repository.clone(),
            repository
                .try_write_token()
                .expect("The repository was opened for writing"),
            CommitOptions::new(message.to_string()),
        )
        .await
        .expect("Commit failed");
        state::State::deserialize(repository.clone(), revision)
            .await
            .expect("Failed to deserialize the committed state")
    }

    /// Moves the instance to `revision` of `branch` without touching the working tree.
    async fn move_instance(repository: &Arc<RepositoryContext>, branch: BranchId, revision: Hash) {
        lore_revision::instance::store_current_anchor_branch(repository, branch)
            .await
            .expect("Failed to store anchor branch");
        lore_revision::instance::store_current_anchor(repository, revision)
            .await
            .expect("Failed to store anchor revision");
    }

    /// A merge numbers the nodes it brings in after the branch merged into: `main` and `feature`
    /// each give their first file the first free slot of the branch point's tree, and the merge
    /// of `main` into `feature` gives `lib.txt` another one. A reset of `lib.txt` to `main`'s
    /// revision, named or as the side of the merge it came from, refuses a staged edit of
    /// `lib.txt`, and not one of `app.txt`, which holds the slot `main` gave `lib.txt`.
    #[tokio::test]
    async fn a_reset_to_a_merged_in_revision_finds_the_staged_file_by_its_path() {
        in_execution(async {
            let (repository, tempdir) = committed_repository("", &[]).await;
            let root = tempdir.to_path_buf();
            let path = |relative: &str| root.join(relative);
            let read = |relative: &str| std::fs::read(path(relative)).expect("Failed to read file");
            let token = || {
                repository
                    .try_write_token()
                    .expect("The repository was opened for writing")
            };
            let anchor = async || {
                lore_revision::instance::load_current_anchor_boxed(&repository)
                    .await
                    .expect("Failed to load current anchor")
            };

            let (base, main_branch) = anchor().await;
            create_branch_on_instance(&repository, "feature", None, "", false)
                .await
                .expect("Branch create failed");
            let feature_branch = anchor().await.1;
            test_file_write(&path("app.txt"), b"app");
            let feature = commit_tree(&repository, &root, "App").await.revision();

            move_instance(&repository, main_branch, base).await;
            std::fs::remove_file(path("app.txt")).expect("Failed to delete file");
            test_file_write(&path("lib.txt"), b"main");
            let main = commit_tree(&repository, &root, "Lib").await;

            move_instance(&repository, feature_branch, feature).await;
            std::fs::remove_file(path("lib.txt")).expect("Failed to delete file");
            test_file_write(&path("app.txt"), b"app");
            LORE_CONTEXT
                .scope(
                    test_execution_with(LoreGlobalArgs::default().set_offline()),
                    Box::pin(branch::merge::merge_start(
                        repository.clone(),
                        token(),
                        main_branch,
                        branch::merge::MergeStartOptions {
                            message: "Merge main".to_string(),
                            link_messages: std::collections::HashMap::new(),
                            no_commit: false,
                            scope: branch::merge::MergeScope::MainOnly,
                            inherit_metadata: Default::default(),
                        },
                    )),
                )
                .await
                .expect("Merge failed");
            let merged = next_tree(&repository).await;
            let lib = node_at(&repository, &main, "lib.txt").await;
            assert_eq!(
                lib,
                node_at(&repository, &merged, "app.txt").await,
                "app.txt holds the slot main gave lib.txt"
            );
            assert_ne!(
                lib,
                node_at(&repository, &merged, "lib.txt").await,
                "the merge gives lib.txt a slot of its own"
            );

            let resets = async || {
                [
                    reset::reset_to_last_merged(
                        repository.clone(),
                        path_arguments(&root, &["lib.txt"]),
                        LoreString::from(branch::DEFAULT_DEFAULT_NAME),
                        ResetMergeSide::ParentOther,
                        ResetOptions::default(),
                    )
                    .await,
                    reset_to_revision(&repository, &root, &["lib.txt"], &main).await,
                ]
            };
            test_file_write(&path("lib.txt"), b"edited");
            stage_paths(&repository, &root, &["lib.txt"], false, false).await;
            for reset in resets().await {
                assert!(
                    reset.is_err_and(|err| err.is_invalid_arguments()),
                    "a reset of a staged file is refused"
                );
            }
            assert_eq!(read("lib.txt"), b"edited");

            file::unstage::unstage(
                repository.clone(),
                token(),
                path_arguments(&root, &["lib.txt"]),
                Default::default(),
            )
            .await
            .expect("Unstage failed");
            test_file_write(&path("app.txt"), b"edited");
            stage_paths(&repository, &root, &["app.txt"], false, false).await;
            for reset in resets().await {
                reset.expect("A reset of a file nothing staged holds must succeed");
            }
            assert_eq!(
                (read("lib.txt"), read("app.txt")),
                (b"main".to_vec(), b"edited".to_vec())
            );
        })
        .await;
    }

    /// A reset to an earlier revision finds each node below a directory it restores in the staged
    /// tree by its path too. Here the current revision deleted `old/`, and `new/` and the file
    /// below it took its slots. The reset of `old/` leaves marked as they were `new/`, the edit
    /// below it and the file marked as added beside the edit.
    #[tokio::test]
    async fn a_reset_to_another_revision_leaves_the_directory_holding_the_restored_slots() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_file_write(&path("kept.txt"), b"kept");
            test_directory_create(&path("old"));
            test_file_write(&path("old/a.txt"), b"old");
            let first = test_commit_tree(&fixture, "First").await;
            std::fs::remove_dir_all(path("old")).expect("Failed to delete directory");
            test_commit_tree(&fixture, "Second").await;
            test_directory_create(&path("new"));
            test_file_write(&path("new/b.txt"), b"new");
            let third = test_commit_tree(&fixture, "Third").await;
            let mut restored = [
                node_at(&fixture.repository, &first, "old").await,
                node_at(&fixture.repository, &first, "old/a.txt").await,
            ];
            let mut holding = [
                node_at(&fixture.repository, &third, "new").await,
                node_at(&fixture.repository, &third, "new/b.txt").await,
            ];
            restored.sort_unstable();
            holding.sort_unstable();
            assert_eq!(restored, holding, "new/ takes the slots old/ held");

            test_file_write(&path("new/b.txt"), b"edited");
            test_file_write(&path("new/a.txt"), b"added");
            file::dirty::dirty(
                fixture.repository.clone(),
                path_arguments(&fixture.path, &["new/b.txt", "new/a.txt"]),
            )
            .await
            .expect("Dirty failed");

            reset_to_revision(&fixture.repository, &fixture.path, &["old"], &first)
                .await
                .expect("A reset must succeed");
            assert_working_files(
                &fixture,
                &[
                    ("old/a.txt", Some(b"old")),
                    ("new/b.txt", Some(b"edited")),
                    ("new/a.txt", Some(b"added")),
                ],
            );
            for (marked, added) in [("new", false), ("new/b.txt", false), ("new/a.txt", true)] {
                let node = next_node(&fixture.repository, marked).await;
                assert!(
                    node.is_dirty() && node.is_dirty_add() == added,
                    "the reset changed the marks of {marked}"
                );
            }
        })
        .await;
    }

    /// A reset to an earlier revision clears the dirty mark of the file the staged tree holds at
    /// each path it restores, which the current revision can hold at another slot than the earlier
    /// one. Here `dir/entry` was deleted, its slot went to `dir/filler`, and `dir/entry` came back
    /// at another slot. A reset naming the file, its directory or the root clears the edit of
    /// `dir/entry` and leaves the edits of `dir/filler` and of `kept.txt`, which the earlier
    /// revision lacks, marked.
    #[tokio::test]
    async fn a_reset_to_another_revision_clears_the_mark_of_the_file_at_the_path_it_restores() {
        for named in ["dir/entry", "dir", ""] {
            with_fixture(async move |fixture| {
                let path = |relative: &str| fixture.path.join(relative);
                test_directory_create(&path("dir"));
                test_file_write(&path("dir/entry"), b"first");
                let first = test_commit_tree(&fixture, "First").await;
                std::fs::remove_file(path("dir/entry")).expect("Failed to delete file");
                test_file_write(&path("kept.txt"), b"kept");
                test_commit_tree(&fixture, "Second").await;
                test_file_write(&path("dir/filler"), b"filler");
                test_commit_tree(&fixture, "Third").await;
                test_file_write(&path("dir/entry"), b"fourth");
                let fourth = test_commit_tree(&fixture, "Fourth").await;
                let entry = node_at(&fixture.repository, &first, "dir/entry").await;
                assert_eq!(
                    entry,
                    node_at(&fixture.repository, &fourth, "dir/filler").await,
                    "dir/filler takes the slot dir/entry held"
                );
                assert_ne!(
                    entry,
                    node_at(&fixture.repository, &fourth, "dir/entry").await,
                    "dir/entry comes back at another slot"
                );

                let edited = ["dir/entry", "dir/filler", "kept.txt"];
                for relative in edited {
                    test_file_write(&path(relative), b"edited");
                }
                file::dirty::dirty(
                    fixture.repository.clone(),
                    path_arguments(&fixture.path, &edited),
                )
                .await
                .expect("Dirty failed");

                reset_to_revision(&fixture.repository, &fixture.path, &[named], &first)
                    .await
                    .expect("A reset must succeed");
                assert_working_files(
                    &fixture,
                    &[
                        ("dir/entry", Some(b"first")),
                        ("dir/filler", Some(b"edited")),
                    ],
                );
                for (relative, marked) in [
                    ("dir/entry", false),
                    ("dir/filler", true),
                    ("kept.txt", true),
                ] {
                    assert_eq!(
                        next_node(&fixture.repository, relative).await.is_dirty(),
                        marked,
                        "a reset of {named:?} marks {relative}"
                    );
                }
            })
            .await;
        }
    }

    /// Marks `from` below the fixture's root moved to `to`, which the working tree holds it at.
    async fn dirty_move(fixture: &TestRepository, from: &str, to: &str) {
        let absolute = |relative: &str| fixture.path.join(relative).to_string_lossy().into_owned();
        file::dirty::dirty_move(fixture.repository.clone(), absolute(from), absolute(to))
            .await
            .expect("Dirty move failed");
    }

    /// The target of each reset a test runs both ways: the first revision, or the current one.
    async fn reset_target(
        fixture: &TestRepository,
        first: &Arc<state::State>,
        to_first: bool,
    ) -> Arc<state::State> {
        if to_first {
            first.clone()
        } else {
            let (current, _) = test_anchor_states(&fixture.repository).await;
            current
        }
    }

    /// A node the staged tree moved away stands at its new path, so a reset of the path it left,
    /// to the current or an earlier revision, leaves its move marked. Here `a.txt` was marked
    /// moved to `b.txt` and a new `a.txt` staged as added in its place, which a reset of `a.txt`
    /// or of the whole tree refuses. Below the root, with the new `dir/a.txt` neither marked nor
    /// staged, a reset of `dir` writes over it.
    #[tokio::test]
    async fn a_reset_leaves_the_move_of_a_node_the_staged_tree_moved_away() {
        for to_first in [true, false] {
            for (named, parent) in [("a.txt", ""), ("", ""), ("dir", "dir/")] {
                with_fixture(async move |fixture| {
                    let path = |relative: &str| fixture.path.join(relative);
                    let (from, to) = (format!("{parent}a.txt"), format!("{parent}b.txt"));
                    test_directory_create(&path("dir"));
                    test_file_write(&path(&from), b"first");
                    test_file_write(&path("kept.txt"), b"kept");
                    let first = test_commit_tree(&fixture, "First").await;
                    test_file_write(&path("other.txt"), b"other");
                    test_commit_tree(&fixture, "Second").await;
                    let target = reset_target(&fixture, &first, to_first).await;

                    std::fs::rename(path(&from), path(&to)).expect("Failed to move file");
                    dirty_move(&fixture, &from, &to).await;
                    test_file_write(&path(&from), b"new");
                    let staged = parent.is_empty();
                    if staged {
                        stage_paths(&fixture.repository, &fixture.path, &[&from], false, false)
                            .await;
                    } else {
                        test_file_write(&path("kept.txt"), b"edited");
                        stage_paths(
                            &fixture.repository,
                            &fixture.path,
                            &["kept.txt"],
                            false,
                            false,
                        )
                        .await;
                    }

                    let reset =
                        reset_to_revision(&fixture.repository, &fixture.path, &[named], &target)
                            .await;
                    let case = format!("a reset of {named:?} to the first revision {to_first}");
                    if staged {
                        assert!(
                            reset.is_err_and(|err| err.is_invalid_arguments()),
                            "{case} over a staged add is refused"
                        );
                    } else {
                        reset.unwrap_or_else(|err| panic!("{case} must succeed, got {err}"));
                    }
                    let written: &[u8] = if staged { b"new" } else { b"first" };
                    assert_working_files(
                        &fixture,
                        &[(&from, Some(written)), (&to, Some(b"first"))],
                    );
                    assert!(
                        next_node(&fixture.repository, &to).await.is_dirty_move(),
                        "{case} cleared the move of {to}"
                    );
                })
                .await;
            }
        }
    }

    /// Marks `name` below the fixture's root deleted, and an added `other` moved onto its name: a
    /// directory holding `inner.txt`, staged, where `staged` says, and a file otherwise.
    async fn replace_with_moved_add(fixture: &TestRepository, name: &str, staged: bool) {
        let path = |relative: &str| fixture.path.join(relative);
        std::fs::remove_file(path(name)).expect("Failed to delete file");
        file::dirty::dirty(
            fixture.repository.clone(),
            path_arguments(&fixture.path, &[name]),
        )
        .await
        .expect("Dirty failed");
        if staged {
            test_directory_create(&path("other"));
            test_file_write(&path("other/inner.txt"), b"inner");
            stage_paths(
                &fixture.repository,
                &fixture.path,
                &["other/inner.txt"],
                false,
                false,
            )
            .await;
        } else {
            test_file_write(&path("other"), b"other");
            file::dirty::dirty(
                fixture.repository.clone(),
                path_arguments(&fixture.path, &["other"]),
            )
            .await
            .expect("Dirty failed");
        }
        std::fs::rename(path("other"), path(name)).expect("Failed to move the add");
        dirty_move(fixture, "other", name).await;
        assert_eq!(
            children_named(
                &fixture.repository,
                &next_tree(&fixture.repository).await,
                ROOT_NODE,
                name
            )
            .await
            .len(),
            2,
            "the staged tree holds {name} deleted and the add moved onto its name"
        );
    }

    /// A reset discards the add that took the place of a node marked deleted, unless the add is
    /// staged. Here `x` was marked deleted, and an added directory, with a file below it staged,
    /// marked moved onto its name. A reset of `x` or of the whole tree, to the current or an
    /// earlier revision, refuses it and leaves the directory as it is.
    #[tokio::test]
    async fn a_reset_refuses_a_staged_add_that_took_the_place_of_a_deleted_node() {
        for to_first in [true, false] {
            for named in ["x", ""] {
                with_fixture(async move |fixture| {
                    let path = |relative: &str| fixture.path.join(relative);
                    test_file_write(&path("x"), b"first");
                    let first = test_commit_tree(&fixture, "First").await;
                    test_file_write(&path("other.txt"), b"other");
                    test_commit_tree(&fixture, "Second").await;
                    let target = reset_target(&fixture, &first, to_first).await;
                    replace_with_moved_add(&fixture, "x", true).await;

                    assert!(
                        reset_to_revision(&fixture.repository, &fixture.path, &[named], &target)
                            .await
                            .is_err_and(|err| err.is_invalid_arguments()),
                        "a reset of {named:?} to the first revision {to_first} is refused"
                    );
                    assert_working_files(&fixture, &[("x/inner.txt", Some(b"inner"))]);
                    assert!(
                        next_node(&fixture.repository, "x/inner.txt")
                            .await
                            .is_staged()
                    );
                })
                .await;
            }
        }
    }

    /// A reset to an earlier revision discards the add that took the place of the current
    /// revision's node at a path it restores, beside that node rather than at the slot the earlier
    /// revision holds the path at. Here `entry` was deleted, its slot went to `filler`, and `entry`
    /// came back at another slot, which an added file marked moved onto its name then replaced. A
    /// reset naming `entry` or the root leaves the current revision's `entry` clean and alone under
    /// its name.
    #[tokio::test]
    async fn a_reset_to_another_revision_discards_the_add_replacing_the_current_node() {
        for named in ["entry", ""] {
            with_fixture(async move |fixture| {
                let path = |relative: &str| fixture.path.join(relative);
                test_file_write(&path("entry"), b"first");
                let first = test_commit_tree(&fixture, "First").await;
                std::fs::remove_file(path("entry")).expect("Failed to delete file");
                test_commit_tree(&fixture, "Second").await;
                test_file_write(&path("filler"), b"filler");
                test_commit_tree(&fixture, "Third").await;
                test_file_write(&path("entry"), b"fourth");
                let fourth = test_commit_tree(&fixture, "Fourth").await;
                let entry = node_at(&fixture.repository, &fourth, "entry").await;
                assert_ne!(
                    node_at(&fixture.repository, &first, "entry").await,
                    entry,
                    "entry comes back at another slot"
                );
                replace_with_moved_add(&fixture, "entry", false).await;

                reset_to_revision(&fixture.repository, &fixture.path, &[named], &first)
                    .await
                    .expect("A reset must succeed");
                assert_working_files(&fixture, &[("entry", Some(b"first"))]);
                assert_eq!(
                    children_named(
                        &fixture.repository,
                        &next_tree(&fixture.repository).await,
                        ROOT_NODE,
                        "entry"
                    )
                    .await,
                    vec![entry],
                    "a reset of {named:?} left the add beside entry"
                );
                assert!(
                    !next_node(&fixture.repository, "entry")
                        .await
                        .is_dirty_or_staged(),
                    "a reset of {named:?} left entry marked"
                );
            })
            .await;
        }
    }

    /// The node a revision holds at an id below one directory is no child of another, so a reset
    /// to an earlier revision leaves alone a file a later revision moved out of the directory it
    /// restores. Here `b/x` was moved to `a/x` and committed, then edited and marked, or marked
    /// moved back to `b/x`. The reset of `b` leaves its marks.
    #[tokio::test]
    async fn a_reset_to_another_revision_leaves_a_file_moved_out_of_the_directory_it_restores() {
        for moved_back in [false, true] {
            with_fixture(async move |fixture| {
                let path = |relative: &str| fixture.path.join(relative);
                test_directory_create(&path("a"));
                test_directory_create(&path("b"));
                test_file_write(&path("a/kept.txt"), b"kept");
                test_file_write(&path("b/kept.txt"), b"kept");
                test_file_write(&path("b/x"), b"first");
                let first = test_commit_tree(&fixture, "First").await;
                std::fs::rename(path("b/x"), path("a/x")).expect("Failed to move file");
                file::stage::stage_move(
                    fixture.repository.clone(),
                    &fixture.write_token,
                    path("b/x").to_string_lossy().into_owned(),
                    path("a/x").to_string_lossy().into_owned(),
                    StageOptions::default(),
                )
                .await
                .expect("Stage move failed");
                let second = test_commit(&fixture, "Second").await;
                assert_eq!(
                    node_at(&fixture.repository, &first, "b/x").await,
                    node_at(&fixture.repository, &second, "a/x").await,
                    "the move keeps the node"
                );

                let marked = if moved_back {
                    std::fs::rename(path("a/x"), path("b/x")).expect("Failed to move file");
                    dirty_move(&fixture, "a/x", "b/x").await;
                    "b/x"
                } else {
                    test_file_write(&path("a/x"), b"edited");
                    file::dirty::dirty(
                        fixture.repository.clone(),
                        path_arguments(&fixture.path, &["a/x"]),
                    )
                    .await
                    .expect("Dirty failed");
                    "a/x"
                };
                test_file_write(&path("a/kept.txt"), b"edited");
                stage_paths(
                    &fixture.repository,
                    &fixture.path,
                    &["a/kept.txt"],
                    false,
                    false,
                )
                .await;

                reset_to_revision(&fixture.repository, &fixture.path, &["b"], &first)
                    .await
                    .expect("A reset must succeed");
                let node = next_node(&fixture.repository, marked).await;
                assert!(
                    node.is_dirty() && node.is_dirty_move() == moved_back,
                    "the reset of b changed the marks of {marked}"
                );
            })
            .await;
        }
    }

    /// A reset of the whole tree to an earlier revision refuses a file staged as added that the
    /// revision holds and the current revision does not, whichever slot the add took, and leaves
    /// the file as it is. Here the slot the revision holds the file at went to a file marked as
    /// added before it.
    #[tokio::test]
    async fn a_reset_of_the_tree_to_another_revision_refuses_a_staged_add_the_current_one_lacks() {
        with_fixture(async |fixture| {
            let path = |relative: &str| fixture.path.join(relative);
            test_file_write(&path("kept.txt"), b"kept");
            test_file_write(&path("x.txt"), b"committed");
            let first = test_commit_tree(&fixture, "First").await;
            std::fs::remove_file(path("x.txt")).expect("Failed to delete file");
            test_commit_tree(&fixture, "Second").await;
            test_file_write(&path("y.txt"), b"marked");
            file::dirty::dirty(
                fixture.repository.clone(),
                path_arguments(&fixture.path, &["y.txt"]),
            )
            .await
            .expect("Dirty failed");
            test_file_write(&path("x.txt"), b"staged");
            stage_paths(&fixture.repository, &fixture.path, &["x.txt"], false, false).await;
            assert_eq!(
                node_at(&fixture.repository, &first, "x.txt").await,
                node_at(
                    &fixture.repository,
                    &*next_tree(&fixture.repository).await,
                    "y.txt"
                )
                .await,
                "y.txt takes the slot x.txt held"
            );

            for named in ["", "x.txt"] {
                assert!(
                    reset_to_revision(&fixture.repository, &fixture.path, &[named], &first)
                        .await
                        .is_err_and(|err| err.is_invalid_arguments()),
                    "a reset of {named:?} over a staged add is refused"
                );
                assert_working_files(&fixture, &[("x.txt", Some(b"staged"))]);
            }
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
