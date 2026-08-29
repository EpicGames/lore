// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for the memory-based revision control API.
//!
//! Drives the public `lore_revision_tree_*` surface over a real in-memory
//! storage handle, covering each batch verb's fan-out, the fields an entry
//! carries into the tree, and every way a batch is rejected. One module per
//! verb over the shared [`support`] scaffolding.

/// Scaffolding shared by every verb's tests: the event sink, a loaded handle
/// over a real in-memory store, and the read verbs a write test checks itself
/// against.
#[cfg(test)]
mod support {
    use std::sync::Arc;
    use std::sync::Mutex;

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
    use lore_base::types::Address;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::LoreErrorCode;
    use lore_revision::event::LoreEvent;
    use lore_revision::event::revision_tree::LoreRevisionTreeInfoEventData;
    use lore_revision::event::revision_tree::LoreRevisionTreeNodeInfoEventData;
    use lore_revision::interface::LoreError;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreMetadata;
    use lore_revision::node::NodeID;

    /// Call-level id every test batch is submitted under, distinct from the
    /// per-entry ids so the two cannot be confused in an assertion.
    pub(super) const CALL_ID: u64 = 900;

    /// The status a batch rejected during validation completes with. Asserted
    /// exactly rather than as "not zero", so a rejection that instead blew up in
    /// the apply phase — leaving part of the batch applied — fails the test.
    pub(super) const REJECTED_STATUS: i32 = LoreError::InvalidArguments as i32;

    #[derive(Debug, Clone, PartialEq)]
    pub(super) enum Captured {
        Opened(u64),
        Loaded(u64),
        AddComplete(u64, NodeID, LoreErrorCode),
        DeleteComplete(u64, u64, LoreErrorCode),
        ModifyComplete(u64, NodeID, LoreErrorCode),
        MoveComplete(u64, NodeID, LoreErrorCode),
        MetadataSetComplete(u64, LoreErrorCode),
        MetadataGetComplete(u64, String, LoreMetadata, LoreErrorCode),
        MetadataClearComplete(u64, u8, LoreErrorCode),
        BatchComplete(u64, LoreErrorCode),
        NodeInfo(Box<LoreRevisionTreeNodeInfoEventData>),
        Info(Box<LoreRevisionTreeInfoEventData>),
        ResolvePath(NodeID, LoreErrorCode),
        CommitComplete(u64, Hash, Hash, LoreErrorCode),
        Child(NodeID, String, Address),
        Complete(i32),
        Other,
    }

