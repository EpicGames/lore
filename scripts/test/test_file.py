# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_file(new_lore_repo):
    repo: Lore = new_lore_repo("File")

    # Verify the repository is created
    assert os.path.isdir(repo.dot_path()), "Repository was not created"

    # Generate some files
    text_file = "text-file.txt"
    another_file = "another-file.txt"

    repo.write_commit_push(
        "Test commit",
        {
            text_file: ["One line\n", "Another line in text file\n", "Third line\n"],
            another_file: [
                "One line\n",
                "Another line in another file\n",
                "Third line\n",
            ],
        },
        offline=True,
    )

    # Modify some files, stage and commit
    repo.write_commit_push(
        "Test commit 2",
        {
            text_file: "Modified on second revision\n",
            another_file: "Reduced to one line\n",
        },
        offline=True,
    )

    # Modify some files, stage and commit
    repo.write_commit_push(
        "Test commit 3",
        {
            text_file: "Modified on third revision\n",
            another_file: "Final modification\n",
        },
        offline=True,
    )

    # Modify a file without committing and use file reset to get old state
    with repo.open_file(another_file, "w+") as output_file:
        output_file.writelines(["Something we want to reset\n"])

    repo.file_reset(another_file, offline=True)
    repo.file_reset(text_file, revision="@1", offline=True)

    with repo.open_file(text_file, "r") as input_file:
        lines = input_file.readlines()
        assert lines == ["One line\n", "Another line in text file\n", "Third line\n"], (
            f"Text file not reset to revision 1, got {lines}"
        )

    with repo.open_file(another_file, "r") as input_file:
        lines = input_file.readlines()
        assert lines == ["Final modification\n"], (
            f"Text file not reset to last revision, got {lines}"
        )

    repo.file_reset(offline=True)

    # Set up some merges and modifications to test reset to last merged

    repo.branch_create("test-branch", offline=True)
    repo.branch_switch("main", offline=True)

    repo.write_commit_push(
        "Modified on main",
        {
            text_file: [
                "Modified line\n",
                "Another line in text file\n",
                "Third line was also modified\n",
            ],
        },
        offline=True,
    )

    repo.branch_switch("test-branch", offline=True)

    repo.write_commit_push(
        "Modified another file on branch",
        {
            another_file: ["A change on the branch\n"],
        },
        offline=True,
    )

    repo.write_commit_push(
        "Modified text file on branch",
        {
            text_file: [
                "Modified line on branch\n",
                "Another line in text file also modified on branch\n",
                "Third line was also modified\n",
            ]
        },
        offline=True,
    )

    repo.file_reset(text_file, last_merged_from="main", offline=True)

    with repo.open_file(text_file, "r") as input_file:
        lines = input_file.readlines()
        assert lines == ["Modified on third revision\n"], (
            f"Text file not reset to last merged revision, got {lines}"
        )

    repo.branch_merge("main", offline=True)

    repo.write_commit_push(
        "Modified again on branch",
        {
            text_file: [
                "Modified line on branch again\n",
                "Another line in text file also modified on branch\n",
                "Third line was also modified\n",
            ]
        },
        offline=True,
    )

    third_file = "third-file.txt"

    repo.write_commit_push(
        "Modified again on branch",
        {third_file: ["An added file on the branch that should be deleted\n"]},
        offline=True,
    )

    repo.file_reset(
        [text_file, third_file], last_merged_from="main", offline=True, debug=True
    )

    with repo.open_file(text_file, "r") as input_file:
        lines = input_file.readlines()
        assert lines == [
            "Modified line\n",
            "Another line in text file\n",
            "Third line was also modified\n",
        ], f"Text file not reset to last merged revision, got {lines}"

    assert not repo.file_exists(third_file), (
        "Third file not deleted by merge to last merged revision"
    )

    # Check that we can reset to last merged when branch has no revisions
    repo.branch_switch("main", offline=True, force=True)
    repo.file_reset(offline=True)

    repo.branch_create("test-dummy-reset", offline=True)
    repo.file_reset(
        [text_file, third_file], last_merged_from="main", offline=True, debug=True
    )

    move_file = "move-file.txt"

    repo.write_commit_push(
        "Test commit for move",
        {move_file: ["One line\n", "Another line in text file\n", "Third line\n"]},
        offline=True,
    )

    move_file_moved = "move-file-moved.txt"

    repo.move(move_file, move_file_moved)

    result = repo.stage_move(move_file, move_file_moved, offline=True)

    assert "Staging 1 files (0 modified, 0 added, 0 deleted, 1 moved)" in result, (
        "Failed to stage move file"
    )

    move_dir = "dir"
    repo.make_dirs(move_dir)

    move_file_in_dir = os.path.join(move_dir, "move-file.txt")

    repo.write_commit_push(
        "Test commit for move in directory",
        {
            move_file_in_dir: [
                "One line\n",
                "Another line in text file\n",
                "Third line\n",
            ]
        },
        offline=True,
    )

    move_dir2 = "dir2"

    repo.move(move_dir, move_dir2)

    result = repo.stage_move(move_dir, move_dir2, offline=True)

    assert "Staging 1 directories (0 added, 0 deleted, 1 moved)" in result, (
        "Failed to move stage directory"
    )


