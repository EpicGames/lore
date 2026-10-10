// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::types::Address;
use lore_base::types::Hash;
use lore_revision::change::FileAction;
use lore_revision::filter::FilterMode;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::immutable;
use lore_revision::instance::InstanceId;
use lore_revision::link::LinkFlags;
use lore_revision::lore::*;
use lore_revision::nametable::NameTable;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::util::path::RelativePath;
use lore_revision::util::path::RelativePathBuf;
use lore_storage::hash;
use zerocopy::FromZeros;

mod diff;
mod stream;

/// Each action the walk settles on records a dirty flag and the staged flag standing
/// for the same change.
#[test]
fn an_action_records_matching_dirty_and_staged_flags() {
    use lore_revision::node::NodeFlags;
    use lore_revision::state::SettledAction;

    for (action, dirty, staged) in [
        (
            SettledAction::Add,
            NodeFlags::DirtyAdd,
            NodeFlags::StagedAdd,
        ),
        (
            SettledAction::Modify,
            NodeFlags::DirtyModify,
            NodeFlags::StagedModify,
        ),
        (
            SettledAction::Move,
            NodeFlags::DirtyMove,
            NodeFlags::StagedMove,
        ),
        (
            SettledAction::Delete,
            NodeFlags::DirtyDelete,
            NodeFlags::StagedDelete,
        ),
    ] {
        assert_eq!(dirty, action.dirty(), "dirty flag for {action:?}");
        assert_eq!(staged, action.staged(), "staged flag for {action:?}");
    }
}

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::state::*;

use crate::fs::filesystem_provider::setup_test_execution;

/// The key every stored modification time is filed under. It has to stay the
/// digest over the path's own lowercase form, or a scan finds nothing it wrote
/// and rehashes every file in the repository.
fn mtime_key_reference(salt: &[u8], instance: InstanceId, path: &RelativePath) -> Hash {
    hash::hash_function_args_slice(
        salt,
        FILE_MTIME,
        instance.data(),
        path.as_str().to_lowercase().as_bytes(),
    )
}

/// Taking the fold [`RelativePath`] carries is only the same key as folding the
/// path here where the two folds agree, which above ASCII is not free: the
/// mapping can change a component's length and can depend on where in a word a
/// character falls.
#[test]
fn the_mtime_key_is_the_digest_over_the_paths_own_lowercase_form() {
    let salt = b"lore";
    let instance = InstanceId::default();
    for name in [
        "Rock.mesh",
        "Assets/Meshes/ROCK.MESH",
        "MIXED_Case-123/PATH/To.TXT",
        // Above ASCII: a fold that changes the length, one that depends on
        // position in a word, and one of each across a separator.
        "\u{0130}stanbul/Map.umap",
        "\u{039f}\u{0394}\u{039f}\u{03a3}",
        "\u{039f}\u{0394}\u{039f}\u{03a3}/Stra\u{00df}e/\u{1e9e}.uasset",
    ] {
        let path = RelativePath::new_from_initial_path(name).expect("a clean relative path");
        assert_eq!(
            file_modified_time_key(salt, instance, &path),
            mtime_key_reference(salt, instance, &path),
            "{name:?}"
        );
    }
}

/// A path built up a component at a time is what the clone and walk paths hand
/// in, and it folds each component as it is appended rather than the whole.
#[test]
fn a_pushed_path_keys_the_same_as_the_whole_of_it() {
    let salt = b"lore";
    let instance = InstanceId::default();
    let mut buf = RelativePathBuf::new();
    buf.push("\u{039f}\u{0394}\u{039f}\u{03a3}");
    buf.push("Stra\u{00df}E");
    buf.push("\u{0130}.uasset");
    let pushed = buf.freeze();
    assert_eq!(
        file_modified_time_key(salt, instance, &pushed),
        mtime_key_reference(salt, instance, &pushed)
    );
}

/// The lowercase form carries offsets of its own, since a fold can change a
/// component's byte length, so a path narrowed to a suffix has to key as that
/// suffix and not as a window into the wrong one.
#[test]
fn a_path_narrowed_to_a_suffix_keys_as_that_suffix() {
    let salt = b"lore";
    let instance = InstanceId::default();
    let mut narrowed = RelativePath::new_from_initial_path("\u{0130}\u{0130}/Assets/Rock.mesh")
        .expect("a clean relative path");
    narrowed.pop_root();
    assert_eq!(narrowed.as_str(), "Assets/Rock.mesh");
    let whole =
        RelativePath::new_from_initial_path("Assets/Rock.mesh").expect("a clean relative path");
    assert_eq!(
        file_modified_time_key(salt, instance, &narrowed),
        file_modified_time_key(salt, instance, &whole)
    );
    assert_eq!(
        file_modified_time_key(salt, instance, &narrowed),
        mtime_key_reference(salt, instance, &narrowed)
    );
}

#[test]
fn resolve_branch_returns_parent_when_branch_is_zero() {
    let link_ref = LinkReference {
        branch: BranchId::default(),
        ..LinkReference::default()
    };
    let parent = BranchId::from([1u8; 16]);
    assert_eq!(link_ref.resolve_branch(parent), parent);
}

#[test]
fn resolve_branch_returns_own_branch_when_non_zero() {
    let own_branch = BranchId::from([2u8; 16]);
    let link_ref = LinkReference {
        branch: own_branch,
        ..LinkReference::default()
    };
    let parent = BranchId::from([1u8; 16]);
    assert_eq!(link_ref.resolve_branch(parent), own_branch);
}

/// A state with no serialized link list, so the registry lives only in the runtime copy and
/// every read has to come from there.
async fn null_repository() -> Arc<RepositoryContext> {
    null_repository_excluding(&[]).await
}

