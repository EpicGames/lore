// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::dependency::DEPENDENCIES_KEY;
    use lore_revision::dependency::DEPENDENCY_INLINE_THRESHOLD;
    use lore_revision::dependency::DEPENDENTS_KEY;
    use lore_revision::dependency::DependencyData;
    use lore_revision::metadata::Metadata;
    use lore_revision::metadata::MetadataType;

    include!("helper.rs");

    // =========================================================================
    // Phase 1: Serialization round-trip
    // =========================================================================

    #[test]
    fn round_trip_empty() {
        let data = DependencyData::new();
        let blob = data.serialize();
        let restored = DependencyData::deserialize(&blob).expect("deserialize empty");
        assert_eq!(data, restored);
        assert!(restored.is_empty());
    }

    #[test]
    fn round_trip_single_entry_no_tags() {
        let mut data = DependencyData::new();
        data.add(42, &[]);
        let blob = data.serialize();
        let restored = DependencyData::deserialize(&blob).expect("deserialize");
        assert_eq!(data, restored);
        assert!(restored.contains(42));
        assert_eq!(restored.get(42).unwrap().tags.len(), 0);
    }

    #[test]
    fn round_trip_single_entry_with_tags() {
        let mut data = DependencyData::new();
        data.add(10, &["build", "source"]);
        let blob = data.serialize();
        let restored = DependencyData::deserialize(&blob).expect("deserialize");
        assert_eq!(data, restored);
        assert!(restored.contains_tag(10, "build"));
        assert!(restored.contains_tag(10, "source"));
        assert!(!restored.contains_tag(10, "other"));
    }

    #[test]
    fn round_trip_multiple_entries() {
        let mut data = DependencyData::new();
        data.add(100, &["a"]);
        data.add(50, &["b", "c"]);
        data.add(200, &[]);
        data.add(1, &["x", "y", "z"]);

        let blob = data.serialize();
        let restored = DependencyData::deserialize(&blob).expect("deserialize");
        assert_eq!(data, restored);
        assert_eq!(restored.len(), 4);

        // Verify sorted order
        let nodes: Vec<u32> = restored.iter().map(|e| e.node).collect();
        assert_eq!(nodes, vec![1, 50, 100, 200]);
    }

    #[test]
    fn round_trip_odd_length_tags() {
        let mut data = DependencyData::new();
        // Odd-length tag to exercise padding
        data.add(1, &["abc"]);
        // Even-length tag
        data.add(2, &["ab"]);
        // Single char
        data.add(3, &["x"]);

        let blob = data.serialize();
        let restored = DependencyData::deserialize(&blob).expect("deserialize");
        assert_eq!(data, restored);
    }

    // =========================================================================
    // Phase 1: Add operations
    // =========================================================================

    #[test]
    fn add_to_empty() {
        let mut data = DependencyData::new();
        assert!(data.is_empty());

        data.add(5, &["tag1"]);
        assert!(!data.is_empty());
        assert_eq!(data.len(), 1);
        assert!(data.contains(5));
        assert!(data.contains_tag(5, "tag1"));
    }

    #[test]
    fn add_duplicate_node_merges_tags() {
        let mut data = DependencyData::new();
        data.add(10, &["build"]);
        data.add(10, &["test", "build"]); // "build" already exists

        assert_eq!(data.len(), 1);
        let entry = data.get(10).unwrap();
        assert_eq!(entry.tags.len(), 2);
        assert!(entry.tags.iter().any(|t| t.as_ref() == "build"));
        assert!(entry.tags.iter().any(|t| t.as_ref() == "test"));
    }

    #[test]
    fn add_preserves_sorted_order() {
        let mut data = DependencyData::new();
        data.add(100, &[]);
        data.add(1, &[]);
        data.add(50, &[]);
        data.add(25, &[]);
        data.add(200, &[]);

        let nodes: Vec<u32> = data.iter().map(|e| e.node).collect();
        assert_eq!(nodes, vec![1, 25, 50, 100, 200]);
    }

    #[test]
    fn add_with_no_tags() {
        let mut data = DependencyData::new();
        data.add(7, &[]);
        assert!(data.contains(7));
        assert_eq!(data.get(7).unwrap().tags.len(), 0);
    }

    #[test]
    fn add_deduplicates_tags_within_single_call() {
        let mut data = DependencyData::new();
        data.add(1, &["a", "b", "a", "c", "b"]);
        let entry = data.get(1).unwrap();
        assert_eq!(entry.tags.len(), 3);
        let tags: Vec<&str> = entry.tags.iter().map(|t| t.as_ref()).collect();
        assert_eq!(tags, vec!["a", "b", "c"]);
    }

    #[test]
    fn tags_are_lexicographically_sorted() {
        let mut data = DependencyData::new();
        data.add(1, &["zebra", "alpha", "middle"]);
        let entry = data.get(1).unwrap();
        let tags: Vec<&str> = entry.tags.iter().map(|t| t.as_ref()).collect();
        assert_eq!(tags, vec!["alpha", "middle", "zebra"]);
    }

    // =========================================================================
    // Phase 1: Remove operations
    // =========================================================================

    #[test]
    fn remove_entire_entry() {
        let mut data = DependencyData::new();
        data.add(10, &["a", "b"]);
        let removed = data.remove(10, &[]);
        assert!(removed);
        assert!(!data.contains(10));
        assert!(data.is_empty());
    }

    #[test]
    fn remove_specific_tags() {
        let mut data = DependencyData::new();
        data.add(10, &["a", "b", "c"]);
        let removed = data.remove(10, &["b"]);
        assert!(!removed); // entry still has tags

        let entry = data.get(10).unwrap();
        assert_eq!(entry.tags.len(), 2);
        assert!(!entry.tags.iter().any(|t| t.as_ref() == "b"));
        assert!(entry.tags.iter().any(|t| t.as_ref() == "a"));
        assert!(entry.tags.iter().any(|t| t.as_ref() == "c"));
    }

    #[test]
    fn remove_all_tags_removes_entry() {
        let mut data = DependencyData::new();
        data.add(10, &["a", "b"]);
        let removed = data.remove(10, &["a", "b"]);
        assert!(removed);
        assert!(!data.contains(10));
    }

    #[test]
    fn remove_nonexistent_entry() {
        let mut data = DependencyData::new();
        data.add(10, &["a"]);
        let removed = data.remove(99, &[]);
        assert!(!removed);
        assert_eq!(data.len(), 1);
    }

    #[test]
    fn remove_nonexistent_tag_no_effect() {
        let mut data = DependencyData::new();
        data.add(10, &["a", "b"]);
        let removed = data.remove(10, &["nonexistent"]);
        assert!(!removed);
        assert_eq!(data.get(10).unwrap().tags.len(), 2);
    }

    #[test]
    fn sorted_invariant_after_add_remove_sequence() {
        let mut data = DependencyData::new();
        data.add(50, &[]);
        data.add(10, &[]);
        data.add(30, &[]);
        data.add(70, &[]);
        data.remove(30, &[]);
        data.add(20, &[]);
        data.add(60, &[]);
        data.remove(10, &[]);

        let nodes: Vec<u32> = data.iter().map(|e| e.node).collect();
        assert_eq!(nodes, vec![20, 50, 60, 70]);
    }

    // =========================================================================
    // Phase 1: Deserialization error cases
    // =========================================================================

    #[test]
    fn deserialize_too_short() {
        assert!(DependencyData::deserialize(&[0u8; 4]).is_err());
    }

    #[test]
    fn deserialize_bad_magic() {
        let mut buf = vec![0u8; 16];
        buf[0..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        buf[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert!(DependencyData::deserialize(&buf).is_err());
    }

    #[test]
    fn deserialize_bad_version() {
        let mut buf = vec![0u8; 16];
        buf[0..4].copy_from_slice(&0x66646570u32.to_le_bytes());
        buf[4..8].copy_from_slice(&99u32.to_le_bytes()); // bad version
        buf[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert!(DependencyData::deserialize(&buf).is_err());
    }

    #[test]
    fn deserialize_truncated_entry() {
        // Valid header claiming 1 entry, but no entry data
        let mut buf = vec![0u8; 16];
        buf[0..4].copy_from_slice(&0x66646570u32.to_le_bytes());
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        buf[8..12].copy_from_slice(&1u32.to_le_bytes()); // 1 entry
        assert!(DependencyData::deserialize(&buf).is_err());
    }

    #[test]
    fn deserialize_truncated_tag() {
        let mut data = DependencyData::new();
        data.add(1, &["hello"]);
        let mut blob = data.serialize().to_vec();
        // Truncate the last byte of the tag data
        blob.truncate(blob.len() - 2);
        assert!(DependencyData::deserialize(&blob).is_err());
    }

    #[test]
    fn deserialize_invalid_utf8_tag() {
        // Build a valid header with 1 entry, 1 tag containing invalid UTF-8
        let mut buf = Vec::new();
        buf.extend_from_slice(&0x66646570u32.to_le_bytes()); // magic
        buf.extend_from_slice(&1u32.to_le_bytes()); // version
        buf.extend_from_slice(&1u32.to_le_bytes()); // entry_count
        buf.extend_from_slice(&0u32.to_le_bytes()); // reserved
        buf.extend_from_slice(&42u32.to_le_bytes()); // node_id
        buf.extend_from_slice(&1u16.to_le_bytes()); // tag_count
        buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
        buf.extend_from_slice(&2u16.to_le_bytes()); // tag_length = 2
        buf.push(0xFF); // invalid UTF-8
        buf.push(0xFE); // invalid UTF-8

        assert!(DependencyData::deserialize(&buf).is_err());
    }

    // =========================================================================
    // Phase 1: Serialized size
    // =========================================================================

    #[test]
    fn empty_serialize_produces_header_only() {
        let data = DependencyData::new();
        let blob = data.serialize();
        assert_eq!(blob.len(), 16); // header only
    }

    #[test]
    fn serialized_size_predictable() {
        let mut data = DependencyData::new();
        // Entry with no tags: header(16) + node_id(4) + tag_count(2) + reserved(2) = 24
        data.add(1, &[]);
        assert_eq!(data.serialize().len(), 24);

        // Add a 4-byte tag (even length, no padding):
        // 24 + tag_length(2) + "abcd"(4) = 30
        let mut data2 = DependencyData::new();
        data2.add(1, &["abcd"]);
        assert_eq!(data2.serialize().len(), 30);

        // Add a 3-byte tag (odd length, 1 byte padding):
        // 24 + tag_length(2) + "abc"(3) + padding(1) = 30
        let mut data3 = DependencyData::new();
        data3.add(1, &["abc"]);
        assert_eq!(data3.serialize().len(), 30);
    }

    #[test]
    fn threshold_boundary_sizes() {
        // Build a blob that is just at the threshold boundary.
        // Each entry with no tags = 8 bytes. Header = 16 bytes.
        // To reach exactly 8192 bytes: (8192 - 16) / 8 = 1022 entries.
        let mut data = DependencyData::new();
        for i in 0..1022u32 {
            data.add(i, &[]);
        }
        let blob = data.serialize();
        assert_eq!(blob.len(), DEPENDENCY_INLINE_THRESHOLD);

        // One more entry pushes it over
        data.add(1022, &[]);
        let blob_over = data.serialize();
        assert!(blob_over.len() > DEPENDENCY_INLINE_THRESHOLD);
    }

    // =========================================================================
    // Phase 1: contains / contains_tag / get
    // =========================================================================

    #[test]
    fn contains_nonexistent() {
        let data = DependencyData::new();
        assert!(!data.contains(0));
        assert!(!data.contains(999));
    }

    #[test]
    fn get_nonexistent() {
        let data = DependencyData::new();
        assert!(data.get(0).is_none());
    }

    #[test]
    fn contains_tag_nonexistent_node() {
        let data = DependencyData::new();
        assert!(!data.contains_tag(0, "tag"));
    }

    // =========================================================================
    // Phase 1: Metadata::get_typed
    // =========================================================================

    #[test]
    fn metadata_get_typed_returns_correct_types() {
        let _execution = setup_test_execution();

        let mut metadata = Metadata::new();

        metadata
            .set_string("mystring", "hello")
            .expect("set string");
        metadata
            .set_address(
                "myaddr",
                Address {
                    hash: Hash::from([1u8; 32]),
                    context: Context::from([2u8; 16]),
                },
            )
            .expect("set address");
        metadata
            .set_binary("mybin", &[0xAA, 0xBB, 0xCC])
            .expect("set binary");
        metadata.set_u64("mynum", 42).expect("set u64");
        metadata.set_bool("mybool", true).expect("set bool");

        let (val, typ) = metadata.get_typed("mystring").expect("get_typed string");
        assert_eq!(typ, MetadataType::String);
        assert_eq!(val, b"hello");

        let (val, typ) = metadata.get_typed("myaddr").expect("get_typed address");
        assert_eq!(typ, MetadataType::Address);
        assert_eq!(val.len(), 48);

        let (val, typ) = metadata.get_typed("mybin").expect("get_typed binary");
        assert_eq!(typ, MetadataType::Binary);
        assert_eq!(val, &[0xAA, 0xBB, 0xCC]);

        let (_, typ) = metadata.get_typed("mynum").expect("get_typed numeric");
        assert_eq!(typ, MetadataType::Numeric);

        let (_, typ) = metadata.get_typed("mybool").expect("get_typed boolean");
        assert_eq!(typ, MetadataType::Boolean);
    }

    #[test]
    fn metadata_get_typed_not_found() {
        let _execution = setup_test_execution();
        let metadata = Metadata::new();
        assert!(metadata.get_typed("nonexistent").is_err());
    }

    // =========================================================================
    // Phase 2: Metadata::remove_key
    // =========================================================================

    #[test]
    fn metadata_remove_key_existing() {
        let _execution = setup_test_execution();
        let mut metadata = Metadata::new();

        metadata.set_string("keep", "value1").expect("set keep");
        metadata
            .set_binary("remove_me", &[1, 2, 3])
            .expect("set remove_me");
        metadata.set_u64("also_keep", 42).expect("set also_keep");

        assert!(metadata.remove_key("remove_me"));

        // Removed key should not be found
        assert!(metadata.get_typed("remove_me").is_err());

        // Other keys should still be accessible
        let (val, _) = metadata.get_typed("keep").expect("keep should exist");
        assert_eq!(val, b"value1");
        let (val, _) = metadata
            .get_typed("also_keep")
            .expect("also_keep should exist");
        assert_eq!(u64::from_le_bytes(val.try_into().unwrap()), 42);
    }

    #[test]
    fn metadata_remove_key_nonexistent() {
        let _execution = setup_test_execution();
        let mut metadata = Metadata::new();
        metadata.set_string("exists", "value").expect("set");
        assert!(!metadata.remove_key("nonexistent"));

        // Existing key should be unaffected
        let (val, _) = metadata.get_typed("exists").expect("exists");
        assert_eq!(val, b"value");
    }

    #[test]
    fn metadata_remove_key_empty_metadata() {
        let _execution = setup_test_execution();
        let mut metadata = Metadata::new();
        assert!(!metadata.remove_key("anything"));
    }

    #[test]
    fn metadata_remove_key_last_key() {
        let _execution = setup_test_execution();
        let mut metadata = Metadata::new();
        metadata.set_string("only", "value").expect("set");
        assert!(metadata.remove_key("only"));
        assert!(metadata.get_typed("only").is_err());
    }

    #[test]
    fn metadata_remove_key_then_re_add() {
        let _execution = setup_test_execution();
        let mut metadata = Metadata::new();

        metadata.set_binary("dep", &[1, 2, 3]).expect("set binary");
        assert!(metadata.remove_key("dep"));
        assert!(metadata.get_typed("dep").is_err());

        // Re-add with different type and value
        metadata.set_string("dep", "new_value").expect("re-set");
        let (val, typ) = metadata.get_typed("dep").expect("re-get");
        assert_eq!(typ, MetadataType::String);
        assert_eq!(val, b"new_value");
    }

    // =========================================================================
    // Phase 2: load/store helpers with State
    // =========================================================================

    use std::sync::Arc;

    use lore_base::error::NoRemote;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_revision::dependency::load_dependency_data;
    use lore_revision::dependency::store_dependency_data;
    use lore_revision::node::Node;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::state::State;
    use lore_storage::hash::hash_string;
    use lore_storage::local::immutable_store::LocalImmutableStore;
    use lore_transport::ProtocolError;

    /// Create a fresh state with three file nodes for testing.
    async fn setup_test_state() -> (Arc<RepositoryContext>, Arc<State>, u32, u32, u32) {
        let (_, mutable_store, _) = test_store_create().await.expect("create stores");
        let repository_id = Context::from(uuid::Uuid::now_v7());
        let tempdir = generate_tempdir();
        let path = tempdir.to_path_buf();

        let immutable_store = LocalImmutableStore::new(
            None,
            lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
        )
        .await
        .expect("create immutable store");

        let repository = Arc::new(RepositoryContext::new(
            Some(path.clone()),
            immutable_store.clone(),
            mutable_store.clone(),
            repository_id.into(),
            lore_revision::instance::InstanceId::default(),
            Err(ProtocolError::from(NoRemote)),
            Arc::default(),
            RepositoryFormat::Lore,
        ));

        let state = Arc::new(State::new());

        // Add three file nodes
        let node_a = state
            .node_add(
                repository.clone(),
                ROOT_NODE,
                Node {
                    name_hash: hash_string("file_a"),
                    ..Default::default()
                },
                "file_a",
            )
            .await
            .expect("add node_a");
        let node_b = state
            .node_add(
                repository.clone(),
                ROOT_NODE,
                Node {
                    name_hash: hash_string("file_b"),
                    ..Default::default()
                },
                "file_b",
            )
            .await
            .expect("add node_b");
        let node_c = state
            .node_add(
                repository.clone(),
                ROOT_NODE,
                Node {
                    name_hash: hash_string("file_c"),
                    ..Default::default()
                },
                "file_c",
            )
            .await
            .expect("add node_c");

        (repository, state, node_a, node_b, node_c)
    }

    #[tokio::test]
    async fn load_empty_returns_empty_data() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let (repository, state, node_a, _, _) = setup_test_state().await;

                let data = load_dependency_data(repository, &state, node_a, DEPENDENCIES_KEY).await;
                let data = data.expect("load_dependency_data");
                assert!(data.is_empty());
            }))
            .await
            .expect("test task");
    }

    #[tokio::test]
    async fn store_and_load_inline() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let (repository, state, node_a, node_b, node_c) = setup_test_state().await;

                // Store a small dependency data (will be inline)
                let mut data = DependencyData::new();
                data.add(node_b, &["build"]);
                data.add(node_c, &["test"]);

                store_dependency_data(repository.clone(), &state, node_a, DEPENDENCIES_KEY, &data)
                    .await
                    .expect("store");

                // Load it back and verify
                let loaded =
                    load_dependency_data(repository.clone(), &state, node_a, DEPENDENCIES_KEY)
                        .await
                        .expect("load");
                assert_eq!(loaded.len(), 2);
                assert!(loaded.contains(node_b));
                assert!(loaded.contains(node_c));
                assert!(loaded.contains_tag(node_b, "build"));
                assert!(loaded.contains_tag(node_c, "test"));
            }))
            .await
            .expect("test task");
    }

    #[tokio::test]
    async fn store_and_load_indirect() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let (repository, state, node_a, _, _) = setup_test_state().await;

                // Build a large dependency data that exceeds the inline threshold
                let mut data = DependencyData::new();
                for i in 0..1100u32 {
                    data.add(i, &[]);
                }
                assert!(data.serialize().len() > DEPENDENCY_INLINE_THRESHOLD);

                store_dependency_data(repository.clone(), &state, node_a, DEPENDENCIES_KEY, &data)
                    .await
                    .expect("store indirect");

                // Load it back and verify
                let loaded =
                    load_dependency_data(repository.clone(), &state, node_a, DEPENDENCIES_KEY)
                        .await
                        .expect("load indirect");
                assert_eq!(loaded.len(), 1100);
                assert!(loaded.contains(0));
                assert!(loaded.contains(1099));
            }))
            .await
            .expect("test task");
    }

    #[tokio::test]
    async fn store_empty_removes_key() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let (repository, state, node_a, node_b, _) = setup_test_state().await;

                // Store some dependencies
                let mut data = DependencyData::new();
                data.add(node_b, &["tag"]);
                store_dependency_data(repository.clone(), &state, node_a, DEPENDENCIES_KEY, &data)
                    .await
                    .expect("store");

                // Verify it's stored
                let loaded =
                    load_dependency_data(repository.clone(), &state, node_a, DEPENDENCIES_KEY)
                        .await
                        .expect("load");
                assert_eq!(loaded.len(), 1);

                // Store empty data to remove the key
                let empty = DependencyData::new();
                store_dependency_data(repository.clone(), &state, node_a, DEPENDENCIES_KEY, &empty)
                    .await
                    .expect("store empty");

                // Should load as empty
                let loaded =
                    load_dependency_data(repository.clone(), &state, node_a, DEPENDENCIES_KEY)
                        .await
                        .expect("load after remove");
                assert!(loaded.is_empty());
            }))
            .await
            .expect("test task");
    }

    #[tokio::test]
    async fn store_forward_and_backward_independently() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let (repository, state, node_a, node_b, _) = setup_test_state().await;

                // Store forward dep: A depends on B
                let mut forward = DependencyData::new();
                forward.add(node_b, &["build"]);
                store_dependency_data(
                    repository.clone(),
                    &state,
                    node_a,
                    DEPENDENCIES_KEY,
                    &forward,
                )
                .await
                .expect("store forward");

                // Store back-ref: B is depended on by A
                let mut backward = DependencyData::new();
                backward.add(node_a, &["build"]);
                store_dependency_data(
                    repository.clone(),
                    &state,
                    node_b,
                    DEPENDENTS_KEY,
                    &backward,
                )
                .await
                .expect("store backward");

                // Verify both independently
                let fwd =
                    load_dependency_data(repository.clone(), &state, node_a, DEPENDENCIES_KEY)
                        .await
                        .expect("load forward");
                assert!(fwd.contains_tag(node_b, "build"));

                let bwd = load_dependency_data(repository.clone(), &state, node_b, DEPENDENTS_KEY)
                    .await
                    .expect("load backward");
                assert!(bwd.contains_tag(node_a, "build"));

                // Forward deps key on B should be empty
                let fwd_b =
                    load_dependency_data(repository.clone(), &state, node_b, DEPENDENCIES_KEY)
                        .await
                        .expect("load B forward");
                assert!(fwd_b.is_empty());
            }))
            .await
            .expect("test task");
    }

    #[tokio::test]
    async fn inline_to_indirect_transition() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let (repository, state, node_a, _, _) = setup_test_state().await;

                // Start with a small (inline) dependency set
                let mut data = DependencyData::new();
                for i in 0..10u32 {
                    data.add(i, &[]);
                }
                assert!(data.serialize().len() <= DEPENDENCY_INLINE_THRESHOLD);

                store_dependency_data(repository.clone(), &state, node_a, DEPENDENCIES_KEY, &data)
                    .await
                    .expect("store inline");

                // Grow to exceed threshold (indirect)
                for i in 10..1100u32 {
                    data.add(i, &[]);
                }
                assert!(data.serialize().len() > DEPENDENCY_INLINE_THRESHOLD);

                store_dependency_data(repository.clone(), &state, node_a, DEPENDENCIES_KEY, &data)
                    .await
                    .expect("store indirect");

                let loaded =
                    load_dependency_data(repository.clone(), &state, node_a, DEPENDENCIES_KEY)
                        .await
                        .expect("load after transition to indirect");
                assert_eq!(loaded.len(), 1100);

                // Shrink back below threshold (back to inline)
                let mut small_data = DependencyData::new();
                for i in 0..5u32 {
                    small_data.add(i, &[]);
                }
                assert!(small_data.serialize().len() <= DEPENDENCY_INLINE_THRESHOLD);

                store_dependency_data(
                    repository.clone(),
                    &state,
                    node_a,
                    DEPENDENCIES_KEY,
                    &small_data,
                )
                .await
                .expect("store back to inline");

                let loaded =
                    load_dependency_data(repository.clone(), &state, node_a, DEPENDENCIES_KEY)
                        .await
                        .expect("load after transition back to inline");
                assert_eq!(loaded.len(), 5);
            }))
            .await
            .expect("test task");
    }

    // =========================================================================
    // Phase 2: Cycle detection
    // =========================================================================

    use lore_revision::dependency::resolve::check_cycle;
    use lore_revision::dependency::resolve::resolve_dependency_file_set;
    use lore_revision::dependency::resolve::transitive_closure;

    #[tokio::test]
    async fn resolve_nonexistent_path_returns_file_not_found() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let (repository, state, _, _, _) = setup_test_state().await;

                let result = resolve_dependency_file_set(
                    repository,
                    state,
                    &["nonexistent/path"],
                    &[],
                    false,
                    0,
                )
                .await;

                assert!(result.is_err());
                assert!(
                    result.unwrap_err().is_file_not_found(),
                    "expected FileNotFound for a path absent from the state"
                );
            }))
            .await
            .expect("test task");
    }

    #[tokio::test]
    async fn cycle_detection_self_dependency() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let (repository, state, node_a, _, _) = setup_test_state().await;

                let result = check_cycle(
                    repository.clone(),
                    state.clone(),
                    node_a,
                    &[node_a],
                    DEPENDENCIES_KEY,
                )
                .await;
                assert!(result.is_err());
                assert!(result.unwrap_err().is_invalid_arguments());
            }))
            .await
            .expect("test task");
    }

    #[tokio::test]
    async fn cycle_detection_transitive() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let (repository, state, node_a, node_b, node_c) = setup_test_state().await;

                // A -> B
                let mut data_a = DependencyData::new();
                data_a.add(node_b, &[]);
                store_dependency_data(
                    repository.clone(),
                    &state,
                    node_a,
                    DEPENDENCIES_KEY,
                    &data_a,
                )
                .await
                .expect("store A->B");

                // B -> C
                let mut data_b = DependencyData::new();
                data_b.add(node_c, &[]);
                store_dependency_data(
                    repository.clone(),
                    &state,
                    node_b,
                    DEPENDENCIES_KEY,
                    &data_b,
                )
                .await
                .expect("store B->C");

                // Now check if C -> A would create a cycle
                let result = check_cycle(
                    repository.clone(),
                    state.clone(),
                    node_c,
                    &[node_a],
                    DEPENDENCIES_KEY,
                )
                .await;
                assert!(
                    result.as_ref().is_err_and(|e| e.is_invalid_arguments()),
                    "C -> A should form a cycle via A -> B -> C"
                );

                // But A -> C should not create a cycle (it's already reachable, not circular)
                // Actually A -> C doesn't create a cycle since C doesn't reach A
                let result = check_cycle(
                    repository.clone(),
                    state.clone(),
                    node_a,
                    &[node_c],
                    DEPENDENCIES_KEY,
                )
                .await;
                assert!(result.is_ok(), "A -> C should not form a cycle");
            }))
            .await
            .expect("test task");
    }

    #[tokio::test]
    async fn transitive_closure_diamond() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let (repository, state, node_a, node_b, node_c) = setup_test_state().await;

                // Create a diamond: A -> B, A -> C, B -> D, C -> D
                // We'll use node IDs directly for D
                let node_d = state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        Node {
                            name_hash: hash_string("file_d"),
                            ..Default::default()
                        },
                        "file_d",
                    )
                    .await
                    .expect("add node_d");

                // A -> B, A -> C
                let mut data_a = DependencyData::new();
                data_a.add(node_b, &[]);
                data_a.add(node_c, &[]);
                store_dependency_data(
                    repository.clone(),
                    &state,
                    node_a,
                    DEPENDENCIES_KEY,
                    &data_a,
                )
                .await
                .expect("store A deps");

                // B -> D
                let mut data_b = DependencyData::new();
                data_b.add(node_d, &[]);
                store_dependency_data(
                    repository.clone(),
                    &state,
                    node_b,
                    DEPENDENCIES_KEY,
                    &data_b,
                )
                .await
                .expect("store B deps");

                // C -> D
                let mut data_c = DependencyData::new();
                data_c.add(node_d, &[]);
                store_dependency_data(
                    repository.clone(),
                    &state,
                    node_c,
                    DEPENDENCIES_KEY,
                    &data_c,
                )
                .await
                .expect("store C deps");

                // Transitive closure from A should include B, C, D
                let reachable = transitive_closure(
                    repository.clone(),
                    state.clone(),
                    &[node_a],
                    DEPENDENCIES_KEY,
                    &[],
                    0,
                    false,
                )
                .await
                .expect("transitive closure");

                assert!(reachable.contains(&node_b));
                assert!(reachable.contains(&node_c));
                assert!(reachable.contains(&node_d));
                assert!(!reachable.contains(&node_a)); // root not included unless it's a target
                assert_eq!(reachable.len(), 3);
            }))
            .await
            .expect("test task");
    }

    #[tokio::test]
    async fn transitive_closure_with_depth_limit() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let (repository, state, node_a, node_b, node_c) = setup_test_state().await;

                // Chain: A -> B -> C
                let mut data_a = DependencyData::new();
                data_a.add(node_b, &[]);
                store_dependency_data(
                    repository.clone(),
                    &state,
                    node_a,
                    DEPENDENCIES_KEY,
                    &data_a,
                )
                .await
                .expect("store A->B");

                let mut data_b = DependencyData::new();
                data_b.add(node_c, &[]);
                store_dependency_data(
                    repository.clone(),
                    &state,
                    node_b,
                    DEPENDENCIES_KEY,
                    &data_b,
                )
                .await
                .expect("store B->C");

                // Depth limit 1: only direct deps from A
                let reachable = transitive_closure(
                    repository.clone(),
                    state.clone(),
                    &[node_a],
                    DEPENDENCIES_KEY,
                    &[],
                    1,
                    false,
                )
                .await
                .expect("depth 1");
                assert!(reachable.contains(&node_b));
                assert!(!reachable.contains(&node_c));

                // Depth limit 2: includes transitive
                let reachable = transitive_closure(
                    repository.clone(),
                    state.clone(),
                    &[node_a],
                    DEPENDENCIES_KEY,
                    &[],
                    2,
                    false,
                )
                .await
                .expect("depth 2");
                assert!(reachable.contains(&node_b));
                assert!(reachable.contains(&node_c));
            }))
            .await
            .expect("test task");
    }

    #[tokio::test]
    async fn transitive_closure_with_tag_filter() {
        let execution = setup_test_execution();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let (repository, state, node_a, node_b, node_c) = setup_test_state().await;

                // A -> B (tag: build), A -> C (tag: test)
                let mut data_a = DependencyData::new();
                data_a.add(node_b, &["build"]);
                data_a.add(node_c, &["test"]);
                store_dependency_data(
                    repository.clone(),
                    &state,
                    node_a,
                    DEPENDENCIES_KEY,
                    &data_a,
                )
                .await
                .expect("store A deps");

                // Filter by "build" tag only
                let reachable = transitive_closure(
                    repository.clone(),
                    state.clone(),
                    &[node_a],
                    DEPENDENCIES_KEY,
                    &["build"],
                    0,
                    false,
                )
                .await
                .expect("filter by build");
                assert!(reachable.contains(&node_b));
                assert!(!reachable.contains(&node_c));

                // Filter by "test" tag only
                let reachable = transitive_closure(
                    repository.clone(),
                    state.clone(),
                    &[node_a],
                    DEPENDENCIES_KEY,
                    &["test"],
                    0,
                    false,
                )
                .await
                .expect("filter by test");
                assert!(!reachable.contains(&node_b));
                assert!(reachable.contains(&node_c));
            }))
            .await
            .expect("test task");
    }
}
