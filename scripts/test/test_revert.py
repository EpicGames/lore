# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest
from error_types import LocalChanges, NotInProgress

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_revert_basic(new_lore_repo):
    """
    Test reverting a commit that modifies, deletes, and adds files:
    1. Create file_a (to be modified) and file_b (to be deleted)
    2. In one commit: modify file_a, delete file_b, add file_c
    3. Revert that commit
    4. Verify: file_a has original content, file_b is restored, file_c is gone
    """
    repo: Lore = new_lore_repo()

    # Create initial files
    initial_a = "Initial A\n"
    initial_b = "Initial B\n"
    with repo.open_file("file_a.txt", "w") as f:
        f.write(initial_a)
    with repo.open_file("file_b.txt", "w") as f:
        f.write(initial_b)
    repo.stage("file_a.txt")
    repo.stage("file_b.txt")
    repo.commit("Initial commit")
    repo.push()

    # One commit: modify file_a, delete file_b, add file_c
    with repo.open_file("file_a.txt", "w") as f:
        f.write("Modified A\n")
    repo.remove_file("file_b.txt")
    with repo.open_file("file_c.txt", "w") as f:
        f.write("New file C\n")
    repo.stage("file_a.txt")
    repo.stage("file_b.txt")
    repo.stage("file_c.txt")
    repo.commit("Modify, delete, and add")
    repo.push()

    # Revert that commit
    revisions = repo.history()
    repo.revision_revert(
        revision=revisions[-1].signature,
        message="Revert modify/delete/add",
    )

    # Modification reverted
    with repo.open_file("file_a.txt", "r") as f:
        assert f.read() == initial_a, "file_a should be reverted to initial content"

    # Deletion reverted
    assert repo.file_exists("file_b.txt"), "file_b should be restored"
    with repo.open_file("file_b.txt", "r") as f:
        assert f.read() == initial_b, "file_b should have original content"

    # Addition reverted
    assert not repo.file_exists("file_c.txt"), "file_c should not exist"


@pytest.mark.smoke
def test_revert_conflict_resolution(new_lore_repo):
    """
    Test revert conflict resolution with both 'mine' and 'theirs' in one test:
    1. Create file_mine and file_theirs with content A
    2. Modify both to content B, commit and push
    3. Modify both to content C, commit and push
    4. Revert the B commit (conflicts on both files)
    5. Resolve file_mine with 'mine' (keep C), file_theirs with 'theirs' (accept A)
    """
    repo: Lore = new_lore_repo()

    # Content A
    content_a = "Content A\n"
    with repo.open_file("file_mine.txt", "w") as f:
        f.write(content_a)
    with repo.open_file("file_theirs.txt", "w") as f:
        f.write(content_a)
    repo.stage("file_mine.txt")
    repo.stage("file_theirs.txt")
    repo.commit("Initial commit with content A")
    repo.push()

    # Content B
    content_b = "Content B\n"
    with repo.open_file("file_mine.txt", "w") as f:
        f.write(content_b)
    with repo.open_file("file_theirs.txt", "w") as f:
        f.write(content_b)
    repo.stage("file_mine.txt")
    repo.stage("file_theirs.txt")
    repo.commit("Modify both to content B")
    repo.push()

    # Capture revision to revert
    revisions = repo.history()
    revision_to_revert = revisions[-1].signature

    # Content C
    content_c = "Content C\n"
    with repo.open_file("file_mine.txt", "w") as f:
        f.write(content_c)
    with repo.open_file("file_theirs.txt", "w") as f:
        f.write(content_c)
    repo.stage("file_mine.txt")
    repo.stage("file_theirs.txt")
    repo.commit("Modify both to content C")
    repo.push()

    # Revert the B commit (conflicts on both files)
    output = repo.revision_revert(
        revision=revision_to_revert,
        message="Revert content B modification",
    )

    assert "Files in conflict:" in output, (
        f"Revert output should contain 'Files in conflict:' header.\nGot:\n{output}"
    )
    assert "file_mine.txt" in output, (
        f"Revert output should list file_mine.txt as conflicted.\nGot:\n{output}"
    )
    assert "file_theirs.txt" in output, (
        f"Revert output should list file_theirs.txt as conflicted.\nGot:\n{output}"
    )

    # Resolve file_mine with 'mine' (keep C), file_theirs with 'theirs' (accept A)
    repo.revision_revert_resolve_mine("file_mine.txt")
    repo.revision_revert_resolve_theirs("file_theirs.txt")
    repo.commit("Resolved revert conflicts")

    with repo.open_file("file_mine.txt", "r") as f:
        assert f.read() == content_c, "file_mine should have content C (mine)"

    with repo.open_file("file_theirs.txt", "r") as f:
        assert f.read() == content_a, "file_theirs should have content A (theirs)"


