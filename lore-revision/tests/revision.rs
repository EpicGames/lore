// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Address;
    use lore_revision::change;
    use lore_revision::change::NodeChange;
    use lore_revision::change::NodeChangeState;
    use lore_revision::lore::RepositoryId;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::revision;
    use lore_revision::revision::ResolveSearchLocation;
    use lore_revision::revision::diff::LoreRevisionDiffFileEventData;
    use lore_revision::state::State;
    use lore_revision::util::path::RelativePathBuf;

    include!("helper.rs");

    async fn make_repo_context() -> Arc<RepositoryContext> {
        let (immutable, mutable, _execution) =
            test_store_create().await.expect("Failed to create stores");
        let tempdir = generate_tempdir();
        Arc::new(RepositoryContext::new(
            default_repository_creation_args(immutable, mutable).with_path(tempdir.path()),
        ))
    }

    // Exercises the BranchError::NotExist arm of branch::resolve.
    #[tokio::test]
    async fn resolve_unresolvable_branch_specifier_returns_revision_not_found() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let repository = make_repo_context().await;
                let err = revision::resolve(
                    repository,
                    "no-such-branch@5",
                    None,
                    ResolveSearchLocation::Local,
                )
                .await
                .expect_err("resolve should fail for unknown branch");
                assert!(
                    err.is_revision_not_found(),
                    "expected RevisionNotFound, got {err:?}"
                );
                assert!(!err.is_internal());
            })
            .await;
    }

    // Exercises the path where branch::resolve succeeds with a default
    // BranchStatus but the find lookups all miss — leaves `revision` zero.
    #[tokio::test]
    async fn resolve_unknown_revision_number_on_unknown_uuid_returns_revision_not_found() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let repository = make_repo_context().await;
                let signature = format!("{}@5", uuid::Uuid::now_v7());
                let err =
                    revision::resolve(repository, signature, None, ResolveSearchLocation::Local)
                        .await
                        .expect_err("resolve should fail for unknown revision number");
                assert!(
                    err.is_revision_not_found(),
                    "expected RevisionNotFound, got {err:?}"
                );
                assert!(!err.is_internal());
            })
            .await;
    }

    // Malformed user input is a user-actionable error: surface RevisionNotFound so
    // the CLI can tell the user their signature is unknown rather than burying it
    // as Internal.
    #[tokio::test]
    async fn resolve_malformed_bare_signature_returns_revision_not_found() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let repository = make_repo_context().await;
                let err = revision::resolve(
                    repository,
                    "not-a-hash-and-no-at",
                    None,
                    ResolveSearchLocation::Local,
                )
                .await
                .expect_err("resolve should fail for malformed signature");
                assert!(
                    err.is_revision_not_found(),
                    "expected RevisionNotFound, got {err:?}"
                );
                assert!(!err.is_internal());
            })
            .await;
    }

    #[tokio::test]
    async fn test_diff3() {
        let (_immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let _repository = RepositoryId::default();

                // TODO(mjansson): Move more revision state and tree logic to Rust to be able to setup
                // the data and logic for these tests
                let _base = State::new();
                //let base_tree = base.deserialize_tree().await?;
                // ... setup base revision state

                let _source = State::new();
                //let source_tree = source.deserialize_tree().await?;
                // ... setup source revision state

                let _target = State::new();
                //let target_tree = target.deserialize_tree().await?;
                // ... setup target revision state

                /*
                    let _diff = diff3(
                        store,
                        repository,
                        None,
                        base.revision(),
                        source.revision(),
                        target.revision(),
                        false,
                    )
                    .await
                    .expect("Diff failed");
                */
            })
            .await;
    }

    fn diff_change(
        repository: &Arc<RepositoryContext>,
        state: &Arc<State>,
        action: change::FileAction,
        path: &str,
        from_path: Option<&str>,
    ) -> NodeChange {
        let side = |node| NodeChangeState {
            repository: repository.clone(),
            state: state.clone(),
            node,
            flags: NodeFlags::NoFlags,
            address: Address::default(),
        };
        NodeChange {
            action,
            flags: change::Flags::None,
            from: side(1),
            to: side(2),
            path: RelativePathBuf::new().push_and_freeze(path),
            from_path: from_path.map(|path| RelativePathBuf::new().push_and_freeze(path)),
        }
    }

    // Without the source path the receiver sees a move as an add at the new path
    // and cannot reproduce it, so the event has to carry it across.
    #[tokio::test]
    async fn diff_file_event_carries_move_from_path() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let repository = make_repo_context().await;
                let state = Arc::new(State::new());
                let change = diff_change(
                    &repository,
                    &state,
                    change::FileAction::Move,
                    "new.txt",
                    Some("old.txt"),
                );

                let data = LoreRevisionDiffFileEventData::from_node_change(&change, true, true);

                assert_eq!(data.path.as_str(), "new.txt");
                assert_eq!(data.from_path.as_str(), "old.txt");
            })
            .await;
    }

    // A change with no source path maps to the empty string the C API documents,
    // not to a dangling pointer a receiver would read past.
    #[tokio::test]
    async fn diff_file_event_from_path_empty_without_move() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let repository = make_repo_context().await;
                let state = Arc::new(State::new());
                let change = diff_change(
                    &repository,
                    &state,
                    change::FileAction::Add,
                    "new.txt",
                    None,
                );

                let data = LoreRevisionDiffFileEventData::from_node_change(&change, false, true);

                assert!(data.from_path.is_empty());
                assert_eq!(data.from_path.as_str(), "");
            })
            .await;
    }

    #[test]
    fn from_metadata_with_truncated_utf8_value() {
        use lore_revision::metadata::Metadata;
        use lore_revision::revision::RevisionMetadata;

        let mut metadata = Metadata::new();
        // Truncated UTF-8: \xe4\xb8 is a 3-byte sequence missing the final byte
        metadata
            .set_binary("message", b"\xe4\xb8")
            .expect("set_binary message");
        metadata
            .set_binary("created-by", b"\xe4\xb8")
            .expect("set_binary created-by");

        let rev = RevisionMetadata::from_metadata(metadata);

        // message falls back to "<binary>" for invalid UTF-8
        assert_eq!(rev.message, "<binary>");
        // created_by falls back to None for invalid UTF-8
        assert!(rev.created_by.is_none());
    }
}
