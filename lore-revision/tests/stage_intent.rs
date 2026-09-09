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

    /// A repository whose ignore filter excludes `excluded`, so a walk has content the view
    /// leaves out to answer for.
    async fn test_repository_excluding(
        immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
        repository_id: RepositoryId,
        excluded: &str,
    ) -> TestRepository {
        let tempdir = generate_tempdir();
        let path = tempdir.to_path_buf();
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
            .add_exclusion(excluded)
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
        lore_revision::instance::store_current_anchor_branch(&repository, default_branch_id)
            .await
            .expect("Failed to store anchor branch");

        TestRepository {
            repository,
            write_token,
            path,
            _tempdir: tempdir,
        }
    }

    /// Stage and commit the whole fixture under force, which is what puts a path the filter
    /// excludes into the tree.
    async fn force_commit_fixture(fixture: &TestRepository) {
        LORE_CONTEXT
            .scope(
                forced_execution(),
                lore_revision::file::stage::stage(
                    fixture.repository.clone(),
                    &fixture.write_token,
                    lore_revision::interface::LoreArray::from_vec(vec![
                        lore_revision::interface::LoreString::from(&fixture.path),
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
            fixture.repository.clone(),
            &fixture.write_token,
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
                let fixture = test_repository_excluding(
                    immutable_store,
                    mutable_store,
                    repository_id,
                    "dir/hidden.txt",
                )
                .await;
                let repository = fixture.repository.clone();
                std::fs::create_dir_all(fixture.path.join("dir")).expect("Create directory failed");
                test_file_write(&fixture.path.join("dir/shown.txt"), b"in view");
                test_file_write(&fixture.path.join("dir/hidden.txt"), b"out of view");
                force_commit_fixture(&fixture).await;

                std::fs::remove_dir_all(fixture.path.join("dir")).expect("Remove directory failed");

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

    /// Stage and commit the whole fixture, so the tree holds a committed base for the walk
    /// to compare a replacement against.
    async fn commit_fixture(fixture: &TestRepository) {
        lore_revision::file::stage::stage(
            fixture.repository.clone(),
            &fixture.write_token,
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
            fixture.repository.clone(),
            &fixture.write_token,
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
    }

    /// The staged flags `node` carries.
    async fn staged_flags(
        repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
        state: &std::sync::Arc<lore_revision::state::State>,
        node: lore_revision::node::NodeID,
    ) -> NodeFlags {
        let node = state
            .node(repository.clone(), node)
            .await
            .expect("the node must read back");
        NodeFlags::from_bits_retain(node.flags)
    }

    /// A file the file system replaced with a directory stages as both halves of the
    /// replacement: the displaced node carries the delete a commit needs to drop the old
    /// content, and the directory that took its place is staged along with what it holds.
    #[tokio::test]
    async fn a_staged_file_replaced_by_a_directory_settles_both() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                test_file_write(&fixture.path.join("thing"), b"a file first");
                commit_fixture(&fixture).await;

                std::fs::remove_file(fixture.path.join("thing")).expect("Remove file failed");
                std::fs::create_dir_all(fixture.path.join("thing"))
                    .expect("Create directory failed");
                test_file_write(&fixture.path.join("thing/inner.txt"), b"content below");

                let (current, staged) = test_anchor_states(&repository).await;
                let displaced = staged
                    .find_node_link(repository.clone(), "thing")
                    .await
                    .expect("the tree must hold the committed file")
                    .node;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let flags = staged_flags(&repository, &staged, displaced).await;
                assert!(
                    flags.contains(NodeFlags::StagedDelete),
                    "the displaced file must be staged for delete, flags {flags:?}"
                );

                let inner = changes
                    .iter()
                    .find(|change| change.path.as_str() == "thing/inner.txt")
                    .expect("the walk must descend into the directory that replaced the file");
                assert_eq!(FileAction::Add, inner.action);
                let flags = staged_flags(&repository, &staged, inner.to.node).await;
                assert!(
                    flags.contains(NodeFlags::StagedAdd),
                    "content below the replacement must be staged, flags {flags:?}"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A staged replacement sits in the tree beside the node it displaced, both spelled the
    /// same. A second pass has to claim the replacement rather than the node carrying the
    /// delete, or it replaces the replacement.
    #[tokio::test]
    async fn staging_a_replacement_twice_replaces_it_once() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                test_file_write(&fixture.path.join("thing"), b"a file first");
                commit_fixture(&fixture).await;

                std::fs::remove_file(fixture.path.join("thing")).expect("Remove file failed");
                std::fs::create_dir_all(fixture.path.join("thing"))
                    .expect("Create directory failed");
                test_file_write(&fixture.path.join("thing/inner.txt"), b"content below");

                let (current, staged) = test_anchor_states(&repository).await;
                test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current.clone(),
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;
                let replacement = staged
                    .find_node_link(repository.clone(), "thing")
                    .await
                    .expect("the walk must mint the replacement")
                    .node;

                test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let after = staged
                    .find_node_link(repository.clone(), "thing")
                    .await
                    .expect("the replacement must still be there")
                    .node;
                assert_eq!(
                    replacement, after,
                    "the second pass must claim the replacement, not the node it displaced"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A directory the file system replaced with a file stages the whole displaced subtree
    /// for delete, and the file that took its place as an add carrying its identity.
    #[tokio::test]
    async fn a_staged_directory_replaced_by_a_file_settles_both() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                std::fs::create_dir_all(fixture.path.join("thing"))
                    .expect("Create directory failed");
                test_file_write(&fixture.path.join("thing/inner.txt"), b"content below");
                commit_fixture(&fixture).await;

                std::fs::remove_dir_all(fixture.path.join("thing"))
                    .expect("Remove directory failed");
                test_file_write(&fixture.path.join("thing"), b"a file now");

                let (current, staged) = test_anchor_states(&repository).await;
                let displaced = staged
                    .find_node_link(repository.clone(), "thing")
                    .await
                    .expect("the tree must hold the committed directory")
                    .node;
                let displaced_child = staged
                    .find_node_link(repository.clone(), "thing/inner.txt")
                    .await
                    .expect("the tree must hold the committed file below it")
                    .node;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let flags = staged_flags(&repository, &staged, displaced).await;
                assert!(
                    flags.contains(NodeFlags::StagedDelete),
                    "the displaced directory must be staged for delete, flags {flags:?}"
                );
                let flags = staged_flags(&repository, &staged, displaced_child).await;
                assert!(
                    flags.contains(NodeFlags::StagedDelete),
                    "the whole displaced subtree must be staged for delete, flags {flags:?}"
                );

                let added = changes
                    .iter()
                    .find(|change| {
                        change.path.as_str() == "thing" && change.action == FileAction::Add
                    })
                    .expect("the walk must report the file that replaced the directory");
                let node = staged
                    .node(repository.clone(), added.to.node)
                    .await
                    .expect("the replacement must read back");
                let flags = NodeFlags::from_bits_retain(node.flags);
                assert!(
                    flags.contains(NodeFlags::StagedAdd),
                    "the replacement must be staged as an add, flags {flags:?}"
                );
                assert!(
                    node.is_file(),
                    "the replacement must take the type the file system holds, flags {flags:?}"
                );
                assert!(
                    !node.address.context.is_zero(),
                    "a staged file node must carry an identity"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Force reports what the view leaves out, all the way down a subtree it is reporting
    /// the delete of. A forced walk consults no filter slot, so the hierarchy under a change
    /// is reported whole rather than folded against rules force put it past.
    #[tokio::test]
    async fn force_reports_an_excluded_path_under_a_deleted_subtree() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = test_repository_excluding(
                    immutable_store,
                    mutable_store,
                    repository_id,
                    "thing/hidden.txt",
                )
                .await;
                let repository = fixture.repository.clone();
                std::fs::create_dir_all(fixture.path.join("thing"))
                    .expect("Create directory failed");
                test_file_write(&fixture.path.join("thing/shown.txt"), b"in view");
                test_file_write(&fixture.path.join("thing/hidden.txt"), b"out of view");
                force_commit_fixture(&fixture).await;

                // A file where the directory was, so the whole subtree is reported deleted
                // through the change hierarchy rather than through the walk.
                std::fs::remove_dir_all(fixture.path.join("thing"))
                    .expect("Remove directory failed");
                test_file_write(&fixture.path.join("thing"), b"a file now");

                let (current, staged) = test_anchor_states(&repository).await;
                let changes = LORE_CONTEXT
                    .scope(
                        forced_execution(),
                        test_scan_with_intent(
                            repository.clone(),
                            staged,
                            current,
                            FilesystemDiffIntent::Stage(StageIntent::default()),
                        ),
                    )
                    .await;

                for path in ["thing/shown.txt", "thing/hidden.txt"] {
                    assert!(
                        changes.iter().any(|change| change.path.as_str() == path
                            && change.action == FileAction::Delete),
                        "a forced walk must report {path} as deleted, reported {:?}",
                        changes
                            .iter()
                            .map(|change| (change.path.as_str(), change.action))
                            .collect::<Vec<_>>()
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A node already carrying its staged action is left alone, so staging the same tree
    /// twice reports the second pass as staging nothing.
    #[tokio::test]
    async fn staging_a_tree_twice_settles_it_once() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                test_file_write(&fixture.path.join("script.sh"), b"staged once");

                let (current, staged) = test_anchor_states(&repository).await;
                let first = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current.clone(),
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;
                assert!(
                    first
                        .iter()
                        .any(|change| change.path.as_str() == "script.sh"),
                    "the first pass must stage the file"
                );

                let second = test_scan_with_intent(
                    repository.clone(),
                    staged,
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;
                assert!(
                    !second
                        .iter()
                        .any(|change| change.path.as_str() == "script.sh"),
                    "a node already staged must not be staged again, reported {:?}",
                    second.iter().map(|c| c.path.as_str()).collect::<Vec<_>>()
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A file the tree holds staged for delete and the file system still has is taken back:
    /// a staged delete the working tree contradicts would drop the file on commit.
    #[tokio::test]
    async fn staging_a_file_the_tree_holds_deleted_takes_the_delete_back() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                let script = fixture.path.join("script.sh");
                test_file_write(&script, b"committed");
                commit_fixture(&fixture).await;

                let (current, staged) = test_anchor_states(&repository).await;
                let node_id = staged
                    .find_node_link(repository.clone(), "script.sh")
                    .await
                    .expect("the tree must hold the committed file")
                    .node;

                std::fs::remove_file(&script).expect("Remove file failed");
                test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current.clone(),
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;
                let flags = staged_flags(&repository, &staged, node_id).await;
                assert!(
                    flags.contains(NodeFlags::StagedDelete),
                    "the fixture must stage the delete first, flags {flags:?}"
                );

                test_file_write(&script, b"back again");
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let flags = staged_flags(&repository, &staged, node_id).await;
                assert!(
                    !flags.contains(NodeFlags::StagedDelete),
                    "the delete must be taken back, flags {flags:?}"
                );
                assert!(
                    flags.contains(NodeFlags::StagedModify),
                    "the file must be settled as a modification, flags {flags:?}"
                );
                assert!(
                    changes
                        .iter()
                        .any(|change| change.path.as_str() == "script.sh"
                            && change.action == FileAction::Add),
                    "the file must be reported back"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// An execution context whose global force flag is set, which is what `--force` gives a
    /// command.
    fn forced_execution() -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
        std::sync::Arc::new(lore_revision::interface::ExecutionContext::new_client(
            lore_revision::interface::LoreGlobalArgs {
                force: 1,
                ..Default::default()
            },
            lore_revision::relay::EventDispatcher::no_dispatch(),
        ))
    }

    /// Force stages what the working copy already matches, the directory included: the user
    /// asked for those nodes to carry the action, and a walk that reported nothing would
    /// leave the ask unanswered.
    #[tokio::test]
    async fn force_stages_what_the_working_copy_already_matches() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                std::fs::create_dir_all(fixture.path.join("dir")).expect("Create directory failed");
                test_file_write(&fixture.path.join("dir/script.sh"), b"unchanged");
                commit_fixture(&fixture).await;

                let (current, staged) = test_anchor_states(&repository).await;
                let node_id = staged
                    .find_node_link(repository.clone(), "dir/script.sh")
                    .await
                    .expect("the tree must hold the committed file")
                    .node;
                let directory_id = staged
                    .find_node_link(repository.clone(), "dir")
                    .await
                    .expect("the tree must hold the committed directory")
                    .node;

                let changes = LORE_CONTEXT
                    .scope(
                        forced_execution(),
                        test_scan_with_intent(
                            repository.clone(),
                            staged.clone(),
                            current,
                            FilesystemDiffIntent::Stage(StageIntent::default()),
                        ),
                    )
                    .await;

                let flags = staged_flags(&repository, &staged, node_id).await;
                assert!(
                    flags.contains(NodeFlags::StagedModify),
                    "force must settle the file it was asked to stage, flags {flags:?}"
                );
                let flags = staged_flags(&repository, &staged, directory_id).await;
                assert!(
                    flags.contains(NodeFlags::StagedModify),
                    "force must settle the directory too, which holds no content to compare, flags {flags:?}"
                );
                for path in ["dir", "dir/script.sh"] {
                    assert!(
                        changes.iter().any(|change| change.path.as_str() == path),
                        "what force staged must be reported, missing {path}"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A staged modification records the size and mode the file was measured with, which is
    /// what a commit reads to realize it.
    #[tokio::test]
    async fn a_staged_modification_records_what_the_file_carries() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                let script = fixture.path.join("script.sh");
                test_file_write(&script, b"short");
                commit_fixture(&fixture).await;

                let grown = b"a good deal longer than the committed content";
                test_file_write(&script, grown);

                let (current, staged) = test_anchor_states(&repository).await;
                test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let node_id = staged
                    .find_node_link(repository.clone(), "script.sh")
                    .await
                    .expect("the tree must hold the file")
                    .node;
                let node = staged
                    .node(repository.clone(), node_id)
                    .await
                    .expect("the staged node must read back");
                assert_eq!(
                    grown.len() as u64,
                    node.size,
                    "the node must record the size the file was measured with"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Staging leaves a merge sibling alone, which a resolution has not consumed yet. A
    /// marking walk reports it, since the working tree does hold it.
    #[tokio::test]
    async fn staging_leaves_a_merge_sibling_alone() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let sibling = format!("script.sh{}", lore_revision::repository::THEIRS_SUFFIX);
                test_file_write(&fixture.path.join(&sibling), b"their version");

                let (current, staged) = test_anchor_states(&repository).await;
                let staged_changes = test_scan_with_intent(
                    repository.clone(),
                    staged,
                    current.clone(),
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;
                assert!(
                    !staged_changes
                        .iter()
                        .any(|change| change.path.as_str() == sibling),
                    "staging must leave the merge sibling alone"
                );

                let (_, staged) = test_anchor_states(&repository).await;
                let marked = test_scan_with_intent(
                    repository.clone(),
                    staged,
                    current,
                    FilesystemDiffIntent::MarkDirty,
                )
                .await;
                assert!(
                    marked.iter().any(|change| change.path.as_str() == sibling),
                    "a marking walk must report what the working tree holds"
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