    pub(super) fn make_sink() -> (Arc<Mutex<Vec<Captured>>>, LoreEventCallback) {
        let sink: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            let record = match event {
                LoreEvent::StorageOpened(data) => Captured::Opened(data.handle_id),
                LoreEvent::RevisionTreeLoaded(data) => Captured::Loaded(data.handle_id),
                LoreEvent::RevisionTreeAddComplete(data) => {
                    Captured::AddComplete(data.entry_id, data.node_id, data.error_code)
                }
                LoreEvent::RevisionTreeDeleteComplete(data) => {
                    Captured::DeleteComplete(data.entry_id, data.node_count, data.error_code)
                }
                LoreEvent::RevisionTreeModifyComplete(data) => {
                    Captured::ModifyComplete(data.entry_id, data.node_id, data.error_code)
                }
                LoreEvent::RevisionTreeMoveComplete(data) => {
                    Captured::MoveComplete(data.entry_id, data.node_id, data.error_code)
                }
                LoreEvent::RevisionTreeMetadataSetComplete(data) => {
                    Captured::MetadataSetComplete(data.entry_id, data.error_code)
                }
                LoreEvent::RevisionTreeMetadataGetComplete(data) => Captured::MetadataGetComplete(
                    data.entry_id,
                    data.key.as_str().to_string(),
                    data.value.clone(),
                    data.error_code,
                ),
                LoreEvent::RevisionTreeMetadataClearComplete(data) => {
                    Captured::MetadataClearComplete(data.entry_id, data.removed, data.error_code)
                }
                LoreEvent::RevisionTreeBatchComplete(data) => {
                    Captured::BatchComplete(data.batch_id, data.error_code)
                }
                LoreEvent::RevisionTreeNodeInfo(data) => Captured::NodeInfo(Box::new(data.clone())),
                LoreEvent::RevisionTreeInfo(data) => Captured::Info(Box::new(data.clone())),
                LoreEvent::RevisionTreeResolvePathComplete(data) => {
                    Captured::ResolvePath(data.node_id, data.error_code)
                }
                LoreEvent::RevisionTreeCommitComplete(data) => Captured::CommitComplete(
                    data.id,
                    data.revision_hash,
                    data.new_tip_hash,
                    data.error_code,
                ),
                LoreEvent::RevisionTreeChild(data) => {
                    Captured::Child(data.node_id, data.name.as_str().to_string(), data.address)
                }
                LoreEvent::Complete(data) => Captured::Complete(data.status),
                _ => Captured::Other,
            };
            sink_for_cb.lock().unwrap().push(record);
        }));
        (sink, callback)
    }

    /// Open an in-memory store and load an empty revision tree handle on it.
    pub(super) async fn load_handle(repository: Partition) -> LoreRevisionTree {
        load_on(open_store().await, repository, Hash::default()).await
    }

    pub(super) async fn open_store() -> u64 {
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
        sink.lock()
            .unwrap()
            .iter()
            .find_map(|event| match event {
                Captured::Opened(id) => Some(*id),
                _ => None,
            })
            .expect("open must emit StorageOpened")
    }

    /// Load a revision tree against an already-open store, at `revision_hash` — the
    /// zero hash for an empty tree, or a committed revision to read it back.
    pub(super) async fn load_on(
        store_handle_id: u64,
        repository: Partition,
        revision_hash: Hash,
    ) -> LoreRevisionTree {
        let (sink, callback) = make_sink();
        let status = load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store: lore::storage::handle::LoreStore {
                    handle_id: store_handle_id,
                },
                repository,
                revision_hash,
            },
            callback,
        )
        .await;
        assert_eq!(
            status,
            0,
            "loading revision {revision_hash} must succeed, got {:?}",
            sink.lock().unwrap()
        );
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

    /// Every batch terminal in emission order, so a test can pin that exactly one
    /// fired and what it carried.
    /// Every move terminal in emission order, so a test can pin what each entry reported
    /// as well as that the call as a whole finished.
    pub(super) fn move_outcomes(events: &[Captured]) -> Vec<(u64, NodeID, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                Captured::MoveComplete(id, node_id, code) => Some((*id, *node_id, *code)),
                _ => None,
            })
            .collect()
    }

    pub(super) fn batch_outcomes(events: &[Captured]) -> Vec<(u64, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                Captured::BatchComplete(id, code) => Some((*id, *code)),
                _ => None,
            })
            .collect()
    }

    pub(super) async fn child_names(
        handle: LoreRevisionTree,
        parent_node_id: NodeID,
    ) -> Vec<String> {
        let mut names: Vec<String> = child_records(handle, parent_node_id)
            .await
            .into_iter()
            .map(|(_, name, _)| name)
            .collect();
        names.sort();
        names
    }

    /// Every child as `(node_id, name, address)`, in listing order.
    pub(super) async fn child_records(
        handle: LoreRevisionTree,
        parent_node_id: NodeID,
    ) -> Vec<(NodeID, String, Address)> {
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
        let records = sink.lock().unwrap();
        records
            .iter()
            .filter_map(|event| match event {
                Captured::Child(node_id, name, address) => Some((*node_id, name.clone(), *address)),
                _ => None,
            })
            .collect()
    }

    pub(super) async fn node_info_of(
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

    pub(super) async fn parent_of(handle: LoreRevisionTree, node_id: NodeID) -> NodeID {
        node_info_of(handle, node_id).await.parent_id
    }

    pub(super) async fn close_handle(handle: LoreRevisionTree) {
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
}

#[cfg(test)]
mod add_tests {
    use std::collections::HashSet;

    use lore::revision_tree::add::LoreRevisionTreeAddArgs;
    use lore::revision_tree::add::LoreRevisionTreeAddEntry;
    use lore::revision_tree::add::add;
    use lore::revision_tree::handle::LoreRevisionTree;
    use lore_base::lore_spawn;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::LoreErrorCode;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreNodeType;
    use lore_revision::interface::LoreString;
    use lore_revision::node::BLOCK_NODE_COUNT;
    use lore_revision::node::INVALID_NODE;
    use lore_revision::node::MAX_NODE_NAME_LEN;
    use lore_revision::node::NodeID;
    use lore_revision::node::ROOT_NODE;
    use tokio::task::JoinSet;

    use super::support::*;

    fn entry(
        entry_id: u64,
        parent_node_id: NodeID,
        name: &str,
        kind: LoreNodeType,
    ) -> LoreRevisionTreeAddEntry {
        LoreRevisionTreeAddEntry {
            entry_id,
            parent_node_id,
            parent_entry_index: 0,
            name: LoreString::from_str(name),
            kind: kind as u32,
            mode: 0o644,
            size: 0,
            address: Address::default(),
        }
    }

    fn nested_entry(
        id: u64,
        parent_entry_index: u32,
        name: &str,
        kind: LoreNodeType,
    ) -> LoreRevisionTreeAddEntry {
        LoreRevisionTreeAddEntry {
            parent_node_id: INVALID_NODE,
            parent_entry_index,
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
                batch_id: CALL_ID,
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

#[cfg(test)]
mod modify_tests {
    use lore::revision_tree::add::LoreRevisionTreeAddArgs;
    use lore::revision_tree::add::LoreRevisionTreeAddEntry;
    use lore::revision_tree::add::add;
    use lore::revision_tree::handle::LoreRevisionTree;
    use lore::revision_tree::modify::LoreRevisionTreeModifyArgs;
    use lore::revision_tree::modify::LoreRevisionTreeModifyEntry;
    use lore::revision_tree::modify::modify;
    use lore_base::lore_spawn;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::LoreErrorCode;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreNodeType;
    use lore_revision::interface::LoreString;
    use lore_revision::node::BLOCK_NODE_COUNT;
    use lore_revision::node::INVALID_NODE;
    use lore_revision::node::NodeID;
    use lore_revision::node::ROOT_NODE;
    use tokio::task::JoinSet;

    use super::support::*;

    /// A content address distinct per `seed`, so a test can tell which value a
    /// node holds without threading the expected hash through every helper.
    fn address(hash: u64, context: Context) -> Address {
        Address {
            hash: Hash::from_u64(hash),
            context,
        }
    }

    fn file_id() -> Context {
        Context::from(uuid::Uuid::now_v7())
    }

    fn modify_entry(entry_id: u64, node_id: NodeID) -> LoreRevisionTreeModifyEntry {
        LoreRevisionTreeModifyEntry {
            entry_id,
            node_id,
            mode: 0o600,
            size: 4096,
            address: address(2, Context::default()),
        }
    }

    /// Add `name` under `parent` and return its node id.
    async fn seed(
        handle: LoreRevisionTree,
        parent: NodeID,
        name: &str,
        kind: LoreNodeType,
        address: Address,
    ) -> NodeID {
        let (sink, callback) = make_sink();
        let status = add(
            LoreGlobalArgs::default(),
            LoreRevisionTreeAddArgs {
                batch_id: 1,
                handle,
                entries: LoreArray::from_vec(vec![LoreRevisionTreeAddEntry {
                    entry_id: 1,
                    parent_node_id: parent,
                    parent_entry_index: 0,
                    name: LoreString::from_str(name),
                    kind: kind as u32,
                    mode: 0o644,
                    size: 10,
                    address,
                }]),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(status, 0, "seeding {name} must succeed, got {events:?}");
        events
            .iter()
            .find_map(|event| match event {
                Captured::AddComplete(_, node_id, LoreErrorCode::None) => Some(*node_id),
                _ => None,
            })
            .unwrap_or_else(|| panic!("seeding {name} must report a node id, got {events:?}"))
    }

    async fn run_modify(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeModifyEntry>,
    ) -> (i32, Vec<Captured>) {
        let (sink, callback) = make_sink();
        let status = modify(
            LoreGlobalArgs::default(),
            LoreRevisionTreeModifyArgs {
                batch_id: CALL_ID,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    /// Every per-entry terminal in emission order, so a test can pin which
    /// entries reported and which stayed silent.
    fn modify_completes(events: &[Captured]) -> Vec<(u64, NodeID, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                Captured::ModifyComplete(id, node_id, code) => Some((*id, *node_id, *code)),
                _ => None,
            })
            .collect()
    }

    /// The single rejected entry a failed batch is expected to report.
    fn rejected(id: u64) -> Vec<(u64, NodeID, LoreErrorCode)> {
        vec![(id, INVALID_NODE, LoreErrorCode::InvalidArguments)]
    }

    /// The content fields a test compares before and after an attempt.
    async fn content_of(handle: LoreRevisionTree, node_id: NodeID) -> (u16, u64, Address) {
        let info = node_info_of(handle, node_id).await;
        (info.mode, info.size, info.address)
    }

    /// Many independent files rewritten in one call. Every entry must land its
    /// own values regardless of which task ran it, and none may disturb its
    /// neighbours' fields, names, or place in the tree.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn modify_rewrites_every_entry_in_one_batch() {
        let handle = load_handle(Partition::from([0xC1u8; 16])).await;
        let parent = seed(
            handle,
            ROOT_NODE,
            "dir",
            LoreNodeType::Directory,
            Address::default(),
        )
        .await;

        const FILES: u64 = 64;
        let mut nodes = Vec::new();
        for index in 0..FILES {
            nodes.push(
                seed(
                    handle,
                    parent,
                    &format!("f{index}"),
                    LoreNodeType::File,
                    address(1, file_id()),
                )
                .await,
            );
        }

        let entries: Vec<_> = nodes
            .iter()
            .enumerate()
            .map(|(index, node_id)| LoreRevisionTreeModifyEntry {
                mode: 0o600,
                size: index as u64,
                address: address(100 + index as u64, Context::default()),
                ..modify_entry(index as u64 + 1, *node_id)
            })
            .collect();

        let (status, events) = run_modify(handle, entries).await;
        assert_eq!(status, 0, "got {events:?}");
        let mut reported = modify_completes(&events);
        reported.sort_by_key(|(id, _, _)| *id);
        let expected: Vec<_> = nodes
            .iter()
            .enumerate()
            .map(|(index, node_id)| (index as u64 + 1, *node_id, LoreErrorCode::None))
            .collect();
        assert_eq!(
            reported, expected,
            "every entry must report its own node exactly once"
        );

        for (index, node_id) in nodes.iter().enumerate() {
            let info = node_info_of(handle, *node_id).await;
            assert_eq!(
                (info.size, info.address.hash),
                (index as u64, Hash::from_u64(100 + index as u64)),
                "entry {index} must have landed its own values, got {info:?}"
            );
            assert_eq!(
                info.name.as_str(),
                format!("f{index}"),
                "modify must not disturb a node's name"
            );
            assert_eq!(
                info.parent_id, parent,
                "modify must not move a node between parents"
            );
        }

        let mut expected_names: Vec<String> = (0..FILES).map(|index| format!("f{index}")).collect();
        expected_names.sort();
        assert_eq!(
            child_names(handle, parent).await,
            expected_names,
            "modify touches no sibling chain, so the parent's children are unchanged"
        );
    }

    /// Node ids are allocated sequentially and a block holds a bounded number of
    /// them, so only a batch past that bound rewrites nodes lying in different
    /// blocks — which is the case where each entry's write takes a different
    /// block's lock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn modify_rewrites_nodes_across_several_node_blocks() {
        let handle = load_handle(Partition::from([0xCAu8; 16])).await;

        let count = BLOCK_NODE_COUNT * 3;
        let add_entries: Vec<LoreRevisionTreeAddEntry> = (0..count)
            .map(|index| LoreRevisionTreeAddEntry {
                entry_id: index as u64 + 1,
                parent_node_id: ROOT_NODE,
                parent_entry_index: 0,
                name: LoreString::from_str(&format!("f-{index:05}")),
                kind: LoreNodeType::File as u32,
                mode: 0o644,
                size: 10,
                address: address(1, file_id()),
            })
            .collect();
        let (sink, callback) = make_sink();
        let status = add(
            LoreGlobalArgs::default(),
            LoreRevisionTreeAddArgs {
                batch_id: 1,
                handle,
                entries: LoreArray::from_vec(add_entries),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "seeding three blocks of files must succeed");
        let mut nodes: Vec<(u64, NodeID)> = sink
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Captured::AddComplete(id, node_id, LoreErrorCode::None) => Some((*id, *node_id)),
                _ => None,
            })
            .collect();
        nodes.sort_by_key(|(id, _)| *id);
        assert_eq!(nodes.len(), count, "every seeded file must report");
        assert!(
            nodes.iter().any(|(_, node_id)| *node_id >= 512),
            "the seed must span more than one node block"
        );

        let entries: Vec<_> = nodes
            .iter()
            .enumerate()
            .map(|(index, (_, node_id))| LoreRevisionTreeModifyEntry {
                size: index as u64,
                address: address(1000 + index as u64, Context::default()),
                ..modify_entry(index as u64 + 1, *node_id)
            })
            .collect();

        let (status, events) = run_modify(handle, entries).await;
        assert_eq!(status, 0, "got {:?}", batch_outcomes(&events));
        assert_eq!(
            modify_completes(&events).len(),
            count,
            "every entry across every block must report"
        );

        for (index, (_, node_id)) in nodes.iter().enumerate() {
            let info = node_info_of(handle, *node_id).await;
            assert_eq!(
                (info.size, info.address.hash),
                (index as u64, Hash::from_u64(1000 + index as u64)),
                "entry {index} in block {} must hold its own values",
                node_id >> 9
            );
        }
    }

    /// Separate calls rewriting separate nodes run concurrently. Each node must
    /// end on its own call's values, with no write landing on another's target.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_batches_on_distinct_nodes_each_land() {
        let handle = load_handle(Partition::from([0xC2u8; 16])).await;

        const BATCHES: u64 = 4;
        const PER_BATCH: u64 = 16;
        let mut nodes = Vec::new();
        for index in 0..(BATCHES * PER_BATCH) {
            nodes.push(
                seed(
                    handle,
                    ROOT_NODE,
                    &format!("f{index}"),
                    LoreNodeType::File,
                    address(1, file_id()),
                )
                .await,
            );
        }

        let mut tasks: JoinSet<i32> = JoinSet::new();
        for batch in 0..BATCHES {
            let batch_nodes: Vec<NodeID> =
                nodes[(batch * PER_BATCH) as usize..((batch + 1) * PER_BATCH) as usize].to_vec();
            lore_spawn!(tasks, async move {
                let entries: Vec<_> = batch_nodes
                    .iter()
                    .enumerate()
                    .map(|(index, node_id)| {
                        let global = batch * PER_BATCH + index as u64;
                        LoreRevisionTreeModifyEntry {
                            size: global,
                            address: address(200 + global, Context::default()),
                            ..modify_entry(global + 1, *node_id)
                        }
                    })
                    .collect();
                run_modify(handle, entries).await.0
            });
        }
        while let Some(result) = tasks.join_next().await {
            assert_eq!(
                result.expect("batch task must not panic"),
                0,
                "every concurrent batch must succeed"
            );
        }

        for (index, node_id) in nodes.iter().enumerate() {
            let info = node_info_of(handle, *node_id).await;
            assert_eq!(
                (info.size, info.address.hash),
                (index as u64, Hash::from_u64(200 + index as u64)),
                "node {index} must hold the values its own batch wrote, got {info:?}"
            );
        }
    }

    /// The file id is the node's identity across revisions, so an edit that
    /// supplies none must keep it — generating one, as `add` does, would record
    /// the edit as a move.
    #[tokio::test]
    async fn modify_preserves_the_file_id_unless_the_caller_supplies_one() {
        let handle = load_handle(Partition::from([0xC3u8; 16])).await;

        let original = file_id();
        let kept = seed(
            handle,
            ROOT_NODE,
            "kept.bin",
            LoreNodeType::File,
            address(1, original),
        )
        .await;
        let replaced = seed(
            handle,
            ROOT_NODE,
            "replaced.bin",
            LoreNodeType::File,
            address(1, file_id()),
        )
        .await;

        let supplied = file_id();
        let (status, events) = run_modify(
            handle,
            vec![
                modify_entry(1, kept),
                LoreRevisionTreeModifyEntry {
                    address: address(2, supplied),
                    ..modify_entry(2, replaced)
                },
            ],
        )
        .await;
        assert_eq!(status, 0, "got {events:?}");

        assert_eq!(
            node_info_of(handle, kept).await.file_id,
            original,
            "a zero context must preserve the file id"
        );
        assert_eq!(
            node_info_of(handle, replaced).await.file_id,
            supplied,
            "a supplied file id must replace the existing one"
        );
    }

    /// One invalid entry rejects the whole batch: the valid entries ahead of it
    /// are not rewritten, only the offending entry reports, and the handle keeps
    /// working for the next batch.
    #[tokio::test]
    async fn a_rejected_batch_rewrites_nothing_and_leaves_the_handle_usable() {
        let handle = load_handle(Partition::from([0xC4u8; 16])).await;
        let first = seed(
            handle,
            ROOT_NODE,
            "a.bin",
            LoreNodeType::File,
            address(1, file_id()),
        )
        .await;
        let second = seed(
            handle,
            ROOT_NODE,
            "b.bin",
            LoreNodeType::File,
            address(1, file_id()),
        )
        .await;
        let before = content_of(handle, first).await;

        let (status, events) = run_modify(
            handle,
            vec![
                modify_entry(1, first),
                modify_entry(2, second),
                modify_entry(3, INVALID_NODE),
            ],
        )
        .await;
        assert_eq!(status, REJECTED_STATUS, "got {events:?}");
        assert_eq!(modify_completes(&events), rejected(3), "got {events:?}");
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::InvalidArguments)],
            "got {events:?}"
        );
        assert_eq!(
            content_of(handle, first).await,
            before,
            "an entry ahead of the rejected one must not have been applied"
        );

        let (status, events) = run_modify(handle, vec![modify_entry(4, first)]).await;
        assert_eq!(
            status, 0,
            "the handle must stay usable after a rejection, got {events:?}"
        );
    }

    /// Every way an entry names something it may not rewrite. Each is asserted on
    /// the exact rejection status, so a case that instead failed part-way through
    /// the apply phase fails the test.
    #[tokio::test]
    async fn modify_rejects_unknown_unallocated_and_non_file_targets() {
        let handle = load_handle(Partition::from([0xC5u8; 16])).await;
        let directory = seed(
            handle,
            ROOT_NODE,
            "dir",
            LoreNodeType::Directory,
            Address::default(),
        )
        .await;
        let link = seed(
            handle,
            ROOT_NODE,
            "link",
            LoreNodeType::Link,
            address(3, file_id()),
        )
        .await;

        for (node_id, what) in [
            (INVALID_NODE, "the invalid sentinel"),
            (400, "an unallocated slot"),
            (ROOT_NODE, "the root"),
            (directory, "a directory"),
            (link, "a link"),
        ] {
            let (status, events) = run_modify(handle, vec![modify_entry(9, node_id)]).await;
            assert_eq!(
                status, REJECTED_STATUS,
                "{what} must be rejected, got {events:?}"
            );
            assert_eq!(modify_completes(&events), rejected(9), "got {events:?}");
        }

        for (node_id, what) in [(directory, "a directory"), (link, "a link")] {
            let info = node_info_of(handle, node_id).await;
            assert_eq!(
                (info.mode, info.size),
                (0o644, 0),
                "{what} must be untouched by the refused attempts, got {info:?}"
            );
        }
    }

    /// Two entries naming one node do not say which value the caller meant, and a
    /// repeated non-zero caller id would make a reported id ambiguous. Both reject
    /// the batch rather than picking a winner.
    #[tokio::test]
    async fn modify_rejects_a_repeated_node_id_and_a_repeated_caller_id() {
        let handle = load_handle(Partition::from([0xC6u8; 16])).await;
        let first = seed(
            handle,
            ROOT_NODE,
            "a.bin",
            LoreNodeType::File,
            address(1, file_id()),
        )
        .await;
        let second = seed(
            handle,
            ROOT_NODE,
            "b.bin",
            LoreNodeType::File,
            address(1, file_id()),
        )
        .await;
        let before = content_of(handle, first).await;

        let (status, events) =
            run_modify(handle, vec![modify_entry(1, first), modify_entry(2, first)]).await;
        assert_eq!(status, REJECTED_STATUS, "got {events:?}");
        assert_eq!(modify_completes(&events), rejected(2), "got {events:?}");
        assert_eq!(
            content_of(handle, first).await,
            before,
            "a rejected batch must leave every field unchanged"
        );

        let (status, events) = run_modify(
            handle,
            vec![modify_entry(5, first), modify_entry(5, second)],
        )
        .await;
        assert_eq!(status, REJECTED_STATUS, "got {events:?}");
        assert_eq!(modify_completes(&events), rejected(5), "got {events:?}");

        // Zero is an explicit "not correlating this entry", so it may repeat.
        let (status, events) = run_modify(
            handle,
            vec![modify_entry(0, first), modify_entry(0, second)],
        )
        .await;
        assert_eq!(status, 0, "got {events:?}");
        assert_eq!(
            modify_completes(&events).len(),
            2,
            "both entries must report under the shared zero id, got {events:?}"
        );
    }

    /// A closed handle is the call's failure, not any entry's: it reports on the
    /// batch terminal, which carries the call id, and leaves every entry silent.
    #[tokio::test]
    async fn modify_on_a_closed_handle_reports_only_the_batch_terminal() {
        let handle = load_handle(Partition::from([0xC7u8; 16])).await;
        let node_id = seed(
            handle,
            ROOT_NODE,
            "a.bin",
            LoreNodeType::File,
            address(1, file_id()),
        )
        .await;
        close_handle(handle).await;

        let (status, events) = run_modify(
            handle,
            vec![modify_entry(7, node_id), modify_entry(8, node_id)],
        )
        .await;

        assert_ne!(status, 0, "a closed handle must fail the call");
        assert!(
            modify_completes(&events).is_empty(),
            "no entry may report when the call itself failed, got {events:?}"
        );
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::InvalidArguments)],
            "got {events:?}"
        );
        assert!(events.contains(&Captured::Complete(status)));
    }

    /// An empty batch is a no-op, but the call still reports once so a caller
    /// waiting on the batch terminal is not left hanging.
    #[tokio::test]
    async fn modify_with_no_entries_reports_the_batch_terminal() {
        let handle = load_handle(Partition::from([0xC8u8; 16])).await;

        let (status, events) = run_modify(handle, Vec::new()).await;
        assert_eq!(status, 0, "got {events:?}");
        assert!(
            modify_completes(&events).is_empty(),
            "no entry terminal may fire for an empty batch, got {events:?}"
        );
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::None)],
            "got {events:?}"
        );
    }

    /// Adding a file and rewriting it in the same handle is the canonical
    /// pipeline shape, and the node must carry the second call's content while
    /// keeping the identity the first gave it.
    #[tokio::test]
    async fn a_file_added_then_modified_reads_back_with_the_new_content() {
        let handle = load_handle(Partition::from([0xC9u8; 16])).await;
        let node_id = seed(
            handle,
            ROOT_NODE,
            "payload.bin",
            LoreNodeType::File,
            Address::default(),
        )
        .await;

        let generated = node_info_of(handle, node_id).await.file_id;
        assert_ne!(
            generated,
            Context::default(),
            "add must have assigned a file id"
        );

        let (status, events) = run_modify(handle, vec![modify_entry(1, node_id)]).await;
        assert_eq!(status, 0, "got {events:?}");

        let info = node_info_of(handle, node_id).await;
        assert_eq!(info.mode, 0o600, "got {info:?}");
        assert_eq!(info.size, 4096, "got {info:?}");
        assert_eq!(info.address.hash, Hash::from_u64(2), "got {info:?}");
        assert_eq!(
            info.file_id, generated,
            "the file id add generated must survive the rewrite"
        );
        assert_eq!(
            parent_of(handle, node_id).await,
            ROOT_NODE,
            "the node must stay where add put it"
        );
    }
}