/// [`null_repository`] whose ignore filter excludes `globs`.
async fn null_repository_excluding(globs: &[&str]) -> Arc<RepositoryContext> {
    let immutable_store = lore_storage::local::immutable_store::create(
        None::<&str>,
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::ImmutableStoreSettings::default(),
    )
    .await
    .expect("in-memory immutable store");
    let mutable_store = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        immutable_store.clone(),
    )
    .await
    .expect("in-memory mutable store");

    let mut filter = lore_revision::filter::Filter::default();
    for glob in globs {
        filter.ignore.add_exclusion(glob).expect("filter exclusion");
    }

    let mut context = RepositoryContext::new_null_context(immutable_store, mutable_store);
    context.filter = Arc::new(filter);
    Arc::new(context)
}

/// A repository with a working tree, so a file on disk can be measured against a node.
async fn working_tree_repository(path: &std::path::Path) -> Arc<RepositoryContext> {
    let immutable_store = lore_storage::local::immutable_store::create(
        None::<&str>,
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::ImmutableStoreSettings::default(),
    )
    .await
    .expect("in-memory immutable store");
    let mutable_store = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        immutable_store.clone(),
    )
    .await
    .expect("in-memory mutable store");

    // Built with the path rather than assigned one afterwards, so the filesystem provider is
    // rooted where the working tree is.
    use crate::repository::test_helpers::RepositoryContextCreationArgsExt;
    Arc::new(RepositoryContext::new(
        crate::repository::test_helpers::default_repository_creation_args(
            immutable_store,
            mutable_store,
        )
        .with_path(path),
    ))
}

fn pseudo_random_bytes(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| (index.wrapping_mul(2_654_435_761) >> 11) as u8)
        .collect()
}

fn content_node(content: &[u8]) -> Node {
    let mut node = Node::new_zeroed();
    node.address = Address::zero_context_hash(Hash::hash_buffer(content));
    node.size = content.len() as u64;
    node
}

/// A file written into the working tree, and the path it is known by.
async fn write_working_file(
    repository: &Arc<RepositoryContext>,
    name: &str,
    content: &[u8],
) -> RelativePath {
    let path = lore_revision::util::path::RelativePathBuf::new().push_and_freeze(name);
    lore_io::IoDriver::global()
        .write_file_bytes(
            path.to_absolute_path(repository.require_path().expect("working tree")),
            bytes::Bytes::copy_from_slice(content),
            false,
        )
        .await
        .expect("write working file");
    path
}

/// An operation on the repository's own filesystem, which is what every caller compares
/// through.
async fn working_operation(repository: &RepositoryContext) -> Arc<InstanceOperationImpl> {
    repository
        .file_system()
        .begin_operation()
        .await
        .expect("beginning an operation")
}

/// What one comparison established answers the next, which is what spares a file measured
/// against several addresses being read once for each.
///
/// The file is removed between the two: a comparison that reached the working tree again
/// would find nothing there and report it unreadable.
#[tokio::test]
async fn what_one_comparison_established_answers_the_next() {
    let dir = lore_base::test_util::TempDir::new("lore-state-test-");
    let repository = working_tree_repository(dir.path()).await;
    let content = pseudo_random_bytes(4 * 1024);
    let path = write_working_file(&repository, "established.bin", &content).await;
    let operation = working_operation(&repository).await;
    let established = lore_storage::ContentHashes::default();

    let mut first = content_node(&content);
    first.address = Address::zero_context_hash(Hash::hash_buffer(b"one address"));
    assert!(matches!(
        file_matches_node(
            repository.clone(),
            &first,
            first.size,
            &path,
            &operation,
            &established,
        )
        .await
        .expect("comparing a readable file must not error"),
        NodeComparison::Differs
    ));

    lore_io::IoDriver::global()
        .remove_file(path.to_absolute_path(repository.require_path().expect("working tree")))
        .await
        .expect("remove working file");

    let mut second = content_node(&content);
    second.address = Address::zero_context_hash(Hash::hash_buffer(b"another address"));
    assert!(matches!(
        file_matches_node(
            repository,
            &second,
            second.size,
            &path,
            &operation,
            &established,
        )
        .await
        .expect("an established comparison reads nothing"),
        NodeComparison::Differs
    ));
}

/// A file removed under a run of comparisons reads as unreadable, not as differing.
///
/// Nothing established stands in for the file being there, so the size is measured afresh for
/// every comparison. Reported unmodified, a routine deletion does not read as local work.
#[tokio::test]
async fn a_file_removed_between_comparisons_reads_as_unreadable() {
    let dir = lore_base::test_util::TempDir::new("lore-state-test-");
    let repository = working_tree_repository(dir.path()).await;
    // Above the minimum cut, so no established hash can answer on its own.
    let content = pseudo_random_bytes(100 * 1024);
    let path = write_working_file(&repository, "removed.bin", &content).await;
    let operation = working_operation(&repository).await;
    let established = lore_storage::ContentHashes::default();

    let mut first = content_node(&content);
    first.address = Address::zero_context_hash(Hash::hash_buffer(b"one address"));
    assert!(matches!(
        file_matches_node(
            repository.clone(),
            &first,
            first.size,
            &path,
            &operation,
            &established,
        )
        .await
        .expect("comparing a readable file must not error"),
        NodeComparison::Differs
    ));

    lore_io::IoDriver::global()
        .remove_file(path.to_absolute_path(repository.require_path().expect("working tree")))
        .await
        .expect("remove working file");

    let mut second = content_node(&content);
    second.address = Address::zero_context_hash(Hash::hash_buffer(b"another address"));
    assert!(matches!(
        file_matches_node(
            repository,
            &second,
            second.size,
            &path,
            &operation,
            &established,
        )
        .await
        .expect("a removed file is not an error"),
        NodeComparison::Unreadable
    ));
}

