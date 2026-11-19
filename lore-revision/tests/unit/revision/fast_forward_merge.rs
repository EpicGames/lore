// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! A fast-forward merge stores the revision `lore branch merge` of the pushed revision commits on
//! the branch latest: the same tree, node for node, the same delta and the same file history.

use std::collections::BTreeMap;

use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::TypedBytes as _;
use lore_revision::branch;
use lore_revision::change::FileAction;
use lore_revision::commit::merge_tree_changes;
use lore_revision::filter::FilterMode;
use lore_revision::metadata::MetadataType;
use lore_revision::node::NodeDelta;
use lore_revision::node::NodeFileMetadata;
use lore_revision::node::NodeFileMetadataBlock;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::node::node_to_file_metadata;
use lore_revision::revision::diff3_collect;
use lore_revision::state::diff_collect;

use super::instance::Edit;
use super::instance::Instance;
use super::instance::commit;
use super::instance::commit_edits;
use super::instance::create_branch;
use super::instance::current;
use super::instance::merge_branch;
use super::instance::revision_state;
use super::instance::run;
use super::instance::stage;
use super::instance::switch_to;
use super::instance::write;

/// The files every fixture starts from, each holding its own path.
const TREE: [&str; 4] = ["Dir/a.txt", "Dir/b.txt", "Dir/Sub/c.txt", "Top.txt"];

/// A node of a stored tree: its ID, address hash, file ID, size, mode and flags.
type Stored = (NodeID, Hash, Context, u64, u16, u16);

/// What a file's metadata records: the metadata blob and, per parent line, the revision, the node,
/// the action and the flags of its last change.
type History = (Hash, [Hash; 2], [u32; 2], [u16; 2], [u16; 2]);

/// What a merge revision records, compared between a fast-forward merge and a client merge.
#[derive(Debug, PartialEq)]
struct Recorded {
    parents: [Hash; 2],
    revision_number: u64,
    root: Hash,
    tree: BTreeMap<String, Stored>,
    /// The delta's entries in node order: a commit of a working tree records them in the order it
    /// walks the tree, which differs from the freeze's.
    delta: Vec<(NodeID, u16, u16)>,
    history: BTreeMap<String, History>,
}

/// The tree, delta and file history `revision` records.
async fn recorded(instance: &Instance, revision: Hash) -> Recorded {
    let state = revision_state(instance, revision).await;
    let repository = instance.repository.clone();
    let (mut tree, mut history) = (BTreeMap::new(), BTreeMap::new());
    let mut pending = vec![(ROOT_NODE, String::new())];
    while let Some((parent, prefix)) = pending.pop() {
        for child in state
            .node_children(repository.clone(), parent)
            .await
            .expect("Failed to list the children")
        {
            let node = state
                .node(repository.clone(), child)
                .await
                .expect("Failed to read a node");
            let name = state
                .node_name_clone(repository.clone(), child)
                .await
                .expect("Failed to read a name");
            let path = format!("{prefix}{name}");
            if node.is_directory() {
                pending.push((child, format!("{path}/")));
            }
            let metadata_node = node_to_file_metadata(child);
            let block = state
                .block_file_metadata(
                    repository.clone(),
                    NodeFileMetadataBlock::index(metadata_node),
                )
                .await
                .expect("Failed to read the file metadata");
            let held = *block.read().node(NodeFileMetadata::index(metadata_node));
            history.insert(
                path.clone(),
                (
                    held.metadata,
                    held.revision,
                    held.node,
                    held.action,
                    held.flags,
                ),
            );
            tree.insert(
                path,
                (
                    child,
                    node.address.hash,
                    node.address.context,
                    node.size,
                    node.mode,
                    node.flags,
                ),
            );
        }
    }
    let mut delta: Vec<_> = state
        .delta_block(repository.clone())
        .await
        .expect("Failed to read the delta")
        .to_aligned::<NodeDelta>()
        .as_type_slice::<NodeDelta>()
        .iter()
        .map(|entry| (entry.node, entry.action, entry.flags))
        .collect();
    delta.sort_unstable();
    Recorded {
        parents: state.parents(),
        revision_number: state.revision_number(),
        root: state
            .tree(repository)
            .await
            .expect("Failed to read the tree")
            .hash_root,
        tree,
        delta,
        history,
    }
}

