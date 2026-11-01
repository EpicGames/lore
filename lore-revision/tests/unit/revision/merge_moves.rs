// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_methods)] // Test fixtures rename and read the working tree directly.

//! `lore branch merge` of a move on one branch against changes to the moved node on the other,
//! as the working tree and the merge revision hold it.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::commit;
use lore_revision::commit::CommitOptions;
use lore_revision::file;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::MetadataInherit;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository;
use lore_revision::repository::BranchSwitchOptions;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::stage::StageCaseChange;
use lore_revision::stage::StageOptions;
use lore_revision::state::State;

use crate::helper::test_file_write;
use crate::helper::test_store_create;

/// The files every fixture starts from, each holding its own path.
const TREE: [&str; 4] = ["Dir/a.txt", "Dir/b.txt", "Dir/Sub/c.txt", "Top.txt"];

/// A repository on a working tree of its own, its stores in the tree's `.lore` directory.
struct Instance {
    repository: Arc<RepositoryContext>,
    write_token: RepositoryWriteToken,
    path: PathBuf,
    _tempdir: TempDir,
}

/// Runs `test` offline against an instance holding [`TREE`] committed on the main branch.
async fn run<F, Fut>(test: F)
where
    F: FnOnce(Instance) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    test_store_create().await.expect("Failed to create stores");
    let execution = Arc::new(ExecutionContext::new_client(
        LoreGlobalArgs::default().set_offline(),
        EventDispatcher::no_dispatch(),
    ));
    execution.set_user_id("operator@example.com").await;
    lore_base::lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let tempdir = TempDir::new("lore-merge-moves-");
        let path = tempdir.to_path_buf();
        let main = BranchId::from(uuid::Uuid::now_v7());
        let write_token = RepositoryWriteToken::acquire(&path).await;
        let repository = repository::create_local(
            &path,
            &write_token,
            RepositoryId::from(uuid::Uuid::now_v7()),
            main,
            branch::DEFAULT_DEFAULT_NAME.to_string(),
            repository::RepositoryConfig::default(),
            false,
        )
        .await
        .expect("Failed to create the repository");
        let instance = Instance {
            repository,
            write_token,
            path,
            _tempdir: tempdir,
        };
        for path in TREE {
            write(&instance, path, path);
        }
        stage_and_commit(&instance, &instance.path, "First").await;
        test(instance).await;
    }))
    .await
    .expect("Test task failed");
}

/// Writes `content` to the file at `path` in the working tree, creating its directory.
fn write(instance: &Instance, path: &str, content: &str) {
    let absolute = instance.path.join(path);
    std::fs::create_dir_all(absolute.parent().expect("A parent directory"))
        .expect("Failed to create the directory");
    test_file_write(&absolute, content.as_bytes());
}

/// Every file under `root` by its path, spelled as the file system holds it, with its content.
fn working_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((directory, prefix)) = pending.pop() {
        for entry in std::fs::read_dir(&directory).expect("Failed to list a directory") {
            let entry = entry.expect("Failed to read a directory entry");
            let name = entry.file_name().into_string().expect("A UTF-8 name");
            if prefix.is_empty() && name == repository::DOT_LORE {
                continue;
            }
            let path = format!("{prefix}{name}");
            if entry.file_type().expect("A file type").is_dir() {
                pending.push((entry.path(), format!("{path}/")));
            } else {
                files.insert(
                    path,
                    std::fs::read(entry.path()).expect("Failed to read a file"),
                );
            }
        }
    }
    files
}

/// `files` with `to` in place of `from` at the start of every path below or at `from`.
fn renamed(files: &BTreeMap<String, Vec<u8>>, from: &str, to: &str) -> BTreeMap<String, Vec<u8>> {
    files
        .iter()
        .map(|(path, content)| {
            let path = match path.strip_prefix(from) {
                Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{to}{rest}"),
                _ => path.clone(),
            };
            (path, content.clone())
        })
        .collect()
}

/// Stages `path` and everything below it, taking a name that differs from the tree's only in case
/// as a rename, and commits it without forcing, answering the revision.
async fn stage_and_commit(instance: &Instance, path: &Path, message: &str) -> Hash {
    file::stage::stage(
        instance.repository.clone(),
        &instance.write_token,
        LoreArray::from_vec(vec![LoreString::from(path)]),
        StageOptions {
            case_change: StageCaseChange::Rename,
            scan: true,
            ..Default::default()
        },
    )
    .await
    .expect("Failed to stage");
    commit::commit_boxed(
        instance.repository.clone(),
        &instance.write_token,
        CommitOptions::new(message.to_string()),
    )
    .await
    .expect("The commit succeeds without --force")
}

