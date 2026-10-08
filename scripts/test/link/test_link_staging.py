# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import os
import re

import pytest
from link_helpers import DEFAULT_PARENT_FILE, make_parent_with_link
from lore_parsers import parse_status_json
from test_utils import unstaged_entries, working_tree_files

from lore import Lore


@pytest.mark.smoke
def test_link_staging(new_lore_repo):
    """Test comprehensive link staging scenarios including staging from within links,
    move operations within links, and cross-repository moves."""
    repo: Lore = new_lore_repo()

    # Setup Phase: Create Main Repository
    main_initial_file = "main-initial.txt"
    with repo.open_file(main_initial_file, "w+") as output_file:
        output_file.writelines(["Initial main repository content\n"])

    repo.stage(scan=True)
    repo.commit("Initial main repository setup")
    repo.push()

    # Setup Phase: Create Link Repository
    link_repo = new_lore_repo()

    # Create initial file structure in link repository
    link_root_file = "root-file.txt"
    link_nested_file = "subdir/nested-file.txt"
    link_another_file = "subdir/another-file.txt"
    link_deep_file = "deep/path/deep-file.txt"

    # Create root file
    with link_repo.open_file(link_root_file, "w+") as output_file:
        output_file.writelines(["Initial content of root file\n"])

    # Create nested file in subdirectory
    link_repo.make_dirs(os.path.dirname(link_nested_file))
    with link_repo.open_file(link_nested_file, "w+") as output_file:
        output_file.writelines(["Initial content of nested file\n"])

    # Create another file in same subdirectory
    with link_repo.open_file(link_another_file, "w+") as output_file:
        output_file.writelines(["Initial content of another file\n"])

    # Create deeply nested file
    link_repo.make_dirs(os.path.dirname(link_deep_file))
    with link_repo.open_file(link_deep_file, "w+") as output_file:
        output_file.writelines(["Initial content of deep file\n"])

    # Stage, commit, and push initial state
    link_repo.stage(scan=True)
    link_repo.commit("Initial link repository structure")
    link_repo.push()

    # Setup Phase: Add Link to Main Repository
    link_path = "link/here"
    repo.link_add(link_path, link_repo.get_id(), "/")

    # Verify initial linked files are accessible
    linked_root_file = f"{link_path}/root-file.txt"
    linked_nested_file = f"{link_path}/subdir/nested-file.txt"
    linked_another_file = f"{link_path}/subdir/another-file.txt"
    linked_deep_file = f"{link_path}/deep/path/deep-file.txt"

    assert repo.compare_file(repo, linked_root_file), (
        "Root file should be accessible via link"
    )
    assert repo.compare_file(repo, linked_nested_file), (
        "Nested file should be accessible via link"
    )
    assert repo.compare_file(repo, linked_another_file), (
        "Another file should be accessible via link"
    )
    assert repo.compare_file(repo, linked_deep_file), (
        "Deep file should be accessible via link"
    )

    # Commit and push link setup
    repo.commit("Add link setup")
    repo.push()

    # Test Case 1: Stage Single File in Link Root

    # Modify file content
    with repo.open_file(linked_root_file, "w+") as output_file:
        output_file.writelines(["Modified content of root file\n"])

    # Stage by path
    repo.stage(linked_root_file)

    # Verify status shows file staged for commit
    status_output = repo.status()
    assert "Changes staged for commit" in status_output, (
        "Status should show staged changes"
    )
    assert "M " + linked_root_file in status_output, (
        f"Status should show modified file {linked_root_file}"
    )

    # Verify parent repository shows link as modified
    assert link_path in status_output, "Parent repository should show link as modified"

    # Test Case 2: Stage Single File in Link Subdirectory

    # Modify file content in subdirectory
    with repo.open_file(linked_nested_file, "w+") as output_file:
        output_file.writelines(["Modified content of nested file\n"])

    # Stage by path
    repo.stage(linked_nested_file)

    # Verify status shows file staged for commit
    status_output_2 = repo.status()
    assert "Changes staged for commit" in status_output_2, (
        "Status should show staged changes"
    )
    assert "M " + linked_nested_file in status_output_2, (
        f"Status should show modified file {linked_nested_file}"
    )

    # Verify parent repository shows link as modified
    assert link_path in status_output_2, (
        "Parent repository should show link as modified"
    )

    # Test Case 3: Stage Multiple Files in Link by Individual Paths

    # Modify multiple files
    with repo.open_file(linked_another_file, "w+") as output_file:
        output_file.writelines(["Modified content of another file\n"])

    linked_new_file = f"{link_path}/new-file.txt"
    with repo.open_file(linked_new_file, "w+") as output_file:
        output_file.writelines(["Content of new file\n"])

    with repo.open_file(linked_deep_file, "w+") as output_file:
        output_file.writelines(["Modified content of deep file\n"])

    # Stage files individually and verify progressive changes
    repo.stage(linked_another_file)
    status_after_3a = repo.status()
    assert "M " + linked_another_file in status_after_3a, (
        "Another file should be staged"
    )

    repo.stage(linked_new_file)
    status_after_3b = repo.status()
    assert "A " + linked_new_file in status_after_3b, (
        "New file should be staged as addition"
    )

    repo.stage(linked_deep_file)

    # Verify all files appear as staged in final status
    final_status_3 = repo.status()
    assert "M " + linked_another_file in final_status_3, "Another file should be staged"
    assert "A " + linked_new_file in final_status_3, "New file should be staged"
    assert "M " + linked_deep_file in final_status_3, "Deep file should be staged"

    # Test Case 4: Stage Files with Mixed Operations

    # First, commit current staged changes to reset for mixed operations test
    repo.commit("Commit previous test changes")

    # Perform mixed file operations
    # Modify existing file (root file was already modified in Test Case 1)
    with repo.open_file(linked_root_file, "w+") as output_file:
        output_file.writelines(["Second modification of root file\n"])

    # Add new file
    linked_added_file = f"{link_path}/subdir/added-file.txt"
    with repo.open_file(linked_added_file, "w+") as output_file:
        output_file.writelines(["Content of added file\n"])

    # Delete existing file (using the nested file)
    linked_deleted_file = linked_nested_file
    repo.remove_file(linked_deleted_file)

    # Stage each operation by path
    repo.stage(linked_root_file)
    repo.stage(linked_added_file)
    repo.stage(linked_deleted_file)

    # Verify status output shows correct operation flags
    status_mixed = repo.status()
    assert "M " + linked_root_file in status_mixed, (
        "Should show M for modified root file"
    )
    assert "A " + linked_added_file in status_mixed, "Should show A for added file"
    assert "D " + linked_deleted_file in status_mixed, "Should show D for deleted file"

    # Test Case 5: Commit and Verify State Serialization

    commit_output = repo.commit("Stage files in link by path")

    # Verify commit success
    assert "Commit succeeded" in commit_output, "Commit should succeed"

    # Verify clean status after commit
    post_commit_status = repo.status()
    assert "Changes staged for commit" not in post_commit_status, (
        "Status should be clean after commit"
    )
    assert "Changes to be committed" not in post_commit_status, (
        "Status should be clean after commit"
    )

    # Push changes before cloning
    repo.push()

    # Test Case 6: Clone and Verify Persistence

    clone_repo = repo.clone()

    # Verify modified files have correct updated content
    clone_root_file = f"{link_path}/root-file.txt"
    assert clone_repo.file_exists(clone_root_file), (
        "Modified root file should exist in clone"
    )
    assert repo.compare_file(clone_repo, clone_root_file), (
        "Modified root file content should match"
    )

    # Verify new files exist with correct content
    clone_added_file = f"{link_path}/subdir/added-file.txt"
    assert clone_repo.file_exists(clone_added_file), "Added file should exist in clone"
    assert repo.compare_file(clone_repo, clone_added_file), (
        "Added file content should match"
    )

    # Verify deleted files are absent
    clone_deleted_file = f"{link_path}/subdir/nested-file.txt"
    assert not clone_repo.file_exists(clone_deleted_file), (
        "Deleted file should not exist in clone"
    )

    # Verify other files still exist and match
    clone_another_file = f"{link_path}/subdir/another-file.txt"
    clone_deep_file = f"{link_path}/deep/path/deep-file.txt"
    clone_new_file = f"{link_path}/new-file.txt"

    assert repo.compare_file(clone_repo, clone_another_file), (
        "Another file should match between repos"
    )
    assert repo.compare_file(clone_repo, clone_deep_file), (
        "Deep file should match between repos"
    )
    assert repo.compare_file(clone_repo, clone_new_file), (
        "New file should match between repos"
    )

    # Test Case 7: Sync and Verify Consistency

    sync_repo = repo.clone()
    sync_repo.sync()

    # Verify synchronized state - all modified files have correct content
    sync_root_file = f"{link_path}/root-file.txt"
    assert sync_repo.compare_file(repo, sync_root_file), (
        "Sync: Modified root file should match"
    )

    # Verify all new files exist
    sync_added_file = f"{link_path}/subdir/added-file.txt"
    sync_new_file = f"{link_path}/new-file.txt"
    assert sync_repo.compare_file(repo, sync_added_file), (
        "Sync: Added file should match"
    )
    assert sync_repo.compare_file(repo, sync_new_file), "Sync: New file should match"

    # Verify deleted files are absent
    sync_deleted_file = f"{link_path}/subdir/nested-file.txt"
    assert not sync_repo.file_exists(sync_deleted_file), (
        "Sync: Deleted file should not exist"
    )

    # Verify link state is consistent between original and sync
    sync_another_file = f"{link_path}/subdir/another-file.txt"
    sync_deep_file = f"{link_path}/deep/path/deep-file.txt"
    assert sync_repo.compare_file(repo, sync_another_file), (
        "Sync: Another file should be consistent"
    )
    assert sync_repo.compare_file(repo, sync_deep_file), (
        "Sync: Deep file should be consistent"
    )

    # Test Case 8: Verify Link Node Updates in Parent

    repo_dump_output = repo.repository_dump()

    # Verify link state - look for link node address hash
    link_repo_id = link_repo.get_id()
    assert link_repo_id in repo_dump_output, (
        "Link repository ID should appear in repository dump"
    )

    # Verify link revision hash reflects changes by checking for valid hash patterns

    link_revision_pattern = r"rev ([0-9a-f]{64})"
    revision_matches = re.findall(link_revision_pattern, repo_dump_output)
    assert revision_matches, "Repository dump should contain link revision hashes"

    # Verify link appears in the dump correctly (no specific staged check needed as we already committed)
    assert "link" in repo_dump_output, "Repository dump should show link information"

    # Test Case 9: Multiple Link Operations

    # Create second link repository
    second_link_repo = new_lore_repo()
    second_link_file = "second-file.txt"
    with second_link_repo.open_file(second_link_file, "w+") as output_file:
        output_file.writelines(["Content from second link repository\n"])

    second_link_repo.stage(scan=True)
    second_link_repo.commit("Initial second link repository")
    second_link_repo.push()

    # Add second link at different path
    second_link_path = "other/link/path"
    repo.link_add(second_link_path, second_link_repo.get_id(), "/")

    # Verify second link files are accessible
    second_linked_file = f"{second_link_path}/second-file.txt"
    assert repo.compare_file(repo, second_linked_file), (
        "Second link file should be accessible"
    )

    repo.commit("Add second link")

    # Stage files in both links
    # Modify file in first link
    first_link_test_file = f"{link_path}/root-file.txt"
    with repo.open_file(first_link_test_file, "w+") as output_file:
        output_file.writelines(["Final modification for first link\n"])

    # Modify file in second link
    with repo.open_file(second_linked_file, "w+") as output_file:
        output_file.writelines(["Modified content from second link\n"])

    # Stage files in both links
    repo.stage(first_link_test_file)
    repo.stage(second_linked_file)

    # Verify independent state tracking
    final_status = repo.status()
    assert "M " + first_link_test_file in final_status, (
        "First link modification should be staged"
    )
    assert "M " + second_linked_file in final_status, (
        "Second link modification should be staged"
    )

    # Both links should show as modified in parent repository
    assert link_path in final_status, "First link path should show as modified"
    assert second_link_path in final_status, "Second link path should show as modified"

    # Verify repository dump reflects both link updates
    final_dump = repo.repository_dump()
    first_link_id = link_repo.get_id()
    second_link_id = second_link_repo.get_id()
    assert first_link_id in final_dump, "First link should be in repository dump"
    assert second_link_id in final_dump, "Second link should be in repository dump"

    # Commit final changes before verification
    repo.commit("Final link staging changes")
    repo.push()

    # Test Case 10: Verification Phase - Clone and Sync Test

    # Test clone and verify all changes persist correctly
    final_clone_repo = repo.clone()

    # Define expected file paths for verification (files that were staged within the link)
    expected_file_path = linked_root_file  # Root file that was modified and staged
    expected_subdir_file_path = (
        linked_added_file  # Subdirectory file that was added and staged
    )

    # Verify files staged from within link
    assert final_clone_repo.file_exists(expected_file_path), (
        "File staged from within link should persist in clone"
    )
    assert final_clone_repo.file_exists(expected_subdir_file_path), (
        "Subdir file staged from within link should persist in clone"
    )

    # Sync test to verify consistency
    sync_test_repo = repo.clone()
    sync_test_repo.sync()

    # Verify synchronized state matches all operations
    assert sync_test_repo.compare_file(repo, expected_file_path), (
        "Sync: File staged from within link should match"
    )


