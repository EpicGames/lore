// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lore_base::error::NoRemote;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::ZeroHeapAlloc;
    use lore_revision::branch;
    use lore_revision::immutable::WriteToImmutable;
    use lore_revision::lore::RepositoryId;
    use lore_revision::node::BLOCK_NODE_COUNT;
    use lore_revision::node::INVALID_NODE;
    use lore_revision::node::NODE_NAME_LIMIT;
    use lore_revision::node::Node;
    use lore_revision::node::NodeBlock;
    use lore_revision::node::NodeBlockDataV0;
    use lore_revision::node::NodeBlockDataV2;
    use lore_revision::node::NodeBlockFlags;
    use lore_revision::node::NodeBlockFormat;
    use lore_revision::node::NodeFileMetadata;
    use lore_revision::node::NodeFlags;
    use lore_revision::node::NodeFlagsV2;
    use lore_revision::repository;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::state::State;
    use lore_storage::hash::hash_string;
    use lore_storage::options::WriteOptions;
    use lore_transport::ProtocolError;
    use rand::Rng;
    use rand::distr::Alphanumeric;

    include!("helper.rs");

    #[test]
    fn size() {
        assert_eq!(std::mem::size_of::<Node>(), 96, "Node size is invalid");
        assert_eq!(
            std::mem::size_of::<NodeFileMetadata>(),
            128,
            "Node file metadata size is invalid"
        );

        assert!(
            std::mem::size_of::<NodeBlock>() < lore_base::types::FRAGMENT_SIZE_THRESHOLD,
            "Node block size is invalid"
        );
        assert!(
            std::mem::size_of::<NodeFileMetadata>() < lore_base::types::FRAGMENT_SIZE_THRESHOLD,
            "Node file metadata block size is invalid"
        );
    }

    #[tokio::test]
    async fn convert_version_v2_to_v1() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = rand::random::<RepositoryId>();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                std::fs::create_dir_all(path.as_path()).expect("Create directory failed");
                let write_token = repository::RepositoryWriteToken::acquire(path.as_path()).await;
                repository::create_local(
                    path.as_path(),
                    &write_token,
                    repository_id,
                    Context::from(uuid::Uuid::now_v7()),
                    branch::DEFAULT_DEFAULT_NAME.to_string(),
                    repository::RepositoryConfig::default(),
                    false,
                )
                .await
                .expect("Failed to create repository");

                let repository = Arc::new(RepositoryContext::new(
                    Some(path.clone()),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository_id,
                    lore_revision::instance::InstanceId::default(),
                    Err(ProtocolError::from(NoRemote)),
                    Arc::default(),
                    RepositoryFormat::Lore,
                ));

                let mut block_v2 = NodeBlockDataV2::new_from_heap_zeroed();

                block_v2.flags = NodeBlockFlags::Dirty.as_u32();
                block_v2.version = NodeBlockFormat::NoTimestamp as u32;
                block_v2.node_count = 4;

                block_v2.node[0].child = 1;

                block_v2.node[1].flags =
                    (NodeFlagsV2::File | NodeFlagsV2::StagedMergeMine).as_u32();
                block_v2.node[1].name_offset = 1;
                block_v2.node[1].name_length = 2;
                block_v2.node[1].address = rand::random::<Address>();
                block_v2.node[1].size = 123;
                block_v2.node[1].name_hash = rand::random();
                block_v2.node[1].parent = 0;
                block_v2.node[1].sibling = 2;

                block_v2.node[2].flags = NodeFlagsV2::NoFlags.as_u32();
                block_v2.node[2].name_offset = 2;
                block_v2.node[2].name_length = 3;
                block_v2.node[2].address = rand::random::<Address>();
                block_v2.node[2].size = 234;
                block_v2.node[2].name_hash = rand::random();
                block_v2.node[2].parent = 0;
                block_v2.node[2].sibling = 0;
                block_v2.node[2].child = 3;

                block_v2.node[3].flags = (NodeFlagsV2::File | NodeFlagsV2::Executable).as_u32();
                block_v2.node[3].name_offset = 3;
                block_v2.node[3].name_length = 4;
                block_v2.node[3].address = rand::random::<Address>();
                block_v2.node[3].size = 345;
                block_v2.node[3].name_hash = rand::random();
                block_v2.node[3].parent = 2;
                block_v2.node[3].sibling = 0;
                block_v2.node[3].child = 0;

                let block = NodeBlock::convert_block_v2(repository.clone(), block_v2.clone())
                    .expect("Failed to convert block");

                assert_eq!(block.node[0].child, 1, "Conversion failure");

                assert_eq!(
                    block.node[1].flags as u32,
                    (NodeFlags::File | NodeFlags::StagedMergeMine).as_u32(),
                    "Conversion failure"
                );

                assert_eq!(block.node[2].flags as u32, 0, "Conversion failure");

                assert_eq!(block.node[2].flags as u32, 0, "Conversion failure");

                for i in 1..3 {
                    assert_eq!(
                        block.node[i].child, block_v2.node[i].child,
                        "Conversion failure"
                    );
                    assert_eq!(
                        block.node[i].name_offset, block_v2.node[i].name_offset,
                        "Conversion failure"
                    );
                    assert_eq!(
                        block.node[i].name_length, block_v2.node[i].name_length,
                        "Conversion failure"
                    );
                    assert_eq!(
                        block.node[i].address, block_v2.node[i].address,
                        "Conversion failure"
                    );
                    assert_eq!(
                        block.node[i].size, block_v2.node[i].size,
                        "Conversion failure"
                    );
                    assert_eq!(
                        block.node[i].name_hash, block_v2.node[i].name_hash,
                        "Conversion failure"
                    );
                    assert_eq!(
                        block.node[i].parent, block_v2.node[i].parent,
                        "Conversion failure"
                    );
                    assert_eq!(
                        block.node[i].sibling, block_v2.node[i].sibling,
                        "Conversion failure"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn add_delete() {
        let (_immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let block = NodeBlock::new_zeroed();

                let mut rng = rand::rng();
                for pass in 0..1000 {
                    let to_add = rng.random_range(1..BLOCK_NODE_COUNT);
                    for _ in 0..to_add {
                        let mut block_writer = block.write();
                        let node_index = block_writer.grab_node_unused(0);
                        if node_index == INVALID_NODE {
                            continue;
                        }
                        let (name_offset, name_length) = {
                            let node = block_writer.node(node_index as usize);
                            (node.name_offset, node.name_length)
                        };
                        let random_string: String = rand::rng()
                            .sample_iter(&Alphanumeric)
                            .take(rng.random_range(1..16))
                            .map(char::from)
                            .collect();
                        let name = format!("{node_index}_{pass}_{random_string}");
                        let (name_offset, name_length) = block_writer
                            .node_name_store(name.as_str(), name_offset, name_length)
                            .expect("node_name_store should succeed in test");

                        let node = block_writer.node(node_index as usize);
                        node.name_offset = name_offset;
                        node.name_length = name_length;
                    }

                    let mut to_delete = rng.random_range(0..block.read().node_count());
                    to_delete = to_delete.saturating_sub(1);
                    if to_delete > 0 {
                        for _ in 0..to_delete {
                            let mut block_writer = block.write();
                            let node_index = rng.random_range(0..block_writer.node_count());
                            let node = block_writer.node(node_index);
                            if node.flags & NodeFlags::Discarded != 0 {
                                continue;
                            }
                            block_writer.discard_node(0, node_index);
                        }
                    }
                    block.node_name_repack();

                    // Loop all nodes and ensure they are correctly named
                    let node_count = block.read().node_count();
                    if node_count > 0 {
                        for node_index in 0..node_count {
                            if block.node(node_index).flags & NodeFlags::Discarded != 0 {
                                continue;
                            }
                            let node_name =
                                block.node_name_ref(node_index).expect("Invalid node name");
                            let (indexstr, _) = node_name
                                .split_once('_')
                                .expect("Node name had unexpected format");
                            assert_eq!(indexstr, format!("{node_index}").as_str());
                        }
                    }
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// Helper to set an inline name on a `NodeV0`.
    fn v0_set_inline_name(node: &mut lore_revision::node::NodeV0, name: &str) {
        assert!(
            name.len() <= NODE_NAME_LIMIT,
            "Name too long for inline V0 storage"
        );
        node.name_string[..name.len()].copy_from_slice(name.as_bytes());
        node.name_string[NODE_NAME_LIMIT] = name.len() as u8;
        node.name_hash = hash_string(name);
    }

    #[tokio::test]
    async fn roundtrip_serialize_node_block_data_v0() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = rand::random::<RepositoryId>();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                std::fs::create_dir_all(path.as_path()).expect("Create directory failed");
                let write_token = repository::RepositoryWriteToken::acquire(path.as_path()).await;
                repository::create_local(
                    path.as_path(),
                    &write_token,
                    repository_id,
                    Context::from(uuid::Uuid::now_v7()),
                    branch::DEFAULT_DEFAULT_NAME.to_string(),
                    repository::RepositoryConfig::default(),
                    false,
                )
                .await
                .expect("Failed to create repository");

                let repository = Arc::new(RepositoryContext::new(
                    Some(path.clone()),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository_id,
                    lore_revision::instance::InstanceId::default(),
                    Err(ProtocolError::from(NoRemote)),
                    Arc::default(),
                    RepositoryFormat::Lore,
                ));

                let mut block_v0 = NodeBlockDataV0::new_from_heap_zeroed();
                block_v0.node_count = 4;

                // Node 0: root directory with child pointing to node 1
                block_v0.node[0].child_mtime_node = 1;
                v0_set_inline_name(&mut block_v0.node[0], "root");

                // Node 1: file node
                block_v0.node[1].flags = NodeFlags::File.bits();
                block_v0.node[1].parent = 0;
                block_v0.node[1].sibling = 2;
                block_v0.node[1].size = 1024;
                block_v0.node[1].address = rand::random::<Address>();
                v0_set_inline_name(&mut block_v0.node[1], "hello.txt");

                // Node 2: subdirectory with a child
                block_v0.node[2].parent = 0;
                block_v0.node[2].sibling = 0;
                block_v0.node[2].child_mtime_node = 3;
                block_v0.node[2].size = 0;
                block_v0.node[2].address = rand::random::<Address>();
                v0_set_inline_name(&mut block_v0.node[2], "subdir");

                // Node 3: file inside subdirectory
                block_v0.node[3].flags = NodeFlags::File.bits();
                block_v0.node[3].parent = 2;
                block_v0.node[3].sibling = 0;
                block_v0.node[3].size = 2048;
                block_v0.node[3].address = rand::random::<Address>();
                v0_set_inline_name(&mut block_v0.node[3], "nested_file.rs");

                // Serialize V0 block to the immutable store
                let (address, _fragment) = block_v0
                    .write_to_immutable(
                        repository.clone(),
                        Context::default(),
                        WriteOptions::default(),
                    )
                    .await
                    .expect("Failed to write V0 block to immutable store");

                // Deserialize it back via NodeBlock::deserialize which should detect V0 and convert
                let state = State::new();
                let deserialized = NodeBlock::deserialize(repository.clone(), &state, address)
                    .await
                    .expect("Failed to deserialize V0 block");

                // Verify node count
                assert_eq!(deserialized.read().node_count(), 4);

                // Verify node 0 (root directory)
                let node0 = deserialized.node(0);
                assert_eq!(node0.child, 1);

                // Verify node 1 (file)
                let node1 = deserialized.node(1);
                assert_eq!(node1.flags & NodeFlags::File, NodeFlags::File.bits());
                assert_eq!(node1.parent, 0);
                assert_eq!(node1.sibling, 2);
                assert_eq!(node1.size, 1024);
                assert_eq!(node1.address, block_v0.node[1].address);
                let name1 = deserialized
                    .node_name_clone(1)
                    .expect("Failed to get node 1 name");
                assert_eq!(name1, "hello.txt");

                // Verify node 2 (subdirectory)
                let node2 = deserialized.node(2);
                assert_eq!(node2.flags & NodeFlags::File, 0);
                assert_eq!(node2.child, 3);
                assert_eq!(node2.parent, 0);
                assert_eq!(node2.sibling, 0);
                assert_eq!(node2.address, block_v0.node[2].address);
                let name2 = deserialized
                    .node_name_clone(2)
                    .expect("Failed to get node 2 name");
                assert_eq!(name2, "subdir");

                // Verify node 3 (nested file)
                let node3 = deserialized.node(3);
                assert_eq!(node3.flags & NodeFlags::File, NodeFlags::File.bits());
                assert_eq!(node3.parent, 2);
                assert_eq!(node3.size, 2048);
                assert_eq!(node3.address, block_v0.node[3].address);
                let name3 = deserialized
                    .node_name_clone(3)
                    .expect("Failed to get node 3 name");
                assert_eq!(name3, "nested_file.rs");

                // Verify the deserialized block has V1 (Nametable) format
                let reader = deserialized.read();
                assert_eq!(
                    reader.node_block().version,
                    NodeBlockFormat::Nametable as u32
                );
                // Dirty flag should be cleared after deserialization
                assert_eq!(
                    reader.node_block().flags & NodeBlockFlags::Dirty.as_u32(),
                    0
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Names no writer may store. `node_name_store` is the enforcement point, so
    /// none of these can reach a name table through the API at all.
    const PATH_TRAVERSAL_NAMES: [&str; 9] = [
        "..",
        "../something",
        "..\\something",
        "/etc/passwd",
        "\0malicious",
        "foo\\bar",
        "a/b",
        "a/",
        "a/\\..",
    ];

    #[test]
    fn node_name_store_rejects_path_traversal() {
        for name in &PATH_TRAVERSAL_NAMES {
            let block = NodeBlock::new_zeroed();
            let err = block
                .write()
                .node_name_store(name, 0, 0)
                .expect_err("node_name_store should reject name");
            assert!(
                err.is_invalid_arguments(),
                "expected invalid arguments for {name:?}, got: {err:?}"
            );
        }
    }

    #[test]
    fn node_name_read_rejects_path_traversal_planted_in_the_name_table() {
        use bytes::BytesMut;
        use lore_revision::node::NodeBlockData;

        // A name table can arrive from a fragment written before the store-side
        // check existed, so the read path validates independently of it.
        for name in &PATH_TRAVERSAL_NAMES {
            let mut data = NodeBlockData::new_from_heap_zeroed();
            data.node_count = 1;
            data.node[0].name_offset = 0;
            data.node[0].name_length = name.len() as u32;
            let block = NodeBlock::new_with_name(data, BytesMut::from(name.as_bytes()));

            assert!(
                block.node_name_clone(0).is_err(),
                "node_name_clone should reject name: {name:?}"
            );
            assert!(
                block.node_name_ref(0).is_err(),
                "node_name_ref should reject name: {name:?}"
            );
        }
    }

    #[test]
    fn node_name_accepts_valid_names() {
        let valid_names = ["valid_file.txt", ".hidden", "...", ".", "a", "hello world"];

        for name in &valid_names {
            let block = NodeBlock::new_zeroed();
            let node_index = {
                let mut writer = block.write();
                let idx = writer.grab_node_unused(0) as usize;
                let (offset, length) = writer
                    .node_name_store(name, 0, 0)
                    .expect("node_name_store should succeed in test");
                let node = writer.node(idx);
                node.name_offset = offset;
                node.name_length = length;
                idx
            };
            let cloned = block
                .node_name_clone(node_index)
                .unwrap_or_else(|_| panic!("node_name_clone should accept name: {name:?}"));
            assert_eq!(cloned, *name);

            let locked = block
                .node_name_ref(node_index)
                .unwrap_or_else(|_| panic!("node_name_ref should accept name: {name:?}"));
            assert_eq!(&*locked, *name);
        }
    }

    #[test]
    fn node_name_rejects_out_of_bounds() {
        let block = NodeBlock::new_zeroed();
        let node_index = {
            let mut writer = block.write();
            let idx = writer.grab_node_unused(0) as usize;
            let node = writer.node(idx);
            node.name_offset = 9999;
            node.name_length = 100;
            idx
        };
        assert!(
            block.node_name_clone(node_index).is_err(),
            "node_name_clone should reject out-of-bounds name"
        );
        assert!(
            block.node_name_ref(node_index).is_err(),
            "node_name_ref should reject out-of-bounds name"
        );
    }

    #[tokio::test]
    async fn roundtrip_serialize_node_block_data_v2() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = rand::random::<RepositoryId>();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                std::fs::create_dir_all(path.as_path()).expect("Create directory failed");
                let write_token = repository::RepositoryWriteToken::acquire(path.as_path()).await;
                repository::create_local(
                    path.as_path(),
                    &write_token,
                    repository_id,
                    Context::from(uuid::Uuid::now_v7()),
                    branch::DEFAULT_DEFAULT_NAME.to_string(),
                    repository::RepositoryConfig::default(),
                    false,
                )
                .await
                .expect("Failed to create repository");

                let repository = Arc::new(RepositoryContext::new(
                    Some(path.clone()),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository_id,
                    lore_revision::instance::InstanceId::default(),
                    Err(ProtocolError::from(NoRemote)),
                    Arc::default(),
                    RepositoryFormat::Lore,
                ));

                let mut block_v2 = NodeBlockDataV2::new_from_heap_zeroed();
                block_v2.flags = NodeBlockFlags::Dirty.as_u32();
                block_v2.version = NodeBlockFormat::NoTimestamp as u32;
                block_v2.node_count = 4;

                // Node 0: root directory
                block_v2.node[0].child = 1;
                block_v2.node[0].name_hash = hash_string("root");

                // Node 1: file with StagedMergeMine flag
                block_v2.node[1].flags =
                    (NodeFlagsV2::File | NodeFlagsV2::StagedMergeMine).as_u32();
                block_v2.node[1].name_offset = 10;
                block_v2.node[1].name_length = 5;
                block_v2.node[1].address = rand::random::<Address>();
                block_v2.node[1].size = 512;
                block_v2.node[1].name_hash = rand::random();
                block_v2.node[1].parent = 0;
                block_v2.node[1].sibling = 2;

                // Node 2: subdirectory
                block_v2.node[2].flags = NodeFlagsV2::NoFlags.as_u32();
                block_v2.node[2].name_offset = 20;
                block_v2.node[2].name_length = 6;
                block_v2.node[2].address = rand::random::<Address>();
                block_v2.node[2].size = 0;
                block_v2.node[2].name_hash = rand::random();
                block_v2.node[2].parent = 0;
                block_v2.node[2].sibling = 0;
                block_v2.node[2].child = 3;

                // Node 3: executable file
                block_v2.node[3].flags = (NodeFlagsV2::File | NodeFlagsV2::Executable).as_u32();
                block_v2.node[3].name_offset = 30;
                block_v2.node[3].name_length = 7;
                block_v2.node[3].address = rand::random::<Address>();
                block_v2.node[3].size = 4096;
                block_v2.node[3].name_hash = rand::random();
                block_v2.node[3].parent = 2;
                block_v2.node[3].sibling = 0;
                block_v2.node[3].child = 0;

                // Serialize V2 block to the immutable store
                let (address, _fragment) = block_v2
                    .write_to_immutable(
                        repository.clone(),
                        Context::default(),
                        WriteOptions::default(),
                    )
                    .await
                    .expect("Failed to write V2 block to immutable store");

                // Deserialize it back via NodeBlock::deserialize which should detect V2 and convert
                let state = State::new();
                let deserialized = NodeBlock::deserialize(repository.clone(), &state, address)
                    .await
                    .expect("Failed to deserialize V2 block");

                // Verify node count
                assert_eq!(deserialized.read().node_count(), 4);

                // Verify node 0 (root directory)
                let node0 = deserialized.node(0);
                assert_eq!(node0.child, 1);

                // Verify node 1 (file with staged flags)
                let node1 = deserialized.node(1);
                assert_eq!(
                    node1.flags & NodeFlags::File,
                    NodeFlags::File.bits(),
                    "File flag not preserved"
                );
                assert_eq!(
                    node1.flags as u32,
                    (NodeFlags::File | NodeFlags::StagedMergeMine).as_u32(),
                    "StagedMergeMine flag not converted correctly"
                );
                assert_eq!(node1.parent, block_v2.node[1].parent);
                assert_eq!(node1.sibling, block_v2.node[1].sibling);
                assert_eq!(node1.size, block_v2.node[1].size);
                assert_eq!(node1.address, block_v2.node[1].address);
                assert_eq!(node1.name_offset, block_v2.node[1].name_offset);
                assert_eq!(node1.name_length, block_v2.node[1].name_length);
                assert_eq!(node1.name_hash, block_v2.node[1].name_hash);

                // Verify node 2 (directory, no flags)
                let node2 = deserialized.node(2);
                assert_eq!(node2.flags as u32, 0, "Directory should have no flags");
                assert_eq!(node2.child, 3);
                assert_eq!(node2.parent, block_v2.node[2].parent);
                assert_eq!(node2.sibling, block_v2.node[2].sibling);
                assert_eq!(node2.address, block_v2.node[2].address);

                // Verify node 3 (executable file)
                let node3 = deserialized.node(3);
                assert_eq!(
                    node3.flags & NodeFlags::File,
                    NodeFlags::File.bits(),
                    "File flag not preserved on executable node"
                );
                // Executable flag is stored in mode field after V2->V1 conversion
                assert_eq!(node3.mode & 0b1, 1, "Executable bit not set in mode");
                assert_eq!(node3.parent, block_v2.node[3].parent);
                assert_eq!(node3.size, block_v2.node[3].size);
                assert_eq!(node3.address, block_v2.node[3].address);

                // Verify the deserialized block has V1 (Nametable) format
                let reader = deserialized.read();
                assert_eq!(
                    reader.node_block().version,
                    NodeBlockFormat::Nametable as u32
                );
                // Dirty flag should be cleared after deserialization
                assert_eq!(
                    reader.node_block().flags & NodeBlockFlags::Dirty.as_u32(),
                    0
                );
            }))
            .await
            .expect("Test task failed");
    }

    // ------------------------------------------------------------------
    // Node name store size-bound enforcement
    // ------------------------------------------------------------------

    #[test]
    fn node_name_store_accepts_small_names() {
        use lore_revision::node::NODE_NAME_MAX_SIZE;

        const _: () = assert!(
            NODE_NAME_MAX_SIZE >= 4 * 1024 * 1024,
            "NODE_NAME_MAX_SIZE must be at least 4 MiB"
        );
        let block = NodeBlock::new_zeroed();
        let (_, _) = block
            .write()
            .node_name_store("a-file.txt", 0, 0)
            .expect("small name should succeed");
    }

    #[test]
    fn node_name_store_rejects_when_cap_exceeded() {
        use lore_revision::node::MAX_NODE_NAME_LEN;
        use lore_revision::node::NODE_NAME_MAX_SIZE;

        let block = NodeBlock::new_zeroed();
        let mut writer = block.write();
        // Fill the buffer with the longest name a single node may carry, until
        // one more of them would cross NODE_NAME_MAX_SIZE.
        let chunk = "A".repeat(MAX_NODE_NAME_LEN);
        let mut total = 0usize;
        while total + chunk.len() <= NODE_NAME_MAX_SIZE {
            let (_, _) = writer
                .node_name_store(chunk.as_str(), 0, 0)
                .expect("fill-up should succeed up to the cap");
            total += chunk.len();
        }
        let err = writer
            .node_name_store(chunk.as_str(), 0, 0)
            .expect_err("node_name_store should reject appends that exceed NODE_NAME_MAX_SIZE");
        assert!(err.is_oversized(), "expected oversize error, got: {err:?}");
    }

    #[test]
    fn node_name_store_rejects_a_name_over_the_length_bound() {
        use lore_revision::node::MAX_NODE_NAME_LEN;

        let block = NodeBlock::new_zeroed();
        let mut writer = block.write();
        // The bound itself is storable; one byte past it is not.
        let (_, length) = writer
            .node_name_store("a".repeat(MAX_NODE_NAME_LEN).as_str(), 0, 0)
            .expect("a name at the length bound must be storable");
        assert_eq!(length as usize, MAX_NODE_NAME_LEN);

        let err = writer
            .node_name_store("a".repeat(MAX_NODE_NAME_LEN + 1).as_str(), 0, 0)
            .expect_err("a name past the length bound must be rejected");
        assert!(err.is_oversized(), "expected oversize error, got: {err:?}");
    }

    #[test]
    fn node_name_store_validates_a_reused_slot() {
        let block = NodeBlock::new_zeroed();
        let mut writer = block.write();
        // A rename takes the in-place reuse path, which must validate too.
        let (offset, length) = writer
            .node_name_store("original", 0, 0)
            .expect("initial append");
        let err = writer
            .node_name_store("..", offset, length)
            .expect_err("reusing a slot must not bypass validation");
        assert!(
            err.is_invalid_arguments(),
            "expected invalid arguments, got: {err:?}"
        );
    }

    #[test]
    fn node_name_store_slot_reuse_is_not_capped() {
        // In-place reuse takes the fast path that doesn't grow the buffer.
        // We confirm that once a slot exists and the new name fits, repeated
        // stores don't grow the buffer.
        let block = NodeBlock::new_zeroed();
        let mut writer = block.write();
        // First append creates the slot at offset 0.
        let (offset0, length0) = writer
            .node_name_store("seed-entry", 0, 0)
            .expect("initial append");
        // Second append moves the buffer past the first slot.
        let (_offset1, _length1) = writer
            .node_name_store("follow", 0, 0)
            .expect("second append");
        // Now the first slot is strictly before buffer end, so a shorter name
        // at (offset0, length0) must be reused in place.
        let (new_offset, new_length) = writer
            .node_name_store("X", offset0, length0)
            .expect("slot reuse should succeed regardless of cap");
        assert_eq!(new_offset, offset0);
        assert_eq!(new_length, 1);
    }
}
