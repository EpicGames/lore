// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use futures::StreamExt;
    use lore_base::error::NotSupported;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Context;
    use lore_revision::branch;
    use lore_revision::fs::filesystem_provider::CloneAnswer;
    use lore_revision::fs::filesystem_provider::CloneRun;
    use lore_revision::fs::filesystem_provider::FilesystemProvider;
    use lore_revision::fs::filesystem_provider::FsError;
    use lore_revision::fs::filesystem_provider::InstanceCloneError;
    use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
    use lore_revision::fs::os::OsFilesystem;
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
    use lore_revision::lore::BranchId;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository;
    use lore_revision::repository::CONFIG;
    use lore_revision::repository::DOT_LORE;
    use lore_revision::repository::INSTANCE;
    use lore_revision::repository::RepositoryAccess;
    use lore_revision::repository::RepositoryConfig;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::repository::SALT_LORE;
    use lore_revision::repository::SharedStoreToUseConfig;
    use lore_storage::Hash;
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

    /// A context over the same stores with no write token, as a read-only
    /// command such as `repository instance list` holds.
    fn read_only_repository(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        repository_id: RepositoryId,
        instance_id: InstanceId,
    ) -> Arc<RepositoryContext> {
        let path = std::env::temp_dir().join(instance_id.to_string());
        Arc::new(RepositoryContext::new(
            default_repository_creation_args(immutable_store, mutable_store)
                .with_path(&path)
                .with_id(repository_id)
                .with_instance_id(instance_id),
        ))
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

    /// Put `instance_id` on `branch`, directly in the store.
    async fn store_current_branch_for(
        mutable_store: &Arc<dyn lore_storage::MutableStore>,
        repository_id: RepositoryId,
        instance_id: InstanceId,
        branch: BranchId,
    ) {
        let (key, key_type) = anchor_key(SALT_LORE, ANCHOR_CURRENT_BRANCH, instance_id);
        mutable_store
            .clone()
            .store(
                repository_id,
                key,
                lore_storage::Hash::from_context(branch),
                key_type,
            )
            .await
            .expect("store current branch failed");
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
                let result = instance::load_current_anchor_boxed(&repository).await;
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

                let (revision, loaded_branch) = instance::load_current_anchor_boxed(&repository)
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

    /// The `stale` field of a `RepositoryInstance` event is a C ABI value.
    #[test]
    fn staleness_event_flags_are_stable() {
        assert_eq!(InstanceStaleness::Active.as_event_flag(), 0);
        assert_eq!(InstanceStaleness::PathMissing.as_event_flag(), 1);
        assert_eq!(InstanceStaleness::Superseded.as_event_flag(), 2);
        assert_eq!(InstanceStaleness::CheckoutMissing.as_event_flag(), 3);
        assert!(!InstanceStaleness::Active.is_stale());
        for staleness in [
            InstanceStaleness::PathMissing,
            InstanceStaleness::Superseded,
            InstanceStaleness::CheckoutMissing,
        ] {
            assert!(staleness.is_stale(), "{staleness:?}");
        }
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
                    InstanceStaleness::CheckoutMissing,
                    "a directory holding no instance file holds no checkout"
                );
                let truncated = root.join("truncated");
                std::fs::create_dir_all(truncated.join(DOT_LORE)).expect("create .lore directory");
                test_file_write(&truncated.join(DOT_LORE).join(INSTANCE), &[0u8; 4]);
                assert_eq!(
                    instance_staleness(&truncated.display().to_string(), other).await,
                    InstanceStaleness::CheckoutMissing,
                    "an instance file too short to name an instance holds no checkout"
                );
                assert_eq!(
                    instance_staleness(&format!("{}\0", root.display()), other).await,
                    InstanceStaleness::Active,
                    "a path that cannot be examined is not evidence of a removed checkout"
                );
                let unreadable = root.join("unreadable");
                std::fs::create_dir_all(unreadable.join(DOT_LORE).join(INSTANCE))
                    .expect("create instance directory");
                assert_eq!(
                    instance_staleness(&unreadable.display().to_string(), other).await,
                    InstanceStaleness::Active,
                    "an instance file that exists but cannot be read is not evidence of a \
                     removed checkout"
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

    /// Prune removes a registration whose path is gone, one whose path now
    /// holds a different instance and one whose path holds no checkout, and
    /// keeps the live ones.
    #[tokio::test]
    async fn instance_prune_removes_missing_superseded_and_checkoutless_registrations() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let root = TempDir::new("lore-instance-prune-");
                let elsewhere = TempDir::new("lore-instance-prune-live-");
                let current = InstanceId::generate();
                let superseded = InstanceId::generate();
                let missing = InstanceId::generate();
                let unchecked = InstanceId::generate();
                let live = InstanceId::generate();
                write_instance_file(&root, current);
                write_instance_file(&elsewhere, live);
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
                instance::store_instance_registration(
                    &repository,
                    live,
                    &elsewhere.display().to_string(),
                )
                .await
                .expect("register live instance failed");
                let revision = lore_storage::Hash::hash_buffer(b"superseded-revision");
                store_current_anchor_for(&mutable_store, repository.id, superseded, revision).await;

                let pruned = instance::instance_prune(repository.clone())
                    .await
                    .expect("prune failed");
                assert_eq!(pruned, 3);

                let mut ids =
                    instance_ids(&list_instances(&repository).await.expect("list failed"));
                ids.sort();
                let mut expected = vec![current, live];
                expected.sort();
                assert_eq!(
                    ids, expected,
                    "a checkout that still names its instance is kept"
                );
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

    /// A duplicate at the path of an already registered instance — which no
    /// registration will come along to retire — is retired when a write
    /// command reads the instance list, without an explicit prune.
    #[tokio::test]
    async fn duplicate_registration_is_retired_when_a_write_command_reads_the_list() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let root = TempDir::new("lore-instance-duplicate-");
                let created = create_checkout(&root).await;
                let duplicate = InstanceId::generate();

                let write_token = RepositoryWriteToken::acquire(&root).await;
                let repository = repository::load_and_connect_with_token(
                    &root,
                    RepositoryAccess::ReadWrite,
                    Some(write_token.share()),
                )
                .await
                .expect("load_and_connect failed");
                assert_eq!(
                    repository.instance_id, created,
                    "the checkout keeps the instance it was created with"
                );

                // Written raw, the way a client without path retirement left it.
                instance::store_instance_registration(
                    &repository,
                    duplicate,
                    &root.display().to_string(),
                )
                .await
                .expect("register duplicate instance failed");
                let mut ids =
                    instance_ids(&list_instances(&repository).await.expect("list failed"));
                ids.sort();
                let mut expected = vec![created, duplicate];
                expected.sort();
                assert_eq!(ids, expected, "the raw write leaves both registrations");

                // What branch switch and sync do with the list.
                instance::warn_branch_multiple_instance(&repository, BranchId::default()).await;

                assert_eq!(
                    instance_ids(&list_instances(&repository).await.expect("list failed")),
                    vec![created],
                    "reading the list for a write command retires {duplicate}"
                );
            }))
            .await
            .expect("Test failed");
    }

    /// Reading the list retires a registration the path itself contradicts, and
    /// leaves one whose path is gone or holds no checkout for an explicit prune.
    /// Neither warns about a shared branch.
    #[tokio::test]
    async fn reading_the_list_retires_contradicted_registrations_only() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let root = TempDir::new("lore-instance-warn-");
                let current = InstanceId::generate();
                let superseded = InstanceId::generate();
                let missing = InstanceId::generate();
                let checkoutless = InstanceId::generate();
                write_instance_file(&root, current);
                let bare = root.join("bare");
                std::fs::create_dir_all(&bare).expect("create bare directory");

                let repository =
                    test_repository(immutable_store, mutable_store.clone(), current).await;
                let shared = BranchId::from(uuid::Uuid::now_v7());
                for (instance, path) in [
                    (superseded, root.display().to_string()),
                    (missing, root.join("gone").display().to_string()),
                    (checkoutless, bare.display().to_string()),
                ] {
                    instance::store_instance_registration(&repository, instance, &path)
                        .await
                        .expect("register instance failed");
                    store_current_branch_for(&mutable_store, repository.id, instance, shared).await;
                }

                let on_branch = instance::instances_on_branch(&repository, shared)
                    .await
                    .expect("instances on branch failed");
                assert!(
                    on_branch.is_empty(),
                    "no stale registration warns about a shared branch: {on_branch:?}"
                );

                let mut ids =
                    instance_ids(&list_instances(&repository).await.expect("list failed"));
                ids.sort();
                let mut expected = vec![missing, checkoutless];
                expected.sort();
                assert_eq!(
                    ids, expected,
                    "reading retires only the registration the path contradicts, leaving a \
                     missing path to prune"
                );
            }))
            .await
            .expect("Test failed");
    }

    /// A read-only command reports a stale registration and leaves it in the
    /// store, having no write token to retire it with.
    #[tokio::test]
    async fn a_read_only_reader_retires_nothing() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let root = TempDir::new("lore-instance-readonly-");
                let current = InstanceId::generate();
                let superseded = InstanceId::generate();
                write_instance_file(&root, current);

                let writer =
                    test_repository(immutable_store.clone(), mutable_store.clone(), current).await;
                instance::store_instance_registration(
                    &writer,
                    superseded,
                    &root.display().to_string(),
                )
                .await
                .expect("register superseded instance failed");
                writer
                    .flush(lore_storage::FlushOptions::default())
                    .await
                    .expect("flush failed");
                let repository_id = writer.id;
                drop(writer);

                let reader =
                    read_only_repository(immutable_store, mutable_store, repository_id, current);
                assert!(
                    reader.try_write_token().is_none(),
                    "the reader must hold no write token"
                );
                instance::instance_list(reader.clone())
                    .await
                    .expect("instance list failed");

                assert_eq!(
                    instance_ids(&list_instances(&reader).await.expect("list failed")),
                    vec![superseded],
                    "a reader that cannot write leaves the registration for a prune"
                );
            }))
            .await
            .expect("Test failed");
    }

    /// A checkout never retires the registration it is running as, however that
    /// registration's path reads.
    #[tokio::test]
    async fn the_current_instance_registration_is_kept() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let current = InstanceId::generate();
                let repository = test_repository(immutable_store, mutable_store, current).await;

                // The context path is a temp directory that was never created,
                // so this registration reads as stale for anyone but itself.
                let own_path = repository.path_for_display().to_string();
                instance::store_instance_registration(&repository, current, &own_path)
                    .await
                    .expect("register current instance failed");
                assert_eq!(
                    instance_staleness(&own_path, current).await,
                    InstanceStaleness::PathMissing
                );

                let pruned = instance::instance_prune(repository.clone())
                    .await
                    .expect("prune failed");
                assert_eq!(pruned, 0, "prune must keep the instance it runs as");
                assert_eq!(
                    instance_ids(&list_instances(&repository).await.expect("list failed")),
                    vec![current]
                );
            }))
            .await
            .expect("Test failed");
    }

    /// A new instance at `root` on the shared store at `shared`, as `lore repository create`
    /// leaves it, opened as `lore repository instance clone` opens it: for writing, holding its
    /// repository lock alone. It has a branch and no revision yet.
    async fn new_shared_store_instance(root: &Path, shared: &Path) -> Arc<RepositoryContext> {
        let config = RepositoryConfig {
            remote_url: Some("lore://localhost/lore-instance-clone-test".to_string()),
            shared_store_to_use: Some(SharedStoreToUseConfig {
                use_shared_store: Some(true),
                shared_store_path: Some(shared.display().to_string()),
            }),
            ..RepositoryConfig::default()
        };
        {
            let write_token = RepositoryWriteToken::acquire(root).await;
            repository::create_local(
                root,
                &write_token,
                RepositoryId::from(uuid::Uuid::now_v7()),
                Context::from(uuid::Uuid::now_v7()),
                branch::DEFAULT_DEFAULT_NAME.to_string(),
                config,
                false,
            )
            .await
            .expect("create_local failed");
        }
        test_file_write(&root.join("tracked.txt"), b"tracked");
        let write_token = RepositoryWriteToken::acquire(root).await;
        repository::load_and_connect_exclusive(root, write_token)
            .await
            .expect("load_and_connect_exclusive failed")
    }

    /// [`new_shared_store_instance`] with a current revision, branch and staged state of its
    /// own, each different.
    async fn shared_store_checkout(root: &Path, shared: &Path) -> Arc<RepositoryContext> {
        let repository = new_shared_store_instance(root, shared).await;
        let handle = repository
            .try_write_mutable_store()
            .expect("a write context");
        for function in [ANCHOR_CURRENT, ANCHOR_CURRENT_BRANCH, ANCHOR_STAGED] {
            let (key, key_type) = anchor_key(SALT_LORE, function, repository.instance_id);
            handle
                .store(
                    repository.id,
                    key,
                    Hash::from_context(Context::from(uuid::Uuid::now_v7())),
                    key_type,
                )
                .await
                .expect("store an anchor");
        }
        repository
    }

    /// The anchors of `instance_id`, in the order current revision, branch, staged.
    async fn anchors_of(repository: &RepositoryContext, instance_id: InstanceId) -> Vec<Hash> {
        let mut anchors = Vec::new();
        for function in [ANCHOR_CURRENT, ANCHOR_CURRENT_BRANCH, ANCHOR_STAGED] {
            let (key, key_type) = anchor_key(SALT_LORE, function, instance_id);
            anchors.push(
                repository
                    .read_mutable_store()
                    .load(repository.id, key, key_type)
                    .await
                    .unwrap_or_default(),
            );
        }
        anchors
    }

    /// A directory holding a file, whose permissions keep its owner from adding or removing
    /// entries, made writable again when dropped so the test's directory can be removed.
    struct ReadOnly(#[cfg_attr(not(target_family = "unix"), allow(dead_code))] std::path::PathBuf);

    impl ReadOnly {
        fn new(path: &Path) -> Self {
            std::fs::create_dir_all(path).expect("create the directory");
            test_file_write(&path.join("file"), b"file");
            #[cfg(target_family = "unix")]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o555))
                    .expect("make it read-only");
            }
            Self(path.to_path_buf())
        }
    }

    impl Drop for ReadOnly {
        fn drop(&mut self) {
            #[cfg(target_family = "unix")]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
        }
    }

    /// The entries of `parent` other than `kept`, as names.
    fn entries_besides(parent: &Path, kept: &[&Path]) -> Vec<String> {
        std::fs::read_dir(parent)
            .expect("read the parent")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| !kept.contains(&path.as_path()))
            .map(|path| path.display().to_string())
            .collect()
    }

    /// Plans, makes and finishes a clone of `repository` to `destination`, as the command does.
    /// Tests that check what a clone carries pass `allow_copy`, so they also run on volumes that
    /// cannot clone, as CI's NTFS and ext4 volumes are.
    async fn clone_instance(
        repository: &Arc<RepositoryContext>,
        destination: &Path,
        allow_copy: bool,
    ) -> Result<(), instance::InstanceError> {
        let destination = destination.display().to_string();
        let Some(plan) =
            instance::instance_clone_plan(repository.clone(), &destination, allow_copy).await?
        else {
            return Ok(());
        };
        let cloned = instance::instance_clone(repository.clone(), &plan).await;
        instance::instance_clone_finish(plan).await;
        cloned
    }

    /// A filesystem provider that answers `answer` for every clone and clones through the OS
    /// provider, or fails partway when `fails` is set, as a backend does when an entry it
    /// promised to share cannot be cloned.
    struct AnsweringProvider {
        answer: CloneAnswer,
        fails: bool,
        os: OsFilesystem,
    }

    #[async_trait::async_trait]
    impl FilesystemProvider for AnsweringProvider {
        async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
            FilesystemProvider::begin_operation(&self.os).await
        }

        async fn clone_answer(
            &self,
            _source: &Path,
            _dot_path: &Path,
            _destination: &Path,
        ) -> Result<CloneAnswer, InstanceCloneError> {
            Ok(self.answer.clone())
        }

        async fn clone_instance(
            &self,
            source: &Path,
            dot_path: &Path,
            destination: &Path,
            answer: &CloneAnswer,
            copy_unclonable: bool,
            run: &CloneRun,
        ) -> Result<lore_io::clone::CloneStats, InstanceCloneError> {
            if self.fails {
                return Err(NotSupported {
                    operation: "cloning an entry".to_string(),
                }
                .into());
            }
            self.os
                .clone_instance(source, dot_path, destination, answer, copy_unclonable, run)
                .await
        }
    }

    fn answering(root: &Path, answer: CloneAnswer, fails: bool) -> Arc<dyn FilesystemProvider> {
        Arc::new(AnsweringProvider {
            answer,
            fails,
            os: OsFilesystem::new(root),
        })
    }

    /// The answer of a provider that cannot share blocks on the volume of `root`: copying `size`
    /// with `available` free.
    fn copies(root: &Path, size: u64, available: u64) -> CloneAnswer {
        CloneAnswer::CopiesBytes {
            reason: "the volume does not clone files".to_string(),
            size: lore_io::clone::TreeSize {
                size,
                file_count: 1,
                entry_count: 1,
            },
            volume: lore_io::clone::volume_of(root).expect("the volume"),
            available,
        }
    }

    /// A clone gets an instance ID of its own, the source's anchors, a registration at its path,
    /// and the source's files and modified-time records, including those not yet written back. It
    /// leaves the source's lock file behind, carries no mark of a clone in progress, and leaves
    /// nothing beside it.
    #[tokio::test]
    async fn an_instance_clone_has_its_own_id_and_the_source_anchors_and_records() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-shared-");
                let root = TempDir::new("lore-instance-clone-root-");
                let parent = TempDir::new("lore-instance-clone-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let source = repository.instance_id;
                let record: lore_revision::records::RecordKey = [1u8; 16];
                repository
                    .instance_records()
                    .expect("the instance keeps its records on disk")
                    .set(record, 42)
                    .await;
                let destination = parent.join("clone");

                clone_instance(&repository, &destination, true)
                    .await
                    .expect("instance_clone failed");

                let dot = destination.join(DOT_LORE);
                let clone = InstanceId::read_from_file(dot.join(INSTANCE)).expect("clone ID");
                assert_ne!(clone, source, "the clone needs an instance ID of its own");
                assert_eq!(
                    std::fs::read(destination.join("tracked.txt")).expect("read the clone"),
                    b"tracked"
                );
                assert!(
                    dot.join(lore_revision::records::RECORDS_DIR)
                        .join("01")
                        .is_file(),
                    "records held in memory are written back before the copy"
                );
                assert!(
                    dot.join(lore_revision::records::RECORDS_DIR)
                        .join(lore_revision::records::NO_STORE_RECORDS)
                        .is_file(),
                    "the clone is marked as holding no store records"
                );
                assert!(
                    root.join(DOT_LORE).join("lock").exists() && !dot.join("lock").exists(),
                    "the lock file does not carry over"
                );
                assert!(!dot.join(instance::CLONE_IN_PROGRESS_MARK).exists());
                let source_anchors = anchors_of(&repository, source).await;
                assert!(!source_anchors[0].is_zero());
                assert_eq!(anchors_of(&repository, clone).await, source_anchors);
                let instances = list_instances(&repository).await.expect("list failed");
                let registered = instances
                    .iter()
                    .find(|instance| instance.instance_id == clone)
                    .expect("the clone is registered");
                let resolved = destination.canonicalize().expect("resolve the clone");
                assert_eq!(
                    registered.path,
                    lore_revision::util::path::clean(resolved.to_string_lossy().into_owned())
                );
                assert_eq!(
                    entries_besides(&parent, &[&destination]),
                    Vec::<String>::new(),
                    "no in-progress directory or lock file is left beside the clone"
                );
            }))
            .await
            .expect("Test failed");
    }

    /// An instance before its first commit has a branch and no revision, and its clone takes
    /// the same.
    #[tokio::test]
    async fn a_clone_of_an_instance_with_no_revision_yet_takes_its_branch() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-new-shared-");
                let root = TempDir::new("lore-instance-clone-new-root-");
                let parent = TempDir::new("lore-instance-clone-new-parent-");
                let repository = new_shared_store_instance(&root, &shared).await;
                let destination = parent.join("clone");

                clone_instance(&repository, &destination, true)
                    .await
                    .expect("a new instance is cloned");

                let clone = InstanceId::read_from_file(destination.join(DOT_LORE).join(INSTANCE))
                    .expect("clone ID");
                let source_anchors = anchors_of(&repository, repository.instance_id).await;
                assert!(source_anchors[0].is_zero(), "no revision yet");
                assert!(!source_anchors[1].is_zero(), "a branch");
                assert_eq!(anchors_of(&repository, clone).await, source_anchors);
            }))
            .await
            .expect("Test failed");
    }

    /// A clone of an instance an older client made, which is not marked as holding no store
    /// records, is marked: its new instance ID has none.
    #[tokio::test]
    async fn a_clone_of_an_unmarked_instance_is_marked() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-unmarked-shared-");
                let root = TempDir::new("lore-instance-clone-unmarked-root-");
                let parent = TempDir::new("lore-instance-clone-unmarked-parent-");
                let mark = |root: &Path| {
                    root.join(DOT_LORE)
                        .join(lore_revision::records::RECORDS_DIR)
                        .join(lore_revision::records::NO_STORE_RECORDS)
                };
                let repository = shared_store_checkout(&root, &shared).await;
                std::fs::remove_file(mark(&root)).expect("unmark the source");
                let destination = parent.join("clone");

                clone_instance(&repository, &destination, true)
                    .await
                    .expect("instance_clone failed");

                assert!(mark(&destination).is_file(), "the clone is marked");
                assert!(!mark(&root).exists(), "the source is left as it was");
            }))
            .await
            .expect("Test failed");
    }

    /// A clone whose last step fails leaves its in-progress directory until it is finished, then
    /// nothing at the destination or beside it, a read-only directory it copied included, and no
    /// keys for the clone in the store.
    #[tokio::test]
    async fn a_failed_instance_clone_leaves_no_instance_behind() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-fail-shared-");
                let root = TempDir::new("lore-instance-clone-fail-root-");
                let parent = TempDir::new("lore-instance-clone-fail-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let _read_only = ReadOnly::new(&root.join("read-only"));
                let destination = parent.join("clone");
                let plan = instance::instance_clone_plan(
                    repository.clone(),
                    &destination.display().to_string(),
                    true,
                )
                .await
                .expect("plan the clone")
                .expect("not a dry run");

                let result = instance::instance_clone_with(
                    repository.clone(),
                    &plan,
                    async |_: &Path, _: &Path| Err(std::io::Error::other("refused")),
                    instance::register_clone,
                )
                .await;

                assert!(result.is_err());
                assert!(
                    plan.in_progress().is_dir(),
                    "the in-progress directory stays until the clone is finished"
                );
                let clone_id = plan.clone_id();
                instance::instance_clone_finish(plan).await;
                assert!(!destination.exists());
                assert_eq!(entries_besides(&parent, &[]), Vec::<String>::new());
                assert_eq!(
                    instance_ids(&list_instances(&repository).await.expect("list failed")),
                    vec![repository.instance_id],
                    "the clone has no registration"
                );
                assert_eq!(
                    anchors_of(&repository, clone_id).await,
                    vec![Hash::default(); 3],
                    "the clone's anchors are removed"
                );
            }))
            .await
            .expect("Test failed");
    }

    /// A clone cancelled after its plan and before its copy starts, as a command dropped while it
    /// waits for the instance's lock is, leaves nothing beside the destination: no in-progress
    /// directory and no lock file.
    #[tokio::test]
    async fn a_clone_cancelled_before_its_copy_leaves_no_lock_file() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-cancelled-shared-");
                let root = TempDir::new("lore-instance-clone-cancelled-root-");
                let parent = TempDir::new("lore-instance-clone-cancelled-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let destination = parent.join("clone");
                let plan = instance::instance_clone_plan(
                    repository.clone(),
                    &destination.display().to_string(),
                    true,
                )
                .await
                .expect("plan the clone")
                .expect("not a dry run");
                assert_eq!(
                    entries_besides(&parent, &[]).len(),
                    1,
                    "the plan holds the in-progress directory's lock file"
                );

                drop(plan);

                assert_eq!(entries_besides(&parent, &[]), Vec::<String>::new());
            }))
            .await
            .expect("Test failed");
    }

    /// An empty directory made at the destination after the clone checked it, and before the
    /// clone moves into place, is kept: the move refuses to replace it, and the clone fails as
    /// invalid arguments, leaving nothing beside the destination once it is finished.
    #[tokio::test]
    async fn a_directory_made_at_the_destination_before_the_move_is_kept() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-kept-shared-");
                let root = TempDir::new("lore-instance-clone-kept-root-");
                let parent = TempDir::new("lore-instance-clone-kept-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let destination = parent.join("clone");
                let plan = instance::instance_clone_plan(
                    repository.clone(),
                    &destination.display().to_string(),
                    true,
                )
                .await
                .expect("plan the clone")
                .expect("not a dry run");

                let result = instance::instance_clone_with(
                    repository.clone(),
                    &plan,
                    async |from: &Path, to: &Path| {
                        std::fs::create_dir(to).expect("make a directory there");
                        instance::move_clone_into_place(from, to).await
                    },
                    instance::register_clone,
                )
                .await;
                instance::instance_clone_finish(plan).await;

                assert!(
                    result.as_ref().is_err_and(|err| err.is_invalid_arguments()),
                    "{:?}",
                    result.err()
                );
                assert_eq!(
                    std::fs::read_dir(&destination)
                        .expect("the directory is kept")
                        .count(),
                    0,
                    "and stays empty"
                );
                assert_eq!(
                    entries_besides(&parent, &[destination.as_path()]),
                    Vec::<String>::new()
                );
            }))
            .await
            .expect("Test failed");
    }

    /// A clone stopped as it moves into place leaves an in-progress copy that names its own ID,
    /// never the source's, and no registration. The next clone to the same destination removes
    /// the copy.
    #[tokio::test]
    async fn an_interrupted_instance_clone_leaves_no_registration_and_is_removed_by_the_next() {
        use futures::FutureExt;

        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-crash-shared-");
                let root = TempDir::new("lore-instance-clone-crash-root-");
                let parent = TempDir::new("lore-instance-clone-crash-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let destination = parent.join("clone");
                let plan = instance::instance_clone_plan(
                    repository.clone(),
                    &destination.display().to_string(),
                    true,
                )
                .await
                .expect("plan the clone")
                .expect("not a dry run");

                let interrupted = std::panic::AssertUnwindSafe(instance::instance_clone_with(
                    repository.clone(),
                    &plan,
                    async |_: &Path, _: &Path| panic!("interrupted before the move"),
                    instance::register_clone,
                ))
                .catch_unwind()
                .await;
                let left = plan.in_progress().to_path_buf();
                // The process ends without finishing, which releases the in-progress lock.
                drop(plan);

                assert!(interrupted.is_err());
                assert!(!destination.exists());
                let left_id = InstanceId::read_from_file(left.join(DOT_LORE).join(INSTANCE))
                    .expect("the in-progress copy names an instance");
                assert_ne!(left_id, repository.instance_id);
                assert_eq!(
                    instance_ids(&list_instances(&repository).await.expect("list failed")),
                    vec![repository.instance_id]
                );

                clone_instance(&repository, &destination, true)
                    .await
                    .expect("clone again");
                assert_eq!(
                    entries_besides(&parent, &[&destination]),
                    Vec::<String>::new(),
                    "the interrupted clone's in-progress directory and lock file are removed"
                );
            }))
            .await
            .expect("Test failed");
    }

    /// A clone removes beside its destination only a directory an interrupted clone left: one
    /// named for a clone to that destination, with a `UUIDv7` instance ID, and holding the mark of
    /// a clone in progress. A directory named like one but unmarked, or named otherwise, stays.
    #[tokio::test]
    async fn only_a_marked_clone_left_in_progress_is_removed() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-leftover-shared-");
                let root = TempDir::new("lore-instance-clone-leftover-root-");
                let parent = TempDir::new("lore-instance-clone-leftover-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let destination = parent.join("clone");
                let left = parent.join(format!("clone.cloning-{}", InstanceId::generate()));
                let unmarked = parent.join(format!("clone.cloning-{}", InstanceId::generate()));
                let named_otherwise = parent.join("clone.cloning-notes");
                std::fs::create_dir_all(left.join(DOT_LORE)).expect("create the leftover");
                test_file_write(
                    &left.join(DOT_LORE).join(instance::CLONE_IN_PROGRESS_MARK),
                    b"",
                );
                for kept in [&unmarked, &named_otherwise] {
                    std::fs::create_dir_all(kept.join(DOT_LORE)).expect("create a sibling");
                    test_file_write(&kept.join("kept.txt"), b"kept");
                }

                clone_instance(&repository, &destination, true)
                    .await
                    .expect("instance_clone failed");

                assert!(!left.exists(), "the marked leftover is removed");
                assert!(unmarked.join("kept.txt").is_file());
                assert!(named_otherwise.join("kept.txt").is_file());
            }))
            .await
            .expect("Test failed");
    }

    /// An in-progress directory whose clone still holds its lock is not taken for a leftover.
    #[tokio::test]
    async fn a_clone_in_progress_keeps_its_directory_from_another() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-busy-shared-");
                let root = TempDir::new("lore-instance-clone-busy-root-");
                let parent = TempDir::new("lore-instance-clone-busy-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let destination = parent.join("clone");
                let first = instance::instance_clone_plan(
                    repository.clone(),
                    &destination.display().to_string(),
                    true,
                )
                .await
                .expect("plan the clone")
                .expect("not a dry run");
                std::fs::create_dir_all(first.in_progress().join(DOT_LORE))
                    .expect("start the first clone");
                test_file_write(
                    &first
                        .in_progress()
                        .join(DOT_LORE)
                        .join(instance::CLONE_IN_PROGRESS_MARK),
                    b"",
                );

                let second = instance::instance_clone_plan(
                    repository.clone(),
                    &destination.display().to_string(),
                    true,
                )
                .await
                .expect("plan the clone")
                .expect("not a dry run");

                assert!(
                    first.in_progress().is_dir(),
                    "a clone in progress keeps its directory"
                );
                instance::instance_clone_finish(second).await;
                instance::instance_clone_finish(first).await;
                assert_eq!(entries_besides(&parent, &[]), Vec::<String>::new());
            }))
            .await
            .expect("Test failed");
    }

    /// A provider that cannot clone, as a virtual instance's is, is refused before anything is
    /// written, even with a copy allowed.
    #[tokio::test]
    async fn an_instance_clone_the_provider_cannot_make_is_refused() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-unsupported-shared-");
                let root = TempDir::new("lore-instance-clone-unsupported-root-");
                let parent = TempDir::new("lore-instance-clone-unsupported-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let provider = answering(
                    &root,
                    CloneAnswer::Unsupported {
                        reason: "this filesystem provider cannot clone instances".to_string(),
                    },
                    false,
                );

                let result = instance::instance_clone_plan_with(
                    provider,
                    repository,
                    &parent.join("clone").display().to_string(),
                    true,
                )
                .await;

                assert!(
                    result.as_ref().is_err_and(|err| err.is_not_supported()),
                    "an unsupported clone is refused: {:?}",
                    result.err()
                );
                assert_eq!(entries_besides(&parent, &[]), Vec::<String>::new());
            }))
            .await
            .expect("Test failed");
    }

    /// A clone that would copy is refused without a copy allowed, and made with one, by copying.
    #[tokio::test]
    async fn an_instance_clone_that_would_copy_needs_a_copy_allowed() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-copy-shared-");
                let root = TempDir::new("lore-instance-clone-copy-root-");
                let parent = TempDir::new("lore-instance-clone-copy-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let destination = parent.join("clone");
                let provider = answering(&root, copies(&root, 7, u64::MAX), false);

                let refused = instance::instance_clone_plan_with(
                    provider.clone(),
                    repository.clone(),
                    &destination.display().to_string(),
                    false,
                )
                .await;
                assert!(
                    refused.as_ref().is_err_and(|err| err.is_not_supported()),
                    "a copy is refused unless allowed: {:?}",
                    refused.err()
                );
                assert_eq!(entries_besides(&parent, &[]), Vec::<String>::new());

                let plan = instance::instance_clone_plan_with(
                    provider,
                    repository.clone(),
                    &destination.display().to_string(),
                    true,
                )
                .await
                .expect("a copy allowed is planned")
                .expect("not a dry run");
                instance::instance_clone(repository.clone(), &plan)
                    .await
                    .expect("the copy is made");
                instance::instance_clone_finish(plan).await;

                assert_eq!(
                    std::fs::read(destination.join("tracked.txt")).expect("read the copy"),
                    b"tracked"
                );
                assert_eq!(
                    list_instances(&repository)
                        .await
                        .expect("list failed")
                        .len(),
                    2
                );
            }))
            .await
            .expect("Test failed");
    }

    /// A copy allowed is still refused, as oversized, when the destination has less room than it
    /// takes.
    #[tokio::test]
    async fn an_instance_copy_that_does_not_fit_is_refused() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-space-shared-");
                let root = TempDir::new("lore-instance-clone-space-root-");
                let parent = TempDir::new("lore-instance-clone-space-parent-");
                let repository = shared_store_checkout(&root, &shared).await;

                let result = instance::instance_clone_plan_with(
                    answering(&root, copies(&root, 100, 99), false),
                    repository,
                    &parent.join("clone").display().to_string(),
                    true,
                )
                .await;

                assert!(
                    result.as_ref().is_err_and(|err| err.is_oversized()),
                    "a copy that does not fit is refused: {:?}",
                    result.err()
                );
                assert_eq!(entries_besides(&parent, &[]), Vec::<String>::new());
            }))
            .await
            .expect("Test failed");
    }

    /// A clone whose backend cannot clone an entry it promised to share fails as not supported,
    /// and leaves no instance behind.
    #[tokio::test]
    async fn an_instance_clone_that_fails_partway_leaves_no_instance_behind() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-partway-shared-");
                let root = TempDir::new("lore-instance-clone-partway-root-");
                let parent = TempDir::new("lore-instance-clone-partway-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let destination = parent.join("clone");
                let shares = CloneAnswer::SharesBlocks {
                    backend: lore_io::clone::Backend::Apfs,
                    volume: lore_io::clone::volume_of(&root).expect("the volume"),
                    entry_by_entry: Default::default(),
                };
                let plan = instance::instance_clone_plan_with(
                    answering(&root, shares, true),
                    repository.clone(),
                    &destination.display().to_string(),
                    false,
                )
                .await
                .expect("plan the clone")
                .expect("not a dry run");

                let result = instance::instance_clone(repository.clone(), &plan).await;
                instance::instance_clone_finish(plan).await;

                assert!(
                    result.as_ref().is_err_and(|err| err.is_not_supported()),
                    "{:?}",
                    result.err()
                );
                assert!(!destination.exists());
                assert_eq!(entries_besides(&parent, &[]), Vec::<String>::new());
                assert_eq!(
                    instance_ids(&list_instances(&repository).await.expect("list failed")),
                    vec![repository.instance_id]
                );
            }))
            .await
            .expect("Test failed");
    }

    /// A destination that is empty, exists, lies inside an instance, or whose name leaves no room
    /// for the in-progress directory's name beside it, is refused as invalid arguments, as a move
    /// refuses such a target.
    #[tokio::test]
    async fn a_clone_to_a_destination_that_exists_is_nested_or_is_named_too_long_is_refused() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-refused-shared-");
                let root = TempDir::new("lore-instance-clone-refused-root-");
                let parent = TempDir::new("lore-instance-clone-refused-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                std::fs::create_dir(parent.join("taken")).expect("take the destination");

                // A name the in-progress directory beside it would take past 255 bytes.
                let too_long = parent.join("n".repeat(210));
                for destination in [
                    parent.join("taken"),
                    root.join("inside"),
                    too_long.clone(),
                    std::path::PathBuf::new(),
                ] {
                    let result = clone_instance(&repository, &destination, true).await;
                    assert!(
                        result.as_ref().is_err_and(|err| err.is_invalid_arguments()),
                        "{}: {:?}",
                        destination.display(),
                        result.err()
                    );
                }
                assert!(!root.join("inside").exists());
                assert!(!too_long.exists());
            }))
            .await
            .expect("Test failed");
    }

    /// An instance with stores of its own is refused, since the clone's anchors and registration
    /// would be written to stores the copy does not read.
    #[tokio::test]
    async fn a_local_store_instance_clone_is_refused() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let root = TempDir::new("lore-instance-clone-local-root-");
                let parent = TempDir::new("lore-instance-clone-local-parent-");
                create_checkout(&root).await;
                let write_token = RepositoryWriteToken::acquire(&root).await;
                let repository = repository::load_and_connect_exclusive(&root, write_token)
                    .await
                    .expect("load_and_connect_exclusive failed");

                let result = clone_instance(&repository, &parent.join("clone"), true).await;

                assert!(result.is_err_and(|err| err.is_not_supported()));
                assert_eq!(entries_besides(&parent, &[]), Vec::<String>::new());
            }))
            .await
            .expect("Test failed");
    }

    /// A provider that, while it copies, registers `other` at `target` and makes the destination
    /// that instance's, as a second clone to the same destination that finishes first does, then
    /// clones through the OS provider.
    struct RacingProvider {
        repository: Arc<RepositoryContext>,
        other: InstanceId,
        target: String,
        os: OsFilesystem,
    }

    #[async_trait::async_trait]
    impl FilesystemProvider for RacingProvider {
        async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
            FilesystemProvider::begin_operation(&self.os).await
        }

        async fn clone_answer(
            &self,
            source: &Path,
            dot_path: &Path,
            destination: &Path,
        ) -> Result<CloneAnswer, InstanceCloneError> {
            self.os.clone_answer(source, dot_path, destination).await
        }

        async fn clone_instance(
            &self,
            source: &Path,
            dot_path: &Path,
            destination: &Path,
            answer: &CloneAnswer,
            copy_unclonable: bool,
            run: &CloneRun,
        ) -> Result<lore_io::clone::CloneStats, InstanceCloneError> {
            instance::store_instance_registration(&self.repository, self.other, &self.target)
                .await
                .expect("register the other instance");
            let taken = Path::new(&self.target).join(DOT_LORE);
            std::fs::create_dir_all(&taken).expect("take the destination");
            self.other
                .write_to_file(taken.join(INSTANCE))
                .await
                .expect("name the other instance");
            self.os
                .clone_instance(source, dot_path, destination, answer, copy_unclonable, run)
                .await
        }
    }

    /// A clone whose destination another instance takes and registers while it copies fails at
    /// the move, and leaves that instance's registration in place.
    #[tokio::test]
    async fn a_clone_whose_destination_is_taken_meanwhile_retires_no_other_instance() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-race-shared-");
                let root = TempDir::new("lore-instance-clone-race-root-");
                let parent = TempDir::new("lore-instance-clone-race-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let destination = parent.join("clone");
                let target = lore_revision::util::path::clean(
                    parent
                        .canonicalize()
                        .expect("resolve the parent")
                        .join("clone")
                        .to_string_lossy()
                        .into_owned(),
                );
                let other = InstanceId::generate();
                let provider: Arc<dyn FilesystemProvider> = Arc::new(RacingProvider {
                    repository: repository.clone(),
                    other,
                    target,
                    os: OsFilesystem::new(&root),
                });
                let plan = instance::instance_clone_plan_with(
                    provider,
                    repository.clone(),
                    &destination.display().to_string(),
                    true,
                )
                .await
                .expect("plan the clone")
                .expect("not a dry run");

                let result = instance::instance_clone(repository.clone(), &plan).await;
                instance::instance_clone_finish(plan).await;

                assert!(
                    result.as_ref().is_err_and(|err| err.is_invalid_arguments()),
                    "the move finds the destination taken: {:?}",
                    result.err()
                );
                assert!(
                    instance_ids(&list_instances(&repository).await.expect("list failed"))
                        .contains(&other),
                    "the instance that took the destination stays registered"
                );
            }))
            .await
            .expect("Test failed");
    }

    /// A call whose global data directory is `global`.
    fn execution_in(global: &Path) -> Arc<lore_revision::interface::ExecutionContext> {
        Arc::new(
            lore_revision::interface::ExecutionContext::new_client_with_user_id(
                Default::default(),
                lore_revision::relay::EventDispatcher::no_dispatch(),
                "test-user".to_string(),
            )
            .with_environment(lore_base::env::CallEnvironment {
                values: [Some(global.display().to_string()), None, None],
            }),
        )
    }

    /// A destination that a live instance is registered at, as an SWFS instance outside its
    /// service is at a path that does not exist, is refused, and that instance stays registered.
    #[tokio::test]
    async fn a_clone_to_where_a_live_instance_is_registered_is_refused() {
        let global = TempDir::new("lore-instance-clone-live-global-");
        let execution = execution_in(&global);

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-live-shared-");
                let root = TempDir::new("lore-instance-clone-live-root-");
                let parent = TempDir::new("lore-instance-clone-live-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let destination = parent.join("clone");
                let target = lore_revision::util::path::clean(
                    parent
                        .canonicalize()
                        .expect("resolve the parent")
                        .join("clone")
                        .to_string_lossy()
                        .into_owned(),
                );
                let other = InstanceId::generate();
                let external =
                    lore_revision::global::external_dir::external_lore_dir(other).expect("dir");
                std::fs::create_dir_all(&external).expect("make the external directory");
                std::fs::write(external.join(CONFIG), b"[vfs]\nvfs_type = \"Swfs\"\n")
                    .expect("write the external config");
                instance::store_instance_registration(&repository, other, &target)
                    .await
                    .expect("register the other instance");
                assert_eq!(
                    instance_staleness(&target, other).await,
                    InstanceStaleness::Active
                );

                let result = clone_instance(&repository, &destination, true).await;

                assert!(
                    result.as_ref().is_err_and(|err| err.is_invalid_arguments()),
                    "{:?}",
                    result.err()
                );
                assert!(
                    instance_ids(&list_instances(&repository).await.expect("list failed"))
                        .contains(&other)
                );
                assert_eq!(entries_besides(&parent, &[]), Vec::<String>::new());
            }))
            .await
            .expect("Test failed");
    }

    /// A directory a removal renamed aside to `….removing` and did not finish is removed by the
    /// next clone to the same destination, whatever is left in it.
    #[tokio::test]
    async fn a_removal_cut_short_is_finished_by_the_next_clone() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-removing-shared-");
                let root = TempDir::new("lore-instance-clone-removing-root-");
                let parent = TempDir::new("lore-instance-clone-removing-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let destination = parent.join("clone");
                let removing =
                    parent.join(format!("clone.cloning-{}.removing", InstanceId::generate()));
                std::fs::create_dir_all(removing.join("partial")).expect("make the leftover");
                test_file_write(&removing.join("partial/file.txt"), b"partial");

                clone_instance(&repository, &destination, true)
                    .await
                    .expect("instance_clone failed");

                assert!(!removing.exists());
                assert_eq!(
                    entries_besides(&parent, &[&destination]),
                    Vec::<String>::new()
                );
            }))
            .await
            .expect("Test failed");
    }

    /// A provider that clones through the OS provider and then gives the source's Lore directory
    /// another instance's ID, as a move or delete of the source during the copy leaves it.
    struct ReplacingProvider {
        os: OsFilesystem,
    }

    #[async_trait::async_trait]
    impl FilesystemProvider for ReplacingProvider {
        async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
            FilesystemProvider::begin_operation(&self.os).await
        }

        async fn clone_answer(
            &self,
            source: &Path,
            dot_path: &Path,
            destination: &Path,
        ) -> Result<CloneAnswer, InstanceCloneError> {
            self.os.clone_answer(source, dot_path, destination).await
        }

        async fn clone_instance(
            &self,
            source: &Path,
            dot_path: &Path,
            destination: &Path,
            answer: &CloneAnswer,
            copy_unclonable: bool,
            run: &CloneRun,
        ) -> Result<lore_io::clone::CloneStats, InstanceCloneError> {
            let stats = self
                .os
                .clone_instance(source, dot_path, destination, answer, copy_unclonable, run)
                .await;
            InstanceId::generate()
                .write_to_file(dot_path.join(INSTANCE))
                .await
                .expect("replace the source");
            stats
        }
    }

    /// A source moved or deleted while it is copied fails the clone as not found, leaving no
    /// instance at the destination and no keys for the clone.
    #[tokio::test]
    async fn a_source_moved_during_its_clone_fails_the_clone() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-moved-shared-");
                let root = TempDir::new("lore-instance-clone-moved-root-");
                let parent = TempDir::new("lore-instance-clone-moved-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let destination = parent.join("clone");
                let provider: Arc<dyn FilesystemProvider> = Arc::new(ReplacingProvider {
                    os: OsFilesystem::new(&root),
                });
                let plan = instance::instance_clone_plan_with(
                    provider,
                    repository.clone(),
                    &destination.display().to_string(),
                    true,
                )
                .await
                .expect("plan the clone")
                .expect("not a dry run");

                let result = instance::instance_clone(repository.clone(), &plan).await;
                instance::instance_clone_finish(plan).await;

                assert!(
                    result.as_ref().is_err_and(|err| err.is_file_not_found()),
                    "{:?}",
                    result.err()
                );
                assert!(!destination.exists());
                assert_eq!(entries_besides(&parent, &[]), Vec::<String>::new());
                assert_eq!(
                    instance_ids(&list_instances(&repository).await.expect("list failed")),
                    vec![repository.instance_id]
                );
            }))
            .await
            .expect("Test failed");
    }

    /// A clone run on an instance other than the one its plan was made for, as after the source
    /// was moved away and another instance moved to its path between the two, fails as not
    /// found and copies nothing.
    #[tokio::test]
    async fn a_clone_of_an_instance_other_than_the_planned_one_fails() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-other-shared-");
                let roots = TempDir::new("lore-instance-clone-other-roots-");
                let parent = TempDir::new("lore-instance-clone-other-parent-");
                let (path, other_path) = (roots.join("source"), roots.join("other"));
                std::fs::create_dir_all(&path).expect("create the source");
                std::fs::create_dir_all(&other_path).expect("create the other instance");
                let planned = shared_store_checkout(&path, &shared).await;
                let other = shared_store_checkout(&other_path, &shared).await;
                let other_id = other.instance_id;
                let destination = parent.join("clone");
                let plan = instance::instance_clone_plan_with(
                    Arc::new(OsFilesystem::new(&path)),
                    planned.clone(),
                    &destination.display().to_string(),
                    true,
                )
                .await
                .expect("plan the clone")
                .expect("not a dry run");
                drop((planned, other));
                std::fs::rename(&path, roots.join("moved")).expect("move the source away");
                std::fs::rename(&other_path, &path).expect("move the other instance in");
                let write_token = RepositoryWriteToken::acquire(&path).await;
                let there = repository::load_and_connect_exclusive(&path, write_token)
                    .await
                    .expect("open the instance at the source's path");
                assert_eq!(there.instance_id, other_id);

                let result = instance::instance_clone(there.clone(), &plan).await;
                instance::instance_clone_finish(plan).await;

                assert!(
                    result.as_ref().is_err_and(|err| err.is_file_not_found()),
                    "{:?}",
                    result.err()
                );
                assert!(!destination.exists());
                assert_eq!(entries_besides(&parent, &[]), Vec::<String>::new());
            }))
            .await
            .expect("Test failed");
    }

    /// A clone whose registration fails after it is at its destination leaves it there, with its
    /// own ID and the source's anchors, and the next write command opened in it registers it.
    #[tokio::test]
    async fn a_clone_whose_registration_fails_registers_itself_when_next_opened() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-unregistered-shared-");
                let root = TempDir::new("lore-instance-clone-unregistered-root-");
                let parent = TempDir::new("lore-instance-clone-unregistered-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let destination = parent.join("clone");
                let plan = instance::instance_clone_plan(
                    repository.clone(),
                    &destination.display().to_string(),
                    true,
                )
                .await
                .expect("plan the clone")
                .expect("not a dry run");

                let result = instance::instance_clone_with(
                    repository.clone(),
                    &plan,
                    async |from: &Path, to: &Path| std::fs::rename(from, to),
                    async |_: &Arc<RepositoryContext>, _: InstanceId, _: &str| {
                        Err(instance::InstanceError::internal(
                            "the registry is unavailable",
                        ))
                    },
                )
                .await;
                instance::instance_clone_finish(plan).await;

                assert!(result.is_err());
                let clone = InstanceId::read_from_file(destination.join(DOT_LORE).join(INSTANCE))
                    .expect("the clone is at its destination");
                assert!(
                    !destination
                        .join(DOT_LORE)
                        .join(instance::CLONE_IN_PROGRESS_MARK)
                        .exists()
                );
                assert_eq!(
                    anchors_of(&repository, clone).await,
                    anchors_of(&repository, repository.instance_id).await
                );
                assert!(
                    !instance_ids(&list_instances(&repository).await.expect("list failed"))
                        .contains(&clone)
                );

                drop(repository);
                let token = RepositoryWriteToken::acquire(&destination).await;
                let opened = repository::load_and_connect_with_token(
                    &destination,
                    RepositoryAccess::ReadWrite,
                    Some(token),
                )
                .await
                .expect("open the clone");
                assert!(
                    instance_ids(&list_instances(&opened).await.expect("list failed"))
                        .contains(&clone),
                    "opening the clone registers it"
                );
            }))
            .await
            .expect("Test failed");
    }

    /// A provider whose copy runs on a thread of its own until `release` is set, keeping what
    /// the clone hands it to hold, as the OS provider's blocking copy does, while the future that
    /// started it never completes. `started` is set once the copy has its hold.
    struct DetachedProvider {
        os: OsFilesystem,
        release: Arc<std::sync::atomic::AtomicBool>,
        started: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl FilesystemProvider for DetachedProvider {
        async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
            FilesystemProvider::begin_operation(&self.os).await
        }

        async fn clone_answer(
            &self,
            source: &Path,
            dot_path: &Path,
            destination: &Path,
        ) -> Result<CloneAnswer, InstanceCloneError> {
            self.os.clone_answer(source, dot_path, destination).await
        }

        async fn clone_instance(
            &self,
            _source: &Path,
            _dot_path: &Path,
            _destination: &Path,
            _answer: &CloneAnswer,
            _copy_unclonable: bool,
            run: &CloneRun,
        ) -> Result<lore_io::clone::CloneStats, InstanceCloneError> {
            let hold = Arc::clone(&run.hold);
            let release = Arc::clone(&self.release);
            self.started
                .store(true, std::sync::atomic::Ordering::Relaxed);
            std::thread::spawn(move || {
                while !release.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                drop(hold);
            });
            futures::future::pending().await
        }
    }

    /// A clone dropped while its copy still runs keeps the source's repository lock until the
    /// copy ends, so a command on the source waits for it.
    #[tokio::test]
    async fn a_dropped_clone_keeps_the_lock_until_its_copy_ends() {
        // Offline, so the command's open reaches the lock without first dialing the instance's
        // remote, which nothing serves and which Windows takes seconds to refuse.
        let execution =
            test_execution_with(lore_revision::interface::LoreGlobalArgs::default().set_offline());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let shared = TempDir::new("lore-instance-clone-dropped-shared-");
                let root = TempDir::new("lore-instance-clone-dropped-root-");
                let parent = TempDir::new("lore-instance-clone-dropped-parent-");
                let repository = shared_store_checkout(&root, &shared).await;
                let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let provider: Arc<dyn FilesystemProvider> = Arc::new(DetachedProvider {
                    os: OsFilesystem::new(&root),
                    release: Arc::clone(&release),
                    started: Arc::clone(&started),
                });
                let plan = instance::instance_clone_plan_with(
                    provider,
                    repository.clone(),
                    &parent.join("clone").display().to_string(),
                    true,
                )
                .await
                .expect("plan the clone")
                .expect("not a dry run");

                // Dropped once its copy has started, however long the clone takes to reach it.
                let clone = instance::instance_clone(repository.clone(), &plan);
                let copying = async {
                    while !started.load(std::sync::atomic::Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                };
                tokio::select! {
                    _ = clone => panic!("the clone never ends while it copies"),
                    reached = tokio::time::timeout(Duration::from_secs(10), copying) => {
                        reached.expect("the copy starts");
                    }
                }
                drop(repository);

                // A command on the source reaches the gate of its repository lock and waits
                // there, without opening the instance, for as long as the copy runs.
                let dot = root.join(DOT_LORE);
                let gate = |dot: &Path| {
                    format!(
                        "{} waiting to share the lock, {} to hold it alone",
                        repository::shared_callers_waiting(dot),
                        repository::exclusive_callers_waiting(dot)
                    )
                };
                let mut opening = std::pin::pin!(repository::load_and_connect_with_token(
                    &root,
                    RepositoryAccess::ReadOnly,
                    None,
                ));
                let at_gate = async {
                    loop {
                        tokio::select! {
                            _ = &mut opening => panic!("a command on the source waits for the copy"),
                            () = tokio::time::sleep(Duration::from_millis(5)) => {}
                        }
                        if repository::shared_callers_waiting(&dot) == 1 {
                            return;
                        }
                    }
                };
                if tokio::time::timeout(Duration::from_secs(10), at_gate)
                    .await
                    .is_err()
                {
                    panic!("the command never reached the gate: {}", gate(&dot));
                }

                release.store(true, std::sync::atomic::Ordering::Relaxed);
                match tokio::time::timeout(Duration::from_secs(10), opening).await {
                    Ok(opened) => drop(opened.expect("open the source")),
                    Err(elapsed) => panic!(
                        "the lock is not released once the copy ends ({elapsed}): {}",
                        gate(&dot)
                    ),
                }
                instance::instance_clone_finish(plan).await;
            }))
            .await
            .expect("Test failed");
    }
}
