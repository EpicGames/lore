// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! A filesystem diff driven by the paths an SWFS instance reports as modified, held to the answer
//! of the walk over the whole tree.

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::change::NodeChange;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::filter::FilterMode;
    use lore_revision::filter::FilterStates;
    use lore_revision::fs::filesystem_provider::FilesystemDiffContext;
    use lore_revision::fs::filesystem_provider::FilesystemDiffIntent;
    use lore_revision::fs::filesystem_provider::FilesystemDiffTree;
    use lore_revision::fs::filesystem_provider::FilesystemProvider;
    use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
    use lore_revision::fs::swfs::modified_tree::Change;
    use lore_revision::fs::swfs::modified_tree::ModifiedTree;
    use lore_revision::fs::swfs::record_diff::answers;
    use lore_revision::fs::swfs::record_diff::changes_from_record;
    use lore_revision::fs::swfs::record_diff::with_ancestors;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::stage::StageOptions;
    use lore_revision::state::FilesystemDiffStats;
    use lore_revision::state::LayerMountInfo;
    use lore_revision::state::LinkMountInfo;
    use lore_revision::state::NodeMapping;
    use lore_revision::state::State;
    use lore_revision::util::path::RelativePath;

    include!("helper.rs");

    /// The paths a diff from `record` walks: those it lists, and the directories leading to them,
    /// which name a created directory the list leaves out.
    async fn candidates_of(record: &ModifiedTree) -> Vec<RelativePath> {
        with_ancestors(record.paths().await.expect("listing modified paths"))
    }

    fn reported(changes: &mut [NodeChange]) -> Vec<(String, String)> {
        lore_revision::change::sort_by_path(changes);
        test_reported(changes)
    }

    /// Files a diff compared or found, which is the measure of what it read.
    fn files_examined(stats: &FilesystemDiffStats) -> u64 {
        [
            &stats.file_add,
            &stats.file_delete,
            &stats.file_retain,
            &stats.file_replace,
        ]
        .into_iter()
        .map(|count| count.load(Ordering::Relaxed))
        .sum()
    }

    /// The changes of the whole tree, or of just `paths`, against the committed state.
    async fn read_changes(
        repository: &Arc<RepositoryContext>,
        paths: Option<Vec<RelativePath>>,
    ) -> (Vec<NodeChange>, FilesystemDiffStats) {
        let (changes, stats, _) =
            read_changes_with(repository, paths, FilesystemDiffIntent::Report).await;
        (changes, stats)
    }

    /// [`read_changes`] under `intent`, and the staged state it left.
    async fn read_changes_with(
        repository: &Arc<RepositoryContext>,
        paths: Option<Vec<RelativePath>>,
        intent: FilesystemDiffIntent,
    ) -> (Vec<NodeChange>, FilesystemDiffStats, Arc<State>) {
        read_changes_via(repository, paths, intent, false).await
    }

    /// [`read_changes_with`], the walk listing the tree through the operation's own reads where
    /// `through_operation` says, as it does for an operation that answers for the tree itself.
    async fn read_changes_via(
        repository: &Arc<RepositoryContext>,
        paths: Option<Vec<RelativePath>>,
        intent: FilesystemDiffIntent,
        through_operation: bool,
    ) -> (Vec<NodeChange>, FilesystemDiffStats, Arc<State>) {
        let (current, staged) = test_anchor_states(repository).await;
        let staged_after = staged.clone();
        let operation = FilesystemProvider::begin_operation(repository.file_system().as_ref())
            .await
            .expect("Failed to start filesystem operation");
        operation.set_listing_through_operation(through_operation);
        let from = FilesystemDiffTree {
            repository: repository.clone(),
            state: staged,
        };
        let current = FilesystemDiffTree {
            repository: repository.clone(),
            state: current,
        };
        let mut stream = match paths {
            Some(paths) => lore_revision::state::diff_filesystem_listed(
                &operation,
                from,
                current,
                paths,
                FilterMode::Full,
                intent,
                Arc::new(Vec::new()),
            )
            .await
            .expect("Failed to diff the listed paths"),
            None => lore_revision::state::diff_filesystem(
                &operation,
                from,
                current,
                None,
                FilterMode::Full,
                intent,
                Arc::new(Vec::new()),
            )
            .await
            .expect("Failed to diff filesystem"),
        };
        let mut changes = Vec::new();
        while let Some(change) = stream.next().await {
            changes.push(change);
        }
        let stats = stream.finish().await.expect("Failed to diff filesystem");
        // The listing came through the operation exactly where it was asked to.
        assert_eq!(
            operation.directory_reads() > 0,
            through_operation,
            "the walk listed {} directories through the operation",
            operation.directory_reads()
        );
        operation
            .finalize()
            .await
            .expect("Failed to finish filesystem operation");
        (changes, stats, staged_after)
    }

    /// Six directories of four files, committed.
    async fn commit_tree(fixture: &TestRepository) {
        for directory in 0..6 {
            let directory = fixture.path.join(format!("dir{directory}"));
            std::fs::create_dir_all(&directory).expect("Create directory failed");
            for file in 0..4 {
                test_file_write(
                    directory.join(format!("file{file}.txt")).as_path(),
                    format!("contents {file}").as_bytes(),
                );
            }
        }
        file::stage::stage(
            fixture.repository.clone(),
            &fixture.write_token,
            LoreArray::from_vec(vec![LoreString::from(&fixture.path)]),
            StageOptions {
                scan: true,
                ..Default::default()
            },
        )
        .await
        .expect("Failed to stage the fixture");
        commit::commit_boxed(
            fixture.repository.clone(),
            &fixture.write_token,
            CommitOptions::new("Initial".to_string()),
        )
        .await
        .expect("Commit failed");
    }

    /// An edit of each kind: a modification, a deletion, an add beside tracked files, and an add
    /// of a new directory holding a file. Returns what SWFS would have reported for them.
    async fn edit_tree(fixture: &TestRepository) -> ModifiedTree {
        let path = &fixture.path;
        test_file_write(path.join("dir0").join("file0.txt").as_path(), b"edited");
        std::fs::remove_file(path.join("dir1").join("file1.txt")).expect("Remove file failed");
        test_file_write(path.join("dir2").join("added.txt").as_path(), b"new");
        std::fs::create_dir_all(path.join("dir3").join("subdir")).expect("Create directory failed");
        test_file_write(
            path.join("dir3").join("subdir").join("added.txt").as_path(),
            b"new",
        );

        let record = ModifiedTree::new(fixture.repository.clone());
        record
            .record(Change::Written("dir0/file0.txt".into()))
            .await;
        record
            .record(Change::Deleted {
                path: "dir1/file1.txt".into(),
                directory: false,
            })
            .await;
        record
            .record(Change::Created {
                path: "dir2/added.txt".into(),
                directory: false,
            })
            .await;
        record
            .record(Change::Created {
                path: "dir3/subdir".into(),
                directory: true,
            })
            .await;
        record
            .record(Change::Created {
                path: "dir3/subdir/added.txt".into(),
                directory: false,
            })
            .await;
        record
    }

    #[tokio::test]
    async fn listed_paths_report_what_the_whole_tree_walk_reports() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        LORE_CONTEXT
            .scope(execution, async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                commit_tree(&fixture).await;
                let record = edit_tree(&fixture).await;
                let candidates = candidates_of(&record).await;

                let (mut whole, whole_stats) = read_changes(&fixture.repository, None).await;
                let (mut listed, listed_stats) =
                    read_changes(&fixture.repository, Some(candidates)).await;

                assert!(!whole.is_empty(), "the edits must report changes");
                assert_eq!(reported(&mut listed), reported(&mut whole));
                assert!(
                    files_examined(&listed_stats) < files_examined(&whole_stats),
                    "the listed diff examined {} files, the whole walk {}",
                    files_examined(&listed_stats),
                    files_examined(&whole_stats),
                );
            })
            .await;
    }

    /// A record may name a file that is not modified, such as one written back to its original
    /// content, which costs a comparison and reports nothing.
    #[tokio::test]
    async fn a_listed_file_that_is_not_modified_reports_nothing() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        LORE_CONTEXT
            .scope(execution, async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                commit_tree(&fixture).await;
                let record = ModifiedTree::new(fixture.repository.clone());
                record
                    .record(Change::Written("dir4/file2.txt".into()))
                    .await;
                let candidates = candidates_of(&record).await;

                let (listed, _) = read_changes(&fixture.repository, Some(candidates)).await;

                assert!(listed.is_empty(), "reported {:?}", test_reported(&listed));
            })
            .await;
    }

    /// A directory deleted whole, and a file renamed within its directory.
    #[tokio::test]
    async fn a_deleted_directory_and_a_rename_report_what_the_whole_tree_walk_reports() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        LORE_CONTEXT
            .scope(execution, async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                commit_tree(&fixture).await;
                std::fs::remove_dir_all(fixture.path.join("dir4")).expect("Remove directory");
                std::fs::rename(
                    fixture.path.join("dir5").join("file0.txt"),
                    fixture.path.join("dir5").join("moved.txt"),
                )
                .expect("Rename file");
                let record = ModifiedTree::new(fixture.repository.clone());
                record
                    .record(Change::Deleted {
                        path: "dir4".into(),
                        directory: true,
                    })
                    .await;
                record
                    .record(Change::Moved {
                        from: "dir5/file0.txt".into(),
                        to: "dir5/moved.txt".into(),
                        directory: false,
                    })
                    .await;
                let candidates = candidates_of(&record).await;

                let (mut whole, _) = read_changes(&fixture.repository, None).await;
                let (mut listed, _) = read_changes(&fixture.repository, Some(candidates)).await;

                assert!(!whole.is_empty(), "the edits must report changes");
                assert_eq!(reported(&mut listed), reported(&mut whole));
            })
            .await;
    }

    /// A scan marks what it finds on the staged state, which is what the status that follows it
    /// reads.
    #[tokio::test]
    async fn listed_paths_mark_the_staged_state_as_the_whole_tree_walk_does() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        LORE_CONTEXT
            .scope(execution, async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                commit_tree(&fixture).await;
                let record = edit_tree(&fixture).await;
                let candidates = candidates_of(&record).await;

                let (_, _, whole) =
                    read_changes_with(&fixture.repository, None, FilesystemDiffIntent::MarkDirty)
                        .await;
                let (_, _, listed) = read_changes_with(
                    &fixture.repository,
                    Some(candidates),
                    FilesystemDiffIntent::MarkDirty,
                )
                .await;

                for path in [
                    "dir0/file0.txt",
                    "dir1/file1.txt",
                    "dir2/added.txt",
                    "dir3/subdir",
                    "dir3/subdir/added.txt",
                    "dir4/file0.txt",
                ] {
                    let flags = |state: &Arc<State>| {
                        let state = state.clone();
                        let repository = fixture.repository.clone();
                        async move {
                            state
                                .find_node(repository, path)
                                .await
                                .map(|node| node.flags)
                                .ok()
                        }
                    };
                    assert_eq!(
                        flags(&listed).await,
                        flags(&whole).await,
                        "{path} is marked differently"
                    );
                }
            })
            .await;
    }

    fn whole_tree_context(
        operation: &Arc<InstanceOperationImpl>,
        repository: &Arc<RepositoryContext>,
        staged: Arc<State>,
        current: Arc<State>,
    ) -> FilesystemDiffContext {
        FilesystemDiffContext {
            operation: operation.clone(),
            from: NodeMapping::root(repository.clone(), staged),
            current: NodeMapping::root(repository.clone(), current),
            filesystem_path: RelativePath::new(),
            states: FilterStates::ROOT,
            from_states: FilterStates::ROOT,
            named_path: None,
            filter_mode: FilterMode::Full,
            intent: FilesystemDiffIntent::Report,
            layer_mounts: Arc::new(Vec::new()),
            link_mounts: Arc::new(Vec::new()),
            discards: None,
        }
    }

    /// The whole-tree diff of the committed state, answered from `record`.
    async fn read_recorded_changes(
        repository: &Arc<RepositoryContext>,
        record: &Arc<ModifiedTree>,
    ) -> (Vec<NodeChange>, FilesystemDiffStats) {
        let (current, staged) = test_anchor_states(repository).await;
        let operation = FilesystemProvider::begin_operation(repository.file_system().as_ref())
            .await
            .expect("Failed to start filesystem operation");
        let mut stream = changes_from_record(
            record.clone(),
            whole_tree_context(&operation, repository, staged, current),
        );
        let mut changes = Vec::new();
        while let Some(change) = stream.next().await {
            changes.push(change);
        }
        let stats = stream
            .finish()
            .await
            .expect("Failed to diff from the record");
        operation
            .finalize()
            .await
            .expect("Failed to finish filesystem operation");
        (changes, stats)
    }

    /// An empty record measured from the instance's current revision, as a mount's is once it has
    /// loaded the instance's state: a diff answers from a record only against the revision it
    /// measures from.
    async fn record_at_current_revision(repository: &Arc<RepositoryContext>) -> Arc<ModifiedTree> {
        let record = Arc::new(ModifiedTree::new(repository.clone()));
        let (current, _) = test_anchor_states(repository).await;
        record.observe_revision(current.revision()).await;
        record
    }

    /// A trusted record, as a rebuild from SWFS's list leaves it, is all a diff needs to find, so
    /// it does not walk the tree.
    #[tokio::test]
    async fn a_trusted_record_answers_without_walking_the_tree() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        LORE_CONTEXT
            .scope(execution, async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                commit_tree(&fixture).await;
                let record = record_at_current_revision(&fixture.repository).await;
                record.set_trusted(true);

                test_file_write(
                    fixture.path.join("dir2").join("file3.txt").as_path(),
                    b"edited later",
                );
                record
                    .record(Change::Written("dir2/file3.txt".into()))
                    .await;

                let (mut whole, whole_stats) = read_changes(&fixture.repository, None).await;
                let (mut recorded, recorded_stats) =
                    read_recorded_changes(&fixture.repository, &record).await;

                assert_eq!(reported(&mut recorded), reported(&mut whole));
                assert!(
                    files_examined(&recorded_stats) < files_examined(&whole_stats),
                    "the recorded diff examined {} files, the whole walk {}",
                    files_examined(&recorded_stats),
                    files_examined(&whole_stats),
                );
            })
            .await;
    }

    /// A record a callback distrusted during the operation is diffed as it stands: what the
    /// callback could not record came after the freeze, and a walk of the tree, through the mount
    /// on a mounted instance, is what can hang the service.
    #[tokio::test]
    async fn an_untrusted_record_is_diffed_as_it_stands_without_walking_the_tree() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        LORE_CONTEXT
            .scope(execution, async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                commit_tree(&fixture).await;
                let record = record_at_current_revision(&fixture.repository).await;
                test_file_write(
                    fixture.path.join("dir2").join("file3.txt").as_path(),
                    b"recorded",
                );
                record
                    .record(Change::Written("dir2/file3.txt".into()))
                    .await;
                // An edit the record does not hold.
                test_file_write(
                    fixture.path.join("dir0").join("file0.txt").as_path(),
                    b"not recorded",
                );
                assert!(!record.is_trusted());

                let (_, whole_stats) = read_changes(&fixture.repository, None).await;
                let (mut recorded, recorded_stats) =
                    read_recorded_changes(&fixture.repository, &record).await;

                let mut paths: Vec<String> = recorded
                    .iter_mut()
                    .map(|change| change.path().as_str().to_owned())
                    .collect();
                paths.sort();
                assert_eq!(paths, vec!["dir2/file3.txt"]);
                assert!(
                    files_examined(&recorded_stats) < files_examined(&whole_stats),
                    "the recorded diff examined {} files, the whole walk {}",
                    files_examined(&recorded_stats),
                    files_examined(&whole_stats),
                );
                assert!(!record.is_trusted());
            })
            .await;
    }

    /// The record covers the whole tree as the filter that skips ignored paths sees it, so a diff
    /// of anything else is left to the walk.
    #[tokio::test]
    async fn the_record_answers_only_a_whole_tree_diff_that_skips_ignored_paths() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        LORE_CONTEXT
            .scope(execution, async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                commit_tree(&fixture).await;
                let (current, staged) = test_anchor_states(&fixture.repository).await;
                let operation =
                    FilesystemProvider::begin_operation(fixture.repository.file_system().as_ref())
                        .await
                        .expect("Failed to start filesystem operation");
                let whole = || {
                    whole_tree_context(
                        &operation,
                        &fixture.repository,
                        staged.clone(),
                        current.clone(),
                    )
                };

                assert!(answers(&whole()));

                let mut below = whole();
                below.filesystem_path = RelativePath::new_from_initial_path("dir0").unwrap();
                assert!(!answers(&below), "a diff of one directory");

                let mut viewed = whole();
                viewed.filter_mode = FilterMode::View;
                assert!(!answers(&viewed), "a diff that keeps ignored paths in");

                let mut layered = whole();
                layered.layer_mounts = Arc::new(vec![LayerMountInfo {
                    mount_path: "dir1".to_string(),
                    repository: fixture.repository.clone(),
                    state: staged.clone(),
                    source_node: 0,
                }]);
                assert!(!answers(&layered), "a diff across a layer");

                // A path below a link resolves through the link's pin, as the record's own paths
                // do, so the record answers a diff that crosses one.
                let mut linked = whole();
                linked.link_mounts = Arc::new(vec![LinkMountInfo {
                    mount_path: RelativePath::new_from_initial_path("dir1").unwrap(),
                }]);
                assert!(answers(&linked), "a diff across a link");

                operation
                    .finalize()
                    .await
                    .expect("Failed to finish operation");
            })
            .await;
    }

    /// An operation that lists the tree through its own reads reports what the walk of the file
    /// system reports, for the whole tree and for the paths a record lists, as reports and as the
    /// flags left on the staged state.
    #[tokio::test]
    async fn listing_through_the_operation_reports_what_the_direct_walk_reports() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        LORE_CONTEXT
            .scope(execution, async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                commit_tree(&fixture).await;
                let record = edit_tree(&fixture).await;
                std::fs::remove_dir_all(fixture.path.join("dir4")).expect("Remove directory");
                std::fs::rename(
                    fixture.path.join("dir5").join("file0.txt"),
                    fixture.path.join("dir5").join("moved.txt"),
                )
                .expect("Rename file");
                let candidates = candidates_of(&record).await;

                for intent in [
                    FilesystemDiffIntent::Report,
                    FilesystemDiffIntent::MarkDirty,
                ] {
                    let (mut direct, _, direct_state) =
                        read_changes_via(&fixture.repository, None, intent, false).await;
                    let (mut through, _, through_state) =
                        read_changes_via(&fixture.repository, None, intent, true).await;
                    assert!(!direct.is_empty(), "the edits must report changes");
                    assert_eq!(reported(&mut through), reported(&mut direct));
                    for path in [
                        "dir0/file0.txt",
                        "dir1/file1.txt",
                        "dir2/added.txt",
                        "dir3/subdir",
                        "dir3/subdir/added.txt",
                        "dir4",
                        "dir5/file0.txt",
                        "dir5/moved.txt",
                    ] {
                        let flags = |state: &Arc<State>| {
                            let state = state.clone();
                            let repository = fixture.repository.clone();
                            async move {
                                state
                                    .find_node(repository, path)
                                    .await
                                    .map(|node| node.flags)
                                    .ok()
                            }
                        };
                        assert_eq!(
                            flags(&through_state).await,
                            flags(&direct_state).await,
                            "{path} is marked differently"
                        );
                    }
                }

                let (mut direct, _, _) = read_changes_via(
                    &fixture.repository,
                    Some(candidates.clone()),
                    FilesystemDiffIntent::Report,
                    false,
                )
                .await;
                let (mut through, _, _) = read_changes_via(
                    &fixture.repository,
                    Some(candidates),
                    FilesystemDiffIntent::Report,
                    true,
                )
                .await;
                assert_eq!(reported(&mut through), reported(&mut direct));
            })
            .await;
    }
}
