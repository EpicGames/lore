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
    use lore_base::types::Context;
    use lore_revision::branch;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;
    use lore_revision::lore_debug;
    use lore_revision::node;
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
    async fn stage_non_exist() {
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

                let paths = LoreArray::from_vec(vec![
                    LoreString::from("does.not.exist"),
                    LoreString::from("some/other/path"),
                ]);

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
                .expect("Stage of nonexisting file failed");

                let _ = std::fs::remove_dir_all(path.as_path());
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn stage_delete() {
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

                let signature = file::stage::stage(
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

                std::fs::remove_file(file_path.as_path()).expect("Failed to remove test file");

                let signature = file::stage::stage(
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
                .expect("Failed to stage file delete");

                // Load the final state and verify it has no entries
                let state = state::State::deserialize(repository.clone(), signature)
                    .await
                    .expect("Failed to deserialize staged state");
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

                let node = block.node(block.node(0).child().unwrap() as usize);
                assert!(node.is_staged_delete());

                let _ = std::fs::remove_dir_all(path.as_path());
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn stage_error_case() {
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                std::fs::create_dir_all(path.as_path()).expect("Create directory failed");
                let write_token = repository::RepositoryWriteToken::acquire(path.as_path()).await;
                let repository = repository::create_local(
                    path.as_path(),
                    &write_token,
                    repository_id,
                    Context::from(uuid::Uuid::now_v7()),
                    branch::DEFAULT_DEFAULT_NAME.to_string(),
                    repository::RepositoryConfig::default(),
                    false,
                )
                .await
                .expect("Failed to initialize repository");

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

                let signature = file::stage::stage(
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

                std::fs::remove_file(file_path.as_path()).expect("Failed to remove test file");

                // Create test file that differs by case
                let file_path = path.as_path().join("test.File");
                {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .read(true)
                        .write(true)
                        .open(file_path.as_path())
                        .expect("Failed to create test file");
                    file.write_all(&[0, 1, 2, 3, 4, 5])
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
                .expect_err("Case difference not detected as expected");

                let _ = std::fs::remove_dir_all(path.as_path());
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn stage_keep_case() {
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

                let first_file_path = path.as_path().join("test.file");
                {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .read(true)
                        .write(true)
                        .open(first_file_path.as_path())
                        .expect("Failed to create test file");
                    file.write_all(&[0, 1, 2, 3, 4])
                        .expect("Failed to write test file");
                }

                let signature = file::stage::stage(
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

                std::fs::remove_file(first_file_path.as_path())
                    .expect("Failed to remove test file");

                // Create test file that differs by case
                let second_file_path = path.as_path().join("test.File");
                {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .read(true)
                        .write(true)
                        .open(second_file_path.as_path())
                        .expect("Failed to create test file");
                    file.write_all(&[0, 1, 2, 3, 4, 5])
                        .expect("Failed to write test file");
                }

                // Verify the file system was updated
                let updated_name =
                    lore_revision::util::fs::filesystem_names(path.as_path(), "Test.file")
                        .await
                        .expect("Failed to get updated file name");
                assert_eq!(updated_name.len(), 1);
                let updated_name = updated_name[0].clone();
                assert_eq!(updated_name, "test.File");

                file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    StageOptions {
                        case_change: stage::StageCaseChange::Keep,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: true,
                    },
                )
                .await
                .expect("Case difference not resolved as expected");

                // Verify the file system was updated
                let updated_name =
                    lore_revision::util::fs::filesystem_names(path.as_path(), "Test.file")
                        .await
                        .expect("Failed to get updated file name");
                assert_eq!(updated_name.len(), 1);
                let updated_name = updated_name[0].clone();
                assert_eq!(updated_name, "test.file");

                let _ = std::fs::remove_dir_all(path.as_path());
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn stage_keep_case_recursive() {
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

                let first_directory_path = path.as_path().join("testDir");
                std::fs::create_dir_all(first_directory_path.as_path())
                    .expect("Create directory failed");
                let first_file_path = first_directory_path.as_path().join("teST.file");
                {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .read(true)
                        .write(true)
                        .open(first_file_path.as_path())
                        .expect("Failed to create test file");
                    file.write_all(&[0, 1, 2, 3, 4])
                        .expect("Failed to write test file");
                }

                let stage_path = path.as_path().join("testdir").join("test.file");
                let signature = file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&stage_path)]),
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

                std::fs::remove_dir_all(first_directory_path.as_path())
                    .expect("Failed to remove test directory");

                // Create test directory and file that differs by case
                let second_directory_path = path.as_path().join("Testdir");
                std::fs::create_dir_all(second_directory_path.as_path())
                    .expect("Create directory failed");
                let second_file_path = second_directory_path.as_path().join("test.File");
                {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .read(true)
                        .write(true)
                        .open(second_file_path.as_path())
                        .expect("Failed to create test file");
                    file.write_all(&[0, 1, 2, 3, 4, 5])
                        .expect("Failed to write test file");
                }

                // Verify the file system was updated
                let updated_name =
                    lore_revision::util::fs::filesystem_names(path.as_path(), "testdir")
                        .await
                        .expect("Failed to get updated directory name");
                assert_eq!(updated_name.len(), 1);
                let updated_name = updated_name[0].clone();
                assert_eq!(updated_name, "Testdir");

                file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    StageOptions {
                        case_change: stage::StageCaseChange::Keep,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: true,
                    },
                )
                .await
                .expect("Case difference not resolved as expected");

                // Verify the file system was updated
                let updated_directory_name =
                    lore_revision::util::fs::filesystem_names(path.as_path(), "testdir")
                        .await
                        .expect("Failed to get updated directory name");
                assert_eq!(updated_directory_name.len(), 1);
                let updated_directory_name = updated_directory_name[0].clone();
                assert_eq!(updated_directory_name, "testDir");
                let updated_file_name = lore_revision::util::fs::filesystem_names(
                    first_directory_path.as_path(),
                    "test.file",
                )
                .await
                .expect("Failed to get updated file name");
                assert_eq!(updated_file_name.len(), 1);
                let updated_file_name = updated_file_name[0].clone();
                assert_eq!(updated_file_name, "teST.file");

                let _ = std::fs::remove_dir_all(path.as_path());
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn stage_rename_case() {
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

                let signature = file::stage::stage(
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

                std::fs::remove_file(file_path.as_path()).expect("Failed to remove test file");

                // Create test file that differs by case
                let file_path = path.as_path().join("test.File");
                {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .read(true)
                        .write(true)
                        .open(file_path.as_path())
                        .expect("Failed to create test file");
                    file.write_all(&[0, 1, 2, 3, 4, 5])
                        .expect("Failed to write test file");
                }

                // Verify the file system was updated
                let updated_name =
                    lore_revision::util::fs::filesystem_names(path.as_path(), "Test.file")
                        .await
                        .expect("Failed to get updated file name");
                assert_eq!(updated_name.len(), 1);
                let updated_name = updated_name[0].clone();
                assert_eq!(updated_name, "test.File");

                let signature = file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    StageOptions {
                        case_change: stage::StageCaseChange::Rename,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: true,
                    },
                )
                .await
                .expect("Case difference not resolved as expected");

                // Verify the file system was maintained
                let updated_name =
                    lore_revision::util::fs::filesystem_names(path.as_path(), "Test.file")
                        .await
                        .expect("Failed to get updated file name");
                assert_eq!(updated_name.len(), 1);
                let updated_name = updated_name[0].clone();
                assert_eq!(updated_name, "test.File");

                // Verify the state was updated
                let state = state::State::deserialize(repository.clone(), signature)
                    .await
                    .expect("Failed to deserialize staged state");

                let node_link = state
                    .find_node_link(repository.clone(), "Test.file")
                    .await
                    .expect("Failed to find staged node");

                assert!(node_link.is_valid());

                let node = state
                    .node(repository.clone(), node_link.node)
                    .await
                    .expect("Failed to load staged node");

                assert!(
                    node.is_staged_move(),
                    "Node is not staged moved as expected"
                );

                let block = state
                    .block_with_nametable(
                        repository.clone(),
                        node::NodeBlock::index(node_link.node),
                    )
                    .await
                    .expect("Failed to deserialize block");
                let node_index = node::Node::index(node_link.node);
                let node_name = block.node_name_ref(node_index).expect("Invalid node name");

                assert_eq!(&*node_name, updated_name.as_str());

                let _ = std::fs::remove_dir_all(path.as_path());
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    #[allow(clippy::large_futures)]
    async fn stage_move() {
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

                let _ = file::stage::stage(
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

                // Commit the initial state
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
                    .expect("Failed to commit");

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

                let node_id = block.node(0).child().unwrap();

                let node = state
                    .node(repository.clone(), node_id)
                    .await
                    .expect("Failed to get staged node");

                let file_id = node.address.context;

                std::fs::remove_file(file_path.as_path()).expect("Failed to remove test file");

                // Create test file that differs in a different path
                let other_path = path.join("someDir");
                std::fs::create_dir_all(other_path.as_path()).expect("Failed to create directory");
                let other_file_path = other_path.as_path().join("Some.file");
                {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .read(true)
                        .write(true)
                        .open(other_file_path.as_path())
                        .expect("Failed to create test file");
                    file.write_all(&[0, 1, 2, 3, 4, 5])
                        .expect("Failed to write test file");
                }

                lore_debug!("Staging bad move, expecting failure",);
                let bad_file_path = other_path.as_path().join("bad.file");
                let _ = file::stage::stage_move(
                    repository.clone(),
                    &write_token,
                    file_path.to_string_lossy().to_string(),
                    bad_file_path.to_string_lossy().to_string(),
                    StageOptions {
                        case_change: stage::StageCaseChange::Rename,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: true,
                    },
                )
                .await
                .expect_err("Stage moved to non-existing target file did not fail as expected");

                lore_debug!("Staging good move, expecting success");
                let signature = file::stage::stage_move(
                    repository.clone(),
                    &write_token,
                    file_path.to_string_lossy().to_string(),
                    other_file_path.to_string_lossy().to_string(),
                    StageOptions {
                        case_change: stage::StageCaseChange::Rename,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: true,
                    },
                )
                .await
                .expect("Stage moved file failed");

                // Verify the state was updated as expected
                lore_debug!("Verify updated state");
                let state = state::State::deserialize(repository.clone(), signature)
                    .await
                    .expect("Failed to deserialize staged state");

                let node_link = state
                    .find_node_link(repository.clone(), "someDir/Some.file")
                    .await
                    .expect("Failed to find staged node");

                let node = state
                    .node(repository.clone(), node_link.node)
                    .await
                    .expect("Failed to load staged node");

                assert!(
                    node.is_staged_move(),
                    "Node is not staged moved as expected"
                );

                // Commit the updated state
                lore_debug!("Commit updated state");
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
                    .expect("Failed to commit");

                // Verify the committed state
                lore_debug!("Verify committed state");
                let state = state::State::deserialize(repository.clone(), signature)
                    .await
                    .expect("Failed to deserialize committed state");

                // Verify the node and file ID was maintained
                lore_debug!("Verify node and file ID was maintained");
                let node_link = state
                    .find_node_link(repository.clone(), "someDir/Some.file")
                    .await
                    .expect("Failed to find staged node");

                assert_eq!(node_link.node, node_id);

                let node = state
                    .node(repository.clone(), node_link.node)
                    .await
                    .expect("Failed to get child node");

                assert_eq!(node.address.context, file_id);

                let _ = std::fs::remove_dir_all(path.as_path());
            }))
            .await
            .expect("Test task failed");
    }
}