async fn switch_to(instance: &Instance, branch: &str) {
    repository::branch_switch(
        instance.repository.clone(),
        &instance.write_token,
        branch.to_string(),
        BranchSwitchOptions {
            signature: None,
            local: true,
            reset: false,
            search_nearest: false,
            bare: false,
        },
    )
    .await
    .expect("Failed to switch branch");
}

async fn create_branch(instance: &Instance, name: &str) {
    branch::create::create(
        instance.repository.clone(),
        &instance.write_token,
        name.to_string(),
        None,
        String::new(),
        false,
    )
    .await
    .expect("Failed to create the branch");
}

async fn current(instance: &Instance) -> Hash {
    lore_revision::instance::load_current_anchor_boxed(&instance.repository)
        .await
        .expect("Failed to load the current revision")
        .0
}

async fn revision_state(instance: &Instance, revision: Hash) -> Arc<State> {
    State::deserialize(instance.repository.clone(), revision)
        .await
        .expect("Failed to read the revision")
}

/// Merges the `feature` branch into the current one, answering whether the merge committed.
async fn merge_feature(instance: &Instance) -> bool {
    let before = current(instance).await;
    let feature = branch::resolve(instance.repository.clone(), "feature")
        .await
        .expect("Failed to resolve the feature branch")
        .id;
    Box::pin(branch::merge::merge_start(
        instance.repository.clone(),
        &instance.write_token,
        feature,
        branch::merge::MergeStartOptions {
            message: "Merge".to_string(),
            link_messages: std::collections::HashMap::new(),
            no_commit: false,
            scope: branch::merge::MergeScope::MainOnly,
            inherit_metadata: MetadataInherit::default(),
        },
    ))
    .await
    .expect("Failed to merge");
    current(instance).await != before
}

/// The names `revision` stores along `path`, which is looked up ignoring case.
async fn names_along(instance: &Instance, revision: Hash, path: &str) -> Vec<String> {
    let state = revision_state(instance, revision).await;
    let mut node: NodeID = state
        .find_node_link(instance.repository.clone(), path)
        .await
        .expect("Failed to find the path")
        .node;
    let mut names = Vec::new();
    while node != ROOT_NODE {
        names.push(
            state
                .node_name_clone(instance.repository.clone(), node)
                .await
                .expect("Failed to read a name"),
        );
        node = state
            .node(instance.repository.clone(), node)
            .await
            .expect("Failed to read a node")
            .parent;
    }
    names.reverse();
    names
}