#[cfg(test)]
mod metadata_tests {
    use lore::revision_tree::handle::LoreRevisionTree;
    use lore::revision_tree::metadata_get::LoreRevisionTreeMetadataGetArgs;
    use lore::revision_tree::metadata_get::LoreRevisionTreeMetadataGetEntry;
    use lore::revision_tree::metadata_get::metadata_get;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
    use lore::revision_tree::metadata_set::metadata_set;
    use lore_base::lore_spawn;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::LoreErrorCode;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreBinary;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreMetadata;
    use lore_revision::interface::LoreString;
    use tokio::task::JoinSet;

    use super::support::*;

    fn set_entry(entry_id: u64, key: &str, value: &str) -> LoreRevisionTreeMetadataSetEntry {
        LoreRevisionTreeMetadataSetEntry {
            entry_id,
            key: LoreString::from_str(key),
            value: LoreMetadata::String(LoreString::from_str(value)),
        }
    }

    fn get_entry(entry_id: u64, key: &str) -> LoreRevisionTreeMetadataGetEntry {
        LoreRevisionTreeMetadataGetEntry {
            entry_id,
            key: LoreString::from_str(key),
        }
    }

    async fn run_set(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeMetadataSetEntry>,
    ) -> (i32, Vec<Captured>) {
        let (sink, callback) = make_sink();
        let status = metadata_set(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataSetArgs {
                batch_id: CALL_ID,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    async fn run_get(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeMetadataGetEntry>,
    ) -> (i32, Vec<Captured>) {
        let (sink, callback) = make_sink();
        let status = metadata_get(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataGetArgs {
                batch_id: CALL_ID,
                handle,
                include_revision: 0,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    fn set_completes(events: &[Captured]) -> Vec<(u64, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                Captured::MetadataSetComplete(id, code) => Some((*id, *code)),
                _ => None,
            })
            .collect()
    }

    fn get_completes(events: &[Captured]) -> Vec<(u64, String, LoreMetadata, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                Captured::MetadataGetComplete(id, key, value, code) => {
                    Some((*id, key.clone(), value.clone(), *code))
                }
                _ => None,
            })
            .collect()
    }

    /// A value of each kind, chosen so no two entries in the batch share one.
    fn value_for(index: u64) -> LoreMetadata {
        match index % 7 {
            0 => LoreMetadata::String(LoreString::from_str(&format!("value-{index}"))),
            1 => LoreMetadata::Numeric(index),
            2 => LoreMetadata::Boolean(u8::from(index.is_multiple_of(2))),
            3 => LoreMetadata::Binary(LoreBinary::from_bytes(&index.to_le_bytes())),
            4 => LoreMetadata::Hash(Hash::from_u64(index)),
            5 => LoreMetadata::Context(Context::from([index as u8; 16])),
            _ => LoreMetadata::Address(Address {
                hash: Hash::from_u64(index),
                context: Context::from([index as u8; 16]),
            }),
        }
    }

    /// A batch larger than a handful, over a real store, asserting every key
    /// lands and reads back under its own entry id — carrying the kind it was
    /// written with, since a typed value is what these verbs exchange.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_large_metadata_batch_lands_every_key() {
        let handle = load_handle(Partition::from([0xD1u8; 16])).await;

        const KEYS: u64 = 256;
        let entries: Vec<_> = (0..KEYS)
            .map(|index| LoreRevisionTreeMetadataSetEntry {
                entry_id: index + 1,
                key: LoreString::from_str(&format!("key-{index:04}")),
                value: value_for(index),
            })
            .collect();
        let (status, events) = run_set(handle, entries).await;
        assert_eq!(status, 0, "got {:?}", batch_outcomes(&events));
        assert_eq!(
            set_completes(&events).len() as u64,
            KEYS,
            "every entry must report"
        );

        let reads: Vec<_> = (0..KEYS)
            .map(|index| get_entry(index + 1, &format!("key-{index:04}")))
            .collect();
        let (status, events) = run_get(handle, reads).await;
        assert_eq!(status, 0, "got {:?}", batch_outcomes(&events));
        let mut reported = get_completes(&events);
        reported.sort_by_key(|(id, _, _, _)| *id);
        let expected: Vec<_> = (0..KEYS)
            .map(|index| {
                (
                    index + 1,
                    format!("key-{index:04}"),
                    value_for(index),
                    LoreErrorCode::None,
                )
            })
            .collect();
        assert_eq!(
            reported, expected,
            "every key must read back in order, under the kind it was set with"
        );
    }

    /// Separate calls on one handle each take the pending-metadata write lock, so
    /// every batch must land whole — no batch may lose keys to another.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_set_batches_each_land_whole() {
        let handle = load_handle(Partition::from([0xD2u8; 16])).await;

        const BATCHES: u64 = 4;
        const PER_BATCH: u64 = 16;
        let mut tasks: JoinSet<i32> = JoinSet::new();
        for batch in 0..BATCHES {
            lore_spawn!(tasks, async move {
                let entries: Vec<_> = (0..PER_BATCH)
                    .map(|index| {
                        let global = batch * PER_BATCH + index;
                        set_entry(global + 1, &format!("b{batch}-k{index}"), "value")
                    })
                    .collect();
                run_set(handle, entries).await.0
            });
        }
        while let Some(result) = tasks.join_next().await {
            assert_eq!(
                result.expect("batch task must not panic"),
                0,
                "every concurrent batch must succeed"
            );
        }

        let reads: Vec<_> = (0..BATCHES)
            .flat_map(|batch| {
                (0..PER_BATCH).map(move |index| {
                    let global = batch * PER_BATCH + index;
                    get_entry(global + 1, &format!("b{batch}-k{index}"))
                })
            })
            .collect();
        let (status, events) = run_get(handle, reads).await;
        assert_eq!(status, 0, "got {:?}", batch_outcomes(&events));
        assert_eq!(
            get_completes(&events).len() as u64,
            BATCHES * PER_BATCH,
            "no key may be lost to a concurrent batch"
        );
    }

    /// A closed handle is the call's failure, not any entry's, for both verbs:
    /// it reports on the batch terminal and leaves every entry silent.
    #[tokio::test]
    async fn metadata_verbs_on_a_closed_handle_report_only_the_batch_terminal() {
        let handle = load_handle(Partition::from([0xD3u8; 16])).await;
        close_handle(handle).await;

        let (status, events) = run_set(handle, vec![set_entry(7, "a", "1")]).await;
        assert_ne!(status, 0, "a closed handle must fail the set");
        assert!(set_completes(&events).is_empty(), "got {events:?}");
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::InvalidArguments)]
        );

        let (status, events) = run_get(handle, vec![get_entry(8, "a")]).await;
        assert_ne!(status, 0, "a closed handle must fail the get");
        assert!(get_completes(&events).is_empty(), "got {events:?}");
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::InvalidArguments)]
        );
    }

    /// A read mixing present and absent keys reports only the present ones and
    /// still succeeds — the one batch verb that tolerates a key it cannot answer.
    #[tokio::test]
    async fn a_read_mixing_present_and_absent_keys_succeeds() {
        let handle = load_handle(Partition::from([0xD4u8; 16])).await;
        run_set(handle, vec![set_entry(1, "here", "yes")]).await;

        let (status, events) = run_get(
            handle,
            vec![
                get_entry(10, "missing-one"),
                get_entry(11, "here"),
                get_entry(12, "missing-two"),
            ],
        )
        .await;
        assert_eq!(status, 0, "absent keys must not fail the call");
        assert_eq!(
            get_completes(&events),
            vec![(
                11,
                "here".to_string(),
                LoreMetadata::String(LoreString::from_str("yes")),
                LoreErrorCode::None
            )],
            "only the present key reports"
        );
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::None)]
        );
    }
}

#[cfg(test)]
mod metadata_clear_tests {
    use lore::revision_tree::handle::LoreRevisionTree;
    use lore::revision_tree::metadata_clear::LoreRevisionTreeMetadataClearArgs;
    use lore::revision_tree::metadata_clear::LoreRevisionTreeMetadataClearEntry;
    use lore::revision_tree::metadata_clear::metadata_clear;
    use lore::revision_tree::metadata_get::LoreRevisionTreeMetadataGetArgs;
    use lore::revision_tree::metadata_get::LoreRevisionTreeMetadataGetEntry;
    use lore::revision_tree::metadata_get::metadata_get;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
    use lore::revision_tree::metadata_set::metadata_set;
    use lore_base::types::Partition;
    use lore_revision::event::LoreErrorCode;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreMetadata;
    use lore_revision::interface::LoreString;

    use super::support::*;

    async fn seed(handle: LoreRevisionTree, keys: &[&str]) {
        let (_, callback) = make_sink();
        let entries: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(index, key)| LoreRevisionTreeMetadataSetEntry {
                entry_id: index as u64 + 1,
                key: LoreString::from_str(key),
                value: LoreMetadata::String(LoreString::from_str("value")),
            })
            .collect();
        let status = metadata_set(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataSetArgs {
                batch_id: 1,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "seeding metadata must succeed");
    }

    async fn run_clear(handle: LoreRevisionTree, keys: &[&str]) -> (i32, Vec<Captured>) {
        let (sink, callback) = make_sink();
        let entries: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(index, key)| LoreRevisionTreeMetadataClearEntry {
                entry_id: index as u64 + 1,
                key: LoreString::from_str(key),
            })
            .collect();
        let status = metadata_clear(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataClearArgs {
                batch_id: CALL_ID,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    async fn keys_present(handle: LoreRevisionTree, keys: &[&str]) -> Vec<String> {
        let (sink, callback) = make_sink();
        let entries: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(index, key)| LoreRevisionTreeMetadataGetEntry {
                entry_id: index as u64 + 1,
                key: LoreString::from_str(key),
            })
            .collect();
        metadata_get(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataGetArgs {
                batch_id: 2,
                handle,
                include_revision: 0,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        sink.lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Captured::MetadataGetComplete(_, key, _, _) => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    fn clear_completes(events: &[Captured]) -> Vec<(u64, u8, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                Captured::MetadataClearComplete(id, removed, code) => Some((*id, *removed, *code)),
                _ => None,
            })
            .collect()
    }

    /// The full cycle over a real store: set, clear a subset, and read back to
    /// confirm exactly the cleared keys are gone.
    #[tokio::test]
    async fn set_then_clear_leaves_only_the_untouched_keys() {
        let handle = load_handle(Partition::from([0xE1u8; 16])).await;
        seed(handle, &["alpha", "beta", "gamma"]).await;

        let (status, events) = run_clear(handle, &["alpha", "gamma"]).await;
        assert_eq!(status, 0, "got {:?}", batch_outcomes(&events));
        assert_eq!(
            clear_completes(&events),
            vec![(1, 1, LoreErrorCode::None), (2, 1, LoreErrorCode::None)],
            "both keys were present and are reported removed"
        );
        assert_eq!(
            keys_present(handle, &["alpha", "beta", "gamma"]).await,
            vec!["beta".to_string()],
            "only the untouched key still reads back"
        );
    }

    /// A larger batch mixing present and absent keys, asserting the no-op is a
    /// success carrying `removed = 0`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_large_clear_batch_reports_each_key_it_did_and_did_not_remove() {
        let handle = load_handle(Partition::from([0xE2u8; 16])).await;
        let present: Vec<String> = (0..64).map(|index| format!("key-{index:04}")).collect();
        let present_refs: Vec<&str> = present.iter().map(String::as_str).collect();
        seed(handle, &present_refs).await;

        let absent: Vec<String> = (0..64).map(|index| format!("gone-{index:04}")).collect();
        let mut all: Vec<&str> = present_refs.clone();
        all.extend(absent.iter().map(String::as_str));

        let (status, events) = run_clear(handle, &all).await;
        assert_eq!(status, 0, "got {:?}", batch_outcomes(&events));
        let outcomes = clear_completes(&events);
        assert_eq!(outcomes.len(), 128, "every entry must report");
        assert_eq!(
            outcomes
                .iter()
                .filter(|(_, removed, _)| *removed == 1)
                .count(),
            64,
            "exactly the seeded keys report a removal"
        );
        assert!(
            outcomes
                .iter()
                .all(|(_, _, code)| *code == LoreErrorCode::None),
            "an absent key is a no-op success, not a failure"
        );
        assert!(
            keys_present(handle, &present_refs).await.is_empty(),
            "every seeded key must be gone"
        );
    }

    /// A closed handle is the call's failure, not any entry's.
    #[tokio::test]
    async fn clear_on_a_closed_handle_reports_only_the_batch_terminal() {
        let handle = load_handle(Partition::from([0xE3u8; 16])).await;
        close_handle(handle).await;

        let (status, events) = run_clear(handle, &["a"]).await;
        assert_ne!(status, 0, "a closed handle must fail the call");
        assert!(clear_completes(&events).is_empty(), "got {events:?}");
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::InvalidArguments)]
        );
    }
}

#[cfg(test)]
mod delete_tests {
    use lore::revision_tree::add::LoreRevisionTreeAddArgs;
    use lore::revision_tree::add::LoreRevisionTreeAddEntry;
    use lore::revision_tree::add::add;
    use lore::revision_tree::delete::LoreRevisionTreeDeleteArgs;
    use lore::revision_tree::delete::LoreRevisionTreeDeleteEntry;
    use lore::revision_tree::delete::delete;
    use lore::revision_tree::handle::LoreRevisionTree;
    use lore_base::types::Address;
    use lore_base::types::Partition;
    use lore_revision::event::LoreErrorCode;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreNodeType;
    use lore_revision::interface::LoreString;
    use lore_revision::node::BLOCK_NODE_COUNT;
    use lore_revision::node::INVALID_NODE;
    use lore_revision::node::NodeID;
    use lore_revision::node::ROOT_NODE;

    use super::support::*;

    /// A handle can only reach nodes it added itself until `commit` exists, and
    /// clearing the staging flags to fake a settled node needs
    /// `RevisionTreeInternal`, which is `pub(crate)`. So these tests exercise the
    /// discard half of the verb — the phase that rewrites sibling pointers over a
    /// real store — and the staged half is covered by the unit tests in
    /// `lore/src/revision_tree/delete.rs`.
    fn add_entry(
        entry_id: u64,
        parent_node_id: NodeID,
        name: &str,
        kind: LoreNodeType,
    ) -> LoreRevisionTreeAddEntry {
        LoreRevisionTreeAddEntry {
            entry_id,
            parent_node_id,
            parent_entry_index: 0,
            name: LoreString::from_str(name),
            kind: kind as u32,
            mode: 0o644,
            size: 0,
            address: Address::default(),
        }
    }

