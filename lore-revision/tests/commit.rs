// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::io::Write;
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_revision::branch;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::BranchId;
    use lore_revision::lore::RepositoryId;
    use lore_revision::metadata::Metadata;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;
    use lore_revision::state;
    use lore_revision::util::path::RelativePath;

    include!("helper.rs");

    #[tokio::test]
    async fn commit_metadata() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                std::fs::create_dir_all(path.as_path()).expect("Create directory failed");

                let default_branch_id = BranchId::from(uuid::Uuid::now_v7());
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
                        default_repository_creation_args(immutable_store, mutable_store)
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

                let file_path = path.as_path().join("test.file");
                {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .read(true)
                        .write(true)
                        .open(file_path.as_path())
                        .expect("Failed to create test file");
                    file.write_all(&[0, 1, 2, 3, 4])
                        .expect("Failed to write test file");
                }

                let _signature = file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    StageOptions {
                        case_change: stage::StageCaseChange::Error,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: true,
                    },
                )
                .await
                .expect("Failed to stage file");

                let options = CommitOptions {
                    message: String::new(),
                    link_messages: std::collections::HashMap::new(),
                    link: None,
                    layer_messages: std::collections::HashMap::new(),
                    layer: None,
                };
                let signature = commit::commit_boxed(repository.clone(), &write_token, options)
                    .await
                    .expect("Failed to commit revision");

                // Load the initial state and verify it has one node
                let state = state::State::deserialize(repository.clone(), signature)
                    .await
                    .expect("Failed to deserialize initial staged state");
                let tree = state
                    .tree(repository.clone())
                    .await
                    .expect("Failed to deserialize tree");
                assert_eq!(tree.block_count, 1);

                let block = state
                    .block(repository.clone(), 0)
                    .await
                    .expect("Failed to deserialize block");
                assert!(block.node(0).child().is_some());

                // Load metadata and verify branch was set correctly
                let metadata = Metadata::deserialize(repository.clone(), state.metadata_hash())
                    .await
                    .expect("Failed to deserialize metadata");

                let branch_id = metadata
                    .get_branch()
                    .expect("Failed to get branch from metadata");

                assert_eq!(branch_id, default_branch_id);
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn commit_emits_revision_commit_event_on_success() {
        use std::sync::Mutex;

        use lore_revision::event::LoreEvent;
        use lore_revision::interface::ExecutionContext;
        use lore_revision::interface::LoreGlobalArgs;
        use lore_revision::relay::EventDispatcher;

        let events: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let callback: lore_revision::interface::LoreEventCallback = Some(Box::new(move |event| {
            captured.lock().expect("lock").push(event.discriminant());
        }));

        let (immutable_store, mutable_store, _unused_execution) =
            test_store_create().await.expect("Failed to create stores");
        let execution = Arc::new(ExecutionContext::new_client_with_user_id(
            LoreGlobalArgs::default(),
            EventDispatcher::new(callback),
            "test-user".to_string(),
        ));
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        let events_for_drain = events.clone();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                std::fs::create_dir_all(path.as_path()).expect("Create directory failed");

                let default_branch_id = BranchId::from(uuid::Uuid::now_v7());
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
                        default_repository_creation_args(immutable_store, mutable_store)
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

                let file_path = path.as_path().join("test.file");
                {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .read(true)
                        .write(true)
                        .open(file_path.as_path())
                        .expect("Failed to create test file");
                    file.write_all(&[9, 8, 7, 6, 5])
                        .expect("Failed to write test file");
                }

                file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    StageOptions {
                        case_change: stage::StageCaseChange::Error,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: true,
                    },
                )
                .await
                .expect("Failed to stage file");

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
            }))
            .await
            .expect("Test task failed");

        // Drop the execution context so its dispatcher closes and the forwarder
        // task drains the channel.
        drop(execution);
        // Give the forwarder a chance to dispatch outstanding events.
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if !events_for_drain.lock().expect("lock").is_empty() {
                break;
            }
        }

        let captured = events_for_drain.lock().expect("lock").clone();
        // RevisionCommitRevision discriminant — match by constructing one and
        // comparing discriminants.
        let commit_event = LoreEvent::RevisionCommitRevision(
            lore_revision::commit::LoreRevisionCommitRevisionEventData {
                repository: RepositoryId::default(),
                branch: BranchId::default(),
                revision: Default::default(),
                revision_number: 0,
                parent: Default::default(),
                parent_other: Default::default(),
            },
        );
        let commit_discriminant = commit_event.discriminant();
        assert!(
            captured.contains(&commit_discriminant),
            "expected RevisionCommitRevision event (discriminant {commit_discriminant}) in captured {captured:?}"
        );
    }

    /// A commit reads and hashes every file it commits. Recording the modified
    /// time it read them at is what lets the next stage answer "unmodified" from
    /// metadata; without it `is_file_modified` finds no cached time, and hashes
    /// the whole tree again to reach the answer the commit already had.
    #[tokio::test]
    async fn commit_records_the_modified_time_of_what_it_committed() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                std::fs::create_dir_all(path.as_path()).expect("Create directory failed");

                let default_branch_id = BranchId::from(uuid::Uuid::now_v7());
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
                        default_repository_creation_args(immutable_store, mutable_store)
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

                let relative_path = RelativePath::new_from_initial_path("recorded.file")
                    .expect("valid relative path");
                let file_path = path.as_path().join("recorded.file");
                {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .write(true)
                        .open(file_path.as_path())
                        .expect("Failed to create test file");
                    file.write_all(b"content the commit will hash")
                        .expect("Failed to write test file");
                }
                // Written before the commit reads the filesystem clock, so its time is one no
                // later write can share.
                set_time(
                    file_path.as_path(),
                    std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
                );

                // Nothing has looked at this file yet, so nothing can have cached
                // a time for it.
                assert_eq!(
                    state::file_modified_time(repository.clone(), &relative_path).await,
                    0,
                    "expected no cached modified time before the commit"
                );

                let _signature = file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    StageOptions {
                        case_change: stage::StageCaseChange::Error,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: true,
                    },
                )
                .await
                .expect("Failed to stage file");

                let options = CommitOptions {
                    message: String::new(),
                    link_messages: std::collections::HashMap::new(),
                    link: None,
                    layer_messages: std::collections::HashMap::new(),
                    layer: None,
                };
                commit::commit_boxed(repository.clone(), &write_token, options)
                    .await
                    .expect("Failed to commit revision");

                let metadata =
                    std::fs::metadata(file_path.as_path()).expect("Failed to stat test file");
                let expected = lore_revision::util::fs::file_mtime(&metadata);
                assert_ne!(expected, 0, "test file has no usable modified time");
                assert_eq!(
                    state::file_modified_time(repository.clone(), &relative_path).await,
                    expected,
                    "commit did not record the modified time it read the file at"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A dry run reads and fragments every staged file but never makes its revision the
    /// current one. Recording the times it read them at would let a later scan answer
    /// "unmodified" about a node the working copy is not on, so nothing is recorded.
    #[tokio::test]
    async fn a_dry_run_commit_records_no_modified_time() {
        use lore_revision::interface::ExecutionContext;
        use lore_revision::interface::LoreGlobalArgs;
        use lore_revision::relay::EventDispatcher;

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                std::fs::create_dir_all(path.as_path()).expect("Create directory failed");

                let default_branch_id = BranchId::from(uuid::Uuid::now_v7());
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
                        default_repository_creation_args(immutable_store, mutable_store)
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

                let relative_path =
                    RelativePath::new_from_initial_path("dry.file").expect("valid relative path");
                let file_path = path.as_path().join("dry.file");
                {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .write(true)
                        .open(file_path.as_path())
                        .expect("Failed to create test file");
                    file.write_all(b"content the dry run will hash")
                        .expect("Failed to write test file");
                }

                let _signature = file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    StageOptions {
                        case_change: stage::StageCaseChange::Error,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: true,
                    },
                )
                .await
                .expect("Failed to stage file");

                let options = CommitOptions {
                    message: String::new(),
                    link_messages: std::collections::HashMap::new(),
                    link: None,
                    layer_messages: std::collections::HashMap::new(),
                    layer: None,
                };
                let callback: lore_revision::interface::LoreEventCallback = None;
                let dry_run = Arc::new(ExecutionContext::new_client_with_user_id(
                    LoreGlobalArgs {
                        dry_run: 1,
                        ..Default::default()
                    },
                    EventDispatcher::new(callback),
                    "test-user".to_string(),
                ));
                LORE_CONTEXT
                    .scope(
                        dry_run,
                        commit::commit_boxed(repository.clone(), &write_token, options),
                    )
                    .await
                    .expect("Failed to commit revision");

                assert_eq!(
                    state::file_modified_time(repository.clone(), &relative_path).await,
                    0,
                    "a dry run recorded a modified time"
                );
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn commit_without_staged_changes_emits_no_commit_event() {
        use std::sync::Mutex;

        use lore_revision::event::LoreEvent;
        use lore_revision::interface::ExecutionContext;
        use lore_revision::interface::LoreGlobalArgs;
        use lore_revision::relay::EventDispatcher;

        let events: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let callback: lore_revision::interface::LoreEventCallback = Some(Box::new(move |event| {
            captured.lock().expect("lock").push(event.discriminant());
        }));

        let (immutable_store, mutable_store, _unused_execution) =
            test_store_create().await.expect("Failed to create stores");
        let execution = Arc::new(ExecutionContext::new_client_with_user_id(
            LoreGlobalArgs::default(),
            EventDispatcher::new(callback),
            "test-user".to_string(),
        ));
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        let events_for_drain = events.clone();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                std::fs::create_dir_all(path.as_path()).expect("Create directory failed");

                let default_branch_id = BranchId::from(uuid::Uuid::now_v7());
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
                        default_repository_creation_args(immutable_store, mutable_store)
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

                // Commit without staging anything — must fail with NothingStaged
                // and must NOT emit a RevisionCommitRevision event.
                let options = CommitOptions {
                    message: String::new(),
                    link_messages: std::collections::HashMap::new(),
                    link: None,
                    layer_messages: std::collections::HashMap::new(),
                    layer: None,
                };
                let result = commit::commit_boxed(repository.clone(), &write_token, options).await;
                assert!(result.is_err(), "commit without staged changes should fail");
            }))
            .await
            .expect("Test task failed");

        drop(execution);
        // Give the forwarder time to flush any pending events.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let captured = events_for_drain.lock().expect("lock").clone();
        let commit_event = LoreEvent::RevisionCommitRevision(
            lore_revision::commit::LoreRevisionCommitRevisionEventData {
                repository: RepositoryId::default(),
                branch: BranchId::default(),
                revision: Default::default(),
                revision_number: 0,
                parent: Default::default(),
                parent_other: Default::default(),
            },
        );
        let commit_discriminant = commit_event.discriminant();
        assert!(
            !captured.contains(&commit_discriminant),
            "expected NO RevisionCommitRevision event (discriminant {commit_discriminant}) in captured {captured:?}"
        );
    }

    /// A repository on its own directory, holding `name` with `content`, its modified time set to
    /// `mtime`, staged and committed. Call inside a `LORE_CONTEXT` scope.
    async fn committed_with_time(
        name: &str,
        content: &[u8],
        mtime: std::time::SystemTime,
    ) -> (TestRepository, RelativePath) {
        let (immutable_store, mutable_store, _execution) =
            test_store_create().await.expect("Failed to create stores");
        let created = test_repository_create(
            immutable_store,
            mutable_store,
            RepositoryId::from(uuid::Uuid::now_v7()),
        )
        .await;
        // The branch's metadata, which `status` reads.
        let (_revision, branch) =
            lore_revision::instance::load_current_anchor_boxed(&created.repository)
                .await
                .expect("the anchor");
        branch::create(
            created.repository.clone(),
            &created.write_token,
            branch,
            branch::DEFAULT_DEFAULT_NAME,
            branch::default_category(),
            "test-user",
            0,
            vec![],
            false,
            false,
        )
        .await
        .expect("the branch's metadata");
        let file_path = created.path.join(name);
        std::fs::write(&file_path, content).expect("Failed to write test file");
        set_time(&file_path, mtime);
        file::stage::stage(
            created.repository.clone(),
            &created.write_token,
            LoreArray::from_vec(vec![LoreString::from(&created.path)]),
            StageOptions {
                case_change: stage::StageCaseChange::Error,
                node_flags: NodeFlags::NoFlags,
                file_id: None,
                no_children: false,
                scan: true,
            },
        )
        .await
        .expect("Failed to stage file");
        commit::commit_boxed(
            created.repository.clone(),
            &created.write_token,
            CommitOptions::new("commit with a time".to_string()),
        )
        .await
        .expect("Failed to commit revision");
        let relative_path = RelativePath::new_from_initial_path(name).expect("valid relative path");
        (created, relative_path)
    }

    fn set_time(path: &std::path::Path, mtime: std::time::SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .expect("Failed to open test file")
            .set_modified(mtime)
            .expect("Failed to set the modified time");
    }

    /// The time `path` holds, as lore records times.
    fn time_of(path: &std::path::Path) -> u64 {
        lore_revision::util::fs::file_mtime(&std::fs::metadata(path).expect("Failed to stat"))
    }

    /// What a `status --scan` of `repository` compared: the files it read, and the files a
    /// recorded modified time answered for.
    async fn scan_comparisons(repository: &Arc<RepositoryContext>) -> (u64, u64) {
        use lore_revision::event::LoreEvent;
        use lore_revision::interface::ExecutionContext;
        use lore_revision::interface::LoreEventCallback;
        use lore_revision::interface::LoreGlobalArgs;
        use lore_revision::relay::EventDispatcher;
        use lore_revision::repository::status::StatusOptions;

        let counted = Arc::new(std::sync::Mutex::new(None));
        let captured = counted.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event| {
            if let LoreEvent::RepositoryStatusSummary(data) = event {
                *captured.lock().expect("summary") = Some((data.hash_checks, data.mtime_matches));
            }
        }));
        let execution = Arc::new(ExecutionContext::new_client_with_user_id(
            LoreGlobalArgs::default(),
            EventDispatcher::new(callback),
            "test-user".to_string(),
        ));
        LORE_CONTEXT
            .scope(
                execution.clone(),
                lore_revision::repository::status::status_boxed(
                    repository.clone(),
                    None,
                    StatusOptions {
                        staged: true,
                        scan: true,
                        check_dirty: false,
                        reset: false,
                        sync_point: false,
                        revision_only: false,
                        count: false,
                    },
                ),
            )
            .await
            .expect("status");
        execution.dispatcher.drain().await;
        let summary = counted.lock().expect("summary").take();
        summary.expect("a status summary")
    }

    /// A file last written before the commit read the filesystem clock has its time recorded,
    /// and a scan after answers for it by that time without reading it.
    #[tokio::test]
    async fn a_commit_records_the_time_of_a_file_written_before_its_clock() {
        let (_stores, _mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution, async {
                let older = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
                let (created, relative_path) =
                    Box::pin(committed_with_time("older.file", b"written before", older)).await;
                assert_eq!(
                    state::file_modified_time(created.repository.clone(), &relative_path).await,
                    time_of(&created.path.join("older.file")),
                    "the commit recorded the time it read the file at"
                );
                assert_eq!(
                    scan_comparisons(&created.repository).await,
                    (0, 1),
                    "files read, files the recorded time answered for"
                );
            })
            .await;
    }

    /// A file whose time is not older than the filesystem clock the commit read before reading
    /// it has no time recorded: a write after the commit read it may share that time. A scan
    /// after reads it.
    #[tokio::test]
    async fn a_commit_records_no_time_a_later_write_could_share() {
        let (_stores, _mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution, async {
                let tick = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
                let (created, relative_path) = Box::pin(committed_with_time(
                    "racy.file",
                    b"written in the tick",
                    tick,
                ))
                .await;
                assert_eq!(
                    state::file_modified_time(created.repository.clone(), &relative_path).await,
                    0,
                    "the commit recorded a time a later write could share"
                );
                assert_eq!(
                    scan_comparisons(&created.repository).await,
                    (1, 0),
                    "files read, files the recorded time answered for"
                );
            })
            .await;
    }

    /// Reading the filesystem clock leaves no probe in the dot directory: not after a commit that
    /// records no time, and not after a status.
    #[tokio::test]
    async fn reading_the_clock_leaves_no_probe_behind() {
        let (_stores, _mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution, async {
                let tick = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
                let (created, relative_path) =
                    Box::pin(committed_with_time("probed.file", b"in the tick", tick)).await;
                assert_eq!(
                    state::file_modified_time(created.repository.clone(), &relative_path).await,
                    0,
                    "the commit recorded no time"
                );
                let probe = state::modified_time_probe_path(&created.repository)
                    .expect("the dot directory");
                assert!(!probe.exists(), "the commit left the clock probe behind");
                assert!(
                    state::read_clock_floor(&created.repository).await > 0,
                    "the dot directory takes the probe"
                );
                assert!(!probe.exists(), "reading the clock left the probe behind");
                scan_comparisons(&created.repository).await;
                assert!(!probe.exists(), "the status left the clock probe behind");
            })
            .await;
    }

    /// A collector with a floor keeps a time just older than the floor and drops one equal to it,
    /// whether recorded, pushed or absorbed from a collector without one.
    #[tokio::test]
    async fn a_collector_keeps_only_times_older_than_its_floor() {
        let (_stores, _mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution, async {
                let (created, relative_path) = Box::pin(committed_with_time(
                    "floored.file",
                    b"content",
                    std::time::SystemTime::now(),
                ))
                .await;
                let floor = 1_000;
                let collector = state::RecordedModifiedTimes::below(floor);
                collector.record(&created.repository, &relative_path, floor - 1);
                collector.record(&created.repository, &relative_path, floor);
                assert_eq!(
                    collector.records.len() + collector.store.len(),
                    1,
                    "recorded"
                );
                let entry = |mtime| {
                    state::file_modified_time_entry(&created.repository, &relative_path, mtime)
                };
                collector.push(entry(floor - 1));
                collector.push(entry(floor));
                assert_eq!(collector.records.len() + collector.store.len(), 2, "pushed");
                let unbounded = state::RecordedModifiedTimes::default();
                unbounded.push(entry(floor - 1));
                unbounded.push(entry(floor));
                collector.absorb(unbounded);
                assert_eq!(
                    collector.records.len() + collector.store.len(),
                    3,
                    "absorbed"
                );
            })
            .await;
    }

    /// A scan that reads a file last written before the scan read the filesystem clock records
    /// its time, and the next scan answers for it by that time without reading it.
    #[tokio::test]
    async fn a_scan_records_the_time_of_a_file_written_before_its_clock() {
        let (_stores, _mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution, async {
                let now = std::time::SystemTime::now();
                let (created, relative_path) =
                    Box::pin(committed_with_time("scanned.file", b"content", now)).await;
                let file_path = created.path.join("scanned.file");
                state::file_modified_time_store(created.repository.clone(), &relative_path, 0)
                    .await;
                set_time(&file_path, now - std::time::Duration::from_secs(3600));
                assert_eq!(scan_comparisons(&created.repository).await, (1, 0));
                assert_eq!(
                    state::file_modified_time(created.repository.clone(), &relative_path).await,
                    time_of(&file_path),
                    "the scan recorded the time it read the file at"
                );
                assert_eq!(
                    scan_comparisons(&created.repository).await,
                    (0, 1),
                    "files read, files the recorded time answered for"
                );
            })
            .await;
    }

    /// A scan that reads a file whose time is not older than the filesystem clock the scan read
    /// before reading it records no time, so the next scan reads it again.
    #[tokio::test]
    async fn a_scan_records_no_time_a_later_write_could_share() {
        let (_stores, _mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution, async {
                let now = std::time::SystemTime::now();
                let (created, relative_path) =
                    Box::pin(committed_with_time("racy-scan.file", b"content", now)).await;
                let file_path = created.path.join("racy-scan.file");
                state::file_modified_time_store(created.repository.clone(), &relative_path, 0)
                    .await;
                set_time(&file_path, now + std::time::Duration::from_secs(3600));
                assert_eq!(scan_comparisons(&created.repository).await, (1, 0));
                assert_eq!(
                    state::file_modified_time(created.repository.clone(), &relative_path).await,
                    0,
                    "the scan recorded a time a later write could share"
                );
                assert_eq!(
                    scan_comparisons(&created.repository).await,
                    (1, 0),
                    "files read, files the recorded time answered for"
                );
            })
            .await;
    }
}
