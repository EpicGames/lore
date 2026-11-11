// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::BTreeMap;
use std::sync::Arc;

use futures::FutureExt;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::change::NodeChange;
use lore_revision::commit::merge_tree_changes;
use lore_revision::filter::FilterMode;
use lore_revision::lore::RepositoryId;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::DiffResult;
use lore_revision::revision::diff3_collect;
use lore_revision::state::LinkPin;
use lore_revision::state::State;
use lore_revision::state::apply_tree_changes;
use lore_revision::state::diff_collect;
use lore_storage::hash::hash_string;

use crate::case_folding::case_folding;
use crate::helper::in_memory;
use crate::helper::in_memory::add;
use crate::helper::in_memory::branch_id;
use crate::helper::in_memory::commit;
use crate::helper::in_memory::directory;
use crate::helper::in_memory::file;
use crate::helper::in_memory::revision_state;
use crate::helper::test_store_create;

const KELVIN: &str = "\u{212A}.txt";

/// The repository the link at `Mount` of the base points into, which no test commits to.
const MOUNT_REPOSITORY: uuid::Uuid =
    uuid::Uuid::from_u128(0x0192_6f3c_5d10_7a2b_8c4e_1f2a_3b4c_5d6e);

/// The address hash, size and mode of a node, and for a link its address context and mount
/// source: the repository and the node it points into.
type Content = (Hash, u64, u16, Option<(Context, NodeID)>);

/// A file holding content `version`, which its address hash and its size both name.
fn file_at(name: &str, version: u64) -> Node {
    let mut node = file(name);
    node.address.hash = Hash::from_u64(version);
    node.size = version;
    node
}

/// A file added at `path` holding content `version`, whose identity derives from the path and the
/// version, so two revisions adding it add the same file.
fn added_at(path: &str, version: u64) -> Node {
    let mut node = file_at(path.rsplit('/').next().unwrap_or(path), version);
    node.address.context = Context::from(uuid::Uuid::from_u64_pair(
        lore_storage::hash::hash_string_bytes(path.as_bytes()),
        version,
    ));
    node
}

/// What [`file_at`] holds, with `mode`.
fn content(version: u64, mode: u16) -> Content {
    (Hash::from_u64(version), version, mode, None)
}

/// An edit a revision makes on the base.
#[derive(Clone, Copy, Debug)]
enum Edit {
    /// Moves the node at the first path to the second.
    Move(&'static str, &'static str),
    /// Gives the file at the path the content of the version, keeping the file it is.
    Write(&'static str, u64),
    /// Gives the file at the path the mode, at version 1.
    Chmod(&'static str, u16),
    /// Gives the file at the path the content of the version and the mode.
    WriteMode(&'static str, u64, u16),
    /// Deletes the node at the path.
    Delete(&'static str),
    /// Adds a file at the path, in a directory that exists, holding the version.
    Add(&'static str, u64),
    /// Adds a file at the path as [`Edit::Add`] does, with the mode.
    AddMode(&'static str, u64, u16),
    /// Adds a directory `Bulk` of that many files, which takes a revision's changes past the count
    /// at which the 3-way diff walks the other side unfiltered.
    Bulk(u32),
}

/// The node at `path` in `state`.
async fn node_at(repository: &Arc<RepositoryContext>, state: &State, path: &str) -> NodeID {
    state
        .find_node_link(repository.clone(), path)
        .await
        .expect("resolving the path")
        .node
}

/// A revision holding `File.txt`, `Other.txt`, a file named with the Kelvin sign, `Dir/a.txt`,
/// `Dir/b.txt` and `Dir/Sub/c.txt`, each at version 1, and a link at `Mount`.
async fn base(repository: &Arc<RepositoryContext>) -> Hash {
    let state = State::new();
    for name in ["File.txt", "Other.txt", KELVIN] {
        add(
            &state,
            repository.clone(),
            ROOT_NODE,
            file_at(name, 1),
            name,
        )
        .await;
    }
    let parent = add(
        &state,
        repository.clone(),
        ROOT_NODE,
        directory("Dir"),
        "Dir",
    )
    .await;
    for name in ["a.txt", "b.txt"] {
        add(&state, repository.clone(), parent, file_at(name, 1), name).await;
    }
    let sub = add(&state, repository.clone(), parent, directory("Sub"), "Sub").await;
    add(
        &state,
        repository.clone(),
        sub,
        file_at("c.txt", 1),
        "c.txt",
    )
    .await;
    let link = Node {
        flags: NodeFlags::Link.bits(),
        mode: 0o755,
        name_hash: hash_string("Mount"),
        child: 7,
        address: Address {
            hash: Hash::from_u64(0xabcd),
            context: Context::from(MOUNT_REPOSITORY),
        },
        ..Default::default()
    };
    add(&state, repository.clone(), ROOT_NODE, link, "Mount").await;
    commit(repository, &state, Hash::default(), branch_id()).await
}

/// The revision making `edits` on `base`.
async fn edited(repository: &Arc<RepositoryContext>, base: Hash, edits: &[Edit]) -> Hash {
    let state = revision_state(repository, base).await;
    for edit in edits {
        let (path, mode, version) = match *edit {
            Edit::Move(from, to) => {
                let (parent, name) = to.rsplit_once('/').unwrap_or(("", to));
                let parent = node_at(repository, &state, parent).await;
                let node = node_at(repository, &state, from).await;
                let permit = state.moves_permit().await.expect("the moves permit");
                state
                    .node_move_staged(
                        repository.clone(),
                        node,
                        parent,
                        (name, hash_string(name)),
                        &permit,
                        false,
                    )
                    .await
                    .expect("moving the node");
                continue;
            }
            Edit::Delete(path) => {
                let node = node_at(repository, &state, path).await;
                state
                    .node_delete(repository.clone(), node)
                    .await
                    .expect("deleting the node");
                continue;
            }
            Edit::Add(path, version) | Edit::AddMode(path, version, _) => {
                let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
                let parent = node_at(repository, &state, parent).await;
                let mut file = added_at(path, version);
                if let Edit::AddMode(_, _, mode) = *edit {
                    file.mode = mode;
                }
                add(&state, repository.clone(), parent, file, name).await;
                continue;
            }
            Edit::Bulk(count) => {
                let bulk = add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    directory("Bulk"),
                    "Bulk",
                )
                .await;
                for index in 0..count {
                    let name = format!("f{index:05}.txt");
                    let file = added_at(&format!("Bulk/{name}"), 7);
                    add(&state, repository.clone(), bulk, file, &name).await;
                }
                continue;
            }
            Edit::Write(path, version) => (path, 0o644, version),
            Edit::Chmod(path, mode) => (path, mode, 1),
            Edit::WriteMode(path, version, mode) => (path, mode, version),
        };
        let node = node_at(repository, &state, path).await;
        let file = state
            .node(repository.clone(), node)
            .await
            .expect("reading the file")
            .address
            .context;
        state
            .node_modify_staged(
                repository,
                node,
                &lore_revision::state::Content {
                    mode,
                    size: version,
                    address: Address {
                        hash: Hash::from_u64(version),
                        context: file,
                    },
                },
            )
            .await
            .expect("modifying and staging the file");
    }
    commit(repository, &state, base, branch_id()).await
}

/// The content of every node in `state`, by path. A node carrying a staged or a dirty flag fails
/// the test: the state is a stored revision.
async fn contents(repository: &Arc<RepositoryContext>, state: &State) -> BTreeMap<String, Content> {
    let mut contents = BTreeMap::new();
    let mut pending = vec![(ROOT_NODE, String::new())];
    while let Some((parent, prefix)) = pending.pop() {
        for child in state
            .node_children(repository.clone(), parent)
            .await
            .expect("listing the children")
        {
            let node = state
                .node(repository.clone(), child)
                .await
                .expect("reading a child");
            let name = state
                .node_name_clone(repository.clone(), child)
                .await
                .expect("reading a name");
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            assert!(
                !node.is_staged() && !node.is_dirty(),
                "{path} is stored with flags {:#x}",
                node.flags
            );
            if node.is_directory() {
                pending.push((child, path.clone()));
            }
            let link = node
                .mount_source()
                .map(|source| (node.address.context, source));
            contents.insert(path, (node.address.hash, node.size, node.mode, link));
        }
    }
    contents
}

/// The 3-way diff a fast-forward merge takes of the edits `source` makes on the base against the
/// edits `target` makes on it.
async fn diff(repository: &Arc<RepositoryContext>, source: &[Edit], target: &[Edit]) -> DiffResult {
    diff_with(repository, source, target, false).await
}

/// [`diff`], listing the changes the two sides make alike where `include_same` asks for them.
async fn diff_with(
    repository: &Arc<RepositoryContext>,
    source: &[Edit],
    target: &[Edit],
    include_same: bool,
) -> DiffResult {
    let base = base(repository).await;
    let source = edited(repository, base, source).await;
    let target = edited(repository, base, target).await;
    diff3_collect(repository.clone(), base, source, target, None, include_same)
        .await
        .expect("diffing the revisions")
}

/// Applies `diff`, which has no conflicts, to its target as a fast-forward merge does, and answers
/// the stored result.
async fn apply(repository: &Arc<RepositoryContext>, diff: &DiffResult) -> Hash {
    let merged = revision_state(repository, diff.target).await;
    merge_tree_changes(repository.clone(), merged.clone(), &diff.changes)
        .await
        .expect("applying the changes");
    merged
        .serialize(repository.clone(), &in_memory::token())
        .await
        .expect("storing the merged revision")
}

/// Applies the edits `source` makes on the base to the revision `target` makes on it, as a
/// fast-forward merge does, stores the result, and answers the stored revision's contents.
async fn merge(
    repository: &Arc<RepositoryContext>,
    source: &[Edit],
    target: &[Edit],
) -> BTreeMap<String, Content> {
    let diff = diff(repository, source, target).await;
    assert!(
        diff.conflicts.is_empty(),
        "{source:?} against {target:?}: {:?}",
        diff.conflicts
    );
    let stored = apply(repository, &diff).await;
    contents(repository, &*revision_state(repository, stored).await).await
}

/// The files of `tree` and what they hold, without the directories above them.
fn files(tree: &BTreeMap<String, Content>) -> BTreeMap<String, Content> {
    let directories: std::collections::HashSet<&str> = tree
        .keys()
        .flat_map(|path| path.match_indices('/').map(|(end, _)| &path[..end]))
        .collect();
    tree.iter()
        .filter(|(path, _)| !directories.contains(path.as_str()))
        .map(|(path, content)| (path.clone(), *content))
        .collect()
}

/// The files of the base with `edits` made on it.
async fn files_after(
    repository: &Arc<RepositoryContext>,
    edits: &[Edit],
) -> BTreeMap<String, Content> {
    let base = base(repository).await;
    files(&tree_on(repository, base, edits).await)
}

/// The tree of the revision `base` with `edits` made on it.
async fn tree_on(
    repository: &Arc<RepositoryContext>,
    base: Hash,
    edits: &[Edit],
) -> BTreeMap<String, Content> {
    let revision = edited(repository, base, edits).await;
    contents(repository, &*revision_state(repository, revision).await).await
}

/// Asserts that `diff` has no conflicts and that none of its changes, nor any path of the tree a
/// fast-forward merge of it stores, spells a directory with a name of `stale`, and that the stored
/// tree holds the paths of `expected`, with what `expected` holds at `at`.
async fn assert_merged_spelling(
    repository: &Arc<RepositoryContext>,
    diff: &DiffResult,
    expected: &BTreeMap<String, Content>,
    at: &str,
    stale: &[&str],
) {
    let spells_stale = |path: &str| path.split('/').any(|name| stale.contains(&name));
    let context = format!("{:?}", outcome(diff));
    assert!(diff.conflicts.is_empty(), "{context}");
    assert!(
        !diff
            .changes
            .iter()
            .any(|change| spells_stale(change.path().as_str())),
        "{context}"
    );
    let merged = contents(
        repository,
        &*revision_state(repository, apply(repository, diff).await).await,
    )
    .await;
    let context = format!("{context}: {merged:?}");
    assert!(!merged.keys().any(|path| spells_stale(path)), "{context}");
    assert!(merged.keys().eq(expected.keys()), "{context}");
    assert_eq!(merged.get(at), expected.get(at), "{context}");
}

/// Each change of `changes` by its action, the path it stands at and the path its node stood at.
fn walked(changes: &[NodeChange]) -> Vec<String> {
    changes
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

/// Each change of `diff` by its action, the path it stands at and the path its node stood at, and
/// each conflict by the paths of its two changes, the source's first.
fn outcome(diff: &DiffResult) -> (Vec<String>, Vec<(String, String)>) {
    (
        walked(&diff.changes),
        diff.conflicts
            .iter()
            .map(|(source, target)| (source.path().to_string(), target.path().to_string()))
            .collect(),
    )
}

/// Runs `test` against a repository of its own.
async fn run<F, Fut>(test: F)
where
    F: FnOnce(Arc<RepositoryContext>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        test(in_memory::repository(immutable, mutable)).await;
    }))
    .await
    .expect("Task failed");
}

/// A file renamed only in case takes its new name and the content the rename comes with.
#[tokio::test]
async fn a_case_only_move_of_a_file_takes_its_new_name_and_content() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[
                Edit::Move("File.txt", "file.txt"),
                Edit::Write("file.txt", 2),
            ],
            &[Edit::Write("Other.txt", 3)],
        )
        .await;

        assert_eq!(merged.get("file.txt"), Some(&content(2, 0o644)));
        assert_eq!(merged.get("Other.txt"), Some(&content(3, 0o644)));
        assert!(!merged.contains_key("File.txt"), "{merged:?}");
    })
    .await;
}

/// A directory renamed only in case takes its new name and keeps every child, the ones the
/// rename leaves unchanged included.
#[tokio::test]
async fn a_case_only_move_of_a_directory_keeps_its_children() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[Edit::Move("Dir", "dir"), Edit::Write("dir/a.txt", 2)],
            &[Edit::Write("Other.txt", 3)],
        )
        .await;

        assert!(merged.contains_key("dir"), "{merged:?}");
        assert_eq!(merged.get("dir/a.txt"), Some(&content(2, 0o644)));
        assert_eq!(merged.get("dir/b.txt"), Some(&content(1, 0o644)));
        assert!(!merged.contains_key("Dir"), "{merged:?}");
    })
    .await;
}

/// A directory renamed only in case on a tree hashed under the rule folding case, which the new
/// address of the rename marks modified, keeps the target's node, holding the edit the target made
/// below it, so a diff against the source reports that edit.
#[tokio::test]
async fn a_case_only_move_of_a_case_folding_directory_keeps_the_target_node() {
    run(|repository| async move {
        let folding = case_folding(&repository, &in_memory::token(), base(&repository).await).await;
        let source = edited(&repository, folding, &[Edit::Move("Dir/Sub", "Dir/sub")]).await;
        let target = edited(&repository, folding, &[Edit::Write("Dir/Sub/c.txt", 3)]).await;
        let diff = diff3_collect(repository.clone(), folding, source, target, None, false)
            .await
            .expect("diffing the revisions");
        let merged = revision_state(&repository, apply(&repository, &diff).await).await;

        let tree = contents(&repository, &merged).await;
        let target_tree = contents(&repository, &*revision_state(&repository, target).await).await;
        assert_eq!(tree.get("Dir/sub"), target_tree.get("Dir/Sub"), "{tree:?}");
        assert_eq!(tree.get("Dir/sub/c.txt"), Some(&content(3, 0o644)));
        let changes = diff_collect(
            repository.clone(),
            merged,
            repository.clone(),
            revision_state(&repository, source).await,
            None,
            FilterMode::Full,
        )
        .await
        .expect("diffing the merged revision");
        let paths: Vec<String> = changes
            .iter()
            .map(|change| change.path().to_string())
            .collect();
        assert!(paths.contains(&"Dir/sub/c.txt".to_string()), "{paths:?}");
    })
    .await;
}

/// A rename that only changes case keeps the content the head gave the file meanwhile, which the
/// 3-way diff folds into the rename.
#[tokio::test]
async fn a_case_only_rename_keeps_the_content_the_head_gave_the_file() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[
                Edit::Move("File.txt", "file.txt"),
                Edit::Write("Other.txt", 2),
            ],
            &[Edit::Write("File.txt", 3)],
        )
        .await;

        assert_eq!(merged.get("file.txt"), Some(&content(3, 0o644)));
        assert_eq!(merged.get("Other.txt"), Some(&content(2, 0o644)));
        assert!(!merged.contains_key("File.txt"), "{merged:?}");
    })
    .await;
}

