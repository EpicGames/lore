# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest

from error_types import LocalChanges, NotInProgress
from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_cherry_pick_from_feature_branch(new_lore_repo):
    """
    Test cherry-picking commits from a feature branch:
    1. Create a branch off main and cherry-pick the last feature commit onto it
       (this produces a conflict since the file doesn't exist, resolve with theirs)
    2. Switch back to main and cherry-pick the second to last feature commit
       (also produces a conflict, resolve with theirs)
    3. Cherry-pick the last feature commit onto main (clean, file now exists)
    """
    repo: Lore = new_lore_repo()
    shared_file = "shared_file.txt"

    # Main branch: initial commit
    with repo.open_file("main_file.txt", "w") as f:
        f.write("Initial content for main file\n")
    repo.stage("main_file.txt")
    repo.commit("Initial commit")
    repo.push()

    # Feature branch: creates and modifies shared_file.txt
    repo.branch_create("feature")

    with repo.open_file(shared_file, "w") as f:
        f.write("Feature - Initial content\n")
    repo.stage(shared_file)
    repo.commit("feature - create shared_file.txt")

    with repo.open_file(shared_file, "a") as f:
        f.write("Feature - Added line 2\n")
    repo.stage(shared_file)
    repo.commit("feature - update shared_file.txt with line 2")

    with repo.open_file(shared_file, "a") as f:
        f.write("Feature - Added line 3\n")
    repo.stage(shared_file)
    repo.commit("feature - update shared_file.txt with line 3")
    repo.push()

    # Get feature revisions
    feature_revisions = repo.history(branch="feature")
    last_feature_revision = feature_revisions[-1].signature
    second_to_last_feature_revision = feature_revisions[-2].signature

    expected_last_contents = (
        "Feature - Initial content\nFeature - Added line 2\nFeature - Added line 3\n"
    )
    expected_second_to_last_contents = (
        "Feature - Initial content\nFeature - Added line 2\n"
    )

    # Step 1: Branch off main and cherry-pick the last feature commit onto the branch
    # This will conflict since shared_file.txt doesn't exist on the branch
    repo.branch_switch("main")
    repo.branch_create("cherry-pick-branch")

    repo.revision_cherry_pick(
        revision=last_feature_revision,
        message="Cherry-picked last commit from feature onto branch",
    )

    # Resolve the conflict by taking theirs (the source branch version)
    repo.revision_cherry_pick_resolve_theirs(shared_file)
    repo.commit("Cherry-picked last commit from feature onto branch")

    assert repo.file_exists(shared_file), (
        f"{shared_file} should exist after cherry-pick on branch"
    )

    with repo.open_file(shared_file, "r") as f:
        branch_contents = f.read()

    assert branch_contents == expected_last_contents, (
        f"File contents should match last feature commit. Got:\n{branch_contents}"
    )

    # Step 2: Switch back to main and cherry-pick the second to last revision
    # This will also conflict since shared_file.txt doesn't exist on main
    repo.branch_switch("main")

    repo.revision_cherry_pick(
        revision=second_to_last_feature_revision,
        message="Cherry-picked second to last commit from feature onto main",
    )

    # Resolve the conflict by taking theirs
    repo.revision_cherry_pick_resolve_theirs(shared_file)
    repo.commit("Cherry-picked second to last commit from feature onto main")

    assert repo.file_exists(shared_file), (
        f"{shared_file} should exist after cherry-pick on main"
    )

    with repo.open_file(shared_file, "r") as f:
        main_contents = f.read()

    assert main_contents == expected_second_to_last_contents, (
        f"File contents should match second to last feature commit. Got:\n{main_contents}"
    )

    # Step 3: Cherry-pick the last feature commit onto main
    # This should apply cleanly since the file now exists with lines 1-2
    repo.revision_cherry_pick(
        revision=last_feature_revision,
        message="Cherry-picked last commit from feature onto main",
    )

    with repo.open_file(shared_file, "r") as f:
        final_contents = f.read()

    assert final_contents == expected_last_contents, (
        f"File contents should match the last feature commit. Got:\n{final_contents}"
    )


@pytest.mark.smoke
def test_cherry_pick_delete_file(new_lore_repo):
    """
    Test cherry-picking a commit that deletes a file.
    """
    repo: Lore = new_lore_repo()
    test_file = "to_delete.txt"

    # Create initial file on main
    with repo.open_file(test_file, "w") as f:
        f.write("This file will be deleted\n")
    repo.stage(test_file)
    repo.commit("Create file to be deleted")
    repo.push()

    # Create feature branch and delete the file
    repo.branch_create("feature-delete")
    repo.remove_file(test_file)
    repo.stage(test_file)
    repo.commit("Delete the file")
    repo.push()

    # Get the delete commit revision
    feature_revisions = repo.history(branch="feature-delete")
    delete_revision = feature_revisions[-1].signature

    # Switch to main and verify file exists
    repo.branch_switch("main")
    assert repo.file_exists(test_file), "File should exist on main before cherry-pick"

    # Cherry-pick the delete commit onto main
    repo.revision_cherry_pick(
        revision=delete_revision,
        message="Cherry-picked file deletion",
    )

    assert not repo.file_exists(test_file), (
        "File should not exist after cherry-picking delete commit"
    )


@pytest.mark.smoke
def test_cherry_pick_multiple_files(new_lore_repo):
    """
    Test cherry-picking a commit that modifies multiple files.
    """
    repo: Lore = new_lore_repo()
    file_a = "file_a.txt"
    file_b = "file_b.txt"
    file_c = "file_c.txt"

    # Create initial files on main
    with repo.open_file(file_a, "w") as f:
        f.write("File A - initial\n")
    with repo.open_file(file_b, "w") as f:
        f.write("File B - initial\n")
    repo.stage(file_a)
    repo.stage(file_b)
    repo.commit("Create initial files")
    repo.push()

    # Create feature branch and modify multiple files in one commit
    repo.branch_create("feature-multi")

    with repo.open_file(file_a, "a") as f:
        f.write("File A - feature addition\n")
    with repo.open_file(file_b, "a") as f:
        f.write("File B - feature addition\n")
    with repo.open_file(file_c, "w") as f:
        f.write("File C - new file\n")

    repo.stage(file_a)
    repo.stage(file_b)
    repo.stage(file_c)
    repo.commit("Modify multiple files")
    repo.push()

    # Get the multi-file commit revision
    feature_revisions = repo.history(branch="feature-multi")
    multi_file_revision = feature_revisions[-1].signature

    # Switch to main
    repo.branch_switch("main")

    # Cherry-pick the multi-file commit
    repo.revision_cherry_pick(
        revision=multi_file_revision,
        message="Cherry-picked multi-file changes",
    )

    # Verify all files have expected content
    with repo.open_file(file_a, "r") as f:
        assert f.read() == "File A - initial\nFile A - feature addition\n"

    with repo.open_file(file_b, "r") as f:
        assert f.read() == "File B - initial\nFile B - feature addition\n"

    assert repo.file_exists(file_c), "New file should exist after cherry-pick"
    with repo.open_file(file_c, "r") as f:
        assert f.read() == "File C - new file\n"