@pytest.mark.smoke
def test_link_unstage(new_lore_repo):
    """Test selective unstaging of individual files within linked repositories."""
    repo: Lore = new_lore_repo()

    # Create source repository
    with repo.open_file("source-file.txt", "w+") as output_file:
        output_file.writelines(["source repository content\n"])

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    # Create link repository with multiple files
    link_repo = new_lore_repo()

    # Create multiple files in different directories
    link_file1 = "file1.txt"
    link_file2 = "file2.txt"
    link_subdir_file = "subdir/file3.txt"

    with link_repo.open_file(link_file1, "w+") as output_file:
        output_file.writelines(["link file 1 content\n"])

    with link_repo.open_file(link_file2, "w+") as output_file:
        output_file.writelines(["link file 2 content\n"])

    link_repo.make_dirs("subdir")
    with link_repo.open_file(link_subdir_file, "w+") as output_file:
        output_file.writelines(["link subdir file content\n"])

    link_repo.stage(scan=True)
    link_repo.commit()
    link_repo.push()

    # Add link to main repository
    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")

    expected_file1 = f"{link_path}/{link_file1}"
    expected_file2 = f"{link_path}/{link_file2}"
    expected_subdir_file = f"{link_path}/{link_subdir_file}"

    # Verify link files exist
    assert repo.compare_file(repo, expected_file1)
    assert repo.compare_file(repo, expected_file2)
    assert repo.compare_file(repo, expected_subdir_file)

    repo.commit()
    repo.push()

    # Make changes to multiple files in linked repository
    with repo.open_file(expected_file1, "w+") as output_file:
        output_file.writelines(["MODIFIED file 1 content\n"])

    with repo.open_file(expected_file2, "w+") as output_file:
        output_file.writelines(["MODIFIED file 2 content\n"])

    with repo.open_file(expected_subdir_file, "w+") as output_file:
        output_file.writelines(["MODIFIED subdir file content\n"])

    # Create a new file in linked repo
    new_link_file = f"{link_path}/new-file.txt"
    with repo.open_file(new_link_file, "w+") as output_file:
        output_file.writelines(["NEW file content\n"])

    # Stage all changes
    output = repo.stage(scan=True)
    assert "4 files" in output, "4 files should be staged (3 modified + 1 added)"

    # Verify all files are staged using JSON status checks
    staged_status_output = repo.status(json=True)
    staged_status_entries = parse_status_json(staged_status_output)
    staged_files = [entry["path"] for entry in staged_status_entries]

    assert expected_file1 in staged_files, f"File {expected_file1} should be staged"
    assert expected_file2 in staged_files, f"File {expected_file2} should be staged"
    assert expected_subdir_file in staged_files, (
        f"File {expected_subdir_file} should be staged"
    )
    assert new_link_file in staged_files, f"File {new_link_file} should be staged"

    # Test 1: Unstage only one specific file within the link
    repo.unstage(expected_file1)

    # Verify status after partial unstage
    partial_staged_output = repo.status(json=True)
    partial_staged_entries = parse_status_json(partial_staged_output)
    staged_files_after = [entry["path"] for entry in partial_staged_entries]

    partial_unstaged_output = repo.status(json=True, unstaged=True)
    partial_unstaged_entries = parse_status_json(partial_unstaged_output)
    unstaged_files_after = [entry["path"] for entry in partial_unstaged_entries]

    # File1 should now be unstaged
    assert expected_file1 in unstaged_files_after, (
        f"File {expected_file1} should be unstaged"
    )

    # Other files should remain staged
    assert expected_file2 in staged_files_after, (
        f"File {expected_file2} should remain staged"
    )
    assert expected_subdir_file in staged_files_after, (
        f"File {expected_subdir_file} should remain staged"
    )
    assert new_link_file in staged_files_after, (
        f"File {new_link_file} should remain staged"
    )

    # Test 2: Unstage an entire subdirectory within the link
    repo.unstage(f"{link_path}/subdir")

    # Verify subdirectory unstaging
    subdir_staged_output = repo.status(json=True)
    subdir_staged_entries = parse_status_json(subdir_staged_output)
    subdir_staged_files = [entry["path"] for entry in subdir_staged_entries]

    subdir_unstaged_output = repo.status(json=True, unstaged=True)
    subdir_unstaged_entries = parse_status_json(subdir_unstaged_output)
    subdir_unstaged_files = [entry["path"] for entry in subdir_unstaged_entries]

    # Subdir file should now be unstaged
    assert expected_subdir_file in subdir_unstaged_files, (
        f"File {expected_subdir_file} should be unstaged"
    )

    # File1 should still be unstaged, file2 and new file should remain staged
    assert expected_file1 in subdir_unstaged_files, (
        f"File {expected_file1} should remain unstaged"
    )
    assert expected_file2 in subdir_staged_files, (
        f"File {expected_file2} should remain staged"
    )
    assert new_link_file in subdir_staged_files, (
        f"File {new_link_file} should remain staged"
    )

    # Test 3: Unstage the entire link directory
    repo.unstage(link_path)

    # Verify all link files are now unstaged
    final_staged_output = repo.status(json=True)
    final_staged_entries = parse_status_json(final_staged_output)
    final_staged_files = [entry["path"] for entry in final_staged_entries]

    final_unstaged_output = repo.status(json=True, unstaged=True)
    final_unstaged_entries = parse_status_json(final_unstaged_output)
    final_unstaged_files = [entry["path"] for entry in final_unstaged_entries]

    # All link files should be unstaged (appear in unstaged status)
    assert expected_file1 in final_unstaged_files, (
        f"File {expected_file1} should be unstaged"
    )
    assert expected_file2 in final_unstaged_files, (
        f"File {expected_file2} should be unstaged"
    )
    assert expected_subdir_file in final_unstaged_files, (
        f"File {expected_subdir_file} should be unstaged"
    )
    assert new_link_file in final_unstaged_files, (
        f"File {new_link_file} should be unstaged"
    )

    # No link files should remain staged
    link_staged_files = [f for f in final_staged_files if f.startswith(link_path)]
    assert len(link_staged_files) == 0, (
        f"No link files should remain staged, but found: {link_staged_files}"
    )