/// A file renamed only in case takes the mode the rename comes with.
#[tokio::test]
async fn a_case_only_move_takes_the_mode_it_comes_with() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[
                Edit::Move("File.txt", "file.txt"),
                Edit::Chmod("file.txt", 0o755),
                Edit::Write("Dir/a.txt", 2),
            ],
            &[Edit::Write("Other.txt", 3)],
        )
        .await;

        assert_eq!(merged.get("file.txt"), Some(&content(1, 0o755)));
        assert!(!merged.contains_key("File.txt"), "{merged:?}");
    })
    .await;
}

/// A rename to the name the Kelvin sign folds to is a rename that only changes case.
#[tokio::test]
async fn a_case_only_move_folds_beyond_ascii() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[Edit::Move(KELVIN, "k.txt"), Edit::Write("k.txt", 2)],
            &[Edit::Write("Other.txt", 3)],
        )
        .await;

        assert_eq!(merged.get("k.txt"), Some(&content(2, 0o644)));
        assert!(!merged.contains_key(KELVIN), "{merged:?}");
    })
    .await;
}

/// A link renamed only in case keeps the revision, the repository and the node it points at.
#[tokio::test]
async fn a_case_only_move_of_a_link_keeps_its_target() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[Edit::Move("Mount", "mount"), Edit::Write("Other.txt", 2)],
            &[Edit::Write("File.txt", 3)],
        )
        .await;

        assert_eq!(
            merged.get("mount"),
            Some(&(
                Hash::from_u64(0xabcd),
                0,
                0o755,
                Some((Context::from(MOUNT_REPOSITORY), 7))
            ))
        );
        assert!(!merged.contains_key("Mount"), "{merged:?}");
    })
    .await;
}

/// A move that changes case and parent both moves the node out of the directory it leaves.
#[tokio::test]
async fn a_move_that_changes_case_and_parent_moves_the_node() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[Edit::Move("Dir/a.txt", "A.txt")],
            &[Edit::Write("Other.txt", 3)],
        )
        .await;

        assert_eq!(merged.get("A.txt"), Some(&content(1, 0o644)));
        assert_eq!(merged.get("Dir/b.txt"), Some(&content(1, 0o644)));
        assert!(!merged.contains_key("Dir/a.txt"), "{merged:?}");
    })
    .await;
}

/// A move the tests below make on one side: the node it moves and where to, and a file at or
/// below the node with the path the move gives it.
struct Moved {
    from: &'static str,
    to: &'static str,
    file: &'static str,
    file_moved: &'static str,
}

/// A file and a directory, each moved to another name and to a name differing only in case.
const MOVES: [Moved; 4] = [
    Moved {
        from: "Dir/a.txt",
        to: "Dir/z.txt",
        file: "Dir/a.txt",
        file_moved: "Dir/z.txt",
    },
    Moved {
        from: "Dir/a.txt",
        to: "Dir/A.txt",
        file: "Dir/a.txt",
        file_moved: "Dir/A.txt",
    },
    Moved {
        from: "Dir/Sub",
        to: "Dir/Zub",
        file: "Dir/Sub/c.txt",
        file_moved: "Dir/Zub/c.txt",
    },
    Moved {
        from: "Dir/Sub",
        to: "Dir/sub",
        file: "Dir/Sub/c.txt",
        file_moved: "Dir/sub/c.txt",
    },
];

/// The edits of the side making `moved`, and an edit of the moved file to `version` where one is
/// given.
fn moving(moved: &Moved, version: Option<u64>) -> Vec<Edit> {
    let mut edits = vec![Edit::Move(moved.from, moved.to)];
    edits.extend(version.map(|version| Edit::Write(moved.file_moved, version)));
    edits
}

/// An edit on one side and a move on the other take both: the file stands where the move put it,
/// with the content the edit gave it.
#[tokio::test]
async fn an_edit_against_a_move_takes_both() {
    run(|repository| async move {
        for moved in &MOVES {
            for (source, target, version) in [
                (moving(moved, None), vec![Edit::Write(moved.file, 3)], 3),
                (vec![Edit::Write(moved.file, 2)], moving(moved, None), 2),
            ] {
                let merged = merge(&repository, &source, &target).await;
                let context = format!("{source:?} against {target:?}: {merged:?}");
                assert_eq!(
                    merged.get(moved.file_moved),
                    Some(&content(version, 0o644)),
                    "{context}"
                );
                assert!(!merged.contains_key(moved.from), "{context}");
            }
        }
    })
    .await;
}

/// Edits of one file on both sides conflict, whether or not one side also moved it.
#[tokio::test]
async fn edits_on_both_sides_conflict_whether_or_not_one_side_moved() {
    run(|repository| async move {
        for moved in &MOVES {
            for (source, target) in [
                (moving(moved, Some(2)), vec![Edit::Write(moved.file, 3)]),
                (vec![Edit::Write(moved.file, 2)], moving(moved, Some(3))),
            ] {
                let (from, to) = if matches!(source[0], Edit::Move(..)) {
                    (moved.file_moved, moved.file)
                } else {
                    (moved.file, moved.file_moved)
                };
                let diff = diff(&repository, &source, &target).await;
                assert_eq!(
                    outcome(&diff).1,
                    vec![(from.to_string(), to.to_string())],
                    "{source:?} against {target:?}: {:?}",
                    outcome(&diff)
                );
            }
        }
    })
    .await;
}

/// A change of a moved file's mode on one side and an edit of it on the other merge, the same move
/// on both sides included: the file stands where the move put it, with the edit and the mode. So
/// do the two for a file below a directory renamed only in case.
#[tokio::test]
async fn a_mode_change_of_a_moved_file_merges_with_an_edit() {
    run(|repository| async move {
        for moved in [&MOVES[0], &MOVES[1], &MOVES[3]] {
            let (file, moved_file) = (moved.file, moved.file_moved);
            let movement = Edit::Move(moved.from, moved.to);
            for (source, target, version) in [
                (
                    vec![movement, Edit::Chmod(moved_file, 0o755)],
                    vec![movement, Edit::Write(moved_file, 3)],
                    3,
                ),
                (
                    vec![movement, Edit::Write(moved_file, 2)],
                    vec![movement, Edit::Chmod(moved_file, 0o755)],
                    2,
                ),
                (
                    vec![movement, Edit::Chmod(moved_file, 0o755)],
                    vec![Edit::Write(file, 3)],
                    3,
                ),
                (
                    vec![Edit::Write(file, 2)],
                    vec![movement, Edit::Chmod(moved_file, 0o755)],
                    2,
                ),
            ] {
                let merged = merge(&repository, &source, &target).await;
                let context = format!("{source:?} against {target:?}: {merged:?}");
                assert_eq!(
                    merged.get(moved_file),
                    Some(&content(version, 0o755)),
                    "{context}"
                );
                assert!(!merged.contains_key(file), "{context}");
            }
        }
    })
    .await;
}

/// A change of a file's mode alone merges with an edit of another file and with an edit of the
/// file itself, whichever side changes the mode: the file holds the edit and the mode.
#[tokio::test]
async fn a_mode_change_merges_with_an_edit() {
    run(|repository| async move {
        let chmod = Edit::Chmod("File.txt", 0o755);
        for (source, target, file, other) in [
            (chmod, Edit::Write("Other.txt", 3), 1, 3),
            (Edit::Write("Other.txt", 2), chmod, 1, 2),
            (chmod, Edit::Write("File.txt", 3), 3, 1),
            (Edit::Write("File.txt", 2), chmod, 2, 1),
        ] {
            let merged = merge(&repository, &[source], &[target]).await;
            let context = format!("{source:?} against {target:?}: {merged:?}");
            assert_eq!(
                merged.get("File.txt"),
                Some(&content(file, 0o755)),
                "{context}"
            );
            assert_eq!(
                merged.get("Other.txt"),
                Some(&content(other, 0o644)),
                "{context}"
            );
        }
    })
    .await;
}

/// A change of a file's mode alone merges with the other side's move of the file, to another name
/// or only in case, of the file or of its directory, and with a move that also edits it, whichever
/// side changes the mode: the moved file holds the mode, and the edit where the move made one.
#[tokio::test]
async fn a_mode_change_merges_with_a_move_of_the_file() {
    run(|repository| async move {
        for moved in &MOVES {
            for version in [None, Some(3)] {
                let chmod = vec![Edit::Chmod(moved.file, 0o755)];
                for (source, target) in [
                    (chmod.clone(), moving(moved, version)),
                    (moving(moved, version), chmod),
                ] {
                    let merged = merge(&repository, &source, &target).await;
                    let context = format!("{source:?} against {target:?}: {merged:?}");
                    assert_eq!(
                        merged.get(moved.file_moved),
                        Some(&content(version.unwrap_or(1), 0o755)),
                        "{context}"
                    );
                    assert!(!merged.contains_key(moved.file), "{context}");
                }
            }
        }
    })
    .await;
}

/// A change of a file's mode alone gives way to the other side's delete of the file, whichever
/// side deletes, where the file stands and below a directory the side changing the mode renames
/// only in case, alone or alike with the deleting side, whether or not the diff lists the changes
/// the two sides make alike: the merge deletes the file, with no conflict.
#[tokio::test]
async fn a_mode_change_gives_way_to_a_delete() {
    run(|repository| async move {
        for (chmod, delete, file) in [
            (
                vec![Edit::Chmod("File.txt", 0o755)],
                vec![Edit::Delete("File.txt")],
                "File.txt",
            ),
            (
                vec![RENAME_SUB, Edit::Chmod("Dir/sub/c.txt", 0o755)],
                vec![Edit::Delete("Dir/Sub/c.txt")],
                "c.txt",
            ),
            (
                vec![RENAME_SUB, Edit::Chmod("Dir/sub/c.txt", 0o755)],
                vec![RENAME_SUB, Edit::Delete("Dir/sub/c.txt")],
                "c.txt",
            ),
        ] {
            for (source, target) in [(&chmod, &delete), (&delete, &chmod)] {
                for include_same in [false, true] {
                    let diff = diff_with(&repository, source, target, include_same).await;
                    let context = format!(
                        "{source:?} against {target:?}, include_same {include_same}: {:?}",
                        outcome(&diff)
                    );
                    assert!(diff.conflicts.is_empty(), "{context}");
                    let merged = contents(
                        &repository,
                        &*revision_state(&repository, apply(&repository, &diff).await).await,
                    )
                    .await;
                    assert!(
                        !merged.keys().any(|path| path.ends_with(file)),
                        "{context}: {merged:?}"
                    );
                }
            }
        }
    })
    .await;
}

/// The same change of a file's mode on both sides merges, and so do edits alike on both sides of
/// which one also changes the mode: the file holds the mode, with no conflict.
#[tokio::test]
async fn mode_changes_on_both_sides_merge() {
    run(|repository| async move {
        for (source, target, version) in [
            (
                Edit::Chmod("File.txt", 0o755),
                Edit::Chmod("File.txt", 0o755),
                1,
            ),
            (
                Edit::WriteMode("File.txt", 3, 0o755),
                Edit::Write("File.txt", 3),
                3,
            ),
            (
                Edit::Write("File.txt", 3),
                Edit::WriteMode("File.txt", 3, 0o755),
                3,
            ),
        ] {
            let merged = merge(&repository, &[source], &[target]).await;
            assert_eq!(
                merged.get("File.txt"),
                Some(&content(version, 0o755)),
                "{source:?} against {target:?}: {merged:?}"
            );
        }
    })
    .await;
}

/// Edits of one file on both sides conflict where one side also changes its mode, as two edits of
/// the file do: the conflict is the content's, not the mode's.
#[tokio::test]
async fn edits_on_both_sides_conflict_where_one_also_changes_the_mode() {
    run(|repository| async move {
        for (source, target) in [
            (
                Edit::WriteMode("File.txt", 2, 0o755),
                Edit::Write("File.txt", 3),
            ),
            (
                Edit::Write("File.txt", 2),
                Edit::WriteMode("File.txt", 3, 0o755),
            ),
        ] {
            let diff = diff(&repository, &[source], &[target]).await;
            assert_eq!(
                outcome(&diff),
                (
                    vec![],
                    vec![("File.txt".to_string(), "File.txt".to_string())]
                ),
                "{source:?} against {target:?}"
            );
        }
    })
    .await;
}

/// A file one side deletes and adds again is a new file, which takes its own mode: one with the
/// executable bit in place of one without keeps the bit, and one without in place of one with has
/// none, whether it holds the content the old one held or other content, at a path and below a
/// directory the same side renames only in case. The diff lists the new file even where only its
/// mode differs from the old one's.
#[tokio::test]
async fn a_file_added_again_takes_its_own_mode() {
    run(|repository| async move {
        let plain = base(&repository).await;
        let executable = edited(
            &repository,
            plain,
            &[
                Edit::Chmod("File.txt", 0o755),
                Edit::Chmod("Dir/Sub/c.txt", 0o755),
            ],
        )
        .await;
        for (base, mode) in [(plain, 0o755), (executable, 0o644)] {
            let target = edited(&repository, base, &[Edit::Write("Other.txt", 3)]).await;
            for (renaming, path) in [(&[][..], "File.txt"), (&[RENAME_SUB][..], "Dir/sub/c.txt")] {
                let deleted = edited(
                    &repository,
                    base,
                    &[renaming, &[Edit::Delete(path)]].concat(),
                )
                .await;
                for version in [1, 5] {
                    let source =
                        edited(&repository, deleted, &[Edit::AddMode(path, version, mode)]).await;
                    let diff = diff3_collect(repository.clone(), base, source, target, None, false)
                        .await
                        .expect("diffing the revisions");
                    let (changes, conflicts) = outcome(&diff);
                    let context = format!("{path} at {mode:o}, version {version}: {changes:?}");
                    assert!(conflicts.is_empty(), "{context} {conflicts:?}");
                    assert!(
                        changes.iter().any(|change| change.contains(path)),
                        "{context}"
                    );
                    let merged = contents(
                        &repository,
                        &*revision_state(&repository, apply(&repository, &diff).await).await,
                    )
                    .await;
                    assert_eq!(
                        merged.get(path),
                        Some(&content(version, mode)),
                        "{context}: {merged:?}"
                    );
                }
            }
        }
    })
    .await;
}

/// A revert of a revision that put a new file, with its own mode, in place of a file restores the
/// old file with the old file's mode: the mode the revision's new file had is not its.
#[tokio::test]
async fn a_revert_of_a_replaced_file_restores_its_own_mode() {
    run(|repository| async move {
        let parent = edited(
            &repository,
            base(&repository).await,
            &[Edit::Chmod("File.txt", 0o755)],
        )
        .await;
        let replaced = edited(
            &repository,
            parent,
            &[Edit::Delete("File.txt"), Edit::Add("File.txt", 5)],
        )
        .await;
        let diff = diff3_collect(repository.clone(), replaced, parent, replaced, None, false)
            .await
            .expect("diffing the revisions");
        assert!(diff.conflicts.is_empty(), "{:?}", outcome(&diff));
        let merged = contents(
            &repository,
            &*revision_state(&repository, apply(&repository, &diff).await).await,
        )
        .await;
        assert_eq!(
            merged.get("File.txt"),
            Some(&content(1, 0o755)),
            "{:?}: {merged:?}",
            outcome(&diff)
        );
    })
    .await;
}

/// A file one side deletes and adds again with other content wins against the other side's change
/// of its mode, as a delete does, whichever side changes the mode: the merged file is the new one,
/// with its content and its mode, and nothing conflicts.
#[tokio::test]
async fn a_file_added_again_wins_against_a_mode_change() {
    run(|repository| async move {
        let base = base(&repository).await;
        let deleted = edited(&repository, base, &[Edit::Delete("File.txt")]).await;
        let added_again = edited(&repository, deleted, &[Edit::Add("File.txt", 5)]).await;
        let chmod = edited(&repository, base, &[Edit::Chmod("File.txt", 0o755)]).await;
        let expected = tree_on(&repository, deleted, &[Edit::Add("File.txt", 5)]).await;
        for (source, target) in [(added_again, chmod), (chmod, added_again)] {
            let diff = diff3_collect(repository.clone(), base, source, target, None, false)
                .await
                .expect("diffing the revisions");
            assert_merged_spelling(&repository, &diff, &expected, "File.txt", &[]).await;
        }
    })
    .await;
}