/// A comparison that settles nothing has to read as modified. The address names a list
/// nothing stored, so neither the stored chunking nor a rehash can answer, and
/// overwriting a file that may hold local work is the one outcome there is no
/// recovering from.
#[tokio::test]
async fn a_comparison_that_settles_nothing_reads_as_modified() {
    let dir = lore_base::test_util::TempDir::new("lore-state-test-");
    let repository = working_tree_repository(dir.path()).await;
    let content = pseudo_random_bytes(150 * 1024);
    let path = write_working_file(&repository, "settles-nothing.bin", &content).await;

    let mut node = content_node(&content);
    node.address = Address::zero_context_hash(Hash::hash_buffer(b"a list nothing stored"));

    let operation = working_operation(&repository).await;
    let established = lore_storage::ContentHashes::default();
    assert!(matches!(
        file_matches_node(
            repository.clone(),
            &node,
            node.size,
            &path,
            &operation,
            &established,
        )
        .await
        .expect("comparing a readable file must not error"),
        NodeComparison::Differs
    ));
    assert!(
        file_modification(
            repository,
            &node,
            1,
            node.size,
            &path,
            true,
            &operation,
            &established,
        )
        .await
        .expect("comparing a readable file must not error")
        .is_modified()
    );
}

/// Content addressed by its own hash needs nothing from the store to be decided,
/// either way.
#[tokio::test]
async fn an_unfragmented_file_is_decided_by_its_own_hash() {
    let dir = lore_base::test_util::TempDir::new("lore-state-test-");
    let repository = working_tree_repository(dir.path()).await;
    let content = pseudo_random_bytes(20 * 1024);
    let path = write_working_file(&repository, "unfragmented.bin", &content).await;
    let node = content_node(&content);

    let operation = working_operation(&repository).await;
    let established = lore_storage::ContentHashes::default();
    assert!(matches!(
        file_matches_node(
            repository.clone(),
            &node,
            node.size,
            &path,
            &operation,
            &established,
        )
        .await
        .expect("comparing a readable file must not error"),
        NodeComparison::Matches
    ));

    let mut edited = content.clone();
    edited[content.len() / 2] ^= 0xff;
    lore_io::IoDriver::global()
        .write_file_bytes(
            path.to_absolute_path(repository.require_path().expect("working tree")),
            bytes::Bytes::from(edited),
            false,
        )
        .await
        .expect("rewrite working file");

    // What the first comparison established answers for the content it read, so the rewrite
    // starts again.
    let established = lore_storage::ContentHashes::default();
    assert!(
        file_modification(
            repository,
            &node,
            1,
            node.size,
            &path,
            true,
            &operation,
            &established,
        )
        .await
        .expect("comparing a readable file must not error")
        .is_modified()
    );
}

/// A file of another size is modified without the file being read at all.
#[tokio::test]
async fn a_file_of_another_size_is_modified_unread() {
    let dir = lore_base::test_util::TempDir::new("lore-state-test-");
    let repository = working_tree_repository(dir.path()).await;
    let content = pseudo_random_bytes(20 * 1024);
    let path = write_working_file(&repository, "resized.bin", &content).await;
    let node = content_node(&content[..content.len() - 1]);

    let operation = working_operation(&repository).await;
    assert!(matches!(
        file_modification(
            repository,
            &node,
            1,
            content.len() as u64,
            &path,
            false,
            &operation,
            &lore_storage::ContentHashes::default(),
        )
        .await
        .expect("comparing a readable file must not error"),
        FileModification::ModifiedBySize
    ));
}

fn link_id(byte: u8) -> RepositoryId {
    RepositoryId::from([byte; 16])
}

/// An update made after the registry was read applies to the entry it names and leaves the
/// other entries as they were.
#[tokio::test]
async fn reading_the_link_list_leaves_it_editable() {
    let repository = null_repository().await;
    let state = State::new();

    state
        .link_add(
            repository.clone(),
            link_id(1),
            BranchId::default(),
            Hash::from([1u8; 32]),
            2,
            LinkFlags::NoFlags,
        )
        .await
        .expect("adding the first link");
    state
        .link_add(
            repository.clone(),
            link_id(2),
            BranchId::default(),
            Hash::from([2u8; 32]),
            3,
            LinkFlags::NoFlags,
        )
        .await
        .expect("adding the second link");

    let read = state
        .link_list(repository.clone())
        .await
        .expect("reading the registry");
    assert_eq!(read.len(), 2, "both links must be registered");

    state
        .link_update(
            repository.clone(),
            link_id(1),
            BranchId::default(),
            Hash::from([9u8; 32]),
            2,
        )
        .await
        .expect("updating a link after the registry was read");

    let updated = state
        .link_list(repository)
        .await
        .expect("re-reading the registry");
    assert_eq!(updated.len(), 2, "the update must not drop the other link");
    assert_eq!(
        updated[0].signature,
        Hash::from([9u8; 32]),
        "the update must be visible"
    );
    assert_eq!(
        updated[1].signature,
        Hash::from([2u8; 32]),
        "the untouched link must keep its signature"
    );
}

/// A node carrying `flags`, named for the name table by its hash.
fn dirty_node(name: &str, flags: NodeFlags) -> Node {
    Node {
        name_hash: lore_storage::hash::hash_string(name),
        flags: flags.bits(),
        ..Default::default()
    }
}

/// Adds `name` under `parent` with `flags` and returns it.
async fn add_dirty_node(
    state: &State,
    repository: Arc<RepositoryContext>,
    parent: NodeID,
    name: &str,
    flags: NodeFlags,
) -> NodeID {
    state
        .node_add(repository, parent, dirty_node(name, flags), name)
        .await
        .expect("adding a node to the walked tree")
}

/// The dirty paths of the whole tree, sorted, so the assertions do not depend
/// on the order the sibling chain happens to hold.
async fn walk_dirty_paths(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    options: DirtyWalkOptions,
) -> Vec<String> {
    let mut collected = walk_dirty_paths_in_order(state, repository, options).await;
    collected.sort();
    collected
}

/// An empty name on a directory, which is descended, so the name is taken off
/// where its level ends rather than where the loop body does.
///
/// The directory appended nothing, so its level must take nothing off. A level
/// that popped unconditionally would remove its parent's own last component,
/// and the sibling walked next would be named against the wrong parent.
#[tokio::test]
async fn an_empty_directory_name_leaves_its_parent_on_the_path() {
    let repository = null_repository().await;
    let state = State::new();

    let file = NodeFlags::DirtyModify | NodeFlags::File;
    let outer = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "outer",
        NodeFlags::Dirty,
    )
    .await;
    // Added first so the prepending chain walks it last, after the empty name.
    add_dirty_node(&state, repository.clone(), outer, "after.txt", file).await;
    let unnamed = add_dirty_node(&state, repository.clone(), outer, "", NodeFlags::Dirty).await;
    add_dirty_node(&state, repository.clone(), unnamed, "inner.txt", file).await;

    assert_eq!(
        walk_dirty_paths_in_order(state, repository, DirtyWalkOptions::default()).await,
        vec!["outer/inner.txt", "outer/after.txt"],
        "the sibling after an empty-named directory keeps its parent's prefix"
    );
}