    fn nested_add_entry(
        entry_id: u64,
        parent_entry_index: u32,
        name: &str,
        kind: LoreNodeType,
    ) -> LoreRevisionTreeAddEntry {
        LoreRevisionTreeAddEntry {
            parent_node_id: INVALID_NODE,
            parent_entry_index,
            ..add_entry(entry_id, ROOT_NODE, name, kind)
        }
    }

    fn entry(entry_id: u64, node_id: NodeID) -> LoreRevisionTreeDeleteEntry {
        LoreRevisionTreeDeleteEntry { entry_id, node_id }
    }

    async fn run_add(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeAddEntry>,
    ) -> Vec<Captured> {
        let (sink, callback) = make_sink();
        let status = add(
            LoreGlobalArgs::default(),
            LoreRevisionTreeAddArgs {
                batch_id: 1,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "seeding the tree must succeed");
        sink.lock().unwrap().clone()
    }

    async fn run_delete(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeDeleteEntry>,
    ) -> (i32, Vec<Captured>) {
        let (sink, callback) = make_sink();
        let status = delete(
            LoreGlobalArgs::default(),
            LoreRevisionTreeDeleteArgs {
                batch_id: CALL_ID,
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
    fn delete_completes(events: &[Captured]) -> Vec<(u64, u64, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                Captured::DeleteComplete(id, count, code) => Some((*id, *count, *code)),
                _ => None,
            })
            .collect()
    }

    /// A subtree this handle added leaves the tree outright, and the nodes it held
    /// stop being reachable through the listing.
    #[tokio::test]
    async fn delete_removes_an_added_subtree_over_a_real_store() {
        let handle = load_handle(Partition::from([0x71u8; 16])).await;

        let seeded = run_add(
            handle,
            vec![
                add_entry(1, ROOT_NODE, "dir", LoreNodeType::Directory),
                nested_add_entry(2, 0, "a.bin", LoreNodeType::File),
                nested_add_entry(3, 0, "b.bin", LoreNodeType::File),
                add_entry(4, ROOT_NODE, "kept.bin", LoreNodeType::File),
            ],
        )
        .await;
        let directory = added_node(&seeded, 1);
        assert_eq!(
            child_names(handle, ROOT_NODE).await,
            vec!["dir".to_string(), "kept.bin".to_string()],
            "the fixture must be in place before the deletion"
        );

        let (status, events) = run_delete(handle, vec![entry(10, directory)]).await;
        assert_eq!(status, 0, "deleting an added subtree must succeed");
        assert_eq!(
            delete_completes(&events),
            vec![(10, 3, LoreErrorCode::None)],
            "the directory and both children must be counted"
        );
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::None)],
            "the batch terminal must fire exactly once"
        );
        assert_eq!(
            child_names(handle, ROOT_NODE).await,
            vec!["kept.bin".to_string()],
            "only the untouched sibling may remain"
        );
        close_handle(handle).await;
    }

    /// Two disjoint subtrees in one call, each reporting its own count. The
    /// discard phase rewrites sibling pointers serially, so this is where a
    /// mis-ordered unlink would strand a node.
    #[tokio::test]
    async fn delete_removes_two_disjoint_subtrees_in_one_call() {
        let handle = load_handle(Partition::from([0x72u8; 16])).await;

        let seeded = run_add(
            handle,
            vec![
                add_entry(1, ROOT_NODE, "left", LoreNodeType::Directory),
                nested_add_entry(2, 0, "a.bin", LoreNodeType::File),
                add_entry(3, ROOT_NODE, "right", LoreNodeType::Directory),
                nested_add_entry(4, 2, "b.bin", LoreNodeType::File),
                nested_add_entry(5, 2, "c.bin", LoreNodeType::File),
                add_entry(6, ROOT_NODE, "kept.bin", LoreNodeType::File),
            ],
        )
        .await;
        let left = added_node(&seeded, 1);
        let right = added_node(&seeded, 3);

        let (status, events) = run_delete(handle, vec![entry(10, left), entry(11, right)]).await;
        assert_eq!(status, 0, "deleting two subtrees must succeed");
        let mut reported = delete_completes(&events);
        reported.sort_by_key(|(id, _, _)| *id);
        assert_eq!(
            reported,
            vec![(10, 2, LoreErrorCode::None), (11, 3, LoreErrorCode::None),],
            "each entry must report the size of its own subtree"
        );
        assert_eq!(
            child_names(handle, ROOT_NODE).await,
            vec!["kept.bin".to_string()],
            "both subtrees must be gone and the sibling untouched"
        );
        close_handle(handle).await;
    }

    /// Deleting frees the name, so the same path can be rebuilt in the same
    /// handle — the case a caller replacing a directory hits.
    #[tokio::test]
    async fn a_name_freed_by_delete_can_be_added_again() {
        let handle = load_handle(Partition::from([0x73u8; 16])).await;

        let seeded = run_add(
            handle,
            vec![add_entry(1, ROOT_NODE, "thing", LoreNodeType::Directory)],
        )
        .await;
        let first = added_node(&seeded, 1);

        let (status, _) = run_delete(handle, vec![entry(10, first)]).await;
        assert_eq!(status, 0, "deleting must succeed");

        let seeded = run_add(
            handle,
            vec![add_entry(2, ROOT_NODE, "thing", LoreNodeType::File)],
        )
        .await;
        let second = added_node(&seeded, 2);
        assert_eq!(
            child_names(handle, ROOT_NODE).await,
            vec!["thing".to_string()],
            "the rebuilt node must be the only child"
        );
        assert_eq!(
            node_info_of(handle, second).await.kind,
            LoreNodeType::File as u32,
            "the rebuilt node must carry its own kind, not the deleted node's"
        );
        close_handle(handle).await;
    }

    /// Validation covers the whole batch before anything is touched, and the
    /// handle stays usable afterwards.
    #[tokio::test]
    async fn delete_rejects_the_batch_and_leaves_the_tree_intact() {
        let handle = load_handle(Partition::from([0x74u8; 16])).await;

        let seeded = run_add(
            handle,
            vec![
                add_entry(1, ROOT_NODE, "dir", LoreNodeType::Directory),
                nested_add_entry(2, 0, "a.bin", LoreNodeType::File),
            ],
        )
        .await;
        let directory = added_node(&seeded, 1);
        let leaf = added_node(&seeded, 2);

        for (entries, reason) in [
            (vec![entry(10, ROOT_NODE)], "the root"),
            (vec![entry(10, INVALID_NODE)], "an unknown node"),
            (
                vec![entry(10, directory), entry(11, directory)],
                "a repeated node",
            ),
            (
                vec![entry(10, directory), entry(11, leaf)],
                "an entry under another entry",
            ),
        ] {
            let (status, events) = run_delete(handle, entries).await;
            assert_eq!(
                status, REJECTED_STATUS,
                "{reason} must reject the batch during validation"
            );
            assert_eq!(
                batch_outcomes(&events),
                vec![(CALL_ID, LoreErrorCode::InvalidArguments)],
                "{reason} must report once on the batch terminal"
            );
            assert_eq!(
                child_names(handle, ROOT_NODE).await,
                vec!["dir".to_string()],
                "{reason} must leave the tree as it was"
            );
        }

        let (status, _) = run_delete(handle, vec![entry(12, directory)]).await;
        assert_eq!(
            status, 0,
            "the handle must stay usable after a rejected batch"
        );
        close_handle(handle).await;
    }

    /// A block holds `BLOCK_NODE_COUNT` nodes and ids are handed out in sequence,
    /// so a subtree has to exceed that count before the walk crosses a block
    /// boundary and a level spreads over every task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn delete_removes_more_nodes_than_one_block_holds() {
        let handle = load_handle(Partition::from([0x75u8; 16])).await;

        let children = 3 * BLOCK_NODE_COUNT;
        let mut entries = vec![add_entry(1, ROOT_NODE, "dir", LoreNodeType::Directory)];
        for index in 0..children {
            entries.push(nested_add_entry(
                index as u64 + 2,
                0,
                &format!("f{index}.bin"),
                LoreNodeType::File,
            ));
        }
        let seeded = run_add(handle, entries).await;
        let directory = added_node(&seeded, 1);

        let (status, events) = run_delete(handle, vec![entry(10, directory)]).await;
        assert_eq!(status, 0, "deleting a multi-block subtree must succeed");
        assert_eq!(
            delete_completes(&events),
            vec![(10, children as u64 + 1, LoreErrorCode::None)],
            "every node across every block must be counted"
        );
        assert!(
            child_names(handle, ROOT_NODE).await.is_empty(),
            "the whole subtree must be gone"
        );
        close_handle(handle).await;
    }

    /// A caller must be able to treat the batch terminal as the end of the call,
    /// which only holds if it fires after every entry and before `Complete`.
    #[tokio::test]
    async fn delete_reports_entries_then_the_batch_terminal_then_complete() {
        let handle = load_handle(Partition::from([0x76u8; 16])).await;

        let seeded = run_add(
            handle,
            vec![
                add_entry(1, ROOT_NODE, "a.bin", LoreNodeType::File),
                add_entry(2, ROOT_NODE, "b.bin", LoreNodeType::File),
            ],
        )
        .await;
        let first = added_node(&seeded, 1);
        let second = added_node(&seeded, 2);

        let (_, events) = run_delete(handle, vec![entry(10, first), entry(11, second)]).await;
        let last_entry_at = events
            .iter()
            .rposition(|event| matches!(event, Captured::DeleteComplete(..)))
            .expect("both entries must report");
        let batch_at = events
            .iter()
            .position(|event| matches!(event, Captured::BatchComplete(..)))
            .expect("the batch terminal must fire");
        let complete_at = events
            .iter()
            .position(|event| matches!(event, Captured::Complete(..)))
            .expect("Complete must fire");
        assert!(
            last_entry_at < batch_at && batch_at < complete_at,
            "order must be entries, then the batch terminal, then Complete: {events:?}"
        );
        close_handle(handle).await;
    }
}

/// What `commit` unblocks for the rest of the namespace: until it existed nothing
/// this API built had ever been serialized, so every test asserted in-memory state.
/// These read a published revision back through a fresh handle.
#[cfg(test)]
mod commit_tests {
    use lore::revision_tree::add::LoreRevisionTreeAddArgs;
    use lore::revision_tree::add::LoreRevisionTreeAddEntry;
    use lore::revision_tree::add::add;
    use lore::revision_tree::commit::LoreRevisionTreeCommitArgs;
    use lore::revision_tree::commit::LoreRevisionTreeCommitOptions;
    use lore::revision_tree::commit::commit;
    use lore::revision_tree::handle::LoreRevisionTree;
    use lore::revision_tree::info::LoreRevisionTreeInfoArgs;
    use lore::revision_tree::info::info;
    use lore::revision_tree::metadata_get::LoreRevisionTreeMetadataGetArgs;
    use lore::revision_tree::metadata_get::LoreRevisionTreeMetadataGetEntry;
    use lore::revision_tree::metadata_get::metadata_get;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
    use lore::revision_tree::metadata_set::metadata_set;
    use lore::revision_tree::resolve_path::LoreRevisionTreeResolvePathArgs;
    use lore::revision_tree::resolve_path::resolve_path;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::LoreErrorCode;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreMetadata;
    use lore_revision::interface::LoreNodeType;
    use lore_revision::interface::LoreString;
    use lore_revision::metadata::BRANCH;
    use lore_revision::metadata::MESSAGE;
    use lore_revision::node::INVALID_NODE;
    use lore_revision::node::NodeID;
    use lore_revision::node::ROOT_NODE;

    use super::support::*;

    fn add_entry(
        entry_id: u64,
        parent_entry_index: u32,
        name: &str,
        kind: LoreNodeType,
        nested: bool,
    ) -> LoreRevisionTreeAddEntry {
        LoreRevisionTreeAddEntry {
            entry_id,
            parent_node_id: if nested { INVALID_NODE } else { ROOT_NODE },
            parent_entry_index,
            name: LoreString::from_str(name),
            kind: kind as u32,
            mode: if kind == LoreNodeType::Directory {
                0o755
            } else {
                0o644
            },
            size: if kind == LoreNodeType::File { 12 } else { 0 },
            address: if kind == LoreNodeType::File {
                Address {
                    hash: Hash::from_u64(0xc0ffee),
                    context: Context::from(uuid::Uuid::now_v7()),
                }
            } else {
                Address::default()
            },
        }
    }

