# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import shutil

import pytest
from error_types import MergeRequired
from lore_parsers import parse_jsonl

from lore import Lore, verify_signatures

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_merge_into(new_lore_repo):
    repo: Lore = new_lore_repo()
    dummy_file = "dummy.txt"
    with repo.open_file(dummy_file, "w+") as output_file:
        output_file.writelines(
            ["This file exists to create an initial commit on the 'main' branch.\n"]
        )
    # Stage the dummy file
    repo.stage(scan=True, offline=True)
    # Commit the dummy file
    repo.commit("Commit 'dummy.txt' on main branch", offline=True)
    # Push the revision
    repo.push()

    # Create example files
    example_file = "example.txt"
    with repo.open_file(example_file, "w+") as output_file:
        output_file.writelines(
            ["This file exists to create an initial commit on the 'main' branch.\n"]
        )
    repo.make_dirs("main-dir")
    with repo.open_file(os.path.join("main-dir", "a.file"), "w+b") as output_file:
        output_file.write(os.urandom(1234))
    # Stage the example file
    repo.stage(scan=True, offline=True)
    # Commit the example file
    repo.commit("Commit 'example.txt' on main branch", offline=True)
    # Push the revision
    repo.push()

    repo.branch_create("test-branch")

    # Modify example file
    with repo.open_file(example_file, "w+") as output_file:
        output_file.writelines(["One line\n", "Another line\n", "Third line\n"])
    # Add a file
    repo.make_dirs("feature-dir")
    with repo.open_file(
        os.path.join("feature-dir", "another.file"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(12345))
    # Delete a file and directory
    shutil.rmtree(os.path.join(repo.path, "main-dir"))
    # Stage the example file
    repo.stage(scan=True, offline=True)
    # Set metadata on the example file
    repo.file_metadata_set(
        example_file, ["Quote", "Code is read much more often than it is written."]
    )
    # Commit the example file
    repo.commit("Commit on feature branch", offline=True)
    # Push the revision
    repo.push()

    # Commit revision #4 merged into main branch from feature branch - skipping #3 on main branch
    repo.branch_merge_into("main", "Commit merge into main from feature branch")
    repo.branch_switch("main")
    repo.file_metadata_get(example_file, "Quote")
    repo.revision_sync()
    output = repo.revision_history()
    verify_signatures(output, 3)

    # Create example file
    with repo.open_file(example_file, "w+") as output_file:
        output_file.writelines(
            ["One line\n", "Another line\n", "Third line\n", "Fourth line\n"]
        )
    with repo.open_file(
        os.path.join("feature-dir", "another.file"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(123456))
    # Stage the example files
    repo.stage(scan=True, offline=True)
    # Commit the example files
    repo.commit("Commit again on main branch", offline=True)
    # Push the revision
    repo.push()

    repo.branch_switch("test-branch")

    # Add binary file
    binary_file = "binary-File.bin"
    with repo.open_file(binary_file, "w+b") as output_file:
        output_file.write(os.urandom(100))
    # Stage the binary file
    repo.stage(scan=True, offline=True)
    # Commit the binary file
    repo.commit("Commit again on feature branch", offline=True)
    # Push the revision
    repo.push()

    # Merge attempt into main branch from feature branch
    # This should fail because latest revision from main isn't merged to feature branch
    with pytest.raises(MergeRequired):
        repo.branch_merge_into("main", "Commit merge into main from feature branch")

    repo.branch_merge_start("main", no_commit=True)
    repo.branch_merge_resolve_mine(example_file, offline=True)
    repo.commit("Commit resolved merge conflict", offline=True)
    repo.push()

    repo.branch_merge_into("main", "Commit merge into main from feature branch")
    repo.branch_switch("main")
    repo.revision_sync()
    output = repo.revision_history()
    verify_signatures(output, 5)


@pytest.mark.smoke
def test_merge_into_records_the_deletion_of_a_file_the_target_added(new_lore_repo):
    """A branch deleting a file the target's latest revision added, merged into the target,
    deletes the file there, and the merge revision's delta records the deletion."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("Base", {"base.txt": "base\n"})
    repo.write_commit_push("Add a file", {"added.txt": "added\n"})

    repo.branch_create("feature")
    repo.remove_file("added.txt")
    repo.stage(["added.txt"])
    repo.commit("Delete the file")
    repo.push()

    repo.branch_merge_into("main", "Merge the deletion")
    repo.branch_switch("main")
    deltas = parse_jsonl(
        repo.revision_info(delta=True, json=True, offline=True), "revisionInfoDelta"
    )
    assert [(delta["action"], delta["path"]) for delta in deltas] == [
        ("delete", "added.txt")
    ], deltas
    assert not repo.file_exists("added.txt")