def _staged_paths(repo: Lore) -> list[str]:
    """Paths `status` reports as staged. It reports unstaged changes too, marked
    with `flagStaged` false, so the flag is what separates the two."""
    return [
        entry["path"]
        for entry in parse_status_json(repo.status(json=True))
        if entry["flagStaged"]
    ]


@pytest.mark.smoke
def test_link_unstage_honours_a_rule_naming_the_mount(new_lore_repo):
    """The filter matches link content by its mount path, not its source path.

    A link mounted at `linked` onto `/sub` reaches `sub/inside.txt` in the source
    repository for a file the flattened tree spells `linked/inside.txt`. Rules are
    written against the flattened tree, so unstage has to match the mount path or
    it unstages content the filter excludes.

    Both crossings are covered: unstaging the repository root reaches the link
    node itself, and unstaging a path inside the link resolves through it to a
    directory in the source repository.
    """
    repo: Lore = new_lore_repo()

    outside_file = "outside.txt"
    with repo.open_file(outside_file, "w+") as output_file:
        output_file.writelines(["outside original\n"])

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    link_repo = new_lore_repo()
    link_repo.make_dirs("sub/nested")
    with link_repo.open_file("sub/inside.txt", "w+") as output_file:
        output_file.writelines(["inside original\n"])
    with link_repo.open_file("sub/nested/deep.txt", "w+") as output_file:
        output_file.writelines(["deep original\n"])

    link_repo.stage(scan=True)
    link_repo.commit()
    link_repo.push()

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/sub")
    repo.commit()
    repo.push()

    mounted_file = f"{link_path}/inside.txt"
    deep_file = f"{link_path}/nested/deep.txt"
    assert repo.file_exists(mounted_file), f"{mounted_file} should be materialized"
    assert repo.file_exists(deep_file), f"{deep_file} should be materialized"

    def restage_everything():
        with repo.open_file(mounted_file, "w+") as output_file:
            output_file.writelines(["inside modified\n"])
        with repo.open_file(deep_file, "w+") as output_file:
            output_file.writelines(["deep modified\n"])
        with repo.open_file(outside_file, "w+") as output_file:
            output_file.writelines(["outside modified\n"])
        repo.stage(scan=True)

    restage_everything()

    staged = _staged_paths(repo)
    assert mounted_file in staged, f"{mounted_file} should be staged"
    assert outside_file in staged, f"{outside_file} should be staged"

    # Names the file below the mount rather than the mount, so the walk descends
    # and the verdict is reached inside the link. It matches the flattened
    # `linked/inside.txt` and cannot match the source path `sub/inside.txt`.
    with repo.open_file(repo.ignore_file(), "w+") as ignore_file:
        ignore_file.write(f"/{link_path}/inside.txt\n")

    repo.unstage(".")

    # Dropped so the status below reports the mounted file either way.
    repo.remove_file(repo.ignore_file())

    staged = _staged_paths(repo)
    assert outside_file not in staged, (
        "Unstage should unstage a file the filter does not exclude"
    )
    assert mounted_file in staged, (
        "Unstage should skip link content the filter excludes"
    )

    # The same verdict, reached the other way: `linked/nested` resolves through
    # the link to a directory in the source repository, which is the crossing
    # the root walk above does not take.
    restage_everything()
    with repo.open_file(repo.ignore_file(), "w+") as ignore_file:
        ignore_file.write(f"/{link_path}/nested/deep.txt\n")

    repo.unstage(f"{link_path}/nested")

    repo.remove_file(repo.ignore_file())

    staged = _staged_paths(repo)
    assert deep_file in staged, (
        "Unstage should skip content the filter excludes below a resolved link"
    )