@pytest.mark.smoke
def test_revert_abort(new_lore_repo):
    """
    Test aborting an in-progress revert restores the workspace to its original state.
    """
    repo: Lore = new_lore_repo()
    test_file = "test.txt"

    # Create initial file
    initial_content = "Initial content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(initial_content)
    repo.stage(test_file)
    repo.commit("Initial commit")
    repo.push()

    # Modify the file
    modified_content = "Modified content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(modified_content)
    repo.stage(test_file)
    repo.commit("Modify file")
    repo.push()

    # Get the revision to revert
    revisions = repo.history()
    revision_to_revert = revisions[-1].signature

    # Modify again to create a conflict scenario
    final_content = "Final content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(final_content)
    repo.stage(test_file)
    repo.commit("Final modification")
    repo.push()

    # Start revert (will conflict)
    repo.revision_revert(
        revision=revision_to_revert,
        message="Revert that will be aborted",
    )

    # Abort the revert
    repo.revision_revert_abort()

    # Verify the workspace is restored to its state before revert
    with repo.open_file(test_file, "r") as f:
        contents = f.read()
    assert contents == final_content, (
        f"File should be restored to state before revert. Got:\n{contents}"
    )

    # Verify we can do another revert (no revert state remains)
    repo.revision_revert(
        revision=revision_to_revert,
        message="Revert after abort",
    )
    repo.revision_revert_resolve_theirs(test_file)
    repo.commit("Revert completed after previous abort")

    # Verify the file has the reverted content (theirs = parent of reverted commit = initial)
    with repo.open_file(test_file, "r") as f:
        contents = f.read()
    assert contents == initial_content, (
        f"File should have initial content after revert with theirs. Got:\n{contents}"
    )


@pytest.mark.smoke
def test_revert_restart(new_lore_repo):
    """
    Test restarting a revert for specific files to reset their conflict state.
    """
    repo: Lore = new_lore_repo()
    test_file = "restart.txt"

    # Create initial file
    initial_content = "Initial content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(initial_content)
    repo.stage(test_file)
    repo.commit("Initial commit")
    repo.push()

    # Modify the file
    modified_content = "Modified content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(modified_content)
    repo.stage(test_file)
    repo.commit("Modify file")
    repo.push()

    # Get the revision to revert
    revisions = repo.history()
    revision_to_revert = revisions[-1].signature

    # Modify again to create a conflict
    final_content = "Final content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(final_content)
    repo.stage(test_file)
    repo.commit("Final modification")
    repo.push()

    # Start revert (will conflict)
    repo.revision_revert(
        revision=revision_to_revert,
        message="Revert for restart test",
    )

    # Save the conflict state contents before any edits
    with repo.open_file(test_file, "r") as f:
        conflict_state_content = f.read()

    # Make manual edits to the conflicted file
    edited_content = "Manually edited content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(edited_content)

    # Verify the edit is in place
    with repo.open_file(test_file, "r") as f:
        contents = f.read()
    assert contents == edited_content, "Edit should be in place before restart"

    # Restart the revert for this file
    repo.revision_revert_restart(test_file)

    # Verify the file is reset to the conflict state contents
    with repo.open_file(test_file, "r") as f:
        contents_after_restart = f.read()
    assert contents_after_restart == conflict_state_content, (
        f"File should be reset to conflict state after restart.\n"
        f"Expected:\n{conflict_state_content}\nGot:\n{contents_after_restart}"
    )

    # Resolve and commit to complete the test
    repo.revision_revert_resolve_theirs(test_file)
    repo.commit("Revert after restart")