/// An edit a branch makes to the working tree.
#[derive(Clone, Copy, Debug)]
enum Edit {
    /// Renames the first path to the second.
    Move(&'static str, &'static str),
    /// Writes the content to the file at the path.
    Write(&'static str, &'static str),
    /// Deletes the file or the directory at the path.
    Delete(&'static str),
}

/// A move the tests make on one branch: the node it moves and where to, and a file at or below the
/// node with the path the move gives it.
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

/// The edits of the branch making `moved`, and an edit of the moved file to `content` where one
/// is given. A directory's address folds case, so the move comes with an edit of `Dir/b.txt` beside
/// it for the revision diff to report a move that only changes case.
fn moving(moved: &Moved, content: Option<&'static str>) -> Vec<Edit> {
    let mut edits = vec![
        Edit::Move(moved.from, moved.to),
        Edit::Write("Dir/b.txt", "moved"),
    ];
    edits.extend(content.map(|content| Edit::Write(moved.file_moved, content)));
    edits
}

/// Makes `edits` in the working tree, stages each move as one, and commits them.
async fn commit_edits(instance: &Instance, edits: &[Edit], message: &str) {
    for edit in edits {
        match *edit {
            Edit::Move(from, to) => {
                let (from, to) = (instance.path.join(from), instance.path.join(to));
                std::fs::rename(&from, &to).expect("rename");
                file::stage::stage_move(
                    instance.repository.clone(),
                    &instance.write_token,
                    from.to_string_lossy().into_owned(),
                    to.to_string_lossy().into_owned(),
                    StageOptions {
                        case_change: StageCaseChange::Rename,
                        scan: true,
                        ..Default::default()
                    },
                )
                .await
                .expect("Failed to stage the move");
            }
            Edit::Write(path, content) => write(instance, path, content),
            Edit::Delete(path) => {
                let absolute = instance.path.join(path);
                if absolute.is_dir() {
                    std::fs::remove_dir_all(absolute).expect("Failed to delete");
                } else {
                    std::fs::remove_file(absolute).expect("Failed to delete");
                }
            }
        }
    }
    stage_and_commit(instance, &instance.path, message).await;
}

/// The paths the staged revision marks as in conflict, and the paths of the files it holds.
async fn staged(instance: &Instance) -> (Vec<String>, Vec<String>) {
    let revision = lore_revision::instance::load_staged_revision(&instance.repository)
        .await
        .expect("Failed to load the staged revision")
        .expect("A merge left a staged revision");
    let state = revision_state(instance, revision).await;
    let (mut conflicts, mut files) = (Vec::new(), Vec::new());
    let mut pending = vec![(ROOT_NODE, String::new())];
    while let Some((parent, prefix)) = pending.pop() {
        for child in state
            .node_children(instance.repository.clone(), parent)
            .await
            .expect("Failed to list the children")
        {
            let node = state
                .node(instance.repository.clone(), child)
                .await
                .expect("Failed to read a node");
            if node.is_staged_delete() {
                continue;
            }
            let name = state
                .node_name_clone(instance.repository.clone(), child)
                .await
                .expect("Failed to read a name");
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if node.is_staged_merge_conflict() {
                conflicts.push(path.clone());
            }
            if node.is_directory() {
                pending.push((child, path));
            } else {
                files.push(path);
            }
        }
    }
    conflicts.sort();
    files.sort();
    (conflicts, files)
}

/// Commits `feature` on a branch of that name and `main` on the main branch, both made on the
/// first revision, and merges the feature branch into main. Answers whether the merge committed,
/// and the working tree it left.
async fn merge(
    instance: &Instance,
    feature: &[Edit],
    main: &[Edit],
) -> (bool, BTreeMap<String, Vec<u8>>) {
    create_branch(instance, "feature").await;
    commit_edits(instance, feature, "Feature").await;
    switch_to(instance, branch::DEFAULT_DEFAULT_NAME).await;
    commit_edits(instance, main, "Main").await;
    let committed = merge_feature(instance).await;
    (committed, working_tree(&instance.path))
}

/// The working tree [`TREE`] with `moved` made and `Dir/b.txt` rewritten as [`moving`] does, and
/// the moved file holding `content`.
fn merged_tree(moved: &Moved, content: &str) -> BTreeMap<String, Vec<u8>> {
    let mut tree: BTreeMap<String, Vec<u8>> = TREE
        .iter()
        .map(|path| (path.to_string(), path.as_bytes().to_vec()))
        .collect();
    tree = renamed(&tree, moved.from, moved.to);
    tree.insert("Dir/b.txt".to_string(), b"moved".to_vec());
    tree.insert(moved.file_moved.to_string(), content.as_bytes().to_vec());
    tree
}

/// A branch moving a file or a directory merges with one editing the moved file, whichever of
/// the two is merged into the other: the working tree and the merge revision hold the file where
/// the move put it, with the content the edit gave it, and nothing at the path it left.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_against_a_move_merges_both() {
    for moved in &MOVES {
        for move_on_feature in [true, false] {
            run(move |instance| async move {
                let edit = vec![Edit::Write(moved.file, "edited")];
                let (feature, main) = if move_on_feature {
                    (moving(moved, None), edit)
                } else {
                    (edit, moving(moved, None))
                };
                let (committed, tree) = merge(&instance, &feature, &main).await;
                let context = format!("{feature:?} into {main:?}");
                assert!(committed, "{context}: the merge did not commit");
                assert_eq!(tree, merged_tree(moved, "edited"), "{context}");
                let names =
                    names_along(&instance, current(&instance).await, moved.file_moved).await;
                assert_eq!(names.join("/"), moved.file_moved, "{context}");
            })
            .await;
        }
    }
}

/// What a merge that conflicts leaves: the path it marks as in conflict, the files of the working
/// tree with what each holds, and the files of the staged revision.
struct Conflicted {
    conflict: &'static str,
    tree: &'static [(&'static str, &'static str)],
    staged: &'static [&'static str],
}

