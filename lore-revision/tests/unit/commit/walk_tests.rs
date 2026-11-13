// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::BytesMut;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::commit::*;
use lore_revision::fs::filesystem_provider::FilesystemProvider;
use lore_revision::fs::os::OsFilesystem;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::Mark;
use lore_revision::state::RecordedModifiedTimes;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;
use lore_storage::write_tracker::WriteTracker;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

use crate::fs::filesystem_provider::test_store_create;
use crate::helper::default_repository_creation_args;
use crate::helper::in_memory;

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
            .node_mark(
                &fixture.repository,
                conflicted,
                Mark::staged_and_dirty(NodeFlags::StagedMergeConflict, NodeFlags::NoFlags),
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

/// A commit from a link's mount commits the staged file outside it with the content it was
/// staged with, though the working tree holds no file there, and the mounted file from the
/// working tree.
#[tokio::test]
async fn a_commit_from_a_mount_takes_the_staged_content_outside_it() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-walk-outside-commits-");
    let root = dir.to_path_buf();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");

    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let fixture = WalkFixture::new(root, immutable_store, mutable_store).await;
        let staged = stored(file("s.bin"));
        let (mounted, settings) = fixture.subtree_beside(staged, "s.bin").await;
        let mounted_file = fixture.mounted_file(mounted).await;

        fixture
            .commit_from_mount(mounted)
            .await
            .expect("The commit must take the staged content outside the mount");

        let committed = fixture.node(settings).await;
        assert_eq!(committed.address, staged.address);
        assert!(!committed.is_staged(), "the file outside must be committed");
        let from_disk = fixture.node(mounted_file).await;
        assert!(
            !from_disk.address.is_zero() && !from_disk.is_staged(),
            "the mounted file must be committed from the working tree"
        );
    }))
    .await
    .expect("Test task failed");
}

/// Outside a link's mount the commit refuses an unresolved conflict, a file or a link whose
/// content is not stored yet, which only a working tree could supply, and the deletion of a
/// directory holding the mount.
#[tokio::test]
async fn a_commit_from_a_mount_refuses_what_it_cannot_take_as_staged_outside_it() {
    let conflict = refused_outside_a_mount(Outside::Conflict).await;
    assert!(conflict.is_conflict(), "Expected Conflict, got {conflict}");

    let not_stored = refused_outside_a_mount(Outside::NotStored).await;
    assert!(
        not_stored.is_not_supported(),
        "Expected NotSupported, got {not_stored}"
    );

    let deleted_mount = refused_outside_a_mount(Outside::DeletedMount).await;
    assert!(
        deleted_mount.is_not_supported(),
        "Expected NotSupported, got {deleted_mount}"
    );

    let uncommitted_pin = refused_outside_a_mount(Outside::UncommittedPin).await;
    assert!(
        uncommitted_pin.is_not_supported(),
        "Expected NotSupported, got {uncommitted_pin}"
    );
}

/// What a commit from a link's mount refuses outside the mount.
enum Outside {
    /// A stored file in an unresolved conflict.
    Conflict,
    /// A file with content but no address.
    NotStored,
    /// A link pinned at a state never committed.
    UncommittedPin,
    /// The deletion of the directory holding the mount.
    DeletedMount,
}