@pytest.mark.smoke
def test_revert_unresolve(new_lore_repo):
    """
    Test marking a resolved conflict back as unresolved using the unresolve command.
    """
    repo: Lore = new_lore_repo()
    test_file = "unresolve.txt"

    # Create initial file
    initial_content = "Initial content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(initial_content)
    repo.stage(test_file)
    repo.commit("Initial commit")
    repo.push()

    # Modify the file
    modified_content = "Modified content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(modified_content)
    repo.stage(test_file)
    repo.commit("Modify file")
    repo.push()

    # Get the revision to revert
    revisions = repo.history()
    revision_to_revert = revisions[-1].signature

    # Modify again to create a conflict
    final_content = "Final content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(final_content)
    repo.stage(test_file)
    repo.commit("Final modification")
    repo.push()

    # Start revert (will conflict)
    repo.revision_revert(
        revision=revision_to_revert,
        message="Revert for unresolve test",
    )

    # Resolve the conflict with theirs
    repo.revision_revert_resolve_theirs(test_file)

    # Re-mark as unresolved
    output = repo.revision_revert_unresolve(test_file)
    assert "Marked unresolved" in output, (
        f"Should report marked unresolved. Got:\n{output}"
    )

    # Verify status shows file as conflicted (indicated by "!")
    status = repo.status()
    assert f"{test_file} (M)!" in status, (
        f"Status should show conflict marker. Got:\n{status}"
    )

    # Resolve again and commit
    repo.revision_revert_resolve_theirs(test_file)
    repo.commit("Revert after re-marking unresolved")


@pytest.mark.smoke
def test_revert_no_commit(new_lore_repo):
    """
    Test the --no-commit flag stages changes without committing.
    """
    repo: Lore = new_lore_repo()
    test_file = "no_commit.txt"

    # Create initial file
    initial_content = "Initial content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(initial_content)
    repo.stage(test_file)
    repo.commit("Initial commit")
    repo.push()

    # Modify the file
    modified_content = "Modified content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(modified_content)
    repo.stage(test_file)
    repo.commit("Modify file")
    repo.push()

    # Get the revision to revert
    revisions = repo.history()
    revision_to_revert = revisions[-1].signature
    revision_count_before = len(revisions)

    # Revert with --no-commit
    repo.revision_revert(
        revision=revision_to_revert,
        message="Revert with no-commit",
        no_commit=True,
    )

    # Verify the file is reverted (changes are staged)
    with repo.open_file(test_file, "r") as f:
        contents = f.read()
    assert contents == initial_content, (
        f"File should be reverted to initial content. Got:\n{contents}"
    )

    # Verify no new commit was made
    revisions_after = repo.history()
    revision_count_after = len(revisions_after)
    assert revision_count_after == revision_count_before, (
        f"No new commit should be made with --no-commit. "
        f"Before: {revision_count_before}, After: {revision_count_after}"
    )

    # Manually commit
    repo.commit("Manual commit after revert with --no-commit")

    # Verify commit was made
    revisions_final = repo.history()
    assert len(revisions_final) == revision_count_before + 1, (
        "Manual commit should increase revision count"
    )

    # --- Second section: no_commit with conflict ---
    conflict_file = "no_commit_conflict.txt"

    # Create initial file
    initial_conflict = "Initial conflict content\n"
    with repo.open_file(conflict_file, "w") as f:
        f.write(initial_conflict)
    repo.stage(conflict_file)
    repo.commit("Initial conflict file")
    repo.push()

    # Modify the file (this commit will be reverted)
    modified_conflict = "Modified conflict content\n"
    with repo.open_file(conflict_file, "w") as f:
        f.write(modified_conflict)
    repo.stage(conflict_file)
    repo.commit("Modify conflict file")
    repo.push()

    # Get revision to revert
    conflict_revisions = repo.history()
    conflict_revision_to_revert = conflict_revisions[-1].signature
    conflict_revision_count_before = len(conflict_revisions)

    # Modify again to create a conflict scenario
    final_conflict = "Final conflict content\n"
    with repo.open_file(conflict_file, "w") as f:
        f.write(final_conflict)
    repo.stage(conflict_file)
    repo.commit("Final conflict file modification")
    repo.push()
    conflict_revision_count_before = len(repo.history())

    # Revert with --no-commit (will conflict)
    repo.revision_revert(
        revision=conflict_revision_to_revert,
        message="Revert with no-commit and conflict",
        no_commit=True,
    )

    # Resolve the conflict
    repo.revision_revert_resolve_theirs(conflict_file)

    # Verify no new commit was made after resolving
    conflict_revisions_after = repo.history()
    assert len(conflict_revisions_after) == conflict_revision_count_before, (
        f"No new commit should be made with --no-commit even after conflict resolution. "
        f"Before: {conflict_revision_count_before}, After: {len(conflict_revisions_after)}"
    )

    # Manually commit
    repo.commit("Manual commit after no-commit revert with conflict")

    # Verify commit was made
    conflict_revisions_final = repo.history()
    assert len(conflict_revisions_final) == conflict_revision_count_before + 1, (
        "Manual commit should increase revision count after conflict resolution"
    )


