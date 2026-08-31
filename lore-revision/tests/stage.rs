// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::io::Write;
    use std::sync::Arc;

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
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;
    use lore_revision::state;

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

    /// Two targets under one directory whose case the tree and the file system
    /// disagree about, staged together, which is the shape that has a shared
    /// ancestor to resolve once for both of them.
    ///
    /// Under `Keep` the staging renames that very directory as it goes, so
    /// anything resolved for it beforehand describes a directory that is no
    /// longer there. Both files have to arrive under the tree's case, and
    /// neither may be taken for a delete because the path it was resolved under
    /// stopped existing.
    #[tokio::test]
    #[allow(clippy::large_futures)]
    async fn stage_keep_case_of_a_directory_two_targets_share() {
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

                let directory = path.as_path().join("Assets");
                std::fs::create_dir(directory.as_path()).expect("Create directory failed");
                for name in ["first.file", "second.file"] {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .write(true)
                        .open(directory.as_path().join(name))
                        .expect("Failed to create test file");
                    file.write_all(&[0, 1, 2])
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
                .expect("Failed to stage the tree");

                // The file system now disagrees with the tree about the
                // directory, and the targets are given in the file system's
                // case - so the given path, the file system and the node all
                // have to be reconciled, for a directory two targets share.
                let renamed = path.as_path().join("assets_renamed");
                std::fs::rename(directory.as_path(), renamed.as_path()).expect("rename away");
                let lowered = path.as_path().join("assets");
                std::fs::rename(renamed.as_path(), lowered.as_path()).expect("rename to lowercase");

                let signature = file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![
                        LoreString::from(&lowered.join("first.file")),
                        LoreString::from(&lowered.join("second.file")),
                    ]),
                    StageOptions {
                        case_change: stage::StageCaseChange::Keep,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: false,
                    },
                )
                .await
                .expect("Case difference not resolved as expected");

                // Keep puts the file system back to what the tree holds, for the
                // directory and for everything that was under it.
                for entry in std::fs::read_dir(path.as_path()).expect("read root") {
                    let entry = entry.expect("entry");
                    println!(
                        "ROOT: {:?} dir={}",
                        entry.file_name(),
                        entry.path().is_dir()
                    );
                    if entry.path().is_dir()
                        && entry.file_name().to_string_lossy().to_lowercase() == "assets"
                    {
                        for sub in std::fs::read_dir(entry.path()).expect("read sub") {
                            println!("   SUB: {:?}", sub.expect("sub").file_name());
                        }
                    }
                }
                let names = lore_revision::util::fs::filesystem_names(path.as_path(), "assets")
                    .await
                    .expect("the directory must still be there");
                assert_eq!(names, vec!["Assets".to_string()]);
                assert!(
                    lore_revision::util::fs::filesystem_names_all_exist(
                        &path.as_path().join("Assets"),
                        &["first.file", "second.file"]
                    )
                    .await,
                    "both files must still be there, under the directory the tree names"
                );

                // And the tree holds one directory, not one per case variation that
                // was resolved along the way.
                let state = state::State::deserialize(repository.clone(), signature)
                    .await
                    .expect("Failed to deserialize the staged state");
                let root = state
                    .block(repository.clone(), 0)
                    .await
                    .expect("Failed to read the root block");
                let mut child = root.node(0).child();
                let mut children = Vec::new();
                while let Some(node) = child {
                    let block_index = node::NodeBlock::index(node);
                    let block = state
                        .block(repository.clone(), block_index)
                        .await
                        .expect("Failed to read a block");
                    block
                        .deserialize_nametable(repository.clone())
                        .await
                        .expect("Failed to read a name table");
                    let index = node::Node::index(node);
                    children.push(
                        block
                            .node_name_clone(index)
                            .expect("Failed to read a node name"),
                    );
                    child = block.node(index).sibling();
                }
                assert_eq!(children, vec!["Assets".to_string()]);

                let _ = std::fs::remove_dir_all(path.as_path());
            }))
            .await
            .expect("Test task failed");
    }

    /// What a staging left behind: whether it succeeded, the names the file
    /// system holds, and the names the tree holds.
    #[derive(Debug, PartialEq, Eq)]
    struct CaseOutcome {
        staged: bool,
        filesystem: Vec<String>,
        tree: Vec<String>,
    }

    fn collect_filesystem(root: &std::path::Path, prefix: &str, into: &mut Vec<String>) {
        let mut entries: Vec<_> = std::fs::read_dir(root)
            .expect("read dir")
            .map(|entry| entry.expect("entry"))
            .collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".lore" {
                continue;
            }
            let path = format!("{prefix}{name}");
            if entry.path().is_dir() {
                collect_filesystem(&entry.path(), &format!("{path}/"), into);
            } else {
                into.push(path);
            }
        }
    }

    async fn collect_tree(
        repository: Arc<RepositoryContext>,
        state: &state::State,
        node: node::NodeID,
        prefix: &str,
        into: &mut Vec<String>,
    ) {
        let block_index = node::NodeBlock::index(node);
        let block = state
            .block(repository.clone(), block_index)
            .await
            .expect("read block");
        block
            .deserialize_nametable(repository.clone())
            .await
            .expect("read name table");
        let index = node::Node::index(node);
        let name = block.node_name_clone(index).expect("read node name");
        let path = format!("{prefix}{name}");
        let entry = block.node(index);
        if entry.is_directory() {
            if let Some(child) = entry.child() {
                Box::pin(collect_tree(
                    repository.clone(),
                    state,
                    child,
                    &format!("{path}/"),
                    into,
                ))
                .await;
            }
        } else {
            into.push(path);
        }
        if let Some(sibling) = entry.sibling() {
            Box::pin(collect_tree(repository, state, sibling, prefix, into)).await;
        }
    }

    /// [`stage_case_scenario_with_scan`], taking `given` as the paths to stage
    /// rather than walking the directory listing.
    async fn stage_case_scenario(
        initial: &[&str],
        on_disk: &[&str],
        given: &[&str],
        case_change: stage::StageCaseChange,
    ) -> CaseOutcome {
        stage_case_scenario_with_scan(initial, on_disk, given, case_change, false).await
    }

    /// Stage `initial` so the tree holds those names, put the file system into
    /// `on_disk`, and stage `given` under `case_change`, walking the directory
    /// listing where `scan` is set and taking the paths as given otherwise.
    ///
    /// The file system is rebuilt rather than renamed into place: on Windows,
    /// writing to a path that differs from an existing one only by case reuses
    /// the name already stored, so the only way to be sure of the case on disk
    /// is to delete what is there and create it afresh.
    async fn stage_case_scenario_with_scan(
        initial: &[&str],
        on_disk: &[&str],
        given: &[&str],
        case_change: stage::StageCaseChange,
        scan: bool,
    ) -> CaseOutcome {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
        let initial: Vec<String> = initial.iter().map(|it| (*it).to_string()).collect();
        let on_disk: Vec<String> = on_disk.iter().map(|it| (*it).to_string()).collect();
        let given: Vec<String> = given.iter().map(|it| (*it).to_string()).collect();

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

                let write_files = |names: &[String]| {
                    for name in names {
                        let file = path.as_path().join(name);
                        std::fs::create_dir_all(file.parent().expect("has a parent"))
                            .expect("create parent");
                        let mut handle = std::fs::File::options()
                            .create(true)
                            .truncate(true)
                            .write(true)
                            .open(file.as_path())
                            .expect("create file");
                        handle.write_all(&[0, 1, 2]).expect("write file");
                    }
                };

                write_files(&initial);
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
                .expect("Failed to stage the initial tree");

                // Delete before creating, so the case on disk is the one asked
                // for rather than the one already recorded.
                for entry in std::fs::read_dir(path.as_path()).expect("read root") {
                    let entry = entry.expect("entry");
                    if entry.file_name() == ".lore" {
                        continue;
                    }
                    if entry.path().is_dir() {
                        std::fs::remove_dir_all(entry.path()).expect("remove dir");
                    } else {
                        std::fs::remove_file(entry.path()).expect("remove file");
                    }
                }
                write_files(&on_disk);

                let targets: Vec<LoreString> = given
                    .iter()
                    .map(|name| LoreString::from(&path.as_path().join(name)))
                    .collect();
                let staged = file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(targets),
                    StageOptions {
                        case_change,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan,
                    },
                )
                .await;

                let mut filesystem = Vec::new();
                collect_filesystem(path.as_path(), "", &mut filesystem);

                let mut tree = Vec::new();
                if let Ok(signature) = staged.as_ref() {
                    let state = state::State::deserialize(repository.clone(), *signature)
                        .await
                        .expect("Failed to deserialize the staged state");
                    let root = state
                        .block(repository.clone(), 0)
                        .await
                        .expect("Failed to read the root block");
                    if let Some(child) = root.node(0).child() {
                        collect_tree(repository.clone(), &state, child, "", &mut tree).await;
                    }
                    tree.sort();
                }

                let outcome = CaseOutcome {
                    staged: staged.is_ok(),
                    filesystem,
                    tree,
                };
                let _ = std::fs::remove_dir_all(path.as_path());
                outcome
            }))
            .await
            .expect("Test task failed")
    }

    const LEAF_TREE: &[&str] = &["Assets/Rock.mesh"];
    const SHARED_TREE: &[&str] = &["Assets/first.file", "Assets/second.file"];

    fn shared(directory: &str) -> Vec<String> {
        vec![
            format!("{directory}/first.file"),
            format!("{directory}/second.file"),
        ]
    }

    /// A component has three names - the path as given, the name the file
    /// system holds, and the name the node holds - and staging has to reconcile
    /// them however they disagree. `Error` refuses when the file system and the
    /// tree disagree, and is untroubled by the caller having typed a third
    /// case variation, since resolving the given path against the file system settles
    /// that before the node is ever consulted.
    #[tokio::test]
    #[allow(clippy::large_futures)]
    async fn stage_case_error_mode() {
        let mode = stage::StageCaseChange::Error;

        // Given differs, file system and node agree: nothing has changed.
        let outcome = stage_case_scenario(LEAF_TREE, LEAF_TREE, &["Assets/ROCK.MESH"], mode).await;
        assert!(outcome.staged, "a differently typed path is not a change");
        assert_eq!(outcome.filesystem, vec!["Assets/Rock.mesh".to_string()]);
        assert_eq!(outcome.tree, vec!["Assets/Rock.mesh".to_string()]);

        for (label, on_disk, given) in [
            // Given follows the file system, which the node disagrees with.
            (
                "given follows the file system",
                "Assets/rock.MESH",
                "Assets/rock.MESH",
            ),
            // Given follows the node, which the file system disagrees with.
            (
                "given follows the node",
                "Assets/rock.MESH",
                "Assets/Rock.mesh",
            ),
            // Given is a third case variation of its own.
            ("all three differ", "Assets/rock.MESH", "Assets/ROCK.mesh"),
        ] {
            let outcome = stage_case_scenario(LEAF_TREE, &[on_disk], &[given], mode).await;
            assert!(!outcome.staged, "{label}: the case change must be refused");
            assert_eq!(
                outcome.filesystem,
                vec![on_disk.to_string()],
                "{label}: a refused stage must leave the file system alone"
            );
        }
    }

    /// `Error` leaves two names in one directory differing only in case to the
    /// walk, so the second entry finds the child the first one claimed already
    /// taken and only the search behind it reaches the node, which is what
    /// reports the mismatch.
    ///
    /// A case insensitive file system holds one of the two names, leaving
    /// nothing to collide and the stage to stand. That is asserted rather than
    /// skipped, so the test cannot pass without having tested anything.
    #[tokio::test]
    #[allow(clippy::large_futures)]
    async fn stage_case_of_two_variations_in_one_directory() {
        let outcome = stage_case_scenario_with_scan(
            LEAF_TREE,
            &["Assets/Rock.mesh", "Assets/rock.mesh"],
            &["Assets"],
            stage::StageCaseChange::Error,
            true,
        )
        .await;

        let both_variations_on_disk = outcome.filesystem.len() == 2;
        assert_eq!(
            outcome.staged, !both_variations_on_disk,
            "a directory holding both case variations must be refused and one holding a single name must not, fs={:?} tree={:?}",
            outcome.filesystem, outcome.tree
        );
    }

    /// Two names differing only in case that the tree holds neither of: the
    /// first stages as a new child, and the second has to find the child the
    /// first one linked in, which is ahead of the listing the walk holds.
    ///
    /// Whichever of the two the walk reaches first creates it, so the second is
    /// refused either way, unlike a scenario where the tree already holds one of
    /// the names and the order decides which entry the mismatch falls to.
    #[tokio::test]
    #[allow(clippy::large_futures)]
    async fn stage_case_of_two_variations_neither_already_in_the_tree() {
        let outcome = stage_case_scenario_with_scan(
            &["Assets/other.file"],
            &["Assets/other.file", "Assets/Rock.mesh", "Assets/rock.mesh"],
            &["Assets"],
            stage::StageCaseChange::Error,
            true,
        )
        .await;

        let both_variations_on_disk = outcome.filesystem.len() == 3;
        assert_eq!(
            outcome.staged, !both_variations_on_disk,
            "the second of two case variations must find the child the first one added, \
             fs={:?} tree={:?}",
            outcome.filesystem, outcome.tree
        );
    }

    /// `Keep` treats the difference as unintended and puts the file system back
    /// to what the tree holds, leaving the tree as it was.
    #[tokio::test]
    #[allow(clippy::large_futures)]
    async fn stage_case_keep_mode() {
        let mode = stage::StageCaseChange::Keep;

        for (label, on_disk, given) in [
            (
                "given follows the file system",
                "Assets/rock.MESH",
                "Assets/rock.MESH",
            ),
            (
                "given follows the node",
                "Assets/rock.MESH",
                "Assets/Rock.mesh",
            ),
            ("all three differ", "Assets/rock.MESH", "Assets/ROCK.mesh"),
            (
                "only the given path differs",
                "Assets/Rock.mesh",
                "Assets/ROCK.MESH",
            ),
        ] {
            let outcome = stage_case_scenario(LEAF_TREE, &[on_disk], &[given], mode).await;
            assert!(outcome.staged, "{label}: staging must succeed");
            assert_eq!(
                outcome.filesystem,
                vec!["Assets/Rock.mesh".to_string()],
                "{label}: the file system must be put back to what the tree holds"
            );
            assert_eq!(
                outcome.tree,
                vec!["Assets/Rock.mesh".to_string()],
                "{label}: the tree keeps the name it had"
            );
        }
    }

    /// `Rename` treats the difference as intended and updates the tree to what
    /// the file system holds, leaving the file system as it is.
    #[tokio::test]
    #[allow(clippy::large_futures)]
    async fn stage_case_rename_mode() {
        let mode = stage::StageCaseChange::Rename;

        for (label, on_disk, given) in [
            (
                "given follows the file system",
                "Assets/rock.MESH",
                "Assets/rock.MESH",
            ),
            (
                "given follows the node",
                "Assets/rock.MESH",
                "Assets/Rock.mesh",
            ),
            ("all three differ", "Assets/rock.MESH", "Assets/ROCK.mesh"),
        ] {
            let outcome = stage_case_scenario(LEAF_TREE, &[on_disk], &[given], mode).await;
            assert!(outcome.staged, "{label}: staging must succeed");
            assert_eq!(
                outcome.filesystem,
                vec![on_disk.to_string()],
                "{label}: the file system keeps the name it has"
            );
            assert_eq!(
                outcome.tree,
                vec![on_disk.to_string()],
                "{label}: the tree must take the name the file system holds"
            );
        }

        // Nothing to rename: the caller simply typed it differently.
        let outcome = stage_case_scenario(LEAF_TREE, LEAF_TREE, &["Assets/ROCK.MESH"], mode).await;
        assert!(outcome.staged);
        assert_eq!(outcome.filesystem, vec!["Assets/Rock.mesh".to_string()]);
        assert_eq!(outcome.tree, vec!["Assets/Rock.mesh".to_string()]);
    }

    /// The same three-way disagreement, on a directory two targets share - which
    /// is the shape that has a prefix to resolve once for both of them, and so
    /// the one where an answer carried between them could be the wrong one.
    #[tokio::test]
    #[allow(clippy::large_futures)]
    async fn stage_case_of_a_shared_directory() {
        // Given differs, file system and node agree.
        let given: Vec<&str> = vec!["ASSETS/first.file", "ASSETS/second.file"];
        let outcome = stage_case_scenario(
            SHARED_TREE,
            SHARED_TREE,
            &given,
            stage::StageCaseChange::Error,
        )
        .await;
        assert!(
            outcome.staged,
            "a differently typed directory is not a change"
        );
        assert_eq!(outcome.filesystem, shared("Assets"));
        assert_eq!(outcome.tree, shared("Assets"));

        // The file system disagrees with the node about the directory, and the
        // targets are given in each of the three case variations in turn.
        let lowered = shared("assets");
        let lowered: Vec<&str> = lowered.iter().map(String::as_str).collect();
        for given in [&lowered, &vec!["Assets/first.file", "Assets/second.file"]] {
            let outcome =
                stage_case_scenario(SHARED_TREE, &lowered, given, stage::StageCaseChange::Error)
                    .await;
            assert!(!outcome.staged, "the case change must be refused");
            assert_eq!(outcome.filesystem, shared("assets"));

            let outcome =
                stage_case_scenario(SHARED_TREE, &lowered, given, stage::StageCaseChange::Keep)
                    .await;
            assert!(outcome.staged);
            assert_eq!(
                outcome.filesystem,
                shared("Assets"),
                "Keep puts the directory back to what the tree holds"
            );
            assert_eq!(outcome.tree, shared("Assets"));

            let outcome =
                stage_case_scenario(SHARED_TREE, &lowered, given, stage::StageCaseChange::Rename)
                    .await;
            assert!(outcome.staged);
            assert_eq!(
                outcome.filesystem,
                shared("assets"),
                "Rename leaves the directory as the file system has it"
            );
            assert_eq!(outcome.tree, shared("assets"));
        }
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

    /// How the targets given to one `stage` call are handed to it.
    enum TargetDelivery {
        /// Every target in one call, which resolves them concurrently and
        /// collects them in completion order.
        Together,
        /// One target per call, which leaves a single resolution in flight and
        /// so cannot reorder anything.
        OneAtATime,
    }

    /// Every file node reachable from the root as `path size flags`, sorted, as
    /// the tree the staging arrived at.
    ///
    /// Size and flags are what make this an assertion rather than a listing: the
    /// seed commit already put every path in the tree, so a staging that reached
    /// fewer targets than it should still names them all, and only the edit it
    /// failed to take up tells the two apart.
    async fn staged_file_listing(
        repository: Arc<RepositoryContext>,
        state: Arc<state::State>,
    ) -> Vec<String> {
        let mut found = Vec::new();
        let mut stack = vec![(node::ROOT_NODE, String::new())];
        while let Some((parent, prefix)) = stack.pop() {
            let children = state
                .node_children(repository.clone(), parent)
                .await
                .expect("Failed to read children");
            for child in children {
                let name = state
                    .node_name_clone(repository.clone(), child)
                    .await
                    .expect("Failed to read a node name");
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                let node = state
                    .node(repository.clone(), child)
                    .await
                    .expect("Failed to read a node");
                if node.is_directory() {
                    stack.push((child, path));
                } else {
                    found.push(format!("{path} {} {:#x}", node.size, node.flags));
                }
            }
        }
        found.sort();
        found
    }

    /// Seed a repository, commit it, dirty an edit under each target, then stage
    /// the targets the given way and return the tree it produced.
    ///
    /// The targets are three directories and a loose file, so one list covers
    /// both resolution results: a directory resolves to its dirty descendants,
    /// the file resolves to itself.
    async fn stage_targets_and_list(
        immutable_store: Arc<dyn lore_storage::immutable_store::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::mutable_store::MutableStore>,
        delivery: TargetDelivery,
    ) -> Vec<String> {
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
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
                default_repository_creation_args(immutable_store, mutable_store)
                    .with_path(&path)
                    .with_id(repository_id)
                    .with_instance_id(created_repo.instance_id),
            )
            .with_write_token(write_token.share()),
        );

        lore_revision::instance::store_current_anchor_branch(&repository, default_branch_id)
            .await
            .expect("Failed to store anchor branch");

        let directories = ["alpha", "beta", "gamma"];
        for directory in directories {
            std::fs::create_dir(path.as_path().join(directory)).expect("Create directory failed");
            for name in ["one.file", "two.file"] {
                let mut file = std::fs::File::create(path.as_path().join(directory).join(name))
                    .expect("Failed to create test file");
                file.write_all(b"seed").expect("Failed to write test file");
            }
        }
        let mut loose =
            std::fs::File::create(path.as_path().join("root.file")).expect("Failed to create");
        loose.write_all(b"seed").expect("Failed to write");

        let scan_options = StageOptions {
            case_change: stage::StageCaseChange::Error,
            node_flags: NodeFlags::NoFlags,
            file_id: None,
            no_children: false,
            scan: true,
        };
        Box::pin(file::stage::stage(
            repository.clone(),
            &write_token,
            LoreArray::from_vec(vec![LoreString::from(&path)]),
            scan_options,
        ))
        .await
        .expect("Failed to stage the seed tree");
        Box::pin(commit::commit(
            repository.clone(),
            &write_token,
            CommitOptions {
                message: String::new(),
                link_messages: std::collections::HashMap::new(),
                link: None,
                layer_messages: std::collections::HashMap::new(),
                layer: None,
            },
        ))
        .await
        .expect("Failed to commit the seed tree");

        let mut edited = Vec::new();
        for directory in directories {
            let edit = path.as_path().join(directory).join("one.file");
            let mut file = std::fs::File::create(edit.as_path()).expect("Failed to reopen");
            file.write_all(b"edited").expect("Failed to write");
            edited.push(LoreString::from(&edit));
        }
        let loose_edit = path.as_path().join("root.file");
        let mut file = std::fs::File::create(loose_edit.as_path()).expect("Failed to reopen");
        file.write_all(b"edited").expect("Failed to write");
        edited.push(LoreString::from(&loose_edit));

        file::dirty::dirty(repository.clone(), LoreArray::from_vec(edited))
            .await
            .expect("Failed to mark the edits dirty");

        let mut targets: Vec<LoreString> = directories
            .iter()
            .map(|directory| LoreString::from(&path.as_path().join(directory)))
            .collect();
        targets.push(LoreString::from(&loose_edit));

        let stage_options = StageOptions {
            case_change: stage::StageCaseChange::Error,
            node_flags: NodeFlags::NoFlags,
            file_id: None,
            no_children: false,
            scan: false,
        };
        let signature = match delivery {
            TargetDelivery::Together => Box::pin(file::stage::stage(
                repository.clone(),
                &write_token,
                LoreArray::from_vec(targets),
                stage_options,
            ))
            .await
            .expect("Failed to stage the targets together"),
            TargetDelivery::OneAtATime => {
                let mut last = None;
                for target in targets {
                    last = Some(
                        Box::pin(file::stage::stage(
                            repository.clone(),
                            &write_token,
                            LoreArray::from_vec(vec![target]),
                            stage_options,
                        ))
                        .await
                        .expect("Failed to stage a target"),
                    );
                }
                last.expect("No target was staged")
            }
        };

        let staged = state::State::deserialize(repository.clone(), signature)
            .await
            .expect("Failed to deserialize the staged state");
        let listed = staged_file_listing(repository.clone(), staged).await;
        let _ = std::fs::remove_dir_all(path.as_path());
        listed
    }

    /// Resolution collects targets in completion order, and only the antichain
    /// built from them decides what the walk covers. A target list resolved all at
    /// once must therefore reach the tree it reaches when one resolution is ever
    /// in flight.
    ///
    /// What that catches is a result lost or left unreaped once several are in
    /// flight, which no other test sees: reaping one task instead of draining
    /// stages the first target and leaves the rest at their committed contents.
    #[tokio::test]
    async fn targets_staged_together_reach_the_same_tree_as_one_at_a_time() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let together = stage_targets_and_list(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    TargetDelivery::Together,
                )
                .await;
                let one_at_a_time = stage_targets_and_list(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    TargetDelivery::OneAtATime,
                )
                .await;

                assert!(
                    !together.is_empty(),
                    "the fixture must stage something for the comparison to mean anything"
                );
                assert_eq!(
                    together, one_at_a_time,
                    "targets resolved at once must reach the same tree as targets resolved singly"
                );
            }))
            .await
            .expect("Test task failed");
    }
}