/// Each path at which `left` and `right` hold different values, with both values.
fn differing<T: PartialEq + std::fmt::Debug>(
    left: &BTreeMap<String, T>,
    right: &BTreeMap<String, T>,
) -> Vec<String> {
    let paths: std::collections::BTreeSet<&String> = left.keys().chain(right.keys()).collect();
    paths
        .into_iter()
        .filter(|path| left.get(*path) != right.get(*path))
        .map(|path| format!("{path}: {:?} != {:?}", left.get(path), right.get(path)))
        .collect()
}

/// Merges `source` into `target`, both made on `base`, as the server's fast-forward merge does,
/// and answers the stored revision.
async fn fast_forward_merge(instance: &Instance, base: Hash, source: Hash, target: Hash) -> Hash {
    let repository = instance.repository.clone();
    let mut diff = diff3_collect(repository.clone(), base, source, target, None, true)
        .await
        .expect("Failed to diff the revisions");
    assert!(diff.conflicts.is_empty(), "{:?}", diff.conflicts);
    let merged = revision_state(instance, target).await;
    merge_tree_changes(
        repository.clone(),
        merged.clone(),
        &revision_state(instance, source).await,
        &mut diff.changes,
    )
    .await
    .expect("Failed to merge the changes");
    merged
        .serialize(repository, &instance.write_token)
        .await
        .expect("Failed to store the merged revision")
}

/// Asserts that each non-delete entry of the delta of `revision` names a node the revision holds
/// at the path the entry resolves to.
async fn assert_delta_names_held_nodes(instance: &Instance, revision: Hash) {
    let state = revision_state(instance, revision).await;
    for (node, action, _) in recorded(instance, revision).await.delta {
        if action == FileAction::Delete as u16 {
            continue;
        }
        let path = state
            .node_path(instance.repository.clone(), node)
            .await
            .expect("Failed to read a delta path");
        let held = state
            .find_node_link(instance.repository.clone(), path.as_str())
            .await
            .expect("Failed to find a delta path")
            .node;
        assert_eq!(held, node, "the delta names {node} at {path}");
    }
}

/// Commits `feature` on a branch of that name and `main` on the main branch, both made on the
/// first revision, merges the feature revision into main as a fast-forward merge does and as
/// `lore branch merge` does, and asserts that both record the same revision ([`assert_recorded`]).
/// Answers the fast-forward merge's revision with its first and second parent.
async fn assert_merges_as_a_client(
    instance: &Instance,
    feature: &[Edit],
    main: &[Edit],
) -> (Hash, Hash, Hash) {
    let base = current(instance).await;
    create_branch(instance, "feature").await;
    let source = commit_edits(instance, feature, "Feature").await;
    let target = commit_main(instance, main).await;
    let merged = assert_recorded(instance, base, source, target).await;
    (merged, target, source)
}

/// Switches to the main branch and commits `main` on it, answering the revision.
async fn commit_main(instance: &Instance, main: &[Edit]) -> Hash {
    switch_to(instance, branch::DEFAULT_DEFAULT_NAME).await;
    commit_edits(instance, main, "Main").await
}