@pytest.mark.smoke
def test_revert_metadata(new_lore_repo):
    """
    Test that REVERTED_FROM metadata is set correctly on revert commits.
    """
    repo: Lore = new_lore_repo()
    test_file = "metadata.txt"

    # Create initial file
    with repo.open_file(test_file, "w") as f:
        f.write("Initial content\n")
    repo.stage(test_file)
    repo.commit("Initial commit")
    repo.push()

    # Modify the file
    with repo.open_file(test_file, "w") as f:
        f.write("Modified content\n")
    repo.stage(test_file)
    repo.commit("Modify file")
    repo.push()

    # Get the revision to revert (most recent commit, list is oldest-first after reverse)
    revisions = repo.history()
    revision_to_revert = revisions[
        -1
    ].signature  # Newest revision (the modification commit)

    # Revert the modification commit
    repo.revision_revert(
        revision=revision_to_revert,
        message="Revert file modification",
    )
    repo.push()

    # Get the revert commit's metadata (now the newest commit)
    revert_revisions = repo.history()
    revert_revision = revert_revisions[
        -1
    ].signature  # Newest revision (the revert commit)

    # Check reverted-from metadata
    metadata = repo.revision_metadata_get("reverted-from", revision=revert_revision)
    assert revision_to_revert in metadata, (
        f"reverted-from should contain the reverted revision signature.\n"
        f"Expected: {revision_to_revert}\nGot: {metadata}"
    )


@pytest.mark.smoke
def test_revert_of_revert(new_lore_repo):
    """
    Test reverting a revert commit (double revert should restore original change).
    """
    repo: Lore = new_lore_repo()
    test_file = "double_revert.txt"

    # Create initial file
    initial_content = "Initial content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(initial_content)
    repo.stage(test_file)
    repo.commit("Initial commit")
    repo.push()

    # Modify the file
    modified_content = "Modified content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(modified_content)
    repo.stage(test_file)
    repo.commit("Modify file")
    repo.push()

    # Get the revision to revert
    revisions = repo.history()
    revision_to_revert = revisions[-1].signature

    # First revert: should restore to initial content
    repo.revision_revert(
        revision=revision_to_revert,
        message="First revert - restore to initial",
    )
    repo.push()

    # Verify file is back to initial
    with repo.open_file(test_file, "r") as f:
        contents = f.read()
    assert contents == initial_content, (
        f"File should be reverted to initial content. Got:\n{contents}"
    )

    # Get the revert commit's revision
    revert_revisions = repo.history()
    first_revert_revision = revert_revisions[-1].signature

    # Second revert (revert the revert): should restore to modified content
    repo.revision_revert(
        revision=first_revert_revision,
        message="Second revert - restore to modified",
    )
    repo.push()

    # Verify file is back to modified content
    with repo.open_file(test_file, "r") as f:
        contents = f.read()
    assert contents == modified_content, (
        f"File should be restored to modified content after reverting the revert. Got:\n{contents}"
    )


