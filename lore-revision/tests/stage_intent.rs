// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_revision::change::FileAction;
    use lore_revision::fs::filesystem_provider::FilesystemDiffIntent;
    use lore_revision::fs::filesystem_provider::StageIntent;
    use lore_revision::lore::RepositoryId;
    use lore_revision::node::NodeFlags;

    include!("helper.rs");

    /// A staging walk records what a staged add carries: the action, the size and mode the
    /// file was measured with, and an identity so metadata can be attached to it before a
    /// commit assigns one. The fixture carries the executable bit, which is the only mode
    /// a node records and the only one a default node does not already have.
    #[tokio::test]
    async fn a_staging_walk_records_an_add_with_its_identity() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                let contents = b"#!/bin/sh\necho staged";
                let script = fixture.path.join("script.sh");
                test_file_write(&script, contents);
                #[cfg(target_family = "unix")]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                        .expect("Failed to set the executable bit");
                }

                let (current, staged) = test_anchor_states(&repository).await;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let added = changes
                    .iter()
                    .find(|change| change.path.as_str() == "script.sh")
                    .expect("the walk must report the new file");
                assert_eq!(FileAction::Add, added.action);

                let node = staged
                    .node(repository.clone(), added.to.node)
                    .await
                    .expect("the staged node must read back");
                let flags = NodeFlags::from_bits_retain(node.flags);
                assert!(
                    flags.contains(NodeFlags::StagedAdd),
                    "a staged add must record the action, flags {:x}",
                    node.flags
                );
                assert!(
                    flags.contains(NodeFlags::DirtyAdd),
                    "a staged add is dirty too, flags {:x}",
                    node.flags
                );
                assert_eq!(
                    contents.len() as u64,
                    node.size,
                    "the node must record the size the file was measured with"
                );
                assert!(
                    !node.address.context.is_zero(),
                    "a staged file node must carry an identity"
                );
                #[cfg(target_family = "unix")]
                assert_eq!(
                    lore_revision::node::NodeFileMode::Executable.bits(),
                    node.mode & lore_revision::node::NodeFileMode::Executable.bits(),
                    "the node must record the mode the file was measured with"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A marking walk records the action without the staged one, which is what separates
    /// `status --scan` from staging.
    #[tokio::test]
    async fn a_marking_walk_records_no_staged_action() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                test_file_write(&fixture.path.join("script.sh"), b"marked only");

                let (current, staged) = test_anchor_states(&repository).await;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::MarkDirty,
                )
                .await;

                let added = changes
                    .iter()
                    .find(|change| change.path.as_str() == "script.sh")
                    .expect("the walk must report the new file");
                let node = staged
                    .node(repository.clone(), added.to.node)
                    .await
                    .expect("the marked node must read back");
                let flags = NodeFlags::from_bits_retain(node.flags);
                assert!(
                    flags.contains(NodeFlags::DirtyAdd),
                    "a marked add records the dirty action, flags {:x}",
                    node.flags
                );
                assert!(
                    !flags.contains(NodeFlags::StagedAdd),
                    "a marked add records no staged action, flags {:x}",
                    node.flags
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A staged delete settles the whole tree subtree, so a commit built from it removes
    /// what the view leaves out too. `MarkDirty` answers for the view alone and leaves an
    /// excluded node untouched.
    async fn deleted_directory_leaves_excluded_child(
        intent: FilesystemDiffIntent,
    ) -> (lore_revision::node::Node, usize) {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                std::fs::create_dir_all(path.join("dir")).expect("Create directory failed");
                let write_token =
                    lore_revision::repository::RepositoryWriteToken::acquire(path.as_path()).await;
                let default_branch_id = lore_base::types::Context::from(uuid::Uuid::now_v7());
                let created = lore_revision::repository::create_local(
                    path.as_path(),
                    &write_token,
                    repository_id,
                    default_branch_id,
                    lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
                    lore_revision::repository::RepositoryConfig::default(),
                    false,
                )
                .await
                .expect("Failed to initialize repository");

                let mut filter = lore_revision::filter::Filter::default();
                filter
                    .ignore
                    .add_exclusion("dir/hidden.txt")
                    .expect("exclusion rule");
                let repository = std::sync::Arc::new(
                    lore_revision::repository::RepositoryContext::new(
                        default_repository_creation_args(immutable_store, mutable_store)
                            .with_path(&path)
                            .with_id(repository_id)
                            .with_instance_id(created.instance_id)
                            .with_filter(std::sync::Arc::new(filter)),
                    )
                    .with_write_token(write_token.share()),
                );
                lore_revision::instance::store_current_anchor_branch(
                    &repository,
                    default_branch_id,
                )
                .await
                .expect("Failed to store anchor branch");

                test_file_write(&path.join("dir/shown.txt"), b"in view");
                test_file_write(&path.join("dir/hidden.txt"), b"out of view");

                // Stage and commit both files, so the tree holds the excluded one too.
                let force =
                    std::sync::Arc::new(lore_revision::interface::ExecutionContext::new_client(
                        lore_revision::interface::LoreGlobalArgs {
                            force: 1,
                            ..Default::default()
                        },
                        lore_revision::relay::EventDispatcher::no_dispatch(),
                    ));
                LORE_CONTEXT
                    .scope(
                        force,
                        lore_revision::file::stage::stage(
                            repository.clone(),
                            &write_token,
                            lore_revision::interface::LoreArray::from_vec(vec![
                                lore_revision::interface::LoreString::from(&path),
                            ]),
                            lore_revision::stage::StageOptions {
                                scan: true,
                                ..Default::default()
                            },
                        ),
                    )
                    .await
                    .expect("Failed to stage the fixture");
                Box::pin(lore_revision::commit::commit(
                    repository.clone(),
                    &write_token,
                    lore_revision::commit::CommitOptions {
                        message: String::new(),
                        link_messages: std::collections::HashMap::new(),
                        link: None,
                        layer_messages: std::collections::HashMap::new(),
                        layer: None,
                    },
                ))
                .await
                .expect("Failed to commit the fixture");

                std::fs::remove_dir_all(path.join("dir")).expect("Remove directory failed");

                let (current, staged) = test_anchor_states(&repository).await;
                let hidden = staged
                    .find_node_link(repository.clone(), "dir/hidden.txt")
                    .await
                    .expect("the tree must hold the excluded file");
                let changes =
                    test_scan_with_intent(repository.clone(), staged.clone(), current, intent)
                        .await;
                let node = staged
                    .node(repository.clone(), hidden.node)
                    .await
                    .expect("the excluded node must read back");
                (node, changes.len())
            }))
            .await
            .expect("Test task failed")
    }

    #[tokio::test]
    async fn a_staged_delete_settles_an_excluded_child() {
        let (node, changes) = deleted_directory_leaves_excluded_child(FilesystemDiffIntent::Stage(
            StageIntent::default(),
        ))
        .await;
        let flags = NodeFlags::from_bits_retain(node.flags);
        assert!(
            flags.contains(NodeFlags::StagedDelete),
            "a staged delete must settle the excluded child, flags {:x}",
            node.flags
        );
        assert!(changes > 0, "the walk must report the deletion it settled");
    }

    #[tokio::test]
    async fn a_marking_delete_leaves_an_excluded_child_alone() {
        let (node, _) =
            deleted_directory_leaves_excluded_child(FilesystemDiffIntent::MarkDirty).await;
        let flags = NodeFlags::from_bits_retain(node.flags);
        assert!(
            !flags.contains(NodeFlags::DirtyDelete),
            "a marking walk answers for the view alone, flags {:x}",
            node.flags
        );
    }

    /// A node the walk already marked keeps its dirty action, and a staging walk over it
    /// still records the staged one: skipping the mark because the dirty action is there
    /// would leave the file reported as an add and never staged.
    #[tokio::test]
    async fn staging_a_file_a_scan_already_marked_records_the_staged_action() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                test_file_write(&fixture.path.join("script.sh"), b"scanned first");

                let (current, staged) = test_anchor_states(&repository).await;
                test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current.clone(),
                    FilesystemDiffIntent::MarkDirty,
                )
                .await;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let added = changes
                    .iter()
                    .find(|change| change.path.as_str() == "script.sh")
                    .expect("the walk must report the file the scan marked");
                let node = staged
                    .node(repository.clone(), added.to.node)
                    .await
                    .expect("the staged node must read back");
                let flags = NodeFlags::from_bits_retain(node.flags);
                assert!(
                    flags.contains(NodeFlags::StagedAdd),
                    "a scan's mark must not suppress the staged action, flags {:x}",
                    node.flags
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A rename the walk reports has to leave a staged move behind it: the walk matches a
    /// node to a filesystem entry by folded name, so a spelling that differs is reported
    /// as a move whether or not the content changed, and a staging walk that reports one
    /// without settling it describes a state the tree does not hold.
    #[tokio::test]
    async fn a_staged_rename_of_unchanged_content_settles_the_move() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;

                let contents = b"unchanged content";
                test_file_write(&fixture.path.join("Script.sh"), contents);
                lore_revision::file::stage::stage(
                    repository.clone(),
                    write_token,
                    lore_revision::interface::LoreArray::from_vec(vec![
                        lore_revision::interface::LoreString::from(&fixture.path),
                    ]),
                    lore_revision::stage::StageOptions {
                        scan: true,
                        ..Default::default()
                    },
                )
                .await
                .expect("Failed to stage the fixture");
                Box::pin(lore_revision::commit::commit(
                    repository.clone(),
                    write_token,
                    lore_revision::commit::CommitOptions {
                        message: String::new(),
                        link_messages: std::collections::HashMap::new(),
                        link: None,
                        layer_messages: std::collections::HashMap::new(),
                        layer: None,
                    },
                ))
                .await
                .expect("Failed to commit the fixture");

                std::fs::rename(
                    fixture.path.join("Script.sh"),
                    fixture.path.join("script.sh"),
                )
                .expect("Failed to rename the fixture");

                let (current, staged) = test_anchor_states(&repository).await;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let moved = changes
                    .iter()
                    .find(|change| change.action == FileAction::Move)
                    .expect("the walk must report the rename");
                let node = staged
                    .node(repository.clone(), moved.from.node)
                    .await
                    .expect("the renamed node must read back");
                let flags = NodeFlags::from_bits_retain(node.flags);
                assert!(
                    flags.contains(NodeFlags::StagedMove),
                    "a reported move must be settled as one, flags {:x}",
                    node.flags
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A path-scoped staging walk creates the ancestors its target needs, and those have
    /// to be staged along with it: `commit` discards a dirty-only add directory and the
    /// subtree under it, which would take the staged file with it.
    #[tokio::test]
    async fn a_staged_nested_path_settles_the_ancestors_it_creates() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                std::fs::create_dir_all(fixture.path.join("new-dir"))
                    .expect("Create directory failed");
                test_file_write(&fixture.path.join("new-dir/file.txt"), b"nested");

                let (current, staged) = test_anchor_states(&repository).await;
                test_scan_path_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    Some(
                        lore_revision::util::path::RelativePath::new_from_initial_path(
                            "new-dir/file.txt",
                        )
                        .expect("path"),
                    ),
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let directory = staged
                    .find_node_link(repository.clone(), "new-dir")
                    .await
                    .expect("the walk must create the ancestor");
                let node = staged
                    .node(repository.clone(), directory.node)
                    .await
                    .expect("the ancestor must read back");
                let flags = NodeFlags::from_bits_retain(node.flags);
                assert!(
                    flags.contains(NodeFlags::Staged),
                    "an ancestor a staging walk creates must be staged, flags {:x}",
                    node.flags
                );
            }))
            .await
            .expect("Test task failed");
    }
}