/// Merges `source`, the latest of the branch `feature`, into `target`, the current revision on
/// the main branch, both made on `base`, as a fast-forward merge does and as `lore branch merge`
/// does, and asserts that both record the same revision: the parents, the revision number, the
/// tree node for node, the delta and the file metadata. Asserts too that the delta names only
/// nodes the fast-forward merge holds and that the 2-way diff from `base` reports the same changes
/// across both, and answers the fast-forward merge's revision.
async fn assert_recorded(instance: &Instance, base: Hash, source: Hash, target: Hash) -> Hash {
    let merged = fast_forward_merge(instance, base, source, target).await;
    assert!(
        merge_branch(instance, "feature").await,
        "the merge committed"
    );
    let client = current(instance).await;

    let (server, expected) = (
        recorded(instance, merged).await,
        recorded(instance, client).await,
    );
    assert_eq!(server.parents, expected.parents);
    assert_eq!(server.revision_number, expected.revision_number);
    assert_eq!(
        differing(&server.tree, &expected.tree),
        Vec::<String>::new(),
        "tree"
    );
    assert_eq!(server.root, expected.root);
    assert_eq!(server.delta, expected.delta, "delta");
    assert_eq!(
        differing(&server.history, &expected.history),
        Vec::<String>::new(),
        "history"
    );
    assert_delta_names_held_nodes(instance, merged).await;
    assert_eq!(
        changes_since(instance, base, merged).await,
        changes_since(instance, base, client).await
    );
    merged
}

/// Each change the 2-way diff from `base` to `revision` reports, by its action, the path it stands
/// at and the path its node stood at.
async fn changes_since(instance: &Instance, base: Hash, revision: Hash) -> Vec<String> {
    diff_collect(
        instance.repository.clone(),
        revision_state(instance, base).await,
        instance.repository.clone(),
        revision_state(instance, revision).await,
        None,
        FilterMode::Full,
    )
    .await
    .expect("Failed to diff the revisions")
    .iter()
    .map(|change| {
        format!(
            "{:?} {} <- {}",
            change.action,
            change.path(),
            change.from.mapping.path
        )
    })
    .collect()
}

/// The node ID and file ID of the node at `path` in `revision`.
async fn identity(instance: &Instance, revision: Hash, path: &str) -> (NodeID, Context) {
    let state = revision_state(instance, revision).await;
    let node = state
        .find_node_link(instance.repository.clone(), path)
        .await
        .expect("Failed to find the node")
        .node;
    let file = state
        .node(instance.repository.clone(), node)
        .await
        .expect("Failed to read the node")
        .address
        .context;
    (node, file)
}

/// The revisions the first-parent and the second-parent history of the file at `path` name in
/// `revision`.
async fn history_of(instance: &Instance, revision: Hash, path: &str) -> [Hash; 2] {
    let (node, _) = identity(instance, revision, path).await;
    let metadata_node = node_to_file_metadata(node);
    let block = revision_state(instance, revision)
        .await
        .block_file_metadata(
            instance.repository.clone(),
            NodeFileMetadataBlock::index(metadata_node),
        )
        .await
        .expect("Failed to read the file metadata");
    block
        .read()
        .node(NodeFileMetadata::index(metadata_node))
        .revision
}

/// Sets the file metadata key `owner` on the staged file at `path`.
async fn set_owner(instance: &Instance, path: &str) {
    let path = instance.path.join(path);
    lore_revision::metadata::set::set_file(
        instance.repository.clone(),
        &instance.write_token,
        &[path.to_str().expect("A UTF-8 path")],
        &[b"owner"],
        &[b"feature"],
        &[MetadataType::String],
        &[1],
    )
    .await
    .expect("Failed to set the file metadata");
}

/// A file move keeps the moved node and its file ID, and the merge records what a client merge
/// records.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_move_keeps_its_node_and_records_what_a_client_merge_records() {
    run(
        "a_file_move_keeps_its_node_and_records_what_a_client_merge_records",
        &TREE,
        |instance| async move {
            let (merged, target, _) = assert_merges_as_a_client(
                &instance,
                &[Edit::Move("Dir/a.txt", "Dir/z.txt")],
                &[Edit::Write("Top.txt", "main")],
            )
            .await;
            assert_eq!(
                identity(&instance, merged, "Dir/z.txt").await,
                identity(&instance, target, "Dir/a.txt").await
            );
        },
    )
    .await;
}