/// A change of a file's mode gives way to the other side's change that adds a file again in its
/// place below a directory that side renames only in case, whichever side changes the mode: the
/// diff lists no change of the file but the side's own that adds it again, and the merged file is
/// the new one.
#[tokio::test]
async fn a_mode_change_gives_way_to_a_file_added_again_below_a_case_only_rename() {
    run(|repository| async move {
        let base = base(&repository).await;
        let deleted = edited(
            &repository,
            base,
            &[RENAME_SUB, Edit::Delete("Dir/sub/c.txt")],
        )
        .await;
        let added_again = edited(&repository, deleted, &[Edit::Add("Dir/sub/c.txt", 5)]).await;
        let chmod = edited(&repository, base, &[Edit::Chmod("Dir/Sub/c.txt", 0o755)]).await;
        let expected = tree_on(&repository, deleted, &[Edit::Add("Dir/sub/c.txt", 5)]).await;
        for (source, target, changes) in [
            (chmod, added_again, &[][..]),
            (
                added_again,
                chmod,
                &[
                    "Move Dir/sub <- Dir/Sub",
                    "Keep Dir/sub/c.txt <- Dir/Sub/c.txt",
                ],
            ),
        ] {
            let diff = diff3_collect(repository.clone(), base, source, target, None, false)
                .await
                .expect("diffing the revisions");
            assert_eq!(
                outcome(&diff),
                (
                    changes.iter().map(|change| change.to_string()).collect(),
                    vec![]
                )
            );
            assert_merged_spelling(&repository, &diff, &expected, "Dir/sub/c.txt", &[]).await;
        }
    })
    .await;
}

/// A file one side deletes and adds again wins against the other side's change of its mode below a
/// directory that side renames only in case, whichever side makes which: the merged tree takes the
/// rename and holds the new file below it, with its own content and mode, and nothing conflicts.
/// So does a new file holding the old content with the bit the other side gives the old one, which
/// stays the new file.
#[tokio::test]
async fn a_file_added_again_wins_against_a_mode_change_below_a_case_only_rename() {
    run(|repository| async move {
        let base = base(&repository).await;
        let deleted = edited(&repository, base, &[Edit::Delete("Dir/Sub/c.txt")]).await;
        let chmod = edited(
            &repository,
            base,
            &[RENAME_SUB, Edit::Chmod("Dir/sub/c.txt", 0o755)],
        )
        .await;
        for added in [
            Edit::Add("Dir/Sub/c.txt", 5),
            Edit::AddMode("Dir/Sub/c.txt", 1, 0o755),
        ] {
            let added_again = edited(&repository, deleted, &[added]).await;
            let new_file = revision_state(&repository, added_again).await;
            let new_file = new_file
                .node(
                    repository.clone(),
                    node_at(&repository, &new_file, "Dir/Sub/c.txt").await,
                )
                .await
                .expect("reading the new file")
                .address
                .context;
            let expected = tree_on(&repository, deleted, &[RENAME_SUB, added]).await;
            let expected = renamed_in_tree(&expected, "Dir/Sub/c.txt", "Dir/sub/c.txt");
            for (source, target) in [(added_again, chmod), (chmod, added_again)] {
                for include_same in [false, true] {
                    let diff =
                        diff3_collect(repository.clone(), base, source, target, None, include_same)
                            .await
                            .expect("diffing the revisions");
                    let context = format!("{added:?}, include_same {include_same}");
                    assert!(diff.conflicts.is_empty(), "{context}: {:?}", outcome(&diff));
                    let stored = apply(&repository, &diff).await;
                    let merged = revision_state(&repository, stored).await;
                    assert_eq!(
                        files(&contents(&repository, &merged).await),
                        files(&expected),
                        "{context}: {:?}",
                        outcome(&diff)
                    );
                    let file = merged
                        .node(
                            repository.clone(),
                            node_at(&repository, &merged, "Dir/sub/c.txt").await,
                        )
                        .await
                        .expect("reading the merged file");
                    assert_eq!(file.address.context, new_file, "{context}");
                }
            }
        }
    })
    .await;
}

/// `tree` with the entry at `from` moved to `to`.
fn renamed_in_tree(
    tree: &BTreeMap<String, Content>,
    from: &str,
    to: &str,
) -> BTreeMap<String, Content> {
    let mut tree = tree.clone();
    if let Some(content) = tree.remove(from) {
        tree.insert(to.to_string(), content);
    }
    tree
}

/// A change that takes a file's executable bit away merges with an edit of the file on the other
/// side, whichever side takes it: the file holds the edit, without the bit.
#[tokio::test]
async fn a_mode_change_taking_the_bit_away_merges_with_an_edit() {
    run(|repository| async move {
        let base = edited(
            &repository,
            base(&repository).await,
            &[Edit::Chmod("File.txt", 0o755)],
        )
        .await;
        let chmod = edited(&repository, base, &[Edit::Chmod("File.txt", 0o644)]).await;
        let edit = edited(&repository, base, &[Edit::WriteMode("File.txt", 3, 0o755)]).await;
        for (source, target) in [(chmod, edit), (edit, chmod)] {
            let diff = diff3_collect(repository.clone(), base, source, target, None, false)
                .await
                .expect("diffing the revisions");
            assert!(diff.conflicts.is_empty(), "{:?}", outcome(&diff));
            let merged = contents(
                &repository,
                &*revision_state(&repository, apply(&repository, &diff).await).await,
            )
            .await;
            assert_eq!(
                merged.get("File.txt"),
                Some(&content(3, 0o644)),
                "{merged:?}"
            );
        }
    })
    .await;
}

/// The revision `base` with the directory `Dir/Sub` replaced by a link to the revision `pin` of
/// the repository `linked`.
async fn linked_in_place_of_sub(
    repository: &Arc<RepositoryContext>,
    base: Hash,
    (linked, pin): (uuid::Uuid, Hash),
) -> Hash {
    let state = revision_state(repository, base).await;
    let sub = node_at(repository, &state, "Dir/Sub").await;
    state
        .node_delete(repository.clone(), sub)
        .await
        .expect("deleting the directory");
    let parent = node_at(repository, &state, "Dir").await;
    let link = Node {
        name_hash: hash_string("Sub"),
        ..link_to(linked, pin)
    };
    add(&state, repository.clone(), parent, link, "Sub").await;
    commit(repository, &state, base, branch_id()).await
}

/// A change of a file's mode gives way to the other side's replacement of the directory above it
/// with a link, as it does to a delete, whichever side makes which: nothing conflicts, and the
/// merged tree holds the link.
#[tokio::test]
async fn a_mode_change_gives_way_to_a_link_in_place_of_its_directory() {
    run(|repository| async move {
        let (id, linked) = linked_repository(&repository).await;
        let state = State::new();
        add(
            &state,
            linked.clone(),
            ROOT_NODE,
            file_at("sub.txt", 9),
            "sub.txt",
        )
        .await;
        let pin = commit(&linked, &state, Hash::default(), branch_id()).await;
        let base = base(&repository).await;
        let link = linked_in_place_of_sub(&repository, base, (id, pin)).await;
        let chmod = edited(&repository, base, &[Edit::Chmod("Dir/Sub/c.txt", 0o755)]).await;
        for (source, target) in [(link, chmod), (chmod, link)] {
            let diff = diff3_collect(repository.clone(), base, source, target, None, false)
                .await
                .expect("diffing the revisions");
            assert!(diff.conflicts.is_empty(), "{:?}", outcome(&diff));
            let merged = contents(
                &repository,
                &*revision_state(&repository, apply(&repository, &diff).await).await,
            )
            .await;
            assert!(
                merged.get("Dir/Sub").is_some_and(|sub| sub.3.is_some()),
                "{merged:?}"
            );
            assert!(!merged.contains_key("Dir/Sub/c.txt"), "{merged:?}");
        }
    })
    .await;
}

/// Two moves of one node to different names conflict, names that differ only in case included,
/// and the same move on both sides merges.
#[tokio::test]
async fn moves_of_one_node_to_different_names_conflict() {
    run(|repository| async move {
        for (from, source, target) in [
            ("File.txt", "B.txt", "A.txt"),
            ("File.txt", "file.txt", "FILE.txt"),
            ("File.txt", "B.txt", "file.txt"),
            ("File.txt", "file.txt", "B.txt"),
            ("Dir/a.txt", "Dir/A.txt", "Dir/A.TXT"),
            ("Dir/Sub", "Dir/sub", "Dir/SUB"),
        ] {
            let diff = diff(
                &repository,
                &[Edit::Move(from, source)],
                &[Edit::Move(from, target)],
            )
            .await;
            assert_eq!(
                outcome(&diff),
                (vec![], vec![(source.to_string(), target.to_string())]),
                "{source} against {target}"
            );
        }
        for (from, name, file) in [
            ("File.txt", "B.txt", "B.txt"),
            ("File.txt", "file.txt", "file.txt"),
            ("Dir/a.txt", "Dir/A.txt", "Dir/A.txt"),
            ("Dir/Sub", "Dir/sub", "Dir/sub/c.txt"),
        ] {
            let merged = merge(
                &repository,
                &[Edit::Move(from, name)],
                &[Edit::Move(from, name)],
            )
            .await;
            assert_eq!(merged.get(file), Some(&content(1, 0o644)), "{merged:?}");
            assert!(!merged.contains_key(from), "{merged:?}");
        }
    })
    .await;
}

/// A file deleted on one side conflicts with the other side's move of it, whichever side deletes,
/// the delete standing where the move put the file.
#[tokio::test]
async fn a_delete_against_a_move_conflicts() {
    run(|repository| async move {
        let delete = [Edit::Delete("Dir/a.txt")];
        let moving = [Edit::Move("Dir/a.txt", "Dir/z.txt")];
        for (source, target, conflict) in [
            (&delete[..], &moving[..], ("Dir/z.txt", "Dir/z.txt")),
            (&moving[..], &delete[..], ("Dir/z.txt", "Dir/a.txt")),
        ] {
            let diff = diff(&repository, source, target).await;
            assert_eq!(
                outcome(&diff),
                (
                    vec![],
                    vec![(conflict.0.to_string(), conflict.1.to_string())]
                ),
                "{source:?} against {target:?}"
            );
        }
    })
    .await;
}

/// A file one side replaces with another, by moving that one onto its path, conflicts with the
/// other side's move of it, whichever side replaces it.
#[tokio::test]
async fn a_replacement_against_a_move_conflicts() {
    run(|repository| async move {
        let replace = [
            Edit::Write("File.txt", 5),
            Edit::Delete("Other.txt"),
            Edit::Move("File.txt", "Other.txt"),
        ];
        let moving = [Edit::Move("Other.txt", "Moved.txt")];
        for (source, target, changes, conflict) in [
            (
                &replace[..],
                &moving[..],
                vec!["Delete File.txt <- File.txt".to_string()],
                ("Other.txt", "Moved.txt"),
            ),
            (
                &moving[..],
                &replace[..],
                vec![],
                ("Moved.txt", "Other.txt"),
            ),
        ] {
            let diff = diff(&repository, source, target).await;
            assert_eq!(
                outcome(&diff),
                (
                    changes,
                    vec![(conflict.0.to_string(), conflict.1.to_string())]
                ),
                "{source:?} against {target:?}"
            );
        }
    })
    .await;
}

/// Where the source changes so much that the 3-way diff walks the target unfiltered, the target's
/// walk lists the destination of its own move as well, and an edit against the move is still one
/// change, the moved file with the edit.
#[tokio::test]
async fn an_edit_against_a_move_is_one_change_without_the_target_filter() {
    run(|repository| async move {
        for (source, target, change, version) in [
            (
                vec![Edit::Bulk(10_000), Edit::Write("Dir/a.txt", 2)],
                vec![Edit::Move("Dir/a.txt", "Dir/z.txt")],
                "Keep Dir/z.txt <- Dir/z.txt",
                2,
            ),
            (
                vec![Edit::Bulk(10_000), Edit::Move("Dir/a.txt", "Dir/z.txt")],
                vec![Edit::Write("Dir/a.txt", 3)],
                "Move Dir/z.txt <- Dir/a.txt",
                3,
            ),
        ] {
            let diff = diff(&repository, &source, &target).await;
            let (changes, conflicts) = outcome(&diff);
            let changes: Vec<String> = changes
                .into_iter()
                .filter(|change| !change.contains("Bulk"))
                .collect();
            assert_eq!(
                (changes, conflicts),
                (vec![change.to_string()], vec![]),
                "{source:?} against {target:?}"
            );
            let merged = files(
                &contents(
                    &repository,
                    &*revision_state(&repository, apply(&repository, &diff).await).await,
                )
                .await,
            );
            assert_eq!(
                merged,
                files_after(
                    &repository,
                    &[
                        Edit::Bulk(10_000),
                        Edit::Write("Dir/a.txt", version),
                        Edit::Move("Dir/a.txt", "Dir/z.txt"),
                    ]
                )
                .await
            );
        }
    })
    .await;
}

/// Files added at one path below a directory one side renames only in case conflict, the other
/// side's add standing at the directory's old spelling.
#[tokio::test]
async fn adds_at_one_path_below_a_directory_renamed_only_in_case_conflict() {
    run(|repository| async move {
        let renaming = [
            Edit::Move("Dir/Sub", "Dir/sub"),
            Edit::Add("Dir/sub/new.txt", 5),
        ];
        let adding = [Edit::Add("Dir/Sub/new.txt", 6)];
        for (source, target, changes) in [
            (
                &renaming[..],
                &adding[..],
                vec!["Move Dir/sub <- Dir/Sub".to_string()],
            ),
            (&adding[..], &renaming[..], vec![]),
        ] {
            let diff = diff(&repository, source, target).await;
            assert_eq!(
                outcome(&diff),
                (
                    changes,
                    vec![("Dir/sub/new.txt".to_string(), "Dir/sub/new.txt".to_string())]
                ),
                "{source:?} against {target:?}"
            );
        }
    })
    .await;
}

/// The moves both sides make alike: a file to another name and only in case, and a directory to
/// another name with the file below it.
const SAME_MOVES: [(&str, &str, &str); 3] = [
    ("Dir/a.txt", "Dir/z.txt", "Dir/z.txt"),
    ("Dir/a.txt", "Dir/A.txt", "Dir/A.txt"),
    ("Dir/Sub", "Dir/Zub", "Dir/Zub/c.txt"),
];

/// The same move on both sides with an edit of the moved file on one side takes the move and the
/// edit, whichever side edits.
#[tokio::test]
async fn the_same_move_on_both_sides_takes_an_edit_from_either() {
    run(|repository| async move {
        for (from, to, file) in SAME_MOVES {
            for (source, target, version) in [
                (
                    vec![Edit::Move(from, to), Edit::Write(file, 2)],
                    vec![Edit::Move(from, to)],
                    2,
                ),
                (
                    vec![Edit::Move(from, to)],
                    vec![Edit::Move(from, to), Edit::Write(file, 3)],
                    3,
                ),
            ] {
                let merged = merge(&repository, &source, &target).await;
                assert_eq!(
                    files(&merged),
                    files_after(
                        &repository,
                        &[Edit::Move(from, to), Edit::Write(file, version)]
                    )
                    .await,
                    "{source:?} against {target:?}"
                );
            }
        }
    })
    .await;
}

/// Edits on both sides of the same move conflict, once, where the moved file stands.
#[tokio::test]
async fn edits_on_both_sides_of_the_same_move_conflict() {
    run(|repository| async move {
        for (from, to, file) in SAME_MOVES {
            let diff = diff(
                &repository,
                &[Edit::Move(from, to), Edit::Write(file, 2)],
                &[Edit::Move(from, to), Edit::Write(file, 3)],
            )
            .await;
            assert_eq!(
                outcome(&diff).1,
                vec![(file.to_string(), file.to_string())],
                "{from} to {to}: {:?}",
                outcome(&diff)
            );
        }
    })
    .await;
}

/// The same case-only rename of a directory on both sides, with an edit below it on one side, is
/// the edit alone, of the file where the target's tree holds it: neither side changes the
/// directory itself.
#[tokio::test]
async fn the_same_case_only_rename_of_a_directory_on_both_sides_is_the_edit_below_it() {
    run(|repository| async move {
        let diff = diff(
            &repository,
            &[
                Edit::Move("Dir/Sub", "Dir/sub"),
                Edit::Write("Dir/sub/c.txt", 2),
            ],
            &[Edit::Move("Dir/Sub", "Dir/sub")],
        )
        .await;
        assert_eq!(
            outcome(&diff),
            (
                vec!["Keep Dir/sub/c.txt <- Dir/sub/c.txt".to_string()],
                vec![]
            )
        );
    })
    .await;
}

