// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use std::sync::Arc;

    use lore_base::error::NoRemote;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Context;
    use lore_revision::branch;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::file::unstage;
    use lore_revision::file::unstage::UnstageOptions;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::stage::StageOptions;
    use lore_revision::state;
    use lore_transport::ProtocolError;

    include!("helper.rs");

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
    async fn unstage_all() {
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
                .expect("Failed to create repository");

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

                // Create initial test directories
                // - dir_deleted
                // - dir_modified
                std::fs::create_dir(path.join("dir_deleted").as_path())
                    .expect("Create dir_deleted failed");
                std::fs::create_dir(path.join("dir_modified").as_path())
                    .expect("Create dir_modified failed");

                let file_modified_path = path.join("dir_modified/file_modified.txt");

                // Create initial test files
                // - dir_deleted/file_deleted.txt
                // - dir_modified/file_modified.txt
                let _ = create_file(path.join("dir_deleted/file_deleted.txt").as_path());
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
                    stats: false,
                };
                let _signature =
                    Box::pin(commit::commit(repository.clone(), &write_token, options))
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

                // Modify a file
                // - dir_modified/file_modified.txt
                file_modified
                    .write_all(&[5, 6, 7, 8, 9])
                    .expect("Failed to modify file_modified.txt");

                // Stage the other stages
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

                let repository_context = Arc::new(
                    RepositoryContext::new(
                        Some(path.as_path().to_path_buf()),
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

                unstage::unstage(
                    repository_context.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    UnstageOptions::default(),
                )
                .await
                .expect("Failed to unstage repository");

                let (current_revision, _current_branch) =
                    lore_revision::instance::load_current_anchor(&repository_context)
                        .await
                        .expect("Failed to load current anchor");

                let staged_revision =
                    lore_revision::instance::load_staged_revision(&repository_context)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or(current_revision);

                let state_current =
                    state::State::deserialize(repository_context.clone(), current_revision)
                        .await
                        .expect("Failed to deserialize current state");

                let state_staged =
                    state::State::deserialize(repository_context.clone(), staged_revision)
                        .await
                        .expect("Failed to deserialize staged state");

                let changes = state::diff_collect(
                    repository_context.clone(),
                    state_current.clone(),
                    repository_context.clone(),
                    state_staged.clone(),
                    None, /* No subpath */
                    lore_revision::filter::FilterMode::Full,
                )
                .await
                .expect("Failed to diff staged and current states");

                for change in changes.iter() {
                    println!(
                        "{}: {} {:?}",
                        change.action.as_string_short(),
                        change.path,
                        change.flags
                    );
                }

                for change in changes.iter() {
                    assert!(!change.flags.is_stage());
                }

                // Modify single file
                file_modified
                    .write_all(&[10, 11, 12, 13, 14])
                    .expect("Failed to modify file_modified.txt");

                // Stage the modified file
                let _signature = file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&file_modified_path)]),
                    StageOptions {
                        scan: true,
                        ..StageOptions::default()
                    },
                )
                .await
                .expect("Failed to stage repository");

                // Unstage single file
                unstage::unstage(
                    repository_context.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(file_modified_path.as_path())]),
                    UnstageOptions::default(),
                )
                .await
                .expect("Failed to unstage modified file");

                let staged_anchor_exists = std::fs::exists(path.join(".urc/staged").as_path())
                    .expect("Failed to check whether staged anchor exists");

                assert!(!staged_anchor_exists);
            }))
            .await
            .expect("Test task failed");
    }
}
