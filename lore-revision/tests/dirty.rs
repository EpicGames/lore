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
    use lore_revision::node::NodeExtFlags;
    use lore_revision::node::NodeFlags;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;
    use lore_revision::state::Ext;
    use lore_revision::state::Mark;
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
                    .node_mark(
                        &repository,
                        file_link.node,
                        Mark::dirty(
                            NodeFlags::DirtyModify,
                            Ext::Set(NodeExtFlags::NoFlags),
                            true,
                        ),
                    )
                    .await
                    .expect("node_mark failed");

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
                    .node_mark(
                        &repository,
                        link_a.node,
                        Mark::dirty(
                            NodeFlags::DirtyModify,
                            Ext::Set(NodeExtFlags::NoFlags),
                            true,
                        ),
                    )
                    .await
                    .expect("node_mark a failed");

                // Mark file b as dirty — parent is already dirty, so early-out should fire
                let link_b = state
                    .find_node_link(repository.clone(), "src/b.txt")
                    .await
                    .expect("Find b.txt failed");
                state
                    .node_mark(
                        &repository,
                        link_b.node,
                        Mark::dirty(
                            NodeFlags::DirtyModify,
                            Ext::Set(NodeExtFlags::NoFlags),
                            false,
                        ),
                    )
                    .await
                    .expect("node_mark b failed");

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
                    .node_mark(
                        &repository,
                        file_link.node,
                        Mark::dirty(
                            NodeFlags::DirtyModify,
                            Ext::Set(NodeExtFlags::NoFlags),
                            true,
                        ),
                    )
                    .await
                    .expect("node_mark failed");

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
                    .node_mark(
                        &repository,
                        link.node,
                        Mark::dirty(
                            NodeFlags::DirtyModify,
                            Ext::Set(NodeExtFlags::NoFlags),
                            true,
                        ),
                    )
                    .await
                    .expect("node_mark modify failed");

                let node = state
                    .node(repository.clone(), link.node)
                    .await
                    .expect("Get node failed");
                assert!(node.is_dirty_modify(), "Should be dirty modify");
                assert!(!node.is_dirty_delete(), "Should not be dirty delete");

                // Now re-mark as DirtyDelete — action should replace
                state
                    .node_mark(
                        &repository,
                        link.node,
                        Mark::dirty(
                            NodeFlags::DirtyDelete,
                            Ext::Set(NodeExtFlags::NoFlags),
                            true,
                        ),
                    )
                    .await
                    .expect("node_mark delete failed");

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
                    .node_mark(
                        &repository,
                        link.node,
                        Mark::dirty(
                            NodeFlags::DirtyModify,
                            Ext::Set(NodeExtFlags::NoFlags),
                            true,
                        ),
                    )
                    .await
                    .expect("node_mark failed");

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
                    let mut writer = block.write_for_tests();
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
                state.block_modified_for_tests(block, block_index);
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
                        file::unstage::UnstageOptions::default(),
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

    /// Staging several files under a removed directory in one call stages a delete of only those
    /// files, not of the directories they share or the other files in them.
    #[tokio::test]
    async fn staging_files_under_a_removed_ancestor_deletes_only_those_files() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                std::fs::create_dir_all(fixture.path.join("A/B/M")).expect("Create dir failed");
                for file in [
                    "A/B/M/a1.txt",
                    "A/B/M/a2.txt",
                    "A/B/M/a3.txt",
                    "A/other.txt",
                ] {
                    test_file_write(&fixture.path.join(file), file.as_bytes());
                }
                test_commit_tree(&fixture, "Initial").await;

                std::fs::remove_dir_all(fixture.path.join("A")).expect("Delete failed");

                file::stage::stage(
                    repository.clone(),
                    &fixture.write_token,
                    LoreArray::from_vec(
                        ["A/B/M/a1.txt", "A/B/M/a2.txt"]
                            .iter()
                            .map(|target| {
                                LoreString::from(
                                    fixture.path.join(target).to_string_lossy().as_ref(),
                                )
                            })
                            .collect(),
                    ),
                    StageOptions::default(),
                )
                .await
                .expect("Stage failed");

                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Should have staged state");
                for (path, deleted) in [
                    ("A/B/M/a1.txt", true),
                    ("A/B/M/a2.txt", true),
                    ("A", false),
                    ("A/B", false),
                    ("A/B/M", false),
                    ("A/B/M/a3.txt", false),
                    ("A/other.txt", false),
                ] {
                    let node = state_staged
                        .find_node(repository.clone(), path)
                        .await
                        .unwrap_or_else(|_| panic!("{path} is committed and stays in the tree"));
                    assert_eq!(
                        node.is_staged_delete(),
                        deleted,
                        "{path} staged for delete should be {deleted}"
                    );
                }
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
                        file::unstage::UnstageOptions::default(),
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
    /// added back, and once moved two levels into a directory staging added and removed along with
    /// it.
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
                            // Two levels down, so the check that the new directory drops
                            // cleanly descends past its first.
                            std::fs::create_dir_all(path.join("new/sub"))
                                .expect("Create dir failed");
                            test_file_write(&path.join("new/keep.txt"), b"new");
                            stage_target(&fixture, "new", false).await;
                            std::fs::rename(path.join("Item.txt"), path.join("new/sub/Item.txt"))
                                .expect("Move failed");
                            file::stage::stage_move(
                                repository.clone(),
                                &fixture.write_token,
                                path.join("Item.txt").to_string_lossy().into_owned(),
                                path.join("new/sub/Item.txt").to_string_lossy().into_owned(),
                                StageOptions::default(),
                            )
                            .await
                            .expect("Stage of the move failed");
                            std::fs::remove_dir_all(path.join("new")).expect("Delete failed");
                            stage_target(&fixture, "", false).await;
                            "new/sub/Item.txt"
                        }
                    };

                    let (_, state_staged, _) =
                        State::deserialize_current_and_staged(repository.clone())
                            .await
                            .expect("Deserialize failed");
                    let Some(state_staged) = state_staged else {
                        return Some(format!("{case}: nothing is staged"));
                    };
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

    /// Removes `path` below `root`, a file or a whole directory.
    // Test fixture writes; not subject to repository write-token discipline.
    #[allow(clippy::disallowed_methods)]
    fn remove_path(root: &std::path::Path, path: &str) {
        let path = root.join(path);
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
                    remove_path(&fixture.path, removed);
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
                remove_path(&fixture.path, "dir/temp");
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
                    remove_path(&fixture.path, added);
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

                remove_path(&fixture.path, "item.txt");
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
                remove_path(&fixture.path, "Assets/b.txt");
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
                remove_path(&fixture.path, "dir/phantom.txt");

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
                remove_path(&fixture.path, "new");
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
                assert!(
                    !dest_node.is_dirty_move() && !dest_node.is_staged(),
                    "dest dir takes the base Dirty bit alone, flags {:#x}",
                    dest_node.flags
                );
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
                    file::unstage::UnstageOptions::default(),
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
                    file::unstage::UnstageOptions::default(),
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
                    file::unstage::UnstageOptions::default(),
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

    /// An unstage of a file whose content went back to the committed one clears the directories
    /// above it only where nothing below keeps them dirty: a dirty sibling keeps the directory
    /// dirty, and the anchor.
    #[tokio::test]
    async fn unstage_of_a_reverted_file_keeps_its_parent_dirty_for_a_sibling() {
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
                let stage_options = |scan| StageOptions {
                    case_change: stage::StageCaseChange::Error,
                    node_flags: NodeFlags::NoFlags,
                    file_id: None,
                    no_children: false,
                    scan,
                };

                let subdir = path.join("src");
                std::fs::create_dir_all(&subdir).expect("Create subdir failed");
                std::fs::write(subdir.join("file.txt"), b"original").expect("Write failed");
                std::fs::write(subdir.join("other.txt"), b"other").expect("Write failed");
                file::stage::stage(
                    repository.clone(),
                    write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    stage_options(true),
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

                // Both edited and marked dirty, file.txt alone staged, then reverted on disk.
                std::fs::write(subdir.join("file.txt"), b"modified").expect("Write failed");
                std::fs::write(subdir.join("other.txt"), b"other modified").expect("Write failed");
                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![
                        LoreString::from(subdir.join("file.txt").to_string_lossy().as_ref()),
                        LoreString::from(subdir.join("other.txt").to_string_lossy().as_ref()),
                    ]),
                )
                .await
                .expect("Dirty failed");
                file::stage::stage(
                    repository.clone(),
                    write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        subdir.join("file.txt").to_string_lossy().as_ref(),
                    )]),
                    stage_options(false),
                )
                .await
                .expect("Stage failed");
                std::fs::write(subdir.join("file.txt"), b"original").expect("Write failed");

                file::unstage::unstage(
                    repository.clone(),
                    write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        subdir.join("file.txt").to_string_lossy().as_ref(),
                    )]),
                    file::unstage::UnstageOptions::default(),
                )
                .await
                .expect("Unstage failed");

                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged =
                    state_staged.expect("Anchor should exist (other.txt still dirty)");
                let link_file = state_staged
                    .find_node_link(repository.clone(), "src/file.txt")
                    .await
                    .expect("Find file.txt");
                let node_file = state_staged
                    .node(repository.clone(), link_file.node)
                    .await
                    .expect("Get file.txt");
                assert!(
                    !node_file.is_dirty() && !node_file.is_staged(),
                    "the reverted file ends clean"
                );
                let link_other = state_staged
                    .find_node_link(repository.clone(), "src/other.txt")
                    .await
                    .expect("Find other.txt");
                let node_other = state_staged
                    .node(repository.clone(), link_other.node)
                    .await
                    .expect("Get other.txt");
                assert!(node_other.is_dirty_modify(), "other.txt stays dirty");
                let parent = state_staged
                    .node(repository.clone(), node_other.parent)
                    .await
                    .expect("Get parent");
                assert!(parent.is_dirty(), "src stays dirty for other.txt");
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

    /// A reset of a dirty add discards its node and clears the directory it left only where
    /// nothing below keeps it dirty: a dirty sibling keeps the directory dirty, and the anchor.
    #[tokio::test]
    async fn reset_of_a_dirty_add_keeps_its_parent_dirty_for_a_sibling() {
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

                let subdir = path.join("src");
                std::fs::create_dir_all(&subdir).expect("Create subdir failed");
                std::fs::write(subdir.join("a.txt"), b"aaa").expect("Write failed");
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

                // A modify and an add in one directory, both marked dirty.
                std::fs::write(subdir.join("a.txt"), b"aaa modified longer").expect("Write failed");
                std::fs::write(subdir.join("new.txt"), b"new").expect("Write failed");
                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![
                        LoreString::from(subdir.join("a.txt").to_string_lossy().as_ref()),
                        LoreString::from(subdir.join("new.txt").to_string_lossy().as_ref()),
                    ]),
                )
                .await
                .expect("Dirty failed");

                file::reset::reset(
                    repository.clone(),
                    write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        subdir.join("new.txt").to_string_lossy().as_ref(),
                    )]),
                    LoreString::default(),
                    file::reset::ResetOptions {
                        purge: false,
                        single_node: false,
                    },
                )
                .await
                .expect("Reset new.txt failed");

                assert!(
                    subdir.join("new.txt").exists(),
                    "the reset of a dirty add keeps the file"
                );
                let (_, state_staged, _) =
                    State::deserialize_current_and_staged(repository.clone())
                        .await
                        .expect("Deserialize failed");
                let state_staged = state_staged.expect("Anchor should exist (a.txt still dirty)");
                assert!(
                    state_staged
                        .find_node_link(repository.clone(), "src/new.txt")
                        .await
                        .is_err_and(|error| error.is_node_not_found()),
                    "the dirty add's node is discarded"
                );
                let link_a = state_staged
                    .find_node_link(repository.clone(), "src/a.txt")
                    .await
                    .expect("Find a.txt");
                let node_a = state_staged
                    .node(repository.clone(), link_a.node)
                    .await
                    .expect("Get a");
                assert!(node_a.is_dirty_modify(), "a.txt should still be dirty");
                let parent = state_staged
                    .node(repository.clone(), node_a.parent)
                    .await
                    .expect("Get parent");
                assert!(
                    parent.is_dirty(),
                    "the directory the add left stays dirty for a.txt"
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

                // Same size, so only the content tells it apart
                std::fs::write(path.join("carried.txt"), b"ORIGINAL").expect("Write failed");
                let rebased_revision = lore_revision::state::rebase_staged_state(
                    repository.clone(),
                    staged_revision,
                    current_revision,
                    false,
                )
                .await
                .expect("Rebase failed");
                assert!(
                    rebased_revision.is_some(),
                    "A file differing in content alone should stay dirty"
                );

                std::fs::write(path.join("carried.txt"), b"original").expect("Write failed");
                let rebased_revision = lore_revision::state::rebase_staged_state(
                    repository.clone(),
                    staged_revision,
                    current_revision,
                    false,
                )
                .await
                .expect("Rebase failed");
                assert_eq!(
                    rebased_revision, None,
                    "A file holding the current content should leave nothing to stage"
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

    /// A status of a file a directory-only rule names reports it, and leaves out the directory the
    /// rule names.
    #[tokio::test]
    async fn a_status_of_a_file_a_directory_only_rule_names_reports_it() {
        in_execution(async {
            let (repository, _tempdir) = intermediate_repository().await;
            let reported = status_reported(
                &repository,
                Some(vec![
                    relative_path("src/Intermediate"),
                    relative_path("build/Intermediate"),
                ]),
                true,
            )
            .await;
            assert!(
                reported.contains(&("A src/Intermediate".to_string(), false)),
                "the status of src/Intermediate left it out: {reported:?}"
            );
            assert!(
                reported
                    .iter()
                    .all(|(line, _)| !line.contains("build/Intermediate")),
                "the status reported the directory build/Intermediate: {reported:?}"
            );
        })
        .await;
    }

    /// A status of a dirty file a directory-only rule names, nothing staged, reports its change.
    #[tokio::test]
    async fn a_status_of_a_dirty_file_a_directory_only_rule_names_reports_it() {
        in_execution(async {
            let (ruled, unruled, tempdir) =
                ignoring_repository(&["src/Intermediate"], "Intermediate/\n").await;
            let root = tempdir.to_path_buf();
            test_file_write(root.join("src/Intermediate").as_path(), b"modified");
            file::dirty::dirty(unruled, path_arguments(&root, &["src/Intermediate"]))
                .await
                .expect("Dirty failed");
            let reported =
                status_reported(&ruled, Some(vec![relative_path("src/Intermediate")]), false).await;
            assert_eq!(
                reported,
                [("M src/Intermediate".to_string(), false)],
                "the status of src/Intermediate left out its change"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_dirty_mark_of_a_file_a_directory_only_rule_names_marks_it() {
        in_execution(async {
            let (repository, tempdir) = intermediate_repository().await;
            let paths = path_arguments(&tempdir, &["src/Intermediate", "build/Intermediate"]);
            file::dirty::dirty(repository.clone(), paths)
                .await
                .expect("Dirty failed");
            assert_file_in_and_directory_out(&repository, "a dirty mark of the paths").await;
        })
        .await;
    }

    #[tokio::test]
    async fn a_dirty_mark_of_a_directory_marks_a_file_a_directory_only_rule_names() {
        in_execution(async {
            let (repository, tempdir) = intermediate_repository().await;
            let paths = path_arguments(&tempdir, &["src", "build"]);
            file::dirty::dirty(repository.clone(), paths)
                .await
                .expect("Dirty failed");
            assert_file_in_and_directory_out(&repository, "a dirty mark of the directories").await;
        })
        .await;
    }

    #[tokio::test]
    // Test fixture writes; not subject to repository write-token discipline.
    #[allow(clippy::disallowed_methods)]
    async fn a_status_of_a_removed_file_a_directory_only_rule_names_reports_its_removal() {
        in_execution(async {
            let (ruled, _, tempdir) =
                ignoring_repository(&["src/Intermediate"], "Intermediate/\n").await;
            std::fs::remove_file(tempdir.join("src/Intermediate")).expect("Delete failed");
            let reported =
                status_reported(&ruled, Some(vec![relative_path("src/Intermediate")]), true).await;
            assert_eq!(
                reported,
                [("D src/Intermediate".to_string(), false)],
                "the status of src/Intermediate left out its removal"
            );
        })
        .await;
    }

    #[tokio::test]
    // Test fixture writes; not subject to repository write-token discipline.
    #[allow(clippy::disallowed_methods)]
    async fn a_dirty_mark_of_a_removed_file_a_directory_only_rule_names_marks_its_removal() {
        in_execution(async {
            let (ruled, _, tempdir) =
                ignoring_repository(&["src/Intermediate"], "Intermediate/\n").await;
            std::fs::remove_file(tempdir.join("src/Intermediate")).expect("Delete failed");
            file::dirty::dirty(
                ruled.clone(),
                path_arguments(&tempdir, &["src/Intermediate"]),
            )
            .await
            .expect("Dirty failed");
            assert!(
                next_node(&ruled, "src/Intermediate")
                    .await
                    .is_dirty_delete(),
                "the dirty mark of src/Intermediate did not mark its removal"
            );
        })
        .await;
    }

    /// A dirty mark of a file below a link mount, removed from the working tree, marks its removal
    /// where a directory-only rule names it: the path is asked about as the file the linked tree
    /// holds there.
    #[tokio::test]
    async fn a_dirty_mark_of_a_removed_file_below_a_mount_a_directory_only_rule_names_marks_it() {
        in_execution(async {
            let (repository, tempdir) = committed_repository("Intermediate/\n", &[]).await;
            let root = tempdir.to_path_buf();
            mount_intermediate(
                &repository,
                "vendor",
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            std::fs::create_dir_all(root.join("vendor/src")).expect("Create directory failed");
            file::dirty::dirty(
                repository.clone(),
                path_arguments(&root, &["vendor/src/Intermediate"]),
            )
            .await
            .expect("Dirty failed");
            assert!(
                next_node(&repository, "vendor/src/Intermediate")
                    .await
                    .is_dirty_delete(),
                "the dirty mark of vendor/src/Intermediate did not mark its removal"
            );
        })
        .await;
    }

    /// A dirty move or copy onto the repository root is refused as invalid.
    #[tokio::test]
    async fn a_dirty_move_or_copy_onto_the_root_is_invalid() {
        in_execution(async {
            let (repository, tempdir) = committed_repository("", &[]).await;
            let at = |path: &str| {
                tempdir
                    .to_path_buf()
                    .join(path)
                    .to_string_lossy()
                    .into_owned()
            };
            let moved = file::dirty::dirty_move(repository.clone(), at("seed.txt"), at("")).await;
            let copied = file::dirty::dirty_copy(repository.clone(), at("seed.txt"), at("")).await;
            assert!(
                matches!(moved, Err(file::dirty::DirtyError::InvalidArguments(_)))
                    && matches!(copied, Err(file::dirty::DirtyError::InvalidArguments(_))),
                "a move or copy onto the root was not refused as invalid: {moved:?}, {copied:?}"
            );
        })
        .await;
    }

    /// A dirty copy into a mount, or into new directories below one however deep they go, is
    /// refused as not supported, whether the mount draws from another repository or from this one
    /// at another revision. A copy into new directories outside links finds no parent.
    #[tokio::test]
    async fn a_dirty_copy_into_a_mount_is_not_supported() {
        for from_this_repository in [false, true] {
            in_execution(async move {
                let (repository, tempdir) = committed_repository("", &[]).await;
                let linked_id = if from_this_repository {
                    repository.id
                } else {
                    RepositoryId::from(uuid::Uuid::now_v7())
                };
                mount_intermediate(&repository, "vendor", linked_id).await;
                let at = |path: &str| {
                    tempdir
                        .to_path_buf()
                        .join(path)
                        .to_string_lossy()
                        .into_owned()
                };
                for to in ["vendor/src/copy.txt", "vendor/fresh/deeper/copy.txt"] {
                    let copied =
                        file::dirty::dirty_copy(repository.clone(), at("seed.txt"), at(to)).await;
                    assert!(
                        matches!(copied, Err(file::dirty::DirtyError::NotSupported(_))),
                        "a copy to {to}, mounted from this repository {from_this_repository}, \
                         was not refused as not supported: {copied:?}"
                    );
                }
                let copied = file::dirty::dirty_copy(
                    repository.clone(),
                    at("seed.txt"),
                    at("fresh/deeper/copy.txt"),
                )
                .await;
                assert!(
                    matches!(copied, Err(file::dirty::DirtyError::NodeNotFound(_))),
                    "a copy into new directories outside links found a parent: {copied:?}"
                );
            })
            .await;
        }
    }

    /// A status of a tracked file the working tree replaced with a directory a directory-only rule
    /// names reports the file's removal and nothing the directory holds, naming the path or
    /// scanning the whole tree: the file the tree holds is asked about as a file and the directory
    /// the working tree holds as a directory.
    #[tokio::test]
    async fn a_status_of_a_file_replaced_by_a_directory_a_directory_only_rule_names_reports_its_removal()
     {
        for paths in [Some(vec![relative_path("src/Intermediate")]), None] {
            in_execution(async move {
                let (ruled, _tempdir) = replaced_intermediate_repository("Intermediate/\n").await;
                let named = paths.is_some();
                let reported = status_reported(&ruled, paths, true).await;
                assert_eq!(
                    reported,
                    [("D src/Intermediate".to_string(), false)],
                    "the status, named {named}, reported other than the removal of the file"
                );
            })
            .await;
        }
    }

    /// A diff between a revision holding the file `src/Intermediate` and one holding a directory
    /// there asks about each side as what it holds: `Intermediate/` leaves out the directory and
    /// reports the file's removal, and `Intermediate` with `!Intermediate/` leaves out the file
    /// and reports what the directory holds.
    #[tokio::test]
    async fn a_diff_of_a_file_replaced_by_a_directory_asks_about_each_as_what_it_is() {
        let cases: [(&str, &[(&str, &str)]); 2] = [
            ("Intermediate/\n", &[("D", "src/Intermediate")]),
            (
                "Intermediate\n!Intermediate/\n",
                &[("A", "src/Intermediate"), ("A", "src/Intermediate/a.txt")],
            ),
        ];
        for (ignore, expected) in cases {
            in_execution(async move {
                let (ruled, tempdir) = replaced_intermediate_repository(ignore).await;
                let before = next_tree(&ruled).await.revision();
                stage_paths(&ruled, &tempdir, &["src/Intermediate"], false, true).await;
                let after = commit::commit_boxed(
                    ruled.clone(),
                    ruled
                        .try_write_token()
                        .expect("The repository was opened for writing"),
                    CommitOptions::new("Replace".to_string()),
                )
                .await
                .expect("Commit failed");
                let revision = async |signature| {
                    State::deserialize(ruled.clone(), signature)
                        .await
                        .expect("Deserialize failed")
                };
                let (before, after) = (revision(before).await, revision(after).await);
                assert!(
                    holds(&ruled, &after, "src/Intermediate/a.txt").await,
                    "the forced stage did not commit src/Intermediate/a.txt"
                );
                let changes = lore_revision::state::diff_collect(
                    ruled.clone(),
                    before,
                    ruled.clone(),
                    after,
                    Some(relative_path("src/Intermediate")),
                    lore_revision::filter::FilterMode::Full,
                )
                .await
                .expect("Diff failed");
                let expected: Vec<(String, String)> = expected
                    .iter()
                    .map(|(action, path)| (action.to_string(), path.to_string()))
                    .collect();
                assert_eq!(
                    test_reported(&changes),
                    expected,
                    "the diff under {ignore:?} asked about a side as other than what it holds"
                );
            })
            .await;
        }
    }

    /// Marks `names` below `root` dirty, under force where `forced` says so.
    async fn mark_dirty(
        repository: &Arc<lore_revision::repository::RepositoryContext>,
        root: &std::path::Path,
        names: &[&str],
        forced: bool,
    ) {
        let marking = file::dirty::dirty(repository.clone(), path_arguments(root, names));
        if forced {
            LORE_CONTEXT.scope(forced_execution(), marking).await
        } else {
            marking.await
        }
        .expect("Dirty failed");
    }

    /// A dirty mark of a sparse tree, forced or not, marks the deletion of the file in view the
    /// working tree lacks and of no file the view leaves out, below a removed directory too, and
    /// status lists only that deletion.
    #[tokio::test]
    async fn a_dirty_mark_of_a_sparse_tree_marks_no_deletion_the_view_excludes() {
        const IN_VIEW: &str = "Game/Saved/Config/a.ini";
        let cases: [(&[&str], &[&str], &[&str]); 5] = [
            (&[IN_VIEW], &["."], &["D Game/Saved/Config/a.ini"]),
            (&[IN_VIEW], &["Game"], &["D Game/Saved/Config/a.ini"]),
            (
                &[IN_VIEW],
                &["top.txt", "Engine/Build/b.txt", IN_VIEW],
                &["D Game/Saved/Config/a.ini"],
            ),
            (&["Game/Saved"], &["."], &["D Game/Saved/"]),
            (
                &["Engine/Build", IN_VIEW],
                &["."],
                &["D Game/Saved/Config/a.ini"],
            ),
        ];
        for force in [false, true] {
            for (removed, names, listed) in cases {
                in_execution(async move {
                    let (repository, tempdir) = sparse_repository().await;
                    for path in removed {
                        remove_path(&tempdir, path);
                    }
                    mark_dirty(&repository, &tempdir, names, force).await;
                    let deleted = sparse_files_marked(&repository, Node::is_dirty_delete).await;
                    assert_eq!(
                        deleted,
                        [IN_VIEW],
                        "a dirty mark of {names:?}, forced {force}, with {removed:?} removed, \
                         marked other deletions than of the file in view"
                    );
                    let deletions: Vec<String> = status_reported(&repository, None, false)
                        .await
                        .into_iter()
                        .map(|(entry, _)| entry)
                        .filter(|entry| entry.starts_with("D "))
                        .collect();
                    assert_eq!(
                        deletions, listed,
                        "status after a dirty mark of {names:?}, forced {force}, with \
                         {removed:?} removed"
                    );
                })
                .await;
            }
        }
    }

    /// A dirty mark of a path the view leaves out, forced or not, keeps the add a forced dirty
    /// mark marked there after the working tree loses the file: a deletion driven by absence asks
    /// the view, as a forced stage does.
    #[tokio::test]
    async fn a_dirty_mark_keeps_an_add_past_the_view_the_working_tree_lost() {
        const ADDED: &str = "Engine/Build/new.txt";
        for force in [false, true] {
            in_execution(async move {
                let (repository, tempdir) = sparse_repository().await;
                test_file_write(tempdir.join(ADDED).as_path(), b"new");
                mark_dirty(&repository, &tempdir, &[ADDED], true).await;
                assert!(
                    next_node(&repository, ADDED).await.is_dirty_add(),
                    "the forced dirty mark marked no add of {ADDED}"
                );
                remove_path(&tempdir, ADDED);
                mark_dirty(&repository, &tempdir, &[ADDED], force).await;
                let kept = next_tree(&repository)
                    .await
                    .find_node(repository.clone(), ADDED)
                    .await
                    .is_ok_and(|node| node.is_dirty_add());
                assert!(
                    kept,
                    "a dirty mark, forced {force}, took back the add of {ADDED}"
                );
            })
            .await;
        }
    }

    /// A status scan of untracked files a directory-only rule names asks about each as the file it
    /// is when it resolves the scan targets, so the new directory they share is created once.
    #[tokio::test]
    async fn a_status_scan_of_files_a_directory_only_rule_names_creates_their_directory_once() {
        const FILES: usize = 64;
        in_execution(async {
            let (repository, tempdir) = committed_repository("Intermediate/\n", &[]).await;
            let root = tempdir.to_path_buf();
            let mut paths = Vec::new();
            for index in 0..FILES {
                let directory = format!("fresh/d{index:02}");
                std::fs::create_dir_all(root.join(&directory)).expect("Create directory failed");
                let path = format!("{directory}/Intermediate");
                test_file_write(root.join(&path).as_path(), b"new");
                paths.push(relative_path(&path));
            }
            status_reported(&repository, Some(paths), true).await;
            let tree = next_tree(&repository).await;
            let fresh = children_named(&repository, &tree, ROOT_NODE, "fresh").await;
            assert_eq!(fresh.len(), 1, "one node for the shared directory fresh");
        })
        .await;
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

    /// A marking walk adding a directory it finds new marks the directory above it before it emits
    /// the add, so a reader that stops listening, which ends the walk at that emit, leaves the add
    /// under a dirty directory.
    #[tokio::test]
    async fn a_walk_ended_at_a_new_directorys_emit_leaves_its_parent_dirty() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                std::fs::create_dir_all(fixture.path.join("t")).expect("Create failed");
                test_file_write(fixture.path.join("t/seed.txt").as_path(), b"seed");
                test_commit_tree(&fixture, "Seed").await;
                let (current, staged) = test_anchor_states(&repository).await;
                std::fs::create_dir_all(fixture.path.join("t/n")).expect("Create failed");
                test_file_write(fixture.path.join("t/n/added.txt").as_path(), b"added");

                let operation =
                    lore_revision::fs::filesystem_provider::FilesystemProvider::begin_operation(
                        repository.file_system().as_ref(),
                    )
                    .await
                    .expect("Failed to start filesystem operation");
                // Abandoned at once: the walk's first emit, the new directory's, finds no reader.
                lore_revision::state::diff_filesystem(
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
                )
                .await
                .expect("Failed to diff filesystem")
                .abandon()
                .await;
                operation
                    .finalize()
                    .await
                    .expect("Failed to finish filesystem operation");

                assert!(
                    holds(&repository, &staged, "t/n").await,
                    "the walk reached the add"
                );
                let t = staged
                    .find_node_link(repository.clone(), "t")
                    .await
                    .expect("t is held")
                    .node;
                assert!(
                    staged
                        .node(repository.clone(), t)
                        .await
                        .expect("t reads")
                        .is_dirty()
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Runs a status, scanning as `scan` asks, over a repository whose provider holds its state,
    /// after a file is added there. Answers the adds the summary of the scan counted, if the status
    /// emitted one, and how many adds named as the file it persisted marks for.
    async fn status_over_an_exact_diff(scan: bool) -> (Option<u64>, usize) {
        let (repository, tempdir) = committed_repository("", &[]).await;
        test_file_write(tempdir.path().join("added.txt").as_path(), b"added");
        let (exact, _) = test_holding_context(&repository);
        let adds = Arc::new(std::sync::Mutex::new(None));
        let captured = adds.clone();
        let execution = Arc::new(
            lore_revision::interface::ExecutionContext::new_client_with_user_id(
                lore_revision::interface::LoreGlobalArgs::default(),
                lore_revision::relay::EventDispatcher::new(Some(Box::new(move |event| {
                    if let lore_revision::event::LoreEvent::RepositoryStatusSummary(data) = event {
                        *captured.lock().expect("summary") = Some(data.adds);
                    }
                }))),
                "test-user".to_string(),
            ),
        );

        LORE_CONTEXT
            .scope(
                execution.clone(),
                lore_revision::repository::status::status_boxed(
                    exact.clone(),
                    None,
                    lore_revision::repository::status::StatusOptions {
                        scan,
                        ..scan_status_options()
                    },
                ),
            )
            .await
            .expect("Status failed");
        execution.dispatcher.drain().await;
        let summary = *adds.lock().expect("summary");

        let (_, staged, _) = State::deserialize_current_and_staged(exact.clone())
            .await
            .expect("Deserialize failed");
        let staged = staged.expect("The status persisted no marks");
        let marked = children_named(&exact, &staged, ROOT_NODE, "added.txt")
            .await
            .len();
        (summary, marked)
    }

    /// A status over a provider that holds its state, whose diff is exact, scans without `--scan`
    /// and persists what it marks. Unasked, it reports as a plain status does, with no summary of
    /// the scan; asked for `--scan`, it emits one.
    #[tokio::test]
    async fn a_status_over_an_exact_diff_persists_its_marks_and_summarises_its_scan_when_asked() {
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let (summary, marked) = status_over_an_exact_diff(false).await;
                assert_eq!(
                    summary, None,
                    "A plain status emitted a summary of its scan"
                );
                assert_eq!(marked, 1, "A plain status marked no add");

                let (summary, marked) = status_over_an_exact_diff(true).await;
                assert_eq!(
                    summary,
                    Some(1),
                    "A status asked to scan emitted no summary of its scan"
                );
                assert_eq!(marked, 1, "A status asked to scan marked no add");
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

    /// `base.txt` and `src/file.txt`, staged and committed: a revision for the tests of the
    /// discards below to start from.
    async fn initial_commit(
        repository: &Arc<lore_revision::repository::RepositoryContext>,
        write_token: &lore_revision::repository::RepositoryWriteToken,
        path: &std::path::Path,
    ) {
        std::fs::create_dir_all(path.join("src")).expect("Create src failed");
        test_file_write(&path.join("base.txt"), b"base");
        test_file_write(&path.join("src/file.txt"), b"original");
        file::stage::stage(
            repository.clone(),
            write_token,
            LoreArray::from_vec(vec![LoreString::from(path)]),
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
    }

    /// Writes `newdir/a.txt`, `newdir/sub/b.txt` and `newdir/sub/deeper/c.txt`, and marks
    /// `newdir` dirty: an added directory with a subtree below it, every node a dirty add.
    async fn dirty_new_directory(
        repository: &Arc<lore_revision::repository::RepositoryContext>,
        path: &std::path::Path,
    ) {
        std::fs::create_dir_all(path.join("newdir/sub/deeper")).expect("Create newdir failed");
        test_file_write(&path.join("newdir/a.txt"), b"a");
        test_file_write(&path.join("newdir/sub/b.txt"), b"b");
        test_file_write(&path.join("newdir/sub/deeper/c.txt"), b"c");
        file::dirty::dirty(
            repository.clone(),
            LoreArray::from_vec(vec![LoreString::from(
                path.join("newdir").to_string_lossy().as_ref(),
            )]),
        )
        .await
        .expect("Dirty of newdir failed");
        let (_, staged, _) = State::deserialize_current_and_staged(repository.clone())
            .await
            .expect("Deserialize failed");
        let staged = staged.expect("a staged State");
        for name in ["newdir", "newdir/a.txt", "newdir/sub/deeper/c.txt"] {
            let link = staged
                .find_node_link(repository.clone(), name)
                .await
                .unwrap_or_else(|_| panic!("{name} is in the staged tree"));
            let node = staged
                .node(repository.clone(), link.node)
                .await
                .expect("reads");
            assert!(node.is_dirty_add(), "{name} is a dirty add");
        }
    }

    /// A directory added through dirty and removed from disk before it is staged goes with its
    /// whole subtree at the next dirty: every slot below it is freed, none leaked.
    #[tokio::test]
    async fn dirty_reverted_add_of_a_directory_frees_its_subtree() {
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
                initial_commit(&repository, &fixture.write_token, &path).await;
                dirty_new_directory(&repository, &path).await;

                std::fs::remove_dir_all(path.join("newdir")).expect("Remove newdir failed");
                test_file_write(&path.join("base.txt"), b"base, edited");
                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![
                        LoreString::from(path.join("newdir").to_string_lossy().as_ref()),
                        LoreString::from(path.join("base.txt").to_string_lossy().as_ref()),
                    ]),
                )
                .await
                .expect("Second dirty failed");

                let (_, staged, _) = State::deserialize_current_and_staged(repository.clone())
                    .await
                    .expect("Deserialize failed");
                let staged = staged.expect("the edit of base.txt keeps a staged State");
                assert!(
                    staged
                        .find_node_link(repository.clone(), "newdir")
                        .await
                        .is_err(),
                    "newdir is gone"
                );
                let leaked = test_leaked_slots(&staged, &repository).await;
                assert!(
                    leaked.is_empty(),
                    "every slot of the subtree is freed: {leaked:?} leaked"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A reset of a directory added through dirty drops it from the staged tree with its whole
    /// subtree, keeping the files on disk: every slot below it is freed, none leaked.
    #[tokio::test]
    async fn reset_of_a_dirty_added_directory_frees_its_subtree() {
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
                initial_commit(&repository, write_token, &path).await;
                dirty_new_directory(&repository, &path).await;
                // A change the reset leaves, so the staged State stays.
                test_file_write(
                    &path.join("src/file.txt"),
                    b"modified content that is longer",
                );
                file::dirty::dirty(
                    repository.clone(),
                    LoreArray::from_vec(vec![LoreString::from(
                        path.join("src/file.txt").to_string_lossy().as_ref(),
                    )]),
                )
                .await
                .expect("Dirty of src/file.txt failed");

                file::reset::reset(
                    repository.clone(),
                    write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        path.join("newdir").to_string_lossy().as_ref(),
                    )]),
                    LoreString::default(),
                    file::reset::ResetOptions {
                        purge: false,
                        single_node: false,
                    },
                )
                .await
                .expect("Reset failed");

                assert!(
                    path.join("newdir/sub/deeper/c.txt").exists(),
                    "the files stay"
                );
                let (_, staged, _) = State::deserialize_current_and_staged(repository.clone())
                    .await
                    .expect("Deserialize failed");
                let staged = staged.expect("the edit of src/file.txt keeps a staged State");
                assert!(
                    staged
                        .find_node_link(repository.clone(), "newdir")
                        .await
                        .is_err(),
                    "newdir is gone from the staged tree"
                );
                let leaked = test_leaked_slots(&staged, &repository).await;
                assert!(
                    leaked.is_empty(),
                    "every slot of the subtree is freed: {leaked:?} leaked"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A commit drops a directory added through dirty and never staged, with its whole subtree,
    /// from the revision it writes: every slot below it is freed, none leaked. `newdir/sub` holds
    /// no dirty flags, so the prune's walk does not enter it, and only the discard of `newdir`
    /// frees it and the nodes below it.
    #[tokio::test]
    async fn commit_prunes_an_unstaged_added_directory_with_its_subtree() {
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
                initial_commit(&repository, write_token, &path).await;
                dirty_new_directory(&repository, &path).await;
                test_file_write(&path.join("base.txt"), b"base, edited");
                file::stage::stage(
                    repository.clone(),
                    write_token,
                    LoreArray::from_vec(vec![LoreString::from(
                        path.join("base.txt").to_string_lossy().as_ref(),
                    )]),
                    StageOptions {
                        case_change: stage::StageCaseChange::Error,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: false,
                    },
                )
                .await
                .expect("Stage of base.txt failed");
                let (_, staged, _) = State::deserialize_current_and_staged(repository.clone())
                    .await
                    .expect("Deserialize failed");
                let staged = staged.expect("a staged State");
                let sub = staged
                    .find_node_link(repository.clone(), "newdir/sub")
                    .await
                    .expect("newdir/sub is in the staged tree")
                    .node;
                let block = staged
                    .block(repository.clone(), NodeBlock::index(sub))
                    .await
                    .expect("the block of newdir/sub");
                staged.node_edit_for_tests(&block, sub, Node::clear_dirty_flags);
                let signature = staged
                    .serialize(repository.clone(), write_token)
                    .await
                    .expect("Serializing the staged tree failed");
                lore_revision::instance::store_staged_anchor(&repository, signature)
                    .await
                    .expect("Storing the staged anchor failed");

                commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    CommitOptions::new("Second".to_string()),
                )
                .await
                .expect("Commit failed");

                let (current, _, _) = State::deserialize_current_and_staged(repository.clone())
                    .await
                    .expect("Deserialize failed");
                assert!(
                    current
                        .find_node_link(repository.clone(), "newdir")
                        .await
                        .is_err(),
                    "the revision holds no newdir"
                );
                assert!(
                    current
                        .find_node_link(repository.clone(), "base.txt")
                        .await
                        .is_ok(),
                    "and holds base.txt"
                );
                let leaked = test_leaked_slots(&current, &repository).await;
                assert!(
                    leaked.is_empty(),
                    "every slot of the pruned subtree is freed: {leaked:?} leaked"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// What a status run over `paths` of `repository` reports, each change as the command line
    /// lists it with an unresolved conflict marked `!`, and whether it is staged, in order.
    async fn status_reported(
        repository: &Arc<lore_revision::repository::RepositoryContext>,
        paths: Option<Vec<lore_revision::util::path::RelativePath>>,
        scan: bool,
    ) -> Vec<(String, bool)> {
        let reported = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = reported.clone();
        let execution = Arc::new(
            lore_revision::interface::ExecutionContext::new_client_with_user_id(
                lore_revision::interface::LoreGlobalArgs::default(),
                lore_revision::relay::EventDispatcher::new(Some(Box::new(move |event| {
                    if let lore_revision::event::LoreEvent::RepositoryStatusFile(data) = event {
                        let source = match data.from_path.as_str() {
                            "" => String::new(),
                            from => format!("{from} -> "),
                        };
                        let directory =
                            if data.r#type == lore_revision::interface::LoreNodeType::File {
                                ""
                            } else {
                                "/"
                            };
                        captured.lock().expect("reported").push((
                            format!(
                                "{} {source}{}{directory}{}",
                                data.action_as_string_short(),
                                data.path.as_str(),
                                data.conflict_as_string_short()
                            ),
                            data.flag_staged != 0,
                        ));
                    }
                }))),
                "test-user".to_string(),
            ),
        );
        LORE_CONTEXT
            .scope(
                execution.clone(),
                lore_revision::repository::status::status_boxed(
                    repository.clone(),
                    paths,
                    lore_revision::repository::status::StatusOptions {
                        staged: true,
                        scan,
                        ..scan_status_options()
                    },
                ),
            )
            .await
            .expect("Status failed");
        execution.dispatcher.drain().await;
        let mut reported = std::mem::take(&mut *reported.lock().expect("reported"));
        reported.sort_unstable();
        reported
    }

    /// What a walk from `repository`'s current state to its staged one through `mode` finds, as
    /// sorted action letters and paths, and how many directories it enters.
    async fn staged_walk(
        repository: &Arc<lore_revision::repository::RepositoryContext>,
        mode: lore_revision::filter::FilterMode,
    ) -> (Vec<(String, String)>, u64) {
        let (current, staged, _) = State::deserialize_current_and_staged(repository.clone())
            .await
            .expect("Deserialize failed");
        let staged = staged.expect("The repository holds a staged state");
        let (from, to) = (repository.clone(), repository.clone());
        let mut walk = lore_revision::state::ChangeStream::spawn(async move |changes| {
            lore_revision::state::diff(from, current, to, staged, None, None, &changes, mode).await
        });
        let mut found = Vec::new();
        while let Some(change) = walk.next().await {
            found.push((
                change.action.as_string_short().to_string(),
                change.path().as_str().to_string(),
            ));
        }
        let stats = walk.finish().await.expect("Walk failed");
        found.sort();
        (
            found,
            stats
                .directories_entered
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Status lists a forced edit and delete below a directory an ignore rule names, and a file
    /// there that is only marked dirty: all three are tracked, so the rule has no say over them.
    /// The dirty file is marked through the ruled repository, which `lore dirty` does without
    /// `--force` for a tracked path. Each is also asked about alone.
    #[tokio::test]
    async fn status_lists_forced_and_dirty_changes_below_an_ignored_directory() {
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let (ruled, _, tempdir) = ignoring_repository(
                    &["bin/edit.txt", "bin/gone.txt", "bin/dirty.txt"],
                    "bin/
",
                )
                .await;
                let root = tempdir.to_path_buf();
                test_file_write(root.join("bin/dirty.txt").as_path(), b"marked");
                file::dirty::dirty(ruled.clone(), path_arguments(&root, &["bin/dirty.txt"]))
                    .await
                    .expect("Dirty failed");
                test_file_write(root.join("bin/edit.txt").as_path(), b"edited");
                std::fs::remove_file(root.join("bin/gone.txt")).expect("Remove failed");
                stage_paths(&ruled, &root, &["bin/edit.txt", "bin/gone.txt"], true, true).await;

                let reported = vec![
                    ("D bin/gone.txt".to_string(), true),
                    ("M bin/dirty.txt".to_string(), false),
                    ("M bin/edit.txt".to_string(), true),
                ];
                for scan in [false, true] {
                    assert_eq!(
                        status_reported(&ruled, None, scan).await,
                        reported,
                        "status with scan {scan}"
                    );
                }
                for (path, expected) in [("bin/dirty.txt", 1), ("bin/edit.txt", 2)] {
                    assert_eq!(
                        status_reported(&ruled, Some(vec![relative_path(path)]), false).await,
                        reported[expected..=expected],
                        "status of {path}"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// What a forced staging scan of a sparse tree finds past its view, with the action status
    /// reports and the content the working tree holds: a new file at the top level, a new file in
    /// a directory the tree holds, an edit of a committed file, and a new file in a directory the
    /// tree does not hold.
    const PAST_THE_VIEW: [(&str, &str, &[u8]); 4] = [
        ("A", "new.txt", b"new at the top level"),
        ("A", "Engine/new.txt", b"new in a committed directory"),
        ("M", "Engine/Build/b.txt", b"edited"),
        ("A", "New/new.txt", b"new in a new directory"),
    ];

    /// The [`sparse_repository`] holding [`PAST_THE_VIEW`] in its working tree, staged by a forced
    /// staging scan.
    async fn staged_past_the_view() -> (Arc<lore_revision::repository::RepositoryContext>, TempDir)
    {
        let (repository, tempdir) = sparse_repository().await;
        let root = tempdir.to_path_buf();
        for (_, path, content) in PAST_THE_VIEW {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().expect("A file has a parent"))
                .expect("Create directory failed");
            test_file_write(file.as_path(), content);
        }
        stage_paths(&repository, &root, &["."], true, true).await;
        (repository, tempdir)
    }

    /// The content `revision` holds at `path`, read back from the store.
    async fn committed_content(
        repository: &Arc<lore_revision::repository::RepositoryContext>,
        revision: Hash,
        path: &str,
    ) -> Vec<u8> {
        let node = State::deserialize(repository.clone(), revision)
            .await
            .expect("Deserialize failed")
            .find_node(repository.clone(), path)
            .await
            .unwrap_or_else(|err| panic!("No node at {path}: {err}"));
        immutable::read(
            repository.clone(),
            node.address,
            None,
            immutable::read_options_from_repository(repository),
        )
        .await
        .unwrap_or_else(|err| panic!("Reading {path} failed: {err}"))
        .to_vec()
    }

    /// The status entries of what [`PAST_THE_VIEW`] holds, staged.
    fn staged_past_the_view_entries() -> Vec<(String, bool)> {
        PAST_THE_VIEW
            .iter()
            .map(|(action, path, _)| (format!("{action} {path}"), true))
            .collect()
    }

    /// Status lists what a forced staging scan staged past the view, each also asked about alone.
    /// Beside it, the scan stages the new directory and restages the committed files the working
    /// tree holds, and stages no delete of those it lacks.
    #[tokio::test]
    async fn status_lists_a_forced_stage_scan_past_the_view() {
        in_execution(async {
            let (repository, _tempdir) = staged_past_the_view().await;
            let mut reported = status_reported(&repository, None, false).await;
            reported.sort();
            let mut expected = staged_past_the_view_entries();
            expected.extend(
                [
                    "A New/",
                    "M .loreignore",
                    "M Game/Saved/Config/a.ini",
                    "M seed.txt",
                ]
                .map(|entry| (entry.to_string(), true)),
            );
            expected.sort();
            assert_eq!(reported, expected);
            for (entry, (_, path, _)) in staged_past_the_view_entries()
                .into_iter()
                .zip(PAST_THE_VIEW)
            {
                assert_eq!(
                    status_reported(&repository, Some(vec![relative_path(path)]), false).await,
                    vec![entry],
                    "status of {path}"
                );
            }
        })
        .await;
    }

    /// A plain unstage follows the view, so it leaves what a forced staging scan staged past the
    /// view staged, for the commit to take. A forced unstage takes it back, after which status
    /// lists none of it: an edit left marked dirty past the view is not listed. A file the view
    /// keeps in the new directory is taken back by either.
    #[tokio::test]
    async fn only_a_forced_unstage_takes_back_a_forced_stage_scan_past_the_view() {
        for forced in [false, true] {
            in_execution(async move {
                let (repository, tempdir) = staged_past_the_view().await;
                let in_view = "New/Saved/Config/in.ini";
                write_working_file(&tempdir, in_view, b"in view");
                stage_paths(&repository, &tempdir, &[in_view], false, false).await;
                let in_view_staged = async || {
                    status_reported(&repository, None, false)
                        .await
                        .contains(&(format!("A {in_view}"), true))
                };
                assert!(in_view_staged().await, "{in_view} is not staged");

                unstage_path(&repository, &tempdir, ".", forced).await;
                let mut listed: Vec<_> = status_reported(&repository, None, false)
                    .await
                    .into_iter()
                    .filter(|(entry, _)| {
                        PAST_THE_VIEW.iter().any(|(_, path, _)| {
                            entry.split_once(' ').is_some_and(|(_, at)| at == *path)
                        })
                    })
                    .collect();
                listed.sort();
                let mut staged = staged_past_the_view_entries();
                staged.sort();
                assert_eq!(listed, if forced { Vec::new() } else { staged });
                assert!(!in_view_staged().await, "{in_view} is still staged");
                if forced {
                    return;
                }

                let revision = commit::commit_boxed(
                    repository.clone(),
                    repository
                        .try_write_token()
                        .expect("The repository was opened for writing"),
                    CommitOptions::new("Past the view".to_string()),
                )
                .await
                .expect("Commit failed");
                for (_, path, content) in PAST_THE_VIEW {
                    assert_eq!(
                        committed_content(&repository, revision, path).await,
                        content,
                        "{path}"
                    );
                }
                assert_eq!(
                    revision_tree_faults(&repository, revision).await,
                    Vec::new()
                );
            })
            .await;
        }
    }

    /// A plain unstage of the tree leaves staged a move a forced stage took past the view, and the
    /// directories above it with it: the commit records the move, in a tree whose directories are
    /// their children's rehash.
    #[tokio::test]
    #[ignore = "an unstage clears the directories a staged move left, and the commit keeps their \
                stale addresses"]
    async fn a_plain_unstage_of_the_tree_keeps_a_move_staged_past_the_view() {
        in_execution(async {
            let (repository, tempdir) = sparse_repository().await;
            let root = tempdir.to_path_buf();
            let moved = "Engine/Build/moved.ini";
            LORE_CONTEXT
                .scope(
                    forced_execution(),
                    TreeEdit::Move("Game/Saved/Config/a.ini".to_string(), moved.to_string())
                        .apply(&repository, &root),
                )
                .await;
            unstage_path(&repository, &root, ".", false).await;
            let mut expected = MergeCommitted::unchanged(Some(b"committed"));
            expected.two_parents = false;
            expected.change = Some(("V", false));
            assert_eq!(commit_merge(&repository, moved).await, expected);
            let (current, _) = lore_revision::instance::load_current_anchor_boxed(&repository)
                .await
                .expect("Anchor load failed");
            assert_eq!(revision_tree_faults(&repository, current).await, Vec::new());
        })
        .await;
    }

    /// A plain unstage of the tree keeps the staged tree for a move a forced stage took past the
    /// view from the root, which leaves nothing marked dirty and no directory staged: the commit
    /// records the move.
    #[tokio::test]
    async fn a_plain_unstage_of_the_tree_keeps_the_staged_tree_for_a_move_past_the_view() {
        in_execution(async {
            let (repository, tempdir) = committed_repository("", &["a.txt".to_string()]).await;
            let root = tempdir.to_path_buf();
            test_file_write(
                root.join(lore_revision::repository::DOT_LORE)
                    .join(lore_revision::repository::VIEW_FILTER)
                    .as_path(),
                b"/moved.txt\n",
            );
            let filter = lore_revision::repository::load_filter(&root).expect("Load filter failed");
            let sparse = Arc::new(repository.to_filter_context(filter));
            LORE_CONTEXT
                .scope(
                    forced_execution(),
                    TreeEdit::Move("a.txt".to_string(), "moved.txt".to_string())
                        .apply(&sparse, &root),
                )
                .await;
            unstage_path(&sparse, &root, ".", false).await;
            let mut expected = MergeCommitted::unchanged(Some(b"committed"));
            expected.two_parents = false;
            expected.change = Some(("V", false));
            assert_eq!(commit_merge(&sparse, "moved.txt").await, expected);
            let (current, _) = lore_revision::instance::load_current_anchor_boxed(&sparse)
                .await
                .expect("Anchor load failed");
            assert_eq!(revision_tree_faults(&sparse, current).await, Vec::new());
        })
        .await;
    }

    /// A commit takes the content the working tree holds where a forced staging scan staged it
    /// past the view.
    #[tokio::test]
    async fn a_commit_takes_a_forced_stage_scan_past_the_view_from_the_working_tree() {
        in_execution(async {
            let (repository, _tempdir) = staged_past_the_view().await;
            let revision = commit::commit_boxed(
                repository.clone(),
                repository
                    .try_write_token()
                    .expect("The repository was opened for writing"),
                CommitOptions::new("Past the view".to_string()),
            )
            .await
            .expect("Commit failed");
            for (_, path, content) in PAST_THE_VIEW {
                assert_eq!(
                    committed_content(&repository, revision, path).await,
                    content,
                    "{path}"
                );
            }
        })
        .await;
    }

    /// The file of the sparse fixture a merge changes past the view, what the merge brings to it,
    /// and what the working tree holds there afterwards, of another size.
    const MERGED_PAST_THE_VIEW: (&str, &[u8], &[u8]) =
        ("Engine/Build/b.txt", b"merged", b"edited after the merge");

    /// A file past the view the sparse fixture does not hold, which a merge adds.
    const ADDED_PAST_THE_VIEW: &str = "Engine/Build/added.txt";

    /// A change a branch of the merge fixture makes to the tree.
    enum TreeEdit {
        /// Writes the content to the file at the path.
        Write(String, &'static [u8]),
        /// Removes the file or the directory at the path.
        Remove(String),
        /// Moves the file at the first path to the second.
        Move(String, String),
    }

    impl TreeEdit {
        /// Makes the edit in the working tree below `root`, and stages it.
        async fn apply(
            &self,
            repository: &Arc<lore_revision::repository::RepositoryContext>,
            root: &std::path::Path,
        ) {
            match self {
                TreeEdit::Write(path, content) => {
                    write_working_file(root, path, content);
                    stage_paths(repository, root, &[path], true, false).await;
                }
                TreeEdit::Remove(path) => {
                    let full = root.join(path);
                    // Test fixture writes; not subject to repository write-token discipline.
                    #[allow(clippy::disallowed_methods)]
                    if full.is_dir() {
                        std::fs::remove_dir_all(&full).expect("Remove failed");
                    } else {
                        std::fs::remove_file(&full).expect("Remove failed");
                    }
                    stage_paths(repository, root, &[path], true, false).await;
                }
                TreeEdit::Move(from, to) => {
                    let target = root.join(to);
                    std::fs::create_dir_all(target.parent().expect("A file has a parent"))
                        .expect("Create directory failed");
                    // Test fixture writes; not subject to repository write-token discipline.
                    #[allow(clippy::disallowed_methods)]
                    std::fs::rename(root.join(from), &target).expect("Rename failed");
                    lore_revision::file::stage::stage_move(
                        repository.clone(),
                        repository
                            .try_write_token()
                            .expect("The repository was opened for writing"),
                        root.join(from).to_string_lossy().into_owned(),
                        target.to_string_lossy().into_owned(),
                        StageOptions::default(),
                    )
                    .await
                    .expect("Stage move failed");
                }
            }
        }

        /// The paths the edit writes to, which the sparse fixture's tree may not hold.
        fn targets(&self) -> Option<&str> {
            match self {
                TreeEdit::Write(path, _) | TreeEdit::Move(_, path) => Some(path),
                TreeEdit::Remove(_) => None,
            }
        }
    }

    /// A repository read through the view of [`sparse_repository`], with a merge in progress of a
    /// feature branch into the main branch. The feature branch's revisions make `feature`, one
    /// revision for each list, and the main branch's revision makes `main` and an edit of its own.
    async fn merge_in_progress(
        feature: Vec<Vec<TreeEdit>>,
        main: Vec<TreeEdit>,
    ) -> (Arc<lore_revision::repository::RepositoryContext>, TempDir) {
        let files: Vec<String> = SPARSE_FILES.iter().map(|file| file.to_string()).collect();
        let (repository, tempdir) = committed_repository("", &files).await;
        let root = tempdir.to_path_buf();
        let token = repository
            .try_write_token()
            .expect("The repository was opened for writing");
        let commit = async || {
            commit::commit_boxed(repository.clone(), token, CommitOptions::new(String::new()))
                .await
                .expect("Commit failed");
        };

        let (base, main_branch) = lore_revision::instance::load_current_anchor_boxed(&repository)
            .await
            .expect("Anchor load failed");
        lore_revision::branch::create::create(
            repository.clone(),
            token,
            "feature".to_string(),
            None,
            String::new(),
            false,
        )
        .await
        .expect("Branch create failed");
        let (_, feature_branch) = lore_revision::instance::load_current_anchor_boxed(&repository)
            .await
            .expect("Anchor load failed");
        for revision in &feature {
            for edit in revision {
                edit.apply(&repository, &root).await;
            }
            commit().await;
        }

        lore_revision::instance::store_current_anchor_branch(&repository, main_branch)
            .await
            .expect("Anchor store failed");
        lore_revision::instance::store_current_anchor(&repository, base)
            .await
            .expect("Anchor store failed");
        let added: Vec<String> = feature
            .iter()
            .flatten()
            .chain(&main)
            .filter_map(TreeEdit::targets)
            .filter(|path| !SPARSE_FILES.contains(path))
            .map(str::to_string)
            .collect();
        for path in &added {
            // Test fixture writes; not subject to repository write-token discipline.
            #[allow(clippy::disallowed_methods)]
            let _ = std::fs::remove_file(root.join(path));
        }
        for file in SPARSE_FILES {
            write_working_file(&root, file, b"committed");
        }
        for edit in main
            .iter()
            .chain([&TreeEdit::Write("seed.txt".to_string(), b"main")])
        {
            edit.apply(&repository, &root).await;
        }
        commit().await;

        test_file_write(
            root.join(lore_revision::repository::DOT_LORE)
                .join(lore_revision::repository::VIEW_FILTER)
                .as_path(),
            b"/*\n!*/Saved/Config/\n",
        );
        let filter = lore_revision::repository::load_filter(&root).expect("Load filter failed");
        for path in SPARSE_FILES
            .iter()
            .copied()
            .chain(added.iter().map(String::as_str))
        {
            let relative = lore_revision::util::path::RelativePath::new_from_initial_path(path)
                .expect("A relative path");
            if filter.excludes_tree(&relative, false, lore_revision::filter::FilterMode::View) {
                // Test fixture writes; not subject to repository write-token discipline.
                #[allow(clippy::disallowed_methods)]
                let _ = std::fs::remove_file(root.join(path));
            }
        }
        let sparse = Arc::new(repository.to_filter_context(filter));
        let offline = Arc::new(
            lore_revision::interface::ExecutionContext::new_client_with_user_id(
                lore_revision::interface::LoreGlobalArgs::default().set_offline(),
                lore_revision::relay::EventDispatcher::no_dispatch(),
                "test-user".to_string(),
            ),
        );
        LORE_CONTEXT
            .scope(
                offline,
                Box::pin(lore_revision::branch::merge::merge_start(
                    sparse.clone(),
                    token,
                    feature_branch,
                    lore_revision::branch::merge::MergeStartOptions {
                        message: String::new(),
                        link_messages: std::collections::HashMap::new(),
                        no_commit: true,
                        scope: lore_revision::branch::merge::MergeScope::MainOnly,
                        inherit_metadata: Default::default(),
                    },
                )),
            )
            .await
            .expect("Merge failed");
        (sparse, tempdir)
    }

    /// A repository read through the view of [`sparse_repository`], with a merge in progress that
    /// staged a feature branch's write of `merged` to `path`, beside an edit of the main branch's.
    async fn merged_past_the_view(
        path: &str,
        merged: &'static [u8],
    ) -> (Arc<lore_revision::repository::RepositoryContext>, TempDir) {
        merge_in_progress(
            vec![vec![TreeEdit::Write(path.to_string(), merged)]],
            Vec::new(),
        )
        .await
    }

    /// What a merge commit holds at a path: the content and the size its node records, none where
    /// it holds no file there, whether the commit has two parents, and the action its delta
    /// records for the node there, or for the node the current revision held there where the
    /// commit holds none, with whether it records a merge, none where it records no change.
    #[derive(Debug, PartialEq)]
    struct MergeCommitted {
        file: Option<(String, u64)>,
        two_parents: bool,
        change: Option<(&'static str, bool)>,
    }

    impl MergeCommitted {
        /// A merge commit holding `content` at a path, which its delta records as `action`,
        /// merged.
        fn merged(content: &[u8], action: &'static str) -> Self {
            MergeCommitted {
                file: Some((
                    String::from_utf8_lossy(content).into_owned(),
                    content.len() as u64,
                )),
                two_parents: true,
                change: Some((action, true)),
            }
        }

        /// A merge commit holding `content` at a path, which its delta records no change of.
        fn unchanged(content: Option<&[u8]>) -> Self {
            MergeCommitted {
                file: content.map(|content| {
                    (
                        String::from_utf8_lossy(content).into_owned(),
                        content.len() as u64,
                    )
                }),
                two_parents: true,
                change: None,
            }
        }
    }

    /// The letter `action`, a node delta's, stands for.
    fn action_letter(action: u16) -> &'static str {
        [
            (lore_revision::change::FileAction::Add, "A"),
            (lore_revision::change::FileAction::Keep, "M"),
            (lore_revision::change::FileAction::Move, "V"),
            (lore_revision::change::FileAction::Delete, "D"),
        ]
        .into_iter()
        .find(|(known, _)| action == *known as u16)
        .map_or("other", |(_, letter)| letter)
    }

    /// Commits what `repository` holds staged, and answers what the revision holds at `path`.
    async fn commit_merge(
        repository: &Arc<lore_revision::repository::RepositoryContext>,
        path: &str,
    ) -> MergeCommitted {
        let (current, _) = lore_revision::instance::load_current_anchor_boxed(repository)
            .await
            .expect("Anchor load failed");
        let held = State::deserialize(repository.clone(), current)
            .await
            .expect("Deserialize failed")
            .find_node_link(repository.clone(), path)
            .await
            .ok()
            .map(|link| link.node);
        let revision = commit::commit_boxed(
            repository.clone(),
            repository
                .try_write_token()
                .expect("The repository was opened for writing"),
            CommitOptions::new("Merge".to_string()),
        )
        .await
        .expect("Commit failed");
        let state = State::deserialize(repository.clone(), revision)
            .await
            .expect("Deserialize failed");
        let link = state.find_node_link(repository.clone(), path).await.ok();
        let change = match link.as_ref().map(|link| link.node).or(held) {
            Some(node) => state
                .node_delta(repository.clone(), node)
                .await
                .expect("The delta reads")
                .map(|delta| {
                    (
                        action_letter(delta.action),
                        lore_revision::change::Flags::from_bits_retain(delta.flags)
                            .contains(lore_revision::change::Flags::Merge),
                    )
                }),
            None => None,
        };
        let file = match link {
            Some(link) => Some((
                String::from_utf8_lossy(&committed_content(repository, revision, path).await)
                    .into_owned(),
                state
                    .node(repository.clone(), link.node)
                    .await
                    .expect("The node reads")
                    .size,
            )),
            None => None,
        };
        MergeCommitted {
            file,
            two_parents: !state.parent_other().is_zero(),
            change,
        }
    }

    /// Writes `content` to the file at `path` below `root`, creating its directory.
    fn write_working_file(root: &std::path::Path, path: &str, content: &[u8]) {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().expect("A file has a parent"))
            .expect("Create directory failed");
        test_file_write(file.as_path(), content);
    }

    /// Unstages `path` below `root`, under force where `forced` says so.
    async fn unstage_path(
        repository: &Arc<lore_revision::repository::RepositoryContext>,
        root: &std::path::Path,
        path: &str,
        forced: bool,
    ) {
        let unstage = file::unstage::unstage(
            repository.clone(),
            repository
                .try_write_token()
                .expect("The repository was opened for writing"),
            path_arguments(root, &[path]),
            file::unstage::UnstageOptions::default(),
        );
        if forced {
            LORE_CONTEXT.scope(forced_execution(), unstage).await
        } else {
            unstage.await
        }
        .expect("Unstage failed");
    }

    /// A forced staging scan over a file a merge staged past the view resolves the merge with the
    /// working tree's file: the merge commit takes its content and size, and records it merged,
    /// an add as an add. An empty file brings its mode, which the commit takes from the node.
    #[tokio::test]
    async fn a_forced_stage_scan_over_a_merge_past_the_view_takes_the_working_tree() {
        let (modified, merged, edited) = MERGED_PAST_THE_VIEW;
        for (path, action) in [(modified, "M"), (ADDED_PAST_THE_VIEW, "A")] {
            in_execution(async move {
                let (repository, tempdir) = merged_past_the_view(path, merged).await;
                write_working_file(&tempdir, path, edited);
                stage_paths(&repository, &tempdir, &["."], true, true).await;
                assert_eq!(
                    commit_merge(&repository, path).await,
                    MergeCommitted::merged(edited, action),
                    "{path}"
                );
            })
            .await;
        }
        #[cfg(unix)]
        in_execution(async move {
            use std::os::unix::fs::PermissionsExt;

            let (repository, tempdir) = merged_past_the_view(modified, merged).await;
            let file = tempdir.join(modified);
            write_working_file(&tempdir, modified, b"");
            // Test fixture writes; not subject to repository write-token discipline.
            #[allow(clippy::disallowed_methods)]
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
                .expect("Setting the mode failed");
            stage_paths(&repository, &tempdir, &["."], true, true).await;
            assert_eq!(
                commit_merge(&repository, modified).await,
                MergeCommitted::merged(b"", "M")
            );
            assert!(committed_executable(&repository, modified).await);
        })
        .await;
    }

    /// Whether the file the current revision of `repository` holds at `path` is executable.
    #[cfg(unix)]
    async fn committed_executable(
        repository: &Arc<lore_revision::repository::RepositoryContext>,
        path: &str,
    ) -> bool {
        let (revision, _) = lore_revision::instance::load_current_anchor_boxed(repository)
            .await
            .expect("Anchor load failed");
        let node = State::deserialize(repository.clone(), revision)
            .await
            .expect("Deserialize failed")
            .find_node(repository.clone(), path)
            .await
            .expect("The revision holds the file");
        let executable = lore_revision::node::NodeFileMode::Executable.bits();
        node.mode & executable == executable
    }

    /// A second forced stage of a file a forced stage took over a merge past the view, emptied and
    /// made executable since, takes the working tree's file again, its mode included: the merge
    /// commit holds an empty executable file.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_second_forced_stage_over_a_merge_past_the_view_takes_an_emptied_file_with_its_mode()
    {
        use std::os::unix::fs::PermissionsExt;

        in_execution(async {
            let (modified, merged, edited) = MERGED_PAST_THE_VIEW;
            let (repository, tempdir) = merged_past_the_view(modified, merged).await;
            write_working_file(&tempdir, modified, edited);
            stage_paths(&repository, &tempdir, &["."], true, true).await;
            write_working_file(&tempdir, modified, b"");
            // Test fixture writes; not subject to repository write-token discipline.
            #[allow(clippy::disallowed_methods)]
            std::fs::set_permissions(
                tempdir.join(modified),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("Setting the mode failed");
            stage_paths(&repository, &tempdir, &["."], true, true).await;
            assert_eq!(
                commit_merge(&repository, modified).await,
                MergeCommitted::merged(b"", "M")
            );
            assert!(committed_executable(&repository, modified).await);
        })
        .await;
    }

    /// A forced unstage of a file a forced stage took from the working tree over a merge past the
    /// view takes the current revision's file back, as the stage dropped the merged content: the
    /// merge commit holds the current revision's file there, unchanged, or no file where the merge
    /// added it, and no file whose content it did not store.
    #[tokio::test]
    async fn a_forced_unstage_takes_back_a_resolved_merge_past_the_view() {
        let (modified, merged, edited) = MERGED_PAST_THE_VIEW;
        for (path, expected) in [
            (modified, MergeCommitted::unchanged(Some(b"committed"))),
            (ADDED_PAST_THE_VIEW, MergeCommitted::unchanged(None)),
        ] {
            in_execution(async move {
                let (repository, tempdir) = merged_past_the_view(path, merged).await;
                write_working_file(&tempdir, path, edited);
                stage_paths(&repository, &tempdir, &["."], true, true).await;
                unstage_path(&repository, &tempdir, path, true).await;
                assert_eq!(commit_merge(&repository, path).await, expected, "{path}");
                let (current, _) = lore_revision::instance::load_current_anchor_boxed(&repository)
                    .await
                    .expect("Anchor load failed");
                assert_eq!(
                    revision_tree_faults(&repository, current).await,
                    Vec::new(),
                    "{path}"
                );
            })
            .await;
        }
    }

    /// Without a stage, the merge commit takes the content the merge staged past the view, whatever
    /// the working tree holds there, and a dirty mark there changes nothing.
    #[tokio::test]
    async fn a_merge_past_the_view_commits_the_merged_content_unless_staged() {
        for mark in [false, true] {
            in_execution(async move {
                let (path, merged, edited) = MERGED_PAST_THE_VIEW;
                let (repository, tempdir) = merged_past_the_view(path, merged).await;
                write_working_file(&tempdir, path, edited);
                if mark {
                    LORE_CONTEXT
                        .scope(
                            forced_execution(),
                            file::dirty::dirty(
                                repository.clone(),
                                path_arguments(&tempdir, &[path]),
                            ),
                        )
                        .await
                        .expect("Forced dirty failed");
                }
                assert_eq!(
                    commit_merge(&repository, path).await,
                    MergeCommitted::merged(merged, "M"),
                    "marked dirty: {mark}"
                );
            })
            .await;
        }
    }

    /// A merge that empties a file past the view, or adds an empty one there, stages a zero hash
    /// with size 0, an empty file, and the merge commit takes it from the node: the working tree
    /// holds no file there to read.
    #[tokio::test]
    async fn a_merge_past_the_view_commits_an_empty_file() {
        let (modified, _, _) = MERGED_PAST_THE_VIEW;
        for (path, action) in [(modified, "M"), (ADDED_PAST_THE_VIEW, "A")] {
            in_execution(async move {
                let (repository, _tempdir) = merged_past_the_view(path, b"").await;
                assert_eq!(
                    commit_merge(&repository, path).await,
                    MergeCommitted::merged(b"", action),
                    "{path}"
                );
            })
            .await;
        }
    }

    /// What a merge's feature branch does at the path a case of the merge table asks about.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum TableChange {
        None,
        Modify,
        Add,
        Delete,
        Move,
        AddAddTheirs,
        Empty,
        DeleteReadd,
        AddDirectory,
        MoveIntoNewDirectory,
        DeleteDirectory,
        ModifyConflict,
    }

    /// What the user does at that path after the merge and before its commit, short of an
    /// unstage.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum TableAction {
        Nothing,
        DirtyMark,
        /// A forced staging scan of the tree, over the user's edit.
        StageOther,
        /// A forced staging scan of the tree, over the content the merge staged.
        StageSame,
        /// A forced staging scan of the tree, over an empty file.
        StageEmpty,
        /// A forced stage of the path alone, over the user's edit.
        StageNamed,
        /// A staging scan of the tree, over the user's edit.
        StagePlain,
    }

    /// What the user unstages after that, if anything.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum TableUnstage {
        None,
        Path,
        Directory,
        Root,
    }

    /// What a user writes over a merged file in the merge table.
    const TABLE_EDIT: &[u8] = b"edited after the merge";

    /// A case of the merge table, set up to its commit: the repository, its working tree, the path
    /// the case asks about, the path a move took the file from, the directory holding the path,
    /// what the current revision holds at the path, or at the path a move took the file from, and
    /// what the merge staged at the path.
    struct TableMerge {
        repository: Arc<lore_revision::repository::RepositoryContext>,
        tempdir: TempDir,
        path: String,
        source: Option<String>,
        directory: String,
        current: Option<&'static [u8]>,
        merged: Option<&'static [u8]>,
    }

    /// The paths and contents of a [`TableMerge`], in the order of its fields.
    type TableCase = (
        String,
        Option<String>,
        String,
        Option<&'static [u8]>,
        Option<&'static [u8]>,
    );

    /// A merge in progress over the sparse fixture whose feature branch made `change` in view or
    /// past it, and the main branch an edit of its own. A conflict in view is resolved, to theirs
    /// for an add/add one and to the user's file for a modify/modify one.
    async fn table_merge(change: TableChange, in_view: bool) -> TableMerge {
        let (directory, existing) = if in_view {
            ("Game/Saved/Config", "Game/Saved/Config/a.ini")
        } else {
            ("Engine/Build", "Engine/Build/b.txt")
        };
        let added = format!("{directory}/added.txt");
        let moved = format!("{directory}/moved.txt");
        let new_directory = format!("{directory}/New");
        let new_added = format!("{new_directory}/added.txt");
        let new_moved = format!("{new_directory}/moved.txt");
        let existing_edit = |content: &'static [u8]| TreeEdit::Write(existing.to_string(), content);
        let (feature, main) = match change {
            TableChange::None => (
                vec![vec![TreeEdit::Write("top.txt".to_string(), b"feature")]],
                Vec::new(),
            ),
            TableChange::Modify => (vec![vec![existing_edit(b"merged")]], Vec::new()),
            TableChange::Empty => (vec![vec![existing_edit(b"")]], Vec::new()),
            TableChange::Add => (
                vec![vec![TreeEdit::Write(added.clone(), b"merged")]],
                Vec::new(),
            ),
            TableChange::AddAddTheirs => (
                vec![vec![TreeEdit::Write(added.clone(), b"theirs")]],
                vec![TreeEdit::Write(added.clone(), b"mine")],
            ),
            TableChange::Delete => (
                vec![vec![TreeEdit::Remove(existing.to_string())]],
                Vec::new(),
            ),
            TableChange::DeleteReadd => (
                vec![
                    vec![TreeEdit::Remove(existing.to_string())],
                    vec![existing_edit(b"merged")],
                ],
                Vec::new(),
            ),
            TableChange::Move => (
                vec![vec![TreeEdit::Move(existing.to_string(), moved.clone())]],
                Vec::new(),
            ),
            TableChange::AddDirectory => (
                vec![vec![TreeEdit::Write(new_added.clone(), b"merged")]],
                Vec::new(),
            ),
            TableChange::MoveIntoNewDirectory => (
                vec![vec![TreeEdit::Move(
                    existing.to_string(),
                    new_moved.clone(),
                )]],
                Vec::new(),
            ),
            TableChange::DeleteDirectory => (
                vec![vec![TreeEdit::Remove(directory.to_string())]],
                Vec::new(),
            ),
            TableChange::ModifyConflict => (
                vec![vec![existing_edit(b"theirs")]],
                vec![existing_edit(b"mine")],
            ),
        };
        let (repository, tempdir) = merge_in_progress(feature, main).await;
        let root = tempdir.to_path_buf();
        let token = repository
            .try_write_token()
            .expect("The repository was opened for writing");
        if in_view && change == TableChange::AddAddTheirs {
            lore_revision::branch::merge::merge_resolve_theirs(
                repository.clone(),
                token,
                path_arguments(&root, &[&added]),
            )
            .await
            .expect("Resolve theirs failed");
        }
        if in_view && change == TableChange::ModifyConflict {
            write_working_file(&root, existing, b"resolved");
            lore_revision::branch::merge::merge_resolve(
                repository.clone(),
                token,
                path_arguments(&root, &[existing]),
                lore_revision::branch::merge::MergeType::BranchMerge,
            )
            .await
            .expect("Resolve failed");
        }

        let committed: Option<&'static [u8]> = Some(b"committed");
        let (path, source, directory, current, merged): TableCase = match change {
            TableChange::None => (
                existing.to_string(),
                None,
                directory.to_string(),
                committed,
                committed,
            ),
            TableChange::Modify | TableChange::DeleteReadd => (
                existing.to_string(),
                None,
                directory.to_string(),
                committed,
                Some(b"merged"),
            ),
            TableChange::Empty => (
                existing.to_string(),
                None,
                directory.to_string(),
                committed,
                Some(b""),
            ),
            TableChange::Delete | TableChange::DeleteDirectory => (
                existing.to_string(),
                None,
                directory.to_string(),
                committed,
                None,
            ),
            TableChange::Add => (added, None, directory.to_string(), None, Some(b"merged")),
            TableChange::AddAddTheirs => (
                added,
                None,
                directory.to_string(),
                Some(b"mine"),
                Some(b"theirs"),
            ),
            TableChange::Move => (
                moved,
                Some(existing.to_string()),
                directory.to_string(),
                committed,
                committed,
            ),
            TableChange::AddDirectory => (new_added, None, new_directory, None, Some(b"merged")),
            TableChange::MoveIntoNewDirectory => (
                new_moved,
                Some(existing.to_string()),
                new_directory,
                committed,
                committed,
            ),
            TableChange::ModifyConflict => (
                existing.to_string(),
                None,
                directory.to_string(),
                Some(b"mine"),
                Some(if in_view { b"resolved" } else { b"theirs" }),
            ),
        };
        TableMerge {
            repository,
            tempdir,
            path,
            source,
            directory,
            current,
            merged,
        }
    }

    /// What is wrong with a revision's tree, as [`revision_tree_faults`] finds it.
    #[derive(Debug, PartialEq)]
    enum TreeFault {
        /// A node that keeps the staged or dirty flags it holds.
        Marked(lore_revision::node::NodeID, u16),
        /// A file with a zero hash and the nonzero size it holds, content no commit stored.
        Unstored(lore_revision::node::NodeID, u64),
        /// A directory whose address is not its children's rehash.
        NotRehashed(lore_revision::node::NodeID),
    }

    /// What is wrong with the tree of `revision`: a node that keeps a staged or dirty mark, a file
    /// with a zero hash and a nonzero size, which is content no commit stored, or a directory whose
    /// address is not its children's rehash, the root's against the hash the revision holds for it.
    /// A tree that keeps a mark is not rehashed, as a rehash refuses a staged node.
    async fn revision_tree_faults(
        repository: &Arc<lore_revision::repository::RepositoryContext>,
        revision: Hash,
    ) -> Vec<TreeFault> {
        let state = State::deserialize(repository.clone(), revision)
            .await
            .expect("Deserialize failed");
        let marks = NodeFlags::Staged | NodeFlags::Dirty;
        let mut faults = Vec::new();
        let mut directories = Vec::new();
        let mut pending = vec![ROOT_NODE];
        while let Some(directory) = pending.pop() {
            for child in state
                .node_children(repository.clone(), directory)
                .await
                .expect("The children read")
            {
                let node = state
                    .node(repository.clone(), child)
                    .await
                    .expect("The node reads");
                if NodeFlags::from_bits_retain(node.flags).intersects(marks) {
                    faults.push(TreeFault::Marked(child, node.flags));
                }
                if node.is_directory() {
                    directories.push((child, node.address.hash));
                    pending.push(child);
                } else if node.is_file() && node.address.hash.is_zero() && node.size != 0 {
                    faults.push(TreeFault::Unstored(child, node.size));
                }
            }
        }
        if !faults.is_empty() {
            return faults;
        }
        let root_hash = state
            .tree(repository.clone())
            .await
            .expect("The tree reads")
            .hash_root;
        for (directory, _) in &directories {
            state
                .node_mark(
                    repository,
                    *directory,
                    Mark::staged(NodeFlags::Staged, true),
                )
                .await
                .expect("Marking the directory failed");
        }
        commit::rehash_directory(repository.clone(), state.clone(), ROOT_NODE)
            .await
            .expect("Rehash failed");
        for (directory, hash) in directories.into_iter().chain([(ROOT_NODE, root_hash)]) {
            let rehashed = state
                .node(repository.clone(), directory)
                .await
                .expect("The node reads")
                .address
                .hash;
            if rehashed != hash {
                faults.push(TreeFault::NotRehashed(directory));
            }
        }
        faults
    }

    /// Runs one case of the merge table, and answers what the merge commit holds at its path and
    /// what is wrong with its tree and, for a move, at the path the move took the file from.
    async fn table_case(
        change: TableChange,
        in_view: bool,
        action: TableAction,
        unstage: TableUnstage,
        forced: bool,
    ) -> (TableMerge, MergeCommitted, Vec<String>) {
        let case = table_merge(change, in_view).await;
        let (repository, root, path) = (&case.repository, case.tempdir.to_path_buf(), &case.path);
        let written: Option<&[u8]> = match action {
            TableAction::Nothing => None,
            TableAction::DirtyMark
            | TableAction::StageOther
            | TableAction::StageNamed
            | TableAction::StagePlain => Some(TABLE_EDIT),
            TableAction::StageSame => case.merged,
            TableAction::StageEmpty => Some(b""),
        };
        if let Some(content) = written {
            write_working_file(&root, path, content);
        }
        match action {
            TableAction::Nothing => {}
            TableAction::DirtyMark => {
                LORE_CONTEXT
                    .scope(
                        forced_execution(),
                        file::dirty::dirty(repository.clone(), path_arguments(&root, &[path])),
                    )
                    .await
                    .expect("Forced dirty failed");
            }
            TableAction::StageOther | TableAction::StageSame | TableAction::StageEmpty => {
                stage_paths(repository, &root, &["."], true, true).await;
            }
            TableAction::StageNamed => stage_paths(repository, &root, &[path], false, true).await,
            TableAction::StagePlain => stage_paths(repository, &root, &["."], true, false).await,
        }
        match unstage {
            TableUnstage::None => {}
            TableUnstage::Path => unstage_path(repository, &root, path, forced).await,
            TableUnstage::Directory => {
                unstage_path(repository, &root, &case.directory, forced).await;
            }
            TableUnstage::Root => unstage_path(repository, &root, ".", forced).await,
        }
        let committed = commit_merge(repository, path).await;
        let (current, _) = lore_revision::instance::load_current_anchor_boxed(repository)
            .await
            .expect("Anchor load failed");
        let left_out = table_left_out(change, in_view, action, unstage, forced);
        let mut faults: Vec<String> = revision_tree_faults(repository, current)
            .await
            .into_iter()
            .filter(|fault| {
                !(matches!(left_out, TableLeftOut::Tree(_))
                    && matches!(fault, TreeFault::NotRehashed(_)))
            })
            .map(|fault| format!("{fault:?}"))
            .collect();
        if let Some(source) = case.source.as_ref() {
            let held = State::deserialize(repository.clone(), current)
                .await
                .expect("Deserialize failed")
                .find_node_link(repository.clone(), source)
                .await
                .is_ok();
            let unstaged = unstage != TableUnstage::None && (forced || in_view);
            // A commit leaves out a move an unstage left unstaged: the file stays at its source.
            if unstaged {
                let content = if held {
                    Some(committed_content(repository, current, source).await)
                } else {
                    None
                };
                if content.as_deref() != case.current {
                    faults.push(format!("the commit holds {content:?} at {source}"));
                }
            } else if held {
                faults.push(format!("the commit holds {source}, which the merge moved"));
            }
        }
        (case, committed, faults)
    }

    /// What the merge table leaves out of a case, and why.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum TableLeftOut {
        /// Nothing: the case is checked whole.
        Nothing,
        /// The check that each directory is its children's rehash, for the reason given.
        Tree(&'static str),
        /// The whole case, for the reason given.
        Case(&'static str),
    }

    /// What the merge table leaves out of a case whose unstage reaches a change the merge staged,
    /// which an unstage does not take out of the merge: an unstage that reaches the path, a forced
    /// one or one in view, where the merge changed it, and a forced unstage of the tree, which
    /// reaches every change the merge staged.
    ///
    /// A forced unstage of the path or of its directory, past the view, of the resolution a forced
    /// stage made of the change, takes the current revision's file back, at the path or, for a
    /// move, at the path the move took the file from, so the table checks the content there. It
    /// leaves out the rehash of the tree, whose directories the merge staged and the unstage
    /// clears. An unstage of a new directory a merge added, or of the tree, reaches the merge's add
    /// of the directory, and of a file the merge moved into it.
    fn table_left_out(
        change: TableChange,
        in_view: bool,
        action: TableAction,
        unstage: TableUnstage,
        forced: bool,
    ) -> TableLeftOut {
        let reaches = match unstage {
            TableUnstage::None => false,
            TableUnstage::Root if forced => true,
            _ => (forced || in_view) && change != TableChange::None,
        };
        let directory = matches!(unstage, TableUnstage::Directory | TableUnstage::Root);
        let resolved = !in_view
            && forced
            && matches!(unstage, TableUnstage::Path | TableUnstage::Directory)
            && matches!(
                action,
                TableAction::StageOther | TableAction::StageEmpty | TableAction::StageNamed
            );
        if !reaches {
            TableLeftOut::Nothing
        } else if change == TableChange::MoveIntoNewDirectory && directory {
            TableLeftOut::Case("a file moved into a directory the merge added")
        } else if change == TableChange::AddDirectory && directory {
            TableLeftOut::Case("an unstage that reaches a directory the merge added")
        } else if resolved && change != TableChange::None {
            TableLeftOut::Tree("an unstage that reaches the directories a merge staged")
        } else {
            TableLeftOut::Case("an unstage that reaches a merge's change")
        }
    }

    /// What the merge commit of a case of the merge table holds at its path, as the rulings on
    /// merges past the view have it, or `None` for a case a gap filed apart leaves open.
    ///
    /// An unstage that reaches the path, a forced one or one in view, takes the current
    /// revision's file back, and a commit leaves out a move it leaves unstaged, so the path a move
    /// took the file to holds nothing. A forced stage takes the working tree's file, and so does a stage
    /// in view. Without either, a commit takes the merge's change, read from the working tree in
    /// view, where the user's edit of a merged file counts, and from the staged node past it.
    fn table_expected(
        case: &TableMerge,
        change: TableChange,
        in_view: bool,
        action: TableAction,
        unstage: TableUnstage,
        forced: bool,
    ) -> Option<MergeCommitted> {
        let unstaged = unstage != TableUnstage::None && (forced || in_view);
        let deletes = matches!(change, TableChange::Delete | TableChange::DeleteDirectory);
        // A dirty mark over a staged delete replaces it with a modify.
        if !unstaged && deletes && action == TableAction::DirtyMark {
            return None;
        }
        if unstaged {
            return Some(MergeCommitted::unchanged(
                case.current.filter(|_| case.source.is_none()),
            ));
        }
        let merged = change != TableChange::None;
        let staged = match action {
            TableAction::StageOther | TableAction::StageEmpty | TableAction::StageNamed => true,
            TableAction::StagePlain => in_view,
            _ => false,
        };
        let file = match action {
            TableAction::DirtyMark | TableAction::StagePlain if !merged && !staged => case.current,
            TableAction::DirtyMark if in_view => Some(TABLE_EDIT),
            TableAction::Nothing | TableAction::StageSame | TableAction::DirtyMark => case.merged,
            TableAction::StagePlain if !in_view => case.merged,
            TableAction::StageOther | TableAction::StageNamed | TableAction::StagePlain => {
                Some(TABLE_EDIT)
            }
            TableAction::StageEmpty => Some(&b""[..]),
        };
        let action_letter = match change {
            TableChange::Add | TableChange::AddDirectory => "A",
            TableChange::Move | TableChange::MoveIntoNewDirectory => "V",
            _ if file.is_none() => "D",
            _ => "M",
        };
        let changed = if !merged {
            staged.then_some(("M", false))
        } else {
            Some((action_letter, true))
        };
        let mut expected = MergeCommitted::unchanged(file);
        expected.change = changed;
        Some(expected)
    }

    /// A plain unstage of the tree leaves a merge's change past the view staged, and the
    /// directories above it with it: the merge commit holds the merge's change, and a tree whose
    /// directories are their children's rehash.
    #[tokio::test]
    async fn a_plain_unstage_of_the_tree_leaves_a_merge_past_the_view_staged() {
        in_execution(async {
            let (_, committed, faults) = table_case(
                TableChange::Modify,
                false,
                TableAction::Nothing,
                TableUnstage::Root,
                false,
            )
            .await;
            assert_eq!(
                (committed, faults),
                (MergeCommitted::merged(b"merged", "M"), Vec::new())
            );
        })
        .await;
    }

    /// A forced stage of a file a merge moved past the view keeps the move: the merge commit
    /// records a move of the working tree's file.
    #[tokio::test]
    async fn a_forced_stage_of_a_merged_move_past_the_view_keeps_the_move() {
        in_execution(async {
            let (_, committed, faults) = table_case(
                TableChange::Move,
                false,
                TableAction::StageOther,
                TableUnstage::None,
                false,
            )
            .await;
            assert_eq!(
                (committed, faults),
                (MergeCommitted::merged(TABLE_EDIT, "V"), Vec::new())
            );
        })
        .await;
    }

    /// A forced stage of a file a stage moved keeps the move, with the edit made since: the
    /// commit records a move to the new path, of the working tree's content.
    #[tokio::test]
    async fn a_forced_stage_of_a_staged_move_keeps_the_move() {
        in_execution(async {
            let (repository, tempdir) = committed_repository("", &["a.txt".to_string()]).await;
            let root = tempdir.to_path_buf();
            TreeEdit::Move("a.txt".to_string(), "b.txt".to_string())
                .apply(&repository, &root)
                .await;
            write_working_file(&root, "b.txt", TABLE_EDIT);
            stage_paths(&repository, &root, &["b.txt"], false, true).await;
            let mut expected = MergeCommitted::unchanged(Some(TABLE_EDIT));
            expected.two_parents = false;
            expected.change = Some(("V", false));
            assert_eq!(commit_merge(&repository, "b.txt").await, expected);
        })
        .await;
    }

    /// The staged comparison status runs keeps a node past the view only where something is
    /// staged at or below it, below a directory a stage added too: a committed file a dirty move
    /// took into such a directory past the view, beside a file a forced stage added, is not listed.
    #[tokio::test]
    async fn status_leaves_out_a_file_dirty_moved_below_a_directory_added_past_the_view() {
        in_execution(async {
            let (repository, tempdir) = committed_repository("", &["moved.txt".to_string()]).await;
            let root = tempdir.to_path_buf();
            test_file_write(
                root.join(lore_revision::repository::DOT_LORE)
                    .join(lore_revision::repository::VIEW_FILTER)
                    .as_path(),
                b"/New/Hidden/\n",
            );
            let filter = lore_revision::repository::load_filter(&root).expect("Load filter failed");
            let sparse = Arc::new(repository.to_filter_context(filter));
            write_working_file(&root, "New/Hidden/b.txt", b"hidden");
            stage_paths(&sparse, &root, &["New/Hidden/b.txt"], false, true).await;
            // Test fixture writes; not subject to repository write-token discipline.
            #[allow(clippy::disallowed_methods)]
            std::fs::rename(root.join("moved.txt"), root.join("New/Hidden/moved.txt"))
                .expect("Rename failed");
            file::dirty::dirty_move(
                sparse.clone(),
                root.join("moved.txt").to_string_lossy().into_owned(),
                root.join("New/Hidden/moved.txt")
                    .to_string_lossy()
                    .into_owned(),
            )
            .await
            .expect("Dirty move failed");
            let listed: Vec<String> = status_reported(&sparse, None, false)
                .await
                .into_iter()
                .map(|(entry, _)| entry)
                .filter(|entry| entry.contains("New/Hidden/moved.txt"))
                .collect();
            assert_eq!(listed, Vec::<String>::new());
        })
        .await;
    }

    /// The merge table: twelve changes a merge makes at a path, in view and past it, under seven
    /// actions a user can take there before the merge commit, each without an unstage and with a
    /// plain and a forced unstage of the path, of its directory and of the tree. The merge commit
    /// succeeds. Short of what [`table_left_out`] leaves out, it holds at the path what
    /// [`table_expected`] says, where that says anything, and records the change's action there,
    /// and its tree holds no node that keeps a staged or dirty mark, no file with a zero hash and
    /// a nonzero size, no directory whose address is not its children's rehash, and no file at the
    /// path a move took it from, where no unstage reached the move.
    #[tokio::test]
    async fn a_merge_commit_holds_what_the_user_made_of_the_merge() {
        let changes = [
            TableChange::None,
            TableChange::Modify,
            TableChange::Add,
            TableChange::Delete,
            TableChange::Move,
            TableChange::AddAddTheirs,
            TableChange::Empty,
            TableChange::DeleteReadd,
            TableChange::AddDirectory,
            TableChange::MoveIntoNewDirectory,
            TableChange::DeleteDirectory,
            TableChange::ModifyConflict,
        ];
        let actions = [
            TableAction::Nothing,
            TableAction::DirtyMark,
            TableAction::StageOther,
            TableAction::StageSame,
            TableAction::StageEmpty,
            TableAction::StageNamed,
            TableAction::StagePlain,
        ];
        let unstages = [
            (TableUnstage::None, false),
            (TableUnstage::Path, false),
            (TableUnstage::Path, true),
            (TableUnstage::Directory, false),
            (TableUnstage::Directory, true),
            (TableUnstage::Root, false),
            (TableUnstage::Root, true),
        ];
        let running = Arc::new(tokio::sync::Semaphore::new(32));
        let mut tasks = Vec::new();
        for change in changes {
            for in_view in [false, true] {
                for action in actions {
                    for (unstage, forced) in unstages {
                        let left_out = table_left_out(change, in_view, action, unstage, forced);
                        if matches!(left_out, TableLeftOut::Case(_)) {
                            continue;
                        }
                        let running = running.clone();
                        #[allow(clippy::disallowed_methods)]
                        let task = runtime().spawn(LORE_CONTEXT.scope(
                            setup_test_execution(),
                            async move {
                                let _permit = running.acquire().await.expect("A permit");
                                let (case, committed, mut faults) =
                                    table_case(change, in_view, action, unstage, forced).await;
                                if let Some(expected) =
                                    table_expected(&case, change, in_view, action, unstage, forced)
                                    && committed != expected
                                {
                                    faults.push(format!("holds {committed:?}, not {expected:?}"));
                                }
                                faults
                            },
                        ));
                        tasks.push(((change, in_view, action, unstage, forced), task));
                    }
                }
            }
        }
        let mut failures = Vec::new();
        for (case, task) in tasks {
            match task.await {
                Ok(faults) => failures.extend(faults.into_iter().map(|f| format!("{case:?}: {f}"))),
                Err(err) => failures.push(format!("{case:?}: {err}")),
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// Rebasing the staged state, as a sync or a branch switch does, carries a dirty mark on a
    /// committed file below a directory an ignore rule names, and leaves out an add marked dirty
    /// beside it under force: the tree tracks the file, so the rule has no say over it, and the add
    /// is untracked. A rebase that dropped the mark would leave the edit unmarked and out of a
    /// status that does not scan.
    #[tokio::test]
    async fn rebase_staged_state_carries_a_dirty_tracked_file_an_ignore_rule_names() {
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let (ruled, _, tempdir) = ignoring_repository(&["bin/edit.txt"], "bin/\n").await;
                let root = tempdir.to_path_buf();
                test_file_write(root.join("bin/edit.txt").as_path(), b"edited");
                file::dirty::dirty(ruled.clone(), path_arguments(&root, &["bin/edit.txt"]))
                    .await
                    .expect("Dirty failed");
                test_file_write(root.join("bin/new.txt").as_path(), b"new");
                LORE_CONTEXT
                    .scope(
                        forced_execution(),
                        file::dirty::dirty(ruled.clone(), path_arguments(&root, &["bin/new.txt"])),
                    )
                    .await
                    .expect("Forced dirty failed");

                let (current, _) = lore_revision::instance::load_current_anchor_boxed(&ruled)
                    .await
                    .expect("Load current anchor failed");
                let staged = lore_revision::instance::load_staged_revision(&ruled)
                    .await
                    .expect("Load staged anchor failed")
                    .expect("Dirty should have anchored a staged state");

                let rebased = lore_revision::state::rebase_staged_state(
                    ruled.clone(),
                    staged,
                    current,
                    false,
                )
                .await
                .expect("Rebase failed")
                .expect("The dirty tracked file should produce a rebased state");
                let rebased = State::deserialize(ruled.clone(), rebased)
                    .await
                    .expect("Deserialize rebased state failed");
                let edit = rebased
                    .find_node(ruled.clone(), "bin/edit.txt")
                    .await
                    .expect("The rebased state holds bin/edit.txt");
                assert!(
                    edit.is_dirty_modify(),
                    "bin/edit.txt should be dirty in the rebased state"
                );
                let new = rebased.find_node(ruled.clone(), "bin/new.txt").await.ok();
                assert!(
                    new.is_none_or(|node| !node.is_dirty_add()),
                    "the untracked bin/new.txt was carried past the ignore rule: {new:?}"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A dirty copy into a directory the ignore file names marks a new node, which the tree does
    /// not track, so an ordinary status leaves it out and an ordinary stage stages nothing of it,
    /// where a copy into a directory no rule names is reported and staged. A dirty move there keeps
    /// the committed node it moves, which the tree tracks wherever it moves, so it is reported as a
    /// move into a directory no rule names is.
    #[tokio::test]
    async fn a_dirty_copy_into_an_ignored_directory_stays_ignored_and_a_dirty_move_does_not() {
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let (ruled, _, tempdir) = ignoring_repository(
                    &[
                        "bin/keep.txt",
                        "copied.txt",
                        "moved.txt",
                        "plain/keep.txt",
                        "other.txt",
                    ],
                    "bin/\n",
                )
                .await;
                let root = tempdir.to_path_buf();
                std::fs::copy(root.join("copied.txt"), root.join("bin/copy.txt"))
                    .expect("Copy failed");
                std::fs::rename(root.join("moved.txt"), root.join("bin/moved.txt"))
                    .expect("Move failed");
                let at = |path: &str| root.join(path).to_string_lossy().into_owned();
                std::fs::copy(root.join("copied.txt"), root.join("plain/copy.txt"))
                    .expect("Copy failed");
                for to in ["bin/copy.txt", "plain/copy.txt"] {
                    file::dirty::dirty_copy(ruled.clone(), at("copied.txt"), at(to))
                        .await
                        .expect("Dirty copy failed");
                }
                file::dirty::dirty_move(ruled.clone(), at("moved.txt"), at("bin/moved.txt"))
                    .await
                    .expect("Dirty move failed");

                std::fs::rename(root.join("other.txt"), root.join("plain/other.txt"))
                    .expect("Move failed");
                file::dirty::dirty_move(ruled.clone(), at("other.txt"), at("plain/other.txt"))
                    .await
                    .expect("Dirty move failed");
                let moves = [
                    ("V moved.txt -> bin/moved.txt".to_string(), false),
                    ("V other.txt -> plain/other.txt".to_string(), false),
                ];
                let copy = |staged: bool| ("A plain/copy.txt".to_string(), staged);
                let reported = |staged: bool| {
                    std::iter::once(copy(staged))
                        .chain(moves.iter().cloned())
                        .collect::<Vec<_>>()
                };
                assert_eq!(
                    status_reported(&ruled, None, false).await,
                    reported(false),
                    "status reported the copy into the ignored directory, or missed a move"
                );
                stage_paths(&ruled, &root, &["."], false, false).await;
                assert_eq!(
                    status_reported(&ruled, None, false).await,
                    reported(true),
                    "stage staged the copy into the ignored directory, or treated the moves apart"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A forced dirty mark of a new file below a new directory the ignore file names leaves both
    /// untracked: the directory the mark creates is a dirty add, as the file is, so an ordinary
    /// status reports neither and an ordinary stage stages neither. Beside them, a new directory no
    /// rule names is staged with the file marked in it, and not with the unmarked file the working
    /// tree holds there too.
    #[tokio::test]
    async fn a_forced_dirty_add_below_a_new_ignored_directory_stays_ignored() {
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let (ruled, _, tempdir) = ignoring_repository(&[], "build/\n").await;
                let root = tempdir.to_path_buf();
                for path in ["build/new.log", "plain/new.txt", "plain/other.txt"] {
                    std::fs::create_dir_all(root.join(path).parent().expect("A parent"))
                        .expect("Create directory failed");
                    test_file_write(root.join(path).as_path(), path.as_bytes());
                }
                LORE_CONTEXT
                    .scope(
                        forced_execution(),
                        file::dirty::dirty(
                            ruled.clone(),
                            path_arguments(&root, &["build/new.log", "plain/new.txt"]),
                        ),
                    )
                    .await
                    .expect("Forced dirty failed");

                assert_eq!(
                    status_reported(&ruled, None, false).await,
                    [
                        ("A plain/".to_string(), false),
                        ("A plain/new.txt".to_string(), false)
                    ],
                    "status reported the ignored directory or missed the marked file"
                );

                stage_paths(&ruled, &root, &["."], false, false).await;
                assert_eq!(
                    status_reported(&ruled, None, false).await,
                    [
                        ("A plain/".to_string(), true),
                        ("A plain/new.txt".to_string(), true)
                    ],
                    "stage staged the ignored directory or the unmarked file"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A commit of another file carries a dirty mark on a committed file below a directory an
    /// ignore rule names onto the staged state it leaves, as it carries any mark it does not
    /// commit.
    #[tokio::test]
    async fn a_commit_carries_a_dirty_tracked_file_an_ignore_rule_names() {
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let (ruled, _, tempdir) =
                    ignoring_repository(&["bin/edit.txt", "keep.txt"], "bin/\n").await;
                let root = tempdir.to_path_buf();
                test_file_write(root.join("bin/edit.txt").as_path(), b"edited");
                file::dirty::dirty(ruled.clone(), path_arguments(&root, &["bin/edit.txt"]))
                    .await
                    .expect("Dirty failed");
                test_file_write(root.join("keep.txt").as_path(), b"changed");
                stage_paths(&ruled, &root, &["keep.txt"], true, false).await;
                commit::commit_boxed(
                    ruled.clone(),
                    ruled
                        .try_write_token()
                        .expect("The repository was opened for writing"),
                    CommitOptions::new("Commit keep.txt".to_string()),
                )
                .await
                .expect("Commit failed");

                let staged = lore_revision::instance::load_staged_revision(&ruled)
                    .await
                    .expect("Load staged anchor failed")
                    .expect("The commit should leave the dirty mark staged-anchored");
                let staged = State::deserialize(ruled.clone(), staged)
                    .await
                    .expect("Deserialize staged state failed");
                let edit = staged
                    .find_node(ruled.clone(), "bin/edit.txt")
                    .await
                    .expect("The staged state holds bin/edit.txt");
                assert!(
                    edit.is_dirty_modify(),
                    "bin/edit.txt should stay dirty after the commit"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Status lists a forced add of a file an ignore rule matches, and staged moves into and out
    /// of a directory one names.
    #[tokio::test]
    async fn status_lists_a_forced_add_and_moves_into_and_out_of_an_ignored_directory() {
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let (ruled, _, tempdir) =
                    ignoring_repository(&["bin/out.txt", "src/into.txt"], "bin/\n").await;
                let root = tempdir.to_path_buf();
                test_file_write(root.join("bin/new.txt").as_path(), b"new");
                stage_paths(&ruled, &root, &["bin/new.txt"], true, true).await;
                for (from, to) in [
                    ("src/into.txt", "bin/into.txt"),
                    ("bin/out.txt", "src/out.txt"),
                ] {
                    std::fs::rename(root.join(from), root.join(to)).expect("Rename failed");
                    LORE_CONTEXT
                        .scope(
                            forced_execution(),
                            file::stage::stage_move(
                                ruled.clone(),
                                ruled
                                    .try_write_token()
                                    .expect("The repository was opened for writing"),
                                root.join(from).to_string_lossy().into_owned(),
                                root.join(to).to_string_lossy().into_owned(),
                                StageOptions::default(),
                            ),
                        )
                        .await
                        .expect("Stage move failed");
                }

                assert_eq!(
                    status_reported(&ruled, None, false).await,
                    vec![
                        ("A bin/new.txt".to_string(), true),
                        ("V bin/out.txt -> src/out.txt".to_string(), true),
                        ("V src/into.txt -> bin/into.txt".to_string(), true),
                    ]
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Status lists a forced delete of a directory an ignore rule names as it lists the delete
    /// without the rule, and the walk does not fan out below it.
    #[tokio::test]
    async fn status_lists_a_forced_delete_of_an_ignored_directory() {
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let (ruled, unruled, tempdir) =
                    ignoring_repository(&["gen/a.txt", "gen/b.txt"], "gen/\n").await;
                let root = tempdir.to_path_buf();
                std::fs::remove_dir_all(root.join("gen")).expect("Remove failed");
                stage_paths(&ruled, &root, &["gen"], true, true).await;

                let reported = status_reported(&ruled, None, false).await;
                assert_eq!(reported, status_reported(&unruled, None, false).await);
                assert_eq!(reported, vec![("D gen/".to_string(), true)]);
                assert_eq!(
                    staged_walk(&ruled, lore_revision::filter::FilterMode::TRACKED)
                        .await
                        .0,
                    vec![("D".to_string(), "gen".to_string())]
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A merge stages a conflict on a path an ignore rule matches, and status lists it
    /// unresolved.
    #[tokio::test]
    async fn status_lists_a_merge_conflict_on_an_ignored_path() {
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let (ruled, _, tempdir) =
                    ignoring_repository(&["bin/conflict.txt"], "bin/\n").await;
                let root = tempdir.to_path_buf();
                let token = ruled
                    .try_write_token()
                    .expect("The repository was opened for writing");
                let commit_edit = async |content: &[u8]| {
                    test_file_write(root.join("bin/conflict.txt").as_path(), content);
                    stage_paths(&ruled, &root, &["bin/conflict.txt"], true, true).await;
                    commit::commit_boxed(ruled.clone(), token, CommitOptions::new(String::new()))
                        .await
                        .expect("Commit failed");
                };

                let (base, main) = lore_revision::instance::load_current_anchor_boxed(&ruled)
                    .await
                    .expect("Anchor load failed");
                lore_revision::branch::create::create(
                    ruled.clone(),
                    token,
                    "feature".to_string(),
                    None,
                    String::new(),
                    false,
                )
                .await
                .expect("Branch create failed");
                let (_, feature) = lore_revision::instance::load_current_anchor_boxed(&ruled)
                    .await
                    .expect("Anchor load failed");
                commit_edit(b"feature").await;
                lore_revision::instance::store_current_anchor_branch(&ruled, main)
                    .await
                    .expect("Anchor store failed");
                lore_revision::instance::store_current_anchor(&ruled, base)
                    .await
                    .expect("Anchor store failed");
                commit_edit(b"main").await;

                let offline = Arc::new(
                    lore_revision::interface::ExecutionContext::new_client_with_user_id(
                        lore_revision::interface::LoreGlobalArgs::default().set_offline(),
                        lore_revision::relay::EventDispatcher::no_dispatch(),
                        "test-user".to_string(),
                    ),
                );
                let merged = LORE_CONTEXT
                    .scope(
                        offline,
                        Box::pin(lore_revision::branch::merge::merge_start(
                            ruled.clone(),
                            token,
                            feature,
                            lore_revision::branch::merge::MergeStartOptions {
                                message: String::new(),
                                link_messages: std::collections::HashMap::new(),
                                no_commit: false,
                                scope: lore_revision::branch::merge::MergeScope::MainOnly,
                                inherit_metadata: Default::default(),
                            },
                        )),
                    )
                    .await;

                merged.expect("Merge failed");
                let (_, staged, _) = State::deserialize_current_and_staged(ruled.clone())
                    .await
                    .expect("Deserialize failed");
                assert!(staged.expect("The merge staged its result").is_conflict());
                assert_eq!(
                    status_reported(&ruled, None, false).await,
                    vec![("M bin/conflict.txt!".to_string(), true)]
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The comparison status runs against the staged state enters a directory an ignore rule
    /// names only along what is staged or marked dirty below it: those are changes to tracked
    /// files, which it lists as it would without the rule, and a subtree with nothing changed in
    /// it is not entered however much it holds.
    #[tokio::test]
    async fn the_staged_comparison_enters_an_ignored_directory_only_along_its_changes() {
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let (ruled, _, tempdir) = ignoring_repository(
                    &["gen/a/f.txt", "gen/b/f.txt", "gen/c/f.txt", "keep.txt"],
                    "gen/
",
                )
                .await;
                let root = tempdir.to_path_buf();
                test_file_write(root.join("keep.txt").as_path(), b"edited");
                stage_paths(&ruled, &root, &["keep.txt"], true, false).await;
                std::fs::remove_dir_all(root.join("gen/a")).expect("Remove failed");
                test_file_write(root.join("gen/b/f.txt").as_path(), b"edited");
                file::dirty::dirty(
                    ruled.clone(),
                    path_arguments(&root, &["gen/a", "gen/b/f.txt"]),
                )
                .await
                .expect("Dirty failed");

                // A deleted directory is listed whole: its files are judged by their own flags in
                // the current tree, which carries none, so the rule leaves them out of the list.
                let (found, entered) =
                    staged_walk(&ruled, lore_revision::filter::FilterMode::TRACKED).await;
                assert_eq!(
                    found,
                    [("D", "gen/a"), ("M", "gen/b/f.txt"), ("M", "keep.txt")]
                        .map(|(action, path)| (action.to_string(), path.to_string()))
                        .to_vec()
                );
                // The root, gen and gen/b: gen/a is listed as deleted without being entered, and
                // gen/c holds nothing changed.
                assert_eq!(entered, 3);
            }))
            .await
            .expect("Test task failed");
    }

    /// Status lists a forced change of a file to a directory below a directory an ignore rule
    /// names as it lists the change without the rule.
    #[tokio::test]
    async fn status_lists_a_forced_type_change_below_an_ignored_directory() {
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let (ruled, unruled, tempdir) = ignoring_repository(&["bin/thing"], "bin/\n").await;
                let root = tempdir.to_path_buf();
                std::fs::remove_file(root.join("bin/thing")).expect("Remove failed");
                std::fs::create_dir_all(root.join("bin/thing")).expect("Create directory failed");
                test_file_write(root.join("bin/thing/inner.txt").as_path(), b"inner");
                stage_paths(&ruled, &root, &["bin/thing"], true, true).await;

                let reported = status_reported(&ruled, None, false).await;
                assert_eq!(reported, status_reported(&unruled, None, false).await);
                assert!(!reported.is_empty());
            }))
            .await
            .expect("Test task failed");
    }

    /// Unstaging a path takes back a staged change to it that an ignore rule matches, which
    /// status lists. The edit stays in the working tree, so the file is still listed, unstaged.
    #[tokio::test]
    async fn unstage_takes_back_a_staged_change_below_an_ignored_directory() {
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
                let (ruled, _, tempdir) = ignoring_repository(&["bin/sub/a.txt"], "bin/\n").await;
                let root = tempdir.to_path_buf();
                test_file_write(root.join("bin/sub/a.txt").as_path(), b"edited");
                stage_paths(&ruled, &root, &["bin/sub/a.txt"], true, true).await;
                assert_eq!(
                    status_reported(&ruled, None, false).await,
                    vec![("M bin/sub/a.txt".to_string(), true)]
                );

                file::unstage::unstage(
                    ruled.clone(),
                    ruled
                        .try_write_token()
                        .expect("The repository was opened for writing"),
                    path_arguments(&root, &["bin/sub/a.txt"]),
                    file::unstage::UnstageOptions::default(),
                )
                .await
                .expect("Unstage failed");
                assert_eq!(
                    status_reported(&ruled, None, false).await,
                    vec![("M bin/sub/a.txt".to_string(), false)]
                );
            }))
            .await
            .expect("Test task failed");
    }
}
