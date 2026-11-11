// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! `lore branch merge` of a move on one branch against changes to the moved node on the other,
//! as the working tree and the merge revision hold it.

use std::collections::BTreeMap;

use lore_revision::branch;
use lore_revision::node::ROOT_NODE;

use super::instance::Edit;
use super::instance::Instance;
use super::instance::commit_edits;
use super::instance::create_branch;
use super::instance::current;
use super::instance::merge_branch;
use super::instance::names_along;
use super::instance::renamed;
use super::instance::revision_state;
use super::instance::run;
use super::instance::switch_to;
use super::instance::working_tree;

/// The files every fixture starts from, each holding its own path.
const TREE: [&str; 4] = ["Dir/a.txt", "Dir/b.txt", "Dir/Sub/c.txt", "Top.txt"];

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
/// is given.
fn moving(moved: &Moved, content: Option<&'static str>) -> Vec<Edit> {
    let mut edits = vec![Edit::Move(moved.from, moved.to)];
    edits.extend(content.map(|content| Edit::Write(moved.file_moved, content)));
    edits
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
    let committed = merge_branch(instance, "feature").await;
    (committed, working_tree(&instance.path))
}

/// The working tree [`TREE`] with `moved` made and the moved file holding `content`.
fn merged_tree(moved: &Moved, content: &str) -> BTreeMap<String, Vec<u8>> {
    let mut tree: BTreeMap<String, Vec<u8>> = TREE
        .iter()
        .map(|path| (path.to_string(), path.as_bytes().to_vec()))
        .collect();
    tree = renamed(&tree, moved.from, moved.to);
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
            run(
                "an_edit_against_a_move_merges_both",
                &TREE,
                move |instance| async move {
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
                },
            )
            .await;
        }
    }
}

/// A branch renaming a directory only in case and editing a file merges with one moving that file,
/// within the directory, out of it or into it, whichever of the two is merged into the other: the
/// working tree and the merge revision hold the file where the move put it, below the directory's
/// new spelling where it stays below the directory, with the content the edit gave it.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_against_a_move_across_a_case_only_rename_merges_all() {
    const RENAME_C: [Edit; 2] = [
        Edit::Move("Dir/Sub", "Dir/sub"),
        Edit::Write("Dir/sub/c.txt", "edited"),
    ];
    const RENAME_TOP: [Edit; 2] = [
        Edit::Move("Dir/Sub", "Dir/sub"),
        Edit::Write("Top.txt", "edited"),
    ];
    const MOVE_C: [Edit; 1] = [Edit::Move("Dir/Sub/c.txt", "Dir/Sub/d.txt")];
    const MOVE_C_OUT: [Edit; 1] = [Edit::Move("Dir/Sub/c.txt", "Dir/d.txt")];
    const MOVE_TOP: [Edit; 1] = [Edit::Move("Top.txt", "Dir/Sub/Top.txt")];
    type Case = (
        &'static [Edit],
        &'static [Edit],
        &'static [(&'static str, &'static str)],
    );
    const CASES: [Case; 3] = [
        (
            &RENAME_C,
            &MOVE_C,
            &[
                ("Dir/a.txt", "Dir/a.txt"),
                ("Dir/b.txt", "Dir/b.txt"),
                ("Dir/sub/d.txt", "edited"),
                ("Top.txt", "Top.txt"),
            ],
        ),
        (
            &RENAME_C,
            &MOVE_C_OUT,
            &[
                ("Dir/a.txt", "Dir/a.txt"),
                ("Dir/b.txt", "Dir/b.txt"),
                ("Dir/d.txt", "edited"),
                ("Top.txt", "Top.txt"),
            ],
        ),
        (
            &RENAME_TOP,
            &MOVE_TOP,
            &[
                ("Dir/a.txt", "Dir/a.txt"),
                ("Dir/b.txt", "Dir/b.txt"),
                ("Dir/sub/Top.txt", "edited"),
                ("Dir/sub/c.txt", "Dir/Sub/c.txt"),
            ],
        ),
    ];
    for (renaming, moving, merged) in &CASES {
        for (feature, main) in [(*renaming, *moving), (*moving, *renaming)] {
            run(
                "an_edit_against_a_move_across_a_case_only_rename_merges_all",
                &TREE,
                move |instance| async move {
                    let (committed, tree) = merge(&instance, feature, main).await;
                    let context = format!("{feature:?} into {main:?}");
                    assert!(committed, "{context}: the merge did not commit");
                    let expected: BTreeMap<String, Vec<u8>> = merged
                        .iter()
                        .map(|(path, content)| (path.to_string(), content.as_bytes().to_vec()))
                        .collect();
                    assert_eq!(tree, expected, "{context}");
                    let (edited, _) = merged
                        .iter()
                        .find(|(_, content)| *content == "edited")
                        .expect("an edited file");
                    let names = names_along(&instance, current(&instance).await, edited).await;
                    assert_eq!(names.join("/"), *edited, "{context}");
                },
            )
            .await;
        }
    }
}

