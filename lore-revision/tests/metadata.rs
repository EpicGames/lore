// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::metadata::METADATA_MAX_SIZE;
    use lore_revision::metadata::Metadata;
    use lore_revision::metadata::MetadataError;
    use lore_revision::repository::RepositoryContext;
    use rand::random;

    include!("helper.rs");

    async fn make_repo_context() -> Arc<RepositoryContext> {
        let (immutable, mutable, _execution) = test_store_create()
            .await
            .expect("Failed to create stores for metadata test");
        let tempdir = generate_tempdir();
        Arc::new(RepositoryContext::new(
            default_repository_creation_args(immutable, mutable).with_path(tempdir.path()),
        ))
    }

    #[test]
    fn metadata_types() {
        let _execution = setup_test_execution();

        let mut metadata = Metadata::new();

        let hash = Hash::from(rand::random::<[u8; 32]>());

        let context = random::<Context>();

        let address = Address {
            hash: Hash::from(rand::random::<[u8; 32]>()),
            context: random::<Context>(),
        };

        let uval = rand::random();

        let sval = "test value";

        let bval_true = true;
        let bval_false = false;

        metadata
            .set_address("test address", address)
            .expect("Failed to store address");
        metadata
            .set_context("test context", context)
            .expect("Failed to store context");
        metadata
            .set_hash("test hash", hash)
            .expect("Failed to store hash");
        metadata
            .set_u64("test u64", uval)
            .expect("Failed to store u64");
        metadata
            .set_string("test string", sval)
            .expect("Failed to store string");
        metadata
            .set_bool("test bool true", bval_true)
            .expect("Failed to store 'true' bool");
        metadata
            .set_bool("test bool false", bval_false)
            .expect("Failed to store 'false' bool");

        assert_eq!(
            address,
            metadata
                .get_address("test address")
                .expect("Failed to get address")
        );

        assert_eq!(
            context,
            metadata
                .get_context("test context")
                .expect("Failed to get context")
        );

        assert_eq!(
            hash,
            metadata.get_hash("test hash").expect("Failed to get hash")
        );

        assert_eq!(
            uval,
            metadata.get_u64("test u64").expect("Failed to get u64")
        );

        assert_eq!(
            sval,
            metadata
                .get_string("test string")
                .expect("Failed to get string")
        );

        assert_eq!(
            bval_true,
            metadata
                .get_bool("test bool true")
                .expect("Failed to get 'true' bool")
        );

        assert_eq!(
            bval_false,
            metadata
                .get_bool("test bool false")
                .expect("Failed to get 'false' bool")
        );

        metadata
            .get_address("test string")
            .expect_err("Did not fail to get mismatching type as expected");

        metadata
            .get_context("test hash")
            .expect_err("Did not fail to get mismatching type as expected");

        metadata
            .get_hash("test u64")
            .expect_err("Did not fail to get mismatching type as expected");

        metadata
            .get_u64("test context")
            .expect_err("Did not fail to get mismatching type as expected");

        metadata
            .get_context("test string")
            .expect_err("Did not fail to get mismatching type as expected");

        metadata
            .get_context("test bool true")
            .expect_err("Did not fail to get mismatching type as expected");

        metadata
            .get_context("test bool false")
            .expect_err("Did not fail to get mismatching type as expected");
    }

    #[test]
    fn remove_key_existing() {
        let _execution = setup_test_execution();
        let mut metadata = Metadata::new();

        metadata.set_string("keep", "value1").expect("set keep");
        metadata
            .set_binary("remove_me", &[1, 2, 3])
            .expect("set remove_me");
        metadata.set_u64("also_keep", 42).expect("set also_keep");

        assert!(metadata.remove_key("remove_me"));

        metadata
            .get_string("remove_me")
            .expect_err("removed key should not be found");

        assert_eq!(
            "value1",
            metadata
                .get_string("keep")
                .expect("keep should still exist")
        );
        assert_eq!(
            42,
            metadata
                .get_u64("also_keep")
                .expect("also_keep should still exist")
        );
    }

    #[test]
    fn remove_key_nonexistent() {
        let _execution = setup_test_execution();
        let mut metadata = Metadata::new();

        metadata.set_string("exists", "value").expect("set");
        assert!(!metadata.remove_key("nonexistent"));

        assert_eq!(
            "value",
            metadata
                .get_string("exists")
                .expect("existing key should be unaffected")
        );
    }

    #[test]
    fn remove_key_empty_metadata() {
        let _execution = setup_test_execution();
        let mut metadata = Metadata::new();
        assert!(!metadata.remove_key("anything"));
    }

    #[test]
    fn remove_key_last_key() {
        let _execution = setup_test_execution();
        let mut metadata = Metadata::new();

        metadata.set_string("only", "value").expect("set");
        assert!(metadata.remove_key("only"));
        metadata
            .get_string("only")
            .expect_err("removed key should not be found");
    }

    #[test]
    fn remove_key_then_re_add_different_type() {
        let _execution = setup_test_execution();
        let mut metadata = Metadata::new();

        metadata.set_binary("key", &[1, 2, 3]).expect("set binary");
        assert!(metadata.remove_key("key"));
        metadata
            .get_binary("key")
            .expect_err("removed key should not be found");

        metadata
            .set_string("key", "new_value")
            .expect("re-set as string");
        assert_eq!(
            "new_value",
            metadata
                .get_string("key")
                .expect("re-added key should exist")
        );
    }

    // ------------------------------------------------------------------
    // Size-bound enforcement tests for metadata
    // ------------------------------------------------------------------

    /// Metadata whose total is past the cap, built from values that each fit
    /// under it. A single value larger than the cap is refused where it is set,
    /// so the total is the only way to reach the check in `serialize`.
    fn over_the_cap_in_legal_entries() -> Metadata {
        let mut metadata = Metadata::new();
        let payload = vec![0u8; METADATA_MAX_SIZE / 2];
        for index in 0..3 {
            metadata
                .set_binary(&format!("part-{index}"), &payload)
                .expect("each value fits on its own");
        }
        metadata
    }

    #[tokio::test]
    async fn serialize_rejects_oversize() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let repo = make_repo_context().await;
                // Each value fits on its own, so only the total is over the cap
                // — which is the one the setter cannot know and serialize must.
                let metadata = over_the_cap_in_legal_entries();
                let err = metadata
                    .serialize(repo.clone())
                    .await
                    .expect_err("serialize should reject oversize metadata");
                assert!(
                    matches!(err, MetadataError::Oversized(_)),
                    "expected Oversized, got {err:?}"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn serialize_local_rejects_oversize() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let repo = make_repo_context().await;
                let metadata = over_the_cap_in_legal_entries();
                let err = metadata
                    .serialize_local(repo.clone())
                    .await
                    .expect_err("serialize_local should reject oversize metadata");
                assert!(matches!(err, MetadataError::Oversized(_)));
            })
            .await;
    }

    #[tokio::test]
    async fn serialize_accepts_just_under_limit() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let repo = make_repo_context().await;
                let mut metadata = Metadata::new();
                // Leave headroom for metadata header/item framing overhead.
                let payload = vec![0u8; METADATA_MAX_SIZE / 2];
                metadata
                    .set_binary("half", &payload)
                    .expect("setter should accept");
                let _hash = metadata
                    .serialize(repo.clone())
                    .await
                    .expect("serialize should succeed under the cap");
            })
            .await;
    }
}