/// Merges `feature` into `main` as [`merge`] does, and checks that the merge leaves `expected`.
async fn merge_conflicts(
    instance: &Instance,
    feature: &[Edit],
    main: &[Edit],
    expected: &Conflicted,
) {
    let (committed, tree) = merge(instance, feature, main).await;
    let context = format!("{feature:?} into {main:?}");
    assert!(!committed, "{context}: the merge committed");
    let expected_tree: BTreeMap<String, Vec<u8>> = expected
        .tree
        .iter()
        .map(|(path, content)| (path.to_string(), content.as_bytes().to_vec()))
        .collect();
    assert_eq!(tree, expected_tree, "{context}");
    assert_eq!(
        staged(instance).await,
        (
            vec![expected.conflict.to_string()],
            expected
                .staged
                .iter()
                .map(|path| path.to_string())
                .collect()
        ),
        "{context}"
    );
}

/// Edits of one file on both branches conflict, whether or not one of them also moved it, the move
/// on either branch, as [`MOVES`] lists them.
#[tokio::test(flavor = "multi_thread")]
async fn edits_on_both_branches_conflict_whether_or_not_one_moved() {
    const EXPECTED: [[Conflicted; 2]; 4] = [
        [
            Conflicted {
                conflict: "Dir/z.txt",
                tree: &[
                    ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
                    ("Dir/a.txt", "main"),
                    ("Dir/b.txt", "moved"),
                    ("Dir/z.txt", "feature"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &[
                    "Dir/Sub/c.txt",
                    "Dir/a.txt",
                    "Dir/b.txt",
                    "Dir/z.txt",
                    "Top.txt",
                ],
            },
            Conflicted {
                conflict: "Dir/a.txt",
                tree: &[
                    ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
                    ("Dir/a.txt", "feature"),
                    ("Dir/b.txt", "moved"),
                    ("Dir/z.txt", "main"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &[
                    "Dir/Sub/c.txt",
                    "Dir/a.txt",
                    "Dir/b.txt",
                    "Dir/z.txt",
                    "Top.txt",
                ],
            },
        ],
        [
            Conflicted {
                conflict: "Dir/a.txt",
                tree: &[
                    ("Dir/A.txt", "feature"),
                    ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
                    ("Dir/a.txt", "main"),
                    ("Dir/b.txt", "moved"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/Sub/c.txt", "Dir/a.txt", "Dir/b.txt", "Top.txt"],
            },
            Conflicted {
                conflict: "Dir/A.txt",
                tree: &[
                    ("Dir/A.txt", "main"),
                    ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
                    ("Dir/a.txt", "feature"),
                    ("Dir/b.txt", "moved"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/A.txt", "Dir/Sub/c.txt", "Dir/b.txt", "Top.txt"],
            },
        ],
        [
            Conflicted {
                conflict: "Dir/Zub/c.txt",
                tree: &[
                    ("Dir/Sub/c.txt", "main"),
                    ("Dir/Zub/c.txt", "feature"),
                    ("Dir/a.txt", "Dir/a.txt"),
                    ("Dir/b.txt", "moved"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/Zub/c.txt", "Dir/a.txt", "Dir/b.txt", "Top.txt"],
            },
            Conflicted {
                conflict: "Dir/Sub/c.txt",
                tree: &[
                    ("Dir/Sub/c.txt", "feature"),
                    ("Dir/Zub/c.txt", "main"),
                    ("Dir/a.txt", "Dir/a.txt"),
                    ("Dir/b.txt", "moved"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &[
                    "Dir/Sub/c.txt",
                    "Dir/Zub/c.txt",
                    "Dir/a.txt",
                    "Dir/b.txt",
                    "Top.txt",
                ],
            },
        ],
        [
            Conflicted {
                conflict: "Dir/sub/c.txt",
                tree: &[
                    ("Dir/a.txt", "Dir/a.txt"),
                    ("Dir/b.txt", "moved"),
                    ("Dir/sub/c.txt", "feature"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/a.txt", "Dir/b.txt", "Dir/sub/c.txt", "Top.txt"],
            },
            Conflicted {
                conflict: "Dir/sub/c.txt",
                tree: &[
                    ("Dir/Sub/c.txt", "feature"),
                    ("Dir/a.txt", "Dir/a.txt"),
                    ("Dir/b.txt", "moved"),
                    ("Dir/sub/c.txt", "main"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/a.txt", "Dir/b.txt", "Dir/sub/c.txt", "Top.txt"],
            },
        ],
    ];
    for (moved, expected) in MOVES.iter().zip(&EXPECTED) {
        for (move_on_feature, expected) in [true, false].into_iter().zip(expected) {
            run(move |instance| async move {
                let (feature, main) = if move_on_feature {
                    (
                        moving(moved, Some("feature")),
                        vec![Edit::Write(moved.file, "main")],
                    )
                } else {
                    (
                        vec![Edit::Write(moved.file, "feature")],
                        moving(moved, Some("main")),
                    )
                };
                merge_conflicts(&instance, &feature, &main, expected).await;
            })
            .await;
        }
    }
}

/// Moves of one file to different names on the two branches conflict, names differing only in
/// case included.
#[tokio::test(flavor = "multi_thread")]
async fn moves_to_different_names_conflict() {
    const CASES: [(&str, &str, Conflicted); 2] = [
        (
            "Dir/z.txt",
            "Dir/y.txt",
            Conflicted {
                conflict: "Dir/z.txt",
                tree: &[
                    ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
                    ("Dir/b.txt", "moved"),
                    ("Dir/y.txt", "Dir/a.txt"),
                    ("Dir/z.txt", "Dir/a.txt"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &[
                    "Dir/Sub/c.txt",
                    "Dir/b.txt",
                    "Dir/y.txt",
                    "Dir/z.txt",
                    "Top.txt",
                ],
            },
        ),
        (
            "Dir/A.txt",
            "Dir/A.TXT",
            Conflicted {
                conflict: "Dir/A.TXT",
                tree: &[
                    ("Dir/A.TXT", "Dir/a.txt"),
                    ("Dir/A.txt", "Dir/a.txt"),
                    ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
                    ("Dir/b.txt", "moved"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/A.TXT", "Dir/Sub/c.txt", "Dir/b.txt", "Top.txt"],
            },
        ),
    ];
    for (feature, main, expected) in &CASES {
        run(move |instance| async move {
            let feature = [
                Edit::Move("Dir/a.txt", feature),
                Edit::Write("Dir/b.txt", "moved"),
            ];
            let main = [
                Edit::Move("Dir/a.txt", main),
                Edit::Write("Dir/b.txt", "moved"),
            ];
            merge_conflicts(&instance, &feature, &main, expected).await;
        })
        .await;
    }
}

/// A file deleted on one branch conflicts with the other branch's move of it, where the move put
/// it, whichever branch deletes.
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_against_a_move_conflicts() {
    const DELETE: [Edit; 1] = [Edit::Delete("Dir/a.txt")];
    const MOVE: [Edit; 1] = [Edit::Move("Dir/a.txt", "Dir/z.txt")];
    const CASES: [(&[Edit], &[Edit], Conflicted); 2] = [
        (
            &DELETE,
            &MOVE,
            Conflicted {
                conflict: "Dir/z.txt",
                tree: &[
                    ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
                    ("Dir/b.txt", "Dir/b.txt"),
                    ("Dir/z.txt", "Dir/a.txt"),
                    ("Dir/z.txt~base", "Dir/a.txt"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/Sub/c.txt", "Dir/b.txt", "Dir/z.txt", "Top.txt"],
            },
        ),
        (
            &MOVE,
            &DELETE,
            Conflicted {
                conflict: "Dir/z.txt",
                tree: &[
                    ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
                    ("Dir/b.txt", "Dir/b.txt"),
                    ("Dir/z.txt", "Dir/a.txt"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/Sub/c.txt", "Dir/b.txt", "Dir/z.txt", "Top.txt"],
            },
        ),
    ];
    for (feature, main, expected) in &CASES {
        run(move |instance| async move {
            merge_conflicts(&instance, feature, main, expected).await;
        })
        .await;
    }
}

/// A file one branch replaces, by moving another file onto its path, conflicts with the other
/// branch's move of it, whichever branch replaces it.
#[tokio::test(flavor = "multi_thread")]
async fn a_replacement_against_a_move_conflicts() {
    const REPLACE: [Edit; 2] = [
        Edit::Delete("Dir/b.txt"),
        Edit::Move("Dir/a.txt", "Dir/b.txt"),
    ];
    const MOVE: [Edit; 1] = [Edit::Move("Dir/b.txt", "Dir/y.txt")];
    const TREE: &[(&str, &str)] = &[
        ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
        ("Dir/b.txt", "Dir/a.txt"),
        ("Dir/y.txt", "Dir/b.txt"),
        ("Top.txt", "Top.txt"),
    ];
    const STAGED: &[&str] = &["Dir/Sub/c.txt", "Dir/b.txt", "Dir/y.txt", "Top.txt"];
    const CASES: [(&[Edit], &[Edit], Conflicted); 2] = [
        (
            &REPLACE,
            &MOVE,
            Conflicted {
                conflict: "Dir/b.txt",
                tree: TREE,
                staged: STAGED,
            },
        ),
        (
            &MOVE,
            &REPLACE,
            Conflicted {
                conflict: "Dir/y.txt",
                tree: TREE,
                staged: STAGED,
            },
        ),
    ];
    for (feature, main, expected) in &CASES {
        run(move |instance| async move {
            merge_conflicts(&instance, feature, main, expected).await;
        })
        .await;
    }
}

/// A branch editing a file and the file at the path the other branch moves it onto, replacing
/// that file, conflicts at both paths, as it does with no move: neither edit can follow the move.
#[tokio::test(flavor = "multi_thread")]
async fn edits_at_both_ends_of_a_move_conflict_at_both() {
    run(|instance| async move {
        let feature = [
            Edit::Write("Dir/a.txt", "edited"),
            Edit::Write("Dir/b.txt", "feature"),
        ];
        let main = [
            Edit::Delete("Dir/b.txt"),
            Edit::Move("Dir/a.txt", "Dir/b.txt"),
        ];
        let (committed, tree) = merge(&instance, &feature, &main).await;
        assert!(!committed, "the merge committed");
        let expected: BTreeMap<String, Vec<u8>> = [
            ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
            ("Dir/a.txt~theirs", "edited"),
            (
                "Dir/b.txt",
                "<<<<<<< ours\nDir/a.txt\n||||||| original\nDir/b.txt\n=======\nfeature\n>>>>>>> theirs\n",
            ),
            ("Dir/b.txt~base", "Dir/b.txt"),
            ("Dir/b.txt~mine", "Dir/a.txt"),
            ("Dir/b.txt~theirs", "feature"),
            ("Top.txt", "Top.txt"),
        ]
        .iter()
        .map(|(path, content)| (path.to_string(), content.as_bytes().to_vec()))
        .collect();
        assert_eq!(tree, expected);
        assert_eq!(
            staged(&instance).await,
            (
                vec!["Dir/a.txt".to_string(), "Dir/b.txt".to_string()],
                ["Dir/Sub/c.txt", "Dir/a.txt", "Dir/b.txt", "Top.txt"]
                    .iter()
                    .map(|path| path.to_string())
                    .collect()
            )
        );
    })
    .await;
}

/// A directory one branch deletes conflicts with the other branch's rename of it that only changes
/// case, whichever branch deletes. The delete of the files below the directory waits for the
/// conflict, so they stay as the working tree holds them.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_delete_against_a_case_only_rename_keeps_its_files() {
    const DELETE: [Edit; 1] = [Edit::Delete("Dir/Sub")];
    const RENAME: [Edit; 2] = [
        Edit::Move("Dir/Sub", "Dir/sub"),
        Edit::Write("Dir/b.txt", "moved"),
    ];
    const CASES: [(&[Edit], &[Edit], Conflicted); 2] = [
        (
            &DELETE,
            &RENAME,
            Conflicted {
                conflict: "Dir/sub",
                tree: &[
                    ("Dir/a.txt", "Dir/a.txt"),
                    ("Dir/b.txt", "moved"),
                    ("Dir/sub/c.txt", "Dir/Sub/c.txt"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/a.txt", "Dir/b.txt", "Dir/sub/c.txt", "Top.txt"],
            },
        ),
        (
            &RENAME,
            &DELETE,
            Conflicted {
                conflict: "Dir/sub",
                tree: &[
                    ("Dir/a.txt", "Dir/a.txt"),
                    ("Dir/b.txt", "moved"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/a.txt", "Dir/b.txt", "Top.txt"],
            },
        ),
    ];
    for (feature, main, expected) in &CASES {
        run(move |instance| async move {
            merge_conflicts(&instance, feature, main, expected).await;
        })
        .await;
    }
}
