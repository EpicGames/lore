# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import json
import logging
import os

import pytest

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_merge_folder_delete(new_lore_repo):
    repo: Lore = new_lore_repo()

    # Generate test file in root.
    root_file = "root.txt"
    with repo.open_file(root_file, "w+") as output_file:
        output_file.writelines(["One line\n", "Another line\n", "Third line\n"])

    repo.stage(scan=True)
    repo.commit("Revision 1 - added test file in root")
    repo.push()

    # Generate test file in folder.
    repo.make_dirs("Test")

    folder_file = "Test/folder.bin"
    with repo.open_file(folder_file, "w+b") as output_file:
        output_file.write(os.urandom(1000))
    repo.stage(scan=True)
    repo.commit("Revision 2 - added test file in folder")
    repo.push()

    # Switch to 'test-branch'.
    repo.branch_create("test-branch")

    # Modify test file in folder.
    with repo.open_file(folder_file, "w+b") as output_file:
        output_file.write(os.urandom(1000))
    repo.stage(scan=True)
    repo.commit("Revision 3 - modify test file in folder")
    repo.push()

    # Switch to 'main'.
    repo.branch_switch("main")

    # Restore revision 1 as latest, which doesn't contain the folder.
    repo.sync("@1")
    repo.revision_restore("Restoring 1, removing folder")

    # Try to merge 'test-branch', which still contains the folder, into an integration branch.
    repo.branch_create("main-personal")
    repo.branch_merge_start("test-branch")

    # Test that there is a pending conflict.
    assert repo.file_exists("Test/folder.bin~theirs"), (
        "Conflict file not materialized while it should be for Test/folder.bin~theirs"
    )

    # Try to resolve conflict using mine.
    repo.branch_merge_resolve_mine("Test/folder.bin")

    status_output = repo.status(json=True)

    # Parse JSONL (each line is a separate JSON object) and check for conflict and resolved status
    folder_bin_status = None
    folder_test_status = None
    for line in status_output.strip().split("\n"):
        if line.strip():
            item = json.loads(line)
            if (
                item.get("tagName") == "repositoryStatusFile"
                and item.get("data", {}).get("path") == "Test/folder.bin"
            ):
                folder_bin_status = item["data"]
            elif (
                item.get("tagName") == "repositoryStatusFile"
                and item.get("data", {}).get("path") == "Test"
                and item.get("data", {}).get("type") == "directory"
            ):
                folder_test_status = item["data"]

    assert folder_bin_status is not None, (
        f"Test/folder.bin not found in status output - Got:\n{status_output}"
    )
    assert folder_bin_status["flagConflict"], (
        f"Test/folder.bin should be in conflict - Got:\n{status_output}"
    )
    assert not folder_bin_status["flagConflictUnresolved"], (
        f"Test/folder.bin should be resolved - Got:\n{status_output}"
    )
    assert folder_bin_status["action"] == "delete", (
        f"Test/folder.bin should show as deleted (mine=delete, theirs=modify) - Got action: {folder_bin_status['action']}"
    )

    assert folder_test_status is not None, (
        f"Test folder not found in status output - Got:\n{status_output}"
    )
    assert folder_test_status["flagConflict"], (
        f"Test folder should be in conflict - Got:\n{status_output}"
    )
    assert not folder_test_status["flagConflictUnresolved"], (
        f"Test folder should be resolved - Got:\n{status_output}"
    )
    assert folder_test_status["action"] == "delete", (
        f"Test folder should show as deleted (mine=delete, theirs=modify) - Got action: {folder_test_status['action']}"
    )

    # Try to resolve conflict using theirs.
    repo.branch_merge_resolve_theirs("Test/folder.bin")

    status_output = repo.status(json=True)

    # Parse JSONL (each line is a separate JSON object) and check for conflict and resolved status
    folder_bin_status = None
    folder_test_status = None
    for line in status_output.strip().split("\n"):
        if line.strip():
            item = json.loads(line)
            if (
                item.get("tagName") == "repositoryStatusFile"
                and item.get("data", {}).get("path") == "Test/folder.bin"
            ):
                folder_bin_status = item["data"]
            elif (
                item.get("tagName") == "repositoryStatusFile"
                and item.get("data", {}).get("path") == "Test"
                and item.get("data", {}).get("type") == "directory"
            ):
                folder_test_status = item["data"]

    assert folder_bin_status is not None, (
        f"Test/folder.bin not found in status output - Got:\n{status_output}"
    )
    assert folder_bin_status["flagConflict"], (
        f"Test/folder.bin should be in conflict - Got:\n{status_output}"
    )
    assert not folder_bin_status["flagConflictUnresolved"], (
        f"Test/folder.bin should be resolved - Got:\n{status_output}"
    )
    assert folder_bin_status["action"] == "add", (
        f"Test/folder.bin should show as added (mine=delete, theirs=modify, resolved to theirs) - Got action: {folder_bin_status['action']}"
    )

    assert folder_test_status is not None, (
        f"Test folder not found in status output - Got:\n{status_output}"
    )
    assert folder_test_status["flagConflict"], (
        f"Test folder should be in conflict - Got:\n{status_output}"
    )
    assert not folder_test_status["flagConflictUnresolved"], (
        f"Test folder should be resolved - Got:\n{status_output}"
    )
    assert folder_test_status["action"] == "add", (
        f"Test folder should show as added (mine=delete, theirs=modify, resolved to theirs) - Got action: {folder_test_status['action']}"
    )

    # Complete the merge by committing and pushing
    repo.commit("Merged test-branch resolving conflict with theirs (modify)")
    repo.push()

    # Verify the merge was committed correctly by checking final status
    final_status = repo.status()

    # Lore shows clean status as "Local branch in sync with remote"
    assert "local branch in sync with remote" in final_status.lower(), (
        f"Working tree should be clean after commit - Got:\n{final_status}"
    )

    # Verify the file exists with the expected content from theirs branch
    assert repo.file_exists("Test/folder.bin"), (
        "Test/folder.bin should exist after resolving to theirs"
    )

    # Verify that merge conflict marker files are cleaned up
    assert not repo.file_exists("Test/folder.bin~mine"), (
        "Conflict marker ~mine should be cleaned up"
    )
    assert not repo.file_exists("Test/folder.bin~theirs"), (
        "Conflict marker ~theirs should be cleaned up"
    )
    assert not repo.file_exists("Test/folder.bin~base"), (
        "Conflict marker ~base should be cleaned up"
    )