/// A move, a delete and a rename below a directory both sides rename only in case to one name
/// take their node from where the target's tree holds it, whether or not the diff lists the
/// changes the two sides make alike.
#[tokio::test]
async fn a_change_below_a_rename_both_sides_make_takes_its_node_where_the_target_holds_it() {
    run(|repository| async move {
        for (source, target, expected) in [
            (
                vec![RENAME_SUB, Edit::Move("Dir/sub/c.txt", "Dir/sub/d.txt")],
                vec![RENAME_SUB],
                "Move Dir/sub/d.txt <- Dir/sub/c.txt",
            ),
            (
                vec![RENAME_SUB, Edit::Delete("Dir/sub/c.txt")],
                vec![RENAME_SUB],
                "Delete Dir/sub/c.txt <- Dir/sub/c.txt",
            ),
            (
                vec![RENAME_DIR, Edit::Move("dir/Sub", "dir/sub")],
                vec![RENAME_DIR],
                "Move dir/sub <- dir/Sub",
            ),
        ] {
            for include_same in [false, true] {
                let diff = diff_with(&repository, &source, &target, include_same).await;
                let (changes, conflicts) = outcome(&diff);
                let context = format!("{source:?} against {target:?}, include_same {include_same}");
                assert!(conflicts.is_empty(), "{context}: {conflicts:?}");
                assert!(
                    changes.iter().any(|change| change == expected),
                    "{context}: {changes:?}"
                );
            }
        }
    })
    .await;
}

/// A source change that a move puts at a path where the target's change leaves the file alike
/// stands only where the diff lists the changes the two sides make alike, as at a path neither
/// side moved.
#[tokio::test]
async fn a_change_alike_where_a_move_puts_it_stands_only_with_the_alike_changes() {
    run(|repository| async move {
        let edited_alike = (
            vec![Edit::Write("Dir/a.txt", 4)],
            vec![
                Edit::Move("Dir/a.txt", "Dir/z.txt"),
                Edit::Write("Dir/z.txt", 4),
            ],
            vec![],
            "Keep Dir/z.txt <- Dir/z.txt",
        );
        let added_alike = (
            vec![
                Edit::Move("Dir/Sub", "Dir/sub"),
                Edit::Add("Dir/sub/new.txt", 5),
            ],
            vec![Edit::Add("Dir/Sub/new.txt", 5)],
            vec!["Move Dir/sub <- Dir/Sub"],
            "Add Dir/sub/new.txt <- Dir/sub/new.txt",
        );
        let moved_alike = (
            vec![
                Edit::Move("Dir/a.txt", "Dir/z.txt"),
                Edit::Write("Dir/z.txt", 4),
            ],
            vec![
                Edit::Move("Dir/a.txt", "Dir/z.txt"),
                Edit::Write("Dir/z.txt", 4),
            ],
            vec![],
            "Keep Dir/z.txt <- Dir/z.txt",
        );
        for (source, target, changes, alike) in [edited_alike, added_alike, moved_alike] {
            for include_same in [false, true] {
                let mut changes: Vec<String> =
                    changes.iter().map(|change| change.to_string()).collect();
                if include_same {
                    changes.push(alike.to_string());
                }
                let diff = diff_with(&repository, &source, &target, include_same).await;
                assert_eq!(
                    outcome(&diff),
                    (changes, vec![]),
                    "{source:?} against {target:?}, include_same {include_same}"
                );
            }
        }
    })
    .await;
}

/// The target revision with the move of `Dir/a.txt` to `Dir/z.txt` a fast-forward merge applied
/// onto an edit of `Other.txt`, which gives the moved file another node.
async fn fast_forward_moved(repository: &Arc<RepositoryContext>, base: Hash) -> Hash {
    let moved = edited(repository, base, &[Edit::Move("Dir/a.txt", "Dir/z.txt")]).await;
    let other = edited(repository, base, &[Edit::Write("Other.txt", 2)]).await;
    let fast_forward = diff3_collect(repository.clone(), base, moved, other, None, false)
        .await
        .expect("diffing the revisions");
    let target = apply(repository, &fast_forward).await;
    assert_ne!(
        node_at(
            repository,
            &*revision_state(repository, target).await,
            "Dir/z.txt"
        )
        .await,
        node_at(
            repository,
            &*revision_state(repository, base).await,
            "Dir/a.txt"
        )
        .await,
        "the fast-forward merge gives the moved file another node"
    );
    target
}

/// A move the target received through a fast-forward merge, which gives the moved file another
/// node, is a delete and an add to the 3-way diff: an edit against it conflicts where the file
/// stood, the same move with an edit conflicts where the move put the file, and the same move
/// alone, or a delete, leaves the target's tree.
#[tokio::test]
async fn a_move_a_fast_forward_merge_applied_is_a_delete_and_an_add() {
    run(|repository| async move {
        let base = base(&repository).await;
        let target = fast_forward_moved(&repository, base).await;
        let movement = Edit::Move("Dir/a.txt", "Dir/z.txt");
        let deleted = "Delete Dir/a.txt <- Dir/a.txt";
        for (source, changes, conflicts) in [
            (
                vec![Edit::Write("Dir/a.txt", 3)],
                vec![],
                vec![("Dir/a.txt", "Dir/a.txt")],
            ),
            (
                vec![movement, Edit::Write("Dir/z.txt", 3)],
                vec![deleted],
                vec![("Dir/z.txt", "Dir/z.txt")],
            ),
            (vec![movement], vec![deleted], vec![]),
            (vec![Edit::Delete("Dir/a.txt")], vec![], vec![]),
        ] {
            let revision = edited(&repository, base, &source).await;
            let diff = diff3_collect(repository.clone(), base, revision, target, None, false)
                .await
                .expect("diffing the revisions");
            assert_eq!(
                outcome(&diff),
                (
                    changes.iter().map(|change| change.to_string()).collect(),
                    conflicts
                        .iter()
                        .map(|(source, target)| (source.to_string(), target.to_string()))
                        .collect()
                ),
                "{source:?}"
            );
            if conflicts.is_empty() {
                let merged = apply(&repository, &diff).await;
                assert_eq!(
                    files(
                        &contents(&repository, &*revision_state(&repository, merged).await).await
                    ),
                    files_after(&repository, &[Edit::Write("Other.txt", 2), movement]).await,
                    "{source:?}"
                );
            }
        }
    })
    .await;
}

/// The contents of the revision `revision` and its root hash.
async fn stored(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
) -> (BTreeMap<String, Content>, Hash) {
    let state = revision_state(repository, revision).await;
    let root = state
        .tree(repository.clone())
        .await
        .expect("reading the tree")
        .hash_root;
    (contents(repository, &state).await, root)
}

/// A fast-forward merge onto `target`, a revision on `base`, of the revision making `edits` on
/// `base` stores the tree a commit of `edits` on `target` stores, its directory addresses and root
/// hash included.
async fn assert_merge_stores_a_commit(
    repository: &Arc<RepositoryContext>,
    base: Hash,
    target: Hash,
    edits: &[Edit],
) {
    let source = edited(repository, base, edits).await;
    let diff = diff3_collect(repository.clone(), base, source, target, None, false)
        .await
        .expect("diffing the revisions");
    assert!(diff.conflicts.is_empty(), "{edits:?}: {:?}", outcome(&diff));
    let merged = stored(repository, apply(repository, &diff).await).await;
    let committed = stored(repository, edited(repository, target, edits).await).await;
    assert_eq!(merged, committed, "{edits:?}");
}

/// A fast-forward merge stores the tree a commit of the source's edits on the target stores, for
/// an edit, a move across directories, a rename that only changes case of a file and of a
/// directory, the latter on a tree hashed under the rule folding case too, a rename of a link, a
/// delete and an add, a directory the merge empties, an added directory, a moved file's mode, a
/// file's mode alone, and the mode of a file below a directory renamed only in case.
#[tokio::test]
async fn a_fast_forward_merge_stores_the_tree_a_commit_stores() {
    run(|repository| async move {
        let base = base(&repository).await;
        let target = edited(&repository, base, &[Edit::Write("Other.txt", 3)]).await;
        let cases: [&[Edit]; 11] = [
            &[Edit::Write("Dir/Sub/c.txt", 2), Edit::Write("File.txt", 4)],
            &[Edit::Move("Dir/a.txt", "Dir/Sub/a.txt")],
            &[Edit::Move("File.txt", "file.txt")],
            &[Edit::Move("Dir/Sub", "Dir/sub")],
            &[Edit::Move("Mount", "Linked")],
            &[Edit::Delete("Dir/b.txt"), Edit::Add("Dir/new.txt", 5)],
            &[Edit::Delete("Dir/Sub/c.txt")],
            &[Edit::Bulk(3)],
            &[
                Edit::Move("File.txt", "Moved.txt"),
                Edit::Chmod("Moved.txt", 0o755),
            ],
            &[Edit::Chmod("File.txt", 0o755)],
            &[RENAME_SUB, Edit::Chmod("Dir/sub/c.txt", 0o755)],
        ];
        for edits in cases {
            assert_merge_stores_a_commit(&repository, base, target, edits).await;
        }

        let folding = case_folding(&repository, &in_memory::token(), base).await;
        let target = edited(&repository, folding, &[Edit::Write("Other.txt", 3)]).await;
        assert_merge_stores_a_commit(
            &repository,
            folding,
            target,
            &[Edit::Move("Dir/Sub", "Dir/sub")],
        )
        .await;
    })
    .await;
}

/// Applies `diff` to its target and stores the result unfrozen, the shape an earlier server's
/// fast-forward merge stored: each merged node and the directories above it staged, at the
/// target's addresses, and each deleted node kept, staged for deletion.
async fn apply_unfrozen(repository: &Arc<RepositoryContext>, diff: &DiffResult) -> Hash {
    let merged = revision_state(repository, diff.target).await;
    apply_tree_changes(repository.clone(), merged.clone(), &diff.changes)
        .await
        .expect("applying the changes");
    merged
        .serialize(repository.clone(), &in_memory::token())
        .await
        .expect("storing the merged revision")
}

/// The revision on `base` an unfrozen fast-forward merge stores of the delete of `Dir/a.txt` onto an
/// edit of `Other.txt`, and the revision committing the delete.
async fn unfrozen_delete(repository: &Arc<RepositoryContext>, base: Hash) -> (Hash, Hash) {
    let deleting = edited(repository, base, &[Edit::Delete("Dir/a.txt")]).await;
    let other = edited(repository, base, &[Edit::Write("Other.txt", 2)]).await;
    let diff = diff3_collect(repository.clone(), base, deleting, other, None, false)
        .await
        .expect("diffing the revisions");
    let unfrozen = apply_unfrozen(repository, &diff).await;
    let state = revision_state(repository, unfrozen).await;
    let kept = state
        .node(
            repository.clone(),
            node_at(repository, &state, "Dir/a.txt").await,
        )
        .await
        .expect("reading the deleted file");
    assert!(kept.is_staged_delete(), "flags {:#x}", kept.flags);
    (unfrozen, deleting)
}

/// A move of a file the target deleted conflicts where the target is an unfrozen fast-forward merge
/// that keeps the file staged for deletion, as where a client committed the delete, for a move to
/// another name and a rename that only changes case.
#[tokio::test]
async fn a_move_against_a_delete_an_unfrozen_merge_keeps_conflicts() {
    run(|repository| async move {
        let base = base(&repository).await;
        let (unfrozen, deleting) = unfrozen_delete(&repository, base).await;
        for to in ["Dir/z.txt", "Dir/A.txt"] {
            let moving = edited(&repository, base, &[Edit::Move("Dir/a.txt", to)]).await;
            let mut outcomes = Vec::new();
            for target in [deleting, unfrozen] {
                let diff = diff3_collect(repository.clone(), base, moving, target, None, false)
                    .await
                    .expect("diffing the revisions");
                outcomes.push(outcome(&diff));
            }
            assert!(!outcomes[0].1.is_empty(), "{to}: {outcomes:?}");
            assert_eq!(outcomes[0], outcomes[1], "{to}");
        }
    })
    .await;
}

/// A fast-forward merge onto an unfrozen one stores the tree a commit of the source's edits on it
/// stores: the file it kept staged for deletion gone, and every directory hashed over what it holds.
#[tokio::test]
async fn a_fast_forward_merge_onto_an_unfrozen_merge_stores_the_tree_a_commit_stores() {
    run(|repository| async move {
        let base = base(&repository).await;
        let (unfrozen, _) = unfrozen_delete(&repository, base).await;
        assert_merge_stores_a_commit(
            &repository,
            base,
            unfrozen,
            &[Edit::Write("Dir/Sub/c.txt", 2)],
        )
        .await;
    })
    .await;
}

/// A diff from a fast-forward merge to the revision it merged onto lists the edit the merge
/// applied below a directory the target left alone.
#[tokio::test]
async fn a_diff_from_a_fast_forward_merge_to_its_target_lists_what_it_merged() {
    run(|repository| async move {
        let diff = diff(
            &repository,
            &[Edit::Write("Dir/Sub/c.txt", 2)],
            &[Edit::Write("Other.txt", 3)],
        )
        .await;
        let merged = revision_state(&repository, apply(&repository, &diff).await).await;
        let changes = diff_collect(
            repository.clone(),
            merged,
            repository.clone(),
            revision_state(&repository, diff.target).await,
            None,
            FilterMode::Full,
        )
        .await
        .expect("diffing the merged revision");
        let paths: Vec<String> = changes
            .iter()
            .map(|change| change.path().to_string())
            .collect();
        assert!(paths.contains(&"Dir/Sub/c.txt".to_string()), "{paths:?}");
    })
    .await;
}

/// A file renamed only in case, in a directory nothing else in the push changes, takes its new
/// name, which the push carries together with an edit elsewhere.
#[tokio::test]
async fn a_case_only_move_of_a_file_alone_in_its_directory_takes_its_new_name() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[
                Edit::Move("Dir/a.txt", "Dir/A.txt"),
                Edit::Write("Other.txt", 2),
            ],
            &[Edit::Write("File.txt", 3)],
        )
        .await;

        assert_eq!(merged.get("Dir/A.txt"), Some(&content(1, 0o644)));
        assert_eq!(merged.get("Dir/b.txt"), Some(&content(1, 0o644)));
        assert_eq!(merged.get("Other.txt"), Some(&content(2, 0o644)));
        assert_eq!(merged.get("File.txt"), Some(&content(3, 0o644)));
        assert!(!merged.contains_key("Dir/a.txt"), "{merged:?}");
    })
    .await;
}

/// A directory renamed only in case, in a directory nothing else in the push changes, takes its
/// new name and keeps its children.
#[tokio::test]
async fn a_case_only_move_of_a_directory_alone_in_its_directory_takes_its_new_name() {
    run(|repository| async move {
        let merged = merge(
            &repository,
            &[
                Edit::Move("Dir/Sub", "Dir/sub"),
                Edit::Write("Other.txt", 2),
            ],
            &[Edit::Write("File.txt", 3)],
        )
        .await;

        assert_eq!(merged.get("Dir/sub/c.txt"), Some(&content(1, 0o644)));
        assert!(!merged.contains_key("Dir/Sub"), "{merged:?}");
        assert_eq!(merged.get("Other.txt"), Some(&content(2, 0o644)));
        assert_eq!(merged.get("File.txt"), Some(&content(3, 0o644)));
    })
    .await;
}