/// The case-only rename of `Dir` the tests below make on one branch.
const RENAME_DIR: Edit = Edit::Move("Dir", "dir");

/// The case-only rename of `Dir/Sub` the tests below make on one branch.
const RENAME_SUB: Edit = Edit::Move("Dir/Sub", "Dir/sub");

/// `tree`, each file holding its own path, with the renames `renames` made one after another, and
/// the file at `edited`, spelled as the renames leave it, moved to `merged_at` and holding
/// `edited`.
fn renamed_and_edited(
    tree: &[&str],
    renames: &[(&str, &str)],
    edited: &str,
    merged_at: &str,
) -> BTreeMap<String, Vec<u8>> {
    let mut files: BTreeMap<String, Vec<u8>> = tree
        .iter()
        .map(|path| (path.to_string(), path.as_bytes().to_vec()))
        .collect();
    for (from, to) in renames {
        files = renamed(&files, from, to);
    }
    files = renamed(&files, edited, merged_at);
    files.insert(merged_at.to_string(), b"edited".to_vec());
    files
}

/// Merges `feature` into `main` as [`merge`] does, and checks that the merge commits, that the
/// working tree holds `expected` with no path spelling a directory with a name of `stale`, and that
/// the merge revision spells `merged_at` as the working tree does.
async fn assert_merges_spelled(
    instance: &Instance,
    (feature, main): (&[Edit], &[Edit]),
    expected: &BTreeMap<String, Vec<u8>>,
    merged_at: &str,
    stale: &[&str],
) {
    let (committed, tree) = merge(instance, feature, main).await;
    let context = format!("{feature:?} into {main:?}");
    assert!(committed, "{context}: the merge did not commit");
    assert!(
        !tree
            .keys()
            .any(|path| path.split('/').any(|name| stale.contains(&name))),
        "{context}: {tree:?}"
    );
    assert_eq!(&tree, expected, "{context}");
    let names = names_along(instance, current(instance).await, merged_at).await;
    assert_eq!(names.join("/"), merged_at, "{context}");
}

/// A branch editing a file merges into one moving the file into `Dir/Sub` or within it, where one
/// branch renames `Dir` only in case and the other `Dir/Sub`, whichever branch renames which: the
/// working tree and the merge revision hold the file below both new spellings, with the content
/// the edit gave it, and no path spells either old one. The other directions are left out: a
/// realize in them, the switch back from the merged branch included, moves or renames a child from
/// below a directory it renames only in case in the same pass, which it does not handle yet.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_against_a_move_across_nested_case_only_renames_merges_all() {
    type Case = ([Edit; 2], [Edit; 2], &'static str, &'static str);
    const CASES: [Case; 4] = [
        (
            [RENAME_SUB, Edit::Write("Top.txt", "edited")],
            [RENAME_DIR, Edit::Move("Top.txt", "dir/Sub/Top.txt")],
            "Top.txt",
            "dir/sub/Top.txt",
        ),
        (
            [RENAME_SUB, Edit::Write("Dir/sub/c.txt", "edited")],
            [RENAME_DIR, Edit::Move("dir/Sub/c.txt", "dir/Sub/d.txt")],
            "dir/sub/c.txt",
            "dir/sub/d.txt",
        ),
        (
            [RENAME_DIR, Edit::Write("Top.txt", "edited")],
            [RENAME_SUB, Edit::Move("Top.txt", "Dir/sub/Top.txt")],
            "Top.txt",
            "dir/sub/Top.txt",
        ),
        (
            [RENAME_DIR, Edit::Write("dir/Sub/c.txt", "edited")],
            [RENAME_SUB, Edit::Move("Dir/sub/c.txt", "Dir/sub/d.txt")],
            "dir/sub/c.txt",
            "dir/sub/d.txt",
        ),
    ];
    for (editing, moving, edited, merged_at) in &CASES {
        run(
            "an_edit_against_a_move_across_nested_case_only_renames_merges_all",
            &TREE,
            move |instance| async move {
                let renames = [("Dir", "dir"), ("dir/Sub", "dir/sub")];
                let expected = renamed_and_edited(&TREE, &renames, edited, merged_at);
                let branches = (&editing[..], &moving[..]);
                assert_merges_spelled(&instance, branches, &expected, merged_at, &["Dir", "Sub"])
                    .await;
            },
        )
        .await;
    }
}