@pytest.mark.smoke
def test_cherry_pick_subdirectory(new_lore_repo):
    """
    Test cherry-picking a commit that creates a file in a subdirectory.
    """
    repo: Lore = new_lore_repo()
    subdir = "src/utils"
    subdir_file = f"{subdir}/helper.txt"

    # Create initial commit on main
    with repo.open_file("README.txt", "w") as f:
        f.write("Project readme\n")
    repo.stage("README.txt")
    repo.commit("Initial commit")
    repo.push()

    # Create feature branch with subdirectory file
    repo.branch_create("feature-subdir")

    repo.make_dirs(subdir)
    with repo.open_file(subdir_file, "w") as f:
        f.write("Helper utility content\n")
    repo.stage(subdir_file)
    repo.commit("Add helper in subdirectory")
    repo.push()

    # Get the commit that creates the subdirectory file
    feature_revisions = repo.history(branch="feature-subdir")
    create_revision = feature_revisions[-1].signature

    # Switch to main
    repo.branch_switch("main")
    assert not repo.file_exists(subdir_file), (
        "Subdirectory file should not exist on main before cherry-pick"
    )

    # Cherry-pick the commit that creates the file in a subdirectory
    repo.revision_cherry_pick(
        revision=create_revision,
        message="Cherry-picked subdirectory file creation",
    )

    assert repo.file_exists(subdir_file), (
        "Subdirectory file should exist after cherry-pick"
    )

    with repo.open_file(subdir_file, "r") as f:
        contents = f.read()

    expected_contents = "Helper utility content\n"
    assert contents == expected_contents, (
        f"Subdirectory file contents should match. Got:\n{contents}"
    )


@pytest.mark.smoke
def test_cherry_pick_from_nested_branch(new_lore_repo):
    """
    Test cherry-picking from a branch that was created off another branch
    (branch-of-a-branch) to verify ancestry depth doesn't matter.
    """
    repo: Lore = new_lore_repo()
    test_file = "nested.txt"

    # Create initial commit on main
    with repo.open_file("main.txt", "w") as f:
        f.write("Main branch content\n")
    repo.stage("main.txt")
    repo.commit("Initial commit on main")
    repo.push()

    # Create first-level branch
    repo.branch_create("level-1")
    with repo.open_file("level1.txt", "w") as f:
        f.write("Level 1 branch content\n")
    repo.stage("level1.txt")
    repo.commit("Commit on level-1 branch")
    repo.push()

    # Create second-level branch (branch off level-1)
    repo.branch_create("level-2")
    with repo.open_file("level2.txt", "w") as f:
        f.write("Level 2 branch content\n")
    repo.stage("level2.txt")
    repo.commit("Commit on level-2 branch")

    # Create the commit we want to cherry-pick
    with repo.open_file(test_file, "w") as f:
        f.write("Content from deeply nested branch\n")
    repo.stage(test_file)
    repo.commit("Add nested file on level-2")
    repo.push()

    # Get the last commit revision from level-2
    level2_revisions = repo.history(branch="level-2")
    nested_revision = level2_revisions[-1].signature

    # Switch back to main
    repo.branch_switch("main")
    assert not repo.file_exists(test_file), (
        "Nested file should not exist on main before cherry-pick"
    )
    assert not repo.file_exists("level1.txt"), "Level 1 file should not exist on main"
    assert not repo.file_exists("level2.txt"), "Level 2 file should not exist on main"

    # Cherry-pick from the deeply nested branch onto main
    repo.revision_cherry_pick(
        revision=nested_revision,
        message="Cherry-picked from nested branch",
    )

    # Verify only the cherry-picked file exists, not the intermediate branch files
    assert repo.file_exists(test_file), "Nested file should exist after cherry-pick"
    assert not repo.file_exists("level1.txt"), (
        "Level 1 file should not be brought over by cherry-pick"
    )
    assert not repo.file_exists("level2.txt"), (
        "Level 2 file should not be brought over by cherry-pick"
    )

    with repo.open_file(test_file, "r") as f:
        contents = f.read()

    assert contents == "Content from deeply nested branch\n", (
        f"File contents should match. Got:\n{contents}"
    )


@pytest.mark.smoke
def test_cherry_pick_abort(new_lore_repo):
    """
    Test aborting a cherry-pick with conflicts restores the workspace to its original state.
    """
    repo: Lore = new_lore_repo()
    shared_file = "shared_file.txt"

    # Create initial commit on main
    with repo.open_file("main.txt", "w") as f:
        f.write("Main branch content\n")
    repo.stage("main.txt")
    repo.commit("Initial commit")
    repo.push()

    # Create feature branch with a new file and then modify it
    # (cherry-picking the modification will conflict since the file doesn't exist on main)
    repo.branch_create("feature")
    with repo.open_file(shared_file, "w") as f:
        f.write("Feature content\n")
    repo.stage(shared_file)
    repo.commit("Create shared file on feature")

    with repo.open_file(shared_file, "a") as f:
        f.write("Added line 2\n")
    repo.stage(shared_file)
    repo.commit("Modify shared file on feature")
    repo.push()

    # Get the last revision (the modification commit)
    feature_revisions = repo.history(branch="feature")
    feature_revision = feature_revisions[-1].signature

    # Switch to main where shared_file doesn't exist
    repo.branch_switch("main")

    # Verify the file doesn't exist before cherry-pick
    assert not repo.file_exists(shared_file), (
        f"{shared_file} should not exist on main before cherry-pick"
    )

    # Start cherry-pick (will produce conflict since file doesn't exist)
    repo.revision_cherry_pick(
        revision=feature_revision,
        message="Cherry-pick that will be aborted",
    )

    # Abort the cherry-pick
    repo.revision_cherry_pick_abort()

    # Verify the workspace is restored - file should not exist
    assert not repo.file_exists(shared_file), (
        f"{shared_file} should not exist after cherry-pick abort"
    )

    # Verify we can do another cherry-pick (no staged state remains)
    repo.revision_cherry_pick(
        revision=feature_revision,
        message="Cherry-pick after abort",
    )
    repo.revision_cherry_pick_resolve_theirs(shared_file)
    repo.commit("Cherry-pick completed after previous abort")

    assert repo.file_exists(shared_file), (
        f"{shared_file} should exist after successful cherry-pick"
    )


