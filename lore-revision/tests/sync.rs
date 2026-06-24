// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::fs;
    use std::io::Write;

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
    use lore_revision::node::NodeFlags;
    use lore_revision::repository;
    use lore_revision::revision::sync;
    use lore_revision::revision::sync::SyncOptions;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;

    include!("helper.rs");

    #[tokio::test(flavor = "multi_thread")]
    async fn sync_explicit_revision() {
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
                let first_signature =
                    Box::pin(commit::commit(repository.clone(), &write_token, options))
                        .await
                        .expect("Failed to commit revision");

                let other_file_path = path.as_path().join("second.test.file");
                {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .read(true)
                        .write(true)
                        .open(other_file_path.as_path())
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
                let second_signature =
                    Box::pin(commit::commit(repository.clone(), &write_token, options))
                        .await
                        .expect("Failed to commit revision");

                // Sync back to first revision
                Box::pin(sync::sync(
                    repository.clone(),
                    &write_token,
                    SyncOptions {
                        revision: Some(first_signature.to_string()),
                        filter_mode: lore_revision::filter::FilterMode::Full,
                        ..Default::default()
                    },
                ))
                .await
                .expect("Failed to sync back to first revision");

                // Verify file added in first revision is still there
                assert!(
                    fs::metadata(file_path.as_path())
                        .expect("Failed to find first file as expected")
                        .is_file()
                );

                // Verify file added in second revision is gone
                fs::metadata(other_file_path.as_path()).expect_err(
                    "File added in second revision was not removed as expected after sync back",
                );

                // Sync forward to second revision
                Box::pin(sync::sync(
                    repository.clone(),
                    &write_token,
                    SyncOptions {
                        revision: Some(second_signature.to_string()),
                        filter_mode: lore_revision::filter::FilterMode::Full,
                        ..Default::default()
                    },
                ))
                .await
                .expect("Failed to sync forward to second revision");

                // Verify file added in first revision is still there
                assert!(
                    fs::metadata(file_path.as_path())
                        .expect("Failed to find first file as expected")
                        .is_file()
                );

                // Verify file added in second revision is restored
                assert!(
                    fs::metadata(other_file_path.as_path())
                        .expect("Failed to find first file as expected")
                        .is_file()
                );

                let _ = std::fs::remove_dir_all(path.as_path());
            }))
            .await
            .expect("Test task failed");

        let _ = std::fs::remove_dir_all(temp_path.as_path());
    }
}