_LINK_MOUNT_TREE = [DEFAULT_PARENT_FILE, "libs/shared/inner.txt"]


@pytest.mark.smoke
def test_link_scan_keeps_a_mount_missing_from_the_working_tree(new_lore_repo):
    """A scan reports the work it was given and leaves the link mounted."""
    link_path = "libs/shared"
    sibling = "libs/notes.txt"
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {"inner.txt": "linked content\n"},
        {DEFAULT_PARENT_FILE: "baseline\n", sibling: "sibling\n"},
    )
    clone = parent_repo.clone()
    assert clone.file_exists(f"{link_path}/inner.txt"), (
        "Setup: the clone realizes the mount"
    )

    clone.rmtree(link_path)
    clone.remove_file(sibling)
    clone.write_files({DEFAULT_PARENT_FILE: "edited\n"})

    clone.status(scan=True)
    scanned = [entry["path"] for entry in unstaged_entries(clone)]
    assert scanned == [DEFAULT_PARENT_FILE, sibling], (
        f"A scan reports the edit and the deletion, got {scanned}"
    )

    clone.stage(scan=True)
    assert _staged_paths(clone) == [DEFAULT_PARENT_FILE, sibling], (
        f"A scan stages the edit and the deletion, got {_staged_paths(clone)}"
    )

    clone.commit("Edit one file and delete another")
    clone.push()

    verify = parent_repo.clone()
    assert working_tree_files(verify) == _LINK_MOUNT_TREE, (
        f"The branch keeps the link mounted, got {working_tree_files(verify)}"
    )


