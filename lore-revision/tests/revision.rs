// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lore_base::error::NoRemote;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Context;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::revision;
    use lore_revision::revision::ResolveSearchLocation;
    use lore_revision::state::State;
    use lore_transport::ProtocolError;

    include!("helper.rs");

    async fn make_repo_context() -> Arc<RepositoryContext> {
        let (immutable, mutable, _execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = Context::from(uuid::Uuid::now_v7());
        let tempdir = generate_tempdir();
        Arc::new(RepositoryContext::new(
            Some(tempdir.to_path_buf()),
            immutable,
            mutable,
            repository_id.into(),
            lore_revision::instance::InstanceId::default(),
            Err(ProtocolError::from(NoRemote)),
            Arc::default(),
            RepositoryFormat::Lore,
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

    // Exercises the branch@LATEST arm with a local-only search location: the
    // remote must not be consulted and the miss surfaces as RevisionNotFound.
    #[tokio::test]
    async fn resolve_latest_local_only_returns_revision_not_found_without_remote() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let repository = make_repo_context().await;
                let signature = format!("{}@LATEST", uuid::Uuid::now_v7());
                let err =
                    revision::resolve(repository, signature, None, ResolveSearchLocation::Local)
                        .await
                        .expect_err("resolve should fail for unknown branch latest");
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
