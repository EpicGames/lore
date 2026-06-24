// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::io::Write;
    use std::sync::Arc;

    use lore_base::error::NoRemote;
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
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;
    use lore_revision::state;
    use lore_transport::ProtocolError;

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
                        Some(path.clone()),
                        immutable_store,
                        mutable_store,
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
                    stats: false,
                };
                let signature = Box::pin(commit::commit(repository.clone(), &write_token, options))
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

                let _ = std::fs::remove_dir_all(path.as_path());
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
                        Some(path.clone()),
                        immutable_store,
                        mutable_store,
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
                    stats: false,
                };
                let _signature =
                    Box::pin(commit::commit(repository.clone(), &write_token, options))
                        .await
                        .expect("Failed to commit revision");

                let _ = std::fs::remove_dir_all(path.as_path());
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
                        Some(path.clone()),
                        immutable_store,
                        mutable_store,
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

                // Commit without staging anything — must fail with NothingStaged
                // and must NOT emit a RevisionCommitRevision event.
                let options = CommitOptions {
                    message: String::new(),
                    link_messages: std::collections::HashMap::new(),
                    link: None,
                    layer_messages: std::collections::HashMap::new(),
                    layer: None,
                    stats: false,
                };
                let result =
                    Box::pin(commit::commit(repository.clone(), &write_token, options)).await;
                assert!(result.is_err(), "commit without staged changes should fail");

                let _ = std::fs::remove_dir_all(path.as_path());
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
}
