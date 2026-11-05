// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::BTreeMap;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::filter::FilterMode;
use lore_revision::lore::RepositoryId;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::DiffResult;
use lore_revision::revision::diff3_collect;
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

/// The repository the link at `Mount` points into.
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
    /// Deletes the node at the path.
    Delete(&'static str),
    /// Adds a file at the path, in a directory that exists, holding the version.
    Add(&'static str, u64),
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
            Edit::Add(path, version) => {
                let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
                let parent = node_at(repository, &state, parent).await;
                add(
                    &state,
                    repository.clone(),
                    parent,
                    file_at(name, version),
                    name,
                )
                .await;
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
                    add(&state, repository.clone(), bulk, file_at(&name, 7), &name).await;
                }
                continue;
            }
            Edit::Write(path, version) => (path, 0o644, version),
            Edit::Chmod(path, mode) => (path, mode, 1),
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

/// The content of every node in `state` not staged for deletion, by path. A node staged as a
/// move fails the test: the state is a stored revision, which records no move of its own.
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
            if node.is_staged_delete() {
                continue;
            }
            let name = state
                .node_name_clone(repository.clone(), child)
                .await
                .expect("reading a name");
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            assert!(!node.is_staged_move(), "{path} is stored staged as a move");
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
    apply_tree_changes(repository.clone(), merged.clone(), &diff.changes)
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
    let revision = edited(repository, base, edits).await;
    files(&contents(repository, &*revision_state(repository, revision).await).await)
}

/// Each change of `diff` by its action, the path it stands at and the path its node stood at, and
/// each conflict by the paths of its two changes, the source's first.
fn outcome(diff: &DiffResult) -> (Vec<String>, Vec<(String, String)>) {
    (
        diff.changes
            .iter()
            .map(|change| {
                format!(
                    "{:?} {} <- {}",
                    change.action,
                    change.path(),
                    change.from.mapping.path
                )
            })
            .collect(),
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

/// A change of a moved file's mode on one side and an edit of it on the other conflict, the same
/// move on both sides included, as two edits of the file do.
#[tokio::test]
async fn a_mode_change_against_an_edit_of_a_moved_file_conflicts() {
    run(|repository| async move {
        for moved in &MOVES[..2] {
            let (file, moved_file) = (moved.file, moved.file_moved);
            let movement = Edit::Move(file, moved_file);
            for (source, target, conflict) in [
                (
                    vec![movement, Edit::Chmod(moved_file, 0o755)],
                    vec![movement, Edit::Write(moved_file, 3)],
                    (moved_file, moved_file),
                ),
                (
                    vec![movement, Edit::Write(moved_file, 2)],
                    vec![movement, Edit::Chmod(moved_file, 0o755)],
                    (moved_file, moved_file),
                ),
                (
                    vec![movement, Edit::Chmod(moved_file, 0o755)],
                    vec![Edit::Write(file, 3)],
                    (moved_file, file),
                ),
                (
                    vec![Edit::Write(file, 2)],
                    vec![movement, Edit::Chmod(moved_file, 0o755)],
                    (file, moved_file),
                ),
            ] {
                let diff = diff(&repository, &source, &target).await;
                assert_eq!(
                    outcome(&diff),
                    (
                        vec![],
                        vec![(conflict.0.to_string(), conflict.1.to_string())]
                    ),
                    "{source:?} against {target:?}"
                );
            }
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
/// the edit alone: neither side changes the directory itself.
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
                vec!["Keep Dir/sub/c.txt <- Dir/Sub/c.txt".to_string()],
                vec![]
            )
        );
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
/// stood and where the move put it, as it does with no move: the edit cannot follow the move.
#[tokio::test]
async fn an_edit_against_a_move_onto_a_path_the_source_changed_conflicts_at_both() {
    run(|repository| async move {
        for (source, target, conflicts) in [
            (
                vec![Edit::Write("Dir/a.txt", 2), Edit::Add("Dir/z.txt", 5)],
                vec![Edit::Move("Dir/a.txt", "Dir/z.txt")],
                ["Dir/a.txt", "Dir/z.txt"],
            ),
            (
                vec![Edit::Write("Dir/a.txt", 2), Edit::Write("Dir/b.txt", 3)],
                vec![
                    Edit::Write("Dir/a.txt", 4),
                    Edit::Delete("Dir/b.txt"),
                    Edit::Move("Dir/a.txt", "Dir/b.txt"),
                ],
                ["Dir/a.txt", "Dir/b.txt"],
            ),
        ] {
            let (changes, mut paired) = outcome(&diff(&repository, &source, &target).await);
            paired.sort();
            assert_eq!(
                (changes, paired),
                (
                    vec![],
                    conflicts
                        .iter()
                        .map(|path| (path.to_string(), path.to_string()))
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

/// A link to the root of the revision `pin` of the repository the link at `Mount` points into.
fn link_to(pin: Hash) -> Node {
    Node {
        flags: NodeFlags::Link.bits(),
        mode: 0o755,
        name_hash: hash_string("Mount"),
        child: ROOT_NODE,
        address: Address {
            hash: pin,
            context: Context::from(MOUNT_REPOSITORY),
        },
        ..Default::default()
    }
}

/// A revision of the repository the link at `Mount` points into, holding `l.txt` and `name`.
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
    let link = node_at(repository, &state, "Mount").await;
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
        .node_set_link_target(
            repository,
            link,
            lore_revision::state::LinkPin::Revision(pin),
        )
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
    commit(repository, &state, base, branch_id()).await
}

/// The same rename of a link that only changes case on both sides conflicts where the two pin it to
/// different revisions, as two edits of one file do, and is no change of either side where they pin
/// it alike.
#[tokio::test]
async fn the_same_case_only_rename_of_a_link_conflicts_where_the_pins_differ() {
    run(|repository| async move {
        let linked = repository
            .to_link_context(RepositoryId::from(MOUNT_REPOSITORY))
            .await;
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
            link_to(base_pin),
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