@pytest.mark.smoke
def test_cherry_pick_resolve_mine(new_lore_repo):
    """
    Test resolving conflicts using the current branch version ("mine").
    """
    repo: Lore = new_lore_repo()
    test_file = "conflict.txt"

    # Create initial file on main
    main_content = "Main branch content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(main_content)
    repo.stage(test_file)
    repo.commit("Initial commit with file")
    repo.push()

    # Create feature branch and modify the file
    repo.branch_create("feature-mine")
    feature_content = "Feature branch content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(feature_content)
    repo.stage(test_file)
    repo.commit("Modify file on feature branch")
    repo.push()

    # Get the feature revision
    feature_revisions = repo.history(branch="feature-mine")
    feature_revision = feature_revisions[-1].signature

    # Switch to main and modify the file differently
    repo.branch_switch("main")
    modified_main_content = "Modified main branch content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(modified_main_content)
    repo.stage(test_file)
    repo.commit("Modify file on main branch")
    repo.push()

    # Cherry-pick the feature commit (will conflict)
    repo.revision_cherry_pick(
        revision=feature_revision,
        message="Cherry-pick with conflict",
    )

    # Resolve using "mine" (the current branch version)
    repo.revision_cherry_pick_resolve_mine(test_file)
    repo.commit("Cherry-pick resolved with mine")

    # Verify the file contains "mine" (main branch) content
    with repo.open_file(test_file, "r") as f:
        contents = f.read()

    assert contents == modified_main_content, (
        f"File should contain 'mine' (main branch) content. Got:\n{contents}"
    )


@pytest.mark.smoke
def test_cherry_pick_resolve_manual(new_lore_repo):
    """
    Test the generic resolve command after manually editing a conflicted file.
    """
    repo: Lore = new_lore_repo()
    test_file = "manual_resolve.txt"

    # Create initial file on main
    with repo.open_file(test_file, "w") as f:
        f.write("Initial content\n")
    repo.stage(test_file)
    repo.commit("Initial commit")
    repo.push()

    # Create feature branch and modify the file
    repo.branch_create("feature-manual")
    with repo.open_file(test_file, "w") as f:
        f.write("Feature content\n")
    repo.stage(test_file)
    repo.commit("Modify on feature branch")
    repo.push()

    # Get the feature revision
    feature_revisions = repo.history(branch="feature-manual")
    feature_revision = feature_revisions[-1].signature

    # Switch to main and modify the file differently
    repo.branch_switch("main")
    with repo.open_file(test_file, "w") as f:
        f.write("Main content\n")
    repo.stage(test_file)
    repo.commit("Modify on main branch")
    repo.push()

    # Cherry-pick (will conflict)
    repo.revision_cherry_pick(
        revision=feature_revision,
        message="Cherry-pick for manual resolve",
    )

    # Manually edit the file to resolve the conflict
    manual_resolution = "Manually resolved content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(manual_resolution)

    # Mark as resolved using generic resolve command
    repo.revision_cherry_pick_resolve(test_file)
    repo.commit("Cherry-pick with manual resolution")

    # Make sure changing branches doesn't affect the manual resolution
    repo.branch_switch("feature-manual")
    repo.branch_switch("main")

    # Verify the manual resolution is preserved
    with repo.open_file(test_file, "r") as f:
        contents = f.read()

    assert contents == manual_resolution, (
        f"File should contain manual resolution. Got:\n{contents}"
    )


@pytest.mark.smoke
def test_cherry_pick_unresolve_mark(new_lore_repo):
    """
    Test marking a resolved conflict back as unresolved using the unresolve command.
    """
    repo: Lore = new_lore_repo()
    test_file = "conflict_mark.txt"

    # Create initial file on main
    with repo.open_file(test_file, "w") as f:
        f.write("Initial content\n")
    repo.stage(test_file)
    repo.commit("Initial commit")
    repo.push()

    # Create feature branch and modify the file
    repo.branch_create("feature-conflict-mark")
    with repo.open_file(test_file, "w") as f:
        f.write("Feature content\n")
    repo.stage(test_file)
    repo.commit("Modify on feature branch")
    repo.push()

    # Get the feature revision
    feature_revisions = repo.history(branch="feature-conflict-mark")
    feature_revision = feature_revisions[-1].signature

    # Switch to main and modify the file differently
    repo.branch_switch("main")
    with repo.open_file(test_file, "w") as f:
        f.write("Main content\n")
    repo.stage(test_file)
    repo.commit("Modify on main branch")
    repo.push()

    # Cherry-pick (will conflict)
    repo.revision_cherry_pick(
        revision=feature_revision,
        message="Cherry-pick for unresolve marking",
    )

    # Resolve the conflict with theirs
    repo.revision_cherry_pick_resolve_theirs(test_file)

    # Re-mark as unresolved
    output = repo.revision_cherry_pick_unresolve(test_file)
    assert "Marked unresolved" in output, (
        f"Should report marked unresolved. Got:\n{output}"
    )

    # Verify status shows file as conflicted (indicated by "!")
    status = repo.status()
    assert f"{test_file} (M)!" in status, (
        f"Status should show conflict marker. Got:\n{status}"
    )

    # Resolve again and commit
    repo.revision_cherry_pick_resolve_theirs(test_file)
    repo.commit("Cherry-pick after re-marking unresolved")


