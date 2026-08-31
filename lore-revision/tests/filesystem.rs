// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for the filesystem provider abstraction.
//!
//! These tests verify the `FilesystemProvider` and `InstanceOperation` traits
//! work correctly with real repository contexts.

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Context;
    use lore_revision::branch;
    use lore_revision::fs::filesystem_provider::FilesystemPath;
    use lore_revision::fs::filesystem_provider::InstanceOperation;
    use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::util::path::RelativePath;
    use lore_revision::util::path::RepositoryPath;

    include!("helper.rs");

    struct Cleanup {
        path: PathBuf,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            #[allow(clippy::disallowed_methods)]
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Runs a filesystem operation test with standard setup and teardown.
    ///
    /// Creates a temporary repository, begins a filesystem operation, runs the test,
    /// then finalizes and cleans up. The test closure receives:
    /// - `operation`: The filesystem operation handle
    /// - `repo_path`: Path to the repository root
    ///
    /// Returns `true` from the closure to indicate changes were made (passed to finalize).
    async fn run_fs_test<F, Fut>(test_fn: F)
    where
        F: FnOnce(Arc<RepositoryContext>, Arc<InstanceOperationImpl>, PathBuf) -> Fut
            + Send
            + 'static,
        Fut: Future<Output = bool> + Send,
    {
        let (_immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
        let tempdir = generate_tempdir();
        let temp_path = tempdir.to_path_buf();
        let path = temp_path.clone();
        let _cleanup = Cleanup { path: path.clone() };

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
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
                .expect("Failed to create repository");

                let fs_provider = repository.file_system();
                let operation = fs_provider
                    .begin_operation()
                    .await
                    .expect("begin_operation should succeed");

                let changes_made =
                    test_fn(repository.clone(), operation.clone(), path.clone()).await;

                operation
                    .finalize(changes_made)
                    .await
                    .expect("finalize should succeed");

                let _ = std::fs::remove_dir_all(path.as_path());
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn filesystem_provider_begin_operation_finalize() {
        let (_immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
        let tempdir = generate_tempdir();
        let temp_path = tempdir.to_path_buf();
        let path = temp_path.clone();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
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
                .expect("Failed to create repository");

                // Test begin_operation returns a valid operation
                let fs_provider = repository.file_system();
                let operation = fs_provider
                    .begin_operation()
                    .await
                    .expect("begin_operation should succeed");

                // Test finalize with changes_made=true
                let result = operation.finalize(true).await;
                assert!(result.is_ok(), "finalize(true) should succeed");

                // Test begin_operation again (should work after finalize)
                let operation2 = fs_provider
                    .begin_operation()
                    .await
                    .expect("second begin_operation should succeed");

                // Test finalize with changes_made=false
                let result = operation2.finalize(false).await;
                assert!(result.is_ok(), "finalize(false) should succeed");

                let _ = std::fs::remove_dir_all(path.as_path());
            }))
            .await
            .expect("Test task failed");

        #[allow(clippy::disallowed_methods)]
        let _ = std::fs::remove_dir_all(temp_path.as_path());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn instance_operation_create_dir_all() {
        run_fs_test(|repository, operation, path| async move {
            let rel_path = RelativePath::new_from_initial_path("test_subdir/nested").unwrap();
            operation
                .create_dir_all(FilesystemPath::Repository(
                    &RepositoryPath::from_relative(&repository, rel_path).unwrap(),
                ))
                .await
                .expect("create_dir_all should succeed");

            let absolute_dir = path.join("test_subdir").join("nested");
            assert!(
                absolute_dir.exists(),
                "Directory should exist after create_dir_all"
            );
            true
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn instance_operation_file_info_directory() {
        run_fs_test(|repository, operation, _path| async move {
            let rel_path = RelativePath::new_from_initial_path("test_dir").unwrap();
            let rel_path = RepositoryPath::from_relative(&repository, rel_path).unwrap();
            operation
                .create_dir_all(FilesystemPath::Repository(&rel_path))
                .await
                .expect("create_dir_all should succeed");

            let info = operation
                .file_info(FilesystemPath::Repository(&rel_path))
                .await
                .expect("file_info should succeed");
            assert!(info.exists, "Directory should exist");
            assert!(info.is_dir, "Should be identified as directory");
            assert!(!info.is_file, "Should not be identified as file");
            true
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn instance_operation_file_info_file() {
        run_fs_test(|repository, operation, path| async move {
            let test_file = path.join("test_file.txt");
            let content = b"test content";
            {
                let mut file = std::fs::File::create(&test_file).expect("Create file failed");
                file.write_all(content).expect("Write failed");
            }

            let rel_path = RelativePath::new_from_initial_path("test_file.txt").unwrap();
            let info = operation
                .file_info(FilesystemPath::Repository(
                    &RepositoryPath::from_relative(&repository, rel_path).unwrap(),
                ))
                .await
                .expect("file_info should succeed");
            assert!(info.exists, "File should exist");
            assert!(info.is_file, "Should be identified as file");
            assert!(!info.is_dir, "Should not be identified as directory");
            assert_eq!(info.size, content.len() as u64, "Size should match");
            false
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn instance_operation_file_info_nonexistent() {
        run_fs_test(|repository, operation, _path| async move {
            let rel_path = RelativePath::new_from_initial_path("nonexistent").unwrap();
            let info = operation
                .file_info(FilesystemPath::Repository(
                    &RepositoryPath::from_relative(&repository, rel_path).unwrap(),
                ))
                .await
                .expect("file_info should succeed even for nonexistent path");
            assert!(!info.exists, "Nonexistent path should have exists=false");
            assert!(!info.is_file, "Nonexistent path should not be a file");
            assert!(!info.is_dir, "Nonexistent path should not be a directory");
            false
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn instance_operation_create_file() {
        run_fs_test(|repository, operation, path| async move {
            let rel_path = RelativePath::new_from_initial_path("new_file.txt").unwrap();
            let rel_path = RepositoryPath::from_relative(&repository, rel_path).unwrap();
            operation
                .create_file(FilesystemPath::Repository(&rel_path))
                .await
                .expect("create_file should succeed");

            let absolute_file = path.join("new_file.txt");
            assert!(
                absolute_file.exists(),
                "File should exist after create_file"
            );
            assert!(
                absolute_file.metadata().unwrap().is_file(),
                "Created path should be a file"
            );

            let info = operation
                .file_info(FilesystemPath::Repository(&rel_path))
                .await
                .expect("file_info should succeed");
            assert_eq!(info.size, 0, "Created file should be empty");
            true
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn instance_operation_remove_file() {
        run_fs_test(|repository, operation, path| async move {
            let test_file = path.join("to_remove.txt");
            {
                let mut file = std::fs::File::create(&test_file).expect("Create file failed");
                file.write_all(b"content").expect("Write failed");
            }
            assert!(test_file.exists(), "File should exist before removal");

            let rel_path = RelativePath::new_from_initial_path("to_remove.txt").unwrap();
            operation
                .remove(FilesystemPath::Repository(
                    &RepositoryPath::from_relative(&repository, rel_path).unwrap(),
                ))
                .await
                .expect("remove should succeed");

            assert!(!test_file.exists(), "File should not exist after remove");
            true
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn instance_operation_remove_empty_directory() {
        run_fs_test(|repository, operation, path| async move {
            let rel_path = RelativePath::new_from_initial_path("empty_dir").unwrap();
            let rel_path = RepositoryPath::from_relative(&repository, rel_path).unwrap();
            operation
                .create_dir_all(FilesystemPath::Repository(&rel_path))
                .await
                .expect("create_dir_all should succeed");

            let absolute_dir = path.join("empty_dir");
            assert!(absolute_dir.exists(), "Directory should exist");

            operation
                .remove(FilesystemPath::Repository(&rel_path))
                .await
                .expect("remove should succeed on empty directory");

            assert!(
                !absolute_dir.exists(),
                "Directory should not exist after remove"
            );
            true
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn instance_operation_remove_recursive() {
        run_fs_test(|repository, operation, path| async move {
            let dir_path = path.join("dir_with_contents");
            std::fs::create_dir_all(&dir_path).expect("Create directory failed");
            {
                let file_path = dir_path.join("file.txt");
                let mut file = std::fs::File::create(&file_path).expect("Create file failed");
                file.write_all(b"content").expect("Write failed");
            }
            {
                let nested_dir = dir_path.join("nested");
                std::fs::create_dir_all(&nested_dir).expect("Create nested dir failed");
                let nested_file = nested_dir.join("nested_file.txt");
                let mut file =
                    std::fs::File::create(&nested_file).expect("Create nested file failed");
                file.write_all(b"nested content").expect("Write failed");
            }

            assert!(dir_path.exists(), "Directory should exist");

            let rel_path = RelativePath::new_from_initial_path("dir_with_contents").unwrap();
            operation
                .remove_recursive(FilesystemPath::Repository(
                    &RepositoryPath::from_relative(&repository, rel_path).unwrap(),
                ))
                .await
                .expect("remove_recursive should succeed");

            assert!(
                !dir_path.exists(),
                "Directory should not exist after remove_recursive"
            );
            true
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn instance_operation_copy_to_scratch_file() {
        run_fs_test(|repository, operation, path| async move {
            let source_file = path.join("source.txt");
            let content = b"source content for copy test";
            {
                let mut file =
                    std::fs::File::create(&source_file).expect("Create source file failed");
                file.write_all(content).expect("Write failed");
            }

            let scratch_dir = std::env::temp_dir();
            let scratch_file = scratch_dir.join("lore_test_scratch_copy.txt");

            let source_rel_path = RelativePath::new_from_initial_path("source.txt").unwrap();
            operation
                .copy_to_scratch_file(
                    FilesystemPath::Repository(
                        &RepositoryPath::from_relative(&repository, source_rel_path).unwrap(),
                    ),
                    &scratch_file,
                )
                .await
                .expect("copy_to_scratch_file should succeed");

            assert!(scratch_file.exists(), "Scratch file should exist");
            let copied_content = std::fs::read(&scratch_file).expect("Read scratch file failed");
            assert_eq!(
                copied_content, content,
                "Copied content should match source"
            );

            #[allow(clippy::disallowed_methods)]
            let _ = std::fs::remove_file(&scratch_file);
            false
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn instance_operation_make_executable() {
        use std::os::unix::fs::PermissionsExt;

        run_fs_test(|repository, operation, path| async move {
            let test_file = path.join("script.sh");
            {
                let mut file = std::fs::File::create(&test_file).expect("Create file failed");
                file.write_all(b"#!/bin/bash\necho hello")
                    .expect("Write failed");
            }

            let metadata = std::fs::metadata(&test_file).expect("Get metadata failed");
            let initial_mode = metadata.permissions().mode();
            assert_eq!(
                initial_mode & 0o111,
                0,
                "File should not be executable initially"
            );

            let rel_path = RelativePath::new_from_initial_path("script.sh").unwrap();
            operation
                .make_executable(
                    FilesystemPath::Repository(
                        &RepositoryPath::from_relative(&repository, rel_path).unwrap(),
                    ),
                    true,
                )
                .await
                .expect("make_executable should succeed");

            let metadata = std::fs::metadata(&test_file).expect("Get metadata failed");
            let final_mode = metadata.permissions().mode();
            assert_ne!(
                final_mode & 0o111,
                0,
                "File should be executable after make_executable"
            );
            true
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn instance_operation_scratch_path() {
        run_fs_test(|_repository, operation, _path| async move {
            let scratch_dir = std::env::temp_dir().join("lore_test_scratch_dir");
            std::fs::create_dir_all(&scratch_dir).expect("Create scratch dir failed");

            let scratch_file = scratch_dir.join("scratch_file.txt");
            {
                let mut file =
                    std::fs::File::create(&scratch_file).expect("Create scratch file failed");
                file.write_all(b"scratch content").expect("Write failed");
            }

            let info = operation
                .file_info(FilesystemPath::Scratch(scratch_file.as_path()))
                .await
                .expect("file_info on scratch path should succeed");
            assert!(info.exists, "Scratch file should exist");
            assert!(info.is_file, "Scratch path should be a file");

            #[allow(clippy::disallowed_methods)]
            let _ = std::fs::remove_dir_all(&scratch_dir);
            false
        })
        .await;
    }
}