@pytest.mark.smoke
def test_link_scan_keeps_a_mount_whose_parent_directory_is_missing(new_lore_repo):
    """A directory holding only a mount survives a scan that cannot see it."""
    link_path = "libs/shared"
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"inner.txt": "linked content\n"}
    )
    clone = parent_repo.clone()

    clone.rmtree("libs")
    clone.write_files({DEFAULT_PARENT_FILE: "edited\n"})

    clone.stage(scan=True)
    assert _staged_paths(clone) == [DEFAULT_PARENT_FILE], (
        f"A scan stages the edited file alone, got {_staged_paths(clone)}"
    )

    clone.commit("Edit the parent file")
    clone.push()

    verify = parent_repo.clone()
    assert working_tree_files(verify) == _LINK_MOUNT_TREE, (
        f"The branch keeps the link mounted, got {working_tree_files(verify)}"
    )


@pytest.mark.smoke
def test_link_scan_scoped_to_a_missing_directory_keeps_the_mount(new_lore_repo):
    """A scan given a path the tree no longer holds records what was below it.

    Node lookup is case-insensitive, so the path answers for the mount however
    the caller spelled it.
    """
    link_path = "libs/shared"
    sibling = "libs/notes.txt"
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path,
        {"inner.txt": "linked content\n"},
        {DEFAULT_PARENT_FILE: "baseline\n", sibling: "sibling\n"},
    )
    clone = parent_repo.clone()

    clone.rmtree("libs")

    scanned = [
        (entry["path"], entry["action"])
        for entry in parse_status_json(clone.status("LIBS", scan=True, json=True))
    ]
    assert scanned == [("LIBS/notes.txt", "delete")], (
        f"A scan of a case variant reports the deletion below it, got {scanned}"
    )

    clone.stage("libs", scan=True)
    assert _staged_paths(clone) == [sibling], (
        f"A scan of the missing path stages the deletion below it, got "
        f"{_staged_paths(clone)}"
    )

    clone.commit("Delete the sibling file")
    clone.push()

    verify = parent_repo.clone()
    assert working_tree_files(verify) == _LINK_MOUNT_TREE, (
        f"The branch keeps the link mounted, got {working_tree_files(verify)}"
    )


@pytest.mark.smoke
def test_link_scan_removes_the_directory_a_removed_link_emptied(new_lore_repo):
    """A link on its way out keeps nothing: the scan finishes what `link remove` started."""
    link_path = "libs/shared"
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"inner.txt": "linked content\n"}
    )
    clone = parent_repo.clone()

    clone.link_remove(link_path)
    clone.rmtree("libs")

    clone.stage(scan=True)
    assert _staged_paths(clone) == ["libs"], (
        f"A scan stages the emptied directory, got {_staged_paths(clone)}"
    )

    clone.commit("Remove the link")
    clone.push()

    verify = parent_repo.clone()
    assert working_tree_files(verify) == [DEFAULT_PARENT_FILE], (
        f"The branch holds the parent's own file alone, got {working_tree_files(verify)}"
    )