/// `U+0130` folds to two scalars, so the directory is one byte longer in the
/// buffer's lowercase form than in its written one. The walk pops the whole
/// component off both, or the sibling that follows inherits what is left.
#[tokio::test]
async fn a_component_whose_fold_is_longer_is_popped_off_both_forms() {
    let repository = null_repository().await;
    let state = State::new();

    let folded = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "MESH_\u{130}",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        folded,
        "inner.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "after.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec!["MESH_\u{130}/inner.txt", "after.txt"],
        "the sibling is named against the root, not against what the fold left"
    );
}

/// A sibling chain longer than one node block, so the walk crosses a block
/// boundary part way along it.
///
/// [`BlockCursor`] holds the block it last read and only fetches another when
/// the node it is asked for is not in that one. A cursor that never moved
/// would read the wrong nodes from the block it opened on, so every chain a
/// test walks has to be long enough to leave it.
#[tokio::test]
async fn a_sibling_chain_spanning_node_blocks_is_walked_whole() {
    const CHILDREN: usize = lore_revision::node::BLOCK_NODE_COUNT + 200;

    let repository = null_repository().await;
    let state = State::new();
    let directory = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "spanning",
        NodeFlags::Dirty,
    )
    .await;
    for index in 0..CHILDREN {
        add_dirty_node(
            &state,
            repository.clone(),
            directory,
            &format!("file_{index:04}.txt"),
            NodeFlags::DirtyModify | NodeFlags::File,
        )
        .await;
    }

    let walked = walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await;
    let expected: Vec<String> = (0..CHILDREN)
        .map(|index| format!("spanning/file_{index:04}.txt"))
        .collect();
    assert_eq!(
        walked, expected,
        "every child is recorded once, whichever block it lives in"
    );
}

/// Points `node`'s sibling link at `sibling`, forging a chain no tree edit
/// produces.
async fn forge_sibling(
    state: &State,
    repository: Arc<RepositoryContext>,
    node: NodeID,
    sibling: NodeID,
) {
    let block = state
        .block(repository, NodeBlock::index(node))
        .await
        .expect("the block holding the node");
    block.write().node(Node::index(node)).sibling = sibling;
}

/// The dirty paths under `parent_node`, in the order the walk records them.
async fn walk_dirty_paths_under(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node: NodeID,
    options: DirtyWalkOptions,
) -> Result<Vec<String>, StateError> {
    let mut paths = Vec::new();
    collect_dirty_paths_inner(
        state,
        repository,
        parent_node,
        &mut RelativePathBuf::new(),
        &mut paths,
        options,
    )
    .await?;
    Ok(paths.iter().map(|p| p.as_str().to_string()).collect())
}

/// The dirty paths of the whole tree, in the order the walk records them.
async fn walk_dirty_paths_in_order(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    options: DirtyWalkOptions,
) -> Vec<String> {
    walk_dirty_paths_under(state, repository, ROOT_NODE, options)
        .await
        .expect("walking the tree")
}

/// Adding a node prepends it, so each level's chain is the reverse of the
/// order its children were added in here.
#[tokio::test]
async fn dirty_paths_are_recorded_depth_first_along_each_sibling_chain() {
    let repository = null_repository().await;
    let state = State::new();

    let gamma = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "gamma",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "beta.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    let alpha = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "alpha",
        NodeFlags::DirtyAdd,
    )
    .await;

    let deep = add_dirty_node(&state, repository.clone(), alpha, "deep", NodeFlags::Dirty).await;
    add_dirty_node(
        &state,
        repository.clone(),
        alpha,
        "one.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        deep,
        "two.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    add_dirty_node(
        &state,
        repository.clone(),
        gamma,
        "four.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        gamma,
        "three.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert_eq!(
        walk_dirty_paths_in_order(state, repository, DirtyWalkOptions::default()).await,
        vec![
            "alpha",
            "alpha/one.txt",
            "alpha/deep/two.txt",
            "beta.txt",
            "gamma/three.txt",
            "gamma/four.txt",
        ],
        "each subtree is recorded where its directory sits in its parent's chain"
    );
}

/// A directory that carries an action of its own and is also descended is
/// recorded before anything below it: the action re-creates the directory the
/// paths under it are re-applied into.
#[tokio::test]
async fn a_directory_is_recorded_before_the_paths_under_it() {
    let repository = null_repository().await;
    let state = State::new();

    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "later.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    let added = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "added",
        NodeFlags::DirtyAdd,
    )
    .await;
    let inner = add_dirty_node(
        &state,
        repository.clone(),
        added,
        "inner",
        NodeFlags::DirtyAdd,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        inner,
        "leaf.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert_eq!(
        walk_dirty_paths_in_order(state, repository, DirtyWalkOptions::default()).await,
        vec!["added", "added/inner", "added/inner/leaf.txt", "later.txt"],
        "a directory precedes its descendants, and its whole subtree precedes its sibling"
    );
}

/// A child the walk passes over — clean, staged where staged paths are
/// skipped, or a directory with nothing dirty under it — leaves its level
/// walking: the siblings behind it are still visited, in chain order.
#[tokio::test]
async fn a_passed_over_child_does_not_end_its_sibling_chain() {
    let repository = null_repository().await;
    let state = State::new();

    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "last.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "empty",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "staged.txt",
        NodeFlags::DirtyModify | NodeFlags::StagedModify | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "clean.txt",
        NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "first.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert_eq!(
        walk_dirty_paths_under(
            state,
            repository,
            ROOT_NODE,
            DirtyWalkOptions {
                skip_staged: true,
                force: false
            }
        )
        .await
        .expect("walking the tree"),
        vec!["first.txt", "last.txt"],
        "the chain is walked past a clean child, a staged child and an empty directory"
    );
}

