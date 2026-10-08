// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::mem::size_of;
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Address;
    use lore_base::types::Hash;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::immutable;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;
    use lore_revision::node::Node;
    use lore_revision::node::NodeBlock;
    use lore_revision::node::NodeFlags;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;
    use lore_revision::state::State;
    use lore_storage::hash::hash_string;
    use zerocopy::FromBytes;

    include!("helper.rs");

    #[tokio::test]
    async fn node_mark_dirty_propagates_to_parents() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

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
                    write_token,
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
                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial commit".to_string()),
                )
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

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
                    write_token,
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

                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial commit".to_string()),
                )
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

                // Create and commit a file
                let file_path = path.join("clean.txt");
                {
                    let mut f = std::fs::File::create(&file_path).expect("Create file failed");
                    f.write_all(b"clean").expect("Write failed");
                }

                let paths = LoreArray::from_vec(vec![LoreString::from(&path)]);
                file::stage::stage(
                    repository.clone(),
                    write_token,
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

                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

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
                    write_token,
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

                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                    write_token,
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

                // Create, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("test.txt")).expect("Create file failed");
                    f.write_all(b"data").expect("Write failed");
                }

                let paths = LoreArray::from_vec(vec![LoreString::from(&path)]);
                file::stage::stage(
                    repository.clone(),
                    write_token,
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

                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

                // Create, stage, commit a file
                {
                    let mut f =
                        std::fs::File::create(path.join("test.txt")).expect("Create file failed");
                    f.write_all(b"content").expect("Write failed");
                }

                let paths = LoreArray::from_vec(vec![LoreString::from(&path)]);
                file::stage::stage(
                    repository.clone(),
                    write_token,
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

                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                let diff_repository = repository.clone();
                let diff_state = state_current.clone();
                let changes = lore_revision::state::ChangeStream::spawn(async move |changes| {
                    lore_revision::state::diff(
                        diff_repository.clone(),
                        diff_state.clone(), // from (also the "to" since we modified in-place)
                        diff_repository,
                        diff_state, // to (same state, but with dirty flag set)
                        None,
                        None, // graft_view: no grafting
                        &changes,
                        lore_revision::filter::FilterMode::Full,
                    )
                    .await
                })
                .collect()
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

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
                    write_token,
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

                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

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
                    write_token,
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

                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

                // Create and commit a base file
                {
                    let mut f =
                        std::fs::File::create(path.join("base.txt")).expect("Create failed");
                    f.write_all(b"base").expect("Write failed");
                }
                file::stage::stage(
                    repository.clone(),
                    write_token,
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
                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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

    /// A tree written before the names were reserved can hold a node named for the repository's
    /// own directory. A dirty path naming it, in any case, is dropped at the door, so files on
    /// disk beneath it are never marked under that node: the walk would otherwise reach the node
    /// by name hash, which folds case.
    #[tokio::test]
    async fn dirty_ignores_a_path_through_a_planted_reserved_name() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let path = fixture.path.clone();

                test_file_write(&path.join("base.txt"), b"base");
                let state = test_commit_tree(&fixture, "Initial").await;

                // Plant the node in a staged tree over the revision, as an older writer could.
                let planted = state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        Node {
                            name_hash: hash_string("xurc"),
                            ..Default::default()
                        },
                        "xurc",
                    )
                    .await
                    .expect("adding the placeholder must succeed");
                let block_index = NodeBlock::index(planted);
                let block = state
                    .block_with_nametable(repository.clone(), block_index)
                    .await
                    .expect("the block must read back");
                {
                    let mut writer = block.write();
                    let node = writer.node(Node::index(planted));
                    let (offset, length) = (node.name_offset, node.name_length);
                    node.name_hash = hash_string(".URC");
                    let (offset, length) = writer
                        .node_name_store_unchecked(".URC", offset, length)
                        .expect("the unchecked store must accept the name");
                    let node = writer.node(Node::index(planted));
                    node.name_offset = offset;
                    node.name_length = length;
                    writer.mark_dirty();
                }
                state.block_modified(block, block_index);
                state.set_parent_self(state.revision());
                state.set_revision_number(0);
                let token = repository
                    .try_write_token()
                    .expect("the fixture holds the write token");
                let staged = state
                    .serialize(repository.clone(), token)
                    .await
                    .expect("serializing the planted tree must succeed");
                lore_revision::instance::store_staged_anchor(&repository, staged)
                    .await
                    .expect("storing the staged anchor must succeed");

                std::fs::create_dir(path.join(".URC")).expect("Create directory failed");
                test_file_write(&path.join(".URC").join("planted.txt"), b"planted");

                let signature = file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![LoreString::from(
                        path.join(".URC").to_string_lossy().as_ref(),
                    )]),
                )
                .await
                .expect("a dirty path through the planted node must be ignored, not fail");
                assert_eq!(
                    signature, staged,
                    "ignoring the path must leave the staged tree as it was"
                );

                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("the planted staged tree must remain");
                let planted_hash = hash_string(".URC");
                let (planted, _) = state_staged
                    .node_children_with_name_hash(repository.clone(), ROOT_NODE)
                    .await
                    .expect("the root's children must read")
                    .into_iter()
                    .find(|(_, name_hash)| *name_hash == planted_hash)
                    .expect("the planted node must still be in the tree");
                let beneath = state_staged
                    .node_children(repository.clone(), planted)
                    .await
                    .expect("the planted node's children must read");
                assert!(
                    beneath.is_empty(),
                    "nothing may be marked beneath the planted node, got {beneath:?}"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Stages `target`, a path relative to the repository root, the way `lore stage` does.
    async fn stage_target(fixture: &TestRepository, target: &str, scan: bool) {
        file::stage::stage(
            fixture.repository.clone(),
            &fixture.write_token,
            LoreArray::from_vec(vec![LoreString::from(
                fixture.path.join(target).to_string_lossy().as_ref(),
            )]),
            StageOptions {
                scan,
                ..Default::default()
            },
        )
        .await
        .unwrap_or_else(|err| panic!("Stage of {target:?} failed: {err}"));
    }

    async fn dirty_paths(fixture: &TestRepository, paths: &[&str]) {
        file::dirty::dirty(
            fixture.repository.clone(),
            LoreArray::from_vec(
                paths
                    .iter()
                    .map(|path| {
                        LoreString::from(fixture.path.join(path).to_string_lossy().as_ref())
                    })
                    .collect(),
            ),
        )
        .await
        .expect("Dirty failed");
    }

    /// The staged tree, which holds no node at any of `absent` and records no change at the root,
    /// so a status reports nothing for it.
    async fn assert_nothing_recorded(fixture: &TestRepository, absent: &[&str], case: &str) {
        let repository = fixture.repository.clone();
        let (_, state_staged, _) = State::deserialize_current_and_staged(repository.clone())
            .await
            .expect("Deserialize failed");
        let Some(state_staged) = state_staged else {
            return;
        };
        for path in absent {
            assert!(
                state_staged
                    .find_node_link(repository.clone(), path)
                    .await
                    .is_err(),
                "{case}: the staged tree still holds {path}, which no commit and no file holds"
            );
        }
        let root = state_staged
            .node(repository.clone(), ROOT_NODE)
            .await
            .expect("Root node");
        assert!(
            !root.is_staged() && !root.is_dirty(),
            "{case}: the staged tree still records a change (root flags {:#x})",
            root.flags
        );
    }

    /// A file created, marked dirty and removed again was never tracked, so staging it stages
    /// nothing and unstaging afterwards leaves nothing behind, whether staging names the file,
    /// collects it from the dirty markers, or scans for it.
    #[tokio::test]
    async fn staging_a_removed_dirty_add_stages_nothing() {
        for (case, target, scan) in [
            ("named", "temp.txt", false),
            ("dirty markers", "", false),
            ("scan", "", true),
        ] {
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

            #[allow(clippy::disallowed_methods)]
            runtime()
                .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                    let fixture =
                        test_repository_create(immutable_store, mutable_store, repository_id).await;
                    test_file_write(&fixture.path.join("base.txt"), b"base");
                    test_commit_tree(&fixture, "Initial").await;

                    test_file_write(&fixture.path.join("temp.txt"), b"temporary");
                    dirty_paths(&fixture, &["temp.txt"]).await;
                    std::fs::remove_file(fixture.path.join("temp.txt")).expect("Delete failed");

                    stage_target(&fixture, target, scan).await;
                    assert_nothing_recorded(&fixture, &["temp.txt"], case).await;

                    file::unstage::unstage(
                        fixture.repository.clone(),
                        &fixture.write_token,
                        LoreArray::from_vec(vec![LoreString::from(&fixture.path)]),
                        file::unstage::UnstageOptions { single_node: false },
                    )
                    .await
                    .expect("Unstage failed");
                    assert_nothing_recorded(&fixture, &["temp.txt"], case).await;
                }))
                .await
                .expect("Test task failed");
        }
    }

    /// `dirty` creates the directory a new file sits in when it marks the file. Once both are
    /// removed neither was ever tracked, so staging drops the directory along with the file.
    #[tokio::test]
    async fn staging_a_removed_dirty_add_drops_the_directory_created_for_it() {
        for (case, target, scan) in [
            ("named", "new/temp.txt", false),
            ("dirty markers", "", false),
            ("scan", "", true),
        ] {
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

            #[allow(clippy::disallowed_methods)]
            runtime()
                .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                    let fixture =
                        test_repository_create(immutable_store, mutable_store, repository_id).await;
                    test_file_write(&fixture.path.join("base.txt"), b"base");
                    test_commit_tree(&fixture, "Initial").await;

                    std::fs::create_dir(fixture.path.join("new")).expect("Create dir failed");
                    test_file_write(&fixture.path.join("new/temp.txt"), b"temporary");
                    dirty_paths(&fixture, &["new/temp.txt"]).await;
                    std::fs::remove_dir_all(fixture.path.join("new")).expect("Delete failed");

                    stage_target(&fixture, target, scan).await;
                    assert_nothing_recorded(&fixture, &["new", "new/temp.txt"], case).await;
                }))
                .await
                .expect("Test task failed");
        }
    }

    /// Only what no commit holds is dropped: in the same stage, a committed file that was removed
    /// is staged for delete and a committed directory that still holds files stays in the tree.
    #[tokio::test]
    async fn staging_a_removed_dirty_add_still_stages_committed_deletes() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                std::fs::create_dir(fixture.path.join("dir")).expect("Create dir failed");
                test_file_write(&fixture.path.join("dir/kept.txt"), b"kept");
                test_file_write(&fixture.path.join("dir/gone.txt"), b"gone");
                test_commit_tree(&fixture, "Initial").await;

                test_file_write(&fixture.path.join("dir/temp.txt"), b"temporary");
                dirty_paths(&fixture, &["dir/temp.txt"]).await;
                std::fs::remove_file(fixture.path.join("dir/temp.txt")).expect("Delete failed");
                std::fs::remove_file(fixture.path.join("dir/gone.txt")).expect("Delete failed");
                dirty_paths(&fixture, &["dir/gone.txt"]).await;

                stage_target(&fixture, "", false).await;

                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Should have staged state");
                assert!(
                    state_staged
                        .find_node_link(repository.clone(), "dir/temp.txt")
                        .await
                        .is_err(),
                    "dir/temp.txt was never tracked and should be gone"
                );
                let gone = state_staged
                    .find_node(repository.clone(), "dir/gone.txt")
                    .await
                    .expect("dir/gone.txt is committed and stays until the commit");
                assert!(
                    gone.is_staged_delete(),
                    "dir/gone.txt should be staged for delete"
                );
                let dir = state_staged
                    .find_node(repository.clone(), "dir")
                    .await
                    .expect("dir is committed and still holds kept.txt");
                assert!(
                    !dir.is_staged_delete(),
                    "dir should not be staged for delete"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The staged tree's children of the root that are named `name`.
    async fn root_children_named(fixture: &TestRepository, name: &str) -> Vec<Node> {
        let repository = fixture.repository.clone();
        let (_, state_staged, _) = State::deserialize_current_and_staged(repository.clone())
            .await
            .expect("Deserialize failed");
        let state_staged = state_staged.expect("Should have staged state");
        let mut named = Vec::new();
        for child in state_staged
            .node_children(repository.clone(), ROOT_NODE)
            .await
            .expect("Root children")
        {
            let child_name = state_staged
                .node_name_clone(repository.clone(), child)
                .await
                .expect("Child name");
            if child_name == name {
                named.push(
                    state_staged
                        .node(repository.clone(), child)
                        .await
                        .expect("Child node"),
                );
            }
        }
        named
    }

    /// A committed file replaced by a directory and staged leaves two nodes of one name in the
    /// staged tree: the file staged for delete and the directory staged for add. Once the
    /// directory is removed as well, staging again drops the directory, which no commit holds,
    /// and keeps the committed file's delete, whichever way staging reaches them.
    #[tokio::test]
    async fn staging_a_removed_replacement_of_a_committed_file_keeps_only_its_delete() {
        let mut failures = Vec::new();
        for (case, target, scan) in [
            ("dirty markers", "", false),
            ("scan", "", true),
            ("named directory", "item", false),
            ("named child", "item/child.txt", false),
        ] {
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

            #[allow(clippy::disallowed_methods)]
            let (staged, unstaged) = runtime()
                .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                    let fixture =
                        test_repository_create(immutable_store, mutable_store, repository_id).await;
                    test_file_write(&fixture.path.join("item"), b"committed");
                    test_commit_tree(&fixture, "Initial").await;

                    std::fs::remove_file(fixture.path.join("item")).expect("Delete failed");
                    std::fs::create_dir(fixture.path.join("item")).expect("Create dir failed");
                    test_file_write(&fixture.path.join("item/child.txt"), b"replacement");
                    stage_target(&fixture, "item", false).await;
                    assert_eq!(
                        root_children_named(&fixture, "item").await.len(),
                        2,
                        "{case}: staging the replacement holds the file and the directory"
                    );

                    std::fs::remove_dir_all(fixture.path.join("item")).expect("Delete failed");
                    stage_target(&fixture, target, scan).await;
                    let staged = root_children_named(&fixture, "item").await;

                    file::unstage::unstage(
                        fixture.repository.clone(),
                        &fixture.write_token,
                        LoreArray::from_vec(vec![LoreString::from(&fixture.path)]),
                        file::unstage::UnstageOptions { single_node: false },
                    )
                    .await
                    .expect("Unstage failed");
                    (staged, root_children_named(&fixture, "item").await)
                }))
                .await
                .expect("Test task failed");

            let flags = |nodes: &[Node]| nodes.iter().map(|node| node.flags).collect::<Vec<_>>();
            if !(staged.len() == 1 && staged[0].is_file() && staged[0].is_staged_delete()) {
                failures.push(format!(
                    "{case}: stage should leave only the committed file staged for delete, \
                     flags {:?}",
                    flags(&staged)
                ));
            }
            if !(unstaged.len() == 1
                && unstaged[0].is_file()
                && !unstaged[0].is_staged()
                && unstaged[0].is_dirty_delete())
            {
                failures.push(format!(
                    "{case}: unstage should leave only the committed file's delete, unstaged, \
                     flags {:?}",
                    flags(&unstaged)
                ));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// A committed file keeps its node however staging has since changed it, so removing it and
    /// staging again stages its delete rather than dropping it: once respelled, once deleted and
    /// added back, and once moved into a directory staging added and removed along with it.
    #[tokio::test]
    async fn staging_a_removed_committed_file_stages_its_delete_whatever_staging_did_to_it() {
        let mut failures = Vec::new();
        for case in [
            "respelled",
            "deleted and added back",
            "moved into a new directory",
        ] {
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

            #[allow(clippy::disallowed_methods)]
            let outcome = runtime()
                .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                    let fixture =
                        test_repository_create(immutable_store, mutable_store, repository_id).await;
                    let repository = fixture.repository.clone();
                    let path = fixture.path.clone();
                    test_file_write(&path.join("Item.txt"), b"committed");
                    let committed = test_commit_tree(&fixture, "Initial").await;
                    let committed_id = committed
                        .find_node_link(repository.clone(), "Item.txt")
                        .await
                        .expect("Item.txt is committed")
                        .node;

                    let staged_path = match case {
                        "respelled" => {
                            std::fs::rename(path.join("Item.txt"), path.join("item.txt"))
                                .expect("Rename failed");
                            file::stage::stage(
                                repository.clone(),
                                &fixture.write_token,
                                LoreArray::from_vec(vec![LoreString::from(
                                    path.join("item.txt").to_string_lossy().as_ref(),
                                )]),
                                StageOptions {
                                    case_change: stage::StageCaseChange::Rename,
                                    ..Default::default()
                                },
                            )
                            .await
                            .expect("Stage of the respelling failed");
                            std::fs::remove_file(path.join("item.txt")).expect("Delete failed");
                            stage_target(&fixture, "", true).await;
                            "item.txt"
                        }
                        "deleted and added back" => {
                            std::fs::remove_file(path.join("Item.txt")).expect("Delete failed");
                            stage_target(&fixture, "Item.txt", false).await;
                            test_file_write(&path.join("Item.txt"), b"added back");
                            stage_target(&fixture, "Item.txt", false).await;
                            std::fs::remove_file(path.join("Item.txt")).expect("Delete failed");
                            stage_target(&fixture, "", false).await;
                            "Item.txt"
                        }
                        _ => {
                            std::fs::create_dir(path.join("new")).expect("Create dir failed");
                            test_file_write(&path.join("new/keep.txt"), b"new");
                            stage_target(&fixture, "new", false).await;
                            std::fs::rename(path.join("Item.txt"), path.join("new/Item.txt"))
                                .expect("Move failed");
                            file::stage::stage_move(
                                repository.clone(),
                                &fixture.write_token,
                                path.join("Item.txt").to_string_lossy().into_owned(),
                                path.join("new/Item.txt").to_string_lossy().into_owned(),
                                StageOptions::default(),
                            )
                            .await
                            .expect("Stage of the move failed");
                            std::fs::remove_dir_all(path.join("new")).expect("Delete failed");
                            stage_target(&fixture, "", false).await;
                            "new/Item.txt"
                        }
                    };

                    let (_, state_staged, _) =
                        State::deserialize_current_and_staged(repository.clone())
                            .await
                            .expect("Deserialize failed");
                    let state_staged = state_staged.expect("Should have staged state");
                    let Ok(link) = state_staged
                        .find_node_link(repository.clone(), staged_path)
                        .await
                    else {
                        return Some(format!("{case}: the committed file was dropped"));
                    };
                    let node = state_staged
                        .node(repository.clone(), link.node)
                        .await
                        .expect("Staged node");
                    (link.node != committed_id || !node.is_staged_delete()).then(|| {
                        format!(
                            "{case}: {staged_path} should be the committed node staged for \
                             delete, node {} (committed {committed_id}) flags {:#x}",
                            link.node, node.flags
                        )
                    })
                }))
                .await
                .expect("Test task failed");
            failures.extend(outcome);
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// Removes `path` below the fixture, a file or a whole directory.
    // Test fixture writes; not subject to repository write-token discipline.
    #[allow(clippy::disallowed_methods)]
    fn remove_path(fixture: &TestRepository, path: &str) {
        let path = fixture.path.join(path);
        if path.is_dir() {
            std::fs::remove_dir_all(path).expect("Delete failed");
        } else {
            std::fs::remove_file(path).expect("Delete failed");
        }
    }

    /// Writes `path` below the fixture, creating the directories it sits in.
    fn write_path(fixture: &TestRepository, path: &str, contents: &[u8]) {
        let path = fixture.path.join(path);
        std::fs::create_dir_all(path.parent().expect("A parent")).expect("Create dir failed");
        test_file_write(&path, contents);
    }

    /// What the fixture still holds staged under `dir`, which a commit has to find nothing in.
    async fn staged_marks_left(fixture: &TestRepository, case: &str) -> Vec<String> {
        let repository = fixture.repository.clone();
        let mut failures = Vec::new();
        let (_, state_staged, _) = State::deserialize_current_and_staged(repository.clone())
            .await
            .expect("Deserialize failed");
        if let Some(state_staged) = state_staged {
            let root = state_staged
                .node(repository.clone(), ROOT_NODE)
                .await
                .expect("Root node");
            let dir = state_staged
                .find_node(repository.clone(), "dir")
                .await
                .expect("dir is committed");
            if root.is_staged() || dir.is_staged() {
                failures.push(format!(
                    "{case}: the staged state still marks staged the root (flags {:#x}) or dir \
                     (flags {:#x})",
                    root.flags, dir.flags
                ));
            }
        }
        match commit::commit_boxed(
            repository.clone(),
            &fixture.write_token,
            CommitOptions::new("Nothing".to_string()),
        )
        .await
        {
            Err(err) if err.is_nothing_staged() => {}
            Err(err) => failures.push(format!("{case}: commit failed: {err}")),
            Ok(_) => failures.push(format!("{case}: commit found something to commit")),
        }
        failures
    }

    /// Staging a new file marks every directory above it staged. Once the file is removed and
    /// staging drops it, those marks go with it, whichever way staging reaches the file: nothing
    /// is left staged, and a commit finds nothing to commit.
    #[tokio::test]
    async fn staging_a_removed_staged_add_clears_what_staging_it_propagated() {
        let mut failures = Vec::new();
        for (case, added, removed, target, scan) in [
            ("named", "dir/temp", "dir/temp", "dir/temp", false),
            ("dirty markers", "dir/temp", "dir/temp", "", false),
            ("scan", "dir/temp", "dir/temp", "", true),
            (
                "deeper chain",
                "dir/sub/temp",
                "dir/sub",
                "dir/sub/temp",
                false,
            ),
        ] {
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

            #[allow(clippy::disallowed_methods)]
            let outcome = runtime()
                .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                    let fixture =
                        test_repository_create(immutable_store, mutable_store, repository_id).await;
                    write_path(&fixture, "dir/base", b"committed");
                    test_commit_tree(&fixture, "Initial").await;

                    write_path(&fixture, added, b"temporary");
                    stage_target(&fixture, added, false).await;
                    remove_path(&fixture, removed);
                    stage_target(&fixture, target, scan).await;
                    staged_marks_left(&fixture, case).await
                }))
                .await
                .expect("Test task failed");
            failures.extend(outcome);
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// Marking a removed staged add dirty drops it as staging does, and the marks staging it
    /// carried up to the directories above it go with it.
    #[tokio::test]
    async fn marking_a_removed_staged_add_dirty_clears_what_staging_it_propagated() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        let failures = runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                write_path(&fixture, "dir/base", b"committed");
                test_commit_tree(&fixture, "Initial").await;

                write_path(&fixture, "dir/temp", b"temporary");
                stage_target(&fixture, "dir/temp", false).await;
                remove_path(&fixture, "dir/temp");
                dirty_paths(&fixture, &["dir/temp"]).await;
                staged_marks_left(&fixture, "marked dirty").await
            }))
            .await
            .expect("Test task failed");
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// Only the marks nothing else accounts for go: a directory that still holds a staged change,
    /// or that is itself a staged add, stays staged when a staged add below it is dropped.
    #[tokio::test]
    async fn staging_a_removed_staged_add_keeps_what_is_still_staged() {
        let mut failures = Vec::new();
        for (case, added, sibling, still_staged) in [
            ("sibling staged change", "dir/temp", true, "dir/base"),
            (
                "new directory still on disk",
                "dir/sub/temp",
                false,
                "dir/sub",
            ),
        ] {
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

            #[allow(clippy::disallowed_methods)]
            let outcome = runtime()
                .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                    let fixture =
                        test_repository_create(immutable_store, mutable_store, repository_id).await;
                    let repository = fixture.repository.clone();
                    write_path(&fixture, "dir/base", b"committed");
                    test_commit_tree(&fixture, "Initial").await;

                    if sibling {
                        write_path(&fixture, "dir/base", b"modified");
                        stage_target(&fixture, "dir/base", false).await;
                    }
                    write_path(&fixture, added, b"temporary");
                    stage_target(&fixture, added, false).await;
                    remove_path(&fixture, added);
                    stage_target(&fixture, "", false).await;

                    let (_, state_staged, _) =
                        State::deserialize_current_and_staged(repository.clone())
                            .await
                            .expect("Deserialize failed");
                    let state_staged = state_staged.expect("Should have staged state");
                    let mut failures = Vec::new();
                    if state_staged
                        .find_node_link(repository.clone(), added)
                        .await
                        .is_ok()
                    {
                        failures.push(format!("{case}: {added} should be dropped"));
                    }
                    for path in [still_staged, "dir"] {
                        let node = state_staged
                            .find_node(repository.clone(), path)
                            .await
                            .unwrap_or_else(|_| panic!("{case}: {path} should remain"));
                        if !node.is_staged() {
                            failures.push(format!(
                                "{case}: {path} should stay staged, flags {:#x}",
                                node.flags
                            ));
                        }
                    }
                    failures
                }))
                .await
                .expect("Test task failed");
            failures.extend(outcome);
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// A stage that cannot read the committed tree fails, rather than taking a committed file it
    /// cannot see for one no commit holds and dropping its delete.
    #[tokio::test]
    async fn staging_a_removed_file_fails_when_the_committed_tree_cannot_be_read() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store.clone(), mutable_store, repository_id)
                        .await;
                let repository = fixture.repository.clone();
                write_path(&fixture, "item.txt", b"committed");
                let block_list = test_commit_tree(&fixture, "Initial")
                    .await
                    .tree(repository.clone())
                    .await
                    .expect("Committed tree")
                    .hash_node;

                remove_path(&fixture, "item.txt");
                dirty_paths(&fixture, &["item.txt"]).await;

                let block_hashes = immutable::read(
                    repository.clone(),
                    Address::zero_context_hash(block_list),
                    None,
                    immutable::read_options_from_repository(&repository),
                )
                .await
                .expect("Committed block list");
                let first_block = Hash::read_from_bytes(&block_hashes[..size_of::<Hash>()])
                    .expect("Committed block hash");
                immutable_store
                    .obliterate(
                        repository.id,
                        Address::zero_context_hash(first_block),
                        Arc::default(),
                    )
                    .await
                    .expect("Obliterate failed");

                let staged = file::stage::stage(
                    repository.clone(),
                    &fixture.write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        fixture.path.join("item.txt").to_string_lossy().as_ref(),
                    )]),
                    StageOptions::default(),
                )
                .await;
                assert!(
                    staged.is_err(),
                    "staging must fail when the committed block cannot be read"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// On a case-sensitive file system a removed add named in another spelling than the one on
    /// disk is dropped alone: the directory holding it is found in its own spelling, so its other
    /// staged add stays.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn staging_a_removed_add_named_in_another_case_drops_only_that_file() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                write_path(&fixture, "base.txt", b"committed");
                test_commit_tree(&fixture, "Initial").await;

                write_path(&fixture, "Assets/a.txt", b"kept");
                write_path(&fixture, "Assets/b.txt", b"removed");
                stage_target(&fixture, "Assets", false).await;
                remove_path(&fixture, "Assets/b.txt");
                stage_target(&fixture, "assets/b.txt", false).await;

                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Should have staged state");
                assert!(
                    state_staged
                        .find_node_link(repository.clone(), "Assets/b.txt")
                        .await
                        .is_err(),
                    "Assets/b.txt was never committed and should be dropped"
                );
                let kept = state_staged
                    .find_node(repository.clone(), "Assets/a.txt")
                    .await
                    .expect("Assets/a.txt is on disk and should stay");
                assert!(
                    kept.is_staged_add(),
                    "Assets/a.txt should stay staged for add"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Under `--case keep` a removed add below a directory that several marked paths share is
    /// dropped, as it is under the other case modes.
    #[tokio::test]
    async fn staging_a_removed_dirty_add_below_a_shared_directory_drops_it_under_case_keep() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                write_path(&fixture, "dir/a.txt", b"committed");
                test_commit_tree(&fixture, "Initial").await;

                write_path(&fixture, "dir/a.txt", b"modified");
                write_path(&fixture, "dir/phantom.txt", b"temporary");
                dirty_paths(&fixture, &["dir/a.txt", "dir/phantom.txt"]).await;
                remove_path(&fixture, "dir/phantom.txt");

                file::stage::stage(
                    repository.clone(),
                    &fixture.write_token,
                    LoreArray::from_vec(vec![LoreString::from(&fixture.path)]),
                    StageOptions {
                        case_change: stage::StageCaseChange::Keep,
                        ..Default::default()
                    },
                )
                .await
                .expect("Stage failed");

                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Should have staged state");
                assert!(
                    state_staged
                        .find_node_link(repository.clone(), "dir/phantom.txt")
                        .await
                        .is_err(),
                    "dir/phantom.txt was never committed and should be dropped"
                );
                let modified = state_staged
                    .find_node(repository.clone(), "dir/a.txt")
                    .await
                    .expect("dir/a.txt is committed");
                assert!(
                    modified.is_staged_modify(),
                    "dir/a.txt should be staged for modify"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A removed add named on its own is dropped even where the new directory holding it also
    /// holds a committed file moved into it, which keeps that directory in the staged tree.
    #[tokio::test]
    async fn staging_a_removed_add_beside_a_moved_committed_file_drops_the_add() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let path = fixture.path.clone();
                write_path(&fixture, "Item.txt", b"committed");
                test_commit_tree(&fixture, "Initial").await;

                write_path(&fixture, "new/keep.txt", b"new");
                stage_target(&fixture, "new", false).await;
                std::fs::rename(path.join("Item.txt"), path.join("new/Item.txt"))
                    .expect("Move failed");
                file::stage::stage_move(
                    repository.clone(),
                    &fixture.write_token,
                    path.join("Item.txt").to_string_lossy().into_owned(),
                    path.join("new/Item.txt").to_string_lossy().into_owned(),
                    StageOptions::default(),
                )
                .await
                .expect("Stage of the move failed");
                remove_path(&fixture, "new");
                stage_target(&fixture, "new/keep.txt", false).await;

                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Should have staged state");
                assert!(
                    state_staged
                        .find_node_link(repository.clone(), "new/keep.txt")
                        .await
                        .is_err(),
                    "new/keep.txt was never committed and should be dropped"
                );
                assert!(
                    state_staged
                        .find_node_link(repository.clone(), "new/Item.txt")
                        .await
                        .is_ok(),
                    "new/Item.txt is committed and keeps new in the staged tree"
                );
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

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
                    write_token,
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

                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

                // Create a file, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("original.txt")).expect("Create failed");
                    f.write_all(b"content").expect("Write failed");
                }

                file::stage::stage(
                    repository.clone(),
                    write_token,
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

                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

                // Create a file, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("file.txt")).expect("Create failed");
                    f.write_all(b"original").expect("Write failed");
                }

                file::stage::stage(
                    repository.clone(),
                    write_token,
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

                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                    write_token,
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

                // Create file, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("file.txt")).expect("Create failed");
                    f.write_all(b"original").expect("Write failed");
                }
                file::stage::stage(
                    repository.clone(),
                    write_token,
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
                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                    write_token,
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
                    write_token,
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

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
                    write_token,
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
                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                    write_token,
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
                    write_token,
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

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
                    write_token,
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
                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                    write_token,
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
                    write_token,
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

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
                    write_token,
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
                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                    write_token,
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

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
                    write_token,
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
                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                    write_token,
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
                    write_token,
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
    // because the error handling path goes through task spawn + error
    // forwarding which makes integration testing complex

    #[tokio::test]
    #[ignore] // Tested via smoke tests
    async fn reset_staged_file_refuses() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

                // Create file, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("file.txt")).expect("Create failed");
                    f.write_all(b"original").expect("Write failed");
                }
                file::stage::stage(
                    repository.clone(),
                    write_token,
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
                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                    write_token,
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
                    write_token,
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
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

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
                    write_token,
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
                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
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
                    write_token,
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
                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Second commit".to_string()),
                )
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

    /// The same as above with the two files a directory down, which is what makes
    /// the commit walk descend rather than only look. Staging `nested/committed.txt`
    /// stages `nested` as well, so the directory holding the dirty-only file is
    /// itself staged - and a walk that decided whether to descend from whether the
    /// directory contributes a path of its own would stop there and lose
    /// `nested/dirty_only.txt`.
    #[tokio::test]
    async fn commit_preserves_a_dirty_only_file_under_a_staged_directory() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();
                std::fs::create_dir_all(path.join("nested")).expect("Create directory failed");

                let committed = path.join("nested").join("committed.txt");
                let dirty_only = path.join("nested").join("dirty_only.txt");
                {
                    let mut f = std::fs::File::create(&committed).expect("Create failed");
                    f.write_all(b"will be committed").expect("Write failed");
                }
                {
                    let mut f = std::fs::File::create(&dirty_only).expect("Create failed");
                    f.write_all(b"will stay dirty").expect("Write failed");
                }
                file::stage::stage(
                    repository.clone(),
                    write_token,
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
                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
                .await
                .expect("Commit failed");

                {
                    let mut f = std::fs::File::create(&committed).expect("Create failed");
                    f.write_all(b"committed modified longer")
                        .expect("Write failed");
                }
                {
                    let mut f = std::fs::File::create(&dirty_only).expect("Create failed");
                    f.write_all(b"dirty modified longer").expect("Write failed");
                }

                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![
                        LoreString::from(committed.to_string_lossy().as_ref()),
                        LoreString::from(dirty_only.to_string_lossy().as_ref()),
                    ]),
                )
                .await
                .expect("Dirty failed");

                file::stage::stage(
                    repository.clone(),
                    write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        committed.to_string_lossy().as_ref(),
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
                .expect("Stage nested/committed.txt failed");

                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Second commit".to_string()),
                )
                .await
                .expect("Commit failed");

                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged =
                    state_staged.expect("Anchor should exist (nested/dirty_only.txt still dirty)");

                let link = state_staged
                    .find_node_link(repository.clone(), "nested/committed.txt")
                    .await
                    .expect("Find nested/committed.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(
                    !node.is_dirty(),
                    "nested/committed.txt should not be dirty after commit"
                );
                assert!(
                    !node.is_staged(),
                    "nested/committed.txt should not be staged after commit"
                );

                let link = state_staged
                    .find_node_link(repository.clone(), "nested/dirty_only.txt")
                    .await
                    .expect("Find nested/dirty_only.txt");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(
                    node.is_dirty_modify(),
                    "nested/dirty_only.txt should still be dirty after commit"
                );
                assert!(
                    !node.is_staged(),
                    "nested/dirty_only.txt should not be staged"
                );

                let link = state_staged
                    .find_node_link(repository.clone(), "nested")
                    .await
                    .expect("Find nested");
                let node = state_staged
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(
                    node.is_dirty(),
                    "nested should stay dirty, carrying the dirty file below it"
                );
                assert_eq!(
                    node.action_bits(),
                    0,
                    "nested carries no action of its own, only the propagated flag"
                );
                assert!(
                    !node.is_staged(),
                    "nested should not be staged after commit"
                );
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn rebase_staged_state_carries_dirty_paths_without_touching_anchor() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

                // Create a file, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("carried.txt")).expect("Create failed");
                    f.write_all(b"original").expect("Write failed");
                }
                file::stage::stage(
                    repository.clone(),
                    write_token,
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
                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
                .await
                .expect("Commit failed");

                let (current_revision, _) =
                    lore_revision::instance::load_current_anchor_boxed(&repository)
                        .await
                        .expect("Load current anchor failed");

                // Modify and dirty the file so an anchored staged state exists
                {
                    let mut f =
                        std::fs::File::create(path.join("carried.txt")).expect("Create failed");
                    f.write_all(b"modified longer").expect("Write failed");
                }
                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![LoreString::from(
                        path.join("carried.txt").to_string_lossy().as_ref(),
                    )]),
                )
                .await
                .expect("Dirty failed");

                let staged_revision = lore_revision::instance::load_staged_revision(&repository)
                    .await
                    .expect("Load staged anchor failed")
                    .expect("Dirty should have anchored a staged state");
                assert_ne!(
                    staged_revision, current_revision,
                    "Dirty should anchor a state distinct from current"
                );

                let rebased_revision = lore_revision::state::rebase_staged_state(
                    repository.clone(),
                    staged_revision,
                    current_revision,
                    false,
                )
                .await
                .expect("Rebase failed")
                .expect("Dirty paths should produce a rebased state");
                assert_ne!(
                    rebased_revision, current_revision,
                    "Rebased state should carry the dirty path"
                );

                let anchored_revision = lore_revision::instance::load_staged_revision(&repository)
                    .await
                    .expect("Load staged anchor failed");
                assert_eq!(
                    anchored_revision,
                    Some(staged_revision),
                    "Rebase should not touch the staged anchor"
                );

                let rebased_state = State::deserialize(repository.clone(), rebased_revision)
                    .await
                    .expect("Deserialize rebased state failed");
                let link = rebased_state
                    .find_node_link(repository.clone(), "carried.txt")
                    .await
                    .expect("Find carried.txt");
                let node = rebased_state
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node");
                assert!(
                    node.is_dirty_modify(),
                    "carried.txt should be dirty in the rebased state"
                );
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn rebase_staged_state_returns_none_without_dirty_paths() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;
                let path = fixture.path.clone();

                // Create a file, stage, commit
                {
                    let mut f =
                        std::fs::File::create(path.join("clean.txt")).expect("Create failed");
                    f.write_all(b"original").expect("Write failed");
                }
                file::stage::stage(
                    repository.clone(),
                    write_token,
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
                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Initial".to_string()),
                )
                .await
                .expect("Commit failed");

                let (current_revision, _) =
                    lore_revision::instance::load_current_anchor_boxed(&repository)
                        .await
                        .expect("Load current anchor failed");

                // The committed revision carries no dirty nodes
                let rebased_revision = lore_revision::state::rebase_staged_state(
                    repository.clone(),
                    current_revision,
                    current_revision,
                    false,
                )
                .await
                .expect("Rebase failed");
                assert!(
                    rebased_revision.is_none(),
                    "A state without dirty paths has nothing to rebase"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The children of `parent` in `state` named `name`.
    async fn children_named(
        repository: &Arc<lore_revision::repository::RepositoryContext>,
        state: &Arc<State>,
        parent: lore_revision::node::NodeID,
        name: &str,
    ) -> Vec<lore_revision::node::NodeID> {
        let name_hash = hash_string(name);
        let mut named = Vec::new();
        for child in state
            .node_children(repository.clone(), parent)
            .await
            .expect("Failed to list the children")
        {
            let node = state
                .node(repository.clone(), child)
                .await
                .expect("Failed to read a child");
            if node.name_hash == name_hash {
                named.push(child);
            }
        }
        named
    }

    fn scan_status_options() -> lore_revision::repository::status::StatusOptions {
        lore_revision::repository::status::StatusOptions {
            staged: false,
            scan: true,
            check_dirty: false,
            reset: false,
            sync_point: false,
            revision_only: false,
            count: false,
        }
    }

    fn relative_path(path: &str) -> lore_revision::util::path::RelativePath {
        lore_revision::util::path::RelativePath::new_from_initial_path(path)
            .expect("Path init failed")
    }

    /// A repository opened as the command line opens it, so a status run finds its branch and
    /// reads `ignore` as the `.loreignore` it holds, with `files` and one more file committed.
    /// The write token it holds is what lets a scan persist a node it creates.
    async fn committed_repository(
        ignore: &str,
        files: &[String],
    ) -> (Arc<lore_revision::repository::RepositoryContext>, TempDir) {
        let tempdir = generate_tempdir();
        let path = tempdir.to_path_buf();
        let branch_id = lore_revision::lore::BranchId::from(uuid::Uuid::now_v7());
        let write_token =
            lore_revision::repository::RepositoryWriteToken::acquire(path.as_path()).await;
        lore_revision::repository::create_local(
            path.as_path(),
            &write_token,
            RepositoryId::from(uuid::Uuid::now_v7()),
            branch_id,
            lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
            lore_revision::repository::RepositoryConfig::default(),
            false,
        )
        .await
        .expect("Failed to initialize repository");
        test_file_write(
            path.join(lore_revision::repository::DOT_LOREIGNORE)
                .as_path(),
            ignore.as_bytes(),
        );
        test_file_write(path.join("seed.txt").as_path(), b"seed");
        for file in files {
            let file = path.join(file);
            std::fs::create_dir_all(file.parent().expect("A file has a parent"))
                .expect("Create directory failed");
            test_file_write(file.as_path(), b"committed");
        }

        let repository = lore_revision::repository::load_and_connect_with_token(
            path.as_path(),
            lore_revision::repository::RepositoryAccess::ReadWrite,
            Some(write_token),
        )
        .await
        .expect("Failed to open the repository");
        lore_revision::instance::store_current_anchor_branch(&repository, branch_id)
            .await
            .expect("Failed to store anchor branch");

        let token = repository
            .try_write_token()
            .expect("The repository was opened for writing");
        file::stage::stage(
            repository.clone(),
            token,
            LoreArray::from_vec(vec![LoreString::from(&path)]),
            StageOptions {
                scan: true,
                ..Default::default()
            },
        )
        .await
        .expect("Stage failed");
        commit::commit_boxed(
            repository.clone(),
            token,
            CommitOptions::new("Seed".to_string()),
        )
        .await
        .expect("Commit failed");

        (repository, tempdir)
    }

    /// A status scan reconciles the paths it is given in parallel, and the scan of an untracked
    /// path creates the directory nodes the tree lacks above it. Many untracked files sharing new
    /// directories must still leave each directory as one node, holding each file once.
    #[tokio::test]
    async fn status_scan_of_untracked_files_creates_each_shared_new_directory_once() {
        const FILES: usize = 64;

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let (repository, tempdir) = committed_repository("", &[]).await;
                let root = tempdir.to_path_buf();

                let mut paths = Vec::new();
                for directory in ["fresh/one", "fresh/two"] {
                    std::fs::create_dir_all(root.join(directory)).expect("Create directory failed");
                    for index in 0..FILES {
                        let path = format!("{directory}/{index:03}.txt");
                        test_file_write(root.join(&path).as_path(), b"new");
                        paths.push(relative_path(&path));
                    }
                }

                lore_revision::repository::status::status_boxed(
                    repository.clone(),
                    Some(paths),
                    scan_status_options(),
                )
                .await
                .expect("Status scan failed");

                let (_, staged, _) = State::deserialize_current_and_staged(repository.clone())
                    .await
                    .expect("Deserialize failed");
                let staged = staged.expect("The scan persists the nodes it created");

                let fresh = children_named(&repository, &staged, ROOT_NODE, "fresh").await;
                assert_eq!(fresh.len(), 1, "one node for the shared directory");
                for directory in ["one", "two"] {
                    let nodes = children_named(&repository, &staged, fresh[0], directory).await;
                    assert_eq!(nodes.len(), 1, "one node for fresh/{directory}");
                    for index in 0..FILES {
                        let name = format!("{index:03}.txt");
                        let files = children_named(&repository, &staged, nodes[0], &name).await;
                        assert_eq!(files.len(), 1, "one node for fresh/{directory}/{name}");
                    }
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A scan creates no directory node above a path it does not scan. Two ignored files share a
    /// directory the tree lacks, as do two paths nothing holds, and scanning them leaves the tree,
    /// and so the staged state, as it was.
    #[tokio::test]
    async fn status_scan_creates_no_directory_shared_only_by_ignored_or_missing_paths() {
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let (repository, tempdir) = committed_repository("*.tmp\n", &[]).await;
                let root = tempdir.to_path_buf();

                std::fs::create_dir_all(root.join("ignored")).expect("Create directory failed");
                test_file_write(root.join("ignored/a.tmp").as_path(), b"ignored");
                test_file_write(root.join("ignored/b.tmp").as_path(), b"ignored");

                lore_revision::repository::status::status_boxed(
                    repository.clone(),
                    Some(vec![
                        relative_path("ghost/a.txt"),
                        relative_path("ghost/b.txt"),
                        relative_path("ignored/a.tmp"),
                        relative_path("ignored/b.tmp"),
                    ]),
                    scan_status_options(),
                )
                .await
                .expect("Status scan failed");

                let (_, staged, _) = State::deserialize_current_and_staged(repository.clone())
                    .await
                    .expect("Deserialize failed");
                assert!(
                    staged.is_none(),
                    "the scan created a node for a directory it scanned nothing in"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Whether `state` holds `path`.
    async fn holds(
        repository: &Arc<lore_revision::repository::RepositoryContext>,
        state: &Arc<State>,
        path: &str,
    ) -> bool {
        state
            .find_node_link(repository.clone(), path)
            .await
            .is_ok_and(|link| link.is_valid())
    }

    /// A marking walk handed a queue for what it finds stale leaves it there for its caller, who
    /// discards it once every walk over the tree has drained. A walk handed none discards it
    /// itself once it has drained, before its changes are answered.
    #[tokio::test]
    async fn a_walk_handed_a_discard_queue_leaves_its_discards_to_the_caller() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                test_file_write(fixture.path.join("seed.txt").as_path(), b"seed");
                test_commit_tree(&fixture, "Seed").await;
                let (current, staged) = test_anchor_states(&repository).await;

                let added = fixture.path.join("added.txt");
                test_file_write(added.as_path(), b"added");
                test_scan(repository.clone(), staged.clone(), current.clone()).await;
                assert!(holds(&repository, &staged, "added.txt").await);
                std::fs::remove_file(&added).expect("Remove failed");

                let operation =
                    lore_revision::fs::filesystem_provider::FilesystemProvider::begin_operation(
                        repository.file_system().as_ref(),
                    )
                    .await
                    .expect("Failed to start filesystem operation");
                let discards = Arc::new(lore_revision::state::WalkDiscards::default());
                lore_revision::state::diff_filesystem_queuing(
                    &operation,
                    lore_revision::fs::filesystem_provider::FilesystemDiffTree {
                        repository: repository.clone(),
                        state: staged.clone(),
                    },
                    lore_revision::fs::filesystem_provider::FilesystemDiffTree {
                        repository: repository.clone(),
                        state: current.clone(),
                    },
                    None,
                    lore_revision::filter::FilterMode::Full,
                    lore_revision::fs::filesystem_provider::FilesystemDiffIntent::MarkDirty,
                    Arc::new(Vec::new()),
                    Some(discards.clone()),
                )
                .await
                .expect("Failed to diff filesystem")
                .collect()
                .await
                .expect("Failed to diff filesystem");
                operation
                    .finalize()
                    .await
                    .expect("Failed to finish filesystem operation");
                assert!(
                    holds(&repository, &staged, "added.txt").await,
                    "the walk left the discard to its caller"
                );
                discards.apply().await.expect("Discarding failed");
                assert!(!holds(&repository, &staged, "added.txt").await);

                test_file_write(added.as_path(), b"added");
                test_scan(repository.clone(), staged.clone(), current.clone()).await;
                assert!(holds(&repository, &staged, "added.txt").await);
                std::fs::remove_file(&added).expect("Remove failed");
                test_scan(repository.clone(), staged.clone(), current.clone()).await;
                assert!(
                    !holds(&repository, &staged, "added.txt").await,
                    "a walk handed no queue discards once it has drained"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A status scan of many directories at once, with an add reverted in half of them and a
    /// committed file modified in the other half, discards the reverted adds, leaves their
    /// directories unmarked, and keeps the directory they share marked for the modified files.
    #[tokio::test]
    async fn a_parallel_status_scan_discards_reverted_adds_and_keeps_shared_marks() {
        const DIRECTORIES: usize = 32;

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let kept: Vec<String> = (0..DIRECTORIES)
                    .map(|index| format!("shared/d{index:02}/keep.txt"))
                    .collect();
                let (repository, tempdir) = committed_repository("", &kept).await;
                let root = tempdir.to_path_buf();
                let directories: Vec<_> = (0..DIRECTORIES)
                    .map(|index| relative_path(&format!("shared/d{index:02}")))
                    .collect();

                for index in (1..DIRECTORIES).step_by(2) {
                    let added = root.join(format!("shared/d{index:02}/added.txt"));
                    test_file_write(added.as_path(), b"added");
                }
                lore_revision::repository::status::status_boxed(
                    repository.clone(),
                    Some(directories.clone()),
                    scan_status_options(),
                )
                .await
                .expect("Status scan failed");

                for index in 0..DIRECTORIES {
                    if index % 2 == 1 {
                        std::fs::remove_file(root.join(format!("shared/d{index:02}/added.txt")))
                            .expect("Remove failed");
                    } else {
                        let kept = root.join(format!("shared/d{index:02}/keep.txt"));
                        test_file_write(kept.as_path(), b"modified");
                    }
                }
                lore_revision::repository::status::status_boxed(
                    repository.clone(),
                    Some(directories),
                    scan_status_options(),
                )
                .await
                .expect("Status scan failed");

                let (_, staged, _) = State::deserialize_current_and_staged(repository.clone())
                    .await
                    .expect("Deserialize failed");
                let staged = staged.expect("The scans persist what they marked");
                let shared = children_named(&repository, &staged, ROOT_NODE, "shared").await;
                assert_eq!(shared.len(), 1);
                let shared_node = staged
                    .node(repository.clone(), shared[0])
                    .await
                    .expect("The node reads back");
                assert!(shared_node.is_dirty(), "shared holds modified files");
                for index in 0..DIRECTORIES {
                    let name = format!("d{index:02}");
                    let directory = children_named(&repository, &staged, shared[0], &name).await;
                    assert_eq!(directory.len(), 1);
                    let added =
                        children_named(&repository, &staged, directory[0], "added.txt").await;
                    assert!(added.is_empty(), "the reverted add in {name} is discarded");
                    let node = staged
                        .node(repository.clone(), directory[0])
                        .await
                        .expect("The node reads back");
                    assert_eq!(
                        node.is_dirty(),
                        index % 2 == 0,
                        "{name} is marked exactly when a file below it is"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }
}
