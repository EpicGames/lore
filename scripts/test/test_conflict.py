# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging

import pytest

from error_types import LocalChanges
from lore import Lore, verify_signatures

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_sync_conflict_forward(new_lore_repo):
    repo: Lore = new_lore_repo()
    # Generate some files
    text_file = "text-File.txt"

    repo.write_commit_push(
        "Snapshot 1 - add text file",
        {text_file: ["One line\n", "Another line\n", "Third line\n"]},
        offline=True,
    )

    repo.write_commit_push(
        "Snapshot 2 - modify text file",
        {text_file: ["One line\n", "Another line\n", "Third line\n", "Fourth line\n"]},
    )

    # List all revisions
    output = repo.revision_history()
    verify_signatures(output, 2)

    # Sync source repo back to initial revision
    repo.sync(output[0].signature)

    # Modify a file
    with repo.open_file(text_file, "w+") as output_file:
        output_file.writelines(
            [
                "One line\n",
                "Another line\n",
                "Third line\n",
                "Fourth line\n",
                "Fifth line\n",
            ]
        )

    # Sync source repo back to local latest to trigger a conflict and refuse the sync
    with pytest.raises(LocalChanges):
        repo.sync()

    # Sync source repo back to local latest with forward changes should succeed
    repo.sync(forward_changes=True)

    # Show status, should now get the file as modified
    output = repo.status(unstaged=True)

    assert "M text-File.txt" in output, "File not marked as modified"


@pytest.mark.smoke
def test_sync_conflict_keep_files(new_lore_repo):
    repo: Lore = new_lore_repo()
    # Generate some files
    text_file = "text-File.txt"

    repo.write_commit_push(
        "Snapshot 1 - add text file",
        {text_file: ["One line\n", "Another line\n", "Third line\n"]},
        offline=True,
    )

    repo.write_commit_push(
        "Snapshot 2 - modify text file",
        {text_file: ["One line\n", "Another line\n", "Third line\n", "Fourth line\n"]},
    )

    # List all revisions
    output = repo.revision_history()
    verify_signatures(output, 2)

    # Sync source repo back to initial revision and reset branch latest
    # to match, so that a divergent commit is allowed
    repo.sync(output[0].signature)
    repo.branch_reset(output[0].signature)

    repo.write_commit_push(
        "Snapshot 2 - modify text file (divergent conflict)",
        {
            text_file: [
                "One line\n",
                "Another line\n",
                "Third line\n",
                "Fourth conflicting line\n",
            ]
        },
        offline=True,
    )

    # Sync source repo back to local latest with forward changes should succeed
    repo.sync(forward_changes=True)

    # Show status, should now get the file as conflicted
    output = repo.status(unstaged=True)

    assert "M  text-File.txt (M)!" in output, (
        "File not marked as modified and in conflict"
    )


@pytest.mark.smoke
def test_merge_conflict_keep_files(new_lore_repo):
    repo: Lore = new_lore_repo()
    # Generate some files
    text_file = "text-File.txt"

    repo.write_commit_push(
        "Snapshot 1 - add text file",
        {text_file: ["One line\n", "Another line\n", "Third line\n"]},
        offline=True,
    )

    repo.branch_create("test1")

    repo.write_commit_push(
        "Snapshot 2 - modify text file in test1",
        {text_file: ["One line\n", "Another line\n", "Third line\n", "Fourth line\n"]},
    )

    # List all revisions
    output = repo.revision_history()
    verify_signatures(output, 2)

    # Sync source repo back to initial revision and create second branch
    repo.branch_switch("main")
    repo.branch_create("test2")

    repo.write_commit_push(
        "Snapshot 2 - modify text file in test2",
        {
            text_file: [
                "One line\n",
                "Another line\n",
                "Third line\n",
                "Fourth conflicting line\n",
            ]
        },
    )

    # Merge first branch to trigger conflict resolution
    repo.branch_merge("test1")

    # Show status, should now get the file as conflicted
    output = repo.status(unstaged=True)

    assert "M  text-File.txt (M)!" in output, (
        "File not marked as modified and in conflict"
    )

    assert repo.file_exists(text_file + "~mine"), "Mine file does not exist"
    assert repo.file_exists(text_file + "~theirs"), "Theirs file does not exist"
    assert repo.file_exists(text_file + "~base"), "Base file does not exist"