/// The refusal of a commit from a link's mount with `outside` staged beside the mount.
async fn refused_outside_a_mount(outside: Outside) -> CommitError {
    let dir = lore_base::test_util::TempDir::new("lore-commit-walk-outside-refuses-");
    let root = dir.to_path_buf();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");

    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let fixture = WalkFixture::new(root, immutable_store, mutable_store).await;
        let node = match outside {
            Outside::Conflict | Outside::DeletedMount => stored(file("s.bin")),
            Outside::NotStored => file("s.bin"),
            Outside::UncommittedPin => {
                let linked = RepositoryId::from(uuid::Uuid::now_v7());
                let pin = State::new()
                    .serialize(
                        fixture.repository.to_link_context(linked).await,
                        &fixture.token,
                    )
                    .await
                    .expect("serializing the linked state");
                in_memory::link_node("s.bin", linked, pin)
            }
        };
        let (mounted, beside) = fixture.subtree_beside(node, "s.bin").await;
        fixture.mounted_file(mounted).await;
        let marked = match outside {
            Outside::Conflict => Some((beside, NodeFlags::StagedMergeConflict)),
            Outside::DeletedMount => {
                Some((fixture.node(mounted).await.parent, NodeFlags::StagedDelete))
            }
            Outside::NotStored | Outside::UncommittedPin => None,
        };
        if let Some((node, flags)) = marked {
            fixture
                .state
                .node_mark(&fixture.repository, node, Mark::staged(flags, true))
                .await
                .expect("marking the node must succeed");
        }
        fixture
            .commit_from_mount(mounted)
            .await
            .expect_err("the commit must refuse the node outside the mount")
    }))
    .await
    .expect("Test task failed")
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
            .node_mark(
                &self.repository,
                node_id,
                Mark::staged_and_dirty(NodeFlags::StagedAdd, NodeFlags::DirtyAdd),
            )
            .await
            .expect("marking the addition must succeed");
        node_id
    }

    /// Stages `content/assets`, the subtree a link mounts, and `node` as `name` in `config`
    /// beside it, and answers the nodes of the subtree and of `node`.
    async fn subtree_beside(&self, node: Node, name: &str) -> (NodeID, NodeID) {
        let content = self.add(ROOT_NODE, directory("content"), "content").await;
        let assets = self.add(content, directory("assets"), "assets").await;
        let config = self.add(ROOT_NODE, directory("config"), "config").await;
        (assets, self.add(config, node, name).await)
    }

    /// Stages `a.bin` in `mounted` with no content stored, as a file staged from the working tree
    /// is, and writes it to the working tree at the mount `linked/repo`.
    #[allow(clippy::disallowed_methods)] // A test fixture in its own temporary directory.
    async fn mounted_file(&self, mounted: NodeID) -> NodeID {
        let mount = self.root.join("linked/repo");
        std::fs::create_dir_all(&mount).expect("creating the mount");
        std::fs::write(mount.join("a.bin"), b"0123456789").expect("writing the mounted file");
        self.add(mounted, file("a.bin"), "a.bin").await
    }

    /// A walk of the fixture's tree, over an operation on its working tree.
    async fn commit_walk(&self) -> (CommitWalk, mpsc::Receiver<FileToCommit>) {
        CommitWalk::new(
            false,
            FilesystemProvider::begin_operation(&OsFilesystem::new(&self.root))
                .await
                .expect("Opening an operation"),
            self.repository.clone(),
            self.token.share(),
            self.state.clone(),
            Arc::new(Metadata::new()),
            Arc::new(HashMap::new()),
            self.stats.clone(),
            BranchId::from(uuid::Uuid::now_v7()),
            Arc::new(WriteTracker::new()),
        )
    }

    /// Commits the tree from `mounted`, the subtree a link mounts at `linked/repo`.
    async fn commit_from_mount(&self, mounted: NodeID) -> Result<(), CommitError> {
        let (walk, file_rx) = self.commit_walk().await;
        commit_walks(
            walk,
            file_rx,
            RelativePath::from_str("linked/repo").expect("a valid path"),
            mounted,
            BasedOn::FirstParent,
            Arc::new(RecordedModifiedTimes::below(0)),
            false,
        )
        .await
        .map(drop)
    }

    async fn node(&self, node_id: NodeID) -> Node {
        let block = self
            .state
            .block(self.repository.clone(), NodeBlock::index(node_id))
            .await
            .expect("reading the node's block");
        *block.read().node(Node::index(node_id))
    }

    /// Walks the tree from the root with no directory budget, and returns the walk's result and
    /// the files it sent, in the order sent.
    async fn walk(&self) -> (Result<(), CommitError>, Vec<(NodeID, String)>) {
        let (file_tx, mut file_rx) = mpsc::channel(16);
        let walk = Arc::new(CommitWalk {
            hands_directories: false,
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

/// `node` with its content stored, as a merge stages a file outside a link's mount.
fn stored(node: Node) -> Node {
    Node {
        address: Address {
            hash: Hash::from_u64(0x5e),
            context: Context::default(),
        },
        ..node
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
