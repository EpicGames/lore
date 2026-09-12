// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Working-tree scan handling of a reverted uncommitted directory add.
//!
//! When a directory (and its contents) is indexed as an uncommitted add and
//! then removed from disk before any commit, the next scan must discard the
//! stale node rather than report a delete. The parent has no committed base the
//! directory could be a deletion of, so a delete entry would be an unremovable
//! "zombie" — the same treatment already given to a reverted single-file add.

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_revision::change::FileAction;
    use lore_revision::lore::RepositoryId;

    include!("helper.rs");

    /// A directory indexed as an uncommitted add (along with its contents) and
    /// then removed from disk must be discarded on the next scan rather than
    /// reported as a delete: with no committed base there is nothing to delete,
    /// and a delete entry would be an unremovable "zombie".
    #[tokio::test]
    async fn removed_uncommitted_directory_is_discarded_not_deleted() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let path = fixture.path.as_path();

                // A directory with content that gets indexed as an uncommitted
                // add (the directory node plus its child file).
                std::fs::create_dir(path.join("ghost").as_path())
                    .expect("Create ghost directory failed");
                test_file_write(path.join("ghost").join("inner.txt").as_path(), &[7, 7, 7]);

                let (state_current, state_staged) = test_anchor_states(&repository).await;

                // First scan indexes the directory as an add.
                let changes = test_scan(
                    repository.clone(),
                    state_staged.clone(),
                    state_current.clone(),
                )
                .await;
                assert!(
                    changes
                        .iter()
                        .any(|c| c.path().as_str() == "ghost" && c.action == FileAction::Add),
                    "expected the new directory to be indexed as an add, found: {:?}",
                    changes
                        .iter()
                        .map(|c| (c.path().as_str().to_string(), c.action))
                        .collect::<Vec<_>>()
                );
                assert!(
                    changes
                        .iter()
                        .any(|c| c.path().as_str() == "ghost/inner.txt"),
                    "expected the directory's contents to be indexed too, found: {:?}",
                    changes
                        .iter()
                        .map(|c| (c.path().as_str().to_string(), c.action))
                        .collect::<Vec<_>>()
                );

                let inner_node_id = state_staged
                    .find_node_link(repository.clone(), "ghost/inner.txt")
                    .await
                    .expect("Failed to resolve the staged child node")
                    .node;

                // Remove it from disk and rescan against the same staged state.
                std::fs::remove_dir_all(path.join("ghost"))
                    .expect("Failed to remove ghost directory");
                let changes = test_scan(
                    repository.clone(),
                    state_staged.clone(),
                    state_current.clone(),
                )
                .await;
                assert!(
                    changes
                        .iter()
                        .all(|c| !c.path().as_str().starts_with("ghost")),
                    "removed uncommitted directory must be discarded, not reported, found: {:?}",
                    changes
                        .iter()
                        .map(|c| (c.path().as_str().to_string(), c.action))
                        .collect::<Vec<_>>()
                );

                // The subtree goes with the directory: a child left allocated is
                // unreachable from the root, so no scan result would reveal it.
                let inner_node = state_staged
                    .node(repository.clone(), inner_node_id)
                    .await
                    .expect("Failed to read the child node");
                assert!(
                    inner_node.is_discarded(),
                    "child node {inner_node_id} of the discarded directory must be discarded too"
                );

                // A further scan stays clean — the node was discarded, not merely
                // hidden, so it cannot resurface.
                let changes = test_scan(
                    repository.clone(),
                    state_staged.clone(),
                    state_current.clone(),
                )
                .await;
                assert!(
                    changes
                        .iter()
                        .all(|c| !c.path().as_str().starts_with("ghost")),
                    "discarded directory must not resurface on a later scan, found: {:?}",
                    changes
                        .iter()
                        .map(|c| (c.path().as_str().to_string(), c.action))
                        .collect::<Vec<_>>()
                );
            }))
            .await
            .expect("Test task panicked");
    }
}