/// A file move that also changes the file keeps the node and takes the new content.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_move_with_an_edit_keeps_its_node_and_takes_the_edit() {
    run(
        "a_file_move_with_an_edit_keeps_its_node_and_takes_the_edit",
        &TREE,
        |instance| async move {
            let (merged, target, _) = assert_merges_as_a_client(
                &instance,
                &[
                    Edit::Move("Dir/a.txt", "Dir/z.txt"),
                    Edit::Write("Dir/z.txt", "feature"),
                ],
                &[Edit::Write("Top.txt", "main")],
            )
            .await;
            assert_eq!(
                identity(&instance, merged, "Dir/z.txt").await,
                identity(&instance, target, "Dir/a.txt").await
            );
        },
    )
    .await;
}

/// A file move against an edit of the file on the branch latest keeps the node and the edit.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_move_against_an_edit_keeps_its_node_and_the_edit() {
    run(
        "a_file_move_against_an_edit_keeps_its_node_and_the_edit",
        &TREE,
        |instance| async move {
            let (merged, target, _) = assert_merges_as_a_client(
                &instance,
                &[Edit::Move("Dir/a.txt", "Dir/z.txt")],
                &[Edit::Write("Dir/a.txt", "main")],
            )
            .await;
            assert_eq!(
                identity(&instance, merged, "Dir/z.txt").await,
                identity(&instance, target, "Dir/a.txt").await
            );
        },
    )
    .await;
}

/// A directory move with an edit below it records what a client merge records, the moved file
/// keeping its node and file ID, and the file history of the merge names the branch latest for the
/// file it changed and the pushed revision for the file the push changed. The 3-way diff reports
/// the move of the directory itself as a delete and an add, so a new node stands at its new path.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_move_with_an_edit_below_keeps_its_files_and_weaves_both_lines() {
    run(
        "a_directory_move_with_an_edit_below_keeps_its_files_and_weaves_both_lines",
        &TREE,
        |instance| async move {
            let (merged, target, source) = assert_merges_as_a_client(
                &instance,
                &[
                    Edit::Move("Dir/Sub", "Dir/Zub"),
                    Edit::Write("Dir/Zub/c.txt", "feature"),
                ],
                &[Edit::Write("Dir/b.txt", "main")],
            )
            .await;
            assert_eq!(
                identity(&instance, merged, "Dir/Zub/c.txt").await,
                identity(&instance, target, "Dir/Sub/c.txt").await
            );
            assert_eq!(history_of(&instance, merged, "Dir/b.txt").await[0], target);
            assert_eq!(
                history_of(&instance, merged, "Dir/Zub/c.txt").await[1],
                source
            );
        },
    )
    .await;
}

/// A file move that only changes case renames the node in place.
#[tokio::test(flavor = "multi_thread")]
async fn a_case_only_file_move_renames_the_node_in_place() {
    run(
        "a_case_only_file_move_renames_the_node_in_place",
        &TREE,
        |instance| async move {
            let (merged, target, _) = assert_merges_as_a_client(
                &instance,
                &[Edit::Move("Dir/a.txt", "Dir/A.txt")],
                &[Edit::Write("Top.txt", "main")],
            )
            .await;
            assert_eq!(
                identity(&instance, merged, "Dir/A.txt").await,
                identity(&instance, target, "Dir/a.txt").await
            );
        },
    )
    .await;
}

/// A directory move that only changes case renames the node in place, keeping its children.
#[tokio::test(flavor = "multi_thread")]
async fn a_case_only_directory_move_renames_the_node_in_place() {
    run(
        "a_case_only_directory_move_renames_the_node_in_place",
        &TREE,
        |instance| async move {
            let (merged, target, _) = assert_merges_as_a_client(
                &instance,
                &[Edit::Move("Dir/Sub", "Dir/sub")],
                &[Edit::Write("Dir/b.txt", "main")],
            )
            .await;
            for (moved, from) in [("Dir/sub", "Dir/Sub"), ("Dir/sub/c.txt", "Dir/Sub/c.txt")] {
                assert_eq!(
                    identity(&instance, merged, moved).await,
                    identity(&instance, target, from).await,
                    "{moved}"
                );
            }
        },
    )
    .await;
}