@pytest.mark.smoke
def test_revert_merge_commit(new_lore_repo):
    """
    Test reverting a merge commit.
    """
    repo: Lore = new_lore_repo()
    main_file = "main.txt"
    feature_file = "feature.txt"

    # Create initial commit on main
    with repo.open_file(main_file, "w") as f:
        f.write("Main branch content\n")
    repo.stage(main_file)
    repo.commit("Initial commit on main")
    repo.push()

    # Create feature branch and add a file
    repo.branch_create("feature")
    with repo.open_file(feature_file, "w") as f:
        f.write("Feature branch content\n")
    repo.stage(feature_file)
    repo.commit("Add feature file")
    repo.push()

    # Switch back to main and make a change
    repo.branch_switch("main")
    with repo.open_file(main_file, "a") as f:
        f.write("Additional main content\n")
    repo.stage(main_file)
    repo.commit("Update main file")
    repo.push()

    # Merge feature into main
    repo.branch_merge("feature")
    repo.push()

    # Verify feature file exists after merge
    assert repo.file_exists(feature_file), "Feature file should exist after merge"

    # Get the merge commit's revision
    revisions = repo.history()
    merge_revision = revisions[-1].signature

    # Verify this is a merge commit
    merge_info = repo.revision_info(revision=merge_revision)
    assert merge_info.merge, "Should be a merge commit"

    # Revert the merge commit
    repo.revision_revert(
        revision=merge_revision,
        message="Revert merge commit",
    )
    repo.push()

    # Verify feature file is removed (merge changes are undone)
    assert not repo.file_exists(feature_file), (
        "Feature file should be removed after reverting merge"
    )

    # Verify main file still has the additional content (not part of the merge diff)
    with repo.open_file(main_file, "r") as f:
        contents = f.read()
    assert "Additional main content" in contents, (
        "Main file should still have its content"
    )


@pytest.mark.smoke
def test_revert_abort_when_none_in_progress(new_lore_repo):
    """
    Test that abort fails gracefully when no revert is in progress.
    """
    repo: Lore = new_lore_repo()

    # Create initial commit
    with repo.open_file("file.txt", "w") as f:
        f.write("Initial content\n")
    repo.stage("file.txt")
    repo.commit("Initial commit")
    repo.push()

    # Attempt to abort when no revert is active
    with pytest.raises(NotInProgress):
        repo.revision_revert_abort()


@pytest.mark.smoke
def test_revert_resolve_manual(new_lore_repo):
    """
    Test the generic resolve command after manually editing a conflicted file.
    """
    repo: Lore = new_lore_repo()
    test_file = "manual_resolve.txt"

    # Create initial file
    with repo.open_file(test_file, "w") as f:
        f.write("Initial content\n")
    repo.stage(test_file)
    repo.commit("Initial commit")
    repo.push()

    # Modify the file
    with repo.open_file(test_file, "w") as f:
        f.write("Modified content\n")
    repo.stage(test_file)
    repo.commit("Modify file")
    repo.push()

    # Get the revision to revert
    revisions = repo.history()
    revision_to_revert = revisions[-1].signature

    # Modify again to create conflict
    with repo.open_file(test_file, "w") as f:
        f.write("Final content\n")
    repo.stage(test_file)
    repo.commit("Final modification")
    repo.push()

    # Start revert (will conflict)
    repo.revision_revert(
        revision=revision_to_revert,
        message="Revert for manual resolve",
    )

    # Manually edit the file to resolve the conflict
    manual_resolution = "Manually resolved content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(manual_resolution)

    # Mark as resolved using generic resolve command
    repo.revision_revert_resolve(test_file)
    repo.commit("Revert with manual resolution")

    # Verify the manual resolution is preserved
    with repo.open_file(test_file, "r") as f:
        contents = f.read()
    assert contents == manual_resolution, (
        f"File should contain manual resolution. Got:\n{contents}"
    )


