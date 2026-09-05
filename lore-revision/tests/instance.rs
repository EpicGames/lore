// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use futures::StreamExt;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Context;
    use lore_revision::branch;
    use lore_revision::instance::ANCHOR_CURRENT;
    use lore_revision::instance::ANCHOR_CURRENT_BRANCH;
    use lore_revision::instance::ANCHOR_STAGED;
    use lore_revision::instance::InstanceId;
    use lore_revision::instance::InstanceMetadata;
    use lore_revision::instance::InstanceStaleness;
    use lore_revision::instance::anchor_key;
    use lore_revision::instance::instance_key;
    use lore_revision::instance::instance_staleness;
    use lore_revision::instance::list_instances;
    use lore_revision::instance::{self};
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository;
    use lore_revision::repository::DOT_LORE;
    use lore_revision::repository::INSTANCE;
    use lore_revision::repository::RepositoryAccess;
    use lore_revision::repository::RepositoryConfig;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::repository::SALT_LORE;
    use lore_revision::repository::SharedStoreToUseConfig;
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

    fn instance_ids(instances: &[InstanceMetadata]) -> Vec<InstanceId> {
        instances
            .iter()
            .map(|metadata| metadata.instance_id)
            .collect()
    }

    /// Store `revision` as the current anchor of `instance_id`, directly in the store.
    async fn store_current_anchor_for(
        mutable_store: &Arc<dyn lore_storage::MutableStore>,
        repository_id: RepositoryId,
        instance_id: InstanceId,
        revision: lore_storage::Hash,
    ) {
        let (key, key_type) = anchor_key(SALT_LORE, ANCHOR_CURRENT, instance_id);
        mutable_store
            .clone()
            .store(repository_id, key, revision, key_type)
            .await
            .expect("store current anchor failed");
    }

    /// The current anchor of `instance_id`, zero when absent.
    async fn current_anchor_for(
        mutable_store: &Arc<dyn lore_storage::MutableStore>,
        repository_id: RepositoryId,
        instance_id: InstanceId,
    ) -> lore_storage::Hash {
        let (key, key_type) = anchor_key(SALT_LORE, ANCHOR_CURRENT, instance_id);
        mutable_store
            .clone()
            .load(repository_id, key, key_type)
            .await
            .unwrap_or_default()
    }

    /// Make `root` look like a checkout whose `.lore/instance` names `instance_id`.
    fn write_instance_file(root: &Path, instance_id: InstanceId) {
        let dot_lore = root.join(DOT_LORE);
        std::fs::create_dir_all(&dot_lore).expect("create .lore directory");
        test_file_write(&dot_lore.join(INSTANCE), instance_id.data());
    }

    /// Create a checkout at `root` on its own on-disk stores and return the
    /// instance ID minted for it. The context is dropped before returning so
    /// the stores and the write token are released for a re-open of the path.
    async fn create_checkout(root: &Path) -> InstanceId {
        let write_token = RepositoryWriteToken::acquire(root).await;
        let created = repository::create_local(
            root,
            &write_token,
            RepositoryId::from(uuid::Uuid::now_v7()),
            Context::from(uuid::Uuid::now_v7()),
            branch::DEFAULT_DEFAULT_NAME.to_string(),
            RepositoryConfig::default(),
            false,
        )
        .await
        .expect("create_local failed");
        created.instance_id
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
    async fn list_instances_for_all_repositories() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let id_a = InstanceId::generate();
                let id_b = InstanceId::generate();
                let id_c = InstanceId::generate();
                let repository =
                    test_repository(immutable_store.clone(), mutable_store.clone(), id_a).await;
                let repository_c =
                    test_repository(immutable_store.clone(), mutable_store.clone(), id_c).await;

                instance::register_instance(&repository, id_a, "/tmp/instance-a")
                    .await
                    .expect("register instance A failed");
                instance::register_instance(&repository, id_b, "/tmp/instance-b")
                    .await
                    .expect("register instance B failed");
                instance::register_instance(&repository_c, id_c, "/tmp/instance-c")
                    .await
                    .expect("register instance C failed");

                let instances = list_instances(&repository)
                    .await
                    .expect("List instances failed");
                assert!(instances.iter().any(|metadata| matches!(
                    metadata,
                    InstanceMetadata {
                        instance_id,
                        ..
                    } if *instance_id == id_a
                )));
                assert!(instances.iter().any(|metadata| matches!(
                    metadata,
                    InstanceMetadata {
                        instance_id,
                        ..
                    } if *instance_id == id_b
                )));

                let instances = list_instances(&repository_c)
                    .await
                    .expect("List instances failed");
                assert!(instances.iter().any(|metadata| matches!(
                    metadata,
                    InstanceMetadata {
                        instance_id,
                        ..
                    } if *instance_id == id_c
                )));

                let instances = list_instances(&Arc::new(RepositoryContext::new_null_context(
                    immutable_store,
                    mutable_store,
                )))
                .await
                .expect("List instances failed");
                assert!(instances.iter().any(|metadata| matches!(
                    metadata,
                    InstanceMetadata {
                        instance_id,
                        ..
                    } if *instance_id == id_a
                )));
                assert!(instances.iter().any(|metadata| matches!(
                    metadata,
                    InstanceMetadata {
                        instance_id,
                        ..
                    } if *instance_id == id_b
                )));
                assert!(instances.iter().any(|metadata| matches!(
                    metadata,
                    InstanceMetadata {
                        instance_id,
                        ..
                    } if *instance_id == id_c
                )));
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

    /// Registering an instance at a path retires whatever other instance was
    /// registered there: the directory holds one checkout, and its
    /// `.lore/instance` names the one being registered.
    #[tokio::test]
    async fn register_instance_retires_earlier_registration_at_same_path() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let earlier = InstanceId::generate();
                let elsewhere = InstanceId::generate();
                let current = InstanceId::generate();
                let repository =
                    test_repository(immutable_store, mutable_store.clone(), current).await;

                instance::register_instance(&repository, earlier, "/tmp/lore-instance-root")
                    .await
                    .expect("register earlier instance failed");
                instance::register_instance(&repository, elsewhere, "/tmp/lore-instance-other")
                    .await
                    .expect("register other instance failed");
                let revision = lore_storage::Hash::hash_buffer(b"earlier-revision");
                store_current_anchor_for(&mutable_store, repository.id, earlier, revision).await;

                instance::register_instance(&repository, current, "/tmp/lore-instance-root")
                    .await
                    .expect("register current instance failed");

                let ids = instance_ids(&list_instances(&repository).await.expect("list failed"));
                assert!(ids.contains(&current), "the new registration is listed");
                assert!(
                    !ids.contains(&earlier),
                    "the earlier registration at the same path must be retired"
                );
                assert!(
                    ids.contains(&elsewhere),
                    "a registration at another path must be kept"
                );
                assert!(
                    current_anchor_for(&mutable_store, repository.id, earlier)
                        .await
                        .is_zero(),
                    "retiring an instance removes its anchors"
                );
            }))
            .await
            .expect("Test failed");
    }

    /// Paths are compared in cleaned form, so a registration made through
    /// repeated separators or `.`/`..` segments still names the same root.
    #[tokio::test]
    async fn register_instance_matches_paths_after_normalization() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let earlier = InstanceId::generate();
                let current = InstanceId::generate();
                let repository = test_repository(immutable_store, mutable_store, current).await;

                instance::register_instance(&repository, earlier, "/tmp/lore-norm//sub/../root/.")
                    .await
                    .expect("register earlier instance failed");
                instance::register_instance(&repository, current, "/tmp/lore-norm/root")
                    .await
                    .expect("register current instance failed");

                let ids = instance_ids(&list_instances(&repository).await.expect("list failed"));
                assert_eq!(ids, vec![current]);
            }))
            .await
            .expect("Test failed");
    }

    /// With duplicate registrations for one path left by an earlier client, a
    /// lost `.lore/instance` is recovered as the newest of them.
    #[tokio::test]
    async fn recover_instance_id_prefers_newest_registration_for_path() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let older = InstanceId::generate();
                let newer = InstanceId::generate();
                let path = "/tmp/lore-instance-recover";
                let repository =
                    test_repository(immutable_store.clone(), mutable_store.clone(), newer).await;

                instance::store_instance_registration(&repository, older, path)
                    .await
                    .expect("register older instance failed");
                // Registration time is in milliseconds; make the two distinct.
                tokio::time::sleep(Duration::from_millis(5)).await;
                instance::store_instance_registration(&repository, newer, path)
                    .await
                    .expect("register newer instance failed");
                assert_eq!(
                    list_instances(&repository)
                        .await
                        .expect("list failed")
                        .len(),
                    2,
                    "the raw write leaves both registrations in place"
                );

                let recovered = instance::recover_instance_id(
                    repository.id,
                    mutable_store.clone(),
                    immutable_store.clone(),
                    path,
                )
                .await;
                assert_eq!(recovered, Some(newer));

                let elsewhere = instance::recover_instance_id(
                    repository.id,
                    mutable_store,
                    immutable_store,
                    "/tmp/lore-instance-unregistered",
                )
                .await;
                assert_eq!(elsewhere, None);
            }))
            .await
            .expect("Test failed");
    }

    #[tokio::test]
    async fn instance_staleness_classifies_registrations() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let root = TempDir::new("lore-instance-stale-");
                let current = InstanceId::generate();
                let other = InstanceId::generate();
                write_instance_file(&root, current);
                let bare = root.join("bare");
                std::fs::create_dir_all(&bare).expect("create bare directory");

                let root_path = root.display().to_string();
                assert_eq!(
                    instance_staleness(&root_path, current).await,
                    InstanceStaleness::Active,
                    "the instance the directory names is active"
                );
                assert_eq!(
                    instance_staleness(&root_path, other).await,
                    InstanceStaleness::Superseded,
                    "an instance the directory no longer names is superseded"
                );
                assert_eq!(
                    instance_staleness(&root.join("gone").display().to_string(), other).await,
                    InstanceStaleness::PathMissing
                );
                assert_eq!(
                    instance_staleness(&bare.display().to_string(), other).await,
                    InstanceStaleness::Active,
                    "a directory without an instance file cannot contradict a registration"
                );
                assert_eq!(
                    instance_staleness("", other).await,
                    InstanceStaleness::Active,
                    "an empty path is corrupt metadata, not a removed directory"
                );
                assert_eq!(
                    instance_staleness(&root_path, InstanceId::default()).await,
                    InstanceStaleness::Active,
                    "a zero instance ID cannot be compared with the file"
                );
            }))
            .await
            .expect("Test failed");
    }

    /// Prune removes a registration whose path is gone and one whose path
    /// now holds a different instance, and keeps the rest.
    #[tokio::test]
    async fn instance_prune_removes_missing_and_superseded_registrations() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let root = TempDir::new("lore-instance-prune-");
                let current = InstanceId::generate();
                let superseded = InstanceId::generate();
                let missing = InstanceId::generate();
                let unchecked = InstanceId::generate();
                write_instance_file(&root, current);
                let bare = root.join("bare");
                std::fs::create_dir_all(&bare).expect("create bare directory");

                let repository =
                    test_repository(immutable_store, mutable_store.clone(), current).await;
                let root_path = root.display().to_string();

                // Written raw, the way a client without path retirement left them.
                instance::store_instance_registration(&repository, superseded, &root_path)
                    .await
                    .expect("register superseded instance failed");
                instance::store_instance_registration(&repository, current, &root_path)
                    .await
                    .expect("register current instance failed");
                instance::store_instance_registration(
                    &repository,
                    missing,
                    &root.join("gone").display().to_string(),
                )
                .await
                .expect("register missing instance failed");
                instance::store_instance_registration(
                    &repository,
                    unchecked,
                    &bare.display().to_string(),
                )
                .await
                .expect("register unchecked instance failed");
                let revision = lore_storage::Hash::hash_buffer(b"superseded-revision");
                store_current_anchor_for(&mutable_store, repository.id, superseded, revision).await;

                let pruned = instance::instance_prune(repository.clone())
                    .await
                    .expect("prune failed");
                assert_eq!(pruned, 2);

                let mut ids =
                    instance_ids(&list_instances(&repository).await.expect("list failed"));
                ids.sort();
                let mut expected = vec![current, unchecked];
                expected.sort();
                assert_eq!(ids, expected);
                assert!(
                    current_anchor_for(&mutable_store, repository.id, superseded)
                        .await
                        .is_zero(),
                    "pruning removes the anchors of the superseded instance"
                );

                let pruned = instance::instance_prune(repository)
                    .await
                    .expect("second prune failed");
                assert_eq!(pruned, 0, "a second prune finds nothing left");
            }))
            .await
            .expect("Test failed");
    }

    /// `repository create --force` and a re-clone on a shared store both wipe
    /// `.lore` and mint a new instance ID while the shared mutable store keeps
    /// the registration of the instance they replaced. The new registration
    /// retires it, so the path is listed once.
    #[tokio::test]
    async fn recreating_a_checkout_on_a_shared_store_leaves_one_registration() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-shared-");
                let root = TempDir::new("lore-instance-root-");
                let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
                let default_branch = Context::from(uuid::Uuid::now_v7());
                let config = RepositoryConfig {
                    remote_url: Some("lore://localhost/lore-instance-test".to_string()),
                    shared_store_to_use: Some(SharedStoreToUseConfig {
                        use_shared_store: Some(true),
                        shared_store_path: Some(shared.display().to_string()),
                    }),
                    ..RepositoryConfig::default()
                };

                let create = |config: RepositoryConfig| {
                    let path = root.to_path_buf();
                    async move {
                        let write_token = RepositoryWriteToken::acquire(&path).await;
                        let repository = repository::create_local(
                            &path,
                            &write_token,
                            repository_id,
                            default_branch,
                            branch::DEFAULT_DEFAULT_NAME.to_string(),
                            config,
                            false,
                        )
                        .await
                        .expect("create_local failed");
                        let instances = list_instances(&repository).await.expect("list failed");
                        (repository.instance_id, instance_ids(&instances))
                    }
                };

                let (first, instances) = create(config.clone()).await;
                assert_eq!(instances, vec![first]);

                // Wipe `.lore` as `repository create --force` does; the shared store survives.
                std::fs::remove_dir_all(root.join(DOT_LORE)).expect("remove .lore");

                let (second, instances) = create(config).await;
                assert_ne!(
                    first, second,
                    "a re-created checkout gets a new instance ID"
                );
                assert_eq!(
                    instances,
                    vec![second],
                    "the registration left by the wiped checkout must be retired"
                );
            }))
            .await
            .expect("Test failed");
    }

    /// A lost `.lore/instance` is recovered from the registration recorded for
    /// the path, so re-opening the checkout does not register a second instance.
    #[tokio::test]
    async fn lost_instance_file_is_recovered_without_a_second_registration() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let root = TempDir::new("lore-instance-lost-");
                let created = create_checkout(&root).await;
                std::fs::remove_file(root.join(DOT_LORE).join(INSTANCE))
                    .expect("remove instance file");

                let write_token = RepositoryWriteToken::acquire(&root).await;
                let reloaded = repository::load_and_connect_with_token(
                    &root,
                    RepositoryAccess::ReadWrite,
                    Some(write_token.share()),
                )
                .await
                .expect("load_and_connect failed");

                assert_eq!(
                    reloaded.instance_id, created,
                    "the instance ID is recovered from the registration at this path"
                );
                assert_eq!(
                    instance_ids(&list_instances(&reloaded).await.expect("list failed")),
                    vec![created]
                );
                assert_eq!(
                    InstanceId::read_from_file(root.join(DOT_LORE).join(INSTANCE))
                        .expect("instance file rewritten"),
                    created
                );
            }))
            .await
            .expect("Test failed");
    }

    /// When `.lore/instance` names an instance the store has not seen, opening
    /// the checkout registers it and retires the instance previously registered
    /// at the path.
    #[tokio::test]
    async fn replaced_instance_file_registers_new_instance_and_retires_previous() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let root = TempDir::new("lore-instance-replaced-");
                let created = create_checkout(&root).await;
                let replacement = InstanceId::generate();
                write_instance_file(&root, replacement);

                let write_token = RepositoryWriteToken::acquire(&root).await;
                let reloaded = repository::load_and_connect_with_token(
                    &root,
                    RepositoryAccess::ReadWrite,
                    Some(write_token.share()),
                )
                .await
                .expect("load_and_connect failed");

                assert_eq!(reloaded.instance_id, replacement);
                let ids = instance_ids(&list_instances(&reloaded).await.expect("list failed"));
                assert_eq!(
                    ids,
                    vec![replacement],
                    "the previous registration at this path must be retired, got {created} too"
                );
            }))
            .await
            .expect("Test failed");
    }
}
