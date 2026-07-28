// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
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
    use lore_revision::node::NodeFlags;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::repository;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;
    use lore_revision::state::State;
    use lore_transport::ProtocolError;

    include!("helper.rs");

    #[tokio::test]
    async fn node_mark_dirty_propagates_to_parents() {
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

                // Create a file in a subdirectory and stage+commit it to get a tree with nodes
                let subdir = path.join("src");
                std::fs::create_dir_all(&subdir).expect("Create subdir failed");
                let file_path = subdir.join("test.txt");
                {
                    let mut f = std::fs::File::create(&file_path).expect("Create file failed");
                    f.write_all(b"hello").expect("Write failed");
                }

                // Stage the file
                let paths = LoreArray::from_vec(vec![LoreString::from(&path)]);
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
                .expect("Stage failed");

                // Commit to create a base revision with the file
                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial commit".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Now load the current state and create a staged state from it
                let (state_current, _, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");

                // Find the file node
                let file_link = state_current
                    .find_node_link(repository.clone(), "src/test.txt")
                    .await
                    .expect("Find node failed");

                // Mark the file as dirty
                state_current
                    .node_mark_dirty(
                        repository.clone(),
                        file_link.node,
                        NodeFlags::DirtyModify,
                        true,
                    )
                    .await
                    .expect("node_mark_dirty failed");

                // Verify the file node is dirty
                let file_node = state_current
                    .node(repository.clone(), file_link.node)
                    .await
                    .expect("Get file node failed");
                assert!(file_node.is_dirty(), "File node should be dirty");
                assert!(
                    file_node.is_dirty_modify(),
                    "File node should be dirty modify"
                );

                // Verify parent directory is dirty (propagated)
                let parent_id = file_node.parent;
                let parent_node = state_current
                    .node(repository.clone(), parent_id)
                    .await
                    .expect("Get parent node failed");
                assert!(
                    parent_node.is_dirty(),
                    "Parent directory should be dirty (propagated)"
                );

                // Parent should have base Dirty only (no action bits)
                assert!(
                    !parent_node.is_dirty_modify(),
                    "Parent should not have modify action"
                );

                // node_has_dirty_children should return true for the parent
                assert!(
                    state_current
                        .node_has_dirty_children(repository.clone(), parent_id)
                        .await
                        .expect("node_has_dirty_children failed"),
                    "Parent should have dirty children"
                );

                // Root node doesn't get Dirty flag (loop exits before root, same as Staged)
                // but root should have dirty children
                assert!(
                    state_current
                        .node_has_dirty_children(repository.clone(), ROOT_NODE)
                        .await
                        .expect("node_has_dirty_children on root failed"),
                    "Root should have dirty children"
                );
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn node_mark_dirty_early_out_on_already_dirty_parent() {
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

                // Create two files in same directory
                let subdir = path.join("src");
                std::fs::create_dir_all(&subdir).expect("Create subdir failed");
                {
                    let mut f =
                        std::fs::File::create(subdir.join("a.txt")).expect("Create file failed");
                    f.write_all(b"aaa").expect("Write failed");
                }
                {
                    let mut f =
                        std::fs::File::create(subdir.join("b.txt")).expect("Create file failed");
                    f.write_all(b"bbb").expect("Write failed");
                }

                // Stage and commit both files
                let paths = LoreArray::from_vec(vec![LoreString::from(&path)]);
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
                .expect("Stage failed");

                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial commit".to_string()),
                ))
                .await
                .expect("Commit failed");

                let (state, _, _) = State::deserialize_current_and_staged(repository.clone())
                    .await
                    .expect("Deserialize failed");

                // Mark file a as dirty
                let link_a = state
                    .find_node_link(repository.clone(), "src/a.txt")
                    .await
                    .expect("Find a.txt failed");
                state
                    .node_mark_dirty(
                        repository.clone(),
                        link_a.node,
                        NodeFlags::DirtyModify,
                        true,
                    )
                    .await
                    .expect("mark_dirty a failed");

                // Mark file b as dirty — parent is already dirty, so early-out should fire
                let link_b = state
                    .find_node_link(repository.clone(), "src/b.txt")
                    .await
                    .expect("Find b.txt failed");
                state
                    .node_mark_dirty(
                        repository.clone(),
                        link_b.node,
                        NodeFlags::DirtyModify,
                        false, // mark_dirty=false to allow early-out
                    )
                    .await
                    .expect("mark_dirty b failed");

                // Both should be dirty
                let node_a = state
                    .node(repository.clone(), link_a.node)
                    .await
                    .expect("Get a failed");
                let node_b = state
                    .node(repository.clone(), link_b.node)
                    .await
                    .expect("Get b failed");
                assert!(node_a.is_dirty_modify());
                assert!(node_b.is_dirty_modify());

                // node_has_dirty_children should find both
                let parent_id = node_a.parent;
                assert!(
                    state
                        .node_has_dirty_children(repository.clone(), parent_id)
                        .await
                        .expect("has_dirty_children failed")
                );
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn node_has_dirty_children_returns_false_when_clean() {
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

                // Create and commit a file
                let file_path = path.join("clean.txt");
                {
                    let mut f = std::fs::File::create(&file_path).expect("Create file failed");
                    f.write_all(b"clean").expect("Write failed");
                }

                let paths = LoreArray::from_vec(vec![LoreString::from(&path)]);
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
                .expect("Stage failed");

                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                let (state, _, _) = State::deserialize_current_and_staged(repository.clone())
                    .await
                    .expect("Deserialize failed");

                // No dirty children on root (everything is clean)
                assert!(
                    !state
                        .node_has_dirty_children(repository.clone(), ROOT_NODE)
                        .await
                        .expect("has_dirty_children failed"),
                    "Clean state should have no dirty children"
                );
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn dirty_and_staged_coexist_on_same_node() {
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

                // Create, stage, commit a file
                let subdir = path.join("src");
                std::fs::create_dir_all(&subdir).expect("Create subdir failed");
                {
                    let mut f =
                        std::fs::File::create(subdir.join("file.txt")).expect("Create file failed");
                    f.write_all(b"content").expect("Write failed");
                }

                let paths = LoreArray::from_vec(vec![LoreString::from(&path)]);
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
                .expect("Stage failed");

                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Modify and re-stage the file (creates staged state)
                {
                    let mut f =
                        std::fs::File::create(subdir.join("file.txt")).expect("Create file failed");
                    f.write_all(b"modified").expect("Write failed");
                }

                let paths = LoreArray::from_vec(vec![LoreString::from(&path)]);
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
                .expect("Re-stage failed");

                // Load staged state and mark the file as dirty too
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Should have staged state");

                let file_link = state_staged
                    .find_node_link(repository.clone(), "src/file.txt")
                    .await
                    .expect("Find file failed");

                // Verify it's staged
                let node = state_staged
                    .node(repository.clone(), file_link.node)
                    .await
                    .expect("Get node failed");
                assert!(node.is_staged(), "Node should be staged");

                // Now mark it dirty (orthogonal — should coexist)
                state_staged
                    .node_mark_dirty(
                        repository.clone(),
                        file_link.node,
                        NodeFlags::DirtyModify,
                        true,
                    )
                    .await
                    .expect("mark_dirty failed");

                let node = state_staged
                    .node(repository.clone(), file_link.node)
                    .await
                    .expect("Get node failed");
                assert!(node.is_dirty(), "Node should be dirty");
                assert!(node.is_staged(), "Node should still be staged");
                assert!(node.is_dirty_or_staged(), "Node should be dirty or staged");

                // Parent directory should have both Dirty and Staged
                let parent_id = node.parent;
                let parent = state_staged
                    .node(repository.clone(), parent_id)
                    .await
                    .expect("Get parent failed");
                assert!(parent.is_dirty(), "Parent should be dirty");
                assert!(parent.is_staged(), "Parent should be staged");
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn node_mark_dirty_replaces_previous_action() {
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

                // Create, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("test.txt")).expect("Create file failed");
                    f.write_all(b"data").expect("Write failed");
                }

                let paths = LoreArray::from_vec(vec![LoreString::from(&path)]);
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
                .expect("Stage failed");

                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                let (state, _, _) = State::deserialize_current_and_staged(repository.clone())
                    .await
                    .expect("Deserialize failed");

                let link = state
                    .find_node_link(repository.clone(), "test.txt")
                    .await
                    .expect("Find file failed");

                // Mark as DirtyModify first
                state
                    .node_mark_dirty(repository.clone(), link.node, NodeFlags::DirtyModify, true)
                    .await
                    .expect("mark_dirty modify failed");

                let node = state
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node failed");
                assert!(node.is_dirty_modify(), "Should be dirty modify");
                assert!(!node.is_dirty_delete(), "Should not be dirty delete");

                // Now re-mark as DirtyDelete — action should replace
                state
                    .node_mark_dirty(repository.clone(), link.node, NodeFlags::DirtyDelete, true)
                    .await
                    .expect("mark_dirty delete failed");

                let node = state
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node failed");
                assert!(node.is_dirty_delete(), "Should be dirty delete now");
                assert!(
                    !node.is_dirty_modify(),
                    "Modify should be replaced by delete"
                );
                assert!(node.is_dirty(), "Should still be dirty");
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn diff_reports_dirty_nodes_with_unchanged_content() {
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

                // Create, stage, commit a file
                {
                    let mut f =
                        std::fs::File::create(path.join("test.txt")).expect("Create file failed");
                    f.write_all(b"content").expect("Write failed");
                }

                let paths = LoreArray::from_vec(vec![LoreString::from(&path)]);
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
                .expect("Stage failed");

                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Load current state
                let (state_current, _, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");

                // Mark the file as dirty (content unchanged — just the flag)
                let link = state_current
                    .find_node_link(repository.clone(), "test.txt")
                    .await
                    .expect("Find file failed");
                state_current
                    .node_mark_dirty(repository.clone(), link.node, NodeFlags::DirtyModify, true)
                    .await
                    .expect("mark_dirty failed");

                // Diff current (clean) vs current-with-dirty-flag
                // The dirty node has same content but the Dirty flag — diff should report it
                let mut changes = Vec::new();
                let mut sink = lore_revision::state::ChangeSink::Vec(&mut changes);
                lore_revision::state::diff(
                    repository.clone(),
                    state_current.clone(), // from (also the "to" since we modified in-place)
                    repository.clone(),
                    state_current.clone(), // to (same state, but with dirty flag set)
                    None,
                    &mut sink,
                    lore_revision::filter::FilterMode::Full,
                )
                .await
                .expect("Diff failed");

                // The dirty-flagged file should appear in the diff even though content is identical
                assert!(
                    !changes.is_empty(),
                    "Diff should report dirty node even with unchanged content"
                );

                // The change should have the Dirty flag set
                let change = &changes[0];
                assert!(change.flags.is_dirty(), "Change should have Dirty flag set");
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn dirty_classify_modify_add_delete_ignore() {
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

                // Create two files, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("existing.txt")).expect("Create failed");
                    f.write_all(b"original").expect("Write failed");
                }
                {
                    let mut f =
                        std::fs::File::create(path.join("to_delete.txt")).expect("Create failed");
                    f.write_all(b"delete me").expect("Write failed");
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
                .expect("Stage failed");

                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Set up filesystem:
                // - existing.txt: still on disk, in revision -> Modify
                // - to_delete.txt: removed from disk, in revision -> Delete
                // - new_file.txt: on disk, not in revision -> Add
                // - ghost.txt: not on disk, not in revision -> Ignore
                std::fs::remove_file(path.join("to_delete.txt")).expect("Delete failed");
                {
                    let mut f =
                        std::fs::File::create(path.join("new_file.txt")).expect("Create failed");
                    f.write_all(b"new content").expect("Write failed");
                }

                // Call dirty() on all four paths
                let paths = LoreArray::from_vec(vec![
                    LoreString::from(path.join("existing.txt").to_string_lossy().as_ref()),
                    LoreString::from(path.join("to_delete.txt").to_string_lossy().as_ref()),
                    LoreString::from(path.join("new_file.txt").to_string_lossy().as_ref()),
                    LoreString::from(path.join("ghost.txt").to_string_lossy().as_ref()),
                ]);

                file::dirty::dirty(repository.clone(), paths)
                    .await
                    .expect("Dirty failed");

                // Verify the staged state
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Should have staged state after dirty");

                // existing.txt -> Dirty+Modify
                let link = state_staged
                    .find_node_link(repository.clone(), "existing.txt")
                    .await
                    .expect("Find existing.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(node.is_dirty_modify(), "existing.txt should be DirtyModify");

                // to_delete.txt -> Dirty+Delete
                let link = state_staged
                    .find_node_link(repository.clone(), "to_delete.txt")
                    .await
                    .expect("Find to_delete.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(
                    node.is_dirty_delete(),
                    "to_delete.txt should be DirtyDelete"
                );

                // new_file.txt -> Dirty+Add (node created)
                let link = state_staged
                    .find_node_link(repository.clone(), "new_file.txt")
                    .await
                    .expect("Find new_file.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(node.is_dirty_add(), "new_file.txt should be DirtyAdd");

                // ghost.txt -> ignored, should NOT exist in staged tree
                assert!(
                    state_staged
                        .find_node_link(repository.clone(), "ghost.txt")
                        .await
                        .is_err(),
                    "ghost.txt should not exist in staged tree"
                );

                // Root should have dirty children
                assert!(
                    state_staged
                        .node_has_dirty_children(repository.clone(), ROOT_NODE)
                        .await
                        .expect("has_dirty_children"),
                    "Root should have dirty children"
                );
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn dirty_directory_recurse_marks_children() {
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

                // Create src/ with two files, stage, commit
                let subdir = path.join("src");
                std::fs::create_dir_all(&subdir).expect("Create subdir failed");
                {
                    let mut f =
                        std::fs::File::create(subdir.join("a.txt")).expect("Create file failed");
                    f.write_all(b"aaa").expect("Write failed");
                }
                {
                    let mut f =
                        std::fs::File::create(subdir.join("b.txt")).expect("Create file failed");
                    f.write_all(b"bbb").expect("Write failed");
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
                .expect("Stage failed");

                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Add a new file, delete one existing, keep one (modify)
                std::fs::remove_file(subdir.join("b.txt")).expect("Delete failed");
                {
                    let mut f =
                        std::fs::File::create(subdir.join("c.txt")).expect("Create file failed");
                    f.write_all(b"ccc").expect("Write failed");
                }

                // Call dirty on the directory
                let paths =
                    LoreArray::from_vec(vec![LoreString::from(subdir.to_string_lossy().as_ref())]);
                file::dirty::dirty(repository.clone(), paths)
                    .await
                    .expect("Dirty directory failed");

                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Should have staged state");

                // a.txt exists on disk + in revision -> Modify
                let link = state_staged
                    .find_node_link(repository.clone(), "src/a.txt")
                    .await
                    .expect("Find a.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(node.is_dirty_modify(), "a.txt should be DirtyModify");

                // b.txt not on disk + in revision -> Delete
                let link = state_staged
                    .find_node_link(repository.clone(), "src/b.txt")
                    .await
                    .expect("Find b.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(node.is_dirty_delete(), "b.txt should be DirtyDelete");

                // c.txt on disk + not in revision -> Add
                let link = state_staged
                    .find_node_link(repository.clone(), "src/c.txt")
                    .await
                    .expect("Find c.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(node.is_dirty_add(), "c.txt should be DirtyAdd");
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn dirty_reverted_add_removes_node() {
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

                // Create and commit a base file
                {
                    let mut f =
                        std::fs::File::create(path.join("base.txt")).expect("Create failed");
                    f.write_all(b"base").expect("Write failed");
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
                .expect("Stage failed");
                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Step 1: Create a new file and mark it dirty (Dirty+Add)
                {
                    let mut f =
                        std::fs::File::create(path.join("temp.txt")).expect("Create failed");
                    f.write_all(b"temporary").expect("Write failed");
                }
                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![LoreString::from(
                        path.join("temp.txt").to_string_lossy().as_ref(),
                    )]),
                )
                .await
                .expect("First dirty failed");

                // Verify the node exists as Dirty+Add
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Should have staged state");
                let link = state_staged
                    .find_node_link(repository.clone(), "temp.txt")
                    .await
                    .expect("Find temp.txt after first dirty");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(node.is_dirty_add(), "temp.txt should be DirtyAdd");

                // Step 2: Delete the file from disk and call dirty again
                std::fs::remove_file(path.join("temp.txt")).expect("Delete failed");
                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![LoreString::from(
                        path.join("temp.txt").to_string_lossy().as_ref(),
                    )]),
                )
                .await
                .expect("Second dirty failed");

                // Verify the node is gone (reverted add should discard it)
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                // The staged state might still exist (base.txt was not dirty, but the
                // state was modified). Check that temp.txt is no longer findable.
                if let Some(state_staged) = state_staged {
                    assert!(
                        state_staged
                            .find_node_link(repository.clone(), "temp.txt")
                            .await
                            .is_err(),
                        "temp.txt should not exist after reverted add"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn dirty_move_relocates_node_and_propagates() {
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

                // Create src/file.txt and dest/ directory, stage, commit
                let src_dir = path.join("src");
                let dest_dir = path.join("dest");
                std::fs::create_dir_all(&src_dir).expect("Create src failed");
                std::fs::create_dir_all(&dest_dir).expect("Create dest failed");
                {
                    let mut f =
                        std::fs::File::create(src_dir.join("file.txt")).expect("Create failed");
                    f.write_all(b"content").expect("Write failed");
                }
                {
                    // Need a file in dest so the directory gets committed
                    let mut f =
                        std::fs::File::create(dest_dir.join("other.txt")).expect("Create failed");
                    f.write_all(b"other").expect("Write failed");
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
                .expect("Stage failed");

                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Move the file on disk (simulate what the caller already did)
                std::fs::rename(src_dir.join("file.txt"), dest_dir.join("file.txt"))
                    .expect("Rename failed");

                // Call dirty_move (absolute paths since new_from_user_path resolves against CWD)
                file::dirty::dirty_move(
                    repository.clone(),
                    src_dir.join("file.txt").to_string_lossy().to_string(),
                    dest_dir.join("file.txt").to_string_lossy().to_string(),
                )
                .await
                .expect("Dirty move failed");

                // Verify
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Should have staged state");

                // File should be findable at new path
                let link = state_staged
                    .find_node_link(repository.clone(), "dest/file.txt")
                    .await
                    .expect("Find dest/file.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(node.is_dirty_move(), "Node should be DirtyMove");

                // src directory should be dirty (child removed)
                let src_link = state_staged
                    .find_node_link(repository.clone(), "src")
                    .await
                    .expect("Find src");
                let src_node = state_staged
                    .node(repository.clone(), src_link.node)
                    .await
                    .expect("Get src node");
                assert!(src_node.is_dirty(), "src dir should be dirty");

                // dest directory should be dirty (child added)
                let dest_link = state_staged
                    .find_node_link(repository.clone(), "dest")
                    .await
                    .expect("Find dest");
                let dest_node = state_staged
                    .node(repository.clone(), dest_link.node)
                    .await
                    .expect("Get dest node");
                assert!(dest_node.is_dirty(), "dest dir should be dirty");
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn dirty_copy_creates_destination_node() {
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

                // Create a file, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("original.txt")).expect("Create failed");
                    f.write_all(b"content").expect("Write failed");
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
                .expect("Stage failed");

                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Call dirty_copy (absolute paths)
                file::dirty::dirty_copy(
                    repository.clone(),
                    path.join("original.txt").to_string_lossy().to_string(),
                    path.join("copy.txt").to_string_lossy().to_string(),
                )
                .await
                .expect("Dirty copy failed");

                // Verify
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Should have staged state");

                // Original should be unchanged (no dirty flag)
                let link = state_staged
                    .find_node_link(repository.clone(), "original.txt")
                    .await
                    .expect("Find original.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(!node.is_dirty(), "Original should not be dirty");

                // Copy should exist with DirtyCopy
                let link = state_staged
                    .find_node_link(repository.clone(), "copy.txt")
                    .await
                    .expect("Find copy.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(node.is_dirty_copy(), "Copy should be DirtyCopy");
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn staging_dirty_file_preserves_dirty_flag() {
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

                // Create a file, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("file.txt")).expect("Create failed");
                    f.write_all(b"original").expect("Write failed");
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
                .expect("Stage failed");

                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Modify the file and mark it dirty
                {
                    let mut f =
                        std::fs::File::create(path.join("file.txt")).expect("Create failed");
                    f.write_all(b"modified").expect("Write failed");
                }

                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![LoreString::from(
                        path.join("file.txt").to_string_lossy().as_ref(),
                    )]),
                )
                .await
                .expect("Dirty failed");

                // Verify it's dirty
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Should have staged state");
                let link = state_staged
                    .find_node_link(repository.clone(), "file.txt")
                    .await
                    .expect("Find file.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(
                    node.is_dirty_modify(),
                    "Should be dirty modify before stage"
                );
                assert!(!node.is_staged(), "Should not be staged yet");

                // Now stage the dirty file
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
                .expect("Stage of dirty file failed");

                // Verify it's both dirty AND staged (orthogonal)
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Should have staged state");
                let link = state_staged
                    .find_node_link(repository.clone(), "file.txt")
                    .await
                    .expect("Find file.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(
                    node.is_dirty(),
                    "Dirty flag should be preserved after stage"
                );
                assert!(node.is_staged(), "Should be staged after stage");
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn unstage_dirty_staged_preserves_dirty_when_file_differs() {
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

                // Create file, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("file.txt")).expect("Create failed");
                    f.write_all(b"original").expect("Write failed");
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
                .expect("Stage failed");
                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Modify the file, mark dirty, then stage
                {
                    let mut f =
                        std::fs::File::create(path.join("file.txt")).expect("Create failed");
                    f.write_all(b"modified").expect("Write failed");
                }
                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![LoreString::from(
                        path.join("file.txt").to_string_lossy().as_ref(),
                    )]),
                )
                .await
                .expect("Dirty failed");
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
                .expect("Stage failed");

                // Now unstage — file still differs from current revision, so Dirty should remain
                file::unstage::unstage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    file::unstage::UnstageOptions { single_node: false },
                )
                .await
                .expect("Unstage failed");

                // Check: Dirty should be preserved (file still modified on disk)
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Anchor should be preserved (dirty remain)");
                let link = state_staged
                    .find_node_link(repository.clone(), "file.txt")
                    .await
                    .expect("Find file.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(node.is_dirty(), "Dirty should be preserved after unstage");
                assert!(!node.is_staged(), "Staged should be cleared after unstage");
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn unstage_preserves_anchor_when_dirty_nodes_remain() {
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

                // Create two files, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("staged.txt")).expect("Create failed");
                    f.write_all(b"staged").expect("Write failed");
                }
                {
                    let mut f =
                        std::fs::File::create(path.join("dirty.txt")).expect("Create failed");
                    f.write_all(b"dirty_original").expect("Write failed");
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
                .expect("Stage failed");
                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Modify both files
                {
                    let mut f =
                        std::fs::File::create(path.join("staged.txt")).expect("Create failed");
                    f.write_all(b"staged_modified").expect("Write failed");
                }
                {
                    let mut f =
                        std::fs::File::create(path.join("dirty.txt")).expect("Create failed");
                    f.write_all(b"dirty_modified").expect("Write failed");
                }

                // Mark dirty.txt as dirty (but don't stage it)
                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![LoreString::from(
                        path.join("dirty.txt").to_string_lossy().as_ref(),
                    )]),
                )
                .await
                .expect("Dirty failed");

                // Stage only staged.txt
                file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        path.join("staged.txt").to_string_lossy().as_ref(),
                    )]),
                    StageOptions {
                        case_change: stage::StageCaseChange::Error,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: true,
                    },
                )
                .await
                .expect("Stage failed");

                // Unstage staged.txt — now no staged nodes remain, but dirty.txt is still dirty
                file::unstage::unstage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        path.join("staged.txt").to_string_lossy().as_ref(),
                    )]),
                    file::unstage::UnstageOptions { single_node: false },
                )
                .await
                .expect("Unstage failed");

                // Anchor should NOT be deleted — dirty.txt is still dirty
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                assert!(
                    state_staged.is_some(),
                    "Staged anchor should be preserved when dirty nodes remain"
                );
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn unstage_clears_dirty_when_file_matches_revision() {
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

                // Create file in subdir, stage, commit
                let subdir = path.join("src");
                std::fs::create_dir_all(&subdir).expect("Create subdir failed");
                {
                    let mut f =
                        std::fs::File::create(subdir.join("file.txt")).expect("Create failed");
                    f.write_all(b"original").expect("Write failed");
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
                .expect("Stage failed");
                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Modify file, mark dirty, then stage it
                {
                    let mut f =
                        std::fs::File::create(subdir.join("file.txt")).expect("Create failed");
                    f.write_all(b"modified").expect("Write failed");
                }
                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![LoreString::from(
                        subdir.join("file.txt").to_string_lossy().as_ref(),
                    )]),
                )
                .await
                .expect("Dirty failed");
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
                .expect("Stage failed");

                // Revert the file back to original content on disk
                {
                    let mut f =
                        std::fs::File::create(subdir.join("file.txt")).expect("Create failed");
                    f.write_all(b"original").expect("Write failed");
                }

                // Unstage — file now matches current revision, so Dirty should be cleared
                file::unstage::unstage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        subdir.join("file.txt").to_string_lossy().as_ref(),
                    )]),
                    file::unstage::UnstageOptions { single_node: false },
                )
                .await
                .expect("Unstage failed");

                // Check: Dirty should be cleared (file matches revision)
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");

                // If state_staged exists, the node should not be dirty
                if let Some(state_staged) = state_staged
                    && let Ok(link) = state_staged
                        .find_node_link(repository.clone(), "src/file.txt")
                        .await
                {
                    let node = state_staged
                        .node(repository.clone(), link.node)
                        .await
                        .expect("Get node");
                    assert!(
                        !node.is_dirty(),
                        "Dirty should be cleared when file matches revision"
                    );
                    assert!(!node.is_staged(), "Staged should be cleared after unstage");

                    // Parent directory should also have Dirty cleared (no dirty children)
                    let parent_id = node.parent;
                    let parent = state_staged
                        .node(repository.clone(), parent_id)
                        .await
                        .expect("Get parent");
                    assert!(
                        !parent.is_dirty(),
                        "Parent Dirty should be cleared (no dirty children)"
                    );
                }
                // If state_staged is None, the anchor was deleted — also correct
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn reset_dirty_only_clears_dirty_with_parent_cleanup() {
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

                // Create file in subdir, stage, commit
                let subdir = path.join("src");
                std::fs::create_dir_all(&subdir).expect("Create subdir failed");
                {
                    let mut f =
                        std::fs::File::create(subdir.join("file.txt")).expect("Create failed");
                    f.write_all(b"original").expect("Write failed");
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
                .expect("Stage failed");
                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Modify file and mark dirty (different size to ensure detection)
                {
                    let mut f =
                        std::fs::File::create(subdir.join("file.txt")).expect("Create failed");
                    f.write_all(b"modified content that is longer")
                        .expect("Write failed");
                }
                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![LoreString::from(
                        subdir.join("file.txt").to_string_lossy().as_ref(),
                    )]),
                )
                .await
                .expect("Dirty failed");

                // Verify dirty
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Should have staged state");
                let link = state_staged
                    .find_node_link(repository.clone(), "src/file.txt")
                    .await
                    .expect("Find file");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(node.is_dirty_modify(), "Should be dirty before reset");

                // Reset the file
                file::reset::reset(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        subdir.join("file.txt").to_string_lossy().as_ref(),
                    )]),
                    LoreString::default(),
                    file::reset::ResetOptions {
                        purge: false,
                        single_node: false,
                    },
                )
                .await
                .expect("Reset failed");

                // Verify file content restored
                let content =
                    std::fs::read_to_string(subdir.join("file.txt")).expect("Read file failed");
                assert_eq!(content, "original", "File should be restored");

                // Verify dirty cleared — anchor should be deleted since nothing remains
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                assert!(
                    state_staged.is_none(),
                    "Anchor should be deleted when no dirty or staged nodes remain"
                );
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn reset_one_dirty_preserves_parent_dirty_for_other() {
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

                // Create two files in subdir, stage, commit
                let subdir = path.join("src");
                std::fs::create_dir_all(&subdir).expect("Create subdir failed");
                {
                    let mut f = std::fs::File::create(subdir.join("a.txt")).expect("Create failed");
                    f.write_all(b"aaa").expect("Write failed");
                }
                {
                    let mut f = std::fs::File::create(subdir.join("b.txt")).expect("Create failed");
                    f.write_all(b"bbb").expect("Write failed");
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
                .expect("Stage failed");
                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Modify both files and mark both dirty
                {
                    let mut f = std::fs::File::create(subdir.join("a.txt")).expect("Create failed");
                    f.write_all(b"aaa modified longer").expect("Write failed");
                }
                {
                    let mut f = std::fs::File::create(subdir.join("b.txt")).expect("Create failed");
                    f.write_all(b"bbb modified longer").expect("Write failed");
                }
                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![
                        LoreString::from(subdir.join("a.txt").to_string_lossy().as_ref()),
                        LoreString::from(subdir.join("b.txt").to_string_lossy().as_ref()),
                    ]),
                )
                .await
                .expect("Dirty failed");

                // Reset only a.txt
                file::reset::reset(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        subdir.join("a.txt").to_string_lossy().as_ref(),
                    )]),
                    LoreString::default(),
                    file::reset::ResetOptions {
                        purge: false,
                        single_node: false,
                    },
                )
                .await
                .expect("Reset a.txt failed");

                // Verify: a.txt should not be dirty, b.txt should still be dirty
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Anchor should exist (b.txt still dirty)");

                let link_a = state_staged
                    .find_node_link(repository.clone(), "src/a.txt")
                    .await
                    .expect("Find a.txt");
                let node_a = state_staged
                    .node(repository.clone(), link_a.node)
                    .await
                    .expect("Get a");
                assert!(!node_a.is_dirty(), "a.txt should not be dirty after reset");

                let link_b = state_staged
                    .find_node_link(repository.clone(), "src/b.txt")
                    .await
                    .expect("Find b.txt");
                let node_b = state_staged
                    .node(repository.clone(), link_b.node)
                    .await
                    .expect("Get b");
                assert!(node_b.is_dirty_modify(), "b.txt should still be dirty");

                // Parent dir should still be dirty (b.txt is still dirty)
                let parent_id = node_b.parent;
                let parent = state_staged
                    .node(repository.clone(), parent_id)
                    .await
                    .expect("Get parent");
                assert!(
                    parent.is_dirty(),
                    "Parent should still be dirty (b.txt remains)"
                );

                // Now reset b.txt too
                file::reset::reset(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        subdir.join("b.txt").to_string_lossy().as_ref(),
                    )]),
                    LoreString::default(),
                    file::reset::ResetOptions {
                        purge: false,
                        single_node: false,
                    },
                )
                .await
                .expect("Reset b.txt failed");

                // Now anchor should be deleted (nothing dirty or staged remains)
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                assert!(
                    state_staged.is_none(),
                    "Anchor should be deleted after all dirty cleared"
                );
            }))
            .await
            .expect("Test task failed");
    }

    // Note: reset_staged_file_refuses test is in smoke tests (Task 17)
    // because the error handling path goes through task spawn + emit_map_err
    // which makes integration testing complex

    #[tokio::test]
    #[ignore] // Tested via smoke tests
    async fn reset_staged_file_refuses() {
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

                // Create file, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("file.txt")).expect("Create failed");
                    f.write_all(b"original").expect("Write failed");
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
                .expect("Stage failed");
                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Modify and stage
                {
                    let mut f =
                        std::fs::File::create(path.join("file.txt")).expect("Create failed");
                    f.write_all(b"staged content").expect("Write failed");
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
                .expect("Stage failed");

                // Reset should refuse (file is staged)
                let result = file::reset::reset(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        path.join("file.txt").to_string_lossy().as_ref(),
                    )]),
                    LoreString::default(),
                    file::reset::ResetOptions {
                        purge: false,
                        single_node: false,
                    },
                )
                .await;

                assert!(result.is_err(), "Reset should refuse staged file");
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn commit_clears_dirty_on_committed_preserves_dirty_only() {
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

                // Create two files, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("committed.txt")).expect("Create failed");
                    f.write_all(b"will be committed").expect("Write failed");
                }
                {
                    let mut f =
                        std::fs::File::create(path.join("dirty_only.txt")).expect("Create failed");
                    f.write_all(b"will stay dirty").expect("Write failed");
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
                .expect("Stage failed");
                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Initial".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Modify both files
                {
                    let mut f =
                        std::fs::File::create(path.join("committed.txt")).expect("Create failed");
                    f.write_all(b"committed modified longer")
                        .expect("Write failed");
                }
                {
                    let mut f =
                        std::fs::File::create(path.join("dirty_only.txt")).expect("Create failed");
                    f.write_all(b"dirty modified longer").expect("Write failed");
                }

                // Mark both as dirty
                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![
                        LoreString::from(path.join("committed.txt").to_string_lossy().as_ref()),
                        LoreString::from(path.join("dirty_only.txt").to_string_lossy().as_ref()),
                    ]),
                )
                .await
                .expect("Dirty failed");

                // Stage only committed.txt (dirty_only.txt stays dirty-only)
                file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        path.join("committed.txt").to_string_lossy().as_ref(),
                    )]),
                    StageOptions {
                        case_change: stage::StageCaseChange::Error,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: true,
                    },
                )
                .await
                .expect("Stage committed.txt failed");

                // Commit — committed.txt should be committed, dirty_only.txt preserved
                Box::pin(commit::commit(
                    repository.clone(),
                    &write_token,
                    CommitOptions::new("Second commit".to_string()),
                ))
                .await
                .expect("Commit failed");

                // Verify: anchor should exist with dirty_only.txt still dirty
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged =
                    state_staged.expect("Anchor should exist (dirty_only.txt still dirty)");

                // committed.txt should not be dirty or staged
                let link = state_staged
                    .find_node_link(repository.clone(), "committed.txt")
                    .await
                    .expect("Find committed.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(
                    !node.is_dirty(),
                    "committed.txt should not be dirty after commit"
                );
                assert!(
                    !node.is_staged(),
                    "committed.txt should not be staged after commit"
                );

                // dirty_only.txt should still be dirty
                let link = state_staged
                    .find_node_link(repository.clone(), "dirty_only.txt")
                    .await
                    .expect("Find dirty_only.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(
                    node.is_dirty_modify(),
                    "dirty_only.txt should still be dirty after commit"
                );
                assert!(!node.is_staged(), "dirty_only.txt should not be staged");
            }))
            .await
            .expect("Test task failed");
    }
}