/// Descent costs a stack entry, not a frame, so nesting past the depth the
/// stack is sized for is walked whole and in constant stack.
#[tokio::test]
async fn a_path_nested_deeper_than_the_walk_stack_is_recorded_in_full() {
    const DEPTH: usize = 1024;

    let repository = null_repository().await;
    let state = State::new();

    let mut parent = ROOT_NODE;
    for level in 0..DEPTH {
        parent = add_dirty_node(
            &state,
            repository.clone(),
            parent,
            &format!("d{level}"),
            NodeFlags::Dirty,
        )
        .await;
    }
    add_dirty_node(
        &state,
        repository.clone(),
        parent,
        "leaf.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    let expected = (0..DEPTH)
        .map(|level| format!("d{level}"))
        .chain(std::iter::once("leaf.txt".to_string()))
        .collect::<Vec<String>>()
        .join("/");
    assert_eq!(
        walk_dirty_paths_in_order(state, repository, DirtyWalkOptions::default()).await,
        vec![expected],
        "the only dirty node is the leaf, named under all {DEPTH} levels above it"
    );
}

/// Every level guards its own sibling chain, so a cycle is reported against
/// the directory whose chain holds it and not against the walk's root.
#[tokio::test]
async fn a_sibling_cycle_below_the_root_is_reported_against_its_own_parent() {
    let repository = null_repository().await;
    let state = State::new();

    let sub = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "sub",
        NodeFlags::Dirty,
    )
    .await;
    let second = add_dirty_node(
        &state,
        repository.clone(),
        sub,
        "second.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    let first = add_dirty_node(
        &state,
        repository.clone(),
        sub,
        "first.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    forge_sibling(&state, repository.clone(), second, first).await;

    let error = walk_dirty_paths_under(
        state,
        repository,
        ROOT_NODE,
        DirtyWalkOptions {
            skip_staged: false,
            force: false,
        },
    )
    .await
    .expect_err("a sibling chain that loops must be reported");
    let hierarchy = error
        .as_invalid_node_hierarchy()
        .unwrap_or_else(|| panic!("a looping chain is an invalid hierarchy, got {error}"));
    assert_eq!(
        hierarchy.expected_parent, sub,
        "the guard that tripped belongs to the level holding the cycle"
    );
}

/// Only a directory holds a chain to walk. A link's children live in another
/// repository's state and a file has none, so a walk rooted at either records
/// nothing, and from above they are recorded without being descended.
#[tokio::test]
async fn a_link_or_file_walk_root_is_not_descended() {
    let repository = null_repository().await;
    let state = State::new();

    let link = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "link",
        NodeFlags::DirtyModify | NodeFlags::Link,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        link,
        "linked.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    let file = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "file.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        file,
        "under.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert!(
        walk_dirty_paths_under(
            state.clone(),
            repository.clone(),
            link,
            DirtyWalkOptions {
                skip_staged: false,
                force: false
            }
        )
        .await
        .expect("walking a link")
        .is_empty(),
        "a link is not descended"
    );
    assert!(
        walk_dirty_paths_under(
            state.clone(),
            repository.clone(),
            file,
            DirtyWalkOptions {
                skip_staged: false,
                force: false
            }
        )
        .await
        .expect("walking a file")
        .is_empty(),
        "a file is not descended"
    );
    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec!["file.txt", "link"],
        "both are recorded from above, neither is descended"
    );
}

/// A `DirtyDelete` or `DirtyMove` directory is re-applied whole, so the walk
/// records it and stops. Descending would collect descendants the parent
/// action already covers.
///
/// A directory that is merely propagated-dirty is the opposite case and is in
/// the same tree: it carries no action of its own, contributes no path, and
/// must still be descended to reach the file that made it dirty.
#[tokio::test]
async fn a_deleted_or_moved_directory_is_recorded_without_descending() {
    let repository = null_repository().await;
    let state = State::new();

    let removed = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "removed",
        NodeFlags::DirtyDelete,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        removed,
        "inside.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    let moved = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "moved",
        NodeFlags::DirtyMove,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        moved,
        "carried.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    let touched = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "touched",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        touched,
        "edited.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec!["moved", "removed", "touched/edited.txt"],
        "a deleted or moved directory is recorded and not descended, \
             and a propagated-dirty directory is descended and not recorded"
    );
}

/// The buffer the walk names into carries the whole ancestry, and a sibling
/// visited after a descent is named against its own parent again.
///
/// A child is prepended into the sibling chain, so the subdirectory added
/// last at each level is walked first and the file beside it is named once the
/// descent has returned.
#[tokio::test]
async fn a_nested_dirty_file_carries_the_whole_prefix() {
    let repository = null_repository().await;
    let state = State::new();

    let file = NodeFlags::DirtyModify | NodeFlags::File;
    let mut parent = ROOT_NODE;
    for (directory, name) in [
        ("assets", "a.txt"),
        ("meshes", "b.txt"),
        ("rock", "c.txt"),
        ("detail", "d.txt"),
    ] {
        parent = add_dirty_node(
            &state,
            repository.clone(),
            parent,
            directory,
            NodeFlags::Dirty,
        )
        .await;
        add_dirty_node(&state, repository.clone(), parent, name, file).await;
    }
    add_dirty_node(&state, repository.clone(), parent, "e.txt", file).await;

    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec![
            "assets/a.txt",
            "assets/meshes/b.txt",
            "assets/meshes/rock/c.txt",
            "assets/meshes/rock/detail/d.txt",
            "assets/meshes/rock/detail/e.txt",
        ],
    );
}