/// A branch renaming `Dir` only in case and editing `Dir/Sub/c.txt` merges with one renaming
/// `Dir/Sub` only in case, whichever is merged into the other: the working tree and the merge
/// revision hold the file below both new spellings, with the content the edit gave it.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_below_case_only_renames_on_both_branches_merges_both() {
    const EDITING: [Edit; 2] = [RENAME_DIR, Edit::Write("dir/Sub/c.txt", "edited")];
    const RENAMING: [Edit; 1] = [RENAME_SUB];
    for branches in [(&EDITING[..], &RENAMING[..]), (&RENAMING[..], &EDITING[..])] {
        run(
            "an_edit_below_case_only_renames_on_both_branches_merges_both",
            &TREE,
            move |instance| async move {
                let renames = [("Dir", "dir"), ("dir/Sub", "dir/sub")];
                let at = "dir/sub/c.txt";
                let expected = renamed_and_edited(&TREE, &renames, at, at);
                assert_merges_spelled(&instance, branches, &expected, at, &["Dir", "Sub"]).await;
            },
        )
        .await;
    }
}

/// [`TREE`] with a directory `Deep` in `Dir/Sub`.
const DEEP_TREE: [&str; 5] = [
    "Dir/a.txt",
    "Dir/b.txt",
    "Dir/Sub/c.txt",
    "Dir/Sub/Deep/e.txt",
    "Top.txt",
];

/// Three directories, one inside the next, renamed only in case on alternating branches: one
/// branch renames `Dir` and `Dir/Sub/Deep`, the other `Dir/Sub`. The branch renaming `Dir/Sub`
/// merges into the other, where one of the two edits a file in `Deep` and the other leaves it or
/// moves it within `Deep`: the working tree and the merge revision hold the file below the three
/// new spellings, with the content the edit gave it. The other directions are left out: a realize
/// in them, the switch back from the merged branch included, moves or renames a child from below a
/// directory it renames only in case in the same pass, which it does not handle yet.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_below_case_only_renames_alternating_over_three_levels_merges_all() {
    const RENAME_DEEP: Edit = Edit::Move("dir/Sub/Deep", "dir/Sub/deep");
    type Case = (&'static [Edit], &'static [Edit], &'static str);
    const CASES: [Case; 3] = [
        (
            &[RENAME_SUB],
            &[
                RENAME_DIR,
                RENAME_DEEP,
                Edit::Write("dir/Sub/deep/e.txt", "edited"),
            ],
            "dir/sub/deep/e.txt",
        ),
        (
            &[RENAME_SUB, Edit::Write("Dir/sub/Deep/e.txt", "edited")],
            &[RENAME_DIR, RENAME_DEEP],
            "dir/sub/deep/e.txt",
        ),
        (
            &[RENAME_SUB, Edit::Write("Dir/sub/Deep/e.txt", "edited")],
            &[
                RENAME_DIR,
                RENAME_DEEP,
                Edit::Move("dir/Sub/deep/e.txt", "dir/Sub/deep/f.txt"),
            ],
            "dir/sub/deep/f.txt",
        ),
    ];
    for (feature, main, merged_at) in &CASES {
        run(
            "an_edit_below_case_only_renames_alternating_over_three_levels_merges_all",
            &DEEP_TREE,
            move |instance| async move {
                let renames = [
                    ("Dir", "dir"),
                    ("dir/Sub", "dir/sub"),
                    ("dir/sub/Deep", "dir/sub/deep"),
                ];
                let expected =
                    renamed_and_edited(&DEEP_TREE, &renames, "dir/sub/deep/e.txt", merged_at);
                let stale = ["Dir", "Sub", "Deep"];
                assert_merges_spelled(&instance, (feature, main), &expected, merged_at, &stale)
                    .await;
            },
        )
        .await;
    }
}