    async fn run_add(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeAddEntry>,
    ) -> Vec<Captured> {
        let (sink, callback) = make_sink();
        let status = add(
            LoreGlobalArgs::default(),
            LoreRevisionTreeAddArgs {
                batch_id: CALL_ID,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(status, 0, "seeding the tree must succeed, got {events:?}");
        events
    }

    fn added_node(events: &[Captured], entry_id: u64) -> NodeID {
        events
            .iter()
            .find_map(|event| match event {
                Captured::AddComplete(id, node_id, code) if *id == entry_id => {
                    assert_eq!(*code, LoreErrorCode::None, "entry {entry_id} must succeed");
                    Some(*node_id)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("entry {entry_id} must report, got {events:?}"))
    }

    async fn set_metadata(handle: LoreRevisionTree, pairs: Vec<(&str, LoreMetadata)>) {
        let entries = pairs
            .into_iter()
            .enumerate()
            .map(|(index, (key, value))| LoreRevisionTreeMetadataSetEntry {
                entry_id: index as u64 + 1,
                key: LoreString::from_str(key),
                value,
            })
            .collect();
        let (sink, callback) = make_sink();
        let status = metadata_set(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataSetArgs {
                batch_id: CALL_ID,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        assert_eq!(
            status,
            0,
            "setting metadata must succeed, got {:?}",
            sink.lock().unwrap()
        );
    }

    async fn run_commit(handle: LoreRevisionTree) -> (i32, Hash, Vec<Captured>) {
        let (sink, callback) = make_sink();
        let status = commit(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCommitArgs {
                id: CALL_ID,
                handle,
                options: LoreRevisionTreeCommitOptions::default(),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        let revision = events
            .iter()
            .find_map(|event| match event {
                Captured::CommitComplete(id, revision, _, _) if *id == CALL_ID => Some(*revision),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the commit terminal must fire, got {events:?}"));
        (status, revision, events)
    }

    async fn resolve(handle: LoreRevisionTree, path: &str) -> NodeID {
        let (sink, callback) = make_sink();
        let status = resolve_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeResolvePathArgs {
                id: 1,
                handle,
                path: LoreString::from_str(path),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(status, 0, "resolving {path} must succeed, got {events:?}");
        events
            .iter()
            .find_map(|event| match event {
                Captured::ResolvePath(node_id, LoreErrorCode::None) => Some(*node_id),
                _ => None,
            })
            .unwrap_or_else(|| panic!("resolving {path} must report a node, got {events:?}"))
    }

    /// The read-back a seeded handle could never do: a revision written by this API,
    /// loaded again from its hash, has to hold exactly the tree that was committed.
    #[tokio::test]
    async fn a_committed_revision_reads_back_through_a_fresh_handle() {
        let repository = Partition::from([0xF1u8; 16]);
        let store = open_store().await;
        let handle = load_on(store, repository, Hash::default()).await;

        let seeded = run_add(
            handle,
            vec![
                add_entry(1, 0, "dir", LoreNodeType::Directory, false),
                add_entry(2, 0, "nested", LoreNodeType::Directory, true),
                add_entry(3, 1, "a.bin", LoreNodeType::File, true),
                add_entry(4, 0, "top.bin", LoreNodeType::File, false),
            ],
        )
        .await;
        let leaf = added_node(&seeded, 3);
        let leaf_file_id = node_info_of(handle, leaf).await.file_id;

        set_metadata(
            handle,
            vec![(
                BRANCH,
                LoreMetadata::Context(Context::from(uuid::Uuid::now_v7())),
            )],
        )
        .await;
        let (status, revision, events) = run_commit(handle).await;
        assert_eq!(status, 0, "committing must succeed, got {events:?}");
        close_handle(handle).await;

        let reloaded = load_on(store, repository, revision).await;
        assert_eq!(
            child_names(reloaded, ROOT_NODE).await,
            vec!["dir".to_string(), "top.bin".to_string()],
            "the committed root must hold what was added"
        );
        let reloaded_leaf = resolve(reloaded, "dir/nested/a.bin").await;
        assert_eq!(
            reloaded_leaf, leaf,
            "node ids persist across a commit, so the path must resolve to the same node"
        );
        let record = node_info_of(reloaded, reloaded_leaf).await;
        assert_eq!(
            record.revision, revision,
            "a node read from the reloaded handle must report the committed revision"
        );
        // The file id is the node's identity across revisions, so it is the field a
        // round-trip has to preserve — the node id being right proves little.
        assert_eq!(
            record.file_id, leaf_file_id,
            "the committed node must keep the identity it was added with"
        );
        assert_eq!(
            record.staged_action, 0,
            "a committed node carries no staged action, got {record:?}"
        );

        close_handle(reloaded).await;
    }

    /// Metadata set on a handle is only observable in-memory until a commit writes
    /// it, which is why both of these read it back from the published revision.
    #[tokio::test]
    async fn a_committed_revision_carries_the_metadata_it_was_given() {
        let repository = Partition::from([0xF2u8; 16]);
        let store = open_store().await;
        let handle = load_on(store, repository, Hash::default()).await;
        let branch = Context::from(uuid::Uuid::now_v7());

        run_add(
            handle,
            vec![add_entry(1, 0, "a.bin", LoreNodeType::File, false)],
        )
        .await;
        set_metadata(
            handle,
            vec![
                (BRANCH, LoreMetadata::Context(branch)),
                (
                    MESSAGE,
                    LoreMetadata::String(LoreString::from_str("import")),
                ),
                ("build", LoreMetadata::Numeric(42)),
            ],
        )
        .await;
        let (status, revision, events) = run_commit(handle).await;
        assert_eq!(status, 0, "committing must succeed, got {events:?}");
        close_handle(handle).await;

        let reloaded = load_on(store, repository, revision).await;

        let (sink, callback) = make_sink();
        let status = info(
            LoreGlobalArgs::default(),
            LoreRevisionTreeInfoArgs {
                id: 1,
                handle: reloaded,
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(
            status, 0,
            "reading revision info must succeed, got {events:?}"
        );
        let record = events
            .iter()
            .find_map(|event| match event {
                Captured::Info(data) => Some((**data).clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("info must report the revision, got {events:?}"));
        assert_eq!(record.revision, revision);
        // branch, message and build from the caller, plus the timestamp the commit
        // stamps. No identity keys: this context has no authenticated user, and the
        // commit only stamps an author it has — that half is covered by
        // `commit_in_memory_revision_stamps_the_timestamp_and_author_when_unset`,
        // whose fixture supplies one.
        assert_eq!(
            record.metadata_key_count, 4,
            "info must count every key the revision carries, got {record:?}",
        );
        assert!(
            record.creation_timestamp > 0,
            "a commit must record when it happened, got {record:?}"
        );

        let (sink, callback) = make_sink();
        let status = metadata_get(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataGetArgs {
                batch_id: CALL_ID,
                handle: reloaded,
                include_revision: 1,
                entries: LoreArray::from_vec(vec![LoreRevisionTreeMetadataGetEntry {
                    entry_id: 1,
                    key: LoreString::from_str("build"),
                }]),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(
            status, 0,
            "reading the committed value must succeed, got {events:?}"
        );
        assert!(
            events.contains(&Captured::MetadataGetComplete(
                1,
                "build".to_string(),
                LoreMetadata::Numeric(42),
                LoreErrorCode::None,
            )),
            "the committed value must read back, got {events:?}"
        );

        close_handle(reloaded).await;
    }
}

/// `move` over the real capi path, on nodes a commit has settled.
///
/// The unit tests fake a settled node by clearing its staging flags through
/// `RevisionTreeInternal`, which is `pub(crate)`; here a commit does it, and a fresh handle
/// loaded on the published revision is the only place the whole claim can be checked — that
/// a moved node is under its new parent, with the identity it had, in a revision somebody
/// else can read.
#[cfg(test)]
mod move_tests {
    use lore::revision_tree::add::LoreRevisionTreeAddArgs;
    use lore::revision_tree::add::LoreRevisionTreeAddEntry;
    use lore::revision_tree::add::add;
    use lore::revision_tree::commit::LoreRevisionTreeCommitArgs;
    use lore::revision_tree::commit::LoreRevisionTreeCommitOptions;
    use lore::revision_tree::commit::commit;
    use lore::revision_tree::handle::LoreRevisionTree;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
    use lore::revision_tree::metadata_set::metadata_set;
    use lore::revision_tree::move_node::LoreRevisionTreeMoveArgs;
    use lore::revision_tree::move_node::LoreRevisionTreeMoveEntry;
    use lore::revision_tree::move_node::move_node;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::LoreErrorCode;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreMetadata;
    use lore_revision::interface::LoreNodeType;
    use lore_revision::interface::LoreString;
    use lore_revision::metadata::BRANCH;
    use lore_revision::node::INVALID_NODE;
    use lore_revision::node::NodeID;
    use lore_revision::node::ROOT_NODE;

    use super::support::*;

    fn add_entry(
        entry_id: u64,
        parent_node_id: NodeID,
        name: &str,
        kind: LoreNodeType,
    ) -> LoreRevisionTreeAddEntry {
        LoreRevisionTreeAddEntry {
            entry_id,
            parent_node_id,
            parent_entry_index: 0,
            name: LoreString::from_str(name),
            kind: kind as u32,
            mode: 0o644,
            size: 8,
            address: Address {
                hash: Hash::from_u64(0x5eed),
                context: Context::from(uuid::Uuid::now_v7()),
            },
        }
    }

    fn entry(
        entry_id: u64,
        node_id: NodeID,
        destination_parent_id: NodeID,
        dst_name: &str,
    ) -> LoreRevisionTreeMoveEntry {
        LoreRevisionTreeMoveEntry {
            entry_id,
            node_id,
            destination_parent_id,
            dst_name: LoreString::from_str(dst_name),
        }
    }

    async fn run_add(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeAddEntry>,
    ) -> Vec<Captured> {
        let (sink, callback) = make_sink();
        let status = add(
            LoreGlobalArgs::default(),
            LoreRevisionTreeAddArgs {
                batch_id: 1,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(status, 0, "seeding must succeed, got {events:?}");
        events
    }

    fn added_node(events: &[Captured], entry_id: u64) -> NodeID {
        events
            .iter()
            .find_map(|event| match event {
                Captured::AddComplete(id, node_id, code) if *id == entry_id => {
                    assert_eq!(*code, LoreErrorCode::None, "entry {entry_id} must succeed");
                    Some(*node_id)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("entry {entry_id} must report, got {events:?}"))
    }

    async fn run_move(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeMoveEntry>,
    ) -> (i32, Vec<Captured>) {
        let (sink, callback) = make_sink();
        let status = move_node(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMoveArgs {
                batch_id: CALL_ID,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    async fn commit_on(handle: LoreRevisionTree, branch: Option<Context>) -> Hash {
        if let Some(branch) = branch {
            let (sink, callback) = make_sink();
            let status = metadata_set(
                LoreGlobalArgs::default(),
                LoreRevisionTreeMetadataSetArgs {
                    batch_id: CALL_ID,
                    handle,
                    entries: LoreArray::from_vec(vec![LoreRevisionTreeMetadataSetEntry {
                        entry_id: 1,
                        key: LoreString::from_str(BRANCH),
                        value: LoreMetadata::Context(branch),
                    }]),
                },
                callback,
            )
            .await;
            assert_eq!(
                status,
                0,
                "naming the branch must succeed, got {:?}",
                sink.lock().unwrap()
            );
        }

        let (sink, callback) = make_sink();
        let status = commit(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCommitArgs {
                id: CALL_ID,
                handle,
                options: LoreRevisionTreeCommitOptions::default(),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(status, 0, "committing must succeed, got {events:?}");
        events
            .iter()
            .find_map(|event| match event {
                Captured::CommitComplete(id, revision, _, _) if *id == CALL_ID => Some(*revision),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the commit terminal must fire, got {events:?}"))
    }

    /// The whole claim of the verb, end to end: a node the revision holds changes parent,
    /// the change is committed, and a handle that knows nothing of this one reads it back
    /// under its new parent with the identity it had.
    #[tokio::test]
    async fn a_move_survives_a_commit_and_reads_back_through_a_fresh_handle() {
        let store = open_store().await;
        let repository = Partition::from([0xa1u8; 16]);
        let handle = load_on(store, repository, Hash::default()).await;

        let seeded = run_add(
            handle,
            vec![
                add_entry(1, ROOT_NODE, "src", LoreNodeType::Directory),
                add_entry(2, ROOT_NODE, "dst", LoreNodeType::Directory),
            ],
        )
        .await;
        let source = added_node(&seeded, 1);
        let destination = added_node(&seeded, 2);
        let leaf = added_node(
            &run_add(
                handle,
                vec![add_entry(3, source, "data.bin", LoreNodeType::File)],
            )
            .await,
            3,
        );
        let first_revision = commit_on(handle, Some(Context::from(uuid::Uuid::now_v7()))).await;

        let settled = load_on(store, repository, first_revision).await;
        let file_id_before = node_info_of(settled, leaf).await.file_id;
        assert!(
            !file_id_before.is_zero(),
            "the fixture must start from a file carrying an identity"
        );

        let (status, events) =
            run_move(settled, vec![entry(10, leaf, destination, "data.bin")]).await;
        assert_eq!(
            status, 0,
            "moving a settled node must succeed, got {events:?}"
        );
        assert_eq!(
            move_outcomes(&events),
            vec![(10, leaf, LoreErrorCode::None)],
            "the terminal must echo the moved node"
        );
        let second_revision = commit_on(settled, None).await;
        assert_ne!(
            second_revision, first_revision,
            "the move must produce a revision of its own"
        );

        let reloaded = load_on(store, repository, second_revision).await;
        assert_eq!(
            child_names(reloaded, destination).await,
            vec!["data.bin".to_string()],
            "the published revision must hold the file under the parent it moved to"
        );
        assert!(
            child_names(reloaded, source).await.is_empty(),
            "and nothing under the one it left"
        );
        let record = node_info_of(reloaded, leaf).await;
        assert_eq!(
            record.parent_id, destination,
            "the node id survives the commit and points at its new parent"
        );
        assert_eq!(
            record.file_id, file_id_before,
            "with the identity it had, which is what makes the delta a move"
        );

        close_handle(reloaded).await;
        close_handle(settled).await;
        close_handle(handle).await;
    }

    /// Two entries exchange names in one call. Neither could run alone — each wants a name
    /// the other still holds — so this is the batch rule reaching the caller through the
    /// real argument marshalling rather than an in-process call.
    #[tokio::test]
    async fn a_batch_swaps_two_names_over_the_capi() {
        let store = open_store().await;
        let repository = Partition::from([0xa2u8; 16]);
        let handle = load_on(store, repository, Hash::default()).await;

        let seeded = run_add(
            handle,
            vec![
                add_entry(1, ROOT_NODE, "a", LoreNodeType::Directory),
                add_entry(2, ROOT_NODE, "b", LoreNodeType::Directory),
            ],
        )
        .await;
        let first = added_node(&seeded, 1);
        let second = added_node(&seeded, 2);
        let leaves = run_add(
            handle,
            vec![
                add_entry(3, first, "x.bin", LoreNodeType::File),
                add_entry(4, second, "x.bin", LoreNodeType::File),
            ],
        )
        .await;
        let from_first = added_node(&leaves, 3);
        let from_second = added_node(&leaves, 4);
        let revision = commit_on(handle, Some(Context::from(uuid::Uuid::now_v7()))).await;

        let settled = load_on(store, repository, revision).await;
        let (status, events) = run_move(
            settled,
            vec![
                entry(10, from_first, second, "x.bin"),
                entry(11, from_second, first, "x.bin"),
            ],
        )
        .await;
        assert_eq!(status, 0, "a swap must succeed, got {events:?}");
        assert_eq!(
            move_outcomes(&events),
            vec![
                (10, from_first, LoreErrorCode::None),
                (11, from_second, LoreErrorCode::None),
            ],
            "both entries report their own node"
        );
        assert_eq!(
            parent_of(settled, from_first).await,
            second,
            "the first node must have taken the second's place"
        );
        assert_eq!(
            parent_of(settled, from_second).await,
            first,
            "and the second the first's"
        );

        close_handle(settled).await;
        close_handle(handle).await;
    }

    /// One bad entry rejects the call and moves nothing — the atomicity contract, asserted
    /// on the status the capi returns rather than on an in-process error type.
    #[tokio::test]
    async fn a_rejected_batch_moves_nothing_over_the_capi() {
        let store = open_store().await;
        let repository = Partition::from([0xa3u8; 16]);
        let handle = load_on(store, repository, Hash::default()).await;

        let seeded = run_add(
            handle,
            vec![
                add_entry(1, ROOT_NODE, "dst", LoreNodeType::Directory),
                add_entry(2, ROOT_NODE, "data.bin", LoreNodeType::File),
            ],
        )
        .await;
        let destination = added_node(&seeded, 1);
        let leaf = added_node(&seeded, 2);
        let revision = commit_on(handle, Some(Context::from(uuid::Uuid::now_v7()))).await;

        let settled = load_on(store, repository, revision).await;
        let (status, events) = run_move(
            settled,
            vec![
                entry(10, leaf, destination, "data.bin"),
                entry(11, INVALID_NODE, destination, "other.bin"),
            ],
        )
        .await;
        assert_eq!(
            status, REJECTED_STATUS,
            "a batch with a bad entry must be rejected during validation, got {events:?}"
        );
        assert_eq!(
            move_outcomes(&events),
            vec![(11, INVALID_NODE, LoreErrorCode::InvalidArguments)],
            "only the offending entry reports"
        );
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::InvalidArguments)],
            "and the batch terminal carries the call's outcome exactly once"
        );
        assert_eq!(
            parent_of(settled, leaf).await,
            ROOT_NODE,
            "the entry that passed its own checks must not have been applied"
        );

        close_handle(settled).await;
        close_handle(handle).await;
    }
}

/// A handle outliving its parent storage handle, driven over the capi. The handle borrows
/// the store rather than looking the parent up per call, so closing the parent leaves it
/// fully usable — and a freshly loaded handle caches no node block, so every read below
/// reaches a store whose handle is gone.
#[cfg(test)]
mod lifecycle_tests {
    use lore::revision_tree::add::LoreRevisionTreeAddArgs;
    use lore::revision_tree::add::LoreRevisionTreeAddEntry;
    use lore::revision_tree::add::add;
    use lore::revision_tree::commit::LoreRevisionTreeCommitArgs;
    use lore::revision_tree::commit::LoreRevisionTreeCommitOptions;
    use lore::revision_tree::commit::commit;
    use lore::revision_tree::handle::LoreRevisionTree;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
    use lore::revision_tree::metadata_set::metadata_set;
    use lore::storage::close::LoreStorageCloseArgs;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::LoreErrorCode;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreMetadata;
    use lore_revision::interface::LoreNodeType;
    use lore_revision::interface::LoreString;
    use lore_revision::metadata::BRANCH;
    use lore_revision::node::BLOCK_NODE_COUNT;
    use lore_revision::node::INVALID_NODE;
    use lore_revision::node::NodeID;
    use lore_revision::node::ROOT_NODE;

    use super::support::*;

    /// One directory plus enough files under it that the tree needs a second node block,
    /// so a read of the last child cannot be served by whatever the first read pulled in.
    fn subtree_entries(file_count: u64) -> Vec<LoreRevisionTreeAddEntry> {
        let directory = LoreRevisionTreeAddEntry {
            entry_id: 1,
            parent_node_id: ROOT_NODE,
            parent_entry_index: 0,
            name: LoreString::from_str("dir"),
            kind: LoreNodeType::Directory as u32,
            mode: 0o755,
            size: 0,
            address: Address::default(),
        };
        let files = (0..file_count).map(|index| LoreRevisionTreeAddEntry {
            entry_id: index + 2,
            parent_node_id: INVALID_NODE,
            parent_entry_index: 0,
            name: LoreString::from_str(&format!("f-{index:05}")),
            kind: LoreNodeType::File as u32,
            mode: 0o644,
            size: 12,
            address: Address {
                hash: Hash::from_u64(0xc0ffee + index),
                context: Context::from(uuid::Uuid::now_v7()),
            },
        });
        std::iter::once(directory).chain(files).collect()
    }

    async fn run_add(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeAddEntry>,
    ) -> Vec<Captured> {
        let (sink, callback) = make_sink();
        let status = add(
            LoreGlobalArgs::default(),
            LoreRevisionTreeAddArgs {
                batch_id: CALL_ID,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(status, 0, "seeding the tree must succeed, got {events:?}");
        events
    }

    fn added_node(events: &[Captured], entry_id: u64) -> NodeID {
        events
            .iter()
            .find_map(|event| match event {
                Captured::AddComplete(id, node_id, code) if *id == entry_id => {
                    assert_eq!(*code, LoreErrorCode::None, "entry {entry_id} must succeed");
                    Some(*node_id)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("entry {entry_id} must report, got {events:?}"))
    }

    /// A file directly under the root.
    fn file_entry(entry_id: u64, name: &str) -> LoreRevisionTreeAddEntry {
        LoreRevisionTreeAddEntry {
            entry_id,
            parent_node_id: ROOT_NODE,
            parent_entry_index: 0,
            name: LoreString::from_str(name),
            kind: LoreNodeType::File as u32,
            mode: 0o644,
            size: 12,
            address: Address {
                hash: Hash::from_u64(0xc0ffee),
                context: Context::from(uuid::Uuid::now_v7()),
            },
        }
    }

    /// `branch` names the branch an initial commit publishes to. A follow-up commit on a
    /// handle that already carries a revision passes `None`, continuing that revision's own.
    async fn commit_on(handle: LoreRevisionTree, branch: Option<Context>) -> Hash {
        if let Some(branch) = branch {
            let (sink, callback) = make_sink();
            let status = metadata_set(
                LoreGlobalArgs::default(),
                LoreRevisionTreeMetadataSetArgs {
                    batch_id: CALL_ID,
                    handle,
                    entries: LoreArray::from_vec(vec![LoreRevisionTreeMetadataSetEntry {
                        entry_id: 1,
                        key: LoreString::from_str(BRANCH),
                        value: LoreMetadata::Context(branch),
                    }]),
                },
                callback,
            )
            .await;
            assert_eq!(
                status,
                0,
                "naming the branch must succeed, got {:?}",
                sink.lock().unwrap()
            );
        }

        let (sink, callback) = make_sink();
        let status = commit(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCommitArgs {
                id: CALL_ID,
                handle,
                options: LoreRevisionTreeCommitOptions::default(),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(status, 0, "committing must succeed, got {events:?}");
        events
            .iter()
            .find_map(|event| match event {
                Captured::CommitComplete(id, revision, _, _) if *id == CALL_ID => Some(*revision),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the commit terminal must fire, got {events:?}"))
    }

    async fn close_store(store_handle_id: u64) {
        let (sink, callback) = make_sink();
        let status = lore::storage::close::close(
            LoreGlobalArgs::default(),
            LoreStorageCloseArgs {
                handle: lore::storage::handle::LoreStore {
                    handle_id: store_handle_id,
                },
            },
            callback,
        )
        .await;
        assert_eq!(
            status,
            0,
            "closing the parent storage handle must succeed, got {:?}",
            sink.lock().unwrap()
        );
    }

    /// Reading a committed revision through a handle whose parent has closed. The blocks
    /// live in the store and the fresh handle holds none of them, so the store it kept has
    /// to be functionally live rather than merely reference-counted.
    #[tokio::test]
    async fn tree_block_reads_continue_after_the_parent_storage_closes() {
        let repository = Partition::from([0x18u8; 16]);
        let store = open_store().await;
        let handle = load_on(store, repository, Hash::default()).await;

        let file_count = BLOCK_NODE_COUNT as u64 + 8;
        let seeded = run_add(handle, subtree_entries(file_count)).await;
        let last_leaf = added_node(&seeded, file_count + 1);
        let revision = commit_on(handle, Some(Context::from(uuid::Uuid::now_v7()))).await;
        close_handle(handle).await;

        let reloaded = load_on(store, repository, revision).await;
        close_store(store).await;

        let names = child_names(reloaded, added_node(&seeded, 1)).await;
        assert_eq!(
            names.len(),
            file_count as usize,
            "every child must come back from a store whose handle is closed"
        );
        // The name comes out of the node's own block, unlike the revision, which the state
        // header carries.
        let record = node_info_of(reloaded, last_leaf).await;
        assert_eq!(
            record.name.as_str(),
            format!("f-{:05}", file_count - 1),
            "a node in the last block must read back too, got {record:?}"
        );

        close_handle(reloaded).await;
    }

    /// The handle still writes, too. A commit is the whole write path at once: it freezes
    /// the tree, serializes blocks through the immutable store and advances a branch
    /// pointer in the mutable one, all through Arcs whose storage handle is gone.
    #[tokio::test]
    async fn edits_and_commits_continue_after_the_parent_storage_closes() {
        let repository = Partition::from([0x19u8; 16]);
        let store = open_store().await;
        let handle = load_on(store, repository, Hash::default()).await;

        run_add(handle, vec![file_entry(1, "before.bin")]).await;
        let first = commit_on(handle, Some(Context::from(uuid::Uuid::now_v7()))).await;

        close_store(store).await;

        let seeded = run_add(handle, vec![file_entry(2, "after.bin")]).await;
        let added = added_node(&seeded, 2);
        let second = commit_on(handle, None).await;

        assert!(
            !second.is_zero() && second != first,
            "the commit must publish a new revision, got {second} after {first}"
        );
        assert_eq!(
            child_names(handle, ROOT_NODE).await,
            vec!["after.bin".to_string(), "before.bin".to_string()],
            "and the tree must hold both what it committed before its parent closed and after"
        );
        assert_eq!(
            node_info_of(handle, added).await.revision,
            second,
            "the node added after the close belongs to the revision the commit published"
        );

        close_handle(handle).await;
    }
}

/// `lore_address_t` crossing between the storage API and this one, over a real store and in
/// both directions: the address a `lore_storage_put` returns is the address `add` and
/// `modify` take, and the address `node_info` and `list_children` report is the address
/// `lore_storage_get` reads. Each test below carries one address across the boundary with no
/// conversion, which only compiles because a single type spans both surfaces — a bridging
/// type or wrapper would break the build rather than a test.
#[cfg(test)]
mod interop_tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore::revision_tree::add::LoreRevisionTreeAddArgs;
    use lore::revision_tree::add::LoreRevisionTreeAddEntry;
    use lore::revision_tree::add::add;
    use lore::revision_tree::commit::LoreRevisionTreeCommitArgs;
    use lore::revision_tree::commit::LoreRevisionTreeCommitOptions;
    use lore::revision_tree::commit::commit;
    use lore::revision_tree::handle::LoreRevisionTree;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
    use lore::revision_tree::metadata_set::metadata_set;
    use lore::revision_tree::modify::LoreRevisionTreeModifyArgs;
    use lore::revision_tree::modify::LoreRevisionTreeModifyEntry;
    use lore::revision_tree::modify::modify;
    use lore::storage::handle::LoreStore;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::LoreBytes;
    use lore_revision::event::LoreErrorCode;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreMetadata;
    use lore_revision::interface::LoreNodeType;
    use lore_revision::interface::LoreString;
    use lore_revision::metadata::BRANCH;
    use lore_revision::node::NodeID;
    use lore_revision::node::ROOT_NODE;

    use super::support::*;

    /// The canonical write path the LEP documents: the caller mints a file id and supplies it
    /// as the put's context, so one address carries both the content and the file identity.
    fn file_id() -> Context {
        Context::from(uuid::Uuid::now_v7())
    }

    /// Store `payload` and return the address the storage API reports for it.
    async fn put_bytes(
        store_handle_id: u64,
        partition: Partition,
        context: Context,
        payload: &[u8],
    ) -> Address {
        let stored: Arc<Mutex<Option<Address>>> = Arc::new(Mutex::new(None));
        let sink = stored.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StoragePutItemComplete(data) = event {
                assert_eq!(
                    data.error_code,
                    LoreErrorCode::None,
                    "storing the payload must succeed"
                );
                *sink.lock().unwrap() = Some(data.address);
            }
        }));
        let status = lore::storage::put::put(
            LoreGlobalArgs::default(),
            lore::storage::put::LoreStoragePutArgs {
                handle: LoreStore {
                    handle_id: store_handle_id,
                },
                items: LoreArray::from_vec(vec![lore::storage::put::LoreStoragePutItem {
                    id: 1,
                    partition,
                    context,
                    data: LoreBytes {
                        ptr: payload.as_ptr().cast(),
                        len: payload.len(),
                    },
                    remote_write: 0,
                    local_cache: 0,
                    fixed_size_chunk: 0,
                }]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "the put call must succeed");
        let address = *stored.lock().unwrap();
        address.expect("the put must report the address it stored the payload at")
    }

    /// Read `address` back through the storage API. The `GET_DATA` view is valid only for the
    /// callback's invocation, so the bytes are copied out inside it.
    async fn get_bytes(store_handle_id: u64, partition: Partition, address: Address) -> Vec<u8> {
        let read: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = read.clone();
        let outcome: Arc<Mutex<Option<LoreErrorCode>>> = Arc::new(Mutex::new(None));
        let outcome_sink = outcome.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::StorageGetData(data) => {
                if data.bytes.len > 0 {
                    let bytes = unsafe {
                        std::slice::from_raw_parts(data.bytes.ptr.cast::<u8>(), data.bytes.len)
                    };
                    sink.lock().unwrap().extend_from_slice(bytes);
                }
            }
            LoreEvent::StorageGetItemComplete(data) => {
                *outcome_sink.lock().unwrap() = Some(data.error_code);
            }
            _ => {}
        }));
        let status = lore::storage::get::get(
            LoreGlobalArgs::default(),
            lore::storage::get::LoreStorageGetArgs {
                handle: LoreStore {
                    handle_id: store_handle_id,
                },
                items: LoreArray::from_vec(vec![lore::storage::get::LoreStorageGetItem {
                    id: 1,
                    partition,
                    address,
                    ..Default::default()
                }]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "reading {address:?} back must succeed");
        assert_eq!(
            *outcome.lock().unwrap(),
            Some(LoreErrorCode::None),
            "the read of {address:?} must report success"
        );
        read.lock().unwrap().clone()
    }

    fn file_entry(
        entry_id: u64,
        name: &str,
        size: u64,
        address: Address,
    ) -> LoreRevisionTreeAddEntry {
        LoreRevisionTreeAddEntry {
            entry_id,
            parent_node_id: ROOT_NODE,
            parent_entry_index: 0,
            name: LoreString::from_str(name),
            kind: LoreNodeType::File as u32,
            mode: 0o644,
            size,
            address,
        }
    }

    async fn run_add(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeAddEntry>,
    ) -> Vec<Captured> {
        let (sink, callback) = make_sink();
        let status = add(
            LoreGlobalArgs::default(),
            LoreRevisionTreeAddArgs {
                batch_id: CALL_ID,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(status, 0, "placing the file must succeed, got {events:?}");
        events
    }

    async fn run_modify(handle: LoreRevisionTree, entries: Vec<LoreRevisionTreeModifyEntry>) {
        let (sink, callback) = make_sink();
        let status = modify(
            LoreGlobalArgs::default(),
            LoreRevisionTreeModifyArgs {
                batch_id: CALL_ID,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        assert_eq!(
            status,
            0,
            "rewriting the file must succeed, got {:?}",
            sink.lock().unwrap()
        );
    }

    fn added_node(events: &[Captured], entry_id: u64) -> NodeID {
        events
            .iter()
            .find_map(|event| match event {
                Captured::AddComplete(id, node_id, code) if *id == entry_id => {
                    assert_eq!(*code, LoreErrorCode::None, "entry {entry_id} must succeed");
                    Some(*node_id)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("entry {entry_id} must report, got {events:?}"))
    }

    async fn commit_on(handle: LoreRevisionTree, branch: Context) -> Hash {
        let (sink, callback) = make_sink();
        let status = metadata_set(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataSetArgs {
                batch_id: CALL_ID,
                handle,
                entries: LoreArray::from_vec(vec![LoreRevisionTreeMetadataSetEntry {
                    entry_id: 1,
                    key: LoreString::from_str(BRANCH),
                    value: LoreMetadata::Context(branch),
                }]),
            },
            callback,
        )
        .await;
        assert_eq!(
            status,
            0,
            "naming the branch must succeed, got {:?}",
            sink.lock().unwrap()
        );

        let (sink, callback) = make_sink();
        let status = commit(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCommitArgs {
                id: CALL_ID,
                handle,
                options: LoreRevisionTreeCommitOptions::default(),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(status, 0, "committing must succeed, got {events:?}");
        events
            .iter()
            .find_map(|event| match event {
                Captured::CommitComplete(id, revision, _, _) if *id == CALL_ID => Some(*revision),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the commit terminal must fire, got {events:?}"))
    }

    /// The write leg: an address produced by `lore_storage_put` reaches the tree with every
    /// field intact, file id included, and the node reports it back as one value rather than
    /// as parts a caller has to reassemble.
    #[tokio::test]
    async fn address_from_storage_put_crosses_unchanged_to_revision_tree_add() {
        let repository = Partition::from([0x1au8; 16]);
        let store = open_store().await;
        let handle = load_on(store, repository, Hash::default()).await;

        let payload = b"content addressed by a put and placed by an add".to_vec();
        let put_address = put_bytes(store, repository, file_id(), &payload).await;

        let seeded = run_add(
            handle,
            vec![file_entry(
                1,
                "payload.bin",
                payload.len() as u64,
                put_address,
            )],
        )
        .await;

        let info = node_info_of(handle, added_node(&seeded, 1)).await;
        assert_eq!(
            info.address, put_address,
            "the whole address must cross unchanged, got {info:?}"
        );
        assert_eq!(
            info.file_id, put_address.context,
            "the file id is the address context the put was given, got {info:?}"
        );

        close_handle(handle).await;
    }

    /// The same leg for `modify`. Rewriting a file's content keeps its identity, so the second
    /// put carries the file id the first one did and the whole address crosses again.
    #[tokio::test]
    async fn address_from_storage_put_crosses_unchanged_to_revision_tree_modify() {
        let repository = Partition::from([0x1bu8; 16]);
        let store = open_store().await;
        let handle = load_on(store, repository, Hash::default()).await;

        let identity = file_id();
        let original = b"the content the file was added with".to_vec();
        let first = put_bytes(store, repository, identity, &original).await;
        let seeded = run_add(
            handle,
            vec![file_entry(1, "payload.bin", original.len() as u64, first)],
        )
        .await;
        let node = added_node(&seeded, 1);

        let rewritten = b"the content the file was rewritten with, which is longer".to_vec();
        let second = put_bytes(store, repository, identity, &rewritten).await;
        assert_ne!(
            second.hash, first.hash,
            "the rewrite must produce different content"
        );
        run_modify(
            handle,
            vec![LoreRevisionTreeModifyEntry {
                entry_id: 1,
                node_id: node,
                mode: 0o644,
                size: rewritten.len() as u64,
                address: second,
            }],
        )
        .await;

        let info = node_info_of(handle, node).await;
        assert_eq!(
            info.address, second,
            "the rewritten address must cross unchanged, got {info:?}"
        );
        assert_eq!(
            info.file_id, identity,
            "and the file survives its own rewrite, got {info:?}"
        );

        close_handle(handle).await;
    }

    /// The read leg: the address `node_info` reports is a complete argument for
    /// `lore_storage_get`, paired with the repository the same event carries. Byte equality
    /// is the assertion that matters — field equality alone would not show that the store
    /// accepts what the tree handed back.
    #[tokio::test]
    async fn address_from_revision_tree_node_info_crosses_unchanged_to_storage_get() {
        let repository = Partition::from([0x1cu8; 16]);
        let store = open_store().await;
        let handle = load_on(store, repository, Hash::default()).await;

        let payload = b"bytes fetched back through the address the tree reports".to_vec();
        let put_address = put_bytes(store, repository, file_id(), &payload).await;
        let seeded = run_add(
            handle,
            vec![file_entry(
                1,
                "payload.bin",
                payload.len() as u64,
                put_address,
            )],
        )
        .await;

        let info = node_info_of(handle, added_node(&seeded, 1)).await;
        assert_eq!(
            info.address, put_address,
            "the reported address must be the one the put returned, got {info:?}"
        );
        assert_eq!(
            info.repository, repository,
            "the node must name the partition to read from, got {info:?}"
        );
        assert_eq!(
            get_bytes(store, info.repository, info.address).await,
            payload,
            "the address the tree reported must read the content back"
        );

        close_handle(handle).await;
    }

    /// `list_children` is the other read verb that surfaces an address, and it must be as
    /// usable as `node_info`'s: the same value, and the same successful read.
    #[tokio::test]
    async fn address_from_revision_tree_list_children_crosses_unchanged_to_storage_get() {
        let repository = Partition::from([0x1du8; 16]);
        let store = open_store().await;
        let handle = load_on(store, repository, Hash::default()).await;

        let payload = b"bytes fetched back through the address a listing reports".to_vec();
        let put_address = put_bytes(store, repository, file_id(), &payload).await;
        run_add(
            handle,
            vec![file_entry(
                1,
                "payload.bin",
                payload.len() as u64,
                put_address,
            )],
        )
        .await;

        let listed = child_records(handle, ROOT_NODE).await;
        let (_, name, address) = listed
            .first()
            .unwrap_or_else(|| panic!("the child must be listed, got {listed:?}"));
        assert_eq!(name, "payload.bin");
        assert_eq!(
            *address, put_address,
            "the listing must carry the address unchanged, got {listed:?}"
        );
        assert_eq!(
            get_bytes(store, repository, *address).await,
            payload,
            "the address the listing reported must read the content back"
        );

        close_handle(handle).await;
    }

    /// The round trip across a commit. The address is written into a node block, serialized,
    /// and read back through a handle that loaded the committed revision from the store — so
    /// this is the leg that would catch an address the tree cannot preserve on disk.
    #[tokio::test]
    async fn an_address_survives_a_commit_and_reads_back_through_storage_get() {
        let repository = Partition::from([0x1eu8; 16]);
        let store = open_store().await;
        let handle = load_on(store, repository, Hash::default()).await;

        let payload = b"content that outlives the handle that placed it".to_vec();
        let put_address = put_bytes(store, repository, file_id(), &payload).await;
        let seeded = run_add(
            handle,
            vec![file_entry(
                1,
                "payload.bin",
                payload.len() as u64,
                put_address,
            )],
        )
        .await;
        let node = added_node(&seeded, 1);
        let revision = commit_on(handle, file_id()).await;
        close_handle(handle).await;

        let reloaded = load_on(store, repository, revision).await;
        let info = node_info_of(reloaded, node).await;
        assert_eq!(
            info.address, put_address,
            "a committed and reloaded node must carry the address the put returned, got {info:?}"
        );
        assert_eq!(
            get_bytes(store, info.repository, info.address).await,
            payload,
            "and it must still read the content back"
        );

        close_handle(reloaded).await;
    }

    /// The one case where the address does not cross unchanged. A node carries its file id in
    /// the context slot of its address, so a file added with a zero context has one generated
    /// into it and the tree reports an address the caller never held. The content is still
    /// reachable here only because a store that does not isolate partitions resolves a read on
    /// the hash alone; one that does serves exact associations only. Minting the file id before
    /// the put, as the LEP's example does, is what makes the two addresses one value.
    #[tokio::test]
    async fn an_add_without_a_file_id_stamps_one_into_the_address_context() {
        let repository = Partition::from([0x1fu8; 16]);
        let store = open_store().await;
        let handle = load_on(store, repository, Hash::default()).await;

        let payload = b"content stored without a file id".to_vec();
        let put_address = put_bytes(store, repository, Context::default(), &payload).await;
        assert!(
            put_address.context.is_zero(),
            "the put must report the context it was given, got {put_address:?}"
        );

        let seeded = run_add(
            handle,
            vec![file_entry(
                1,
                "payload.bin",
                payload.len() as u64,
                put_address,
            )],
        )
        .await;

        let info = node_info_of(handle, added_node(&seeded, 1)).await;
        assert_eq!(
            info.address.hash, put_address.hash,
            "the content hash must be untouched, got {info:?}"
        );
        assert_ne!(
            info.address.context, put_address.context,
            "the zero context must be replaced by a generated file id, got {info:?}"
        );
        assert_eq!(
            info.file_id, info.address.context,
            "which is where the node keeps its file id, got {info:?}"
        );
        assert_eq!(
            get_bytes(store, repository, info.address).await,
            payload,
            "the content is still reachable through the hash on a store that does not isolate \
             partitions"
        );

        close_handle(handle).await;
    }
}

/// Calls racing each other on a multi-threaded runtime, which is how the dispatcher drives
/// this surface. What each test pins is an outcome the scheduling cannot change: a read never
/// sees a tree that never existed, two commits on one handle chain rather than interleave, two
/// handles racing one branch produce exactly one winner, and two repositories on one storage
/// handle do not touch each other.
#[cfg(test)]
mod concurrency_tests {
    use std::collections::BTreeSet;

    use lore::revision_tree::add::LoreRevisionTreeAddArgs;
    use lore::revision_tree::add::LoreRevisionTreeAddEntry;
    use lore::revision_tree::add::add;
    use lore::revision_tree::commit::LoreRevisionTreeCommitArgs;
    use lore::revision_tree::commit::LoreRevisionTreeCommitOptions;
    use lore::revision_tree::commit::commit;
    use lore::revision_tree::handle::LoreRevisionTree;
    use lore::revision_tree::metadata_get::LoreRevisionTreeMetadataGetArgs;
    use lore::revision_tree::metadata_get::LoreRevisionTreeMetadataGetEntry;
    use lore::revision_tree::metadata_get::metadata_get;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
    use lore::revision_tree::metadata_set::metadata_set;
    use lore::revision_tree::resolve_path::LoreRevisionTreeResolvePathArgs;
    use lore::revision_tree::resolve_path::resolve_path;
    use lore_base::lore_spawn;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::LoreErrorCode;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreMetadata;
    use lore_revision::interface::LoreNodeType;
    use lore_revision::interface::LoreString;
    use lore_revision::metadata::BRANCH;
    use lore_revision::node::NodeID;
    use lore_revision::node::ROOT_NODE;
    use tokio::task::JoinSet;

    use super::support::*;

    fn file_entry(entry_id: u64, parent_node_id: NodeID, name: &str) -> LoreRevisionTreeAddEntry {
        LoreRevisionTreeAddEntry {
            entry_id,
            parent_node_id,
            parent_entry_index: 0,
            name: LoreString::from_str(name),
            kind: LoreNodeType::File as u32,
            mode: 0o644,
            size: 12,
            address: Address {
                hash: Hash::from_u64(0xc0ffee),
                context: Context::from(uuid::Uuid::now_v7()),
            },
        }
    }

    fn directory_entry(entry_id: u64, name: &str) -> LoreRevisionTreeAddEntry {
        LoreRevisionTreeAddEntry {
            entry_id,
            parent_node_id: ROOT_NODE,
            parent_entry_index: 0,
            name: LoreString::from_str(name),
            kind: LoreNodeType::Directory as u32,
            mode: 0o755,
            size: 0,
            address: Address::default(),
        }
    }

    async fn run_add(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeAddEntry>,
    ) -> Vec<Captured> {
        let (sink, callback) = make_sink();
        let status = add(
            LoreGlobalArgs::default(),
            LoreRevisionTreeAddArgs {
                batch_id: CALL_ID,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(status, 0, "the add must succeed, got {events:?}");
        events
    }

    fn added_node(events: &[Captured], entry_id: u64) -> NodeID {
        events
            .iter()
            .find_map(|event| match event {
                Captured::AddComplete(id, node_id, code) if *id == entry_id => {
                    assert_eq!(*code, LoreErrorCode::None, "entry {entry_id} must succeed");
                    Some(*node_id)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("entry {entry_id} must report, got {events:?}"))
    }

    async fn resolve(handle: LoreRevisionTree, path: &str) -> NodeID {
        let (sink, callback) = make_sink();
        let status = resolve_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeResolvePathArgs {
                id: 1,
                handle,
                path: LoreString::from_str(path),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(status, 0, "resolving {path} must succeed, got {events:?}");
        events
            .iter()
            .find_map(|event| match event {
                Captured::ResolvePath(node_id, LoreErrorCode::None) => Some(*node_id),
                _ => None,
            })
            .unwrap_or_else(|| panic!("resolving {path} must report a node, got {events:?}"))
    }

    async fn set_branch(handle: LoreRevisionTree, branch: Context) {
        let (sink, callback) = make_sink();
        let status = metadata_set(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataSetArgs {
                batch_id: CALL_ID,
                handle,
                entries: LoreArray::from_vec(vec![LoreRevisionTreeMetadataSetEntry {
                    entry_id: 1,
                    key: LoreString::from_str(BRANCH),
                    value: LoreMetadata::Context(branch),
                }]),
            },
            callback,
        )
        .await;
        assert_eq!(
            status,
            0,
            "naming the branch must succeed, got {:?}",
            sink.lock().unwrap()
        );
    }

    /// The commit terminal in full: status, the revision it published, the tip to reload from
    /// when it did not, and the outcome code.
    async fn run_commit(handle: LoreRevisionTree) -> (i32, Hash, Hash, LoreErrorCode) {
        let (sink, callback) = make_sink();
        let status = commit(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCommitArgs {
                id: CALL_ID,
                handle,
                options: LoreRevisionTreeCommitOptions::default(),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        let (revision, new_tip, code) = events
            .iter()
            .find_map(|event| match event {
                Captured::CommitComplete(id, revision, new_tip, code) if *id == CALL_ID => {
                    Some((*revision, *new_tip, *code))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("the commit terminal must fire, got {events:?}"));
        (status, revision, new_tip, code)
    }

    /// Reads and writes overlapping on one handle. A reader may see any number of the
    /// concurrent adds, but never fewer nodes than were already committed to the tree and
    /// never a name nobody added — the two ways a torn read would show.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reads_and_writes_on_one_handle_do_not_race() {
        const SEEDED: u64 = 4;
        const WRITERS: u64 = 4;
        const PER_WRITER: u64 = 8;
        const READERS: u64 = 4;
        const READS_EACH: u64 = 12;

        let handle = load_handle(Partition::from([0x20u8; 16])).await;
        let seeded = run_add(handle, vec![directory_entry(1, "dir")]).await;
        let directory = added_node(&seeded, 1);
        run_add(
            handle,
            (0..SEEDED)
                .map(|index| file_entry(index + 1, directory, &format!("seed-{index}")))
                .collect(),
        )
        .await;
        let anchor = resolve(handle, "dir/seed-0").await;

        let settled: BTreeSet<String> = (0..SEEDED).map(|index| format!("seed-{index}")).collect();
        let written: BTreeSet<String> = (0..WRITERS)
            .flat_map(|writer| (0..PER_WRITER).map(move |index| format!("w{writer}-{index}")))
            .collect();

        let mut writers: JoinSet<()> = JoinSet::new();
        for writer in 0..WRITERS {
            lore_spawn!(writers, async move {
                for index in 0..PER_WRITER {
                    run_add(
                        handle,
                        vec![file_entry(
                            writer * PER_WRITER + index + 1,
                            directory,
                            &format!("w{writer}-{index}"),
                        )],
                    )
                    .await;
                }
            });
        }

        let mut readers: JoinSet<Vec<BTreeSet<String>>> = JoinSet::new();
        for _ in 0..READERS {
            lore_spawn!(readers, async move {
                let mut seen = Vec::new();
                for _ in 0..READS_EACH {
                    assert_eq!(
                        node_info_of(handle, anchor).await.name.as_str(),
                        "seed-0",
                        "a settled node must keep reading back while the tree is written to"
                    );
                    assert_eq!(
                        resolve(handle, "dir/seed-0").await,
                        anchor,
                        "and must keep resolving to the same node"
                    );
                    seen.push(child_names(handle, directory).await.into_iter().collect());
                }
                seen
            });
        }

        while let Some(result) = writers.join_next().await {
            result.expect("a writer must not panic");
        }
        for result in readers.join_all().await {
            for observed in result {
                assert!(
                    settled.is_subset(&observed),
                    "a read must never lose a settled node, got {observed:?}"
                );
                assert!(
                    observed
                        .difference(&settled)
                        .all(|name| written.contains(name)),
                    "a read must never invent a node, got {observed:?}"
                );
            }
        }

        assert_eq!(
            child_names(handle, directory).await,
            settled.union(&written).cloned().collect::<Vec<String>>(),
            "every concurrent write must survive the reads"
        );
        close_handle(handle).await;
    }

    /// Two commits racing on one handle. The exclusive claim makes them serialize, so the
    /// second runs against the state the first published rather than beside it — the branch
    /// never moves under either, and no edit is dropped between them.
    ///
    /// Which of the two publishes what is the scheduler's to decide: one commit can sweep up
    /// both adds and leave the other with nothing staged, which is a legal outcome and not the
    /// failure this test is looking for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_commit_on_one_handle_serializes() {
        let repository = Partition::from([0x21u8; 16]);
        let store = open_store().await;
        let handle = load_on(store, repository, Hash::default()).await;
        set_branch(handle, Context::from(uuid::Uuid::now_v7())).await;

        let mut commits: JoinSet<(i32, Hash, Hash, LoreErrorCode)> = JoinSet::new();
        for task in 0..2u64 {
            lore_spawn!(commits, async move {
                run_add(
                    handle,
                    vec![file_entry(task + 1, ROOT_NODE, &format!("f{task}.bin"))],
                )
                .await;
                run_commit(handle).await
            });
        }
        let outcomes = commits.join_all().await;

        for (_, _, new_tip, _) in &outcomes {
            assert!(
                new_tip.is_zero(),
                "a commit on one handle must never find the branch advanced under it, got \
                 {outcomes:?}"
            );
        }
        let published: Vec<Hash> = outcomes
            .iter()
            .filter(|(status, ..)| *status == 0)
            .map(|(_, revision, ..)| *revision)
            .collect();
        assert!(
            !published.is_empty(),
            "one of the two must publish, got {outcomes:?}"
        );

        let mut carried_both = false;
        for revision in &published {
            let reloaded = load_on(store, repository, *revision).await;
            carried_both |= child_names(reloaded, ROOT_NODE).await
                == vec!["f0.bin".to_string(), "f1.bin".to_string()];
            close_handle(reloaded).await;
        }
        assert!(
            carried_both,
            "a published revision must carry both adds — a lost one means the commits \
             interleaved, got {outcomes:?}"
        );

        close_handle(handle).await;
    }

    /// Two handles racing one branch. There is no shared claim to serialize them, so the CAS in
    /// the mutable store decides: exactly one tip advance lands, and the loser is handed the
    /// revision it has to reload from rather than an error it has to interpret.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_commit_on_same_branch_from_two_handles_resolves_via_branch_advanced() {
        let repository = Partition::from([0x22u8; 16]);
        let branch = Context::from(uuid::Uuid::now_v7());
        let store = open_store().await;

        let mut handles = Vec::new();
        for task in 0..2u64 {
            let handle = load_on(store, repository, Hash::default()).await;
            run_add(
                handle,
                vec![file_entry(task + 1, ROOT_NODE, &format!("f{task}.bin"))],
            )
            .await;
            set_branch(handle, branch).await;
            handles.push(handle);
        }

        let mut commits: JoinSet<(i32, Hash, Hash, LoreErrorCode)> = JoinSet::new();
        for handle in handles.iter().copied() {
            lore_spawn!(commits, async move { run_commit(handle).await });
        }
        let outcomes = commits.join_all().await;

        let published: Vec<Hash> = outcomes
            .iter()
            .filter(|(status, ..)| *status == 0)
            .map(|(_, revision, ..)| *revision)
            .collect();
        assert_eq!(
            published.len(),
            1,
            "exactly one commit may advance the branch, got {outcomes:?}"
        );

        let (status, revision, new_tip, code) = outcomes
            .iter()
            .find(|(status, ..)| *status != 0)
            .copied()
            .unwrap_or_else(|| panic!("one commit must lose, got {outcomes:?}"));
        assert_eq!(status, -1, "got {outcomes:?}");
        assert_eq!(code, LoreErrorCode::Internal, "got {outcomes:?}");
        assert!(
            revision.is_zero(),
            "a loser publishes nothing, got {outcomes:?}"
        );
        assert_eq!(
            new_tip, published[0],
            "the loser must be told the tip the winner set, got {outcomes:?}"
        );

        for handle in handles {
            close_handle(handle).await;
        }
    }

    /// Two repositories committing concurrently through one storage handle. They share the
    /// backend stores, so the isolation being proved is that a revision lands in the partition
    /// it belongs to and neither branch tip is written by the other's commit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_commits_on_different_repositories_one_storage_handle_dont_interfere() {
        let repositories = [Partition::from([0x23u8; 16]), Partition::from([0x24u8; 16])];
        let store = open_store().await;

        let mut handles = Vec::new();
        for (task, repository) in repositories.iter().enumerate() {
            let handle = load_on(store, *repository, Hash::default()).await;
            run_add(
                handle,
                vec![file_entry(
                    task as u64 + 1,
                    ROOT_NODE,
                    &format!("r{task}.bin"),
                )],
            )
            .await;
            set_branch(handle, Context::from(uuid::Uuid::now_v7())).await;
            handles.push(handle);
        }

        // `join_all` yields results as tasks finish, so each carries the repository it
        // committed for rather than relying on the order they come back in.
        let mut commits: JoinSet<(usize, i32, Hash, LoreErrorCode)> = JoinSet::new();
        for (task, handle) in handles.iter().copied().enumerate() {
            lore_spawn!(commits, async move {
                let (status, revision, _, code) = run_commit(handle).await;
                (task, status, revision, code)
            });
        }
        let mut outcomes = commits.join_all().await;
        outcomes.sort_unstable_by_key(|(task, ..)| *task);

        for (_, status, revision, code) in &outcomes {
            assert_eq!(
                *status, 0,
                "separate repositories must not collide, got {outcomes:?}"
            );
            assert_eq!(*code, LoreErrorCode::None, "got {outcomes:?}");
            assert!(!revision.is_zero(), "got {outcomes:?}");
        }
        assert_ne!(
            outcomes[0].2, outcomes[1].2,
            "two repositories must not publish one revision, got {outcomes:?}"
        );

        for (task, repository) in repositories.iter().enumerate() {
            let reloaded = load_on(store, *repository, outcomes[task].2).await;
            assert_eq!(
                child_names(reloaded, ROOT_NODE).await,
                vec![format!("r{task}.bin")],
                "each revision must hold only its own repository's tree"
            );
            close_handle(reloaded).await;
        }
        for handle in handles {
            close_handle(handle).await;
        }
    }

    /// Reads and writes racing on the pending metadata buffer. A key is either not there yet or
    /// carries the value it was written with; a read that returned some other value would mean
    /// the buffer was observed mid-write.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_metadata_set_and_metadata_get_do_not_race() {
        const SETTERS: u64 = 4;
        const PER_SETTER: u64 = 8;
        const READERS: u64 = 4;
        const READS_EACH: u64 = 12;
        const KEYS: u64 = SETTERS * PER_SETTER;

        let handle = load_handle(Partition::from([0x25u8; 16])).await;

        let mut setters: JoinSet<()> = JoinSet::new();
        for setter in 0..SETTERS {
            lore_spawn!(setters, async move {
                for index in 0..PER_SETTER {
                    let key = setter * PER_SETTER + index;
                    let (sink, callback) = make_sink();
                    let status = metadata_set(
                        LoreGlobalArgs::default(),
                        LoreRevisionTreeMetadataSetArgs {
                            batch_id: CALL_ID,
                            handle,
                            entries: LoreArray::from_vec(vec![LoreRevisionTreeMetadataSetEntry {
                                entry_id: key + 1,
                                key: LoreString::from_str(&format!("k-{key}")),
                                value: LoreMetadata::Numeric(key),
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(
                        status,
                        0,
                        "a concurrent set must succeed, got {:?}",
                        sink.lock().unwrap()
                    );
                }
            });
        }

        let mut readers: JoinSet<()> = JoinSet::new();
        for _ in 0..READERS {
            lore_spawn!(readers, async move {
                for _ in 0..READS_EACH {
                    let (sink, callback) = make_sink();
                    let status = metadata_get(
                        LoreGlobalArgs::default(),
                        LoreRevisionTreeMetadataGetArgs {
                            batch_id: CALL_ID,
                            handle,
                            include_revision: 0,
                            entries: LoreArray::from_vec(
                                (0..KEYS)
                                    .map(|key| LoreRevisionTreeMetadataGetEntry {
                                        entry_id: key + 1,
                                        key: LoreString::from_str(&format!("k-{key}")),
                                    })
                                    .collect(),
                            ),
                        },
                        callback,
                    )
                    .await;
                    let events = sink.lock().unwrap().clone();
                    assert_eq!(
                        status, 0,
                        "a read must tolerate a key not set yet, got {events:?}"
                    );
                    for event in &events {
                        if let Captured::MetadataGetComplete(entry_id, key, value, code) = event {
                            assert_eq!(*code, LoreErrorCode::None, "got {events:?}");
                            let expected = entry_id - 1;
                            assert_eq!(key.as_str(), format!("k-{expected}"), "got {events:?}");
                            assert_eq!(
                                *value,
                                LoreMetadata::Numeric(expected),
                                "a reported key must carry the value it was written with"
                            );
                        }
                    }
                }
            });
        }

        setters.join_all().await;
        readers.join_all().await;

        let (sink, callback) = make_sink();
        let status = metadata_get(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataGetArgs {
                batch_id: CALL_ID,
                handle,
                include_revision: 0,
                entries: LoreArray::from_vec(
                    (0..KEYS)
                        .map(|key| LoreRevisionTreeMetadataGetEntry {
                            entry_id: key + 1,
                            key: LoreString::from_str(&format!("k-{key}")),
                        })
                        .collect(),
                ),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(status, 0, "got {events:?}");
        let reported = events
            .iter()
            .filter(|event| matches!(event, Captured::MetadataGetComplete(..)))
            .count();
        assert_eq!(
            reported as u64, KEYS,
            "every concurrently written key must be present once the writers are done"
        );

        close_handle(handle).await;
    }
}