/// Three subtrees under one parent, each of a different depth. Every path is
/// named against the parent and not against whatever the subtree walked
/// before it left behind.
#[tokio::test]
async fn sibling_subtrees_at_one_depth_are_named_against_their_parent() {
    let repository = null_repository().await;
    let state = State::new();

    let file = NodeFlags::DirtyModify | NodeFlags::File;
    for (subtree, depth) in [("left", 1usize), ("middle", 2), ("right", 3)] {
        let mut parent = add_dirty_node(
            &state,
            repository.clone(),
            ROOT_NODE,
            subtree,
            NodeFlags::Dirty,
        )
        .await;
        for level in 0..depth {
            add_dirty_node(&state, repository.clone(), parent, "leaf.txt", file).await;
            parent = add_dirty_node(
                &state,
                repository.clone(),
                parent,
                &format!("level_{level}"),
                NodeFlags::Dirty,
            )
            .await;
        }
        add_dirty_node(&state, repository.clone(), parent, "leaf.txt", file).await;
    }

    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec![
            "left/leaf.txt",
            "left/level_0/leaf.txt",
            "middle/leaf.txt",
            "middle/level_0/leaf.txt",
            "middle/level_0/level_1/leaf.txt",
            "right/leaf.txt",
            "right/level_0/leaf.txt",
            "right/level_0/level_1/leaf.txt",
            "right/level_0/level_1/level_2/leaf.txt",
        ],
    );
}

/// A directory carrying an action of its own is recorded and then descended,
/// and the path recorded for it is its own however deep its children go.
#[tokio::test]
async fn a_directory_is_recorded_before_it_is_descended() {
    let repository = null_repository().await;
    let state = State::new();

    let file = NodeFlags::DirtyModify | NodeFlags::File;
    add_dirty_node(&state, repository.clone(), ROOT_NODE, "tail.txt", file).await;
    let added = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "added",
        NodeFlags::DirtyAdd,
    )
    .await;
    add_dirty_node(&state, repository.clone(), added, "leaf.txt", file).await;
    let deeper = add_dirty_node(
        &state,
        repository.clone(),
        added,
        "deeper",
        NodeFlags::DirtyAdd,
    )
    .await;
    add_dirty_node(&state, repository.clone(), deeper, "deep.txt", file).await;

    assert_eq!(
        walk_dirty_paths_in_order(state, repository, DirtyWalkOptions::default()).await,
        vec![
            "added",
            "added/deeper",
            "added/deeper/deep.txt",
            "added/leaf.txt",
            "tail.txt",
        ],
        "a directory is recorded before it is descended, and its own path \
             is not extended by what its children append"
    );
}

/// A node the filter excludes is neither recorded nor descended, and the
/// siblings walked after it are still named against their own parent.
///
/// The excluded node sits in the middle of the chain: children are prepended,
/// so `blocked` is walked between `gamma` and `alpha`.
#[tokio::test]
async fn siblings_after_a_filtered_node_keep_their_own_prefix() {
    let repository = null_repository_excluding(&["blocked"]).await;
    let state = State::new();

    let file = NodeFlags::DirtyModify | NodeFlags::File;
    let alpha = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "alpha",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(&state, repository.clone(), alpha, "one.txt", file).await;
    let blocked = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "blocked",
        NodeFlags::DirtyAdd,
    )
    .await;
    add_dirty_node(&state, repository.clone(), blocked, "hidden.txt", file).await;
    let gamma = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "gamma",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(&state, repository.clone(), gamma, "two.txt", file).await;

    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec!["alpha/one.txt", "gamma/two.txt"],
        "the excluded subtree is pruned and the sibling after it is named \
             against the root"
    );
}

/// A node whose name is empty names its parent, since
/// [`RelativePathBuf::push`] ignores an empty component. The sibling after it
/// is still named against the parent, so nothing was taken off for a
/// component that was never appended.
#[tokio::test]
async fn an_empty_node_name_names_its_parent() {
    let repository = null_repository().await;
    let state = State::new();

    let file = NodeFlags::DirtyModify | NodeFlags::File;
    let outer = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "outer",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(&state, repository.clone(), outer, "x.txt", file).await;
    add_dirty_node(&state, repository.clone(), outer, "", file).await;

    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec!["outer", "outer/x.txt"],
    );
}

/// The commit walk records nothing for a node that is also staged: its action
/// belongs to the revision being written. Every other caller wants both.
#[tokio::test]
async fn a_staged_node_is_recorded_only_when_staged_nodes_are_wanted() {
    let repository = null_repository().await;
    let state = State::new();

    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "staged.txt",
        NodeFlags::DirtyModify | NodeFlags::File | NodeFlags::StagedModify,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "dirty_only.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert_eq!(
        walk_dirty_paths(
            state.clone(),
            repository.clone(),
            DirtyWalkOptions {
                skip_staged: true,
                force: false,
            },
        )
        .await,
        vec!["dirty_only.txt"],
        "a staged node is already in the commit and must not be re-applied"
    );
    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec!["dirty_only.txt", "staged.txt"],
        "without skip_staged both carry an action to record"
    );
}

/// A path the filter excludes cannot be re-applied against a checkout that
/// never materializes it, so it is not recorded. `force` records it anyway.
#[tokio::test]
async fn a_filtered_path_is_recorded_only_under_force() {
    let repository = null_repository_excluding(&["ignored.txt"]).await;
    let state = State::new();

    for name in ["ignored.txt", "kept.txt"] {
        add_dirty_node(
            &state,
            repository.clone(),
            ROOT_NODE,
            name,
            NodeFlags::DirtyModify | NodeFlags::File,
        )
        .await;
    }

    assert_eq!(
        walk_dirty_paths(
            state.clone(),
            repository.clone(),
            DirtyWalkOptions::default()
        )
        .await,
        vec!["kept.txt"],
        "an excluded path has no checkout to be re-applied against"
    );
    assert_eq!(
        walk_dirty_paths(
            state,
            repository,
            DirtyWalkOptions {
                skip_staged: false,
                force: true,
            },
        )
        .await,
        vec!["ignored.txt", "kept.txt"],
        "force bypasses the filter"
    );
}