@pytest.mark.smoke
def test_merge_conflict_reset_shows_file_once(new_lore_repo):
    repo: Lore = new_lore_repo()
    # This test reproduces a bug where after:
    # 1. Merging (with auto-merged file staged)
    # 2. Unstaging the auto-merged file
    # 3. Resetting the auto-merged file
    # The file appears TWICE in "Changes not staged for commit"

    auto_merged_file = "conflicting_file.txt"
    conflicting_file = "truly_conflicting.txt"

    # Initial commit with a typo in auto_merged_file
    with repo.open_file(auto_merged_file, "w+") as output_file:
        output_file.writelines(["conflic in main\n"])  # codespell:ignore conflic

    with repo.open_file(conflicting_file, "w+") as output_file:
        output_file.writelines(["Line A\n", "Line B\n", "Line C\n"])

    repo.stage(scan=True)
    repo.commit("Initial commit")

    # Branch 1: Fix the typo in auto_merged_file
    repo.branch_create("branch1")
    with repo.open_file(auto_merged_file, "w+") as output_file:
        output_file.writelines(["conflict in main\n"])

    with repo.open_file(conflicting_file, "w+") as output_file:
        output_file.writelines(["Line A\n", "Modified by branch1\n", "Line C\n"])

    repo.stage(scan=True)
    repo.commit("Fix typo in branch1")
    repo.push()

    # Branch 2: Keep the typo in auto_merged_file, modify conflicting_file differently
    repo.branch_switch("main")
    repo.branch_create("branch2")
    with repo.open_file(auto_merged_file, "w+") as output_file:
        output_file.writelines(["conflic in main\n"])  # codespell:ignore conflic

    with repo.open_file(conflicting_file, "w+") as output_file:
        output_file.writelines(["Line A\n", "Modified by branch2\n", "Line C\n"])

    repo.stage(scan=True)
    repo.commit("Branch2 changes (keep typo)")
    repo.push()

    # Merge branch1 into branch2
    # auto_merged_file will auto-merge to "conflict in main" (branch1's version, staged)
    # conflicting_file will be in conflict
    repo.branch_merge_start("branch1", no_commit=True, check=False)

    # Unstage the auto-merged file (now contains "conflict in main")
    repo.unstage(auto_merged_file)

    # Reset the auto-merged file (reverts to "conflic in main" - branch2's HEAD)  # codespell:ignore conflic
    # BUG: This should show the file once, but it shows twice
    repo.reset(auto_merged_file)

    # Get status for unstaged changes
    output = repo.status(unstaged=True)

    # Count how many times auto_merged_file appears in "Changes not staged for commit" section
    lines = output.splitlines()
    in_unstaged_section = False
    file_count = 0

    for line in lines:
        if "Changes not staged for commit:" in line:
            in_unstaged_section = True
        elif in_unstaged_section:
            # Check if we've hit a new section (non-indented line that's not a file status)
            if line and not line[0].isspace() and not line[0] in ["M", "A", "D", "R"]:
                # New section started (e.g., empty line or different section header)
                break
            # Count occurrences of the file
            if auto_merged_file in line:
                file_count += 1

    assert file_count == 1, (
        f"BUG: File '{auto_merged_file}' should appear exactly once in unstaged changes, but appeared {file_count} times"
    )


@pytest.mark.smoke
def test_merge_conflict_files_preserved_after_resolve(new_lore_repo):
    """Conflict files (~theirs, ~base) should survive resolve mine/theirs
    so they remain available after unresolve."""
    repo: Lore = new_lore_repo()
    text_file = "text-File.txt"

    repo.write_commit_push(
        "Base commit",
        {text_file: ["One line\n", "Another line\n", "Third line\n"]},
        offline=True,
    )

    repo.branch_create("test1")

    repo.write_commit_push(
        "Modify in test1",
        {text_file: ["One line\n", "Another line\n", "Third line\n", "Fourth line\n"]},
    )

    repo.branch_switch("main")
    repo.branch_create("test2")

    repo.write_commit_push(
        "Modify in test2",
        {
            text_file: [
                "One line\n",
                "Another line\n",
                "Third line\n",
                "Fourth conflicting line\n",
            ]
        },
    )

    repo.branch_merge("test1")

    # Conflict files should exist after merge
    assert repo.file_exists(text_file + "~mine"), "~mine missing after merge"
    assert repo.file_exists(text_file + "~theirs"), "~theirs missing after merge"
    assert repo.file_exists(text_file + "~base"), "~base missing after merge"

    # Resolve mine — file is kept on disk, conflict files should be preserved
    repo.branch_merge_resolve_mine(text_file)

    assert repo.file_exists(text_file + "~theirs"), "~theirs missing after resolve mine"
    assert repo.file_exists(text_file + "~base"), "~base missing after resolve mine"

    # Unresolve — conflict files should still be there
    repo.branch_merge_unresolve(text_file)

    output = repo.status(unstaged=True)
    assert "(M)!" in output, "File should be conflicted after unresolve"
    assert repo.file_exists(text_file + "~theirs"), "~theirs missing after unresolve"
    assert repo.file_exists(text_file + "~base"), "~base missing after unresolve"

    # Resolve theirs — file is kept on disk, conflict files should be preserved
    repo.branch_merge_resolve_theirs(text_file)

    assert repo.file_exists(text_file + "~theirs"), (
        "~theirs missing after resolve theirs"
    )
    assert repo.file_exists(text_file + "~base"), "~base missing after resolve theirs"

    # Commit the merge
    repo.commit("Resolved merge")

    assert not repo.file_exists(text_file + "~mine"), (
        "~mine should be cleaned up after commit"
    )
    assert not repo.file_exists(text_file + "~theirs"), (
        "~theirs should be cleaned up after commit"
    )
    assert not repo.file_exists(text_file + "~base"), (
        "~base should be cleaned up after commit"
    )