/// An edit of a file the target moves onto a path the source also changed conflicts where the file
/// stood and where the move put it, as it does with no move: the edit cannot follow the move. A
/// rename of the directory that only changes case, which the move moves within or into, leaves the
/// path the source changed in the source's spelling where the source renames, and the move's
/// destination in the target's where the target does.
#[tokio::test]
async fn an_edit_against_a_move_onto_a_path_the_source_changed_conflicts_at_both() {
    run(|repository| async move {
        let renamed = vec!["Move Dir/sub <- Dir/Sub"];
        for (source, target, changes, conflicts) in [
            (
                vec![Edit::Write("Dir/a.txt", 2), Edit::Add("Dir/z.txt", 5)],
                vec![Edit::Move("Dir/a.txt", "Dir/z.txt")],
                vec![],
                vec![("Dir/a.txt", "Dir/a.txt"), ("Dir/z.txt", "Dir/z.txt")],
            ),
            (
                vec![Edit::Write("Dir/a.txt", 2), Edit::Write("Dir/b.txt", 3)],
                vec![
                    Edit::Write("Dir/a.txt", 4),
                    Edit::Delete("Dir/b.txt"),
                    Edit::Move("Dir/a.txt", "Dir/b.txt"),
                ],
                vec![],
                vec![("Dir/a.txt", "Dir/a.txt"), ("Dir/b.txt", "Dir/b.txt")],
            ),
            (
                vec![
                    RENAME_SUB,
                    Edit::Write("Dir/sub/c.txt", 2),
                    Edit::Add("Dir/sub/d.txt", 5),
                ],
                vec![Edit::Move("Dir/Sub/c.txt", "Dir/Sub/d.txt")],
                renamed.clone(),
                vec![
                    ("Dir/sub/c.txt", "Dir/Sub/d.txt"),
                    ("Dir/sub/d.txt", "Dir/sub/d.txt"),
                ],
            ),
            (
                vec![
                    RENAME_SUB,
                    Edit::Write("File.txt", 2),
                    Edit::Add("Dir/sub/File.txt", 5),
                ],
                vec![Edit::Move("File.txt", "Dir/Sub/File.txt")],
                renamed.clone(),
                vec![
                    ("Dir/sub/File.txt", "Dir/sub/File.txt"),
                    ("File.txt", "Dir/Sub/File.txt"),
                ],
            ),
            (
                vec![
                    Edit::Write("Dir/Sub/c.txt", 2),
                    Edit::Add("Dir/Sub/d.txt", 5),
                ],
                vec![RENAME_SUB, Edit::Move("Dir/sub/c.txt", "Dir/sub/d.txt")],
                vec![],
                vec![
                    ("Dir/Sub/c.txt", "Dir/sub/d.txt"),
                    ("Dir/sub/d.txt", "Dir/sub/d.txt"),
                ],
            ),
        ] {
            let (found, mut paired) = outcome(&diff(&repository, &source, &target).await);
            paired.sort();
            assert_eq!(
                (found, paired),
                (
                    changes.iter().map(|change| change.to_string()).collect(),
                    conflicts
                        .iter()
                        .map(|(source, target)| (source.to_string(), target.to_string()))
                        .collect()
                ),
                "{source:?} against {target:?}"
            );
        }
    })
    .await;
}

/// The source's move of a file away conflicts with the target's move of it onto a path the source
/// changed, as two moves of one file to different paths do.
#[tokio::test]
async fn a_move_away_against_a_move_onto_a_path_the_source_changed_conflicts() {
    run(|repository| async move {
        let diff = diff(
            &repository,
            &[
                Edit::Move("Dir/a.txt", "Dir/y.txt"),
                Edit::Write("Dir/b.txt", 3),
            ],
            &[
                Edit::Write("Dir/a.txt", 4),
                Edit::Delete("Dir/b.txt"),
                Edit::Move("Dir/a.txt", "Dir/b.txt"),
            ],
        )
        .await;
        assert_eq!(
            outcome(&diff),
            (
                vec![],
                vec![
                    ("Dir/b.txt".to_string(), "Dir/b.txt".to_string()),
                    ("Dir/y.txt".to_string(), "Dir/b.txt".to_string()),
                ]
            )
        );
    })
    .await;
}

/// A directory deleted on one side conflicts with the other side's rename of it that only changes
/// case, whichever side deletes. The source's delete of the files below the directory waits for
/// the conflict.
#[tokio::test]
async fn a_directory_delete_against_a_case_only_rename_conflicts_on_the_directory() {
    run(|repository| async move {
        let deleting = [Edit::Delete("Dir/Sub")];
        let renaming = [Edit::Move("Dir/Sub", "Dir/sub")];
        for (source, target, changes, conflict) in [
            (&deleting[..], &renaming[..], vec![], ("Dir/sub", "Dir/sub")),
            (
                &renaming[..],
                &deleting[..],
                vec!["Delete Dir/Sub/c.txt <- Dir/Sub/c.txt"],
                ("Dir/sub", "Dir/Sub"),
            ),
        ] {
            let diff = diff(&repository, source, target).await;
            assert_eq!(
                outcome(&diff),
                (
                    changes.iter().map(|change| change.to_string()).collect(),
                    vec![(conflict.0.to_string(), conflict.1.to_string())]
                ),
                "{source:?} against {target:?}"
            );
        }
    })
    .await;
}

/// The case-only rename of `Dir/Sub` the tests below make on one side.
const RENAME_SUB: Edit = Edit::Move("Dir/Sub", "Dir/sub");

/// What the tests below make on either side of [`RENAME_SUB`]: the file the renaming side edits,
/// where the other side moves that file from and to, and where the merged tree holds it. The moves
/// take the file out of the directory's old spelling or into it.
const ACROSS_THE_RENAME: [(&str, &str, &str, &str); 3] = [
    (
        "Dir/sub/c.txt",
        "Dir/Sub/c.txt",
        "Dir/Sub/d.txt",
        "Dir/sub/d.txt",
    ),
    ("Dir/sub/c.txt", "Dir/Sub/c.txt", "Dir/d.txt", "Dir/d.txt"),
    (
        "File.txt",
        "File.txt",
        "Dir/Sub/File.txt",
        "Dir/sub/File.txt",
    ),
];

/// An edit of a file on the side that renames a directory only in case, against the other side's
/// move of the file out of the directory or into it, takes the rename, the move and the edit,
/// whichever side renames: the paths of the merged tree are those of the three made one after
/// another, and the moved file holds the edit.
#[tokio::test]
async fn an_edit_against_a_move_across_a_case_only_rename_takes_all() {
    run(|repository| async move {
        for (edited, from, to, merged_at) in ACROSS_THE_RENAME {
            let movement = Edit::Move(from, to);
            for (source, target, version) in [
                (vec![RENAME_SUB, Edit::Write(edited, 2)], vec![movement], 2),
                (vec![movement], vec![RENAME_SUB, Edit::Write(edited, 3)], 3),
            ] {
                let merged = files(&merge(&repository, &source, &target).await);
                let expected = files_after(
                    &repository,
                    &[
                        RENAME_SUB,
                        Edit::Move(edited, merged_at),
                        Edit::Write(merged_at, version),
                    ],
                )
                .await;
                let context = format!("{source:?} against {target:?}: {merged:?}");
                assert!(merged.keys().eq(expected.keys()), "{context}");
                assert_eq!(
                    merged.get(merged_at),
                    Some(&content(version, 0o644)),
                    "{context}"
                );
            }
        }
    })
    .await;
}

/// Edits of one file on both sides conflict where one side renames a directory only in case and
/// the other moves the file out of the directory or into it, whichever side renames.
#[tokio::test]
async fn edits_against_a_move_across_a_case_only_rename_conflict() {
    run(|repository| async move {
        for (edited, from, to, _) in ACROSS_THE_RENAME {
            let renaming = |version| vec![RENAME_SUB, Edit::Write(edited, version)];
            let moving = |version| vec![Edit::Move(from, to), Edit::Write(to, version)];
            for (source, target, conflict) in [
                (renaming(2), moving(3), (edited, to)),
                (moving(2), renaming(3), (to, edited)),
            ] {
                let diff = diff(&repository, &source, &target).await;
                assert_eq!(
                    outcome(&diff).1,
                    vec![(conflict.0.to_string(), conflict.1.to_string())],
                    "{source:?} against {target:?}: {:?}",
                    outcome(&diff)
                );
            }
        }
    })
    .await;
}

/// The case-only rename of `Dir` the tests below make on one side.
const RENAME_DIR: Edit = Edit::Move("Dir", "dir");

/// What the tests below make on the two sides where one renames `Dir` only in case and the other
/// `Dir/Sub`: the edits of the side that edits a file, those of the side that moves it into
/// `Dir/Sub` or within it, and the file and where the move puts it, both in the merged spelling.
type NestedRenames = ([Edit; 2], [Edit; 2], &'static str, &'static str);

/// The edits [`NestedRenames`] describes, for each side renaming either directory.
const NESTED_RENAMES: [NestedRenames; 4] = [
    (
        [RENAME_SUB, Edit::Write("File.txt", 2)],
        [RENAME_DIR, Edit::Move("File.txt", "dir/Sub/File.txt")],
        "File.txt",
        "dir/sub/File.txt",
    ),
    (
        [RENAME_SUB, Edit::Write("Dir/sub/c.txt", 2)],
        [RENAME_DIR, Edit::Move("dir/Sub/c.txt", "dir/Sub/d.txt")],
        "dir/sub/c.txt",
        "dir/sub/d.txt",
    ),
    (
        [RENAME_DIR, Edit::Write("File.txt", 2)],
        [RENAME_SUB, Edit::Move("File.txt", "Dir/sub/File.txt")],
        "File.txt",
        "dir/sub/File.txt",
    ),
    (
        [RENAME_DIR, Edit::Write("dir/Sub/c.txt", 2)],
        [RENAME_SUB, Edit::Move("Dir/sub/c.txt", "Dir/sub/d.txt")],
        "dir/sub/c.txt",
        "dir/sub/d.txt",
    ),
];

/// An edit on one side against the other side's move of the file into `Dir/Sub` or within it, where
/// one side renames `Dir` only in case and the other `Dir/Sub`, whichever side renames which and
/// whichever side edits: the moved file stands below both new spellings, with the edit, and no
/// change and no path of the merged tree spells either old one.
#[tokio::test]
async fn an_edit_against_a_move_across_nested_case_only_renames_takes_all() {
    run(|repository| async move {
        let base = base(&repository).await;
        for (editing, moving, edited, merged_at) in NESTED_RENAMES {
            let expected = tree_on(
                &repository,
                base,
                &[
                    RENAME_DIR,
                    Edit::Move("dir/Sub", "dir/sub"),
                    Edit::Move(edited, merged_at),
                    Edit::Write(merged_at, 2),
                ],
            )
            .await;
            for (source, target) in [(editing, moving), (moving, editing)] {
                let diff = diff(&repository, &source, &target).await;
                assert_merged_spelling(&repository, &diff, &expected, merged_at, &["Dir", "Sub"])
                    .await;
            }
        }
    })
    .await;
}

/// An edit below directories each side renames only in case, one side `Dir` and the other
/// `Dir/Sub`, stands below both new spellings, whichever side renames which: the change is the
/// edit at the merged spelling of the file, of the file where the target holds it.
#[tokio::test]
async fn an_edit_below_case_only_renames_on_both_sides_stands_below_both() {
    run(|repository| async move {
        let base = base(&repository).await;
        let expected = tree_on(
            &repository,
            base,
            &[
                RENAME_DIR,
                Edit::Move("dir/Sub", "dir/sub"),
                Edit::Write("dir/sub/c.txt", 2),
            ],
        )
        .await;
        for (source, target, changes) in [
            (
                vec![RENAME_DIR, Edit::Write("dir/Sub/c.txt", 2)],
                vec![RENAME_SUB],
                ["Move dir <- Dir", "Keep dir/sub/c.txt <- Dir/sub/c.txt"],
            ),
            (
                vec![RENAME_SUB, Edit::Write("Dir/sub/c.txt", 2)],
                vec![RENAME_DIR],
                [
                    "Move dir/sub <- dir/Sub",
                    "Keep dir/sub/c.txt <- dir/Sub/c.txt",
                ],
            ),
        ] {
            let diff = diff(&repository, &source, &target).await;
            assert_eq!(
                outcome(&diff),
                (
                    changes.iter().map(|change| change.to_string()).collect(),
                    vec![]
                ),
                "{source:?} against {target:?}"
            );
            assert_merged_spelling(
                &repository,
                &diff,
                &expected,
                "dir/sub/c.txt",
                &["Dir", "Sub"],
            )
            .await;
        }
    })
    .await;
}

/// [`base`] with a directory `Deep` in `Dir/Sub`, holding `e.txt` at version 1.
async fn deep_base(repository: &Arc<RepositoryContext>) -> Hash {
    let base = base(repository).await;
    let state = revision_state(repository, base).await;
    let sub = node_at(repository, &state, "Dir/Sub").await;
    let deep = add(&state, repository.clone(), sub, directory("Deep"), "Deep").await;
    add(
        &state,
        repository.clone(),
        deep,
        file_at("e.txt", 1),
        "e.txt",
    )
    .await;
    commit(repository, &state, base, branch_id()).await
}

/// Three directories, one inside the next, renamed only in case on alternating sides: one side
/// renames `Dir` and `Dir/Sub/Deep`, the other `Dir/Sub`. An edit of a file in `Deep` on one side
/// stands below the three new spellings, where the other side only renames and where it moves the
/// file within `Deep`, whichever side edits.
#[tokio::test]
async fn an_edit_below_case_only_renames_alternating_over_three_levels_stands_below_all() {
    run(|repository| async move {
        let base = deep_base(&repository).await;
        let outer = [RENAME_DIR, Edit::Move("dir/Sub/Deep", "dir/Sub/deep")];
        let inner = [RENAME_SUB];
        let renames = [
            RENAME_DIR,
            Edit::Move("dir/Sub", "dir/sub"),
            Edit::Move("dir/sub/Deep", "dir/sub/deep"),
        ];
        for (editing, other, merged_at) in [
            (
                [&outer[..], &[Edit::Write("dir/Sub/deep/e.txt", 2)]].concat(),
                vec![RENAME_SUB],
                "dir/sub/deep/e.txt",
            ),
            (
                [&inner[..], &[Edit::Write("Dir/sub/Deep/e.txt", 2)]].concat(),
                outer.to_vec(),
                "dir/sub/deep/e.txt",
            ),
            (
                [&outer[..], &[Edit::Write("dir/Sub/deep/e.txt", 2)]].concat(),
                [
                    &inner[..],
                    &[Edit::Move("Dir/sub/Deep/e.txt", "Dir/sub/Deep/f.txt")],
                ]
                .concat(),
                "dir/sub/deep/f.txt",
            ),
            (
                [&inner[..], &[Edit::Write("Dir/sub/Deep/e.txt", 2)]].concat(),
                [
                    &outer[..],
                    &[Edit::Move("dir/Sub/deep/e.txt", "dir/Sub/deep/f.txt")],
                ]
                .concat(),
                "dir/sub/deep/f.txt",
            ),
        ] {
            let mut edits = renames.to_vec();
            if merged_at != "dir/sub/deep/e.txt" {
                edits.push(Edit::Move("dir/sub/deep/e.txt", merged_at));
            }
            edits.push(Edit::Write(merged_at, 2));
            let expected = tree_on(&repository, base, &edits).await;
            for (source, target) in [(&editing, &other), (&other, &editing)] {
                let source_revision = edited(&repository, base, source).await;
                let target_revision = edited(&repository, base, target).await;
                let diff = diff3_collect(
                    repository.clone(),
                    base,
                    source_revision,
                    target_revision,
                    None,
                    false,
                )
                .await
                .expect("diffing the revisions");
                assert_merged_spelling(
                    &repository,
                    &diff,
                    &expected,
                    merged_at,
                    &["Dir", "Sub", "Deep"],
                )
                .await;
            }
        }
    })
    .await;
}

/// An edit of a file against the other side's rename of its directory that only changes case,
/// where that side also deleted the file and added it again with the same content, follows the
/// rename with no conflict, as it merges where no rename comes with it: the file stands below the
/// new spelling with the edit.
#[tokio::test]
async fn an_edit_against_a_case_only_rename_that_adds_the_file_again_follows_the_rename() {
    run(|repository| async move {
        let base = base(&repository).await;
        let deleted = edited(
            &repository,
            base,
            &[RENAME_SUB, Edit::Delete("Dir/sub/c.txt")],
        )
        .await;
        let target = edited(&repository, deleted, &[Edit::Add("Dir/sub/c.txt", 1)]).await;
        let source = edited(&repository, base, &[Edit::Write("Dir/Sub/c.txt", 2)]).await;
        let diff = diff3_collect(repository.clone(), base, source, target, None, false)
            .await
            .expect("diffing the revisions");
        assert_eq!(
            outcome(&diff),
            (
                vec!["Keep Dir/sub/c.txt <- Dir/sub/c.txt".to_string()],
                vec![]
            )
        );
        let merged = files(
            &contents(
                &repository,
                &*revision_state(&repository, apply(&repository, &diff).await).await,
            )
            .await,
        );
        assert_eq!(
            merged.get("Dir/sub/c.txt"),
            Some(&content(2, 0o644)),
            "{merged:?}"
        );
    })
    .await;
}