/// A delete is recorded as a client merge records it.
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_records_what_a_client_merge_records() {
    run(
        "a_delete_records_what_a_client_merge_records",
        &TREE,
        |instance| async move {
            assert_merges_as_a_client(
                &instance,
                &[Edit::Delete("Dir/b.txt")],
                &[Edit::Write("Top.txt", "main")],
            )
            .await;
        },
    )
    .await;
}

/// An edit both sides make alike is recorded as a client merge records it.
#[tokio::test(flavor = "multi_thread")]
async fn the_same_edit_on_both_sides_records_what_a_client_merge_records() {
    run(
        "the_same_edit_on_both_sides_records_what_a_client_merge_records",
        &TREE,
        |instance| async move {
            assert_merges_as_a_client(
                &instance,
                &[
                    Edit::Write("Top.txt", "same"),
                    Edit::Write("Dir/b.txt", "feature"),
                ],
                &[Edit::Write("Top.txt", "same")],
            )
            .await;
        },
    )
    .await;
}

/// A delete both sides make alike is recorded as a client merge records it.
#[tokio::test(flavor = "multi_thread")]
async fn the_same_delete_on_both_sides_records_what_a_client_merge_records() {
    run(
        "the_same_delete_on_both_sides_records_what_a_client_merge_records",
        &TREE,
        |instance| async move {
            assert_merges_as_a_client(
                &instance,
                &[Edit::Delete("Dir/b.txt"), Edit::Write("Top.txt", "feature")],
                &[Edit::Delete("Dir/b.txt")],
            )
            .await;
        },
    )
    .await;
}

/// A move both sides make alike is recorded as a client merge records it.
#[tokio::test(flavor = "multi_thread")]
async fn the_same_move_on_both_sides_records_what_a_client_merge_records() {
    run(
        "the_same_move_on_both_sides_records_what_a_client_merge_records",
        &TREE,
        |instance| async move {
            assert_merges_as_a_client(
                &instance,
                &[
                    Edit::Move("Dir/a.txt", "Dir/z.txt"),
                    Edit::Write("Top.txt", "feature"),
                ],
                &[Edit::Move("Dir/a.txt", "Dir/z.txt")],
            )
            .await;
        },
    )
    .await;
}

/// A file the push adds with file metadata takes that metadata, as a client merge gives it.
#[tokio::test(flavor = "multi_thread")]
async fn an_added_file_takes_its_file_metadata() {
    run(
        "an_added_file_takes_its_file_metadata",
        &TREE,
        |instance| async move {
            let base = current(&instance).await;
            create_branch(&instance, "feature").await;
            write(&instance, "Dir/new.txt", "feature");
            stage(&instance, &instance.path).await;
            set_owner(&instance, "Dir/new.txt").await;
            let source = commit(&instance, "Feature")
                .await
                .expect("Failed to commit");
            let target = commit_main(&instance, &[Edit::Write("Top.txt", "main")]).await;
            let merged = assert_recorded(&instance, base, source, target).await;
            assert_ne!(
                recorded(&instance, merged).await.history["Dir/new.txt"].0,
                Hash::default()
            );
        },
    )
    .await;
}

/// A file both sides edit alike takes the file metadata the push gives it, as a client merge
/// gives it.
#[tokio::test(flavor = "multi_thread")]
async fn the_same_edit_on_both_sides_takes_the_file_metadata_of_the_push() {
    run(
        "the_same_edit_on_both_sides_takes_the_file_metadata_of_the_push",
        &TREE,
        |instance| async move {
            let base = current(&instance).await;
            create_branch(&instance, "feature").await;
            write(&instance, "Top.txt", "same");
            stage(&instance, &instance.path).await;
            set_owner(&instance, "Top.txt").await;
            let source = commit(&instance, "Feature")
                .await
                .expect("Failed to commit");
            let target = commit_main(&instance, &[Edit::Write("Top.txt", "same")]).await;
            let merged = assert_recorded(&instance, base, source, target).await;
            assert_ne!(
                recorded(&instance, merged).await.history["Top.txt"].0,
                Hash::default()
            );
        },
    )
    .await;
}