@pytest.mark.smoke
def test_revert_binary_conflict(new_lore_repo):
    """
    Test reverting with binary file conflicts, resolving with both 'mine' and 'theirs'.
    """
    repo: Lore = new_lore_repo()
    binary_file = "binary.bin"

    # Create initial binary file on main
    initial_binary_data = os.urandom(1024)
    with repo.open_file(binary_file, "wb") as f:
        f.write(initial_binary_data)
    repo.stage(binary_file)
    repo.commit("Initial binary file")
    repo.push()

    # Modify binary file (this commit will be reverted)
    modified_binary_data = os.urandom(1024)
    with repo.open_file(binary_file, "wb") as f:
        f.write(modified_binary_data)
    repo.stage(binary_file)
    repo.commit("Modify binary file")
    repo.push()

    # Get revision to revert
    revisions = repo.history()
    revision_to_revert = revisions[-1].signature

    # Modify binary file again differently to create conflict
    latest_binary_data = os.urandom(1024)
    with repo.open_file(binary_file, "wb") as f:
        f.write(latest_binary_data)
    repo.stage(binary_file)
    repo.commit("Further modify binary file")
    repo.push()

    # Revert the middle commit (will conflict with latest binary content)
    repo.revision_revert(
        revision=revision_to_revert,
        message="Revert binary modification",
    )

    # Resolve with 'mine' — keep the latest version
    repo.revision_revert_resolve_mine(binary_file)
    repo.commit("Revert binary resolved with mine")

    with repo.open_file(binary_file, "rb") as f:
        contents = f.read()
    assert contents == latest_binary_data, (
        "Binary file should contain 'mine' (latest) content after resolve mine"
    )

    # --- Now test 'theirs' path ---
    repo2: Lore = new_lore_repo()
    binary_file2 = "binary2.bin"

    # Create initial binary file
    initial_binary_data2 = os.urandom(1024)
    with repo2.open_file(binary_file2, "wb") as f:
        f.write(initial_binary_data2)
    repo2.stage(binary_file2)
    repo2.commit("Initial binary file")
    repo2.push()

    # Modify binary file (this commit will be reverted)
    modified_binary_data2 = os.urandom(1024)
    with repo2.open_file(binary_file2, "wb") as f:
        f.write(modified_binary_data2)
    repo2.stage(binary_file2)
    repo2.commit("Modify binary file")
    repo2.push()

    # Get revision to revert
    revisions2 = repo2.history()
    revision_to_revert2 = revisions2[-1].signature

    # Modify binary file again to create conflict
    latest_binary_data2 = os.urandom(1024)
    with repo2.open_file(binary_file2, "wb") as f:
        f.write(latest_binary_data2)
    repo2.stage(binary_file2)
    repo2.commit("Further modify binary file")
    repo2.push()

    # Revert the middle commit (will conflict)
    repo2.revision_revert(
        revision=revision_to_revert2,
        message="Revert binary modification",
    )

    # Resolve with 'theirs' — accept the pre-modification version
    repo2.revision_revert_resolve_theirs(binary_file2)
    repo2.commit("Revert binary resolved with theirs")

    with repo2.open_file(binary_file2, "rb") as f:
        contents2 = f.read()
    assert contents2 == initial_binary_data2, (
        "Binary file should contain 'theirs' (initial) content after resolve theirs"
    )


