// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use std::sync::Arc;

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
    use lore_revision::node::ROOT_NODE;
    use lore_revision::repository;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::stage::StageOptions;
    use lore_revision::state;

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

                unstage::unstage(
                    repository_context.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    UnstageOptions::default(),
                )
                .await
                .expect("Failed to unstage repository");

                let (current_revision, _current_branch) =
                    lore_revision::instance::load_current_anchor_boxed(&repository_context)
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
                        change.path(),
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

    /// Unstaging a staged modification of a file puts the current revision's size and mode back
    /// into its node, while another file stays staged.
    #[tokio::test]
    async fn unstage_restores_a_modified_files_size_and_mode() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution, async move {
                let instance = test_repository_create(
                    immutable_store,
                    mutable_store,
                    RepositoryId::from(uuid::Uuid::now_v7()),
                )
                .await;
                test_file_write(&instance.path.join("a.txt"), b"short");
                test_file_write(&instance.path.join("b.txt"), b"other");
                let committed = test_commit_tree(&instance, "First").await;
                let repository = instance.repository.clone();
                let committed_a = committed
                    .find_node_link(repository.clone(), "a.txt")
                    .await
                    .expect("a.txt");
                let committed_a = committed
                    .node(repository.clone(), committed_a.node)
                    .await
                    .expect("a.txt");
                test_file_write(&instance.path.join("a.txt"), b"a longer content");
                test_file_write(&instance.path.join("b.txt"), b"other content");
                file::stage::stage(
                    repository.clone(),
                    &instance.write_token,
                    LoreArray::from_vec(vec![LoreString::from(&instance.path)]),
                    StageOptions {
                        scan: true,
                        ..StageOptions::default()
                    },
                )
                .await
                .expect("Failed to stage the changes");

                unstage::unstage(
                    repository.clone(),
                    &instance.write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        instance.path.join("a.txt").as_path(),
                    )]),
                    UnstageOptions::default(),
                )
                .await
                .expect("Failed to unstage a.txt");

                let staged = lore_revision::instance::load_staged_revision(&repository)
                    .await
                    .expect("Failed to load the staged revision")
                    .expect("b.txt keeps a staged revision");
                let staged = state::State::deserialize(repository.clone(), staged)
                    .await
                    .expect("Failed to deserialize the staged state");
                let link = staged
                    .find_node_link(repository.clone(), "a.txt")
                    .await
                    .expect("a.txt");
                let a = staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("a.txt");
                assert!(!a.is_staged(), "a.txt is unstaged, flags {:#x}", a.flags);
                assert_eq!(
                    (a.size, a.mode),
                    (committed_a.size, committed_a.mode),
                    "a.txt takes the current revision's size and mode back"
                );
            })
            .await;
    }

    /// Unstaging a staged file a directory-only view rule names unstages it: the view is asked
    /// about the path as the file the tree holds there. The ignore file leaves out no staged node.
    #[tokio::test]
    async fn unstaging_a_file_a_directory_only_view_rule_names_unstages_it() {
        in_execution(async {
            let (repository, tempdir) =
                committed_repository("", &["src/Intermediate".to_string()]).await;
            let root = tempdir.to_path_buf();
            test_file_write(root.join("src/Intermediate").as_path(), b"modified");
            stage_paths(&repository, &root, &["src/Intermediate"], false, false).await;
            assert!(
                next_node(&repository, "src/Intermediate").await.is_staged(),
                "the stage of src/Intermediate staged nothing"
            );
            test_file_write(
                root.join(repository::DOT_LORE)
                    .join(repository::VIEW_FILTER)
                    .as_path(),
                b"Intermediate/\n",
            );
            let viewed = Arc::new(
                repository
                    .to_filter_context(repository::load_filter(&root).expect("Load filter failed")),
            );
            unstage::unstage(
                viewed.clone(),
                viewed
                    .try_write_token()
                    .expect("The repository was opened for writing"),
                path_arguments(&root, &["src/Intermediate"]),
                UnstageOptions::default(),
            )
            .await
            .expect("Unstage failed");
            assert!(
                !next_node(&viewed, "src/Intermediate").await.is_staged(),
                "unstage left src/Intermediate staged"
            );
        })
        .await;
    }

    /// Unstages `name` below `root`, and answers the nodes of that name the next commit's tree
    /// holds below its root.
    async fn unstage_and_list(
        repository: &Arc<RepositoryContext>,
        root: &Path,
        name: &str,
    ) -> Vec<lore_revision::node::Node> {
        unstage::unstage(
            repository.clone(),
            repository
                .try_write_token()
                .expect("The repository was opened for writing"),
            path_arguments(root, &[name]),
            UnstageOptions::default(),
        )
        .await
        .expect("Unstage failed");

        let tree = next_tree(repository).await;
        let mut nodes = Vec::new();
        for id in children_named(repository, &tree, ROOT_NODE, name).await {
            nodes.push(
                tree.node(repository.clone(), id)
                    .await
                    .expect("Failed to read a node"),
            );
        }
        nodes
    }

    /// Unstaging a path a directory-to-file change holds discards the file staged for add, and
    /// leaves the directory it replaced as a dirty delete.
    #[tokio::test]
    async fn unstaging_a_staged_type_change_discards_the_added_file() {
        in_execution(async {
            let (repository, tempdir) =
                committed_repository("", &["shared/a.txt".to_string()]).await;
            let root = tempdir.to_path_buf();
            std::fs::remove_dir_all(root.join("shared")).expect("Failed to delete directory");
            test_file_write(root.join("shared").as_path(), b"file");
            stage_paths(&repository, &root, &["shared"], true, false).await;

            let nodes = unstage_and_list(&repository, &root, "shared").await;
            assert_eq!(nodes.len(), 1, "{nodes:?}");
            assert!(nodes[0].is_directory(), "{nodes:?}");
            assert!(nodes[0].is_dirty_delete(), "{nodes:?}");
        })
        .await;
    }

    /// Unstaging a path a file-to-directory change holds discards the directory staged for add
    /// with its subtree, and leaves the file it replaced as a dirty delete.
    #[tokio::test]
    async fn unstaging_a_staged_type_change_discards_the_added_directory() {
        in_execution(async {
            let (repository, tempdir) = committed_repository("", &["entry".to_string()]).await;
            let root = tempdir.to_path_buf();
            std::fs::remove_file(root.join("entry")).expect("Failed to delete file");
            std::fs::create_dir(root.join("entry")).expect("Failed to create directory");
            test_file_write(root.join("entry/inner.txt").as_path(), b"inner");
            stage_paths(&repository, &root, &["entry"], true, false).await;

            let staged = next_tree(&repository).await;
            let added = staged
                .find_node_link(repository.clone(), "entry")
                .await
                .expect("No node at entry")
                .node;
            let added_child = staged
                .find_node_link(repository.clone(), "entry/inner.txt")
                .await
                .expect("No node at entry/inner.txt")
                .node;

            let nodes = unstage_and_list(&repository, &root, "entry").await;
            assert_eq!(nodes.len(), 1, "{nodes:?}");
            assert!(nodes[0].is_file(), "{nodes:?}");
            assert!(nodes[0].is_dirty_delete(), "{nodes:?}");

            let tree = next_tree(&repository).await;
            for id in [added, added_child] {
                let node = tree
                    .node(repository.clone(), id)
                    .await
                    .expect("Failed to read a node");
                assert!(node.is_discarded(), "{id}: {node:?}");
            }
        })
        .await;
    }
}