/// Excluding a directory prunes its subtree: the filter is asked about the
/// directory before the walk descends into it, so a path below an excluded
/// directory is never reached.
#[tokio::test]
async fn an_excluded_directory_is_not_descended() {
    let repository = null_repository_excluding(&["build"]).await;
    let state = State::new();

    let build = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "build",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        build,
        "artifact.o",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "kept.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert_eq!(
        walk_dirty_paths(
            state.clone(),
            repository.clone(),
            DirtyWalkOptions::default()
        )
        .await,
        vec!["kept.txt"],
        "the subtree of an excluded directory is not walked"
    );
    assert_eq!(
        walk_dirty_paths(
            state,
            repository,
            DirtyWalkOptions {
                skip_staged: false,
                force: true,
            },
        )
        .await,
        vec!["build/artifact.o", "kept.txt"],
        "force reaches what the exclusion pruned"
    );
}

/// The walk descends only directories. Nothing below a link is in this state,
/// and `Node::child` means nothing on a file - it holds a modification time.
#[tokio::test]
async fn walking_from_anything_but_a_directory_records_nothing() {
    let repository = null_repository().await;
    let state = State::new();

    let file = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "file.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    let link = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "link",
        NodeFlags::Dirty | NodeFlags::Link,
    )
    .await;

    for (label, node) in [("a file", file), ("a link", link)] {
        let mut paths = Vec::new();
        collect_dirty_paths_inner(
            state.clone(),
            repository.clone(),
            node,
            &mut RelativePathBuf::new(),
            &mut paths,
            DirtyWalkOptions::default(),
        )
        .await
        .expect("walking from a non-directory");
        assert!(paths.is_empty(), "walking from {label} records nothing");
    }
}

/// Most states are read for their header alone, so the block-loading permits
/// are allocated by the first block load and not by reading the state.
#[tokio::test]
async fn the_block_loading_permits_are_allocated_by_the_first_block_load() {
    let repository = null_repository().await;
    let state = State::new();
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "file.txt",
        NodeFlags::File,
    )
    .await;
    let token = repository
        .try_write_token()
        .expect("a null context carries a write token");
    let signature = state
        .serialize(repository.clone(), token)
        .await
        .expect("serializing the state");

    let loaded = State::deserialize(repository.clone(), signature)
        .await
        .expect("deserializing the state");
    assert!(
        loaded.block_loading.get().is_none(),
        "reading the state allocated the permits"
    );

    loaded
        .block(repository.clone(), 0)
        .await
        .expect("loading the first node block");
    assert!(
        loaded.block_loading.get().is_some(),
        "loading a block allocated no permits"
    );
}

/// The changes a diff from `from` to `to` emits, as each one's action and path, in the order
/// it emits them. The diff runs in an execution context, which the events it sends need.
async fn diff_in_order(
    repository: Arc<RepositoryContext>,
    from: Arc<State>,
    to: Arc<State>,
) -> Vec<(FileAction, String)> {
    let walk = async move {
        ChangeStream::spawn(async move |changes| {
            diff(
                repository.clone(),
                from,
                repository,
                to,
                None,
                None,
                &changes,
                FilterMode::Full,
            )
            .await
        })
        .collect()
        .await
    };
    LORE_CONTEXT
        .scope(setup_test_execution(), walk)
        .await
        .expect("diffing the trees")
        .iter()
        .map(|change| {
            let path = change.resolved_side().mapping.path.as_str().to_string();
            (change.action, path)
        })
        .collect()
}

/// A directory added or deleted as a whole is emitted depth first along each sibling chain,
/// and a link below it is emitted without what it holds.
///
/// Adding a node prepends it, so each level's chain is the reverse of the order its children
/// are added in here.
#[tokio::test]
async fn a_hierarchy_is_emitted_depth_first_along_each_sibling_chain() {
    let repository = null_repository().await;
    let empty = State::new();
    let tree = State::new();
    let directory = NodeFlags::NoFlags;

    let top = add_dirty_node(&tree, repository.clone(), ROOT_NODE, "top", directory).await;
    let mount = add_dirty_node(&tree, repository.clone(), top, "mount", NodeFlags::Link).await;
    add_dirty_node(
        &tree,
        repository.clone(),
        mount,
        "held.txt",
        NodeFlags::File,
    )
    .await;
    add_dirty_node(&tree, repository.clone(), top, "empty", directory).await;
    add_dirty_node(
        &tree,
        repository.clone(),
        top,
        "middle.txt",
        NodeFlags::File,
    )
    .await;
    let first = add_dirty_node(&tree, repository.clone(), top, "first", directory).await;
    let inner = add_dirty_node(&tree, repository.clone(), first, "inner", directory).await;
    add_dirty_node(&tree, repository.clone(), inner, "two.txt", NodeFlags::File).await;
    add_dirty_node(&tree, repository.clone(), first, "one.txt", NodeFlags::File).await;

    let order = [
        "top",
        "top/first",
        "top/first/one.txt",
        "top/first/inner",
        "top/first/inner/two.txt",
        "top/middle.txt",
        "top/empty",
        "top/mount",
    ];
    for (action, from, to) in [
        (FileAction::Add, empty.clone(), tree.clone()),
        (FileAction::Delete, tree, empty),
    ] {
        let expected: Vec<_> = order
            .iter()
            .map(|path| (action, path.to_string()))
            .collect();
        assert_eq!(
            diff_in_order(repository.clone(), from, to).await,
            expected,
            "a {action:?} emits each directory before what it holds, and nothing a link holds"
        );
    }
}

