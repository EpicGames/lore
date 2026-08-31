// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::StreamExt;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Context;
    use lore_revision::instance::ANCHOR_CURRENT;
    use lore_revision::instance::ANCHOR_CURRENT_BRANCH;
    use lore_revision::instance::ANCHOR_STAGED;
    use lore_revision::instance::InstanceId;
    use lore_revision::instance::anchor_key;
    use lore_revision::instance::instance_key;
    use lore_revision::instance::{self};
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::SALT_LORE;
    use lore_storage::store_types::KeyType;

    include!("helper.rs");

    async fn test_repository(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        instance_id: InstanceId,
    ) -> Arc<RepositoryContext> {
        // Per-test unique path so each test acquires its own write mutex
        // rather than serializing on the shared system temp dir.
        let path = std::env::temp_dir().join(instance_id.to_string());
        let write_token = lore_revision::repository::RepositoryWriteToken::acquire(&path).await;
        Arc::new(
            RepositoryContext::new(
                default_repository_creation_args(immutable_store, mutable_store)
                    .with_path(&path)
                    .with_instance_id(instance_id),
            )
            .with_write_token(write_token.share()),
        )
    }

    #[tokio::test]
    async fn register_and_load_instance_metadata() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let id = InstanceId::generate();
                let repository = test_repository(immutable_store, mutable_store.clone(), id).await;

                instance::register_instance(&repository, id, "/tmp/test-instance")
                    .await
                    .expect("register_instance failed");

                let (key, key_type) = instance_key(SALT_LORE, id);
                assert_eq!(key_type, KeyType::Instance);

                let metadata_hash = mutable_store
                    .clone()
                    .load(repository.id, key, key_type)
                    .await
                    .expect("instance key not found in mutable store");
                assert!(!metadata_hash.is_zero());

                let metadata = instance::load_instance_metadata(&repository, metadata_hash)
                    .await
                    .expect("load_instance_metadata failed");
                assert_eq!(metadata.instance_id, id);
                assert_eq!(metadata.path, "/tmp/test-instance");
                assert!(metadata.created > 0);
            }))
            .await
            .expect("Test failed");
    }

    #[tokio::test]
    async fn list_instances_returns_registered_entries() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let id_a = InstanceId::generate();
                let id_b = InstanceId::generate();
                let repository =
                    test_repository(immutable_store, mutable_store.clone(), id_a).await;

                instance::register_instance(&repository, id_a, "/tmp/instance-a")
                    .await
                    .expect("register instance A failed");
                instance::register_instance(&repository, id_b, "/tmp/instance-b")
                    .await
                    .expect("register instance B failed");

                // Verify both instances can be loaded individually
                let (key_a, typ_a) = instance_key(SALT_LORE, id_a);
                let (key_b, typ_b) = instance_key(SALT_LORE, id_b);

                let val_a = mutable_store
                    .clone()
                    .load(repository.id, key_a, typ_a)
                    .await
                    .expect("load instance A failed");
                assert!(!val_a.is_zero(), "Instance A value should be non-zero");

                let val_b = mutable_store
                    .clone()
                    .load(repository.id, key_b, typ_b)
                    .await
                    .expect("load instance B failed");
                assert!(!val_b.is_zero(), "Instance B value should be non-zero");

                // Verify list enumerates both instances.
                // The mutable store embeds the key type in the stored key hash,
                // so we compare on the values (metadata hashes) which are unique
                // per instance and unmodified.
                let mut stream = mutable_store
                    .clone()
                    .list(repository.id, KeyType::Instance)
                    .await
                    .expect("list instances failed");

                let mut found_values = Vec::new();
                while let Some((_key, value)) = stream.next().await {
                    found_values.push(value);
                }

                assert!(
                    found_values.contains(&val_a),
                    "Instance A metadata not found in list (found {} entries)",
                    found_values.len()
                );
                assert!(
                    found_values.contains(&val_b),
                    "Instance B metadata not found in list (found {} entries)",
                    found_values.len()
                );
            }))
            .await
            .expect("Test failed");
    }

    #[tokio::test]
    async fn anchor_store_roundtrip() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let id = InstanceId::generate();
                let repository = test_repository(immutable_store, mutable_store.clone(), id).await;

                let fake_revision = lore_storage::Hash::hash_buffer(b"test-revision");

                let (current_key, current_type) = anchor_key(SALT_LORE, ANCHOR_CURRENT, id);
                mutable_store
                    .clone()
                    .store(repository.id, current_key, fake_revision, current_type)
                    .await
                    .expect("store current anchor failed");

                let loaded = mutable_store
                    .clone()
                    .load(repository.id, current_key, current_type)
                    .await
                    .expect("load current anchor failed");
                assert_eq!(loaded, fake_revision);

                // Store and load branch key
                let fake_branch = lore_storage::Context::from([0x42; 16]);
                let (branch_key, branch_type) = anchor_key(SALT_LORE, ANCHOR_CURRENT_BRANCH, id);
                mutable_store
                    .clone()
                    .store(
                        repository.id,
                        branch_key,
                        lore_storage::Hash::from_context(fake_branch),
                        branch_type,
                    )
                    .await
                    .expect("store current anchor branch failed");

                let loaded_branch = mutable_store
                    .clone()
                    .load(repository.id, branch_key, branch_type)
                    .await
                    .expect("load current anchor branch failed");
                assert_eq!(loaded_branch.to_context(), fake_branch);

                // Branch key is distinct from revision and staged keys
                assert_ne!(current_key, branch_key);

                let (staged_key, staged_type) = anchor_key(SALT_LORE, ANCHOR_STAGED, id);
                let result = mutable_store
                    .clone()
                    .load(repository.id, staged_key, staged_type)
                    .await;
                assert!(
                    result.is_err() || result.unwrap().is_zero(),
                    "Staged anchor should not exist yet"
                );
            }))
            .await
            .expect("Test failed");
    }

    #[tokio::test]
    async fn separate_instances_have_independent_anchors() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let id_a = InstanceId::generate();
                let id_b = InstanceId::generate();
                let repository =
                    test_repository(immutable_store, mutable_store.clone(), id_a).await;

                let revision_a = lore_storage::Hash::hash_buffer(b"revision-a");
                let revision_b = lore_storage::Hash::hash_buffer(b"revision-b");

                let (key_a, typ_a) = anchor_key(SALT_LORE, ANCHOR_CURRENT, id_a);
                let (key_b, typ_b) = anchor_key(SALT_LORE, ANCHOR_CURRENT, id_b);

                mutable_store
                    .clone()
                    .store(repository.id, key_a, revision_a, typ_a)
                    .await
                    .expect("store anchor A failed");
                mutable_store
                    .clone()
                    .store(repository.id, key_b, revision_b, typ_b)
                    .await
                    .expect("store anchor B failed");

                let loaded_a = mutable_store
                    .clone()
                    .load(repository.id, key_a, typ_a)
                    .await
                    .expect("load anchor A failed");
                let loaded_b = mutable_store
                    .clone()
                    .load(repository.id, key_b, typ_b)
                    .await
                    .expect("load anchor B failed");

                assert_eq!(loaded_a, revision_a);
                assert_eq!(loaded_b, revision_b);
                assert_ne!(loaded_a, loaded_b);
            }))
            .await
            .expect("Test failed");
    }

    #[tokio::test]
    async fn load_current_anchor_not_found_when_no_branch_key() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let repository =
                    test_repository(immutable_store, mutable_store, InstanceId::generate()).await;

                // No branch key stored — should return NotFound
                let result = instance::load_current_anchor(&repository).await;
                assert!(result.is_err(), "Expected NotFound for empty anchor");
            }))
            .await
            .expect("Test failed");
    }

    #[tokio::test]
    async fn load_current_anchor_returns_branch_with_zero_revision() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let branch = Context::from([0x42; 16]);
                let repository =
                    test_repository(immutable_store, mutable_store, InstanceId::generate()).await;

                // Store only the branch key (no revision) — simulates fresh repo
                instance::store_current_anchor_branch(&repository, branch)
                    .await
                    .expect("store branch failed");

                let (revision, loaded_branch) = instance::load_current_anchor(&repository)
                    .await
                    .expect("load_current_anchor failed");
                assert!(revision.is_zero(), "Revision should be zero");
                assert_eq!(loaded_branch, branch, "Branch should match");
            }))
            .await
            .expect("Test failed");
    }
}
