// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::BytesMut;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::commit::*;
use lore_revision::fs::filesystem_provider::FilesystemProvider;
use lore_revision::fs::os::OsFilesystem;
use lore_revision::lore::BranchId;
use lore_revision::metadata::Metadata;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;
use lore_storage::write_tracker::WriteTracker;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

use crate::fs::filesystem_provider::test_store_create;
use crate::helper::default_repository_creation_args;

/// Past the directory budget the walk enters each subdirectory where it finds it, and resumes its
/// parent once it is left, so the files arrive in the tree's depth-first order. `node_add`
/// prepends, so the root walks `a`, `mid.bin`, `c`, and `a` walks `early.bin`, `b`, `late.bin`: a
/// file after a subdirectory arrives after everything below it.
#[tokio::test]
async fn a_walk_past_the_budget_enters_each_subdirectory_where_it_finds_it() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-walk-test-");
    let root = dir.to_path_buf();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");

    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let fixture = WalkFixture::new(root, immutable_store, mutable_store).await;
        let c = fixture.add(ROOT_NODE, directory("c"), "c").await;
        let c1 = fixture.add(c, file("c1.bin"), "c1.bin").await;
        let mid = fixture.add(ROOT_NODE, file("mid.bin"), "mid.bin").await;
        let a = fixture.add(ROOT_NODE, directory("a"), "a").await;
        let late = fixture.add(a, file("late.bin"), "late.bin").await;
        let b = fixture.add(a, directory("b"), "b").await;
        let b1 = fixture.add(b, file("b1.bin"), "b1.bin").await;
        let early = fixture.add(a, file("early.bin"), "early.bin").await;

        let (result, sent) = fixture.walk().await;

        result.expect("The walk must commit the tree");
        let expected: Vec<(NodeID, String)> = [
            (early, "a/early.bin"),
            (b1, "a/b/b1.bin"),
            (late, "a/late.bin"),
            (mid, "mid.bin"),
            (c1, "c/c1.bin"),
        ]
        .into_iter()
        .map(|(node_id, path)| (node_id, path.to_owned()))
        .collect();
        assert_eq!(
            sent, expected,
            "the files must arrive once each, in depth-first order"
        );
        assert_eq!(
            fixture
                .stats
                .complete
                .directory_count
                .load(Ordering::Relaxed),
            4,
            "each directory holding a file must be counted once"
        );
    }))
    .await
    .expect("Test task failed");
}

/// An unresolved conflict in a directory the walk entered past the budget stops the walk: the
/// conflict is returned, and the root's file after that directory is never sent.
#[tokio::test]
async fn a_failure_past_the_budget_stops_the_walk() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-walk-test-");
    let root = dir.to_path_buf();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");

    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let fixture = WalkFixture::new(root, immutable_store, mutable_store).await;
        fixture.add(ROOT_NODE, file("after.bin"), "after.bin").await;
        let a = fixture.add(ROOT_NODE, directory("a"), "a").await;
        let conflicted = fixture.add(a, directory("conflicted"), "conflicted").await;
        fixture
            .state
            .node_mark_staged(
                fixture.repository.clone(),
                conflicted,
                NodeFlags::StagedMergeConflict,
                NodeFlags::NoFlags,
            )
            .await
            .expect("marking the conflict must succeed");
        let before = fixture.add(a, file("before.bin"), "before.bin").await;

        let (result, sent) = fixture.walk().await;

        let failure = result.expect_err("an unresolved conflict must stop the walk");
        assert!(failure.is_conflict(), "Expected Conflict, got {failure}");
        assert_eq!(
            sent,
            vec![(before, "a/before.bin".to_owned())],
            "the walk must send nothing after the conflict"
        );
    }))
    .await
    .expect("Test task failed");
}

/// A staged tree, and what a walk of it with no directory budget reports to.
struct WalkFixture {
    root: PathBuf,
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    state: Arc<State>,
    stats: Arc<CommitStats>,
}

impl WalkFixture {
    /// Call from inside a `LORE_CONTEXT` scope: the commit counters read the execution context.
    async fn new(
        root: PathBuf,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
    ) -> Self {
        let token = RepositoryWriteToken::acquire(&root).await;
        let repository = Arc::new(
            RepositoryContext::new(default_repository_creation_args(
                immutable_store,
                mutable_store,
            ))
            .with_write_token(token.share()),
        );
        Self {
            root,
            repository,
            token,
            state: State::new(),
            stats: CommitStats::new(),
        }
    }

    /// Adds `node` under `parent` staged as an addition, which also stages its ancestors.
    async fn add(&self, parent: NodeID, node: Node, name: &str) -> NodeID {
        let node_id = self
            .state
            .node_add(self.repository.clone(), parent, node, name)
            .await
            .expect("adding the node must succeed");
        self.state
            .node_mark_staged(
                self.repository.clone(),
                node_id,
                NodeFlags::StagedAdd,
                NodeFlags::DirtyAdd,
            )
            .await
            .expect("marking the addition must succeed");
        node_id
    }

    /// Walks the tree from the root with no directory budget, and returns the walk's result and
    /// the files it sent, in the order sent.
    async fn walk(&self) -> (Result<(), CommitError>, Vec<(NodeID, String)>) {
        let (file_tx, mut file_rx) = mpsc::channel(16);
        let walk = Arc::new(CommitWalk {
            operation: FilesystemProvider::begin_operation(&OsFilesystem::new(&self.root))
                .await
                .expect("Opening an operation"),
            repository: self.repository.clone(),
            token: self.token.share(),
            state: self.state.clone(),
            delta: Arc::new(parking_lot::RwLock::new(BytesMut::new())),
            discard: Arc::default(),
            subnodes_to_discard: Arc::default(),
            file_tx,
            metadata: Arc::new(Metadata::new()),
            link_messages: Arc::new(HashMap::new()),
            stats: self.stats.clone(),
            parent_branch: BranchId::from(uuid::Uuid::now_v7()),
            tracker: Arc::new(WriteTracker::new()),
            dir_semaphore: Arc::new(Semaphore::new(0)),
        });
        let result = commit_directory(walk, RelativePath::new(), ROOT_NODE, None).await;
        let mut sent = Vec::new();
        while let Some(file) = file_rx.recv().await {
            sent.push((file.node_id, file.relative_path.as_str().to_owned()));
        }
        (result, sent)
    }
}

fn file(name: &str) -> Node {
    Node {
        flags: NodeFlags::File.bits(),
        mode: 0o644,
        size: 10,
        name_hash: hash_string(name),
        ..Default::default()
    }
}

fn directory(name: &str) -> Node {
    Node {
        flags: NodeFlags::NoFlags.bits(),
        mode: 0o755,
        name_hash: hash_string(name),
        ..Default::default()
    }
}