/// A child the filter excludes, and a directory with an empty name, leave the walk's path as
/// their parent's, so the siblings after them are named under it. The unnamed directory is
/// named as its parent, which it appends nothing to.
#[tokio::test]
async fn siblings_after_an_excluded_or_unnamed_child_are_named_under_their_parent() {
    let repository = null_repository_excluding(&["skipped"]).await;
    let tree = State::new();
    let directory = NodeFlags::NoFlags;

    let top = add_dirty_node(&tree, repository.clone(), ROOT_NODE, "top", directory).await;
    add_dirty_node(&tree, repository.clone(), top, "last.txt", NodeFlags::File).await;
    let unnamed = add_dirty_node(&tree, repository.clone(), top, "", directory).await;
    add_dirty_node(
        &tree,
        repository.clone(),
        unnamed,
        "inner.txt",
        NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &tree,
        repository.clone(),
        top,
        "middle.txt",
        NodeFlags::File,
    )
    .await;
    let skipped = add_dirty_node(&tree, repository.clone(), top, "skipped", directory).await;
    add_dirty_node(
        &tree,
        repository.clone(),
        skipped,
        "hidden.txt",
        NodeFlags::File,
    )
    .await;

    let expected: Vec<_> = [
        "top",
        "top/middle.txt",
        "top",
        "top/inner.txt",
        "top/last.txt",
    ]
    .iter()
    .map(|path| (FileAction::Add, path.to_string()))
    .collect();
    assert_eq!(
        diff_in_order(repository, State::new(), tree).await,
        expected,
        "nothing under the excluded directory is emitted, and every sibling keeps the prefix"
    );
}

/// Descent costs a stack entry, not a frame, so a hierarchy a thousand levels deep is emitted
/// whole, in the stack a shallow one takes.
#[tokio::test]
async fn a_deeply_nested_hierarchy_is_emitted_in_full() {
    const DEPTH: usize = 1024;

    let repository = null_repository().await;
    let tree = State::new();

    let mut parent = ROOT_NODE;
    let mut path = RelativePath::new();
    let mut expected = Vec::with_capacity(DEPTH + 1);
    for level in 0..DEPTH {
        let name = format!("d{level}");
        parent = add_dirty_node(&tree, repository.clone(), parent, &name, NodeFlags::NoFlags).await;
        path = path.push_into_buf(&name).freeze();
        expected.push((FileAction::Add, path.as_str().to_string()));
    }
    add_dirty_node(
        &tree,
        repository.clone(),
        parent,
        "leaf.txt",
        NodeFlags::File,
    )
    .await;
    expected.push((
        FileAction::Add,
        path.push_into_buf("leaf.txt").freeze().as_str().to_string(),
    ));

    assert_eq!(
        diff_in_order(repository, State::new(), tree).await,
        expected,
        "every one of the {DEPTH} levels is emitted, and the leaf below them"
    );
}

/// A file metadata block's prefetch and load, and the deprecated name table's load, keep the
/// fields of the tree they read rather than the tree, so none holds a tree across its read.
#[tokio::test]
async fn tree_readers_hold_its_fields_not_the_tree() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let repository = null_repository().await;
            let state = State::new();
            let options = immutable::read_options_from_repository(&repository);

            let list_read = immutable::read(repository.clone(), Address::default(), None, options);
            let name_table_read = NameTable::deserialize(repository.clone(), Hash::default());
            let prefetch = state.block_file_metadata_cache(repository.clone(), 0);
            let load = state.block_file_metadata_load(repository.clone(), 0);
            let name_table = state.nametable_load(repository);

            for (what, future, read) in [
                ("prefetch", size_of_val(&prefetch), size_of_val(&list_read)),
                ("load", size_of_val(&load), size_of_val(&list_read)),
                (
                    "name table load",
                    size_of_val(&name_table),
                    size_of_val(&name_table_read),
                ),
            ] {
                assert!(
                    future < read + size_of::<Tree>(),
                    "the {what} holds {future} bytes over a read of {read}"
                );
            }
        })
        .await;
}

/// Files under `fill/` in [`stored_with_two_blocks`]: with the root and `fill/`, they run past the
/// first node block.
const FILL_NODES: usize = BLOCK_NODE_COUNT;

/// A stored revision of two node blocks: the root holds `fill/`, and `fill/` holds
/// [`FILL_NODES`] files running into the second.
async fn stored_with_two_blocks(repository: &Arc<RepositoryContext>) -> Hash {
    let state = State::new();
    let fill = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "fill",
        NodeFlags::NoFlags,
    )
    .await;
    for index in 0..FILL_NODES {
        let name = format!("f{index}");
        add_dirty_node(&state, repository.clone(), fill, &name, NodeFlags::File).await;
    }
    let token = repository
        .try_write_token()
        .expect("a null context carries a write token");
    state
        .serialize(repository.clone(), token)
        .await
        .expect("the State serializes")
}

/// A State with no node block in memory puts a fresh block past the tree's blocks, its block
/// count grows by one rather than shrinking to the fresh block's, and serializing it keeps the
/// blocks it never loaded.
#[tokio::test]
async fn a_fresh_block_of_a_state_with_no_block_in_memory_goes_past_the_trees_blocks() {
    let repository = null_repository().await;
    let stored = stored_with_two_blocks(&repository).await;
    let state = State::deserialize(repository.clone(), stored)
        .await
        .expect("the State loads");
    let tree = state.tree(repository.clone()).await.expect("the tree");
    let count = state.block_count();
    assert_eq!(count, tree.block_count as usize);
    assert_eq!(count, 2, "the tree holds two blocks");

    let (index, block) = state.allocate_fresh_block().expect("a fresh block");
    assert_eq!(index, count, "the fresh block goes past the tree's blocks");
    assert_eq!(
        state.block_count(),
        count + 1,
        "the block count grows by one"
    );
    let found = state
        .block(repository.clone(), index)
        .await
        .expect("the fresh block");
    assert!(
        Arc::ptr_eq(&found, &block),
        "the fresh block is found at its index"
    );

    state.mark_dirty();
    let token = repository
        .try_write_token()
        .expect("a null context carries a write token");
    let reserialized = state
        .serialize(repository.clone(), token)
        .await
        .expect("the State serializes");
    let reloaded = State::deserialize(repository.clone(), reserialized)
        .await
        .expect("the State reloads");
    let fill = reloaded
        .find_subnode(repository.clone(), ROOT_NODE, hash::hash_string("fill"))
        .await
        .expect("the root still holds fill");
    assert_eq!(
        reloaded
            .node_children(repository, fill)
            .await
            .expect("fill's children")
            .len(),
        FILL_NODES,
        "fill still holds its files"
    );
}