@pytest.mark.smoke
def test_revert_then_merge(new_lore_repo):
    """
    Test that reverting a commit on main and then merging a feature branch works correctly.

    Steps:
    1. Create file on main, commit, push
    2. Create feature branch, modify file on feature, commit, push
    3. Switch to main, modify file differently on main, commit, push
    4. Revert the main modification (restores to initial content)
    5. Merge feature branch into main
    6. Verify the feature branch changes are present after merge
    """
    repo: Lore = new_lore_repo()
    test_file = "revert_merge.txt"

    # Create initial file on main
    initial_content = "Initial content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(initial_content)
    repo.stage(test_file)
    repo.commit("Initial commit")
    repo.push()

    # Create feature branch and modify the file
    repo.branch_create("feature")
    feature_content = "Feature branch content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(feature_content)
    repo.stage(test_file)
    repo.commit("Modify file on feature branch")
    repo.push()

    # Switch to main and modify the file differently
    repo.branch_switch("main")
    main_modified_content = "Main branch modified content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(main_modified_content)
    repo.stage(test_file)
    repo.commit("Modify file on main")
    repo.push()

    # Get the main modification revision to revert
    revisions = repo.history()
    revision_to_revert = revisions[-1].signature

    # Revert the main modification (restores to initial content)
    repo.revision_revert(
        revision=revision_to_revert,
        message="Revert main modification",
    )
    repo.push()

    # Verify file is back to initial content
    with repo.open_file(test_file, "r") as f:
        contents = f.read()
    assert contents == initial_content, (
        f"File should be reverted to initial content before merge. Got:\n{contents}"
    )

    # Merge feature branch into main
    repo.branch_merge("feature")
    repo.push()

    # Verify the feature branch changes are present after merge
    with repo.open_file(test_file, "r") as f:
        final_contents = f.read()
    assert final_contents == feature_content, (
        f"File should have feature branch content after merge. Got:\n{final_contents}"
    )


@pytest.mark.smoke
def test_revert_oldest_commit(new_lore_repo):
    """
    Test reverting the very first commit in history (edge case).

    Steps:
    1. Create initial file, commit, push (this is the only commit)
    2. Get the first (and only) revision
    3. Revert that commit
    4. Verify the file is deleted (since the commit added it)
    """
    repo: Lore = new_lore_repo()
    test_file = "oldest.txt"

    # Create initial file (this is the first and only commit)
    with repo.open_file(test_file, "w") as f:
        f.write("Content from the very first commit\n")
    repo.stage(test_file)
    repo.commit("First commit ever")
    repo.push()

    # Get the first (and only) revision
    revisions = repo.history()
    assert len(revisions) == 1, f"Should have exactly 1 revision. Got: {len(revisions)}"
    first_revision = revisions[0].signature

    # Revert the first commit (should delete the file it created)
    repo.revision_revert(
        revision=first_revision,
        message="Revert the very first commit",
    )

    # Verify the file is deleted
    assert not repo.file_exists(test_file), (
        "File should be deleted after reverting the commit that created it"
    )


@pytest.mark.smoke
def test_revert_of_a_move_of_a_locally_edited_file_refuses(new_lore_repo):
    """A revert of a revision that moved a file and changed its content moves it back, so an edit
    at the path the revision moved it to is local work the revert refuses, and keeps."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("Base", {"moved/from.txt": "base\n", "other.txt": "o\n"})
    repo.move("moved/from.txt", "moved/to.txt")
    repo.stage_move("moved/from.txt", "moved/to.txt")
    repo.write_files({"moved/to.txt": "moved\n"})
    repo.stage("moved/to.txt")
    repo.commit("Move and modify")
    repo.push()
    moved = repo.branch_info().local_latest
    repo.write_files({"moved/to.txt": "locally edited\n"})

    with pytest.raises(LocalChanges):
        repo.revision_revert(moved)

    with repo.open_file("moved/to.txt") as f:
        assert f.read() == "locally edited\n", "the refused revert should keep the edit"
    assert not repo.file_exists("moved/from.txt")