@pytest.mark.smoke
def test_cherry_pick_restart(new_lore_repo):
    """
    Test restarting a cherry-pick for specific files to reset their conflict state.
    """
    repo: Lore = new_lore_repo()
    test_file = "restart.txt"

    # Create initial file on main
    initial_content = "Initial content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(initial_content)
    repo.stage(test_file)
    repo.commit("Initial commit")
    repo.push()

    # Create feature branch and modify the file
    repo.branch_create("feature-restart")
    feature_content = "Feature content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(feature_content)
    repo.stage(test_file)
    repo.commit("Modify on feature branch")
    repo.push()

    # Get the feature revision
    feature_revisions = repo.history(branch="feature-restart")
    feature_revision = feature_revisions[-1].signature

    # Switch to main and modify the file differently
    repo.branch_switch("main")
    main_content = "Main content\n"
    with repo.open_file(test_file, "w") as f:
        f.write(main_content)
    repo.stage(test_file)
    repo.commit("Modify on main branch")
    repo.push()

    # Cherry-pick (will conflict)
    repo.revision_cherry_pick(
        revision=feature_revision,
        message="Cherry-pick for restart test",
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

    # Restart the cherry-pick for this file
    repo.revision_cherry_pick_restart(test_file)

    # Verify the file is reset to the conflict state contents
    with repo.open_file(test_file, "r") as f:
        contents_after_restart = f.read()
    assert contents_after_restart == conflict_state_content, (
        f"File should be reset to conflict state after restart.\n"
        f"Expected:\n{conflict_state_content}\nGot:\n{contents_after_restart}"
    )

    # After restart, the file should be back to conflict state
    # Resolve and commit to complete the test
    repo.revision_cherry_pick_resolve_theirs(test_file)
    repo.commit("Cherry-pick after restart")

    # Verify the theirs content is now in place
    with repo.open_file(test_file, "r") as f:
        contents = f.read()
    assert contents == feature_content, (
        f"File should contain feature content after restart and resolve theirs. Got:\n{contents}"
    )


@pytest.mark.smoke
def test_cherry_pick_no_commit(new_lore_repo):
    """
    Test the --no-commit flag prevents auto-commit even when no conflicts arise.
    """
    repo: Lore = new_lore_repo()
    test_file = "no_commit.txt"

    # Create initial commit on main
    with repo.open_file("initial.txt", "w") as f:
        f.write("Initial file\n")
    repo.stage("initial.txt")
    repo.commit("Initial commit")
    repo.push()

    # Create feature branch and add a new file (no conflict scenario)
    repo.branch_create("feature-no-commit")
    with repo.open_file(test_file, "w") as f:
        f.write("New file from feature\n")
    repo.stage(test_file)
    repo.commit("Add new file on feature")
    repo.push()

    # Get the feature revision
    feature_revisions = repo.history(branch="feature-no-commit")
    feature_revision = feature_revisions[-1].signature

    # Switch to main
    repo.branch_switch("main")

    # Get current revision count before cherry-pick
    main_revisions_before = repo.history(branch="main")
    revision_count_before = len(main_revisions_before)

    # Cherry-pick with --no-commit (should not auto-commit even though no conflict)
    repo.revision_cherry_pick(
        revision=feature_revision,
        message="Cherry-pick with no-commit",
        no_commit=True,
    )

    # Verify the file exists (changes are staged)
    assert repo.file_exists(test_file), (
        "File should exist after cherry-pick with --no-commit"
    )

    # Verify no new commit was made
    main_revisions_after = repo.history(branch="main")
    revision_count_after = len(main_revisions_after)
    assert revision_count_after == revision_count_before, (
        f"No new commit should be made with --no-commit. "
        f"Before: {revision_count_before}, After: {revision_count_after}"
    )

    # Manually commit
    repo.commit("Manual commit after cherry-pick with --no-commit")

    # Verify commit was made
    main_revisions_final = repo.history(branch="main")
    assert len(main_revisions_final) == revision_count_before + 1, (
        "Manual commit should increase revision count"
    )


@pytest.mark.smoke
def test_cherry_pick_binary_conflict(new_lore_repo):
    """
    Test cherry-picking with binary file conflicts.
    """
    repo: Lore = new_lore_repo()
    binary_file = "binary.bin"

    # Create initial binary file on main
    main_binary_data = os.urandom(1024)
    with repo.open_file(binary_file, "wb") as f:
        f.write(main_binary_data)
    repo.stage(binary_file)
    repo.commit("Initial binary file")
    repo.push()

    # Create feature branch and modify the binary file
    repo.branch_create("feature-binary")
    feature_binary_data = os.urandom(1024)
    with repo.open_file(binary_file, "wb") as f:
        f.write(feature_binary_data)
    repo.stage(binary_file)
    repo.commit("Modify binary on feature")
    repo.push()

    # Get the feature revision
    feature_revisions = repo.history(branch="feature-binary")
    feature_revision = feature_revisions[-1].signature

    # Switch to main and modify the binary file differently
    repo.branch_switch("main")
    modified_main_binary_data = os.urandom(1024)
    with repo.open_file(binary_file, "wb") as f:
        f.write(modified_main_binary_data)
    repo.stage(binary_file)
    repo.commit("Modify binary on main")
    repo.push()

    # Cherry-pick (will conflict)
    repo.revision_cherry_pick(
        revision=feature_revision,
        message="Cherry-pick binary conflict",
    )

    # Resolve using "mine" and verify content
    repo.revision_cherry_pick_resolve_mine(binary_file)
    repo.commit("Cherry-pick binary resolved with mine")

    with repo.open_file(binary_file, "rb") as f:
        contents = f.read()
    assert contents == modified_main_binary_data, (
        "Binary file should contain 'mine' (main branch) content"
    )


@pytest.mark.smoke
def test_cherry_pick_abort_when_none_in_progress(new_lore_repo):
    """
    Test that abort fails gracefully when no cherry-pick is in progress.
    """
    repo: Lore = new_lore_repo()

    # Create initial commit
    with repo.open_file("file.txt", "w") as f:
        f.write("Initial content\n")
    repo.stage("file.txt")
    repo.commit("Initial commit")
    repo.push()

    # Attempt to abort when no cherry-pick is active
    with pytest.raises(NotInProgress):
        repo.revision_cherry_pick_abort()


@pytest.mark.smoke
def test_cherry_pick_multiple_conflicts_mixed_resolution(new_lore_repo):
    """
    Test resolving multiple conflicts with different strategies (mine and theirs).
    """
    repo: Lore = new_lore_repo()
    file_a = "file_a.txt"
    file_b = "file_b.txt"

    # Create initial files on main
    main_content_a = "Main content A\n"
    main_content_b = "Main content B\n"
    with repo.open_file(file_a, "w") as f:
        f.write(main_content_a)
    with repo.open_file(file_b, "w") as f:
        f.write(main_content_b)
    repo.stage(file_a)
    repo.stage(file_b)
    repo.commit("Initial commit with both files")
    repo.push()

    # Create feature branch and modify both files
    repo.branch_create("feature-mixed")
    feature_content_a = "Feature content A\n"
    feature_content_b = "Feature content B\n"
    with repo.open_file(file_a, "w") as f:
        f.write(feature_content_a)
    with repo.open_file(file_b, "w") as f:
        f.write(feature_content_b)
    repo.stage(file_a)
    repo.stage(file_b)
    repo.commit("Modify both files on feature")
    repo.push()

    # Get the feature revision
    feature_revisions = repo.history(branch="feature-mixed")
    feature_revision = feature_revisions[-1].signature

    # Switch to main and modify both files differently
    repo.branch_switch("main")
    modified_main_content_a = "Modified main content A\n"
    modified_main_content_b = "Modified main content B\n"
    with repo.open_file(file_a, "w") as f:
        f.write(modified_main_content_a)
    with repo.open_file(file_b, "w") as f:
        f.write(modified_main_content_b)
    repo.stage(file_a)
    repo.stage(file_b)
    repo.commit("Modify both files on main")
    repo.push()

    # Cherry-pick (will conflict on both files)
    repo.revision_cherry_pick(
        revision=feature_revision,
        message="Cherry-pick with multiple conflicts",
    )

    # Resolve file_a with "mine" and file_b with "theirs"
    repo.revision_cherry_pick_resolve_mine(file_a)
    repo.revision_cherry_pick_resolve_theirs(file_b)
    repo.commit("Cherry-pick with mixed resolution")

    # Verify file_a has "mine" content
    with repo.open_file(file_a, "r") as f:
        contents_a = f.read()
    assert contents_a == modified_main_content_a, (
        f"file_a should contain 'mine' content. Got:\n{contents_a}"
    )

    # Verify file_b has "theirs" content
    with repo.open_file(file_b, "r") as f:
        contents_b = f.read()
    assert contents_b == feature_content_b, (
        f"file_b should contain 'theirs' content. Got:\n{contents_b}"
    )


@pytest.mark.smoke
def test_cherry_pick_divergent_branches(new_lore_repo):
    """
    Test cherry-picking between two branches that have diverged from a common ancestor.

    This tests the scenario where:
    1. Main branch has initial commits
    2. Branch A is created from main and evolves independently
    3. Branch B is created from main (not from A) and evolves independently
    4. Cherry-pick a commit from branch A onto branch B

    Both branches have diverged from each other with no direct ancestry relationship
    other than the original main branch.
    """
    repo: Lore = new_lore_repo()
    common_file = "common.txt"
    branch_a_file = "branch_a_only.txt"
    branch_b_file = "branch_b_only.txt"
    divergent_file = "divergent.txt"

    # Create initial commit on main (common ancestor)
    with repo.open_file(common_file, "w") as f:
        f.write("Common ancestor content\n")
    repo.stage(common_file)
    repo.commit("Initial commit - common ancestor")
    repo.push()

    # Create branch-a from main and make independent commits
    repo.branch_create("branch-a")

    with repo.open_file(branch_a_file, "w") as f:
        f.write("Branch A exclusive file\n")
    repo.stage(branch_a_file)
    repo.commit("branch-a - add exclusive file")

    # Create the file we'll cherry-pick (on branch-a)
    with repo.open_file(divergent_file, "w") as f:
        f.write("Content created on branch-a\n")
    repo.stage(divergent_file)
    repo.commit("branch-a - create divergent file")

    # Add more commits to branch-a to increase divergence
    with repo.open_file(divergent_file, "a") as f:
        f.write("Additional line on branch-a\n")
    repo.stage(divergent_file)
    repo.commit("branch-a - update divergent file")
    repo.push()

    # Get the commit we want to cherry-pick (the one that creates divergent_file)
    branch_a_revisions = repo.history(branch="branch-a")
    # The second-to-last commit creates the divergent file
    divergent_file_create_revision = branch_a_revisions[-2].signature

    # Switch back to main and make additional commits (main diverges from branch-a)
    repo.branch_switch("main")

    with repo.open_file(common_file, "a") as f:
        f.write("Main branch continued after branch-a was created\n")
    repo.stage(common_file)
    repo.commit("main - continue development")
    repo.push()

    # Create branch-b from current main (not from branch-a)
    # This creates a divergent branch structure
    repo.branch_create("branch-b")

    with repo.open_file(branch_b_file, "w") as f:
        f.write("Branch B exclusive file\n")
    repo.stage(branch_b_file)
    repo.commit("branch-b - add exclusive file")

    # Make more commits on branch-b to increase divergence
    with repo.open_file(branch_b_file, "a") as f:
        f.write("More content on branch-b\n")
    repo.stage(branch_b_file)
    repo.commit("branch-b - update exclusive file")
    repo.push()

    # Verify the divergent state - branch-b should NOT have branch-a's files
    assert not repo.file_exists(branch_a_file), (
        "branch-b should not have branch-a's exclusive file"
    )
    assert not repo.file_exists(divergent_file), (
        "branch-b should not have the divergent file before cherry-pick"
    )
    assert repo.file_exists(branch_b_file), (
        "branch-b should have its own exclusive file"
    )

    # Cherry-pick the divergent file creation commit from branch-a onto branch-b
    repo.revision_cherry_pick(
        revision=divergent_file_create_revision,
        message="Cherry-picked divergent file from branch-a to branch-b",
    )

    # Verify the cherry-pick succeeded
    assert repo.file_exists(divergent_file), (
        "divergent file should exist after cherry-pick from divergent branch"
    )

    # Verify the content is correct (should be the state at the cherry-picked commit)
    with repo.open_file(divergent_file, "r") as f:
        contents = f.read()
    expected_content = "Content created on branch-a\n"
    assert contents == expected_content, (
        f"Divergent file should have content from cherry-picked commit. Got:\n{contents}"
    )

    # Verify branch-b still has its own files and doesn't have branch-a's other files
    assert repo.file_exists(branch_b_file), (
        "branch-b should still have its exclusive file"
    )
    assert not repo.file_exists(branch_a_file), (
        "branch-b should NOT have branch-a's exclusive file after cherry-pick"
    )


@pytest.mark.smoke
def test_cherry_pick_divergent_with_conflict(new_lore_repo):
    """
    Test cherry-picking between divergent branches where both modified the same file.

    This tests conflict resolution when:
    1. Main creates a file
    2. Branch A modifies the file
    3. Branch B (created independently from main) also modifies the file differently
    4. Cherry-pick from branch A to branch B causes a conflict
    """
    repo: Lore = new_lore_repo()
    shared_file = "shared.txt"

    # Create initial file on main
    initial_content = "Initial shared content\n"
    with repo.open_file(shared_file, "w") as f:
        f.write(initial_content)
    repo.stage(shared_file)
    repo.commit("Initial commit with shared file")
    repo.push()

    # Create branch-a and modify the shared file
    repo.branch_create("branch-a")
    branch_a_content = "Branch A modified this content\n"
    with repo.open_file(shared_file, "w") as f:
        f.write(branch_a_content)
    repo.stage(shared_file)
    repo.commit("branch-a - modify shared file")
    repo.push()

    # Get branch-a's commit
    branch_a_revisions = repo.history(branch="branch-a")
    branch_a_modify_revision = branch_a_revisions[-1].signature

    # Switch back to main
    repo.branch_switch("main")

    # Create branch-b (divergent from branch-a) and modify the same file differently
    repo.branch_create("branch-b")
    branch_b_content = "Branch B modified this content differently\n"
    with repo.open_file(shared_file, "w") as f:
        f.write(branch_b_content)
    repo.stage(shared_file)
    repo.commit("branch-b - modify shared file differently")
    repo.push()

    # Cherry-pick from branch-a to branch-b (will conflict)
    repo.revision_cherry_pick(
        revision=branch_a_modify_revision,
        message="Cherry-pick conflicting change from branch-a",
    )

    # Resolve with theirs (branch-a's version)
    repo.revision_cherry_pick_resolve_theirs(shared_file)
    repo.commit("Resolved divergent cherry-pick with theirs")

    with repo.open_file(shared_file, "r") as f:
        contents = f.read()
    assert contents == branch_a_content, (
        f"File should have branch-a content after resolve theirs. Got:\n{contents}"
    )


@pytest.mark.smoke
def test_cherry_pick_then_merge(new_lore_repo):
    """
    Test cherry-picking commits from branch_a onto branch_b, then merging branch_a into branch_b.

    This verifies that:
    1. Cherry-picked commits are correctly applied
    2. Subsequent merge of branch_a into branch_b brings in non-cherry-picked changes
    3. All content from branch_a is present in branch_b after the merge
    4. Cherry-pick only applies specific commit changes, not earlier changes to the same file
    5. Merge brings in earlier non-cherry-picked changes to files that were cherry-picked

    Setup:
    - main: Creates file_1.txt, file_2.txt, file_3.txt
    - branch_a: 5 commits:
        - Commits 1-3: Create NEW files (branch_a_1.txt, branch_a_2.txt, branch_a_3.txt)
                       AND edit file_2.txt at the TOP (non-overlapping with commits 4-5)
                       These are NOT cherry-picked and must come via merge
        - Commits 4-5: Modify existing files from main (file_2.txt at the BOTTOM, file_3.txt)
                       These ARE cherry-picked and apply cleanly
    - branch_b: Only modifies file_1 (no overlap with cherry-picked commits)
    - Cherry-pick commits 4 and 5 from branch_a onto branch_b
    - Verify file_2 has only commit 4-5 changes, NOT the commits 1-3 changes
    - Merge branch_a into branch_b
    - Verify file_2 now has BOTH the early edits (commits 1-3) AND cherry-picked edits (commits 4-5)
    - Verify the new files from commits 1-3 are present (proves merge brought in early history)
    """
    repo: Lore = new_lore_repo()

    file_1 = "file_1.txt"
    file_2 = "file_2.txt"
    file_3 = "file_3.txt"
    branch_a_file_1 = "branch_a_1.txt"
    branch_a_file_2 = "branch_a_2.txt"
    branch_a_file_3 = "branch_a_3.txt"

    # ========== Setup main branch with initial files ==========
    with repo.open_file(file_1, "w") as f:
        f.write("file_1 - initial content\n")
    with repo.open_file(file_2, "w") as f:
        f.write("file_2 - initial content\n")
    with repo.open_file(file_3, "w") as f:
        f.write("file_3 - initial content\n")
    repo.stage(file_1)
    repo.stage(file_2)
    repo.stage(file_3)
    repo.commit("Initial commit with three files")
    repo.push()

    # ========== Create branch_a and make several commits ==========
    repo.branch_create("branch_a")

    # Commits 1-3: Create NEW files AND edit file_2 at the TOP
    # (these will NOT be cherry-picked, must come via merge)
    with repo.open_file(branch_a_file_1, "w") as f:
        f.write("branch_a_1 - created in commit 1\n")
    # Edit file_2 at the top (prepend content)
    with repo.open_file(file_2, "r") as f:
        original_file_2 = f.read()
    with repo.open_file(file_2, "w") as f:
        f.write("file_2 - branch_a commit 1 (early edit at top)\n")
        f.write(original_file_2)
    repo.stage(branch_a_file_1)
    repo.stage(file_2)
    repo.commit("branch_a - create branch_a_1.txt and edit file_2 top (commit 1)")

    with repo.open_file(branch_a_file_2, "w") as f:
        f.write("branch_a_2 - created in commit 2\n")
    # Edit file_2 at the top again (prepend more content)
    with repo.open_file(file_2, "r") as f:
        current_file_2 = f.read()
    with repo.open_file(file_2, "w") as f:
        f.write("file_2 - branch_a commit 2 (early edit at top)\n")
        f.write(current_file_2)
    repo.stage(branch_a_file_2)
    repo.stage(file_2)
    repo.commit("branch_a - create branch_a_2.txt and edit file_2 top (commit 2)")

    with repo.open_file(branch_a_file_3, "w") as f:
        f.write("branch_a_3 - created in commit 3\n")
    # Edit file_2 at the top once more (prepend more content)
    with repo.open_file(file_2, "r") as f:
        current_file_2 = f.read()
    with repo.open_file(file_2, "w") as f:
        f.write("file_2 - branch_a commit 3 (early edit at top)\n")
        f.write(current_file_2)
    repo.stage(branch_a_file_3)
    repo.stage(file_2)
    repo.commit("branch_a - create branch_a_3.txt and edit file_2 top (commit 3)")

    # Commits 4-5: Modify existing files at the BOTTOM (these WILL be cherry-picked)
    with repo.open_file(file_2, "a") as f:
        f.write("file_2 - branch_a commit 4 (bottom edit)\n")
    repo.stage(file_2)
    repo.commit("branch_a - modify file_2 bottom (commit 4)")

    with repo.open_file(file_3, "a") as f:
        f.write("file_3 - branch_a commit 5 (bottom edit)\n")
    repo.stage(file_3)
    repo.commit("branch_a - modify file_3 bottom (commit 5)")
    repo.push()

    # Get branch_a revisions for cherry-picking
    branch_a_revisions = repo.history(branch="branch_a")
    # Revisions are ordered from oldest to newest
    # Index -1 is commit 5, Index -2 is commit 4
    commit_4_revision = branch_a_revisions[-2].signature
    commit_5_revision = branch_a_revisions[-1].signature

    # Record expected final content for branch_a files
    with repo.open_file(file_2, "r") as f:
        expected_file_2_content = f.read()
    with repo.open_file(file_3, "r") as f:
        expected_file_3_content = f.read()
    with repo.open_file(branch_a_file_1, "r") as f:
        expected_branch_a_file_1_content = f.read()
    with repo.open_file(branch_a_file_2, "r") as f:
        expected_branch_a_file_2_content = f.read()
    with repo.open_file(branch_a_file_3, "r") as f:
        expected_branch_a_file_3_content = f.read()

    # ========== Create branch_b from main and make commits ==========
    repo.branch_switch("main")
    repo.branch_create("branch_b")

    # Commits on branch_b: modify file_1 only (no overlap with cherry-picked commits)
    with repo.open_file(file_1, "a") as f:
        f.write("file_1 - branch_b commit 1\n")
    repo.stage(file_1)
    repo.commit("branch_b - modify file_1 (commit 1)")

    with repo.open_file(file_1, "a") as f:
        f.write("file_1 - branch_b commit 2\n")
    repo.stage(file_1)
    repo.commit("branch_b - modify file_1 (commit 2)")
    repo.push()

    # ========== Cherry-pick commits 4 and 5 from branch_a onto branch_b ==========
    # These should apply cleanly since they modify file_2 and file_3 which branch_b hasn't touched
    repo.revision_cherry_pick(
        revision=commit_4_revision,
        message="Cherry-pick commit 4 from branch_a",
    )

    repo.revision_cherry_pick(
        revision=commit_5_revision,
        message="Cherry-pick commit 5 from branch_a",
    )
    repo.push()

    # Verify cherry-picked content is present
    with repo.open_file(file_2, "r") as f:
        file_2_after_cherry_pick = f.read()
    with repo.open_file(file_3, "r") as f:
        file_3_after_cherry_pick = f.read()

    # Verify ONLY the cherry-picked edits (commits 4-5) are present, NOT the early edits (commits 1-3)
    assert "file_2 - branch_a commit 4 (bottom edit)" in file_2_after_cherry_pick, (
        "file_2 should contain commit 4 bottom edit after cherry-pick"
    )
    assert (
        "file_2 - branch_a commit 1 (early edit at top)" not in file_2_after_cherry_pick
    ), "file_2 should NOT contain commit 1 top edit after cherry-pick (not yet merged)"
    assert (
        "file_2 - branch_a commit 2 (early edit at top)" not in file_2_after_cherry_pick
    ), "file_2 should NOT contain commit 2 top edit after cherry-pick (not yet merged)"
    assert (
        "file_2 - branch_a commit 3 (early edit at top)" not in file_2_after_cherry_pick
    ), "file_2 should NOT contain commit 3 top edit after cherry-pick (not yet merged)"
    assert "file_3 - branch_a commit 5 (bottom edit)" in file_3_after_cherry_pick, (
        "file_3 should contain commit 5 content after cherry-pick"
    )

    # Verify the NEW files from commits 1-3 do NOT exist yet (they weren't cherry-picked)
    assert not repo.file_exists(branch_a_file_1), (
        "branch_a_1.txt should NOT exist before merge"
    )
    assert not repo.file_exists(branch_a_file_2), (
        "branch_a_2.txt should NOT exist before merge"
    )
    assert not repo.file_exists(branch_a_file_3), (
        "branch_a_3.txt should NOT exist before merge"
    )

    # ========== Merge branch_a into branch_b ==========
    # This should bring in commits 1, 2, 3 (the new files) from branch_a
    repo.branch_merge("branch_a")
    repo.push()

    # ========== Verify all branch_a content is present in branch_b ==========
    # Verify the new files from commits 1-3 now exist (brought in by merge)
    assert repo.file_exists(branch_a_file_1), (
        "branch_a_1.txt should exist after merge - proves merge brought in early history"
    )
    assert repo.file_exists(branch_a_file_2), "branch_a_2.txt should exist after merge"
    assert repo.file_exists(branch_a_file_3), "branch_a_3.txt should exist after merge"

    # Verify the content matches expected
    with repo.open_file(file_2, "r") as f:
        final_file_2 = f.read()
    with repo.open_file(file_3, "r") as f:
        final_file_3 = f.read()
    with repo.open_file(branch_a_file_1, "r") as f:
        final_branch_a_file_1 = f.read()
    with repo.open_file(branch_a_file_2, "r") as f:
        final_branch_a_file_2 = f.read()
    with repo.open_file(branch_a_file_3, "r") as f:
        final_branch_a_file_3 = f.read()

    # Verify file_2 now contains BOTH the early edits (commits 1-3) AND the cherry-picked edits (commits 4-5)
    assert final_file_2 == expected_file_2_content, (
        f"file_2 content should match branch_a's final state.\n"
        f"Expected:\n{expected_file_2_content}\nGot:\n{final_file_2}"
    )
    assert final_file_3 == expected_file_3_content, (
        f"file_3 content should match branch_a's final state.\n"
        f"Expected:\n{expected_file_3_content}\nGot:\n{final_file_3}"
    )
    assert final_branch_a_file_1 == expected_branch_a_file_1_content, (
        f"branch_a_1.txt content should match.\n"
        f"Expected:\n{expected_branch_a_file_1_content}\nGot:\n{final_branch_a_file_1}"
    )
    assert final_branch_a_file_2 == expected_branch_a_file_2_content, (
        f"branch_a_2.txt content should match.\n"
        f"Expected:\n{expected_branch_a_file_2_content}\nGot:\n{final_branch_a_file_2}"
    )
    assert final_branch_a_file_3 == expected_branch_a_file_3_content, (
        f"branch_a_3.txt content should match.\n"
        f"Expected:\n{expected_branch_a_file_3_content}\nGot:\n{final_branch_a_file_3}"
    )


@pytest.mark.smoke
def test_cherry_pick_metadata(new_lore_repo):
    """
    A cherry-pick carries the keys named by --inherit-metadata from the source
    revision, not from the target branch.

    Both revisions set reviewed-by, merged-by and change-request to different
    values. The named keys arrive from the source; merged-by is reserved, and a
    cherry-pick is not a merge, so it names no merger at all.
    """
    repo: Lore = new_lore_repo()

    # Create initial file on main
    with repo.open_file("initial.txt", "w") as f:
        f.write("Initial content\n")
    repo.stage("initial.txt")
    repo.commit("Initial commit")
    repo.push()

    # Create feature branch and add a new file with source metadata.
    repo.branch_create("feature-metadata")
    with repo.open_file("feature_file.txt", "w") as f:
        f.write("Feature content\n")
    repo.stage("feature_file.txt")
    repo.revision_metadata_set(["merged-by", "source-merger@example.com"])
    repo.revision_metadata_set(["reviewed-by", "source-reviewer@example.com"])
    repo.revision_metadata_set(["change-request", "SOURCE-CR-111"])
    repo.commit("Add feature file")
    repo.push()

    # Verify the source metadata is set on the feature branch
    assert "source-merger@example.com" in repo.revision_metadata_get("merged-by")
    assert "source-reviewer@example.com" in repo.revision_metadata_get("reviewed-by")
    assert "SOURCE-CR-111" in repo.revision_metadata_get("change-request")

    # Switch back to main and make a second change with different metadata
    repo.branch_switch("main")
    with repo.open_file("initial.txt", "a") as f:
        f.write("Second line\n")
    repo.stage("initial.txt")
    repo.revision_metadata_set(["reviewed-by", "target-reviewer@example.com"])
    repo.revision_metadata_set(["merged-by", "target-merger@example.com"])
    repo.revision_metadata_set(["change-request", "TARGET-CR-999"])
    repo.commit("Second commit with metadata")
    repo.push()

    # Verify the target metadata is present on main's latest revision
    assert "target-reviewer@example.com" in repo.revision_metadata_get("reviewed-by")
    assert "target-merger@example.com" in repo.revision_metadata_get("merged-by")
    assert "TARGET-CR-999" in repo.revision_metadata_get("change-request")

    # Cherry-pick the feature commit (should auto-commit since no conflicts).
    repo.revision_cherry_pick(
        revision="feature-metadata@LATEST",
        message="Cherry-pick feature file onto main",
        inherit_metadata=["change-request", "reviewed-by"],
    )

    logger.info(repo.revision_metadata_get())

    # Verify cherry-picked-from IS present
    cherry_picked_from = repo.revision_metadata_get("cherry-picked-from")
    assert cherry_picked_from.strip(), (
        f"cherry-picked-from should be set on the cherry-picked revision. Got:\n{cherry_picked_from}"
    )

    # The named keys arrive from the source, not the target
    change_request_after = repo.revision_metadata_get("change-request")
    assert "SOURCE-CR-111" in change_request_after, (
        f"change-request should come from the source revision. Got:\n{change_request_after}"
    )
    assert "TARGET-CR-999" not in change_request_after, (
        f"change-request should NOT come from the target branch. Got:\n{change_request_after}"
    )

    reviewed_by_after = repo.revision_metadata_get("reviewed-by")
    assert "source-reviewer@example.com" in reviewed_by_after, (
        f"reviewed-by should come from the source revision. Got:\n{reviewed_by_after}"
    )
    assert "target-reviewer@example.com" not in reviewed_by_after, (
        f"reviewed-by should NOT come from the target branch. Got:\n{reviewed_by_after}"
    )

    # merged-by is reserved, and a cherry-pick performs no merge
    merged_by_after = repo.revision_metadata_get("merged-by").strip()
    assert not merged_by_after, (
        f"a cherry-pick should name no merger. Got:\n{merged_by_after}"
    )


@pytest.mark.smoke
def test_cherry_pick_default_commit_message(new_lore_repo):
    """
    Test that when no message is provided, cherry-pick uses the source
    revision's original commit message for the auto-commit.
    """
    repo: Lore = new_lore_repo()

    # Create initial commit on main
    main_file = "main.txt"
    main_message = "initial commit"
    with repo.open_file(main_file, "w") as f:
        f.write(main_message)
    repo.stage(main_file)
    repo.commit(main_message)
    repo.push()

    # Create feature branch and commit with a known message
    repo.branch_create("feature")
    feature_file = "feature.txt"
    feature_message = "feature content"
    with repo.open_file(feature_file, "w") as f:
        f.write(feature_message)
    repo.stage(feature_file)
    repo.commit(feature_message)
    repo.push()

    # Get the feature revision signature
    feature_revisions = repo.history(branch="feature")
    feature_revision = feature_revisions[-1].signature

    # Switch to main and cherry-pick without providing a message
    repo.branch_switch("main")
    repo.revision_cherry_pick(revision=feature_revision)

    # Verify the new commit on main uses the source revision's message
    main_revisions = repo.history(branch="main")
    cherry_picked_revision = main_revisions[-1]
    assert cherry_picked_revision.message == feature_message, (
        "Cherry-pick should use the source commit message when --message is not provided. "
    )


@pytest.mark.smoke
def test_cherry_pick_of_a_move_of_a_locally_edited_file_refuses(new_lore_repo):
    """A cherry-pick of a revision that moves a file the working tree edited, and changes its
    content, refuses and keeps the edit at the path the file moves from."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("Base", {"moved/from.txt": "base\n", "other.txt": "o\n"})
    repo.branch_create("move-file")
    repo.move("moved/from.txt", "moved/to.txt")
    repo.stage_move("moved/from.txt", "moved/to.txt")
    repo.write_files({"moved/to.txt": "incoming\n"})
    repo.stage("moved/to.txt")
    repo.commit("Move and modify")
    repo.push()
    moved = repo.branch_info().local_latest
    repo.branch_switch("main")
    repo.write_files({"moved/from.txt": "locally edited\n"})

    with pytest.raises(LocalChanges):
        repo.revision_cherry_pick(moved)

    with repo.open_file("moved/from.txt") as f:
        assert f.read() == "locally edited\n", "the refused cherry-pick should keep the edit"
    assert not repo.file_exists("moved/to.txt")
