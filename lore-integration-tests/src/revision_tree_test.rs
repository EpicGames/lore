// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for the memory-based revision control API.
//!
//! Drives the public `lore_revision_tree_*` surface over a real in-memory
//! storage handle, covering batch add fan-out onto shared parents, the fields
//! an entry carries into the tree, and every way a batch is rejected.

#[cfg(test)]
mod add_tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore::revision_tree::add::LoreRevisionTreeAddArgs;
    use lore::revision_tree::add::LoreRevisionTreeAddEntry;
    use lore::revision_tree::add::add;
    use lore::revision_tree::close::LoreRevisionTreeCloseArgs;
    use lore::revision_tree::close::close;
    use lore::revision_tree::handle::LoreRevisionTree;
    use lore::revision_tree::list_children::LoreRevisionTreeListChildrenArgs;
    use lore::revision_tree::list_children::list_children;
    use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
    use lore::revision_tree::load::load;
    use lore::revision_tree::node_info::LoreRevisionTreeNodeInfoArgs;
    use lore::revision_tree::node_info::node_info;
    use lore::storage::open;
    use lore::storage::open::LoreStorageOpenArgs;
    use lore_base::lore_spawn;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::LoreErrorCode;
    use lore_revision::event::LoreEvent;
    use lore_revision::event::revision_tree::LoreRevisionTreeNodeInfoEventData;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreError;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreNodeType;
    use lore_revision::interface::LoreString;
    use lore_revision::node::BLOCK_NODE_COUNT;
    use lore_revision::node::INVALID_NODE;
    use lore_revision::node::MAX_NODE_NAME_LEN;
    use lore_revision::node::NodeID;
    use lore_revision::node::ROOT_NODE;
    use tokio::task::JoinSet;

    /// Call-level id every test batch is submitted under, distinct from the
    /// per-entry ids so the two cannot be confused in an assertion.
    const CALL_ID: u64 = 900;

    /// The status a batch rejected during validation completes with. Asserted
    /// exactly rather than as "not zero", so a rejection that instead blew up in
    /// the apply phase — leaving part of the batch created — fails the test.
    const REJECTED_STATUS: i32 = LoreError::InvalidArguments as i32;

    #[derive(Debug, Clone, PartialEq)]
    enum Captured {
        Opened(u64),
        Loaded(u64),
        AddComplete(u64, NodeID, LoreErrorCode),
        BatchComplete(u64, LoreErrorCode),
        NodeInfo(Box<LoreRevisionTreeNodeInfoEventData>),
        Child(NodeID, String),
        Complete(i32),
        Other,
    }

    fn make_sink() -> (Arc<Mutex<Vec<Captured>>>, LoreEventCallback) {
        let sink: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            let record = match event {
                LoreEvent::StorageOpened(data) => Captured::Opened(data.handle_id),
                LoreEvent::RevisionTreeLoaded(data) => Captured::Loaded(data.handle_id),
                LoreEvent::RevisionTreeAddComplete(data) => {
                    Captured::AddComplete(data.id, data.node_id, data.error_code)
                }
                LoreEvent::RevisionTreeBatchComplete(data) => {
                    Captured::BatchComplete(data.id, data.error_code)
                }
                LoreEvent::RevisionTreeNodeInfo(data) => Captured::NodeInfo(Box::new(data.clone())),
                LoreEvent::RevisionTreeChild(data) => {
                    Captured::Child(data.node_id, data.name.as_str().to_string())
                }
                LoreEvent::Complete(data) => Captured::Complete(data.status),
                _ => Captured::Other,
            };
            sink_for_cb.lock().unwrap().push(record);
        }));
        (sink, callback)
    }

    /// Open an in-memory store and load an empty revision tree handle on it.
    async fn load_handle(repository: Partition) -> LoreRevisionTree {
        let (sink, callback) = make_sink();
        let status = open::open(
            LoreGlobalArgs::default(),
            LoreStorageOpenArgs {
                in_memory: 1,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "opening an in-memory store must succeed");
        let store_handle_id = sink
            .lock()
            .unwrap()
            .iter()
            .find_map(|event| match event {
                Captured::Opened(id) => Some(*id),
                _ => None,
            })
            .expect("open must emit StorageOpened");

        let (sink, callback) = make_sink();
        let status = load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store: lore::storage::handle::LoreStore {
                    handle_id: store_handle_id,
                },
                repository,
                revision_hash: Hash::default(),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "loading an empty revision tree must succeed");
        let handle_id = sink
            .lock()
            .unwrap()
            .iter()
            .find_map(|event| match event {
                Captured::Loaded(id) => Some(*id),
                _ => None,
            })
            .expect("load must emit RevisionTreeLoaded");
        LoreRevisionTree { handle_id }
    }

    fn entry(
        id: u64,
        parent_node_id: NodeID,
        name: &str,
        kind: LoreNodeType,
    ) -> LoreRevisionTreeAddEntry {
        LoreRevisionTreeAddEntry {
            id,
            parent_node_id,
            parent_entry: 0,
            name: LoreString::from_str(name),
            kind: kind as u32,
            mode: 0o644,
            size: 0,
            address: Address::default(),
        }
    }

    fn nested_entry(
        id: u64,
        parent_entry: u32,
        name: &str,
        kind: LoreNodeType,
    ) -> LoreRevisionTreeAddEntry {
        LoreRevisionTreeAddEntry {
            parent_node_id: INVALID_NODE,
            parent_entry,
            ..entry(id, ROOT_NODE, name, kind)
        }
    }

    async fn run_add(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeAddEntry>,
    ) -> (i32, Vec<Captured>) {
        let (sink, callback) = make_sink();
        let status = add(
            LoreGlobalArgs::default(),
            LoreRevisionTreeAddArgs {
                id: CALL_ID,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    fn added_node(events: &[Captured], id: u64) -> NodeID {
        events
            .iter()
            .find_map(|event| match event {
                Captured::AddComplete(event_id, node_id, code) if *event_id == id => {
                    assert_eq!(*code, LoreErrorCode::None, "entry {id} must succeed");
                    Some(*node_id)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("entry {id} must emit AddComplete, got {events:?}"))
    }

    /// Every per-entry terminal in emission order, so a test can pin which
    /// entries reported and which stayed silent.
    fn add_completes(events: &[Captured]) -> Vec<(u64, NodeID, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                Captured::AddComplete(id, node_id, code) => Some((*id, *node_id, *code)),
                _ => None,
            })
            .collect()
    }

    /// The single rejected entry a failed batch is expected to report.
    fn rejected(id: u64) -> Vec<(u64, NodeID, LoreErrorCode)> {
        vec![(id, INVALID_NODE, LoreErrorCode::InvalidArguments)]
    }

    /// Every batch terminal in emission order, so a test can pin that exactly one
    /// fired and what it carried.
    fn batch_outcomes(events: &[Captured]) -> Vec<(u64, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                Captured::BatchComplete(id, code) => Some((*id, *code)),
                _ => None,
            })
            .collect()
    }

    async fn child_names(handle: LoreRevisionTree, parent_node_id: NodeID) -> Vec<String> {
        let (sink, callback) = make_sink();
        let status = list_children(
            LoreGlobalArgs::default(),
            LoreRevisionTreeListChildrenArgs {
                id: 1,
                handle,
                parent_node_id,
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "listing children must succeed");
        let mut names: Vec<String> = sink
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Captured::Child(_, name) => Some(name.clone()),
                _ => None,
            })
            .collect();
        names.sort();
        names
    }

    async fn node_info_of(
        handle: LoreRevisionTree,
        node_id: NodeID,
    ) -> LoreRevisionTreeNodeInfoEventData {
        let (sink, callback) = make_sink();
        let status = node_info(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodeInfoArgs {
                id: 1,
                handle,
                node_id,
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "node info must succeed");
        sink.lock()
            .unwrap()
            .iter()
            .find_map(|event| match event {
                Captured::NodeInfo(data) if data.node_id == node_id => Some((**data).clone()),
                _ => None,
            })
            .expect("node info must report the node")
    }

    async fn parent_of(handle: LoreRevisionTree, node_id: NodeID) -> NodeID {
        node_info_of(handle, node_id).await.parent_id
    }

    async fn close_handle(handle: LoreRevisionTree) {
        let (sink, callback) = make_sink();
        let status = close(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCloseArgs { id: 1, handle },
            callback,
        )
        .await;
        assert_eq!(
            status,
            0,
            "closing a loaded handle must succeed, got {:?}",
            sink.lock().unwrap()
        );
    }

    /// Many siblings landing under one parent that an earlier call created, so
    /// every entry in this batch is a leaf of the same existing node and each
    /// gets its own slot in that node's child chain.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn add_under_a_preexisting_shared_parent_keeps_every_sibling() {
        let handle = load_handle(Partition::from([0x11u8; 16])).await;

        let (status, events) = run_add(
            handle,
            vec![entry(1, ROOT_NODE, "shared", LoreNodeType::Directory)],
        )
        .await;
        assert_eq!(
            status, 0,
            "creating the parent must succeed, got {events:?}"
        );
        let shared = added_node(&events, 1);

        const COUNT: u64 = 64;
        let entries: Vec<LoreRevisionTreeAddEntry> = (0..COUNT)
            .map(|index| {
                entry(
                    100 + index,
                    shared,
                    &format!("file-{index}"),
                    LoreNodeType::File,
                )
            })
            .collect();

        let (status, events) = run_add(handle, entries).await;
        assert_eq!(status, 0, "the fan-out batch must succeed");

        let mut nodes = HashSet::new();
        for index in 0..COUNT {
            let node_id = added_node(&events, 100 + index);
            assert!(
                nodes.insert(node_id),
                "every sibling must get a distinct node id, {node_id} repeated"
            );
        }

        let names = child_names(handle, shared).await;
        let mut expected: Vec<String> = (0..COUNT).map(|index| format!("file-{index}")).collect();
        expected.sort();
        assert_eq!(
            names, expected,
            "every sibling in the batch must get its own slot in the child chain"
        );
    }

    /// The order a caller can rely on: every per-entry terminal, then exactly one
    /// batch terminal carrying the call id, then `Complete`. A caller waiting on
    /// the batch terminal must already have seen every entry it will hear about.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn add_reports_entries_then_the_batch_terminal_then_complete() {
        let handle = load_handle(Partition::from([0xEEu8; 16])).await;

        const COUNT: u64 = 32;
        let entries: Vec<LoreRevisionTreeAddEntry> = (0..COUNT)
            .map(|index| {
                entry(
                    index + 1,
                    ROOT_NODE,
                    &format!("f-{index:03}"),
                    LoreNodeType::File,
                )
            })
            .collect();

        let (status, events) = run_add(handle, entries).await;
        assert_eq!(status, 0, "got {events:?}");

        let position = |predicate: fn(&Captured) -> bool| {
            events
                .iter()
                .position(predicate)
                .unwrap_or_else(|| panic!("expected event missing from {events:?}"))
        };
        let last_add = events
            .iter()
            .rposition(|event| matches!(event, Captured::AddComplete(..)))
            .expect("every entry must report");
        let batch = position(|event| matches!(event, Captured::BatchComplete(..)));
        let complete = position(|event| matches!(event, Captured::Complete(_)));

        assert_eq!(
            add_completes(&events).len(),
            COUNT as usize,
            "every entry reports exactly once, got {events:?}"
        );
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::None)],
            "exactly one batch terminal, carrying the call id, got {events:?}"
        );
        assert!(
            last_add < batch,
            "every entry must report before the batch terminal, got {events:?}"
        );
        assert!(
            batch < complete,
            "the batch terminal must precede Complete, got {events:?}"
        );
    }

    /// The shared parents are created by the same batch, each before the leaves
    /// that reference it, so a parent referenced by many entries is created
    /// exactly once and no leaf races its own ancestor. The four parents' leaf
    /// groups then run concurrently.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn add_under_parents_created_in_the_same_batch_fans_out_per_parent() {
        let handle = load_handle(Partition::from([0x22u8; 16])).await;

        const PARENTS: u64 = 4;
        const CHILDREN: u64 = 16;

        let mut entries = Vec::new();
        for parent in 0..PARENTS {
            entries.push(entry(
                parent,
                ROOT_NODE,
                &format!("dir-{parent}"),
                LoreNodeType::Directory,
            ));
        }
        for parent in 0..PARENTS {
            for child in 0..CHILDREN {
                entries.push(nested_entry(
                    1000 + parent * CHILDREN + child,
                    parent as u32,
                    &format!("file-{child}"),
                    LoreNodeType::File,
                ));
            }
        }

        let (status, events) = run_add(handle, entries).await;
        assert_eq!(status, 0, "the subtree batch must succeed");

        let roots = child_names(handle, ROOT_NODE).await;
        let mut expected_dirs: Vec<String> =
            (0..PARENTS).map(|parent| format!("dir-{parent}")).collect();
        expected_dirs.sort();
        assert_eq!(
            roots, expected_dirs,
            "each shared parent must be created exactly once"
        );

        let mut expected_files: Vec<String> =
            (0..CHILDREN).map(|child| format!("file-{child}")).collect();
        expected_files.sort();
        for parent in 0..PARENTS {
            let parent_node = added_node(&events, parent);
            assert_eq!(
                child_names(handle, parent_node).await,
                expected_files,
                "every child fanned out under dir-{parent} must survive"
            );
            for child in 0..CHILDREN {
                let child_node = added_node(&events, 1000 + parent * CHILDREN + child);
                assert_eq!(
                    parent_of(handle, child_node).await,
                    parent_node,
                    "child must hang off the parent its entry referenced"
                );
            }
        }
    }

    /// A batch several directories deep. Each level is applied as one wave, so
    /// this pins that a node is never created before the entry it parents onto:
    /// the branches are independent and run together, while the depth ordering
    /// within a branch is respected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn add_builds_a_multi_level_subtree_in_one_batch() {
        let handle = load_handle(Partition::from([0xCCu8; 16])).await;

        // ids encode the shape: 1 "a", 2 "b" at the top; then a/x a/y b/x;
        // then a/x/deep; then one file in each leaf directory.
        let entries = vec![
            entry(1, ROOT_NODE, "a", LoreNodeType::Directory),
            entry(2, ROOT_NODE, "b", LoreNodeType::Directory),
            nested_entry(3, 0, "x", LoreNodeType::Directory),
            nested_entry(4, 0, "y", LoreNodeType::Directory),
            nested_entry(5, 1, "x", LoreNodeType::Directory),
            nested_entry(6, 2, "deep", LoreNodeType::Directory),
            nested_entry(7, 5, "leaf.txt", LoreNodeType::File),
            nested_entry(8, 3, "leaf.txt", LoreNodeType::File),
            nested_entry(9, 4, "leaf.txt", LoreNodeType::File),
        ];

        let (status, events) = run_add(handle, entries).await;
        assert_eq!(status, 0, "the nested batch must succeed, got {events:?}");

        let node = |id: u64| added_node(&events, id);
        for (child, parent) in [(3u64, 1u64), (4, 1), (5, 2), (6, 3), (7, 6), (8, 4), (9, 5)] {
            assert_eq!(
                parent_of(handle, node(child)).await,
                node(parent),
                "entry {child} must hang off entry {parent}"
            );
        }

        assert_eq!(
            child_names(handle, ROOT_NODE).await,
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            child_names(handle, node(1)).await,
            vec!["x".to_string(), "y".to_string()]
        );
        assert_eq!(child_names(handle, node(3)).await, vec!["deep".to_string()]);
        assert_eq!(
            child_names(handle, node(6)).await,
            vec!["leaf.txt".to_string()]
        );
    }

    /// A batch mixing both parent forms: leaves onto a parent that already
    /// exists in the tree, and leaves onto a parent this batch creates.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn add_mixes_preexisting_and_in_batch_parents() {
        let handle = load_handle(Partition::from([0x33u8; 16])).await;

        let (status, events) = run_add(
            handle,
            vec![entry(1, ROOT_NODE, "existing", LoreNodeType::Directory)],
        )
        .await;
        assert_eq!(status, 0, "got {events:?}");
        let existing = added_node(&events, 1);

        const COUNT: u64 = 24;
        let mut entries = vec![entry(2, ROOT_NODE, "fresh", LoreNodeType::Directory)];
        for index in 0..COUNT {
            entries.push(entry(
                100 + index,
                existing,
                &format!("old-{index}"),
                LoreNodeType::File,
            ));
            entries.push(nested_entry(
                200 + index,
                0,
                &format!("new-{index}"),
                LoreNodeType::File,
            ));
        }

        let (status, events) = run_add(handle, entries).await;
        assert_eq!(status, 0, "the mixed batch must succeed");
        let fresh = added_node(&events, 2);

        let mut expected_old: Vec<String> =
            (0..COUNT).map(|index| format!("old-{index}")).collect();
        expected_old.sort();
        assert_eq!(
            child_names(handle, existing).await,
            expected_old,
            "leaves onto the pre-existing parent must all survive"
        );

        let mut expected_new: Vec<String> =
            (0..COUNT).map(|index| format!("new-{index}")).collect();
        expected_new.sort();
        assert_eq!(
            child_names(handle, fresh).await,
            expected_new,
            "leaves onto the in-batch parent must all survive"
        );
    }

    /// Concurrent batches from separate tasks, each adding distinct names under
    /// one shared parent. Every sibling must land: the adds are distinct, which
    /// is the case the tree add is safe to run concurrently.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_batches_under_one_shared_parent_keep_every_sibling() {
        let handle = load_handle(Partition::from([0x44u8; 16])).await;

        let (status, events) = run_add(
            handle,
            vec![entry(1, ROOT_NODE, "shared", LoreNodeType::Directory)],
        )
        .await;
        assert_eq!(status, 0, "got {events:?}");
        let shared = added_node(&events, 1);

        const BATCHES: u64 = 4;
        const PER_BATCH: u64 = 16;
        let mut tasks: JoinSet<i32> = JoinSet::new();
        for batch in 0..BATCHES {
            lore_spawn!(tasks, async move {
                let entries: Vec<LoreRevisionTreeAddEntry> = (0..PER_BATCH)
                    .map(|index| {
                        entry(
                            batch * PER_BATCH + index,
                            shared,
                            &format!("b{batch}-f{index}"),
                            LoreNodeType::File,
                        )
                    })
                    .collect();
                run_add(handle, entries).await.0
            });
        }
        while let Some(result) = tasks.join_next().await {
            assert_eq!(
                result.expect("batch task must not panic"),
                0,
                "every concurrent batch must succeed"
            );
        }

        let names = child_names(handle, shared).await;
        let mut expected: Vec<String> = (0..BATCHES)
            .flat_map(|batch| (0..PER_BATCH).map(move |index| format!("b{batch}-f{index}")))
            .collect();
        expected.sort();
        assert_eq!(
            names, expected,
            "concurrent batches of distinct siblings must all survive"
        );
    }

    /// Every field an entry carries must reach the tree and read back through
    /// `node_info`, and a file arriving without a file id must be assigned one
    /// without disturbing the content address it supplied.
    #[tokio::test]
    async fn add_carries_every_entry_field_into_the_tree() {
        let handle = load_handle(Partition::from([0x55u8; 16])).await;
        let address = Address {
            hash: Hash::from([0x37u8; 32]),
            context: Context::default(),
        };

        let (status, events) = run_add(
            handle,
            vec![LoreRevisionTreeAddEntry {
                mode: 0o755,
                size: 4096,
                address,
                ..entry(1, ROOT_NODE, "payload.bin", LoreNodeType::File)
            }],
        )
        .await;
        assert_eq!(status, 0, "got {events:?}");

        let info = node_info_of(handle, added_node(&events, 1)).await;
        assert_eq!(info.name.as_str(), "payload.bin");
        assert_eq!(info.parent_id, ROOT_NODE);
        assert_eq!(info.kind, LoreNodeType::File as u32);
        assert_eq!(info.mode, 0o755, "got {info:?}");
        assert_eq!(info.size, 4096, "got {info:?}");
        assert_eq!(
            info.address.hash, address.hash,
            "the supplied content hash must cross unchanged, got {info:?}"
        );
        assert_ne!(
            info.file_id,
            Context::default(),
            "a file added without a file id must be assigned one, got {info:?}"
        );
    }

    /// One invalid entry rejects the whole batch: the valid entries ahead of it
    /// are not created, only the offending entry reports, and the handle keeps
    /// working for the next batch.
    #[tokio::test]
    async fn a_rejected_batch_creates_nothing_and_leaves_the_handle_usable() {
        let handle = load_handle(Partition::from([0x66u8; 16])).await;

        let (status, events) = run_add(
            handle,
            vec![
                entry(1, ROOT_NODE, "dir", LoreNodeType::Directory),
                entry(2, ROOT_NODE, "file", LoreNodeType::File),
                entry(3, ROOT_NODE, "", LoreNodeType::File),
            ],
        )
        .await;
        assert_eq!(
            status, REJECTED_STATUS,
            "a batch with an invalid entry must be rejected, got {events:?}"
        );
        assert_eq!(
            add_completes(&events),
            rejected(3),
            "only the offending entry reports, got {events:?}"
        );
        assert!(
            child_names(handle, ROOT_NODE).await.is_empty(),
            "a rejected batch must create nothing"
        );

        let (status, events) = run_add(
            handle,
            vec![entry(4, ROOT_NODE, "dir", LoreNodeType::Directory)],
        )
        .await;
        assert_eq!(
            status, 0,
            "the handle must stay usable after a rejected batch, got {events:?}"
        );
        assert_eq!(
            child_names(handle, ROOT_NODE).await,
            vec!["dir".to_string()]
        );
    }

    /// A name already taken under the parent rejects even when it differs in
    /// case, and the child that was already there is left alone.
    #[tokio::test]
    async fn add_rejects_a_collision_with_an_existing_child() {
        let handle = load_handle(Partition::from([0x77u8; 16])).await;

        let (status, events) = run_add(
            handle,
            vec![entry(1, ROOT_NODE, "doc.md", LoreNodeType::File)],
        )
        .await;
        assert_eq!(status, 0, "got {events:?}");

        let (status, events) = run_add(
            handle,
            vec![
                entry(2, ROOT_NODE, "notes.txt", LoreNodeType::File),
                entry(3, ROOT_NODE, "DOC.MD", LoreNodeType::File),
            ],
        )
        .await;
        assert_eq!(
            status, REJECTED_STATUS,
            "colliding with an existing child must be rejected, got {events:?}"
        );
        assert_eq!(add_completes(&events), rejected(3), "got {events:?}");
        assert_eq!(
            child_names(handle, ROOT_NODE).await,
            vec!["doc.md".to_string()],
            "the existing child survives and the batch adds nothing"
        );
    }

    /// Parents that cannot take a child. `UNALLOCATED_SLOT` is an id inside the
    /// block the tree already occupies but on a slot no node has been handed
    /// out from: it reads back as a zeroed, nameless node, which is a directory
    /// by flags and would otherwise be accepted as a parent.
    #[tokio::test]
    async fn add_rejects_unknown_unallocated_and_leaf_parents() {
        const OUT_OF_RANGE: NodeID = 1_000_000;
        const UNALLOCATED_SLOT: NodeID = 400;

        let handle = load_handle(Partition::from([0x88u8; 16])).await;
        let (status, events) = run_add(
            handle,
            vec![entry(1, ROOT_NODE, "leaf.txt", LoreNodeType::File)],
        )
        .await;
        assert_eq!(status, 0, "got {events:?}");
        let leaf = added_node(&events, 1);

        for (id, parent) in [(2u64, OUT_OF_RANGE), (3, UNALLOCATED_SLOT), (4, leaf)] {
            let (status, events) =
                run_add(handle, vec![entry(id, parent, "child", LoreNodeType::File)]).await;
            assert_eq!(
                status, REJECTED_STATUS,
                "parent node {parent} must be rejected, got {events:?}"
            );
            assert_eq!(add_completes(&events), rejected(id), "got {events:?}");
        }

        assert_eq!(
            child_names(handle, ROOT_NODE).await,
            vec!["leaf.txt".to_string()],
            "no rejected batch may leave a node behind"
        );
    }

    /// The entry-shaped rejections, each as its own batch: an empty name, a
    /// name carrying a path separator, an unsupported kind, two entries
    /// claiming one name under a parent that already exists, a parent reference
    /// to a later entry, a parent reference to an entry that is a file, a parent
    /// reference to an entry that is a link, and two entries claiming one name
    /// under a parent the same batch creates.
    ///
    /// The last four are names the node name table refuses. They are checked
    /// here rather than at write time, so they fail as a rejection with nothing
    /// created — including when the offending entry sits between valid ones,
    /// which is the case that would otherwise apply part of the batch.
    #[tokio::test]
    async fn add_rejects_invalid_names_kinds_and_entry_references() {
        let handle = load_handle(Partition::from([0x99u8; 16])).await;
        let oversize = "x".repeat(MAX_NODE_NAME_LEN + 1);

        let batches: Vec<(u64, Vec<LoreRevisionTreeAddEntry>)> = vec![
            (1, vec![entry(1, ROOT_NODE, "", LoreNodeType::File)]),
            (2, vec![entry(2, ROOT_NODE, "a/b", LoreNodeType::File)]),
            (
                3,
                vec![LoreRevisionTreeAddEntry {
                    kind: 99,
                    ..entry(3, ROOT_NODE, "thing", LoreNodeType::File)
                }],
            ),
            (
                5,
                vec![
                    entry(4, ROOT_NODE, "dup", LoreNodeType::File),
                    entry(5, ROOT_NODE, "DUP", LoreNodeType::File),
                ],
            ),
            (
                6,
                vec![
                    nested_entry(6, 1, "early", LoreNodeType::File),
                    entry(7, ROOT_NODE, "later", LoreNodeType::Directory),
                ],
            ),
            (
                9,
                vec![
                    entry(8, ROOT_NODE, "file", LoreNodeType::File),
                    nested_entry(9, 0, "child", LoreNodeType::File),
                ],
            ),
            (
                11,
                vec![
                    entry(10, ROOT_NODE, "link", LoreNodeType::Link),
                    nested_entry(11, 0, "child", LoreNodeType::File),
                ],
            ),
            (
                14,
                vec![
                    entry(12, ROOT_NODE, "dir", LoreNodeType::Directory),
                    nested_entry(13, 0, "dup", LoreNodeType::File),
                    nested_entry(14, 0, "DUP", LoreNodeType::File),
                ],
            ),
            (15, vec![entry(15, ROOT_NODE, "..", LoreNodeType::File)]),
            (16, vec![entry(16, ROOT_NODE, "a\\b", LoreNodeType::File)]),
            (17, vec![entry(17, ROOT_NODE, "\0lead", LoreNodeType::File)]),
            (
                18,
                vec![entry(18, ROOT_NODE, &oversize, LoreNodeType::File)],
            ),
            (
                20,
                vec![
                    entry(19, ROOT_NODE, "before", LoreNodeType::File),
                    entry(20, ROOT_NODE, "..", LoreNodeType::File),
                    entry(21, ROOT_NODE, "after", LoreNodeType::File),
                ],
            ),
        ];

        for (offending, entries) in batches {
            let (status, events) = run_add(handle, entries).await;
            assert_eq!(
                status, REJECTED_STATUS,
                "entry {offending} must reject its batch, got {events:?}"
            );
            assert_eq!(
                add_completes(&events),
                rejected(offending),
                "got {events:?}"
            );
        }

        assert!(
            child_names(handle, ROOT_NODE).await.is_empty(),
            "no rejected batch may leave a node behind"
        );
    }

    /// A batch holding more nodes than one block has slots. Crossing a block
    /// boundary is what drives the allocator to recycle and allocate blocks,
    /// which no other add test reaches.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn add_fills_more_nodes_than_one_block_holds() {
        let handle = load_handle(Partition::from([0xDDu8; 16])).await;

        let count = (BLOCK_NODE_COUNT * 3) as u64;
        let entries: Vec<LoreRevisionTreeAddEntry> = (0..count)
            .map(|index| {
                entry(
                    index + 1,
                    ROOT_NODE,
                    &format!("f-{index:05}"),
                    LoreNodeType::File,
                )
            })
            .collect();

        let (status, events) = run_add(handle, entries).await;
        assert_eq!(status, 0, "a batch spanning several blocks must succeed");

        let mut nodes = HashSet::new();
        for index in 0..count {
            let node_id = added_node(&events, index + 1);
            assert!(
                nodes.insert(node_id),
                "every node must get a distinct id, {node_id} repeated"
            );
        }
        assert_eq!(
            child_names(handle, ROOT_NODE).await.len(),
            count as usize,
            "every node must survive in the child chain"
        );
    }

    /// A closed handle is the call's failure, not any entry's: it reports on the
    /// batch terminal, which carries the call id, and leaves every entry silent.
    #[tokio::test]
    async fn add_on_a_closed_handle_reports_only_the_batch_terminal() {
        let handle = load_handle(Partition::from([0xAAu8; 16])).await;
        close_handle(handle).await;

        let (status, events) = run_add(
            handle,
            vec![
                entry(7, ROOT_NODE, "x", LoreNodeType::File),
                entry(8, ROOT_NODE, "y", LoreNodeType::File),
            ],
        )
        .await;

        assert_ne!(status, 0, "a closed handle must fail the call");
        assert!(
            add_completes(&events).is_empty(),
            "no entry may report when the call itself failed, got {events:?}"
        );
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::InvalidArguments)],
            "got {events:?}"
        );
        assert!(events.contains(&Captured::Complete(status)));
    }

    /// A link addresses another revision, which this handle does not mutate, so
    /// it cannot take a child — even though `list_children` will happily list the
    /// children it resolves to.
    #[tokio::test]
    async fn add_rejects_a_link_parent_that_list_children_resolves() {
        let handle = load_handle(Partition::from([0xBBu8; 16])).await;

        let (status, events) = run_add(
            handle,
            vec![entry(1, ROOT_NODE, "target", LoreNodeType::Link)],
        )
        .await;
        assert_eq!(status, 0, "got {events:?}");
        let link = added_node(&events, 1);

        let (status, events) =
            run_add(handle, vec![entry(2, link, "child", LoreNodeType::File)]).await;
        assert_eq!(
            status, REJECTED_STATUS,
            "a link must not take a child, got {events:?}"
        );
        assert_eq!(add_completes(&events), rejected(2), "got {events:?}");

        assert!(
            child_names(handle, ROOT_NODE).await == vec!["target".to_string()],
            "the rejected batch adds nothing"
        );
    }
}