@pytest.mark.smoke
def test_file_reset_view(new_lore_repo, scratch_dir):
    repo: Lore = new_lore_repo("FileResetView")

    # Mix of file-level and whole-subdir view filtering. Every directory that
    # survives the view filter retains at least one kept file alongside the
    # dropped one(s).
    keep_top = "top-keep.txt"
    drop_top = "top-drop.txt"
    sub_dir = "sub"
    sub_keep = os.path.join(sub_dir, "keep.txt")
    sub_drop = os.path.join(sub_dir, "drop.txt")
    nested_dir = os.path.join(sub_dir, "nested")
    nested_keep = os.path.join(nested_dir, "keep.txt")
    nested_drop = os.path.join(nested_dir, "drop.txt")
    drop_subdir = "drop_subdir"
    drop_subdir_files = [os.path.join(drop_subdir, f"{i}.txt") for i in range(3)]

    files = {
        keep_top: ["keep at top\n"],
        drop_top: ["drop at top\n"],
        sub_keep: ["keep in sub\n"],
        sub_drop: ["drop in sub\n"],
        nested_keep: ["keep in nested\n"],
        nested_drop: ["drop in nested\n"],
    }
    for path in drop_subdir_files:
        files[path] = [f"{path}\n"]
    repo.write_commit_push("Initial commit", files)

    view_dir = scratch_dir("file-reset-view", create=True)
    view_path = os.path.join(view_dir, "view.txt")
    with open(view_path, "w+") as view_file:
        view_file.write("/" + drop_top + "\n")
        view_file.write("/" + sub_drop.replace(os.sep, "/") + "\n")
        view_file.write("/" + nested_drop.replace(os.sep, "/") + "\n")
        view_file.write("/" + drop_subdir + "\n")

    clone = repo.clone(view=view_path)

    def snapshot_tree(root: str) -> set[str]:
        entries: set[str] = set()
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [d for d in dirnames if d not in (".lore", ".urc")]
            rel_dir = os.path.relpath(dirpath, root)
            for d in dirnames:
                entries.add(os.path.join(rel_dir, d) if rel_dir != "." else d)
            for f in filenames:
                entries.add(os.path.join(rel_dir, f) if rel_dir != "." else f)
        return entries

    before = snapshot_tree(clone.path)

    clone.file_reset(".")

    after = snapshot_tree(clone.path)

    extra = after - before
    assert not extra, (
        f"file reset materialized paths that the clone's view filter did not: "
        f"{sorted(extra)}"
    )
    missing = before - after
    assert not missing, (
        f"file reset removed paths that the clone had materialized: {sorted(missing)}"
    )