@pytest.mark.smoke
def test_merge_folder_delete_with_added_child(new_lore_repo):
    """
    `branch_merge_start` must not hard-error when the incoming branch adds a
    brand-new file under a nested folder that the current branch has deleted.

    Scenario:
      - Base on `main`: root.txt + Test/Second/existing.bin (Test/Second/ exists).
      - On `feature`: add Test/Second/new_file.bin (new path, didn't exist at base).
      - Back on `main`: delete the whole Test/ folder and commit.
      - Merge `feature` into a personal branch off `main`.

    The merge must complete without errors: the multi-level folder
    (Test/ and Test/Second/) is expected to be recreated in both the Merkle
    tree and on disk, and branchMergeStartEnd must report a clean run.
    """
    repo: Lore = new_lore_repo()

    # Base revision on main: contains Test/Second/ with an existing file.
    with repo.open_file("root.txt", "w+") as output_file:
        output_file.writelines(["One line\n"])
    repo.make_dirs("Test/Second")
    with repo.open_file("Test/Second/existing.bin", "w+b") as output_file:
        output_file.write(os.urandom(512))
    repo.stage(scan=True)
    repo.commit("Revision 1 - root.txt and Test/Second/existing.bin")
    repo.push()

    # Source branch adds a brand-new file deep under the existing folder.
    repo.branch_create("feature")
    with repo.open_file("Test/Second/new_file.bin", "w+b") as output_file:
        output_file.write(os.urandom(512))
    repo.stage(scan=True)
    repo.commit("Revision 2 (feature) - add Test/Second/new_file.bin")
    repo.push()

    # Target branch (main) deletes the Test/ folder entirely, removing both
    # levels of the nested directory.
    repo.branch_switch("main")
    repo.rmtree("Test")
    repo.stage(scan=True)
    repo.commit("Revision 2 (main) - delete Test/ folder")
    repo.push()

    # Match the convention from test_merge_folder_delete: merge into a personal
    # branch rather than directly onto main.
    repo.branch_create("main-personal")

    output = repo.branch_merge_start(
        "feature",
        check=False,
        no_commit=True,
        json=True,
    )
    logger.info("merge_start output:\n%s", output)

    merge_end: dict | None = None
    complete_status: int | None = None
    for line in output.strip().split("\n"):
        line = line.strip()
        if not line:
            continue
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        tag = item.get("tagName")
        data = item.get("data", {})
        if tag == "branchMergeStartEnd":
            merge_end = data
        elif tag == "complete":
            complete_status = data.get("status")

    assert complete_status == 0, (
        f"merge_start reported non-zero completion status {complete_status}. "
        f"Output:\n{output}"
    )
    assert merge_end is not None, (
        f"merge_start did not emit branchMergeStartEnd. Output:\n{output}"
    )
    assert merge_end.get("hasConflicts") in (0, False), (
        f"Unexpected conflicts reported by merge_start: {merge_end}"
    )
    assert repo.file_exists("Test/Second/new_file.bin"), (
        "Test/Second/new_file.bin should exist after recreating the folder"
    )