@pytest.mark.smoke
def test_merge_resolve_blocked_by_conflict_markers(new_lore_repo):
    """Resolve should be rejected when the file still contains conflict markers."""
    repo: Lore = new_lore_repo()
    text_file = "text-File.txt"

    repo.write_commit_push(
        "Base commit",
        {text_file: ["One line\n", "Another line\n", "Third line\n"]},
        offline=True,
    )

    repo.branch_create("test1")

    repo.write_commit_push(
        "Modify in test1",
        {text_file: ["One line\n", "Another line\n", "Third line\n", "Fourth line\n"]},
    )

    assert not repo.file_exists(text_file + "~mine"), (
        "~mine should be cleaned up after commit"
    )
    assert not repo.file_exists(text_file + "~theirs"), (
        "~theirs should be cleaned up after commit"
    )
    assert not repo.file_exists(text_file + "~base"), (
        "~base should be cleaned up after commit"
    )

    repo.branch_switch("main")
    repo.branch_create("test2")

    repo.write_commit_push(
        "Modify in test2",
        {
            text_file: [
                "One line\n",
                "Another line\n",
                "Third line\n",
                "Fourth conflicting line\n",
            ]
        },
    )

    repo.branch_merge("test1")

    # File should have conflict markers after merge
    with repo.open_file(text_file, "r") as f:
        content = f.read()
    assert "<<<<<<<" in content, "File should contain conflict markers after merge"

    # Attempting to resolve while conflict markers remain should skip the file with a warning
    output = repo.branch_merge_resolve(text_file)
    assert "conflict markers" in output.lower(), (
        "Resolve should warn about conflict markers"
    )

    # Status should still show the file as conflicted (unresolved)
    output = repo.status(unstaged=True)
    assert "(M)!" in output, "File should still be unresolved after failed resolve"

    # Now remove conflict markers by writing clean content
    with repo.open_file(text_file, "w+") as f:
        f.writelines(
            [
                "One line\n",
                "Another line\n",
                "Third line\n",
                "Fourth line merged\n",
            ]
        )

    # Resolve should now succeed
    repo.branch_merge_resolve(text_file)


@pytest.mark.smoke
def test_merge_conflict_binary_files_written(new_lore_repo):
    """Binary file conflicts should write ~base and ~theirs but not ~mine
    (the working copy already is the mine version)."""
    repo: Lore = new_lore_repo()
    binary_file = "image.bin"

    base_content = b"\x89PNG\r\n\x1a\n" + b"\x00" * 64
    mine_content = b"\x89PNG\r\n\x1a\n" + b"\x01" * 64
    theirs_content = b"\x89PNG\r\n\x1a\n" + b"\x02" * 64

    repo.write_commit_push(
        "Base commit with binary file",
        {binary_file: base_content},
        offline=True,
    )

    repo.branch_create("feature")

    repo.write_commit_push(
        "Modify binary in feature",
        {binary_file: theirs_content},
    )

    repo.branch_switch("main")
    repo.branch_create("work")

    repo.write_commit_push(
        "Modify binary in work",
        {binary_file: mine_content},
    )

    repo.branch_merge("feature")

    # Status should show the file as conflicted
    output = repo.status(unstaged=True)
    assert "!" in output, "Binary file should be in conflict"

    # ~base and ~theirs should be written for binary conflicts
    assert repo.file_exists(binary_file + "~base"), "~base missing for binary conflict"
    assert repo.file_exists(binary_file + "~theirs"), "~theirs missing for binary conflict"

    # ~mine should NOT be written for binary — the working copy is already the mine version
    assert not repo.file_exists(binary_file + "~mine"), (
        "~mine should not be written for binary conflict"
    )

    # Verify file contents match expectations
    with repo.open_file(binary_file, "rb") as f:
        assert f.read() == mine_content, "Working copy should be the mine version"
    with repo.open_file(binary_file + "~theirs", "rb") as f:
        assert f.read() == theirs_content, "~theirs should contain incoming version"
    with repo.open_file(binary_file + "~base", "rb") as f:
        assert f.read() == base_content, "~base should contain common ancestor version"
