// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bytes::Bytes;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::lore::*;
    use lore_revision::repository::cache_in_memory_stores;
    use lore_revision::repository::create_client_memory_stores;
    use lore_revision::repository::get_cached_in_memory_stores;
    use lore_revision::repository::repository_release;
    use lore_storage::*;

    include!("helper.rs");

    #[tokio::test]
    async fn create_client_memory_stores_returns_functional_stores() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let (imm, mut_) = create_client_memory_stores()
                    .await
                    .expect("Failed to create in-memory stores");

                let repository = RepositoryId::from([1; 16]);
                let key = Hash::from(rand::random::<[u8; 32]>());
                let value = Hash::from(rand::random::<[u8; 32]>());
                mut_.clone()
                    .store(repository, key, value, KeyType::Untyped)
                    .await
                    .expect("Failed to store value");

                let loaded = mut_
                    .clone()
                    .load(repository, key, KeyType::Untyped)
                    .await
                    .expect("Failed to load value");
                assert_eq!(loaded, value, "Mutable store should round-trip data");

                // Immutable store: put a fragment with payload and get it back
                let address = Address {
                    context: repository.into(),
                    hash: Hash::from(rand::random::<[u8; 32]>()),
                };
                let payload = Bytes::from_static(b"immutable test payload");
                let fragment = Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };
                ImmutableStore::put(
                    imm.clone(),
                    repository,
                    address,
                    fragment,
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("Failed to put into immutable store");

                let (got_fragment, got_payload) =
                    ImmutableStore::get(imm.clone(), repository, address)
                        .await
                        .and_then(lore_storage::StoreGetData::into_payload)
                        .expect("Failed to get from immutable store");
                assert_eq!(
                    got_fragment.size_payload, fragment.size_payload,
                    "Fragment size_payload mismatch"
                );
                assert_eq!(
                    got_fragment.size_content, fragment.size_content,
                    "Fragment size_content mismatch"
                );
                assert_eq!(got_payload, payload, "Immutable store payload mismatch");
            })
            .await;
    }

    #[tokio::test]
    async fn repository_release_does_not_panic_on_uncached_path() {
        let path = PathBuf::from("/tmp/lore-test-release-uncached");
        repository_release(&path);
    }

    #[tokio::test]
    async fn release_clears_cached_in_memory_store_data() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let path = PathBuf::from("/tmp/lore-test-release-clears-data");
                let dot_path = path.join(".urc");
                let repository = RepositoryId::from([2; 16]);

                // Create stores, write data to both, and cache them
                let (imm, mut_) = create_client_memory_stores()
                    .await
                    .expect("Failed to create in-memory stores");

                // Write to mutable store
                let mutable_key = Hash::from(rand::random::<[u8; 32]>());
                let mutable_value = Hash::from(rand::random::<[u8; 32]>());
                mut_.clone()
                    .store(repository, mutable_key, mutable_value, KeyType::Untyped)
                    .await
                    .expect("Failed to store mutable value");

                // Write to immutable store
                let address = Address {
                    context: repository.into(),
                    hash: Hash::from(rand::random::<[u8; 32]>()),
                };
                let payload = Bytes::from_static(b"cached store test");
                let fragment = Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };
                ImmutableStore::put(
                    imm.clone(),
                    repository,
                    address,
                    fragment,
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("Failed to put immutable value");

                // Cache the stores
                cache_in_memory_stores(dot_path.clone(), imm, mut_);

                // Verify the cache is populated
                assert!(
                    get_cached_in_memory_stores(&dot_path).is_some(),
                    "Cache should contain stores"
                );

                // Release all references — drop locals already moved into cache
                repository_release(&path);

                // Cache should now be empty
                assert!(
                    get_cached_in_memory_stores(&dot_path).is_none(),
                    "Cache should be empty after release"
                );

                // Create new stores for the same path via cache
                let (new_imm, new_mut) = create_client_memory_stores()
                    .await
                    .expect("Failed to create new in-memory stores");
                cache_in_memory_stores(dot_path.clone(), new_imm.clone(), new_mut.clone());

                // Mutable store should not contain the previously stored value
                let load_result = new_mut
                    .clone()
                    .load(repository, mutable_key, KeyType::Untyped)
                    .await;
                assert!(
                    load_result.is_err(),
                    "New mutable store should not contain data from released store"
                );

                // Immutable store should not contain the previously stored fragment
                let get_result = ImmutableStore::get(new_imm, repository, address).await;
                assert!(
                    get_result.is_err(),
                    "New immutable store should not contain data from released store"
                );

                // Clean up
                repository_release(&path);
            })
            .await;
    }

    #[test]
    fn global_args_in_memory_flag() {
        let mut args = LoreGlobalArgs::default();
        assert!(!args.in_memory(), "Default should be false");

        args.in_memory = 1;
        assert!(args.in_memory(), "Should be true when set to 1");

        args.in_memory = 0;
        assert!(!args.in_memory(), "Should be false when set to 0");
    }
}