/// A file deleted and added again with the content it held, below a directory the same side renames
/// only in case, is no change, as it is no change where no rename comes with it: the diff lists
/// nothing of it, whichever side makes it. The other side's change of the file merges with the
/// rename, an edit where that side renames `Dir/Sub` only in case as well and a move within the
/// renamed directory.
#[tokio::test]
async fn a_file_added_again_below_a_case_only_rename_is_no_change() {
    run(|repository| async move {
        let base = base(&repository).await;
        for (renaming, added_at, other, expected_edits, at, changes) in [
            (
                RENAME_DIR,
                "dir/Sub/c.txt",
                vec![RENAME_SUB, Edit::Write("Dir/sub/c.txt", 2)],
                vec![
                    RENAME_DIR,
                    Edit::Move("dir/Sub", "dir/sub"),
                    Edit::Write("dir/sub/c.txt", 2),
                ],
                "dir/sub/c.txt",
                [
                    &["Move dir <- Dir"][..],
                    &[
                        "Move dir/sub <- dir/Sub",
                        "Keep dir/sub/c.txt <- dir/Sub/c.txt",
                    ],
                ],
            ),
            (
                RENAME_SUB,
                "Dir/sub/c.txt",
                vec![Edit::Move("Dir/Sub/c.txt", "Dir/Sub/d.txt")],
                vec![RENAME_SUB, Edit::Move("Dir/sub/c.txt", "Dir/sub/d.txt")],
                "Dir/sub/d.txt",
                [
                    &["Move Dir/sub <- Dir/Sub"][..],
                    &["Move Dir/sub/d.txt <- Dir/sub/c.txt"],
                ],
            ),
        ] {
            let deleted = edited(&repository, base, &[renaming, Edit::Delete(added_at)]).await;
            let added_again = edited(&repository, deleted, &[Edit::Add(added_at, 1)]).await;
            let other = edited(&repository, base, &other).await;
            let expected = tree_on(&repository, base, &expected_edits).await;
            for ((source, target), changes) in [(added_again, other), (other, added_again)]
                .into_iter()
                .zip(changes)
            {
                let diff = diff3_collect(repository.clone(), base, source, target, None, false)
                    .await
                    .expect("diffing the revisions");
                assert_eq!(
                    outcome(&diff),
                    (
                        changes.iter().map(|change| change.to_string()).collect(),
                        vec![]
                    )
                );
                assert_merged_spelling(&repository, &diff, &expected, at, &[]).await;
            }
        }
    })
    .await;
}

/// Renames of one directory to different spellings that only change case conflict, and edits of a
/// file below it on both sides conflict as well. An edit on one side alone leaves the directory's
/// conflict, the target's standing in the target and the source's at the source's spelling. An
/// edit on the source's side against the target's move of the file, within the directory or out of
/// it, waits for the directory's conflict, and so does a move on the source's side against the
/// target's edit: the two conflict, as the merged tree spells the directory neither way.
#[tokio::test]
async fn renames_of_a_directory_to_different_spellings_conflict() {
    run(|repository| async move {
        let other = Edit::Move("Dir/Sub", "Dir/SUB");
        let directories = ("Dir/sub", "Dir/SUB");
        for (source, target, changes, conflicts) in [
            (
                vec![RENAME_SUB, Edit::Write("Dir/sub/c.txt", 2)],
                vec![other, Edit::Write("Dir/SUB/c.txt", 3)],
                vec![],
                vec![directories, ("Dir/sub/c.txt", "Dir/SUB/c.txt")],
            ),
            (
                vec![RENAME_SUB, Edit::Write("Dir/sub/c.txt", 2)],
                vec![other],
                vec!["Keep Dir/sub/c.txt <- Dir/SUB/c.txt"],
                vec![directories],
            ),
            (
                vec![RENAME_SUB],
                vec![other, Edit::Write("Dir/SUB/c.txt", 3)],
                vec![],
                vec![directories],
            ),
            (
                vec![RENAME_SUB, Edit::Write("Dir/sub/c.txt", 2)],
                vec![other, Edit::Move("Dir/SUB/c.txt", "Dir/SUB/d.txt")],
                vec!["Add Dir/SUB/d.txt <- Dir/SUB/d.txt"],
                vec![directories, ("Dir/sub/c.txt", "Dir/SUB/d.txt")],
            ),
            (
                vec![RENAME_SUB, Edit::Write("Dir/sub/c.txt", 2)],
                vec![other, Edit::Move("Dir/SUB/c.txt", "Dir/d.txt")],
                vec![],
                vec![directories, ("Dir/sub/c.txt", "Dir/d.txt")],
            ),
            (
                vec![RENAME_SUB, Edit::Move("Dir/sub/c.txt", "Dir/sub/d.txt")],
                vec![other, Edit::Write("Dir/SUB/c.txt", 3)],
                vec![],
                vec![directories, ("Dir/sub/d.txt", "Dir/SUB/c.txt")],
            ),
        ] {
            assert_eq!(
                outcome(&diff(&repository, &source, &target).await),
                (
                    changes.iter().map(|change| change.to_string()).collect(),
                    conflicts
                        .iter()
                        .map(|(source, target)| (source.to_string(), target.to_string()))
                        .collect()
                ),
                "{source:?} against {target:?}"
            );
        }
    })
    .await;
}

/// The same rename that only changes case, of a directory or of a file, on both sides is one move,
/// where one side also renames a directory above it only in case, whichever side does and whether
/// or not the diff lists the changes the two sides make alike: the merge takes both renames with no
/// conflict, and a move within the directory on one side still takes the other side's edit.
#[tokio::test]
async fn the_same_case_only_rename_below_another_rename_merges() {
    run(|repository| async move {
        let base = base(&repository).await;
        let both = [RENAME_DIR, Edit::Move("dir/Sub", "dir/sub")];
        for (renaming_both, renaming_one, expected_edits, at, stale) in [
            (
                both.to_vec(),
                vec![RENAME_SUB],
                both.to_vec(),
                "dir/sub/c.txt",
                &["Dir", "Sub"][..],
            ),
            (
                [&both[..], &[Edit::Write("dir/sub/c.txt", 2)]].concat(),
                vec![RENAME_SUB, Edit::Move("Dir/sub/c.txt", "Dir/sub/d.txt")],
                [
                    &both[..],
                    &[
                        Edit::Move("dir/sub/c.txt", "dir/sub/d.txt"),
                        Edit::Write("dir/sub/d.txt", 2),
                    ],
                ]
                .concat(),
                "dir/sub/d.txt",
                &["Dir", "Sub"],
            ),
            (
                vec![RENAME_DIR, Edit::Move("dir/a.txt", "dir/A.txt")],
                vec![Edit::Move("Dir/a.txt", "Dir/A.txt")],
                vec![RENAME_DIR, Edit::Move("dir/a.txt", "dir/A.txt")],
                "dir/A.txt",
                &["Dir", "a.txt"],
            ),
        ] {
            let expected = tree_on(&repository, base, &expected_edits).await;
            for (source, target) in [
                (&renaming_both, &renaming_one),
                (&renaming_one, &renaming_both),
            ] {
                for include_same in [false, true] {
                    let diff = diff_with(&repository, source, target, include_same).await;
                    assert_merged_spelling(&repository, &diff, &expected, at, stale).await;
                }
            }
        }
    })
    .await;
}

/// The same move of a file on both sides, within `Dir/Sub` or into it, is one move where one side
/// also renames `Dir` or `Dir/Sub` above the destination only in case, whichever side renames and
/// whether or not the diff lists the changes the two sides make alike: the merge takes the rename
/// and the move with no conflict. Where the target renames and the diff lists no alike changes, it
/// lists nothing at the destination.
#[tokio::test]
async fn the_same_move_below_a_case_only_rename_on_one_side_merges() {
    run(|repository| async move {
        let base = base(&repository).await;
        for (rename, (dir, sub), stale) in [
            (RENAME_DIR, ("dir", "Sub"), "Dir"),
            (RENAME_SUB, ("Dir", "sub"), "Sub"),
        ] {
            let at = |name: &str| leaked(format!("{dir}/{sub}/{name}"));
            for (from, name) in [("Dir/Sub/c.txt", "d.txt"), ("File.txt", "File.txt")] {
                let renaming = [
                    rename,
                    Edit::Move(
                        leaked(from.replace("Dir/Sub", &format!("{dir}/{sub}"))),
                        at(name),
                    ),
                ];
                let moving = [Edit::Move(from, leaked(format!("Dir/Sub/{name}")))];
                let expected = tree_on(&repository, base, &renaming).await;
                for (source, target) in [(&renaming[..], &moving[..]), (&moving[..], &renaming[..])]
                {
                    for include_same in [false, true] {
                        let diff = diff_with(&repository, source, target, include_same).await;
                        assert_merged_spelling(&repository, &diff, &expected, at(name), &[stale])
                            .await;
                        let changes = outcome(&diff).0;
                        let folded = at(name).to_lowercase();
                        assert!(
                            include_same
                                || std::ptr::eq(source.as_ptr(), renaming.as_ptr())
                                || !changes
                                    .iter()
                                    .any(|change| change.to_lowercase().contains(&folded)),
                            "the target's own change stands at {folded}: {changes:?}"
                        );
                    }
                }
            }
        }
    })
    .await;
}

/// Two adds at one place below a directory the two sides rename only in case to different names,
/// with the directory below it renamed alike, conflict at the add as well as at the renames, whether
/// or not the diff lists the changes the two sides make alike: the target's add is found where the
/// target's tree spells the path.
#[tokio::test]
async fn adds_at_one_place_below_conflicting_case_only_renames_conflict() {
    run(|repository| async move {
        let renaming = |dir: &'static str, version| {
            [
                Edit::Move("Dir", dir),
                Edit::Move(leaked(format!("{dir}/Sub")), leaked(format!("{dir}/sub"))),
                Edit::Add(leaked(format!("{dir}/sub/new.txt")), version),
            ]
        };
        let (lower, upper) = (renaming("dir", 2), renaming("DIR", 3));
        for (source, target) in [(&lower, &upper), (&upper, &lower)] {
            for include_same in [false, true] {
                let diff = diff_with(&repository, source, target, include_same).await;
                let (changes, conflicts) = outcome(&diff);
                let context = format!("include_same {include_same}: {changes:?} {conflicts:?}");
                assert!(
                    !changes.iter().any(|change| change.contains("new.txt")),
                    "{context}"
                );
                let conflicts_at = |name: &str| {
                    conflicts.iter().any(|(source, target)| {
                        source.to_lowercase().ends_with(name)
                            && target.to_lowercase().ends_with(name)
                    })
                };
                assert!(conflicts_at("dir") && conflicts_at("/new.txt"), "{context}");
            }
        }
    })
    .await;
}

/// An add on each side at one place below directories renamed only in case conflicts, whether or
/// not the diff lists the changes the two sides make alike: where the source renames `Dir` and
/// `Dir/Sub` and the target neither, where each side renames one of the two, where both rename
/// `Dir` alike and the source `Dir/Sub` too, and where the add stands in `Dir` while the source
/// renames both and the target `Dir/Sub` alike.
#[tokio::test]
async fn adds_at_one_place_below_nested_case_only_renames_conflict() {
    run(|repository| async move {
        let both = [RENAME_DIR, Edit::Move("dir/Sub", "dir/sub")];
        for (source, target) in [
            (
                [&both[..], &[Edit::Add("dir/sub/new.txt", 2)]].concat(),
                vec![Edit::Add("Dir/Sub/new.txt", 3)],
            ),
            (
                vec![Edit::Add("Dir/Sub/new.txt", 2)],
                [&both[..], &[Edit::Add("dir/sub/new.txt", 3)]].concat(),
            ),
            (
                vec![RENAME_DIR, Edit::Add("dir/Sub/new.txt", 2)],
                vec![RENAME_SUB, Edit::Add("Dir/sub/new.txt", 3)],
            ),
            (
                vec![RENAME_SUB, Edit::Add("Dir/sub/new.txt", 2)],
                vec![RENAME_DIR, Edit::Add("dir/Sub/new.txt", 3)],
            ),
            (
                [&both[..], &[Edit::Add("dir/sub/new.txt", 2)]].concat(),
                vec![RENAME_DIR, Edit::Add("dir/Sub/new.txt", 3)],
            ),
            (
                vec![RENAME_DIR, Edit::Add("dir/Sub/new.txt", 2)],
                [&both[..], &[Edit::Add("dir/sub/new.txt", 3)]].concat(),
            ),
            (
                [&both[..], &[Edit::Add("dir/new.txt", 2)]].concat(),
                vec![RENAME_SUB, Edit::Add("Dir/new.txt", 3)],
            ),
            (
                vec![RENAME_SUB, Edit::Add("Dir/new.txt", 2)],
                [&both[..], &[Edit::Add("dir/new.txt", 3)]].concat(),
            ),
        ] {
            for include_same in [false, true] {
                let diff = diff_with(&repository, &source, &target, include_same).await;
                let (changes, conflicts) = outcome(&diff);
                let context = format!(
                    "{source:?} against {target:?}, include_same {include_same}: {changes:?} \
                     {conflicts:?}"
                );
                assert!(
                    !changes.iter().any(|change| change.contains("new.txt")),
                    "{context}"
                );
                assert!(
                    matches!(
                        &conflicts[..],
                        [(source, target)] if source.ends_with("/new.txt") && target.ends_with("/new.txt")
                    ),
                    "{context}"
                );
            }
        }
    })
    .await;
}

/// Where the source changes so much that the 3-way diff walks the target unfiltered, an edit below
/// a directory one side renames only in case, against the other side's move of the file, is still
/// one change of the file, whichever side renames.
#[tokio::test]
async fn an_edit_below_a_case_only_rename_against_a_move_is_one_change_without_the_target_filter() {
    run(|repository| async move {
        let movement = Edit::Move("Dir/Sub/c.txt", "Dir/Sub/d.txt");
        for (source, target, changes, version) in [
            (
                vec![
                    Edit::Bulk(10_000),
                    RENAME_SUB,
                    Edit::Write("Dir/sub/c.txt", 2),
                ],
                vec![movement],
                vec![
                    "Move Dir/sub <- Dir/Sub",
                    "Keep Dir/sub/d.txt <- Dir/Sub/d.txt",
                ],
                2,
            ),
            (
                vec![Edit::Bulk(10_000), movement],
                vec![RENAME_SUB, Edit::Write("Dir/sub/c.txt", 3)],
                vec!["Move Dir/sub/d.txt <- Dir/sub/c.txt"],
                3,
            ),
        ] {
            let diff = diff(&repository, &source, &target).await;
            let (found, conflicts) = outcome(&diff);
            let found: Vec<String> = found
                .into_iter()
                .filter(|change| !change.contains("Bulk"))
                .collect();
            let context = format!("{source:?} against {target:?}");
            assert_eq!(
                (found, conflicts),
                (
                    changes.iter().map(|change| change.to_string()).collect(),
                    vec![]
                ),
                "{context}"
            );
            let merged = files(
                &contents(
                    &repository,
                    &*revision_state(&repository, apply(&repository, &diff).await).await,
                )
                .await,
            );
            assert_eq!(
                merged.get("Dir/sub/d.txt"),
                Some(&content(version, 0o644)),
                "{context}"
            );
            assert!(!merged.contains_key("Dir/sub/c.txt"), "{context}");
        }
    })
    .await;
}

/// Edits against several moves on the other side each follow their move, however the paths the
/// moves put the files at sort among the source's changes.
#[tokio::test]
async fn edits_against_several_moves_each_follow_their_move() {
    run(|repository| async move {
        let edits = [
            Edit::Write("Dir/Sub/c.txt", 2),
            Edit::Write("Dir/a.txt", 2),
            Edit::Write("Dir/b.txt", 2),
            Edit::Write("File.txt", 2),
            Edit::Write("Other.txt", 2),
        ];
        let moves = [
            Edit::Move("File.txt", "Zz.txt"),
            Edit::Move("Other.txt", "Yy.txt"),
        ];
        let merged = merge(&repository, &edits, &moves).await;
        assert_eq!(
            files(&merged),
            files_after(&repository, &[&edits[..], &moves[..]].concat()).await
        );
    })
    .await;
}

/// A link named `Mount` to the root of the revision `pin` of the repository `linked`.
fn link_to(linked: uuid::Uuid, pin: Hash) -> Node {
    Node {
        flags: NodeFlags::Link.bits(),
        mode: 0o755,
        name_hash: hash_string("Mount"),
        child: ROOT_NODE,
        address: Address {
            hash: pin,
            context: Context::from(linked),
        },
        ..Default::default()
    }
}

/// The repository a test's links point into: one of its own, as a commit uploading what another
/// test's commit to the same repository uploads at the same time waits on that upload, which goes
/// to the other test's store.
async fn linked_repository(
    repository: &Arc<RepositoryContext>,
) -> (uuid::Uuid, Arc<RepositoryContext>) {
    let id = uuid::Uuid::now_v7();
    (id, repository.to_link_context(RepositoryId::from(id)).await)
}

