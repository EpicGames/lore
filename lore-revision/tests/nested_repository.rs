// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Filesystem-walk handling of a nested repository — a child directory that is
//! itself a Lore working copy (it carries its own `.lore/`).
//!
//! A nested repository is an implicit boundary on every walk: its contents
//! belong to the nested repository, not the parent, so the parent neither
//! descends into it nor indexes it. A scan additionally discards a
//! never-committed entry an older client indexed before the boundary existed,
//! taking its subtree with it since the parent has no committed base it could
//! be a deletion of; an entry the current revision holds stays tracked.

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository::DOT_LORE;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;

    include!("helper.rs");

    /// A child directory carrying its own `.lore/` is a nested repository: the
    /// parent scan must not index it or pull its contents into the parent tree.
    #[tokio::test]
    async fn nested_repository_is_not_indexed() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let path = fixture.path.as_path();

                // A tracked file in the parent so the scan has real work to do.
                test_file_write(path.join("parent_file.txt").as_path(), &[0, 1, 2, 3]);

                // A nested repository: a child directory with its own `.lore/`
                // control directory and content that belongs to it, not the parent.
                std::fs::create_dir_all(path.join("nested").join(DOT_LORE).as_path())
                    .expect("Create nested/.lore directory failed");
                test_file_write(path.join("nested").join("inner.txt").as_path(), &[9, 9, 9]);

                // A second nested repository below an untracked directory, so the
                // boundary is reached through the parent's recursion into a new
                // subtree rather than from the root listing.
                let deep = path.join("outer").join("deep");
                std::fs::create_dir_all(deep.join(DOT_LORE).as_path())
                    .expect("Create outer/deep/.lore directory failed");
                test_file_write(deep.join("inner.txt").as_path(), &[8, 8, 8]);
                test_file_write(
                    path.join("outer").join("outer_file.txt").as_path(),
                    &[5, 5, 5],
                );

                let (state_current, state_staged) = test_anchor_states(&repository).await;
                let changes = test_scan(repository.clone(), state_staged, state_current).await;
                let reported = || {
                    changes
                        .iter()
                        .map(|c| c.path().as_str().to_string())
                        .collect::<Vec<_>>()
                };

                assert!(
                    changes
                        .iter()
                        .any(|c| c.path().as_str() == "parent_file.txt"),
                    "expected the parent's own file to be indexed, found: {:?}",
                    reported()
                );
                assert!(
                    changes
                        .iter()
                        .all(|c| !c.path().as_str().starts_with("nested")),
                    "nested repository contents must not be indexed, found: {:?}",
                    reported()
                );
                assert!(
                    changes
                        .iter()
                        .any(|c| c.path().as_str() == "outer/outer_file.txt"),
                    "expected a new directory's own file to be indexed, found: {:?}",
                    reported()
                );
                assert!(
                    changes
                        .iter()
                        .all(|c| !c.path().as_str().starts_with("outer/deep")),
                    "a nested repository below a new directory must not be indexed, found: {:?}",
                    reported()
                );
            }))
            .await
            .expect("Test task panicked");
    }

    /// A directory already staged as a normal dirty-add that then becomes a
    /// nested repository root (a `.lore/` appears inside it) is a stale
    /// "zombie" entry: the next scan must discard the staged subtree instead
    /// of continuing to index the nested repository's contents.
    #[tokio::test]
    async fn staged_directory_becoming_nested_repository_is_discarded() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let path = fixture.path.as_path();

                // A plain child directory with content: the first scan stages
                // it as an ordinary dirty-add subtree.
                std::fs::create_dir(path.join("nested").as_path())
                    .expect("Create nested directory failed");
                test_file_write(path.join("nested").join("inner.txt").as_path(), &[9, 9, 9]);

                let (state_current, state_staged) = test_anchor_states(&repository).await;
                let changes = test_scan(
                    repository.clone(),
                    state_staged.clone(),
                    state_current.clone(),
                )
                .await;
                assert!(
                    changes
                        .iter()
                        .any(|c| c.path().as_str().starts_with("nested")),
                    "expected the plain directory to be indexed by the first scan"
                );

                let inner_node_id = state_staged
                    .find_node_link(repository.clone(), "nested/inner.txt")
                    .await
                    .expect("Failed to resolve the staged child node")
                    .node;

                // The directory becomes a nested repository root — as when
                // `lore repository create` runs inside a staged directory.
                std::fs::create_dir(path.join("nested").join(DOT_LORE).as_path())
                    .expect("Create nested/.lore directory failed");

                // The rescan discards the stale staged entry instead of
                // continuing to index the nested repository's contents.
                let changes =
                    test_scan(repository.clone(), state_staged.clone(), state_current).await;
                assert!(
                    changes
                        .iter()
                        .all(|c| !c.path().as_str().starts_with("nested")),
                    "zombie entry for a staged directory turned nested repository must be \
                     discarded, found: {:?}",
                    changes
                        .iter()
                        .map(|c| c.path().as_str().to_string())
                        .collect::<Vec<_>>()
                );

                // The staged subtree goes with the entry: a child left allocated
                // is unreachable from the root, so no scan result reveals it.
                let inner_node = state_staged
                    .node(repository.clone(), inner_node_id)
                    .await
                    .expect("Failed to read the child node");
                assert!(
                    inner_node.is_discarded(),
                    "child node {inner_node_id} of the discarded zombie entry must be discarded too"
                );
            }))
            .await
            .expect("Test task panicked");
    }

    /// A directory already committed into the parent tree keeps being tracked,
    /// and its contents keep being descended into and indexed, even after a
    /// `.lore/` control directory appears inside it. The discard applies to
    /// never-committed entries alone; untracking committed content is an
    /// explicit user action, not this scan.
    #[tokio::test]
    async fn committed_directory_becoming_nested_repository_stays_tracked() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let path = fixture.path.clone();

                // A plain directory with a file, staged and committed into the
                // parent tree — so it is present in state_current, not just
                // state_staged.
                std::fs::create_dir(path.join("nested").as_path())
                    .expect("Create nested directory failed");
                test_file_write(path.join("nested").join("inner.txt").as_path(), &[1, 2, 3]);

                file::stage::stage(
                    repository.clone(),
                    &fixture.write_token,
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
                    &fixture.write_token,
                    CommitOptions::new("Commit nested directory".to_string()),
                ))
                .await
                .expect("Commit failed");

                let (state_current, state_staged) = test_anchor_states(&repository).await;

                // The already-committed directory becomes a nested repository
                // root, as when `lore repository create` runs inside it.
                std::fs::create_dir(path.join("nested").join(DOT_LORE).as_path())
                    .expect("Create nested/.lore directory failed");

                // Modify the already-tracked file so an "unmodified, no diff"
                // scan result can't be mistaken for the boundary having
                // silently swallowed it: if the parent still descends into
                // and indexes `nested/`, this modification must surface.
                test_file_write(path.join("nested").join("inner.txt").as_path(), &[4, 5, 6]);

                let changes = test_scan(repository.clone(), state_staged, state_current).await;
                assert!(
                    changes
                        .iter()
                        .any(|c| c.path().as_str() == "nested/inner.txt"),
                    "a directory committed before becoming a nested repository root \
                     must stay tracked, with its contents still indexed, found: {:?}",
                    changes
                        .iter()
                        .map(|c| c.path().as_str().to_string())
                        .collect::<Vec<_>>()
                );
            }))
            .await
            .expect("Test task panicked");
    }

    /// Stages `target` into `fixture`'s repository, walking the file system.
    async fn stage_target(
        fixture: &TestRepository,
        target: &std::path::Path,
    ) -> Result<(), stage::StageError> {
        file::stage::stage(
            fixture.repository.clone(),
            &fixture.write_token,
            LoreArray::from_vec(vec![LoreString::from(&target.to_path_buf())]),
            StageOptions {
                case_change: stage::StageCaseChange::Error,
                node_flags: NodeFlags::NoFlags,
                file_id: None,
                no_children: false,
                scan: true,
            },
        )
        .await
        .map(|_| ())
    }

    /// A path the caller names explicitly does not reach the child loop that
    /// holds the boundary — `stage_filesystem_path` stages each component of the
    /// target before it recurses — so the boundary is held on the components
    /// themselves, and a nested repository among them is refused rather than
    /// staged into the parent.
    #[tokio::test]
    async fn staging_a_nested_repository_by_name_is_refused() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let path = fixture.path.clone();

                std::fs::create_dir_all(path.join("nested").join(DOT_LORE).as_path())
                    .expect("Create nested/.lore directory failed");
                test_file_write(path.join("nested").join("inner.txt").as_path(), &[9, 9, 9]);

                stage_target(&fixture, path.join("nested").as_path())
                    .await
                    .expect_err("naming a nested repository root is refused");

                // Reached as an ancestor of the target rather than as the target:
                // the file named belongs to the nested repository just the same.
                stage_target(&fixture, path.join("nested").join("inner.txt").as_path())
                    .await
                    .expect_err("naming a file inside a nested repository is refused");
            }))
            .await
            .expect("Test task panicked");
    }

    /// The refusal covers never-committed entries alone: a directory already in
    /// the parent's tree stays stageable by name once a `.lore/` appears inside
    /// it, matching the scan that keeps indexing it.
    #[tokio::test]
    async fn staging_a_committed_directory_turned_nested_by_name_is_allowed() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let path = fixture.path.clone();

                std::fs::create_dir(path.join("nested").as_path())
                    .expect("Create nested directory failed");
                test_file_write(path.join("nested").join("inner.txt").as_path(), &[1, 2, 3]);

                stage_target(&fixture, path.as_path())
                    .await
                    .expect("staging the plain directory succeeds");

                Box::pin(commit::commit(
                    fixture.repository.clone(),
                    &fixture.write_token,
                    CommitOptions::new("Commit nested directory".to_string()),
                ))
                .await
                .expect("Commit failed");

                std::fs::create_dir(path.join("nested").join(DOT_LORE).as_path())
                    .expect("Create nested/.lore directory failed");
                test_file_write(path.join("nested").join("inner.txt").as_path(), &[4, 5, 6]);

                stage_target(&fixture, path.join("nested").as_path())
                    .await
                    .expect("a directory committed before becoming nested stays stageable");
            }))
            .await
            .expect("Test task panicked");
    }
}
