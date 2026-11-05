// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! H13: a directory's stale copy, `child` included, written over a concurrent add. The stage's
//! update of a node it found in the tree, against an add landing between its read of the node and
//! its writes, driven through the `StageWindow` scheduling point.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::filter::FilterStates;
use lore_revision::fs::filesystem_provider::InstanceOperation as _;
use lore_revision::lore::RepositoryId;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::stage::KnownChild;
use lore_revision::stage::StageCaseChange;
use lore_revision::stage::StageOptions;
use lore_revision::stage::stage_node_from_metadata;
use lore_revision::state::NodeMapping;
use lore_revision::state::State;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Point;
use lore_revision::state::node::hooks::Resource;
use lore_revision::util::path::RelativePath;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::hash::hash_string;

use crate::helper::TestRepository;
use crate::helper::test_commit_tree;
use crate::helper::test_file_write;
use crate::helper::test_repository_create;
use crate::helper::test_store_create;

/// Adds `added.o` to `directory` the first time a stage reaches its window, and records its id.
struct AddInWindow {
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    directory: NodeID,
    fired: AtomicBool,
    added: Arc<AtomicU32>,
}

impl hooks::Scheduler for AddInWindow {
    fn reached(&self, point: Point) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        if point != Point::StageWindow || self.fired.swap(true, Ordering::SeqCst) {
            return None;
        }
        let (state, repository) = (self.state.clone(), self.repository.clone());
        let (directory, added) = (self.directory, self.added.clone());
        Some(Box::pin(async move {
            let node = Node {
                flags: NodeFlags::File.bits(),
                name_hash: hash_string("added.o"),
                ..Node::default()
            };
            let id = state
                .node_add(repository, directory, node, "added.o")
                .await
                .expect("an add into the directory");
            added.store(id, Ordering::SeqCst);
        }))
    }

    fn touched(&self, _resource: Resource, _write: bool) {}
}

/// The stores of [`test_store_create`].
type Stores = (Arc<dyn ImmutableStore>, Arc<dyn MutableStore>);

/// A stage of directory `d`, committed holding `a.txt` in `stores`, its node's mode set to
/// `mode` first, spelled `spelled` in the working tree, with `options`, while `added.o` is added
/// to `d` the first time the stage reaches its window. Answers the instance, kept alive, the
/// State and `d`'s node, having checked the added node is still in `d`'s chain.
async fn stage_with_an_add_in_window(
    (immutable_store, mutable_store): Stores,
    mode: u16,
    spelled: &str,
    options: StageOptions,
) -> (TestRepository, Arc<State>, NodeID) {
    let instance = test_repository_create(
        immutable_store,
        mutable_store,
        RepositoryId::from(uuid::Uuid::now_v7()),
    )
    .await;
    #[allow(clippy::disallowed_methods)]
    std::fs::create_dir(instance.path.join("d")).expect("mkdir d");
    test_file_write(&instance.path.join("d/a.txt"), b"a");
    let state = test_commit_tree(&instance, "First").await;
    let repository = instance.repository.clone();
    let directory = state
        .find_subnode(repository.clone(), ROOT_NODE, hash_string("d"))
        .await
        .expect("d");
    if mode != 0 {
        let block = state
            .block(repository.clone(), NodeBlock::index(directory))
            .await
            .expect("d's block");
        assert!(state.node_change_in_place(
            &block,
            directory,
            (ROOT_NODE, hash_string("d")),
            |held| {
                held.set_mode(mode);
                true
            },
        ));
    }
    if spelled != "d" {
        #[allow(clippy::disallowed_methods)]
        std::fs::rename(instance.path.join("d"), instance.path.join(spelled)).expect("respell d");
    }
    let added = Arc::new(AtomicU32::new(INVALID_NODE));
    let installed = hooks::install(Arc::new(AddInWindow {
        state: state.clone(),
        repository: repository.clone(),
        directory,
        fired: AtomicBool::new(false),
        added: added.clone(),
    }));
    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .expect("a filesystem operation");
    let info = operation
        .file_info(&RelativePath::new().join(spelled))
        .await
        .expect("d's file information");

    stage_node_from_metadata(
        &operation,
        NodeMapping::root(repository.clone(), state.clone()),
        spelled.to_string(),
        info,
        options,
        Arc::default(),
        None,
        KnownChild::Resolved(Some(directory)),
        FilterStates::ROOT,
    )
    .await
    .expect("the stage of d");
    drop(installed);
    operation.finalize().await.expect("the operation finishes");

    let added = added.load(Ordering::SeqCst);
    assert_ne!(added, INVALID_NODE, "the add ran inside the stage's window");
    assert_eq!(
        state
            .find_subnode(repository.clone(), directory, hash_string("added.o"))
            .await
            .expect("added.o is in d's chain"),
        added
    );
    (instance, state, directory)
}

/// H13: a stage updating a committed directory, as a merge stage does with `StagedMerge`, keeps
/// a child added to the directory between its read of the directory and its update: the update
/// writes no stale copy of the directory, whose `child` would drop the add from the chain.
#[tokio::test]
async fn a_directory_staged_while_a_child_is_added_keeps_the_child() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let options = StageOptions {
                node_flags: NodeFlags::StagedMerge,
                ..StageOptions::default()
            };
            let stores = (immutable_store, mutable_store);
            let (instance, state, directory) =
                stage_with_an_add_in_window(stores, 0, "d", options).await;
            let staged = state
                .node(instance.repository.clone(), directory)
                .await
                .expect("d");
            assert!(staged.is_staged_merge(), "the stage marked d");
        })
        .await;
}

/// H13: a stage clearing a directory's mode in its window keeps a child added meanwhile.
#[tokio::test]
async fn a_directory_whose_mode_the_stage_clears_keeps_a_child_added_meanwhile() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let stores = (immutable_store, mutable_store);
            let (instance, state, directory) =
                stage_with_an_add_in_window(stores, 0o755, "d", StageOptions::default()).await;
            let staged = state
                .node(instance.repository.clone(), directory)
                .await
                .expect("d");
            assert_eq!(staged.mode, 0, "the stage cleared d's mode");
        })
        .await;
}

/// H13: a stage renaming a directory's case in its window, as `--case rename` does, keeps a
/// child added meanwhile.
#[tokio::test]
async fn a_directory_whose_case_the_stage_renames_keeps_a_child_added_meanwhile() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let options = StageOptions {
                case_change: StageCaseChange::Rename,
                ..StageOptions::default()
            };
            let stores = (immutable_store, mutable_store);
            let (instance, state, directory) =
                stage_with_an_add_in_window(stores, 0, "D", options).await;
            let name = state
                .node_name_clone(instance.repository.clone(), directory)
                .await
                .expect("d's name");
            assert_eq!(name, "D", "the stage took the working tree's case");
        })
        .await;
}