/// A revision of the repository `linked`, holding `l.txt` and `name`.
async fn linked_revision(linked: &Arc<RepositoryContext>, name: &str) -> Hash {
    let state = State::new();
    for name in ["l.txt", name] {
        add(&state, linked.clone(), ROOT_NODE, file_at(name, 1), name).await;
    }
    commit(linked, &state, Hash::default(), branch_id()).await
}

/// The revision renaming the link of `base` at `Mount` to `mount` and pinning it to `pin`.
async fn renamed_and_pinned(repository: &Arc<RepositoryContext>, base: Hash, pin: Hash) -> Hash {
    let state = revision_state(repository, base).await;
    stage_renamed_and_pinned(repository, &state, LinkPin::Revision(pin)).await;
    commit(repository, &state, base, branch_id()).await
}

/// Renames the link of `state` at `Mount` to `mount` and gives it `pin`, staged.
async fn stage_renamed_and_pinned(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    pin: LinkPin,
) {
    let link = node_at(repository, state, "Mount").await;
    state
        .node_move_staged(
            repository.clone(),
            link,
            ROOT_NODE,
            ("mount", hash_string("mount")),
            &state.moves_permit().await.expect("the moves permit"),
            false,
        )
        .await
        .expect("renaming the link");
    state
        .node_set_link_target(repository, link, pin)
        .await
        .expect("pinning the link");
    state
        .node_mark(
            repository,
            link,
            lore_revision::state::Mark::staged_and_dirty(
                NodeFlags::StagedModify,
                NodeFlags::DirtyModify,
            ),
        )
        .await
        .expect("staging the pin");
}

/// The same rename of a link that only changes case on both sides conflicts where the two pin it to
/// different revisions, as two edits of one file do, and is no change of either side where they pin
/// it alike.
#[tokio::test]
async fn the_same_case_only_rename_of_a_link_conflicts_where_the_pins_differ() {
    run(|repository| async move {
        let (id, linked) = linked_repository(&repository).await;
        let base_pin = linked_revision(&linked, "base.txt").await;
        let (source_pin, target_pin) = (
            linked_revision(&linked, "x.txt").await,
            linked_revision(&linked, "y.txt").await,
        );
        let state = State::new();
        add(
            &state,
            repository.clone(),
            ROOT_NODE,
            link_to(id, base_pin),
            "Mount",
        )
        .await;
        let base = commit(&repository, &state, Hash::default(), branch_id()).await;
        let source = renamed_and_pinned(&repository, base, source_pin).await;
        for (target_pin, conflicts) in [
            (target_pin, vec![("mount".to_string(), "mount".to_string())]),
            (source_pin, vec![]),
        ] {
            let target = renamed_and_pinned(&repository, base, target_pin).await;
            let diff = diff3_collect(repository.clone(), base, source, target, None, false)
                .await
                .expect("diffing the revisions");
            assert_eq!(outcome(&diff).1, conflicts, "{:?}", outcome(&diff));
            assert!(
                diff.changes.iter().all(|change| !change.to.is_link()),
                "{:?}",
                outcome(&diff)
            );
        }
    })
    .await;
}

/// The walk of `repository` from `from` to `to`.
async fn walk(
    repository: &Arc<RepositoryContext>,
    from: Arc<State>,
    to: Arc<State>,
) -> Vec<NodeChange> {
    diff_collect(
        repository.clone(),
        from,
        repository.clone(),
        to,
        None,
        FilterMode::Full,
    )
    .await
    .expect("walking the revisions")
}

/// A link mounting another node of the revision it pins, on one side, is that side's change of the
/// link, where the link stands and below a directory the same side renames only in case: the diff
/// lists it, and a fast-forward merge takes the mount, and the rename.
#[tokio::test]
async fn a_link_remounted_is_a_change_where_it_stands_and_below_a_case_only_rename() {
    run(|repository| async move {
        let (id, linked) = linked_repository(&repository).await;
        let linked_state = State::new();
        for name in ["first.txt", "second.txt"] {
            add(
                &linked_state,
                linked.clone(),
                ROOT_NODE,
                file_at(name, 4),
                name,
            )
            .await;
        }
        let pin = commit(&linked, &linked_state, Hash::default(), branch_id()).await;
        let pinned = revision_state(&linked, pin).await;
        let first = node_at(&linked, &pinned, "first.txt").await;
        let second = node_at(&linked, &pinned, "second.txt").await;
        let state = revision_state(&repository, base(&repository).await).await;
        let sub = node_at(&repository, &state, "Dir/Sub").await;
        let link = Node {
            name_hash: hash_string("Link"),
            child: first,
            ..link_to(id, pin)
        };
        add(&state, repository.clone(), sub, link, "Link").await;
        let base = commit(&repository, &state, base(&repository).await, branch_id()).await;
        let target = edited(&repository, base, &[Edit::Write("Other.txt", 3)]).await;
        for (renaming, changes, at) in [
            (
                &[RENAME_SUB][..],
                &[
                    "Move Dir/sub <- Dir/Sub",
                    "Keep Dir/sub/Link <- Dir/Sub/Link",
                ][..],
                "Dir/sub/Link",
            ),
            (&[], &["Keep Dir/Sub/Link <- Dir/Sub/Link"], "Dir/Sub/Link"),
        ] {
            let state =
                revision_state(&repository, edited(&repository, base, renaming).await).await;
            let link = node_at(&repository, &state, at).await;
            let address = Address {
                hash: pin,
                context: Context::from(id),
            };
            state
                .node_set_link_target(
                    &repository,
                    link,
                    LinkPin::Whole {
                        address,
                        child: second,
                    },
                )
                .await
                .expect("mounting another node");
            state
                .node_mark(
                    &repository,
                    link,
                    lore_revision::state::Mark::staged_and_dirty(
                        NodeFlags::StagedModify,
                        NodeFlags::DirtyModify,
                    ),
                )
                .await
                .expect("staging the mount");
            let source = commit(&repository, &state, base, branch_id()).await;
            let diff = diff3_collect(repository.clone(), base, source, target, None, false)
                .await
                .expect("diffing the revisions");
            assert_eq!(
                outcome(&diff),
                (changes.iter().map(ToString::to_string).collect(), vec![])
            );
            let merged = contents(
                &repository,
                &*revision_state(&repository, apply(&repository, &diff).await).await,
            )
            .await;
            assert_eq!(
                merged.get(at).and_then(|link| link.3),
                Some((Context::from(id), second)),
                "{merged:?}"
            );
        }
    })
    .await;
}

/// A link one side mounts at another node of the revision it pins, against the other side's
/// rename of the link that changes only case, whichever side makes which: the fast-forward merge
/// stores the link at the new name, mounting the new node.
#[tokio::test]
async fn a_remount_merges_with_a_case_only_rename_of_the_link() {
    run(|repository| async move {
        let (id, linked) = linked_repository(&repository).await;
        let linked_state = State::new();
        for name in ["first.txt", "second.txt"] {
            add(
                &linked_state,
                linked.clone(),
                ROOT_NODE,
                file_at(name, 4),
                name,
            )
            .await;
        }
        let pin = commit(&linked, &linked_state, Hash::default(), branch_id()).await;
        let pinned = revision_state(&linked, pin).await;
        let second = node_at(&linked, &pinned, "second.txt").await;
        let state = State::new();
        let link = Node {
            child: node_at(&linked, &pinned, "first.txt").await,
            ..link_to(id, pin)
        };
        add(&state, repository.clone(), ROOT_NODE, link, "Mount").await;
        let base = commit(&repository, &state, Hash::default(), branch_id()).await;
        let state = revision_state(&repository, base).await;
        let link = node_at(&repository, &state, "Mount").await;
        let address = Address {
            hash: pin,
            context: Context::from(id),
        };
        state
            .node_set_link_target(
                &repository,
                link,
                LinkPin::Whole {
                    address,
                    child: second,
                },
            )
            .await
            .expect("mounting another node");
        state
            .node_mark(
                &repository,
                link,
                lore_revision::state::Mark::staged_and_dirty(
                    NodeFlags::StagedModify,
                    NodeFlags::DirtyModify,
                ),
            )
            .await
            .expect("staging the mount");
        let remounted = commit(&repository, &state, base, branch_id()).await;
        let renamed = edited(&repository, base, &[Edit::Move("Mount", "mount")]).await;
        for (source, target) in [(remounted, renamed), (renamed, remounted)] {
            let diff = diff3_collect(repository.clone(), base, source, target, None, false)
                .await
                .expect("diffing the revisions");
            assert!(diff.conflicts.is_empty(), "{:?}", outcome(&diff));
            let merged = contents(
                &repository,
                &*revision_state(&repository, apply(&repository, &diff).await).await,
            )
            .await;
            assert_eq!(
                merged.get("mount").and_then(|link| link.3),
                Some((Context::from(id), second)),
                "{:?}: {merged:?}",
                outcome(&diff)
            );
            assert!(!merged.contains_key("Mount"), "{merged:?}");
        }
    })
    .await;
}

/// Two changes of one link that keep its pin and mount other nodes of the revision it pins, one on
/// each side, leave the diff nothing to apply and no conflict, whether or not it lists the changes
/// the two sides make alike: a link merge, where one runs, settles the two mounts.
#[tokio::test]
async fn remounts_of_one_link_on_both_sides_are_left_to_the_link_merge() {
    run(|repository| async move {
        let (id, linked) = linked_repository(&repository).await;
        let linked_state = State::new();
        for name in ["first.txt", "second.txt", "third.txt"] {
            add(
                &linked_state,
                linked.clone(),
                ROOT_NODE,
                file_at(name, 4),
                name,
            )
            .await;
        }
        let pin = commit(&linked, &linked_state, Hash::default(), branch_id()).await;
        let pinned = revision_state(&linked, pin).await;
        let state = State::new();
        let link = Node {
            child: node_at(&linked, &pinned, "first.txt").await,
            ..link_to(id, pin)
        };
        add(&state, repository.clone(), ROOT_NODE, link, "Mount").await;
        let base = commit(&repository, &state, Hash::default(), branch_id()).await;
        let mut remounted = Vec::new();
        for name in ["second.txt", "third.txt"] {
            let state = revision_state(&repository, base).await;
            let link = node_at(&repository, &state, "Mount").await;
            let child = node_at(&linked, &pinned, name).await;
            let address = Address {
                hash: pin,
                context: Context::from(id),
            };
            state
                .node_set_link_target(&repository, link, LinkPin::Whole { address, child })
                .await
                .expect("mounting another node");
            state
                .node_mark(
                    &repository,
                    link,
                    lore_revision::state::Mark::staged_and_dirty(
                        NodeFlags::StagedModify,
                        NodeFlags::DirtyModify,
                    ),
                )
                .await
                .expect("staging the mount");
            remounted.push(commit(&repository, &state, base, branch_id()).await);
        }
        for (source, target) in [(remounted[0], remounted[1]), (remounted[1], remounted[0])] {
            for include_same in [false, true] {
                let diff =
                    diff3_collect(repository.clone(), base, source, target, None, include_same)
                        .await
                        .expect("diffing the revisions");
                assert_eq!(
                    outcome(&diff),
                    (vec![], vec![]),
                    "include_same {include_same}"
                );
            }
        }
    })
    .await;
}

/// A link renamed only in case and given another target is one change where the walk does not
/// descend into the linked repository: pinned to another revision where the reader may not read the
/// linked repository, mounting another node of the revision it pins, and staged, pinned to the
/// revision it pins, with nothing staged below it. For the first two, the 3-way diff lists it once
/// and a fast-forward merge takes the rename and the target.
#[tokio::test]
async fn a_link_renamed_only_in_case_and_repinned_is_one_change_where_the_walk_stops_at_it() {
    run(|repository| async move {
        let (id, linked) = linked_repository(&repository).await;
        let pin = linked_revision(&linked, "x.txt").await;
        let state = State::new();
        add(
            &state,
            repository.clone(),
            ROOT_NODE,
            link_to(id, pin),
            "Mount",
        )
        .await;
        add(
            &state,
            repository.clone(),
            ROOT_NODE,
            file_at("Other.txt", 1),
            "Other.txt",
        )
        .await;
        let base = commit(&repository, &state, Hash::default(), branch_id()).await;
        let target = edited(&repository, base, &[Edit::Write("Other.txt", 3)]).await;
        let repinned = LinkPin::Revision(linked_revision(&linked, "y.txt").await);
        let remounted = LinkPin::Whole {
            address: Address {
                hash: pin,
                context: Context::from(id),
            },
            child: node_at(&linked, &*revision_state(&linked, pin).await, "x.txt").await,
        };
        let unreadable = Arc::new(
            repository
                .to_filter_context(repository.filter.clone())
                .with_link_read(Arc::new(|_| false)),
        );
        let renamed = vec!["Move mount <- Mount".to_string()];
        for (reader, pin) in [(unreadable, repinned), (repository.clone(), remounted)] {
            let source_state = revision_state(&reader, base).await;
            stage_renamed_and_pinned(&reader, &source_state, pin).await;
            let source = commit(&reader, &source_state, base, branch_id()).await;
            let source_state = revision_state(&reader, source).await;
            let link = source_state
                .node(
                    reader.clone(),
                    node_at(&reader, &source_state, "mount").await,
                )
                .await
                .expect("reading the link");
            assert_eq!(
                walked(&walk(&reader, revision_state(&reader, base).await, source_state).await),
                renamed
            );
            let diff = diff3_collect(reader.clone(), base, source, target, None, false)
                .await
                .expect("diffing the revisions");
            assert_eq!(outcome(&diff), (renamed.clone(), vec![]));
            let merged = contents(
                &reader,
                &*revision_state(&reader, apply(&reader, &diff).await).await,
            )
            .await;
            assert_eq!(
                merged.get("mount"),
                Some(&(
                    link.address.hash,
                    0,
                    0o755,
                    link.mount_source()
                        .map(|source| (link.address.context, source))
                ))
            );
            assert!(!merged.contains_key("Mount"), "{merged:?}");
            assert_eq!(merged.get("Other.txt"), Some(&content(3, 0o644)));
        }
        let staged = revision_state(&repository, base).await;
        stage_renamed_and_pinned(&repository, &staged, LinkPin::Revision(pin)).await;
        assert_eq!(
            walked(&walk(&repository, revision_state(&repository, base).await, staged).await),
            renamed
        );
    })
    .await;
}

/// How one side of a generated case renames `Dir` or `Dir/Sub`: not at all, or only in case to the
/// lowercase or the uppercase spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Respelling {
    Kept,
    Lower,
    Upper,
}

/// What one side of a generated case does below `Dir/Sub`: nothing, an edit of `c.txt`, an add of
/// `new.txt`, a delete of `c.txt`, a move of `c.txt` within the directory or out of both, a move
/// of `File.txt` into the directory, a rename of `c.txt` only in case, a change of the mode of
/// `c.txt` alone, or a delete of `c.txt` and an add of another file in its place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Below {
    Nothing,
    Edit,
    Add,
    Delete,
    MoveWithin,
    MoveOut,
    MoveIn,
    RenameFile,
    Chmod,
    Replace,
}

impl Below {
    /// Every change below `Dir/Sub` a generated case makes.
    const ALL: [Below; 10] = [
        Below::Nothing,
        Below::Edit,
        Below::Add,
        Below::Delete,
        Below::MoveWithin,
        Below::MoveOut,
        Below::MoveIn,
        Below::RenameFile,
        Below::Chmod,
        Below::Replace,
    ];

    /// The edits this makes below `dir/sub`, as the side spells the two, writing and adding
    /// content `version`.
    fn edits(self, dir: &str, sub: &str, version: u64) -> Vec<Edit> {
        let at = |name: &str| leaked(format!("{dir}/{sub}/{name}"));
        match self {
            Below::Nothing => vec![],
            Below::Edit => vec![Edit::Write(at("c.txt"), version)],
            Below::Add => vec![Edit::Add(at("new.txt"), version)],
            Below::Delete => vec![Edit::Delete(at("c.txt"))],
            Below::MoveWithin => vec![Edit::Move(at("c.txt"), at("d.txt"))],
            Below::MoveOut => vec![Edit::Move(at("c.txt"), "Out.txt")],
            Below::MoveIn => vec![Edit::Move("File.txt", at("File.txt"))],
            Below::RenameFile => vec![Edit::Move(at("c.txt"), at("C.txt"))],
            Below::Chmod => vec![Edit::Chmod(at("c.txt"), 0o755)],
            Below::Replace => vec![Edit::Delete(at("c.txt")), Edit::Add(at("c.txt"), version)],
        }
    }