/// A branch renaming `Dir/Sub` only in case, alone, with an edit below it, or a branch renaming
/// `Dir/a.txt` only in case, merges into one renaming `Dir` and the same directory or file to the
/// same names: the two renames of one node are one, and the working tree and the merge revision
/// hold the files below both new spellings.
#[tokio::test(flavor = "multi_thread")]
async fn the_same_case_only_rename_below_another_rename_merges() {
    type Case = (
        &'static [Edit],
        &'static [Edit],
        &'static [(&'static str, &'static str)],
        &'static str,
        Option<&'static str>,
        &'static [&'static str],
    );
    const RENAMES: [(&str, &str); 2] = [("Dir", "dir"), ("dir/Sub", "dir/sub")];
    const CASES: [Case; 3] = [
        (
            &[RENAME_SUB],
            &[RENAME_DIR, Edit::Move("dir/Sub", "dir/sub")],
            &RENAMES,
            "dir/sub/c.txt",
            None,
            &["Dir", "Sub"],
        ),
        (
            &[RENAME_SUB, Edit::Write("Dir/sub/c.txt", "edited")],
            &[RENAME_DIR, Edit::Move("dir/Sub", "dir/sub")],
            &RENAMES,
            "dir/sub/c.txt",
            Some("edited"),
            &["Dir", "Sub"],
        ),
        (
            &[Edit::Move("Dir/a.txt", "Dir/A.txt")],
            &[RENAME_DIR, Edit::Move("dir/a.txt", "dir/A.txt")],
            &[("Dir", "dir"), ("dir/a.txt", "dir/A.txt")],
            "dir/A.txt",
            None,
            &["Dir", "a.txt"],
        ),
    ];
    for (feature, main, renames, checked, content, stale) in &CASES {
        run(
            "the_same_case_only_rename_below_another_rename_merges",
            &TREE,
            move |instance| async move {
                let mut expected: BTreeMap<String, Vec<u8>> = TREE
                    .iter()
                    .map(|path| (path.to_string(), path.as_bytes().to_vec()))
                    .collect();
                for (from, to) in *renames {
                    expected = renamed(&expected, from, to);
                }
                if let Some(content) = content {
                    expected.insert(checked.to_string(), content.as_bytes().to_vec());
                }
                assert_merges_spelled(&instance, (feature, main), &expected, checked, stale).await;
            },
        )
        .await;
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

/// Edits of one file on both branches conflict for each of `cases`: a move one branch also makes,
/// whether the feature branch makes it rather than main, and what the merge leaves. Each case runs
/// in an instance of its own, named for `name`.
async fn edits_on_both_branches_conflict(
    name: &str,
    cases: &'static [(&'static Moved, bool, Conflicted)],
) {
    for (moved, move_on_feature, expected) in cases {
        run(name, &TREE, move |instance| async move {
            let (feature, main) = if *move_on_feature {
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

/// Edits of one file on both branches conflict, whether or not one of them also moved it, the move
/// on either branch, as [`MOVES`] lists them, except where the merge keeps two spellings of a name.
#[tokio::test(flavor = "multi_thread")]
async fn edits_on_both_branches_conflict_whether_or_not_one_moved() {
    const CASES: [(&Moved, bool, Conflicted); 5] = [
        (
            &MOVES[0],
            true,
            Conflicted {
                conflict: "Dir/z.txt",
                tree: &[
                    ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
                    ("Dir/a.txt", "main"),
                    ("Dir/b.txt", "Dir/b.txt"),
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
        ),
        (
            &MOVES[0],
            false,
            Conflicted {
                conflict: "Dir/a.txt",
                tree: &[
                    ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
                    ("Dir/a.txt", "feature"),
                    ("Dir/b.txt", "Dir/b.txt"),
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
        ),
        (
            &MOVES[2],
            true,
            Conflicted {
                conflict: "Dir/Zub/c.txt",
                tree: &[
                    ("Dir/Sub/c.txt", "main"),
                    ("Dir/Zub/c.txt", "feature"),
                    ("Dir/a.txt", "Dir/a.txt"),
                    ("Dir/b.txt", "Dir/b.txt"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/Zub/c.txt", "Dir/a.txt", "Dir/b.txt", "Top.txt"],
            },
        ),
        (
            &MOVES[2],
            false,
            Conflicted {
                conflict: "Dir/Sub/c.txt",
                tree: &[
                    ("Dir/Sub/c.txt", "feature"),
                    ("Dir/Zub/c.txt", "main"),
                    ("Dir/a.txt", "Dir/a.txt"),
                    ("Dir/b.txt", "Dir/b.txt"),
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
        ),
        (
            &MOVES[3],
            true,
            Conflicted {
                conflict: "Dir/sub/c.txt",
                tree: &[
                    ("Dir/a.txt", "Dir/a.txt"),
                    ("Dir/b.txt", "Dir/b.txt"),
                    ("Dir/sub/c.txt", "feature"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/a.txt", "Dir/b.txt", "Dir/sub/c.txt", "Top.txt"],
            },
        ),
    ];
    edits_on_both_branches_conflict(
        "edits_on_both_branches_conflict_whether_or_not_one_moved",
        &CASES,
    )
    .await;
}

/// Edits of one file on both branches conflict where one of them also moved it to a name differing
/// only in case, and the merge keeps both spellings, which only a case-sensitive file system holds.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn edits_on_both_branches_conflict_keeping_both_spellings() {
    const CASES: [(&Moved, bool, Conflicted); 3] = [
        (
            &MOVES[1],
            true,
            Conflicted {
                conflict: "Dir/a.txt",
                tree: &[
                    ("Dir/A.txt", "feature"),
                    ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
                    ("Dir/a.txt", "main"),
                    ("Dir/b.txt", "Dir/b.txt"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/Sub/c.txt", "Dir/a.txt", "Dir/b.txt", "Top.txt"],
            },
        ),
        (
            &MOVES[1],
            false,
            Conflicted {
                conflict: "Dir/A.txt",
                tree: &[
                    ("Dir/A.txt", "main"),
                    ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
                    ("Dir/a.txt", "feature"),
                    ("Dir/b.txt", "Dir/b.txt"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/A.txt", "Dir/Sub/c.txt", "Dir/b.txt", "Top.txt"],
            },
        ),
        (
            &MOVES[3],
            false,
            Conflicted {
                conflict: "Dir/sub/c.txt",
                tree: &[
                    ("Dir/Sub/c.txt", "feature"),
                    ("Dir/a.txt", "Dir/a.txt"),
                    ("Dir/b.txt", "Dir/b.txt"),
                    ("Dir/sub/c.txt", "main"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/a.txt", "Dir/b.txt", "Dir/sub/c.txt", "Top.txt"],
            },
        ),
    ];
    edits_on_both_branches_conflict(
        "edits_on_both_branches_conflict_keeping_both_spellings",
        &CASES,
    )
    .await;
}

/// Moves `Dir/a.txt` on both branches for each of `cases`: where the feature branch moves it,
/// where main does, and what the merge leaves. Each case runs in an instance of its own, named for
/// `name`.
async fn moves_conflict(name: &str, cases: &'static [(&'static str, &'static str, Conflicted)]) {
    for (feature, main, expected) in cases {
        run(name, &TREE, move |instance| async move {
            let feature = [Edit::Move("Dir/a.txt", feature)];
            let main = [Edit::Move("Dir/a.txt", main)];
            merge_conflicts(&instance, &feature, &main, expected).await;
        })
        .await;
    }
}

/// Moves of one file to different names on the two branches conflict.
#[tokio::test(flavor = "multi_thread")]
async fn moves_to_different_names_conflict() {
    const CASES: [(&str, &str, Conflicted); 1] = [(
        "Dir/z.txt",
        "Dir/y.txt",
        Conflicted {
            conflict: "Dir/z.txt",
            tree: &[
                ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
                ("Dir/b.txt", "Dir/b.txt"),
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
    )];
    moves_conflict("moves_to_different_names_conflict", &CASES).await;
}

/// Moves of one file to names differing only in case on the two branches conflict, and the merge
/// keeps both spellings, which only a case-sensitive file system holds.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn moves_to_names_differing_only_in_case_conflict() {
    const CASES: [(&str, &str, Conflicted); 1] = [(
        "Dir/A.txt",
        "Dir/A.TXT",
        Conflicted {
            conflict: "Dir/A.TXT",
            tree: &[
                ("Dir/A.TXT", "Dir/a.txt"),
                ("Dir/A.txt", "Dir/a.txt"),
                ("Dir/Sub/c.txt", "Dir/Sub/c.txt"),
                ("Dir/b.txt", "Dir/b.txt"),
                ("Top.txt", "Top.txt"),
            ],
            staged: &["Dir/A.TXT", "Dir/Sub/c.txt", "Dir/b.txt", "Top.txt"],
        },
    )];
    moves_conflict("moves_to_names_differing_only_in_case_conflict", &CASES).await;
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
        run(
            "a_delete_against_a_move_conflicts",
            &TREE,
            move |instance| async move {
                merge_conflicts(&instance, feature, main, expected).await;
            },
        )
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
    const MERGED: &[(&str, &str)] = &[
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
                tree: MERGED,
                staged: STAGED,
            },
        ),
        (
            &MOVE,
            &REPLACE,
            Conflicted {
                conflict: "Dir/y.txt",
                tree: MERGED,
                staged: STAGED,
            },
        ),
    ];
    for (feature, main, expected) in &CASES {
        run(
            "a_replacement_against_a_move_conflicts",
            &TREE,
            move |instance| async move {
                merge_conflicts(&instance, feature, main, expected).await;
            },
        )
        .await;
    }
}

/// A branch editing a file and the file at the path the other branch moves it onto, replacing
/// that file, conflicts at both paths, as it does with no move: neither edit can follow the move.
#[tokio::test(flavor = "multi_thread")]
async fn edits_at_both_ends_of_a_move_conflict_at_both() {
    run(
        "edits_at_both_ends_of_a_move_conflict_at_both",
        &TREE,
        |instance| async move {
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
        },
    )
    .await;
}

/// A directory one branch deletes conflicts with the other branch's rename of it that only changes
/// case, whichever branch deletes. The delete of the files below the directory waits for the
/// conflict, so they stay as the working tree holds them.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_delete_against_a_case_only_rename_keeps_its_files() {
    const DELETE: [Edit; 1] = [Edit::Delete("Dir/Sub")];
    const RENAME: [Edit; 1] = [Edit::Move("Dir/Sub", "Dir/sub")];
    const CASES: [(&[Edit], &[Edit], Conflicted); 2] = [
        (
            &DELETE,
            &RENAME,
            Conflicted {
                conflict: "Dir/sub",
                tree: &[
                    ("Dir/a.txt", "Dir/a.txt"),
                    ("Dir/b.txt", "Dir/b.txt"),
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
                    ("Dir/b.txt", "Dir/b.txt"),
                    ("Top.txt", "Top.txt"),
                ],
                staged: &["Dir/a.txt", "Dir/b.txt", "Top.txt"],
            },
        ),
    ];
    for (feature, main, expected) in &CASES {
        run(
            "a_directory_delete_against_a_case_only_rename_keeps_its_files",
            &TREE,
            move |instance| async move {
                merge_conflicts(&instance, feature, main, expected).await;
            },
        )
        .await;
    }
}

/// A move or a delete of a file, and a rename only in case of a directory, below a directory both
/// branches rename only in case to one name, take the node from where the working tree holds it:
/// the merged working tree holds the node where the change put it, or no longer, and nothing at its
/// old path.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_or_delete_below_a_rename_both_branches_make_reaches_the_working_tree() {
    type Case = (
        Edit,
        Edit,
        &'static [(&'static str, &'static str)],
        Option<&'static str>,
    );
    const CASES: [Case; 3] = [
        (
            Edit::Move("Dir/Sub", "Dir/sub"),
            Edit::Move("Dir/sub/c.txt", "Dir/sub/d.txt"),
            &[("Dir/Sub", "Dir/sub"), ("Dir/sub/c.txt", "Dir/sub/d.txt")],
            None,
        ),
        (
            Edit::Move("Dir/Sub", "Dir/sub"),
            Edit::Delete("Dir/sub/c.txt"),
            &[("Dir/Sub", "Dir/sub")],
            Some("Dir/sub/c.txt"),
        ),
        (
            Edit::Move("Dir", "dir"),
            Edit::Move("dir/Sub", "dir/sub"),
            &[("Dir", "dir"), ("dir/Sub", "dir/sub")],
            None,
        ),
    ];
    for (rename, change, renames, deleted) in CASES {
        run(
            "a_move_or_delete_below_a_rename_both_branches_make_reaches_the_working_tree",
            &TREE,
            move |instance| async move {
                create_branch(&instance, "feature").await;
                switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
                commit_edits(&instance, &[rename], "Main").await;
                switch_to(&instance, "feature").await;
                commit_edits(&instance, &[rename, change], "Feature").await;
                switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
                let mut expected: BTreeMap<String, Vec<u8>> = TREE
                    .iter()
                    .map(|path| (path.to_string(), path.as_bytes().to_vec()))
                    .collect();
                for (from, to) in renames {
                    expected = renamed(&expected, from, to);
                }
                if let Some(deleted) = deleted {
                    expected.remove(deleted);
                }
                assert!(
                    merge_branch(&instance, "feature").await,
                    "{change:?}: the merge did not commit"
                );
                assert_eq!(working_tree(&instance.path), expected, "{change:?}");
            },
        )
        .await;
    }
}