    /// Whether this moves a file.
    fn moves(self) -> bool {
        matches!(
            self,
            Below::MoveWithin | Below::MoveOut | Below::MoveIn | Below::RenameFile
        )
    }

    /// Whether this moves `c.txt`.
    fn moves_c(self) -> bool {
        self.moves() && self != Below::MoveIn
    }

    /// The names of the files this changes, at either end of a move.
    fn names(self) -> &'static [&'static str] {
        match self {
            Below::Nothing => &[],
            Below::Edit | Below::Delete | Below::Chmod | Below::Replace => &["c.txt"],
            Below::Add => &["new.txt"],
            Below::MoveWithin => &["c.txt", "d.txt"],
            Below::MoveOut => &["c.txt", "Out.txt"],
            Below::MoveIn => &["File.txt"],
            Below::RenameFile => &["c.txt", "C.txt"],
        }
    }

    /// Whether what `source` and `target` do below one spelling of `Dir/Sub` conflicts: two adds
    /// at one path, two edits of `c.txt`, an edit and a delete of it, a delete and a move of it, two
    /// moves of it to different places, or another file in its place against anything but a change
    /// of its mode or the same nothing.
    fn conflicts(source: Below, target: Below) -> bool {
        match (source, target) {
            (Below::Add, Below::Add)
            | (Below::Edit, Below::Edit | Below::Delete)
            | (Below::Delete, Below::Edit) => true,
            (Below::Replace, other) | (other, Below::Replace) => {
                matches!(other, Below::Edit | Below::Delete | Below::Replace) || other.moves_c()
            }
            (Below::Delete, other) | (other, Below::Delete) => other.moves_c(),
            (lhs, rhs) => lhs != rhs && lhs.moves_c() && rhs.moves_c(),
        }
    }
}

/// One side of a generated case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Side {
    dir: Respelling,
    sub: Respelling,
    below: Below,
}

/// `path` for the life of the test, as [`Edit`] holds its paths.
fn leaked(path: String) -> &'static str {
    Box::leak(path.into_boxed_str())
}

impl Side {
    /// The names this side gives `Dir` and `Sub`.
    fn names(self) -> (&'static str, &'static str) {
        let dir = match self.dir {
            Respelling::Kept => "Dir",
            Respelling::Lower => "dir",
            Respelling::Upper => "DIR",
        };
        let sub = match self.sub {
            Respelling::Kept => "Sub",
            Respelling::Lower => "sub",
            Respelling::Upper => "SUB",
        };
        (dir, sub)
    }

    /// The edits this side makes on the base, writing and adding content `version`.
    fn edits(self, version: u64) -> Vec<Edit> {
        let (dir, sub) = self.names();
        let mut edits = renamed_to(dir, sub);
        edits.extend(self.below.edits(dir, sub, version));
        edits
    }
}

/// The edits renaming `Dir` to `dir` and `Dir/Sub` to `dir/sub`, where either changes.
fn renamed_to(dir: &'static str, sub: &'static str) -> Vec<Edit> {
    let mut edits = Vec::new();
    if dir != "Dir" {
        edits.push(Edit::Move("Dir", dir));
    }
    if sub != "Sub" {
        edits.push(Edit::Move(
            leaked(format!("{dir}/Sub")),
            leaked(format!("{dir}/{sub}")),
        ));
    }
    edits
}

/// The edits whose tree a merge of `source` and `target` stores, where nothing conflicts: the
/// merged renames, then what each side does below them, spelled as the merged tree spells them,
/// the source's at content 2 and the target's at content 3. A change both sides make is made
/// once, and the moves come last, so that a move takes the content and the mode an edit gives
/// the file. A change of the mode gives way to a delete and to another file in its place, and joins
/// an edit of the content.
fn merged_edits(
    (dir, sub): (&'static str, &'static str),
    source: Below,
    target: Below,
) -> Vec<Edit> {
    let mut belows = vec![(source, 2)];
    if target != source {
        belows.push((target, 3));
    }
    let edited = belows
        .iter()
        .find(|(below, _)| *below == Below::Edit)
        .map(|&(_, version)| version);
    if belows.iter().any(|(below, _)| *below == Below::Chmod)
        && (edited.is_some()
            || belows
                .iter()
                .any(|(below, _)| matches!(below, Below::Delete | Below::Replace)))
    {
        belows.retain(|(below, _)| !matches!(below, Below::Chmod | Below::Edit));
    }
    belows.sort_by_key(|(below, _)| below.moves());
    let mut edits = renamed_to(dir, sub);
    if let Some(version) = edited.filter(|_| !belows.iter().any(|(below, _)| *below == Below::Edit))
    {
        edits.push(Edit::WriteMode(
            leaked(format!("{dir}/{sub}/c.txt")),
            version,
            0o755,
        ));
    }
    edits.extend(
        belows
            .into_iter()
            .flat_map(|(below, version)| below.edits(dir, sub, version)),
    );
    edits
}

/// The name a merged tree gives a directory that `source` and `target` name so: the side's that
/// renames it, or `None` where both rename it to different names.
fn merged_name(
    (source, renamed_by_source): (&'static str, bool),
    (target, renamed_by_target): (&'static str, bool),
) -> Option<&'static str> {
    match (renamed_by_source, renamed_by_target) {
        (true, true) if source != target => None,
        (true, _) => Some(source),
        _ => Some(target),
    }
}

/// The cross product of how two sides rename `Dir` and `Dir/Sub` only in case, neither, one, or
/// both alike or to different spellings, and what each does below them. Each case is diffed with
/// and without the changes the two sides make alike listed, and with the sides exchanged, and each
/// of the three diffs is checked against what the case expects:
///
/// - Where the sides rename a directory to different spellings, the diff conflicts on that
///   directory. Where the changes below conflict, as [`Below::conflicts`] says, it conflicts at a
///   path of the files they change. Otherwise it has no conflict. Each of its conflicts stands at
///   the files the changes below touch, or on a directory the two rename to different spellings.
/// - A diff with no conflict merges into the tree [`merged_edits`] make on the base: the same
///   paths, each directory spelled as the side that renames it spells it, and the same content at
///   every file.
/// - In a diff with no conflict, no change stands below a directory the merged tree doesn't hold,
///   and no move or delete other than the target's own takes its node from below a directory the
///   target's tree doesn't hold.
/// - Listing the changes the two sides make alike adds only changes that keep or delete their
///   node, drops only the target's own changes, and leaves the same conflicts, their paths
///   compared ignoring case.
///
/// Every case of the product runs: 25 pairs of renames by 100 pairs of changes below them, 2500
/// cases. A case that panics fails with its two sides named.
#[tokio::test]
async fn merges_below_case_only_renames_keep_their_invariants() {
    run(|repository| async move {
        use Respelling::*;
        let base = base(&repository).await;
        let renames = [
            (Kept, Kept),
            (Lower, Kept),
            (Kept, Lower),
            (Lower, Lower),
            (Lower, Upper),
        ];
        let mut cache = Trees::default();
        let mut failures = Vec::new();
        for (dir_source, dir_target) in renames {
            for (sub_source, sub_target) in renames {
                for source_below in Below::ALL {
                    for target_below in Below::ALL {
                        let source = Side {
                            dir: dir_source,
                            sub: sub_source,
                            below: source_below,
                        };
                        let target = Side {
                            dir: dir_target,
                            sub: sub_target,
                            below: target_below,
                        };
                        let checked = std::panic::AssertUnwindSafe(check_merge_invariants(
                            &repository,
                            base,
                            &mut cache,
                            (source, target),
                        ))
                        .catch_unwind()
                        .await;
                        let failure = match checked {
                            Ok(checked) => checked.err(),
                            Err(panic) => Some(format!(
                                "panics: {}",
                                panic
                                    .downcast_ref::<String>()
                                    .map(String::as_str)
                                    .or_else(|| panic.downcast_ref::<&str>().copied())
                                    .unwrap_or_default()
                            )),
                        };
                        if let Some(failure) = failure {
                            failures.push(format!("{source:?} against {target:?}: {failure}"));
                        }
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} cases fail:\n{}",
            failures.len(),
            failures.join("\n")
        );
    })
    .await;
}

/// The renames and the changes below them a merge with no conflict stores the tree of.
type MergedCase = ((&'static str, &'static str), Below, Below);

/// The revisions and the trees [`merges_below_case_only_renames_keep_their_invariants`] builds,
/// each built once.
#[derive(Default)]
struct Trees {
    revisions: std::collections::HashMap<(Side, u64), Hash>,
    contents: std::collections::HashMap<Hash, BTreeMap<String, Content>>,
    merged: std::collections::HashMap<MergedCase, BTreeMap<String, Content>>,
}

impl Trees {
    /// The revision `side` makes on `base`, writing and adding content `version`.
    async fn revision(
        &mut self,
        repository: &Arc<RepositoryContext>,
        base: Hash,
        side: Side,
        version: u64,
    ) -> Hash {
        if let Some(revision) = self.revisions.get(&(side, version)) {
            return *revision;
        }
        let revision = edited(repository, base, &side.edits(version)).await;
        self.revisions.insert((side, version), revision);
        revision
    }

    /// The tree of `revision`.
    async fn contents(
        &mut self,
        repository: &Arc<RepositoryContext>,
        revision: Hash,
    ) -> BTreeMap<String, Content> {
        if let Some(tree) = self.contents.get(&revision) {
            return tree.clone();
        }
        let tree = contents(repository, &*revision_state(repository, revision).await).await;
        self.contents.insert(revision, tree.clone());
        tree
    }

    /// The tree a merge of `case` stores, as [`merged_edits`] makes it on `base`.
    async fn merged(
        &mut self,
        repository: &Arc<RepositoryContext>,
        base: Hash,
        case: MergedCase,
    ) -> BTreeMap<String, Content> {
        if let Some(tree) = self.merged.get(&case) {
            return tree.clone();
        }
        let (names, source, target) = case;
        let tree = tree_on(repository, base, &merged_edits(names, source, target)).await;
        self.merged.insert(case, tree.clone());
        tree
    }
}

/// What [`merges_below_case_only_renames_keep_their_invariants`] checks of one case: `Err` with
/// what fails.
async fn check_merge_invariants(
    repository: &Arc<RepositoryContext>,
    base: Hash,
    trees: &mut Trees,
    (source, target): (Side, Side),
) -> Result<(), String> {
    let source_revision = trees.revision(repository, base, source, 2).await;
    let target_revision = trees.revision(repository, base, target, 3).await;
    let diff_of = async |from: Hash, onto: Hash, include_same: bool| {
        diff3_collect(repository.clone(), base, from, onto, None, include_same)
            .await
            .expect("diffing the revisions")
    };
    let merged_of = async |diff: &DiffResult| {
        contents(
            repository,
            &*revision_state(repository, apply(repository, diff).await).await,
        )
        .await
    };
    let without = diff_of(source_revision, target_revision, false).await;
    let with = diff_of(source_revision, target_revision, true).await;
    let exchanged = diff_of(target_revision, source_revision, false).await;
    let (changes, conflicts) = outcome(&without);
    let (changes_alike, conflicts_alike) = outcome(&with);
    let context = format!("{changes:?} {conflicts:?}");

    let (source_dir, source_sub) = source.names();
    let (target_dir, target_sub) = target.names();
    let dir = merged_name(
        (source_dir, source.dir != Respelling::Kept),
        (target_dir, target.dir != Respelling::Kept),
    );
    let sub = merged_name(
        (source_sub, source.sub != Respelling::Kept),
        (target_sub, target.sub != Respelling::Kept),
    );
    let below_conflicts = Below::conflicts(source.below, target.below);
    let names = [source.below.names(), target.below.names()].concat();
    for (diff, name) in [
        (&without, "the diff"),
        (&with, "the diff listing alike changes"),
        (&exchanged, "the diff of the sides exchanged"),
    ] {
        let conflicts = outcome(diff).1;
        let conflicts_on = |at: &dyn Fn(&str) -> bool| {
            conflicts
                .iter()
                .any(|(source, target)| at(source) && at(target))
        };
        if dir.is_none() && !conflicts_on(&|path| path.to_lowercase() == "dir") {
            return Err(format!(
                "{name} has no conflict on Dir: {conflicts:?}: {context}"
            ));
        }
        if dir.is_some() && sub.is_none() && !conflicts_on(&|path| path.to_lowercase() == "dir/sub")
        {
            return Err(format!(
                "{name} has no conflict on Dir/Sub: {conflicts:?}: {context}"
            ));
        }
        let at_files = |path: &str| names.contains(&path.rsplit('/').next().unwrap_or(path));
        if below_conflicts && !conflicts_on(&at_files) {
            return Err(format!(
                "{name} has no conflict at {names:?}: {conflicts:?}: {context}"
            ));
        }
        if dir.is_some() && sub.is_some() && !below_conflicts && !conflicts.is_empty() {
            return Err(format!("{name} conflicts: {conflicts:?}: {context}"));
        }
        let expected_at = |path: &str| {
            let folded = path.to_lowercase();
            at_files(path)
                || (dir.is_none() && folded == "dir")
                || (sub.is_none() && folded == "dir/sub")
        };
        if let Some(stray) = conflicts
            .iter()
            .find(|(source, target)| !expected_at(source) || !expected_at(target))
        {
            return Err(format!(
                "{name} conflicts at {stray:?}: {conflicts:?}: {context}"
            ));
        }
    }
    let sorted = |conflicts: &[(String, String)]| {
        let mut sorted = conflicts.to_vec();
        sorted.sort();
        sorted
    };
    let folded = |conflicts: &[(String, String)]| {
        sorted(
            &conflicts
                .iter()
                .map(|(source, target)| (source.to_lowercase(), target.to_lowercase()))
                .collect::<Vec<_>>(),
        )
    };
    if folded(&conflicts) != folded(&conflicts_alike) {
        return Err(format!(
            "listing alike changes conflicts otherwise: {conflicts_alike:?} against {context}"
        ));
    }
    if let Some(extra) = changes_alike.iter().find(|change| {
        !changes.contains(change) && !change.starts_with("Keep ") && !change.starts_with("Delete ")
    }) {
        return Err(format!("listing alike changes adds {extra}: {context}"));
    }
    let is_targets_own = |change: &NodeChange| {
        change.from.mapping.state.revision() == base
            && change.to.mapping.state.revision() == target_revision
    };
    let targets_own = walked(
        &without
            .changes
            .iter()
            .filter(|change| is_targets_own(change))
            .cloned()
            .collect::<Vec<_>>(),
    );
    if let Some(missing) = changes
        .iter()
        .find(|change| !changes_alike.contains(change) && !targets_own.contains(change))
    {
        return Err(format!("listing alike changes drops {missing}: {context}"));
    }
    let (Some(dir), Some(sub)) = (dir, sub) else {
        return Ok(());
    };
    if below_conflicts {
        return Ok(());
    }

    let target_tree = trees.contents(repository, target_revision).await;
    let expected = trees
        .merged(repository, base, ((dir, sub), source.below, target.below))
        .await;
    for (diff, name) in [
        (&without, "the diff"),
        (&with, "the diff listing alike changes"),
    ] {
        for change in &diff.changes {
            let stands_below = |path: &str, tree: &BTreeMap<String, Content>| {
                path.match_indices('/')
                    .map(|(end, _)| path[..end].to_string())
                    .find(|above| !tree.contains_key(above))
            };
            if change.action != lore_revision::change::FileAction::Delete
                && let Some(above) = stands_below(change.path().as_str(), &expected)
            {
                return Err(format!(
                    "{name} puts {} below {above}, which the merged tree doesn't hold: {context}",
                    change.path()
                ));
            }
            if matches!(
                change.action,
                lore_revision::change::FileAction::Move | lore_revision::change::FileAction::Delete
            ) && !is_targets_own(change)
                && let Some(above) = stands_below(change.from.mapping.path.as_str(), &target_tree)
            {
                return Err(format!(
                    "{name} takes {} from below {above}, which the target's tree doesn't hold: \
                     {context}",
                    change.from.mapping.path
                ));
            }
        }
    }
    for (diff, name) in [
        (&without, "the diff"),
        (&with, "the diff listing alike changes"),
        (&exchanged, "the diff of the sides exchanged"),
    ] {
        let merged = merged_of(diff).await;
        if files(&merged) != files(&expected) || !merged.keys().eq(expected.keys()) {
            return Err(format!(
                "{name} merges {merged:?}, not {expected:?}: {:?}",
                outcome(diff)
            ));
        }
    }
    Ok(())
}
