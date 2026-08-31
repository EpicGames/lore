# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import json
import logging
import os
import re
import shutil

import pytest
from error_types import (
    LinkPinDivergedError,
    LocalChanges,
    LocalModificationsError,
    NestedLinkError,
    NotALinkError,
    NothingStagedError,
    PathExistChildrenLinkError,
    PathExistLinkError,
)
from lore_parsers import parse_commit_stats_json, parse_jsonl, parse_status_json
from thin_client import (
    ACTION_ADD,
    ACTION_DELETE,
    NODE_TYPE_FILE,
    NODE_TYPE_LINK,
    revision_diff,
    revision_tree,
)

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_link(new_lore_repo):
    repo: Lore = new_lore_repo()

    with repo.open_file(os.path.join(repo.dot_dir(), "id"), "rb") as id_file:
        raw_repository_id = id_file.read(32)
    _repository_id = raw_repository_id.hex()

    # Generate some files in source repo
    text_file = "text-File.txt"
    subpath_file = "path/to/some/file.uasset"

    with repo.open_file(text_file, "w+") as output_file:
        output_file.writelines(["source repository text file\n"])

    repo.make_dirs(os.path.dirname(subpath_file))
    with repo.open_file(subpath_file, "w+") as output_file:
        output_file.writelines(["something something in the source repository\n"])

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    # Create link repository
    link_repo = new_lore_repo()

    # Generate some files in link repo
    link_text_file = "text-File.txt"
    link_subpath_file = "another/path/with/a/file.uasset"
    link_extra_file = "another/path/with/extra.file"

    with link_repo.open_file(link_text_file, "w+") as output_file:
        output_file.writelines(["link repository text file\n"])

    link_repo.make_dirs(os.path.dirname(link_subpath_file))
    with link_repo.open_file(link_subpath_file, "w+") as output_file:
        output_file.writelines(["something something in the link repository\n"])

    with link_repo.open_file(link_extra_file, "w+") as output_file:
        output_file.writelines(["an extra file in the link repository\n"])

    link_repo.stage(scan=True)
    link_repo.commit()
    link_repo.push()

    # Create restricted repository
    restricted_repo = new_lore_repo()

    # Generate files in restricted repo
    restricted_text_file = "topsecret/test-file.txt"

    restricted_repo.make_dirs(os.path.dirname(restricted_text_file))
    with restricted_repo.open_file(restricted_text_file, "w+") as output_file:
        output_file.writelines(["top secret file content\n"])

    restricted_repo.stage(scan=True)
    restricted_repo.commit()
    restricted_repo.push()

    # Create new branch in restricted repo
    restricted_repo.branch_create("feature-branch")

    restricted_other_file = "topsecret/other-file.txt"

    with restricted_repo.open_file(restricted_other_file, "w+") as output_file:
        output_file.writelines(["other topsecret file\n"])

    restricted_repo.stage(scan=True)
    restricted_repo.commit()
    restricted_repo.push()

    sync_repo = repo.clone()

    # Create directory to link into
    link_relative_path = "link/insert/here"
    link_relative_path_file = os.path.join(link_relative_path, "blocking.txt")
    repo.make_dirs(link_relative_path)

    with repo.open_file(link_relative_path_file, "w+") as some_file:
        some_file.writelines(["this file is supposed to block adding the link"])

    # Link repository in subpath, expected to fail because of the blocking file
    try:
        link_add_output = repo.link_add(
            link_relative_path,
            link_repo.get_id(),
            "another/path",
        )
    except PathExistChildrenLinkError:
        pass
    else:
        assert "Failed to add link" in link_add_output, (
            "Link should not have been added to directory with children"
        )

    repo.remove_file(link_relative_path_file)

    # Try to add link again
    repo.link_add(link_relative_path, link_repo.get_id(), "another/path")

    expect_subpath_file = "link/insert/here/with/a/file.uasset"
    expect_extra_file = "link/insert/here/with/extra.file"

    # Verify files
    assert repo.compare_file(repo, expect_subpath_file)
    assert repo.compare_file(repo, expect_extra_file)

    repo.commit()
    repo.push()

    # Verify added link syncs
    sync_repo.sync()

    assert sync_repo.compare_file(repo, expect_subpath_file), "Subpath file missing"
    assert sync_repo.compare_file(repo, expect_extra_file), "Extra file missing"

    # Clone and verify link repositories
    clone = repo.clone()

    clone.repository_dump()

    # Verify files
    assert repo.compare_file(clone, text_file)
    assert repo.compare_file(clone, subpath_file)
    assert repo.compare_file(clone, expect_subpath_file)
    assert repo.compare_file(clone, expect_extra_file)

    # Create new branch
    clone.branch_create("another-feature")
    clone.push()
    sync_repo.branch_switch("another-feature")

    linked_branch_list = link_repo.branch_list()
    assert "another-feature" in linked_branch_list.remote_branches, (
        "Branch for linked repository was not created"
    )

    linked_branch_info = link_repo.branch_info("another-feature")
    linked_branch_id = linked_branch_info.id

    # List links and verify branch name is resolved
    output = clone.link_list()
    assert linked_branch_id in output, "Branch ID not shown in link list"
    assert "another-feature" in output, "Branch name not resolved in link list"

    # Create and stage files in link repository
    link_added_file = "link/insert/here/addedfile.file"
    link_modified_file = expect_extra_file
    link_deleted_file = expect_subpath_file
    some_file = "path/to/some/file.uasset"

    # Create a file
    with clone.open_file(link_added_file, "w+") as output_file:
        output_file.writelines(["AAAbbbCCCddd\n"])

    # Modify a file
    with clone.open_file(link_modified_file, "w+") as output_file:
        output_file.writelines(["modified file content\n"])

    # Modify some file
    with clone.open_file(some_file, "w+") as output_file:
        output_file.writelines(["MODIFIED.\n"])

    # Delete a file
    clone.remove_file(link_deleted_file)

    # Check file system changes
    output = clone.status(unstaged=True)

    assert "Changes not staged" in output, "No unstaged changes found before staging"
    assert "Untracked files" in output, "No untracked files found before staging"

    # Check 4 files were staged
    output = clone.stage(scan=True)

    assert "4 files" in output, "4 changed files were not staged"

    # Dump repository to compare link hashes
    output = clone.repository_dump()

    # Regex to match the link revision hash
    match = re.search(r"rev ([0-9a-f]{64})", output)

    assert match, "Link revision not found"
    previous_link_hash = match.group(1)

    # Unstage link files
    output = clone.unstage("link/insert", debug=True, offline=True)

    expected_output = f"old_hash={previous_link_hash}"
    assert expected_output in output, (
        f"link unstage revision not found: {expected_output} got instead"
    )

    clone.repository_dump()

    # Check status of link repository after unstage
    output = clone.status()

    assert "link/insert" not in output, "Some link changes still staged"

    output = clone.status(unstaged=True)

    assert "Changes not staged" in output, "No unstaged changes found after unstaging"
    assert "Untracked files" in output, "No untracked files found after unstaging"

    # Create and stage file in link again
    link_added_file = "link/insert/here/addedfile.file"
    link_modified_file = expect_extra_file
    link_deleted_file = expect_subpath_file
    some_file = "path/to/some/file.uasset"

    # Modify a file
    with clone.open_file(link_added_file, "w+") as output_file:
        output_file.writelines(["DDQQWJKHJALKSHLKA\n"])

    # Modify a file
    with clone.open_file(link_modified_file, "w+") as output_file:
        output_file.writelines(["content file modified\n"])

    # Modify some file
    with clone.open_file(some_file, "w+") as output_file:
        output_file.writelines(["MODIFIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIEEEED\n"])

    # Force stage files
    output = clone.stage(scan=True, debug=True, force=True)

    # Regex to match the staged link parent hash
    match = re.search(r"link parent to\s+([a-f0-9]{64})", output)

    assert match, "Force staged link parent not found"
    force_staged_link_parent = match.group(1)

    assert force_staged_link_parent != previous_link_hash, (
        "Parent of staged link is previously staged revision instead of original revision"
    )

    # Check status of link repository
    output = clone.status()

    assert "Changes staged for commit" in output, "No changes staged for commit"
    assert "A " + link_added_file in output, "Added file not staged"
    assert "M " + link_modified_file in output, "Modified file not staged"
    assert "D " + link_deleted_file in output, "Deleted file not staged"

    # Commit staged files in link repository
    output = clone.commit("Update link files")

    assert "Commit succeeded" in output, "Commit did not succeed"

    # Check unstaged status
    output = clone.status(unstaged=True)

    assert "Changes not staged" not in output, "Found unexpected unstaged changes"
    assert "Untracked files" not in output, "Found unexpected untracked files"

    # Reset a modified file inside the linked directory
    with clone.open_file(expect_extra_file, "w+") as output_file:
        output_file.writelines(["TEMPORARY MODIFICATION for reset test\n"])

    with clone.open_file(expect_extra_file, "r") as f:
        assert "TEMPORARY MODIFICATION" in f.read(), (
            "File should be modified before reset"
        )

    clone.reset(expect_extra_file)

    with clone.open_file(expect_extra_file, "r") as f:
        content = f.read()
        assert "TEMPORARY MODIFICATION" not in content, (
            "File should be restored after reset"
        )
        assert "content file modified" in content, (
            "File should have committed content after reset"
        )

    # Dump repository for link change validation after commit
    output = clone.repository_dump()

    # Regex to match the link revision hash
    match = re.search(r"rev ([0-9a-f]{64})", output)

    assert match, "New revision hash not found"
    new_link_hash = match.group(1)

    assert previous_link_hash != new_link_hash, "link revisions are the same"

    # Push link repository changes
    output = clone.push(debug=True)

    pattern = re.compile(
        r"(?im)^\s*(pushed revision)\b.*?\b([0-9a-f]{40}|[0-9a-f]{64})\b"
    )
    matches = [match.group(2) for match in pattern.finditer(output)]

    assert matches, "No revisions pushed"

    assert new_link_hash in matches, "Link revision not pushed"

    # List linked repositories
    output = clone.link_list()

    assert link_repo.get_id() in output, "Link not found in list"

    # Link repository in root
    restricted_relative_path = "restricted"
    clone.link_add(
        restricted_relative_path,
        restricted_repo.get_id(),
        "/",
        pin="feature-branch@LATEST",
        disable_branching=True,
    )

    # Verify restricted files
    expect_restricted_file = "restricted/topsecret/test-file.txt"
    expect_other_restricted_file = "restricted/topsecret/other-file.txt"

    assert clone.compare_file(clone, expect_restricted_file)
    assert clone.compare_file(clone, expect_other_restricted_file)

    clone.commit()
    clone.push()

    # Check whether changes sync
    sync_repo.sync()

    assert sync_repo.compare_file(clone, link_added_file)
    assert sync_repo.compare_file(clone, link_modified_file)
    assert not sync_repo.file_exists(link_deleted_file)
    assert sync_repo.compare_file(clone, some_file)

    # Verify that new link is fixed
    output = clone.link_list()

    pattern = rf"Link\s+{restricted_repo.get_id()}.*?Flags:\s+DisableAutoFollow \(0x1\)"
    match = re.search(pattern, output, re.DOTALL)

    assert match, f"Restricted link {restricted_repo.get_id()} is not fixed"

    # Remove link
    output = clone.link_remove(link_relative_path)

    assert "Removed link" in output, "Link was not removed"

    # List links to verify link that one link was removed
    output = clone.link_list()

    assert link_repo.get_id() not in output, "Initial subrepository still linked"

    clone.commit()
    clone.push()

    # Check whether link removal syncs correctly
    sync_repo.sync()

    assert not sync_repo.path_exists(link_relative_path)


@pytest.mark.smoke
def test_link_update(new_lore_repo):
    repo: Lore = new_lore_repo()

    # Create source repository for linking
    source_repo = new_lore_repo()

    # Generate initial files in source repo on main branch
    initial_file = "main-branch-file.txt"
    with source_repo.open_file(initial_file, "w+") as output_file:
        output_file.writelines(["Initial content on main branch\n"])

    source_repo.stage(scan=True)
    source_repo.commit("Initial commit on main")
    source_repo.push()

    # Get the current revision of main branch
    main_latest = source_repo.branch_info().local_latest

    # Create a feature branch with different content
    source_repo.branch_create("feature-branch")

    feature_file = "feature-branch-file.txt"
    with source_repo.open_file(feature_file, "w+") as output_file:
        output_file.writelines(["Content on feature branch\n"])

    # Modify the initial file as well
    with source_repo.open_file(initial_file, "w+") as output_file:
        output_file.writelines(["Modified content on feature branch\n"])

    source_repo.stage(scan=True)
    source_repo.commit("Feature branch commit")
    source_repo.push()

    # Get feature branch revision
    feature_latest = source_repo.branch_info().local_latest

    # Switch back to main and make another commit
    source_repo.branch_switch("main")

    main_update_file = "main-update-file.txt"
    with source_repo.open_file(main_update_file, "w+") as output_file:
        output_file.writelines(["Additional content on main\n"])

    source_repo.stage(scan=True)
    source_repo.commit("Second commit on main")
    source_repo.push()

    # Create main repository and add initial link to main branch
    link_path = "linked/repo"
    repo.link_add(
        link_path,
        source_repo.get_id(),
        "/",
        pin="main@LATEST",
    )

    # Verify initial files from main branch are present
    main_branch_file = f"{link_path}/{initial_file}"
    main_update_path = f"{link_path}/{main_update_file}"
    feature_branch_file_path = f"{link_path}/{feature_file}"

    assert repo.compare_file(repo, main_branch_file), (
        "Initial main file should be present"
    )
    assert repo.compare_file(repo, main_update_path), "Main update file should be present"
    assert not repo.file_exists(feature_branch_file_path), (
        "Feature file should not be present initially"
    )

    repo.commit("Add initial link to main branch")
    repo.push()

    # Update link pin to different branch (feature-branch)
    output = repo.link_update(
        link_path,
        pin="feature-branch@LATEST",
    )

    assert "Link updated" in output or "updated" in output.lower(), (
        "Link update should succeed"
    )

    # Verify status works after link update
    status_after_update = repo.status()
    assert "Changes staged for commit" in status_after_update, (
        "Status should show staged link change after update"
    )
    assert link_path in status_after_update, (
        "Link path should appear in staged status after update"
    )

    # Verify that files from feature branch are now present
    assert repo.compare_file(repo, main_branch_file), (
        "Modified main file should be present from feature branch"
    )
    assert repo.compare_file(repo, feature_branch_file_path), (
        "Feature file should now be present"
    )
    assert not repo.file_exists(main_update_path), (
        "Main update file should no longer be present"
    )

    # Verify the link list shows the new pin
    link_output = repo.link_list()
    assert feature_latest in link_output, (
        "Link should now point to feature branch latest"
    )

    repo.commit("Update link to feature branch")
    repo.push()

    # Update link pin to different revision on same branch (earlier commit on main)
    output = repo.link_update(
        link_path,
        pin=f"{main_latest}",
    )

    assert "Link updated" in output or "updated" in output.lower(), (
        "Link update to specific revision should succeed"
    )

    # Verify that we now have the earlier state of main (without the update file)
    assert repo.compare_file(repo, main_branch_file), (
        "Initial main file should be present"
    )
    assert not repo.file_exists(main_update_path), (
        "Main update file should not be present (earlier commit)"
    )
    assert not repo.file_exists(feature_branch_file_path), (
        "Feature file should not be present (back to main)"
    )

    # Verify the link list shows the specific revision
    link_output = repo.link_list()
    assert main_latest in link_output, (
        "Link should now point to specific main branch revision"
    )

    repo.commit("Update link to specific revision on main")
    repo.push()

    # Test sync functionality
    sync_repo = repo.clone()

    # Verify sync repository has the correct files after all updates
    sync_main_file = f"{link_path}/{initial_file}"
    sync_update_file = f"{link_path}/{main_update_file}"
    sync_feature_file = f"{link_path}/{feature_file}"

    assert sync_repo.compare_file(repo, sync_main_file), (
        "Sync should have correct main file"
    )
    assert not sync_repo.file_exists(sync_update_file), (
        "Sync should not have update file (earlier revision)"
    )
    assert not sync_repo.file_exists(sync_feature_file), (
        "Sync should not have feature file (back to main)"
    )

    # Test filesystem verification - link update should fail with local changes
    # Store current link state for verification
    link_output_before = repo.link_list()
    current_pin_match = re.search(
        rf"{source_repo.get_id()}.*?Revision: ([0-9a-f]{{64}})",
        link_output_before,
        re.DOTALL,
    )
    assert current_pin_match, (
        f"Could not find current link revision in output:\n{link_output_before}"
    )
    current_pin_revision = current_pin_match.group(1)

    # Create local filesystem changes in the linked repository path
    modified_file = f"{link_path}/{initial_file}"
    new_local_file = f"{link_path}/local-changes.txt"

    backup_content = None
    with repo.open_file(modified_file, "r") as f:
        backup_content = f.read()

    with repo.open_file(modified_file, "w+") as output_file:
        output_file.writelines(["LOCAL MODIFICATION - should prevent link update\n"])

    with repo.open_file(new_local_file, "w+") as output_file:
        output_file.writelines(["New local file that should prevent link update\n"])

    # Attempt link update with local changes - expected to fail
    try:
        update_output = repo.link_update(
            link_path,
            pin="feature-branch@LATEST",
        )
        # If we get here, the update unexpectedly succeeded
        assert False, (
            f"Link update should have failed with local changes, but got: {update_output}"
        )
    except LocalChanges:
        # This exception is expected - filesystem verification caught local changes
        pass

    # Verify link pin is unchanged after failed update
    link_output_after_fail = repo.link_list()
    assert current_pin_revision in link_output_after_fail, (
        "Link pin should remain unchanged after failed update"
    )
    assert feature_latest not in link_output_after_fail, (
        "Link should not have been updated to feature branch"
    )

    # Clean up local changes to test recovery
    with repo.open_file(modified_file, "w+") as output_file:
        output_file.write(backup_content)

    repo.remove_file(new_local_file)

    # Now attempt the same link update - this should SUCCEED
    success_output = repo.link_update(
        link_path,
        pin="feature-branch@LATEST",
    )
    assert "Link updated" in success_output or "updated" in success_output.lower(), (
        f"Link update should succeed after cleaning local changes, got: {success_output}"
    )

    # Verify the link was actually updated to feature branch
    final_link_output = repo.link_list()
    assert feature_latest in final_link_output, (
        "Link should now point to feature branch after successful update"
    )
    assert current_pin_revision not in final_link_output, (
        "Link should no longer point to previous revision"
    )

    # Verify filesystem content matches the feature branch
    feature_content_file = f"{link_path}/{feature_file}"
    assert repo.file_exists(feature_content_file), (
        "Feature branch file should be present after update"
    )


@pytest.mark.smoke
def test_link_update_status(new_lore_repo):
    """Test that status works after staging a link update.

    Regression test: status used to fail with 'Invalid block index' because
    file_size_from_node_change_id looked up linked-repo node IDs in the parent
    repository state instead of using the state carried on the NodeChange.
    """
    repo: Lore = new_lore_repo()

    # Create source repository with initial files
    source_repo = new_lore_repo()

    initial_file = "initial.txt"
    with source_repo.open_file(initial_file, "w+") as f:
        f.writelines(["initial content\n"])

    source_repo.stage(scan=True)
    source_repo.commit("Initial commit")
    source_repo.push()

    # Create a feature branch with additional content so the link update
    # actually changes the tree structure
    source_repo.branch_create("feature")

    feature_file = "feature-file.txt"
    with source_repo.open_file(feature_file, "w+") as f:
        f.writelines(["feature content\n"])

    with source_repo.open_file(initial_file, "w+") as f:
        f.writelines(["modified on feature\n"])

    source_repo.stage(scan=True)
    source_repo.commit("Feature commit")
    source_repo.push()

    # Switch back to main
    source_repo.branch_switch("main")

    # Add link to main repository pinned to main branch
    link_path = "linked"
    repo.link_add(link_path, source_repo.get_id(), "/", pin="main@LATEST")

    repo.commit("Add link")
    repo.push()

    # Update the link to the feature branch (stages a link change)
    repo.link_update(link_path, pin="feature@LATEST")

    # This status call used to fail with "Invalid block index" because
    # the diff recursed into the link and produced NodeChange entries with
    # node IDs from the linked repo, but status tried to look them up in
    # the parent repo's state.
    output = repo.status()
    assert "Changes staged for commit" in output, (
        "Status should show staged link change"
    )
    assert link_path in output, "Link path should appear in staged status"

    # Also verify --unstaged works (the original repro scenario)
    output_unstaged = repo.status(unstaged=True)
    assert link_path in output_unstaged, (
        "Link path should appear in unstaged status output"
    )

    # Commit and verify clean status
    repo.commit("Update link to feature branch")
    post_commit = repo.status()
    assert "Changes staged for commit" not in post_commit, (
        "Status should be clean after commit"
    )


@pytest.mark.smoke
def test_link_unchanged_commit(new_lore_repo):
    """Test that links are not committed when their content hasn't changed."""
    repo: Lore = new_lore_repo()

    # Create source repository with initial files
    source_text_file = "source-file.txt"
    source_subdir_file = "source/subdir/file.txt"

    with repo.open_file(source_text_file, "w+") as output_file:
        output_file.writelines(["initial source content\n"])

    repo.make_dirs(os.path.dirname(source_subdir_file))
    with repo.open_file(source_subdir_file, "w+") as output_file:
        output_file.writelines(["initial source subdir content\n"])

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    # Create link repository with initial files
    link_repo = new_lore_repo()
    link_file1 = "link-file1.txt"
    link_subdir_file = "linkdir/link-file2.txt"

    with link_repo.open_file(link_file1, "w+") as output_file:
        output_file.writelines(["initial link content 1\n"])

    link_repo.make_dirs(os.path.dirname(link_subdir_file))
    with link_repo.open_file(link_subdir_file, "w+") as output_file:
        output_file.writelines(["initial link content 2\n"])

    link_repo.stage(scan=True)
    link_repo.commit()
    link_repo.push()

    # Add the link to the source repository
    link_relative_path = "linked"
    repo.link_add(link_relative_path, link_repo.get_id(), "/")

    expected_link_file1 = "linked/link-file1.txt"
    expected_link_subdir_file = "linked/linkdir/link-file2.txt"

    # Verify initial link files are present
    assert repo.compare_file(repo, expected_link_file1)
    assert repo.compare_file(repo, expected_link_subdir_file)

    # Commit the initial link setup
    repo.commit()
    repo.push()

    # Modify only source repository files (not linked content)
    with repo.open_file(source_text_file, "w+") as output_file:
        output_file.writelines(["modified source content\n"])

    new_source_file = "new-source-file.txt"
    with repo.open_file(new_source_file, "w+") as output_file:
        output_file.writelines(["new source file content\n"])

    # Stage and commit changes to source repository only
    output = repo.stage(scan=True)
    assert "2 files" in output, "Expected 2 files to be staged"

    # Commit the source repository changes
    output = repo.commit("Modify source files only", debug=True)
    assert "Commit succeeded" in output, "Commit should succeed"
    assert "Before committing link node" not in output, "Expected no link changes"

    # Now modify link content and verify link content is committed
    with repo.open_file(expected_link_file1, "w+") as output_file:
        output_file.writelines(["modified link content\n"])

    repo.stage(scan=True)
    commit_output = repo.commit("Modify link content", debug=True)

    assert "Before committing link node" in commit_output, (
        "Link should have changed when content was modified"
    )

    # More source-only changes
    final_source_file = "final-source.txt"
    with repo.open_file(final_source_file, "w+") as output_file:
        output_file.writelines(["final source content\n"])

    repo.stage(scan=True)
    commit_output = repo.commit("Final source changes", debug=True)

    assert "Before committing link node" not in commit_output, (
        "Expected link unchanged message after link content was modified"
    )


@pytest.mark.smoke
def test_link_with_url(new_lore_repo):
    """Test link add functionality with repository URLs"""
    repo: Lore = new_lore_repo()

    # Create a repository to link to
    link_repo = new_lore_repo()

    # Generate content in the link repository
    link_file = "data/content.txt"
    link_repo.make_dirs(os.path.dirname(link_file))
    with link_repo.open_file(link_file, "w+") as output_file:
        output_file.writelines(["Content from linked repository\n"])

    link_repo.stage(scan=True)
    link_repo.commit("Initial content")
    link_repo.push()

    # Get repository details for URL construction
    link_repo_id = link_repo.get_id()
    link_repo_remote_url = link_repo.remote_path

    # Extract base URL (remove repository name from the end)
    base_url = link_repo.remote

    # Test 1: Use full remote URL to the link repository
    full_url_link_path = "link_by_full_url"
    repo.link_add(full_url_link_path, link_repo_remote_url, "data")

    # Verify the link was created and files are accessible
    expected_file_full_url = os.path.join(full_url_link_path, "content.txt")
    assert repo.compare_file(repo, expected_file_full_url), (
        "Link created with full URL should have accessible files"
    )

    # Test 2: Use remote URL with repo ID appended instead of repository name
    url_with_repo_id = f"{base_url}/{link_repo_id}"
    link_path_repo_url = "link_by_repo_id_url"
    repo.link_add(link_path_repo_url, url_with_repo_id, "data")

    # Verify the link was created and files are accessible
    expected_file_repo_url = os.path.join(link_path_repo_url, "content.txt")
    assert repo.compare_file(repo, expected_file_repo_url), (
        "Link created with URL+repo ID should have accessible files"
    )

    # Stage and commit the links to verify they work properly
    repo.stage(scan=True)
    repo.commit("Added links using URLs")
    repo.push()

    # Verify both links appear in link list
    link_list_output = repo.link_list()
    assert link_list_output.count(link_repo_id) == 2, (
        "Repository should appear twice in link list (once for each URL method)"
    )

    # Verify both link paths exist
    assert full_url_link_path in link_list_output, (
        "Full URL link path should be in link list"
    )
    assert link_path_repo_url in link_list_output, (
        "repo ID URL link path should be in link list"
    )

    logger.info("URL-based linking functionality validated successfully")


@pytest.mark.smoke
def test_link_add_remove(new_lore_repo):
    """Test adding a link and then removing it without committing in between."""
    # Create main repository
    main_repo: Lore = new_lore_repo()

    # Create initial content in main repo
    main_file = "main-content.txt"
    with main_repo.open_file(main_file, "w+") as output_file:
        output_file.writelines(["Initial main repository content\n"])

    main_repo.stage(scan=True)
    main_repo.commit("Initial main repo content")
    main_repo.push()

    # Create source repository to link
    source_repo: Lore = new_lore_repo()

    # Add content to source repository
    source_file1 = "data/source-file1.txt"
    source_file2 = "config/source-file2.txt"

    source_repo.make_dirs("data")
    source_repo.make_dirs("config")

    with source_repo.open_file(source_file1, "w+") as output_file:
        output_file.writelines(["Source repository file 1 content\n"])

    with source_repo.open_file(source_file2, "w+") as output_file:
        output_file.writelines(["Source repository file 2 content\n"])

    source_repo.stage(scan=True)
    source_repo.commit("Initial source repo content")
    source_repo.push()

    # Uncommitted link: add, remove, re-add without any commit
    link_path = "linked-source"
    expected_file1 = f"{link_path}/{source_file1}"
    expected_file2 = f"{link_path}/{source_file2}"

    main_repo.link_add(link_path, source_repo.get_id(), "/")
    assert main_repo.file_exists(expected_file1), (
        "Linked file 1 should be accessible after initial add"
    )

    main_repo.link_remove(link_path)
    assert not main_repo.file_exists(expected_file1), (
        "Linked file 1 should not be accessible after remove"
    )

    main_repo.link_add(link_path, source_repo.get_id(), "/")
    assert main_repo.file_exists(expected_file1), (
        "Linked file 1 should be accessible after re-add"
    )
    assert main_repo.file_exists(expected_file2), (
        "Linked file 2 should be accessible after re-add"
    )

    link_list_after_readd = main_repo.link_list()
    assert source_repo.get_id() in link_list_after_readd, (
        "Source repo should appear in link list after re-add"
    )
    assert link_path in link_list_after_readd, (
        "Link path should appear in link list after re-add"
    )

    # Remove again so the rest of the test starts from a clean slate
    main_repo.link_remove(link_path)

    # Uncommitted link: add then unstage
    main_repo.link_add(link_path, source_repo.get_id(), "/")
    assert main_repo.file_exists(expected_file1), (
        "Linked file 1 should be accessible after add for unstage test"
    )

    link_list_before_unstage = main_repo.link_list()
    assert source_repo.get_id() in link_list_before_unstage, (
        "Source repo should appear in link list before unstage"
    )

    main_repo.unstage(link_path)

    link_list_after_unstage_add = main_repo.link_list()
    assert source_repo.get_id() not in link_list_after_unstage_add, (
        "Source repo should not appear in link list after unstaging a staged-add link"
    )
    assert link_path not in link_list_after_unstage_add, (
        "Link path should not appear in link list after unstaging a staged-add link"
    )

    # Unstaging a staged-add link must remove the cloned files from the
    # filesystem.
    link_abs_path = os.path.join(main_repo.path, link_path)
    assert not os.path.exists(link_abs_path), (
        "Unstage of a staged-add link must remove the cloned link directory"
    )

    # Original test: add link without committing
    main_repo.link_add(link_path, source_repo.get_id(), "/")

    # Verify link was added (files should be accessible)

    assert main_repo.file_exists(expected_file1), "Linked file 1 should be accessible"
    assert main_repo.file_exists(expected_file2), "Linked file 2 should be accessible"
    assert main_repo.compare_file(main_repo, expected_file1), (
        "Linked file 1 content should match"
    )
    assert main_repo.compare_file(main_repo, expected_file2), (
        "Linked file 2 content should match"
    )

    # Verify link appears in link list
    link_list_after_add = main_repo.link_list()
    assert source_repo.get_id() in link_list_after_add, (
        "Source repo should appear in link list after add"
    )
    assert link_path in link_list_after_add, "Link path should appear in link list"

    # Check status using JSON - should show staged changes for link directory only
    status_output_after_add = main_repo.status(json=True)
    status_entries_after_add = parse_status_json(status_output_after_add)

    # Verify link directory is staged
    staged_paths = [entry.get("path", "") for entry in status_entries_after_add]
    assert link_path in staged_paths, "Link directory should be staged after link add"

    # Verify individual link files are not shown in staged changes
    assert expected_file1 not in staged_paths, (
        "Individual linked files should not appear in staged changes"
    )
    assert expected_file2 not in staged_paths, (
        "Individual linked files should not appear in staged changes"
    )

    # Verify no unstaged changes (status --unstaged returns both staged and unstaged,
    # so filter to only truly unstaged entries)
    unstaged_output_after_add = main_repo.status(json=True, unstaged=True)
    unstaged_entries_after_add = [
        entry
        for entry in parse_status_json(unstaged_output_after_add)
        if not entry.get("flagStaged", False)
    ]
    assert len(unstaged_entries_after_add) == 0, (
        "Should have no unstaged changes after link add"
    )

    # Remove the link WITHOUT committing first
    remove_output = main_repo.link_remove(link_path)
    assert "Removed link" in remove_output, "Link removal should succeed"

    # Verify link directory still exists but contents are gone
    assert main_repo.path_exists(link_path), (
        "Link directory should still exist after removal"
    )
    assert not main_repo.file_exists(expected_file1), (
        "Linked file 1 should no longer be accessible after link removal"
    )
    assert not main_repo.file_exists(expected_file2), (
        "Linked file 2 should no longer be accessible after link removal"
    )

    # Verify link no longer appears in link list
    link_list_after_remove = main_repo.link_list()
    assert source_repo.get_id() not in link_list_after_remove, (
        "Source repo should not appear in link list after removal"
    )
    assert link_path not in link_list_after_remove, (
        "Link path should not appear in link list after removal"
    )

    # Check status using JSON - should be clean after link add/remove cycle
    staged_output_after_remove = main_repo.status(json=True)
    staged_entries_after_remove = parse_status_json(staged_output_after_remove)

    unstaged_output_after_remove = main_repo.status(json=True, unstaged=True)
    all_entries_after_remove = parse_status_json(unstaged_output_after_remove)
    unstaged_entries_after_remove = [
        entry
        for entry in all_entries_after_remove
        if not entry.get("flagStaged", False)
    ]

    # Should have no staged changes
    assert len(staged_entries_after_remove) == 0, (
        "Should have no staged changes after link add/remove cycle"
    )

    # Link directory should appear in unstaged status
    unstaged_paths = [entry.get("path", "") for entry in unstaged_entries_after_remove]
    assert link_path in unstaged_paths, (
        "Link directory should be unstaged after removal"
    )

    # Verify main repository content is still intact
    assert main_repo.file_exists(main_file), (
        "Original main repo file should still exist"
    )
    assert main_repo.compare_file(main_repo, main_file), (
        "Original main repo file content should be unchanged"
    )

    # Commit should report no changes (unstaged empty directory doesn't prevent commit)
    commit_output = main_repo.commit("No changes expected", debug=True, check=False)
    assert (
        "nothing to commit" in commit_output.lower()
        or "no changes" in commit_output.lower()
    ), "Should have nothing to commit after add/remove cycle"

    # Re-add the link to verify it can be added again and works correctly
    main_repo.link_add(link_path, source_repo.get_id(), "/")

    # Verify link works again (files should be accessible)
    assert main_repo.file_exists(expected_file1), (
        "Linked file 1 should be accessible after re-adding link"
    )
    assert main_repo.file_exists(expected_file2), (
        "Linked file 2 should be accessible after re-adding link"
    )
    assert main_repo.compare_file(main_repo, expected_file1), (
        "Linked file 1 content should match after re-adding"
    )
    assert main_repo.compare_file(main_repo, expected_file2), (
        "Linked file 2 content should match after re-adding"
    )

    # Verify link appears in link list again
    link_list_after_readd = main_repo.link_list()
    assert source_repo.get_id() in link_list_after_readd, (
        "Source repo should appear in link list after re-adding"
    )
    assert link_path in link_list_after_readd, (
        "Link path should appear in link list after re-adding"
    )

    # Final commit to clean up
    main_repo.commit("Re-added link successfully")
    main_repo.push()

    # Removed committed link without committing again, then re-add
    remove_output_committed = main_repo.link_remove(link_path)
    assert "Removed link" in remove_output_committed, (
        "Committed link removal should succeed"
    )

    link_list_after_committed_remove = main_repo.link_list()
    assert source_repo.get_id() not in link_list_after_committed_remove, (
        "Source repo should not appear in link list after committed link removal"
    )
    assert link_path not in link_list_after_committed_remove, (
        "Link path should not appear in link list after committed link removal"
    )

    assert not main_repo.file_exists(expected_file1), (
        "Linked file 1 should not be accessible after committed link removal"
    )
    assert not main_repo.file_exists(expected_file2), (
        "Linked file 2 should not be accessible after committed link removal"
    )

    # Re-add the same link without committing the removal first
    main_repo.link_add(link_path, source_repo.get_id(), "/")

    assert main_repo.file_exists(expected_file1), (
        "Linked file 1 should be accessible after re-adding committed link"
    )
    assert main_repo.file_exists(expected_file2), (
        "Linked file 2 should be accessible after re-adding committed link"
    )
    assert main_repo.compare_file(main_repo, expected_file1), (
        "Linked file 1 content should match after re-adding committed link"
    )
    assert main_repo.compare_file(main_repo, expected_file2), (
        "Linked file 2 content should match after re-adding committed link"
    )

    link_list_after_committed_readd = main_repo.link_list()
    assert source_repo.get_id() in link_list_after_committed_readd, (
        "Source repo should appear in link list after re-adding committed link"
    )
    assert link_path in link_list_after_committed_readd, (
        "Link path should appear in link list after re-adding committed link"
    )

    main_repo.push()

    # --- Committed link: remove then unstage to restore ---
    main_repo.link_remove(link_path)

    # Verify link is gone — registry and on-disk content
    link_list_after_remove_for_unstage = main_repo.link_list()
    assert source_repo.get_id() not in link_list_after_remove_for_unstage, (
        "Source repo should not appear in link list after removal for unstage test"
    )
    assert not main_repo.file_exists(expected_file1), (
        "Linked file 1 should not exist after link_remove"
    )
    assert not main_repo.file_exists(expected_file2), (
        "Linked file 2 should not exist after link_remove"
    )

    # Unstage the removal to restore the link
    main_repo.unstage(link_path)

    # Verify link is fully restored — registry entry, file access, and on-disk
    # content.
    link_list_after_unstage = main_repo.link_list()
    assert source_repo.get_id() in link_list_after_unstage, (
        "Source repo should appear in link list after unstaging removal"
    )
    assert link_path in link_list_after_unstage, (
        "Link path should appear in link list after unstaging removal"
    )
    assert main_repo.file_exists(expected_file1), (
        "Linked file 1 should be re-materialized after unstaging removal"
    )
    assert main_repo.file_exists(expected_file2), (
        "Linked file 2 should be re-materialized after unstaging removal"
    )
    assert main_repo.compare_file(main_repo, expected_file1), (
        "Re-materialized file 1 content should match"
    )
    assert main_repo.compare_file(main_repo, expected_file2), (
        "Re-materialized file 2 content should match"
    )


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
    assert sync_repo.compare_file(repo, sync_added_file), "Sync: Added file should match"
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
    import re

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
def test_link_validation_checks(new_lore_repo):
    """Test new link validation checks: nested links, file paths, and directories with children."""
    # Create main repository
    main_repo: Lore = new_lore_repo()

    # Create first repository to be linked
    repo_to_link: Lore = new_lore_repo()

    # Create second repository for nested link test
    repo_to_nest: Lore = new_lore_repo()

    # Setup content in all three repositories
    # Main repo: create subdirectory with file
    main_subdir = "main_folder"
    main_file = "main_folder/main_file.txt"

    main_repo.make_dirs(main_subdir)
    with main_repo.open_file(main_file, "w+") as output_file:
        output_file.writelines(["Main repository file content\n"])

    main_repo.stage(scan=True)
    main_repo.commit("Initial main repo content")
    main_repo.push()

    # First link repo: create subdirectory with file
    link_subdir = "link_folder"
    link_file = "link_folder/link_file.txt"

    repo_to_link.make_dirs(link_subdir)
    with repo_to_link.open_file(link_file, "w+") as output_file:
        output_file.writelines(["First link repository file content\n"])

    repo_to_link.stage(scan=True)
    repo_to_link.commit("Initial link repo content")
    repo_to_link.push()

    # Second link repo: create subdirectory with file
    nest_subdir = "nest_folder"
    nest_file = "nest_folder/nest_file.txt"

    repo_to_nest.make_dirs(nest_subdir)
    with repo_to_nest.open_file(nest_file, "w+") as output_file:
        output_file.writelines(["Second link repository file content\n"])

    repo_to_nest.stage(scan=True)
    repo_to_nest.commit("Initial nest repo content")
    repo_to_nest.push()

    # Delete the directory from filesystem
    logger.info("Deleting directory from filesystem")
    main_dir_path = os.path.join(main_repo.path, main_subdir)
    shutil.rmtree(main_dir_path)

    # Test 1: Try to add link to where the file was before (with same name as file) - should fail
    logger.info("Testing link add to file path - should fail")
    try:
        main_repo.link_add(main_file, repo_to_link.get_id(), "/")
        assert False, "Link add should have failed for file path"
    except (PathExistLinkError, Exception) as e:
        if isinstance(e, PathExistLinkError):
            logger.info("Correctly caught PathExistLinkError")
        else:
            error_msg = str(e)
            # Should fail because trying to link to a file path while Lore state still has the file
            assert any(
                keyword in error_msg.lower()
                for keyword in ["directory", "file", "path", "exist", "link", "already"]
            ), "Unexpected error message: %s" % error_msg
            logger.info("Correctly caught error for file path: %s", error_msg)

    # Test 2: Try to add link to deleted directory - should fail because Lore state directory still has children
    logger.info(
        "Testing link add to deleted directory with children still in state directory - should fail"
    )
    try:
        main_repo.link_add(main_subdir, repo_to_link.get_id(), "/")
        assert False, "Link add should have failed for directory with children in state"
    except (PathExistChildrenLinkError, Exception) as e:
        # Check if it's the expected error type or contains expected message
        if isinstance(e, PathExistChildrenLinkError):
            logger.info("Correctly caught LinkPathExistChildrenError")
        else:
            # Check stderr for expected error message
            error_msg = str(e)
            assert any(
                keyword in error_msg.lower()
                for keyword in ["children", "exist", "has", "directory"]
            ), "Unexpected error message: %s" % error_msg
            logger.info("Caught expected error for children check: %s", error_msg)

    # Stage and commit the deletions - this removes the directory from Lore's state
    main_repo.stage(scan=True)
    main_repo.commit("Remove files and folders to prepare for linking")
    main_repo.push()

    # Test 3: Now add link should work (directory no longer exists in Lore state)
    logger.info("Testing successful link add after committing directory deletion")
    main_repo.link_add(main_subdir, repo_to_link.get_id(), "/")

    # Verify link was added successfully
    expected_link_file = f"{main_subdir}/{link_file}"
    assert main_repo.file_exists(expected_link_file), (
        "Link file should exist after successful link add"
    )

    # Commit the successful link
    main_repo.commit("Successfully added link to empty directory")
    main_repo.push()

    # Test 4: Try to add second repository as link into the linked repository (nested link - should fail)
    logger.info("Testing nested link add - should fail")
    nested_link_path = f"{main_subdir}/nested_link"

    try:
        main_repo.link_add(nested_link_path, repo_to_nest.get_id(), "/")
        assert False, "Nested link add should have failed"
    except (NestedLinkError, Exception) as e:
        if isinstance(e, NestedLinkError):
            logger.info("Correctly caught NestedLinkError")
        else:
            error_msg = str(e)
            # Should fail because trying to add link inside another link
            assert any(
                keyword in error_msg.lower()
                for keyword in ["nested", "link", "repository", "different"]
            ), "Unexpected error message: %s" % error_msg
            logger.info("Correctly caught error for nested link: %s", error_msg)

    # Verify original link still works
    assert main_repo.file_exists(expected_link_file), (
        "Original link should still be functional"
    )

    logger.info("All link validation checks completed successfully")


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


@pytest.mark.smoke
def test_link_reset(new_lore_repo):
    """Test resetting files within linked repositories."""
    repo: Lore = new_lore_repo()

    # Create source repository
    source_file = "source-file.txt"
    with repo.open_file(source_file, "w+") as output_file:
        output_file.writelines(["source repository content\n"])

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    # Create link repository with multiple files
    link_repo = new_lore_repo()

    link_file1 = "file1.txt"
    link_file2 = "file2.txt"
    link_subdir_file = "subdir/file3.txt"
    link_deep_file = "deep/path/file4.txt"

    with link_repo.open_file(link_file1, "w+") as output_file:
        output_file.writelines(["link file 1 original\n"])

    with link_repo.open_file(link_file2, "w+") as output_file:
        output_file.writelines(["link file 2 original\n"])

    link_repo.make_dirs("subdir")
    with link_repo.open_file(link_subdir_file, "w+") as output_file:
        output_file.writelines(["link subdir file original\n"])

    link_repo.make_dirs(os.path.dirname(link_deep_file))
    with link_repo.open_file(link_deep_file, "w+") as output_file:
        output_file.writelines(["link deep file original\n"])

    link_repo.stage(scan=True)
    link_repo.commit()
    link_repo.push()

    # Add link to main repository
    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")

    expected_file1 = f"{link_path}/{link_file1}"
    expected_file2 = f"{link_path}/{link_file2}"
    expected_subdir_file = f"{link_path}/{link_subdir_file}"
    expected_deep_file = f"{link_path}/{link_deep_file}"

    # Verify link files exist
    assert repo.compare_file(repo, expected_file1)
    assert repo.compare_file(repo, expected_file2)
    assert repo.compare_file(repo, expected_subdir_file)
    assert repo.compare_file(repo, expected_deep_file)

    repo.commit()
    repo.push()

    # Test 1: Reset a single modified file inside a link
    with repo.open_file(expected_file1, "w+") as output_file:
        output_file.writelines(["MODIFIED file 1\n"])

    repo.reset(expected_file1)

    with repo.open_file(expected_file1, "r") as f:
        content = f.read()
        assert "MODIFIED" not in content, "File1 should be restored after reset"
        assert "link file 1 original" in content, "File1 should have original content"

    # Verify other files are unaffected
    with repo.open_file(expected_file2, "r") as f:
        assert "link file 2 original" in f.read(), "File2 should be unaffected"

    # Test 2: Reset a modified file in a link subdirectory
    with repo.open_file(expected_subdir_file, "w+") as output_file:
        output_file.writelines(["MODIFIED subdir file\n"])

    repo.reset(expected_subdir_file)

    with repo.open_file(expected_subdir_file, "r") as f:
        content = f.read()
        assert "MODIFIED" not in content, "Subdir file should be restored after reset"
        assert "link subdir file original" in content, (
            "Subdir file should have original content"
        )

    # Test 3: Reset an entire linked subdirectory
    with repo.open_file(expected_subdir_file, "w+") as output_file:
        output_file.writelines(["MODIFIED subdir file again\n"])

    untracked_file = f"{link_path}/subdir/untracked.txt"
    with repo.open_file(untracked_file, "w+") as output_file:
        output_file.writelines(["untracked file content\n"])

    repo.reset(f"{link_path}/subdir")

    with repo.open_file(expected_subdir_file, "r") as f:
        content = f.read()
        assert "MODIFIED" not in content, (
            "Subdir file should be restored after directory reset"
        )
        assert "link subdir file original" in content, (
            "Subdir file should have original content"
        )

    # Untracked file should still exist (purge is off)
    assert repo.file_exists(untracked_file), (
        "Untracked file should still exist without purge"
    )

    # Clean up untracked file
    repo.remove_file(untracked_file)

    # Test 4: Reset the entire link directory
    with repo.open_file(expected_file1, "w+") as output_file:
        output_file.writelines(["MODIFIED file 1 for test 4\n"])

    with repo.open_file(expected_file2, "w+") as output_file:
        output_file.writelines(["MODIFIED file 2 for test 4\n"])

    with repo.open_file(expected_deep_file, "w+") as output_file:
        output_file.writelines(["MODIFIED deep file for test 4\n"])

    repo.remove_file(expected_subdir_file)

    repo.reset(link_path)

    with repo.open_file(expected_file1, "r") as f:
        assert "link file 1 original" in f.read(), (
            "File1 should be restored after link reset"
        )

    with repo.open_file(expected_file2, "r") as f:
        assert "link file 2 original" in f.read(), (
            "File2 should be restored after link reset"
        )

    with repo.open_file(expected_deep_file, "r") as f:
        assert "link deep file original" in f.read(), (
            "Deep file should be restored after link reset"
        )

    assert repo.file_exists(expected_subdir_file), (
        "Deleted subdir file should be restored after link reset"
    )
    with repo.open_file(expected_subdir_file, "r") as f:
        assert "link subdir file original" in f.read(), (
            "Restored subdir file should have original content"
        )

    # Test 5: Reset entire repository traverses into links
    with repo.open_file(source_file, "w+") as output_file:
        output_file.writelines(["MODIFIED source file\n"])

    with repo.open_file(expected_file1, "w+") as output_file:
        output_file.writelines(["MODIFIED file 1 for test 5\n"])

    repo.reset(".")

    with repo.open_file(source_file, "r") as f:
        assert "source repository content" in f.read(), (
            "Source file should be restored after root reset"
        )

    with repo.open_file(expected_file1, "r") as f:
        assert "link file 1 original" in f.read(), (
            "Linked file should be restored after root reset"
        )

    # Test 6: Reset with purge removes untracked files in links
    untracked_link_file = f"{link_path}/untracked-purge.txt"
    with repo.open_file(untracked_link_file, "w+") as output_file:
        output_file.writelines(["untracked file for purge test\n"])

    assert repo.file_exists(untracked_link_file), (
        "Untracked file should exist before purge reset"
    )

    repo.reset(link_path, purge=True)

    assert not repo.file_exists(untracked_link_file), (
        "Untracked file should be deleted after purge reset"
    )

    # Test 7: Reset a staged file in a link returns error
    with repo.open_file(expected_file1, "w+") as output_file:
        output_file.writelines(["MODIFIED file 1 for staged test\n"])

    repo.stage(expected_file1)

    try:
        repo.reset(expected_file1)
        assert False, "Reset of staged file should have failed"
    except Exception:
        pass

    repo.unstage(expected_file1)

    # Test 8: Reset with multiple links
    second_link_repo = new_lore_repo()

    second_link_file = "second-file.txt"
    with second_link_repo.open_file(second_link_file, "w+") as output_file:
        output_file.writelines(["second link file original\n"])

    second_link_repo.stage(scan=True)
    second_link_repo.commit()
    second_link_repo.push()

    second_link_path = "other-link"
    repo.link_add(second_link_path, second_link_repo.get_id(), "/")

    expected_second_file = f"{second_link_path}/{second_link_file}"
    assert repo.compare_file(repo, expected_second_file)

    repo.commit()
    repo.push()

    # Modify files in both links
    with repo.open_file(expected_file1, "w+") as output_file:
        output_file.writelines(["MODIFIED first link file\n"])

    with repo.open_file(expected_second_file, "w+") as output_file:
        output_file.writelines(["MODIFIED second link file\n"])

    repo.reset(".")

    with repo.open_file(expected_file1, "r") as f:
        assert "link file 1 original" in f.read(), (
            "First link file should be restored after multi-link root reset"
        )

    with repo.open_file(expected_second_file, "r") as f:
        assert "second link file original" in f.read(), (
            "Second link file should be restored after multi-link root reset"
        )


# ---------------------------------------------------------------------------
# Helpers for the link-reset tests below.
# ---------------------------------------------------------------------------


def _assert_crr_clean(
    repo: Lore,
    expected_files_present: list[str],
    expected_files_absent: list[str],
    expected_link_registry: dict[str, bool],
):
    """Clean status, realized disk, registry-correct.

    `expected_link_registry` maps a needle (repo id or link path) to whether
    it should appear in `link_list()` output.
    """
    staged = parse_status_json(repo.status(json=True))
    assert staged == [], f"Expected clean staged status, got {staged}"
    unstaged = [
        e
        for e in parse_status_json(repo.status(json=True, unstaged=True))
        if not e.get("flagStaged", False)
    ]
    assert unstaged == [], f"Expected clean unstaged status, got {unstaged}"
    for path in expected_files_present:
        assert repo.file_exists(path), f"Expected file present: {path}"
    for path in expected_files_absent:
        assert not repo.file_exists(path), f"Expected file absent: {path}"
    link_list_output = repo.link_list()
    for needle, present in expected_link_registry.items():
        if present:
            assert needle in link_list_output, (
                f"Expected {needle!r} in link_list, got: {link_list_output}"
            )
        else:
            assert needle not in link_list_output, (
                f"Expected {needle!r} not in link_list, got: {link_list_output}"
            )


def _commit_initial_main(new_lore_repo, file_name: str) -> Lore:
    """Create a main repo with one committed file and push."""
    repo: Lore = new_lore_repo()
    with repo.open_file(file_name, "w+") as f:
        f.writelines(["main repo initial content\n"])
    repo.stage(scan=True)
    repo.commit("initial main")
    repo.push()
    return repo


def _make_link_source(
    new_lore_repo, file_paths: list[str]
) -> tuple[Lore, list[str]]:
    """Create a link source repo with the given files committed and pushed."""
    repo: Lore = new_lore_repo()
    for path in file_paths:
        directory = os.path.dirname(path)
        if directory:
            repo.make_dirs(directory)
        with repo.open_file(path, "w+") as f:
            f.writelines([f"link source content for {path}\n"])
    repo.stage(scan=True)
    repo.commit("initial source")
    repo.push()
    return repo, file_paths


# ---------------------------------------------------------------------------
# Reset for added/removed/updated links.
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_link_reset_staged_add(new_lore_repo):
    """`lore file reset` undoes a staged-add link: registry, on-disk content,
    and parent state are restored to pre-add."""
    main_repo = _commit_initial_main(new_lore_repo, "main.txt")
    source_repo, source_files = _make_link_source(
        new_lore_repo, ["a.txt", "nested/b.txt"]
    )

    link_path = "linked"
    expected = [f"{link_path}/{p}" for p in source_files]

    main_repo.link_add(link_path, source_repo.get_id(), "/")
    for p in expected:
        assert main_repo.file_exists(p), f"Pre-reset: expected {p} on disk"
    assert source_repo.get_id() in main_repo.link_list()

    main_repo.reset(link_path)

    _assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt"],
        expected_files_absent=expected,
        expected_link_registry={source_repo.get_id(): False, link_path: False},
    )


def test_link_reset_staged_add_reports_reset(new_lore_repo):
    """Resetting a staged-add link counts the node it reset in the summary."""
    main_repo = _commit_initial_main(new_lore_repo, "main.txt")
    source_repo, _ = _make_link_source(new_lore_repo, ["a.txt"])

    link_path = "linked"
    main_repo.link_add(link_path, source_repo.get_id(), "/")

    output = main_repo.reset(link_path, json=True)

    events = [json.loads(line) for line in output.splitlines() if line.strip()]
    reset_end = next(e for e in events if e.get("tagName") == "fileResetEnd")
    count = reset_end["data"]["count"]
    total = (
        count["directoryResetCount"]
        + count["directoryDeleteCount"]
        + count["fileResetCount"]
        + count["fileDeleteCount"]
    )
    assert total >= 1, f"Reset of a staged link must count the reset, got: {count}"


def test_link_reset_staged_add_into_existing_directory(new_lore_repo):
    """If the link path was a committed directory before `link add`, reset
    restores the empty directory rather than removing it."""
    main_repo = _commit_initial_main(new_lore_repo, "main.txt")
    source_repo, _ = _make_link_source(new_lore_repo, ["a.txt"])

    # Commit an empty directory at the link path so it predates the link add.
    link_path = "linked"
    main_repo.make_dirs(link_path)
    main_repo.stage(scan=True)
    main_repo.commit("add empty linked directory")
    main_repo.push()

    main_repo.link_add(link_path, source_repo.get_id(), "/")
    assert main_repo.file_exists(f"{link_path}/a.txt")

    main_repo.reset(link_path)

    _assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt"],
        expected_files_absent=[f"{link_path}/a.txt"],
        expected_link_registry={source_repo.get_id(): False},
    )
    assert main_repo.path_exists(link_path), (
        "Pre-existing committed directory must survive reset"
    )


def test_link_reset_staged_add_creates_parent_directories(new_lore_repo):
    """If `link add` had to auto-stage parent directories, reset removes
    both the link and the auto-staged parents."""
    main_repo = _commit_initial_main(new_lore_repo, "main.txt")
    source_repo, _ = _make_link_source(new_lore_repo, ["a.txt"])

    link_path = "deep/parent/chain/linked"
    main_repo.link_add(link_path, source_repo.get_id(), "/")

    staged_paths = {
        e["path"] for e in parse_status_json(main_repo.status(json=True))
    }
    assert any(p.startswith("deep") for p in staged_paths), (
        f"Auto-staged parents should be visible in pre-reset status, got: {staged_paths}"
    )

    main_repo.reset(link_path)

    _assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt"],
        expected_files_absent=[f"{link_path}/a.txt"],
        expected_link_registry={source_repo.get_id(): False},
    )
    assert not main_repo.path_exists("deep"), (
        "Parent chain that didn't exist before the link must be gone"
    )


@pytest.mark.smoke
def test_link_reset_staged_remove(new_lore_repo):
    """`lore file reset` of a staged-remove link restores the registry, the
    link node, and re-materializes content on disk."""
    main_repo = _commit_initial_main(new_lore_repo, "main.txt")
    source_repo, source_files = _make_link_source(
        new_lore_repo, ["a.txt", "nested/b.txt"]
    )

    link_path = "linked"
    expected = [f"{link_path}/{p}" for p in source_files]

    main_repo.link_add(link_path, source_repo.get_id(), "/")
    main_repo.commit("add link")
    main_repo.push()

    main_repo.link_remove(link_path)
    for p in expected:
        assert not main_repo.file_exists(p), (
            f"Pre-reset: expected {p} absent after link_remove"
        )

    main_repo.reset(link_path)

    _assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt", *expected],
        expected_files_absent=[],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )
    for p in expected:
        source_relative = p.removeprefix(f"{link_path}/")
        assert main_repo.compare_file(
            source_repo, p, other_file_path=source_relative
        ), f"Restored content of {p} must match source repo content"


@pytest.mark.smoke
def test_link_reset_staged_update(new_lore_repo):
    """`lore file reset` of a staged pin change restores the previous pin in
    the registry and re-realizes content from the previous pin."""
    main_repo = _commit_initial_main(new_lore_repo, "main.txt")

    source_repo: Lore = new_lore_repo()
    with source_repo.open_file("v1.txt", "w+") as f:
        f.writelines(["v1\n"])
    source_repo.stage(scan=True)
    source_repo.commit("v1")
    source_repo.push()

    source_repo.branch_create("feature")
    with source_repo.open_file("v2.txt", "w+") as f:
        f.writelines(["v2\n"])
    source_repo.stage(scan=True)
    source_repo.commit("v2")
    source_repo.push()

    link_path = "linked"
    main_repo.link_add(link_path, source_repo.get_id(), "/", pin="main@LATEST")
    main_repo.commit("add link")
    main_repo.push()

    pre_link_list = main_repo.link_list()
    assert main_repo.file_exists(f"{link_path}/v1.txt")
    assert not main_repo.file_exists(f"{link_path}/v2.txt")

    main_repo.link_update(link_path, pin="feature@LATEST")
    assert main_repo.file_exists(f"{link_path}/v2.txt"), (
        "Pre-reset: link_update should have realized v2.txt"
    )

    main_repo.reset(link_path)

    _assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt", f"{link_path}/v1.txt"],
        expected_files_absent=[f"{link_path}/v2.txt"],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )
    assert main_repo.link_list() == pre_link_list, (
        "Registry must match pre-update exactly (branch + signature)"
    )


def test_link_reset_root_handles_staged_link_changes(new_lore_repo):
    """Root reset (`reset(".")`) traverses into the link node and reverts
    a staged add/remove/update there."""
    # --- staged-add ---
    main_repo = _commit_initial_main(new_lore_repo, "main.txt")
    source_repo, _ = _make_link_source(new_lore_repo, ["a.txt"])
    link_path = "linked"
    main_repo.link_add(link_path, source_repo.get_id(), "/")
    main_repo.reset(".")
    _assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt"],
        expected_files_absent=[f"{link_path}/a.txt"],
        expected_link_registry={source_repo.get_id(): False},
    )

    # --- staged-remove ---
    main_repo.link_add(link_path, source_repo.get_id(), "/")
    main_repo.commit("add link")
    main_repo.push()
    main_repo.link_remove(link_path)
    main_repo.reset(".")
    _assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt", f"{link_path}/a.txt"],
        expected_files_absent=[],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )

    # --- staged-update ---
    source_repo.branch_create("feature")
    with source_repo.open_file("a.txt", "w+") as f:
        f.writelines(["feature a\n"])
    source_repo.stage(scan=True)
    source_repo.commit("feature a")
    source_repo.push()

    pre_link_list = main_repo.link_list()
    main_repo.link_update(link_path, pin="feature@LATEST")
    main_repo.reset(".")
    _assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt", f"{link_path}/a.txt"],
        expected_files_absent=[],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )
    assert main_repo.link_list() == pre_link_list, (
        "Registry must match pre-update after root reset"
    )


def test_link_reset_does_not_affect_unrelated_links(new_lore_repo):
    """Reset of one staged link change must not touch unrelated committed
    links."""
    main_repo = _commit_initial_main(new_lore_repo, "main.txt")
    source_repo_a, _ = _make_link_source(new_lore_repo, ["a.txt"])
    source_repo_b, _ = _make_link_source(new_lore_repo, ["b.txt"])

    link_a = "linkA"
    link_b = "linkB"

    main_repo.link_add(link_a, source_repo_a.get_id(), "/")
    main_repo.link_add(link_b, source_repo_b.get_id(), "/")
    main_repo.commit("add both links")
    main_repo.push()

    # Stage a remove on linkA only; reset only that. linkB untouched.
    main_repo.link_remove(link_a)
    main_repo.reset(link_a)

    _assert_crr_clean(
        main_repo,
        expected_files_present=[
            "main.txt",
            f"{link_a}/a.txt",
            f"{link_b}/b.txt",
        ],
        expected_files_absent=[],
        expected_link_registry={
            source_repo_a.get_id(): True,
            link_a: True,
            source_repo_b.get_id(): True,
            link_b: True,
        },
    )


def test_link_reset_idempotent(new_lore_repo):
    """Running reset twice on the same staged-add link change is a no-op
    on the second call."""
    main_repo = _commit_initial_main(new_lore_repo, "main.txt")
    source_repo, _ = _make_link_source(new_lore_repo, ["a.txt"])

    link_path = "linked"
    main_repo.link_add(link_path, source_repo.get_id(), "/")
    main_repo.reset(link_path)
    # Second reset is a no-op (path no longer exists).
    main_repo.reset(link_path)

    _assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt"],
        expected_files_absent=[f"{link_path}/a.txt"],
        expected_link_registry={source_repo.get_id(): False},
    )


# ---------------------------------------------------------------------------
# Links crossing branches: the link registry travels with the merge.
#
# A link lives both as a link node in the tree and as a `LinkReference` in the
# state's link registry. A merge that carries the node has to carry the entry,
# or the link lands in a state whose registry never mentions it.
# ---------------------------------------------------------------------------


def _link_added_on_feature_branch(
    new_lore_repo, link_path: str, **link_add_options
) -> tuple[Lore, Lore]:
    """Parent repo on `main`, link added and committed on `feature-branch` only."""
    repo = _commit_initial_main(new_lore_repo, "main-file.txt")
    source_repo, _ = _make_link_source(new_lore_repo, ["link-file.txt"])

    repo.branch_create("feature-branch")
    repo.link_add(link_path, source_repo.get_id(), "/", **link_add_options)
    repo.commit("Add link on feature branch")
    repo.push()

    return repo, source_repo


@pytest.mark.smoke
def test_link_add_on_branch_merge_start(new_lore_repo):
    """A link added on a branch survives `branch merge start` into main."""
    link_path = "linked/repo"
    repo, source_repo = _link_added_on_feature_branch(new_lore_repo, link_path)

    repo.branch_switch("main")
    assert not repo.path_exists(link_path), (
        "Link path should not exist on main before the merge"
    )

    repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    repo.push()

    _assert_crr_clean(
        repo,
        expected_files_present=["main-file.txt", f"{link_path}/link-file.txt"],
        expected_files_absent=[],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )

    # The merged link tracks the branch it landed on, not the source branch.
    link_output = repo.link_list()
    assert re.search(
        rf"Link\s+{source_repo.get_id()}.*?Branch:\s+main", link_output, re.DOTALL
    ), f"Merged link should track 'main'.\nGot: {link_output}"

    # A fresh clone of main resolves the same link.
    clone = repo.clone()
    clone_link_output = clone.link_list()
    assert source_repo.get_id() in clone_link_output, (
        f"Merged link should be listed in a fresh clone.\nGot: {clone_link_output}"
    )
    assert clone.file_exists(f"{link_path}/link-file.txt"), (
        "Linked content should be cloned from the merged revision"
    )


@pytest.mark.smoke
def test_link_add_on_branch_merge_into(new_lore_repo):
    """A link added on a branch survives `branch merge into` main."""
    link_path = "linked/repo"
    repo, source_repo = _link_added_on_feature_branch(new_lore_repo, link_path)

    repo.branch_merge_into("main", message="Merge feature-branch into main")
    repo.branch_switch("main")

    _assert_crr_clean(
        repo,
        expected_files_present=["main-file.txt", f"{link_path}/link-file.txt"],
        expected_files_absent=[],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )


@pytest.mark.smoke
def test_link_add_on_branch_merge_start_link_usable(new_lore_repo):
    """A merged-in link still stages and commits through its mount path.

    Without a registry entry the link node is unreachable: staging under the
    mount path fails with `Link not found`.
    """
    link_path = "linked/repo"
    repo, source_repo = _link_added_on_feature_branch(new_lore_repo, link_path)

    repo.branch_switch("main")
    repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    repo.push()

    pin_before = re.search(
        rf"{source_repo.get_id()}.*?Revision:\s*(\w+)", repo.link_list(), re.DOTALL
    )
    assert pin_before, "Merged link should have a pinned revision"

    linked_file = f"{link_path}/link-file.txt"
    with repo.open_file(linked_file, "w+") as f:
        f.writelines(["changed through the mount path after the merge\n"])

    repo.stage(scan=True)
    output = repo.commit("Change inside the merged link")
    assert "Commit succeeded" in output, f"Commit did not succeed - Got:\n{output}"
    repo.push()

    with repo.open_file(linked_file, "r") as f:
        assert "changed through the mount path after the merge" in f.read(), (
            "Committed content should remain on disk"
        )

    pin_after = re.search(
        rf"{source_repo.get_id()}.*?Revision:\s*(\w+)", repo.link_list(), re.DOTALL
    )
    assert pin_after, "Link should still be in the registry after committing into it"
    assert pin_after.group(1) != pin_before.group(1), (
        "Committing into the merged link should advance its pin"
    )

    _assert_crr_clean(
        repo,
        expected_files_present=[linked_file],
        expected_files_absent=[],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )


@pytest.mark.smoke
def test_link_add_on_branch_merge_start_preserves_link_flags(new_lore_repo):
    """A merged-in fixed link keeps its `DisableAutoFollow` flag and branch."""
    link_path = "linked/fixed"
    repo, source_repo = _link_added_on_feature_branch(
        new_lore_repo, link_path, disable_branching=True
    )

    repo.branch_switch("main")
    repo.branch_merge_start("feature-branch", message="Merge feature-branch")
    repo.push()

    link_output = repo.link_list()
    assert re.search(
        rf"Link\s+{source_repo.get_id()}.*?Flags:\s+DisableAutoFollow \(0x1\)",
        link_output,
        re.DOTALL,
    ), f"Merged link should keep the DisableAutoFollow flag.\nGot: {link_output}"
    assert re.search(
        rf"Link\s+{source_repo.get_id()}.*?Branch:\s+main", link_output, re.DOTALL
    ), f"Merged fixed link should keep tracking 'main'.\nGot: {link_output}"


_DEFAULT_LINK_MOUNT = "vendor/lib"
_DEFAULT_PARENT_FILE = "README.txt"


def _make_parent_with_link(
    new_lore_repo,
    link_path: str = _DEFAULT_LINK_MOUNT,
    link_files: dict | None = None,
    parent_files: dict | None = None,
    **link_add_kwargs,
):
    """Returns (parent_repo, link_repo) with the link committed and pushed."""
    parent_repo: Lore = new_lore_repo()
    link_repo: Lore = new_lore_repo()

    parent_repo.write_commit_push(
        "Baseline",
        parent_files or {_DEFAULT_PARENT_FILE: "baseline\n"},
    )
    link_repo.write_commit_push(
        "Initial linked content", link_files or {"linked.txt": "linked content\n"}
    )

    parent_repo.link_add(link_path, link_repo.get_id(), "/", **link_add_kwargs)
    parent_repo.commit("Add link")
    parent_repo.push()
    return parent_repo, link_repo


@pytest.mark.smoke
def test_link_remove_keeps_uncommitted_local_edit(new_lore_repo):
    link_path = "libs/shared"
    edited_file = f"{link_path}/deep/inner.txt"
    parent_repo, _link_repo = _make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    parent_repo.write_files({edited_file: "locally edited\n"})

    with pytest.raises(LocalModificationsError):
        parent_repo.link_remove(link_path)

    assert parent_repo.file_exists(edited_file)
    with parent_repo.open_file(edited_file) as f:
        assert f.read() == "locally edited\n"
    assert link_path in parent_repo.link_list(), (
        "A refused remove must leave the link in place"
    )


@pytest.mark.smoke
def test_link_remove_keeps_staged_local_edit(new_lore_repo):
    """A staged edit is still uncommitted, and unlinking would take it with it."""
    link_path = "libs/shared"
    edited_file = f"{link_path}/deep/inner.txt"
    parent_repo, _link_repo = _make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    parent_repo.write_files({edited_file: "locally edited\n"})
    parent_repo.stage(edited_file)

    with pytest.raises(LocalModificationsError):
        parent_repo.link_remove(link_path)

    with parent_repo.open_file(edited_file) as f:
        assert f.read() == "locally edited\n"


@pytest.mark.smoke
def test_link_remove_keeps_untracked_file_under_mount(new_lore_repo):
    """Unlinking deletes the mount wholesale, untracked files included."""
    link_path = "libs/shared"
    untracked_file = f"{link_path}/deep/scratch.txt"
    parent_repo, _link_repo = _make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    parent_repo.write_files({untracked_file: "not tracked yet\n"})

    with pytest.raises(LocalModificationsError):
        parent_repo.link_remove(link_path)

    assert parent_repo.file_exists(untracked_file)


@pytest.mark.smoke
def test_link_remove_refuses_when_path_case_differs(new_lore_repo):
    """Node lookup is case-insensitive, so the guard has to be too."""
    link_path = "libs/shared"
    edited_file = f"{link_path}/deep/inner.txt"
    parent_repo, _link_repo = _make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    parent_repo.write_files({edited_file: "locally edited\n"})

    with pytest.raises(LocalModificationsError):
        parent_repo.link_remove("libs/SHARED")

    with parent_repo.open_file(edited_file) as f:
        assert f.read() == "locally edited\n"


@pytest.mark.smoke
def test_link_remove_force_discards_local_edit(new_lore_repo):
    link_path = "libs/shared"
    edited_file = f"{link_path}/deep/inner.txt"
    parent_repo, _link_repo = _make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    parent_repo.write_files({edited_file: "locally edited\n"})

    parent_repo.link_remove(link_path, force=True)

    assert not parent_repo.file_exists(edited_file), (
        "A forced remove must discard the edited file with the mount"
    )
    assert "No links found in this repository" in parent_repo.link_list()


@pytest.mark.smoke
def test_link_remove_ignores_changes_outside_the_mount(new_lore_repo):
    """The guard scans the whole working tree, so it must filter to the mount."""
    link_path = "libs/shared"
    parent_repo, _link_repo = _make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    parent_repo.write_files(
        {
            "README.txt": "edited outside the mount\n",
            "libs/shared-extra/sibling.txt": "a path sharing the mount prefix\n",
        }
    )

    parent_repo.link_remove(link_path)

    assert "No links found in this repository" in parent_repo.link_list()
    assert parent_repo.file_exists("libs/shared-extra/sibling.txt")


@pytest.mark.smoke
def test_link_remove_keeps_ignored_file_under_mount(new_lore_repo):
    """Ignoring a file says it is not Lore's to track, not that it is worthless
    - a key or a local config is exactly the sort of thing that gets ignored,
    and removal takes the whole mount with it.
    """
    link_path = "libs/shared"
    ignored_file = f"{link_path}/deep/local.key"
    parent_repo, _link_repo = _make_parent_with_link(
        new_lore_repo, link_path, {"deep/inner.txt": "pinned content\n"}
    )

    with parent_repo.open_file(parent_repo.ignore_file(), "w+") as ignore_file:
        ignore_file.write("*.key\n")
    parent_repo.write_commit_push("Add ignore file", {"placeholder.txt": "x\n"})
    parent_repo.write_files({ignored_file: "secret\n"})

    with pytest.raises(LocalModificationsError):
        parent_repo.link_remove(link_path)

    assert parent_repo.file_exists(ignored_file), (
        "An ignored file must survive a refused remove"
    )


@pytest.mark.smoke
def test_link_remove_on_branch_merge_start(new_lore_repo):
    """A link removed on a branch is gone from the registry after merging."""
    repo = _commit_initial_main(new_lore_repo, "main-file.txt")
    source_repo, _ = _make_link_source(new_lore_repo, ["link-file.txt"])

    link_path = "linked/repo"
    repo.link_add(link_path, source_repo.get_id(), "/")
    repo.commit("Add link on main")
    repo.push()

    # The link is removed only on the feature branch.
    repo.branch_create("feature-branch")
    repo.link_remove(link_path)
    repo.commit("Remove link on feature branch")
    repo.push()

    repo.branch_switch("main")
    assert repo.file_exists(f"{link_path}/link-file.txt"), (
        "Linked content should still be on main before the merge"
    )

    repo.branch_merge_start("feature-branch", message="Merge link removal")
    repo.push()

    _assert_crr_clean(
        repo,
        expected_files_present=["main-file.txt"],
        expected_files_absent=[f"{link_path}/link-file.txt"],
        expected_link_registry={source_repo.get_id(): False, link_path: False},
    )


def _link_pin(repo: Lore, source_id: str) -> str:
    """The revision `source_id` is pinned at, read from `link list`."""
    output = repo.link_list()
    match = re.search(rf"Link\s+{source_id}.*?Revision:\s*(\w+)", output, re.DOTALL)
    assert match, f"No pin for {source_id} in link list:\n{output}"
    return match.group(1)


def _fixed_link_pinned_on_main(new_lore_repo, link_path: str) -> tuple[Lore, Lore, str]:
    """Parent on main with a fixed link, and a newer revision to move it to.

    The link is added with `--disable-branching`, so its registry entry names a
    branch of the linked repository and its pin is plain revision data that a
    merge has to carry. The linked repository then gets a second revision, so
    the pin can move with nothing staged through the mount path.
    """
    repo = _commit_initial_main(new_lore_repo, "main-file.txt")
    source_repo, _ = _make_link_source(new_lore_repo, ["link-file.txt"])

    repo.link_add(link_path, source_repo.get_id(), "/", disable_branching=True)
    repo.commit("Add fixed link on main")
    repo.push()

    with source_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link source revision two\n"])
    source_repo.stage(scan=True)
    source_repo.commit("Second source revision")
    source_repo.push()

    return repo, source_repo, _link_pin(repo, source_repo.get_id())


@pytest.mark.smoke
def test_link_pin_update_on_branch_merge_start(new_lore_repo):
    """A pin-only `link update` on a branch survives `branch merge start`."""
    link_path = "linked/repo"
    repo, source_repo, pin_base = _fixed_link_pinned_on_main(new_lore_repo, link_path)
    linked_file = f"{link_path}/link-file.txt"

    repo.branch_create("feature-branch")
    repo.link_update(link_path)
    repo.commit("Move link pin on feature branch")
    repo.push()

    pin_feature = _link_pin(repo, source_repo.get_id())
    assert pin_feature != pin_base, "link update should have moved the pin"

    repo.branch_switch("main")
    assert _link_pin(repo, source_repo.get_id()) == pin_base, (
        "Main should still hold the original pin before the merge"
    )

    repo.branch_merge_start("feature-branch", message="Merge link pin update")
    repo.push()

    assert _link_pin(repo, source_repo.get_id()) == pin_feature, (
        "Merge should carry the pin the merged branch moved"
    )
    with repo.open_file(linked_file, "r") as f:
        assert "revision two" in f.read(), (
            "Mount path should hold the content of the newly pinned revision"
        )

    _assert_crr_clean(
        repo,
        expected_files_present=["main-file.txt", linked_file],
        expected_files_absent=[],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )


@pytest.mark.smoke
def test_link_pin_update_on_branch_merge_into(new_lore_repo):
    """A pin-only update reaches main through `branch merge into`.

    The link's own files must stay in the linked repository: realizing them into
    the parent tree overwrites the link node's child pointer and drops the
    linked content out of the parent's tree.
    """
    link_path = "linked/repo"
    repo, source_repo, _pin_base = _fixed_link_pinned_on_main(new_lore_repo, link_path)
    linked_file = f"{link_path}/link-file.txt"

    repo.branch_create("feature-branch")
    repo.link_update(link_path)
    repo.commit("Move link pin on feature branch")
    repo.push()
    pin_feature = _link_pin(repo, source_repo.get_id())

    repo.branch_merge_into("main", message="Merge link pin update into main")
    repo.branch_switch("main")

    assert _link_pin(repo, source_repo.get_id()) == pin_feature, (
        "merge into should carry the pin onto the target branch"
    )

    link_output = repo.link_list()
    assert re.search(
        rf"Link\s+{source_repo.get_id()}.*?Source path:\s+/", link_output, re.DOTALL
    ), f"Merged link should still mount the linked repository root.\nGot: {link_output}"

    with repo.open_file(linked_file, "r") as f:
        assert "revision two" in f.read(), (
            "Mount path should hold the content of the newly pinned revision"
        )

    # A fresh clone proves the linked content is reachable through the link node
    # in the committed tree rather than only present on this working copy.
    clone = repo.clone()
    with clone.open_file(linked_file, "r") as f:
        assert "revision two" in f.read(), (
            "Fresh clone should realize the newly pinned linked content"
        )


@pytest.mark.smoke
def test_link_autofollow_pin_not_carried_by_merge_into(new_lore_repo):
    """`merge into` leaves an auto-following link's pin on the target branch.

    Such a pin points into the branch of the linked repository that mirrors the
    parent branch, so writing it onto another parent branch leaves the parent
    pinned off the mirror its row resolves to, and the next commit into the link
    builds on the wrong mirror and fails to push.
    """
    repo = _commit_initial_main(new_lore_repo, "main-file.txt")
    source_repo, _ = _make_link_source(new_lore_repo, ["link-file.txt"])

    link_path = "linked/repo"
    repo.link_add(link_path, source_repo.get_id(), "/")
    repo.commit("Add auto-following link on main")
    repo.push()
    pin_main = _link_pin(repo, source_repo.get_id())

    # Advance the link through the mount path, which moves its pin onto the
    # linked repository's mirror of the feature branch.
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/feature-file.txt", "w+") as f:
        f.writelines(["feature content\n"])
    repo.stage(scan=True)
    repo.commit("Add a file inside the link on feature branch")
    repo.push()
    assert _link_pin(repo, source_repo.get_id()) != pin_main, (
        "Committing into the link should have moved its pin on the feature branch"
    )

    repo.branch_merge_into("main", message="Merge feature branch into main")
    repo.branch_switch("main")

    assert _link_pin(repo, source_repo.get_id()) == pin_main, (
        "merge into should not move an auto-following link's pin across branches"
    )

    # The link stays usable on the target branch: its pin is still on main's own
    # mirror, so committing into it and pushing succeed.
    with repo.open_file(f"{link_path}/link-file.txt", "w+") as f:
        f.writelines(["changed on main after merge into\n"])
    repo.stage(scan=True)
    output = repo.commit("Change inside the link on main")
    assert "Commit succeeded" in output, f"Commit did not succeed - Got:\n{output}"
    repo.push()


def _diverge_link_pins(new_lore_repo, link_path: str) -> tuple[Lore, Lore, str, str]:
    """Feature branch pins a third source revision, main pins the second."""
    repo, source_repo, _pin_base = _fixed_link_pinned_on_main(new_lore_repo, link_path)
    revision_two = source_repo.branch_info().local_latest

    with source_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link source revision three\n"])
    source_repo.stage(scan=True)
    source_repo.commit("Third source revision")
    source_repo.push()
    revision_three = source_repo.branch_info().local_latest

    repo.branch_create("feature-branch")
    repo.link_update(link_path, pin=f"{revision_three}")
    repo.commit("Pin link to the third revision on feature branch")
    repo.push()

    repo.branch_switch("main")
    repo.link_update(link_path, pin=f"{revision_two}")
    repo.commit("Pin link to the second revision on main")
    repo.push()

    return repo, source_repo, revision_two, revision_three


@pytest.mark.smoke
def test_link_pin_update_divergent_does_not_silently_pick_a_side(new_lore_repo):
    """Both branches moving a link pin stops the merge instead of resolving it.

    Divergent rows are a parent-level conflict. Until they can be resolved in
    place the merge refuses, so neither side's row is adopted silently.
    """
    link_path = "linked/repo"
    repo, source_repo, revision_two, _revision_three = _diverge_link_pins(
        new_lore_repo, link_path
    )

    with pytest.raises(LinkPinDivergedError):
        repo.branch_merge_start("feature-branch", message="Merge divergent link pins")

    assert _link_pin(repo, source_repo.get_id()) == revision_two, (
        "A refused merge should leave main's pin alone"
    )

    repo.branch_merge_abort()
    assert _link_pin(repo, source_repo.get_id()) == revision_two, (
        "Aborting should leave main's pin alone"
    )


@pytest.mark.smoke
def test_link_pin_update_divergent_detected_with_ignore_links(new_lore_repo):
    """`--ignore-links` skips link content work but not the row comparison.

    The parent's link list is parent state, so a row both branches moved is a
    parent merge conflict that the flag does not suppress.
    """
    link_path = "linked/repo"
    repo, source_repo, revision_two, _revision_three = _diverge_link_pins(
        new_lore_repo, link_path
    )

    with pytest.raises(LinkPinDivergedError):
        repo.branch_merge_start(
            "feature-branch", message="Merge divergent link pins", ignore_links=True
        )

    assert _link_pin(repo, source_repo.get_id()) == revision_two, (
        "A refused merge should leave main's pin alone"
    )

    repo.branch_merge_abort()


@pytest.mark.smoke
def test_link_pin_update_merge_ignore_links_keeps_pin(new_lore_repo):
    """`--ignore-links` leaves a one-sided pin move where the target has it.

    Carrying the row would have to realize the linked content at the mount,
    which is the link content work this flag suppresses.
    """
    link_path = "linked/repo"
    repo, source_repo, pin_base = _fixed_link_pinned_on_main(new_lore_repo, link_path)

    repo.branch_create("feature-branch")
    repo.link_update(link_path)
    with repo.open_file("feature-file.txt", "w+") as f:
        f.writelines(["feature content\n"])
    repo.stage(scan=True)
    repo.commit("Move link pin and add a file on feature branch")
    repo.push()

    repo.branch_switch("main")
    repo.branch_merge_start(
        "feature-branch", message="Merge without links", ignore_links=True
    )
    repo.push()

    assert repo.file_exists("feature-file.txt"), (
        "Parent changes should still merge with --ignore-links"
    )
    assert _link_pin(repo, source_repo.get_id()) == pin_base, (
        "--ignore-links should not move the link pin"
    )


def test_link_merge_specific(new_lore_repo):
    """Merge only a specific linked repository via --link."""
    urc: Lore = new_lore_repo()

    # Create initial file in main repo
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])

    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    # Create link repository with initial content
    link_repo = new_lore_repo()

    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link repo base content\n"])

    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    # Add link to main repo
    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)

    urc.commit("Add link")
    urc.push()

    # Create feature branch (auto-follows into linked repo)
    urc.branch_create("feature-branch")

    # On feature branch, add new files in both main and linked repos
    with urc.open_file("feature-main-file.txt", "w+") as f:
        f.writelines(["feature branch main repo addition\n"])

    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature branch link repo addition\n"])

    urc.stage(scan=True)
    urc.commit("Feature branch additions")
    urc.push()

    # Switch back to main and add a different new file in main repo
    urc.branch_switch("main")

    with urc.open_file("main-only-file.txt", "w+") as f:
        f.writelines(["main branch only addition\n"])

    urc.stage(scan=True)
    urc.commit("Main branch addition")
    urc.push()

    # Merge only the specific linked repo (auto-commit)
    urc.branch_merge_start(
        "feature-branch",
        link=link_path,
        message="Merge feature-branch linked repo only",
    )
    urc.push()

    # Verify: linked repo additions from feature branch are applied
    assert urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature branch link repo file should be present after link-specific merge"
    )

    # Verify: main repo additions from feature branch are NOT present
    assert not urc.file_exists("feature-main-file.txt"), (
        "Feature branch main repo file should NOT be present after link-only merge"
    )

    # Verify link pin was updated via link list
    link_list_output = urc.link_list()
    assert link_repo.get_id() in link_list_output, (
        "Link should still be in the link list after merge"
    )

    # Verify post-merge state is clean
    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after merge commit - Got:\n{status}"
    )


def test_link_merge_abort_specific(new_lore_repo):
    """Abort only a specific linked repository merge via --link."""
    urc: Lore = new_lore_repo()

    # Create initial file in main repo
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])

    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    # Create link repository with initial content
    link_repo = new_lore_repo()

    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link repo base content\n"])

    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    # Add link to main repo
    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)

    urc.commit("Add link")
    urc.push()

    # Create feature branch (auto-follows into linked repo)
    urc.branch_create("feature-branch")

    # On feature branch, add a new file in the linked repo
    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature branch link repo addition\n"])

    urc.stage(scan=True)
    urc.commit("Feature branch link addition")
    urc.push()

    # Switch back to main
    urc.branch_switch("main")

    # Merge only the linked repo with no_commit
    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    # Verify the linked repo file is present after merge
    assert urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature link file should be present after merge"
    )

    # Abort the linked repo merge
    urc.branch_merge_abort(link=link_path)

    # Verify the linked repo file is rolled back
    assert not urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature link file should not exist after link-specific abort"
    )

    # Verify main repo file is still present
    assert urc.file_exists("main-file.txt"), (
        "Main repo file should still exist after abort"
    )


def test_link_merge_preserves_tracked_branch(new_lore_repo):
    """After merge --link, the link's tracked branch is preserved (not overwritten by source)."""
    repo: Lore = new_lore_repo()

    with repo.open_file("main-file.txt", "w+") as f:
        f.writelines(["main content\n"])
    repo.stage(scan=True)
    repo.commit("Initial main commit")
    repo.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Snapshot the link list before merge
    link_list_before = repo.link_list()

    # Create feature branch and add content
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/feature-file.txt", "w+") as f:
        f.writelines(["feature content\n"])
    repo.stage(scan=True)
    repo.commit("Feature branch addition")
    repo.push()

    repo.branch_switch("main")

    # Merge only the linked repo
    repo.branch_merge_start(
        "feature-branch", link=link_path, message="Link-only merge"
    )
    repo.push()

    # Verify: link list still shows "main" as tracked branch, not "feature-branch"
    link_list_after = repo.link_list()
    assert "feature-branch" not in link_list_after, (
        f"Link should track 'main' branch after merge, not 'feature-branch'.\n"
        f"Before: {link_list_before}\nAfter: {link_list_after}"
    )
    assert "main" in link_list_after, (
        f"Link should still track 'main' branch after merge.\nGot: {link_list_after}"
    )


def test_link_merge_sequential(new_lore_repo):
    """Two sequential link merges from the same feature branch work correctly."""
    repo: Lore = new_lore_repo()

    with repo.open_file("main-file.txt", "w+") as f:
        f.writelines(["main content\n"])
    repo.stage(scan=True)
    repo.commit("Initial main commit")
    repo.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Create feature branch and add first file
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/first.txt", "w+") as f:
        f.writelines(["first feature file\n"])
    repo.stage(scan=True)
    repo.commit("First feature commit")
    repo.push()

    # First link merge
    repo.branch_switch("main")
    repo.branch_merge_start(
        "feature-branch", link=link_path, message="First link merge"
    )
    repo.push()

    assert repo.file_exists(f"{link_path}/first.txt"), "First file should be present"

    # Add second file on feature branch
    repo.branch_switch("feature-branch")
    with repo.open_file(f"{link_path}/second.txt", "w+") as f:
        f.writelines(["second feature file\n"])
    repo.stage(scan=True)
    repo.commit("Second feature commit")
    repo.push()

    # Second link merge
    repo.branch_switch("main")
    repo.branch_merge_start(
        "feature-branch", link=link_path, message="Second link merge"
    )
    repo.push()

    assert repo.file_exists(f"{link_path}/first.txt"), (
        "First file should still be present after second merge"
    )
    assert repo.file_exists(f"{link_path}/second.txt"), (
        "Second file should be present after second merge"
    )

    status = repo.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after second merge - Got:\n{status}"
    )


def test_link_update_after_merge(new_lore_repo):
    """Link update works correctly after a link merge (tracked branch is intact)."""
    repo: Lore = new_lore_repo()

    with repo.open_file("main-file.txt", "w+") as f:
        f.writelines(["main content\n"])
    repo.stage(scan=True)
    repo.commit("Initial main commit")
    repo.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Create feature branch, add content, merge the link
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/feature-file.txt", "w+") as f:
        f.writelines(["feature content\n"])
    repo.stage(scan=True)
    repo.commit("Feature commit")
    repo.push()

    repo.branch_switch("main")
    repo.branch_merge_start(
        "feature-branch", link=link_path, message="Link merge"
    )
    repo.push()

    # Now push a new commit to the linked repo directly (on main branch)
    # Sync first since the link merge advanced the linked repo's branch
    link_repo.sync()
    with link_repo.open_file("direct-update.txt", "w+") as f:
        f.writelines(["direct update content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Direct update to linked repo")
    link_repo.push()

    # Link update should follow the link's tracked branch (main), not the feature branch
    repo.link_update(link_path)

    assert repo.file_exists(f"{link_path}/direct-update.txt"), (
        "Link update should pick up changes from the link's main branch"
    )

    repo.commit("Update link after merge")


def test_link_merge_abort_restores_link_state(new_lore_repo):
    """After merge --link abort, link list shows original branch and revision."""
    repo: Lore = new_lore_repo()

    with repo.open_file("main-file.txt", "w+") as f:
        f.writelines(["main content\n"])
    repo.stage(scan=True)
    repo.commit("Initial main commit")
    repo.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Snapshot link state before merge
    link_list_before = repo.link_list()

    # Create feature branch with linked content
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/feature-file.txt", "w+") as f:
        f.writelines(["feature content\n"])
    repo.stage(scan=True)
    repo.commit("Feature commit")
    repo.push()

    # Start link merge with no_commit to leave it pending
    repo.branch_switch("main")
    repo.branch_merge_start(
        "feature-branch", link=link_path, no_commit=True
    )

    # Verify file is present during pending merge
    assert repo.file_exists(f"{link_path}/feature-file.txt"), (
        "Feature file should be present during pending merge"
    )

    # Abort the link merge
    repo.branch_merge_abort(link=link_path)

    # Verify file is rolled back
    assert not repo.file_exists(f"{link_path}/feature-file.txt"), (
        "Feature file should not exist after abort"
    )

    # Verify link state is restored to pre-merge state
    link_list_after = repo.link_list()
    assert "main" in link_list_after, (
        f"Link should still track 'main' branch after abort.\nGot: {link_list_after}"
    )

    # Verify no merge is in progress — status should not say "pending merge"
    status = repo.status()
    assert "pending merge" not in status.lower(), (
        f"No merge should be in progress after abort - Got:\n{status}"
    )


def test_link_merge_abort_preserves_parent_staged_state(new_lore_repo):
    """Aborting a link merge must not destroy pre-existing staged changes in the parent repo."""
    repo: Lore = new_lore_repo()

    with repo.open_file("main-file.txt", "w+") as f:
        f.writelines(["main content\n"])
    repo.stage(scan=True)
    repo.commit("Initial main commit")
    repo.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Create feature branch with linked content
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/feature-file.txt", "w+") as f:
        f.writelines(["feature content\n"])
    repo.stage(scan=True)
    repo.commit("Feature commit")
    repo.push()

    # Switch back to main and stage a parent-level change BEFORE the merge
    repo.branch_switch("main")
    with repo.open_file("parent-staged.txt", "w+") as f:
        f.writelines(["staged parent content\n"])
    repo.stage(scan=True)

    # Verify parent file is staged
    status_before = repo.status()
    assert "parent-staged.txt" in status_before, (
        f"Parent file should be staged before merge - Got:\n{status_before}"
    )

    # Start link merge with no_commit
    repo.branch_merge_start(
        "feature-branch", link=link_path, no_commit=True
    )

    # Abort the link merge
    repo.branch_merge_abort(link=link_path)

    # The parent's staged change must survive the abort
    assert repo.file_exists("parent-staged.txt"), (
        "Parent staged file should still exist on disk after abort"
    )
    status_after = repo.status()
    assert "parent-staged.txt" in status_after, (
        f"Parent file should still be staged after link merge abort - Got:\n{status_after}"
    )

    # The merge state should be cleared
    assert "pending merge" not in status_after.lower(), (
        f"No merge should be in progress after abort - Got:\n{status_after}"
    )

    # Should be able to commit the parent change normally
    repo.commit("Commit parent staged change after abort")
    repo.push()

    assert repo.file_exists("parent-staged.txt"), (
        "Parent file should be present after commit"
    )


def test_link_merge_file_conflict_resolve(new_lore_repo):
    """File conflict in linked repo is resolvable from the main repo."""
    urc: Lore = new_lore_repo()

    # Create initial file in main repo
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])

    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    # Create link repository with a file that will be conflicted
    link_repo = new_lore_repo()

    with link_repo.open_file("shared-data.txt", "w+") as f:
        f.writelines(["base content\n"])

    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    # Add link to main repo
    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)

    urc.commit("Add link")
    urc.push()

    # Create feature branch (auto-follows into linked repo)
    urc.branch_create("feature-branch")

    # On feature branch, modify the shared file through the main repo's mount path
    with urc.open_file(f"{link_path}/shared-data.txt", "w+") as f:
        f.writelines(["feature branch content\n"])

    urc.stage(scan=True)
    urc.commit("Feature branch modifies shared data")
    urc.push()

    # Switch to main and modify the same file differently through the mount path
    urc.branch_switch("main")

    with urc.open_file(f"{link_path}/shared-data.txt", "w+") as f:
        f.writelines(["main branch content\n"])

    urc.stage(scan=True)
    urc.commit("Main branch modifies shared data")
    urc.push()

    # Merge with --link — should produce conflicts, not fail
    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    # Verify the conflict file exists at the mount path
    conflict_file = f"{link_path}/shared-data.txt"
    assert urc.file_exists(conflict_file), (
        "Conflicted file should exist at mount path"
    )

    # Resolve the conflict by writing the desired content and marking as resolved
    with urc.open_file(conflict_file, "w+") as f:
        f.writelines(["manually resolved content\n"])

    urc.branch_merge_resolve(conflict_file)

    # Commit and push
    urc.commit("Merge with resolved conflict in linked repo")
    urc.push()

    # Verify post-merge state
    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after merge commit - Got:\n{status}"
    )

    # Verify the resolved content is present
    with urc.open_file(conflict_file, "r") as f:
        content = f.read()
    assert "manually resolved content" in content, (
        f"File should have manually resolved content - Got: {content}"
    )


def _setup_link_merge_conflict(new_lore_repo, link_path="linked/repo", files=None):
    """Helper: create main repo + linked repo with conflicting changes on feature branch.

    `files` is a list of dicts with keys: path, base, mine, theirs.
    Each file will be created with base content, then modified on both branches.
    Returns (urc, link_repo, link_path).
    """
    if files is None:
        files = [{"path": "data.txt", "base": "base\n", "mine": "mine\n", "theirs": "theirs\n"}]

    urc: Lore = new_lore_repo()

    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo content\n"])
    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    link_repo = new_lore_repo()

    # Create base files in linked repo
    for file_info in files:
        dirs = "/".join(file_info["path"].split("/")[:-1])
        if dirs:
            link_repo.make_dirs(dirs)
        with link_repo.open_file(file_info["path"], "w+") as f:
            f.writelines([file_info["base"]])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)
    urc.commit("Add link")
    urc.push()

    # Create feature branch
    urc.branch_create("feature-branch")

    # Feature branch: modify files through mount path
    for file_info in files:
        with urc.open_file(f"{link_path}/{file_info['path']}", "w+") as f:
            f.writelines([file_info["theirs"]])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    # Switch to main: modify same files differently
    urc.branch_switch("main")
    for file_info in files:
        with urc.open_file(f"{link_path}/{file_info['path']}", "w+") as f:
            f.writelines([file_info["mine"]])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

    return urc, link_repo, link_path


def test_link_merge_file_conflict_in_subdirectory(new_lore_repo):
    """File conflict in a subdirectory of a linked repo."""
    urc, _link_repo, link_path = _setup_link_merge_conflict(
        new_lore_repo,
        files=[{"path": "src/module.rs", "base": "base\n", "mine": "mine content\n", "theirs": "theirs content\n"}],
    )

    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    conflict_file = f"{link_path}/src/module.rs"
    assert urc.file_exists(conflict_file), "Conflict file should exist at mount path"

    with urc.open_file(conflict_file, "w+") as f:
        f.writelines(["resolved subdirectory content\n"])

    urc.branch_merge_resolve(conflict_file)
    urc.commit("Merge with resolved subdirectory conflict")
    urc.push()

    with urc.open_file(conflict_file, "r") as f:
        assert "resolved subdirectory content" in f.read()


def test_link_merge_file_conflict_in_nested_subdirectory(new_lore_repo):
    """File conflict in a deeply nested subdirectory of a linked repo."""
    urc, _link_repo, link_path = _setup_link_merge_conflict(
        new_lore_repo,
        files=[{
            "path": "src/core/engine/config.txt",
            "base": "base config\n",
            "mine": "mine config\n",
            "theirs": "theirs config\n",
        }],
    )

    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    conflict_file = f"{link_path}/src/core/engine/config.txt"
    assert urc.file_exists(conflict_file), "Deeply nested conflict file should exist"

    with urc.open_file(conflict_file, "w+") as f:
        f.writelines(["resolved deep config\n"])

    urc.branch_merge_resolve(conflict_file)
    urc.commit("Merge with resolved deep nested conflict")
    urc.push()

    with urc.open_file(conflict_file, "r") as f:
        assert "resolved deep config" in f.read()


def test_link_merge_multiple_file_conflicts_across_directories(new_lore_repo):
    """Multiple file conflicts at different depths, resolved independently."""
    urc, _link_repo, link_path = _setup_link_merge_conflict(
        new_lore_repo,
        files=[
            {"path": "readme.txt", "base": "base readme\n", "mine": "mine readme\n", "theirs": "theirs readme\n"},
            {"path": "src/lib.rs", "base": "base lib\n", "mine": "mine lib\n", "theirs": "theirs lib\n"},
            {"path": "src/util/helpers.rs", "base": "base helpers\n", "mine": "mine helpers\n", "theirs": "theirs helpers\n"},
        ],
    )

    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    # All three conflict files should exist
    f1 = f"{link_path}/readme.txt"
    f2 = f"{link_path}/src/lib.rs"
    f3 = f"{link_path}/src/util/helpers.rs"
    assert urc.file_exists(f1), "Root conflict file should exist"
    assert urc.file_exists(f2), "Subdirectory conflict file should exist"
    assert urc.file_exists(f3), "Nested subdirectory conflict file should exist"

    # Resolve each with different content
    with urc.open_file(f1, "w+") as f:
        f.writelines(["resolved readme\n"])
    with urc.open_file(f2, "w+") as f:
        f.writelines(["resolved lib\n"])
    with urc.open_file(f3, "w+") as f:
        f.writelines(["resolved helpers\n"])

    urc.branch_merge_resolve([f1, f2, f3])
    urc.commit("Merge with multiple resolved conflicts")
    urc.push()

    with urc.open_file(f1, "r") as f:
        assert "resolved readme" in f.read()
    with urc.open_file(f2, "r") as f:
        assert "resolved lib" in f.read()
    with urc.open_file(f3, "r") as f:
        assert "resolved helpers" in f.read()


def test_link_merge_directory_level_resolve(new_lore_repo):
    """Resolve multiple conflicts by specifying the directory path."""
    urc, _link_repo, link_path = _setup_link_merge_conflict(
        new_lore_repo,
        files=[
            {"path": "src/a.txt", "base": "base a\n", "mine": "mine a\n", "theirs": "theirs a\n"},
            {"path": "src/b.txt", "base": "base b\n", "mine": "mine b\n", "theirs": "theirs b\n"},
        ],
    )

    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    # Manually resolve both files
    with urc.open_file(f"{link_path}/src/a.txt", "w+") as f:
        f.writelines(["resolved a\n"])
    with urc.open_file(f"{link_path}/src/b.txt", "w+") as f:
        f.writelines(["resolved b\n"])

    # Resolve by directory path
    urc.branch_merge_resolve(f"{link_path}/src")
    urc.commit("Merge with directory-level resolve")
    urc.push()

    with urc.open_file(f"{link_path}/src/a.txt", "r") as f:
        assert "resolved a" in f.read()
    with urc.open_file(f"{link_path}/src/b.txt", "r") as f:
        assert "resolved b" in f.read()


def test_link_merge_delete_vs_modify_in_link(new_lore_repo):
    """Delete-vs-modify file conflict inside a linked repo. Feature branch deletes
    the file; main branch modifies it. The default merge must surface the conflict
    in a recoverable way: either the file remains on disk with conflict markers,
    or `.mine` / `.theirs` / `.base` sidecars are present. The user must not see
    the file silently vanish."""
    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("doomed.txt", "w+") as f:
        f.writelines(["link base\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # Feature branch: delete the link file via mount path
    urc.branch_create("feature-branch")
    urc.remove_file(f"{link_path}/doomed.txt")
    urc.stage(scan=True)
    urc.commit("Feature branch deletes doomed.txt")
    urc.push()

    # Main branch: modify the same file via mount path
    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/doomed.txt", "w+") as f:
        f.writelines(["main modified\n"])
    urc.stage(scan=True)
    urc.commit("Main branch modifies doomed.txt")
    urc.push()

    # Default merge — must report the conflict, not auto-commit
    urc.branch_merge_start("feature-branch", message="Merge feature-branch", no_commit=True)

    # Either the file is on disk with markers OR sidecars exist. Either is
    # acceptable; silent disappearance is not.
    file_path = f"{link_path}/doomed.txt"
    mine_sidecar = f"{file_path}.mine"
    theirs_sidecar = f"{file_path}.theirs"
    base_sidecar = f"{file_path}.base"
    has_file = urc.file_exists(file_path)
    has_any_sidecar = (
        urc.file_exists(mine_sidecar)
        or urc.file_exists(theirs_sidecar)
        or urc.file_exists(base_sidecar)
    )
    assert has_file or has_any_sidecar, (
        f"Delete-vs-modify must leave recoverable artifacts in link mount; "
        f"found neither {file_path} nor sidecars."
    )

    # Resolving via "mine" (the modify side) restores the file on disk.
    urc.branch_merge_resolve_mine(file_path)
    assert urc.file_exists(file_path), (
        "After resolve_mine, the modified version should be on disk."
    )

    urc.commit("Merge with delete-vs-modify resolved as mine")
    urc.push()


def test_link_merge_mixed_conflict_and_clean(new_lore_repo):
    """Linked repo merge with both conflicting and cleanly merged files."""
    urc: Lore = new_lore_repo()

    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo content\n"])
    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("conflict.txt", "w+") as f:
        f.writelines(["base conflict\n"])
    with link_repo.open_file("clean.txt", "w+") as f:
        f.writelines(["base clean\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)
    urc.commit("Add link")
    urc.push()

    urc.branch_create("feature-branch")

    # Feature branch: modify both files, and add a new file
    with urc.open_file(f"{link_path}/conflict.txt", "w+") as f:
        f.writelines(["theirs conflict\n"])
    with urc.open_file(f"{link_path}/clean.txt", "w+") as f:
        f.writelines(["clean modified by feature\n"])
    with urc.open_file(f"{link_path}/new-feature-file.txt", "w+") as f:
        f.writelines(["new file from feature\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    # Main branch: only modify the conflicting file
    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/conflict.txt", "w+") as f:
        f.writelines(["mine conflict\n"])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

    # Merge
    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    # Clean file should be merged automatically
    with urc.open_file(f"{link_path}/clean.txt", "r") as f:
        content = f.read()
    assert "clean modified by feature" in content, (
        f"Clean file should have feature branch content after auto-merge - Got: {content}"
    )

    # New file from feature branch should be present
    assert urc.file_exists(f"{link_path}/new-feature-file.txt"), (
        "New file from feature branch should be present"
    )

    # Conflict file needs manual resolution
    with urc.open_file(f"{link_path}/conflict.txt", "w+") as f:
        f.writelines(["resolved conflict\n"])
    urc.branch_merge_resolve(f"{link_path}/conflict.txt")

    urc.commit("Merge with mixed conflict and clean changes")
    urc.push()

    # Verify all files present and correct
    with urc.open_file(f"{link_path}/conflict.txt", "r") as f:
        assert "resolved conflict" in f.read()
    with urc.open_file(f"{link_path}/clean.txt", "r") as f:
        assert "clean modified by feature" in f.read()
    assert urc.file_exists(f"{link_path}/new-feature-file.txt")


def test_link_merge_file_conflict_resolve_mine(new_lore_repo):
    """File conflict in linked repo resolved with mine."""
    urc, _link_repo, link_path = _setup_link_merge_conflict(
        new_lore_repo,
        files=[{"path": "data.txt", "base": "base content\n", "mine": "mine content\n", "theirs": "theirs content\n"}],
    )

    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    conflict_file = f"{link_path}/data.txt"
    assert urc.file_exists(conflict_file), "Conflict file should exist"

    urc.branch_merge_resolve_mine(conflict_file)

    urc.commit("Merge with mine resolution")
    urc.push()

    with urc.open_file(conflict_file, "r") as f:
        content = f.read()
    assert "mine content" in content, (
        f"File should have mine content after resolve mine - Got: {content}"
    )


def test_link_merge_file_conflict_resolve_theirs(new_lore_repo):
    """File conflict in linked repo resolved with theirs."""
    urc, _link_repo, link_path = _setup_link_merge_conflict(
        new_lore_repo,
        files=[{"path": "data.txt", "base": "base content\n", "mine": "mine content\n", "theirs": "theirs content\n"}],
    )

    urc.branch_merge_start("feature-branch", link=link_path, no_commit=True)

    conflict_file = f"{link_path}/data.txt"
    assert urc.file_exists(conflict_file), "Conflict file should exist"

    urc.branch_merge_resolve_theirs(conflict_file)

    urc.commit("Merge with theirs resolution")
    urc.push()

    with urc.open_file(conflict_file, "r") as f:
        content = f.read()
    assert "theirs content" in content, (
        f"File should have theirs content after resolve theirs - Got: {content}"
    )


def test_link_merge_into_specific(new_lore_repo):
    """Merge current linked repo branch into target branch via --link."""
    urc: Lore = new_lore_repo()

    # Create initial file in main repo
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])

    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    # Create link repository with initial content
    link_repo = new_lore_repo()

    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link repo base content\n"])

    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    # Add link to main repo
    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)

    urc.commit("Add link")
    urc.push()

    # Create feature branch (auto-follows into linked repo)
    urc.branch_create("feature-branch")

    # On feature branch, add a file in the linked repo through mount path
    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature branch link repo addition\n"])

    urc.stage(scan=True)
    urc.commit("Feature branch link addition")
    urc.push()

    # Merge the feature branch's linked repo into main via merge_into --link.
    # This merges the linked repo's feature branch into its main branch on the remote,
    # then updates the main repo's link pin on the feature branch.
    urc.branch_merge_into(
        "main", "Merge feature linked repo into main", link=link_path
    )

    # Verify we're still on feature branch with the file present
    assert urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature branch link file should still be present"
    )
    assert urc.file_exists("main-file.txt"), (
        "Main repo file should still exist"
    )


def test_link_merge_into_scope_isolation(new_lore_repo):
    """merge into --link only merges linked repo changes, not main repo changes."""
    repo: Lore = new_lore_repo()

    with repo.open_file("main-file.txt", "w+") as f:
        f.writelines(["main content\n"])
    repo.stage(scan=True)
    repo.commit("Initial main commit")
    repo.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Create feature branch with changes in BOTH main repo and linked repo
    repo.branch_create("feature-branch")

    with repo.open_file("feature-main-only.txt", "w+") as f:
        f.writelines(["feature main content\n"])
    with repo.open_file(f"{link_path}/feature-link-only.txt", "w+") as f:
        f.writelines(["feature link content\n"])

    repo.stage(scan=True)
    repo.commit("Feature branch additions in both repos")
    repo.push()

    # Merge into main scoped to link only
    repo.branch_merge_into(
        "main", "Merge only linked repo into main", link=link_path
    )

    # Switch to main and sync to see what landed
    repo.branch_switch("main")
    repo.sync()

    # Linked repo file should be present on main
    assert repo.file_exists(f"{link_path}/feature-link-only.txt"), (
        "Link file from feature branch should be on main after merge into --link"
    )

    # Main repo feature file should NOT be on main
    assert not repo.file_exists("feature-main-only.txt"), (
        "Main repo file from feature branch should NOT be on main after link-only merge into"
    )


def test_link_merge_into_sequential(new_lore_repo):
    """Two sequential merge into --link operations from the same feature branch."""
    repo: Lore = new_lore_repo()

    with repo.open_file("main-file.txt", "w+") as f:
        f.writelines(["main content\n"])
    repo.stage(scan=True)
    repo.commit("Initial main commit")
    repo.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Create feature branch, add first link file
    repo.branch_create("feature-branch")
    with repo.open_file(f"{link_path}/first.txt", "w+") as f:
        f.writelines(["first feature file\n"])
    repo.stage(scan=True)
    repo.commit("First feature commit")
    repo.push()

    # First merge into main
    repo.branch_merge_into(
        "main", "First link merge into main", link=link_path
    )

    # Sync and merge main into feature branch (main advanced from the merge_into)
    repo.sync()
    repo.branch_merge_start("main", message="Merge main into feature")
    repo.push()

    # Add second link file on feature branch
    with repo.open_file(f"{link_path}/second.txt", "w+") as f:
        f.writelines(["second feature file\n"])
    repo.stage(scan=True)
    repo.commit("Second feature commit")
    repo.push()

    # Second merge into main
    repo.branch_merge_into(
        "main", "Second link merge into main", link=link_path
    )

    # Verify both files landed on main
    repo.branch_switch("main")
    repo.sync()

    assert repo.file_exists(f"{link_path}/first.txt"), (
        "First file should be on main after sequential merge into"
    )
    assert repo.file_exists(f"{link_path}/second.txt"), (
        "Second file should be on main after sequential merge into"
    )


@pytest.mark.smoke
def test_link_commit_per_link_message(new_lore_repo):
    """Test that per-link commit messages are applied to linked repositories."""
    # Create link repository with initial content
    link_repo: Lore = new_lore_repo()
    link_file = "link-file.txt"
    with link_repo.open_file(link_file, "w+") as f:
        f.write("initial link content\n")
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    # Create main repository and add link
    urc: Lore = new_lore_repo()
    main_file = "main-file.txt"
    with urc.open_file(main_file, "w+") as f:
        f.write("initial main content\n")
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_path = "linked"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # Modify files in both repos
    with urc.open_file(main_file, "w+") as f:
        f.write("updated main content\n")
    with urc.open_file(os.path.join(link_path, link_file), "w+") as f:
        f.write("updated link content\n")
    urc.stage(scan=True)

    # Commit with per-link message
    urc.commit(
        "Main repo update",
        link_messages={link_path: "Link-specific update message"},
        non_interactive=True,
    )
    urc.push()

    # Verify main repo message
    main_info = urc.revision_info(check=True, no_pager=True)
    assert main_info.message == "Main repo update", (
        f"Expected main message 'Main repo update', got '{main_info.message}'"
    )

    # Verify link repo message by checking revision info on the link repo directly
    link_repo.sync()
    link_info = link_repo.revision_info(check=True, no_pager=True)
    assert link_info.message == "Link-specific update message", (
        f"Expected link message 'Link-specific update message', got '{link_info.message}'"
    )


@pytest.mark.smoke
def test_link_commit_no_link_message_fallback(new_lore_repo):
    """Test that without per-link messages, all repos get the main message."""
    # Create link repository
    link_repo: Lore = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.write("initial link content\n")
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    # Create main repository and add link
    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.write("initial main content\n")
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_path = "linked"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # Modify files in both repos
    with urc.open_file("main-file.txt", "w+") as f:
        f.write("updated main content\n")
    with urc.open_file(os.path.join(link_path, "link-file.txt"), "w+") as f:
        f.write("updated link content\n")
    urc.stage(scan=True)

    # Commit without link messages — non-interactive to avoid prompts
    urc.commit("Shared message for all", non_interactive=True)
    urc.push()

    # Verify both repos get the same message
    main_info = urc.revision_info(check=True, no_pager=True)
    assert main_info.message == "Shared message for all"

    link_repo.sync()
    link_info = link_repo.revision_info(check=True, no_pager=True)
    assert link_info.message == "Shared message for all"


@pytest.mark.smoke
def test_link_commit_invalid_link_message_errors(new_lore_repo):
    """Test that --link-message with an invalid path produces an error."""
    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.write("content\n")
    urc.stage(scan=True)
    urc.commit("Initial commit")
    urc.push()

    # Modify and stage
    with urc.open_file("main-file.txt", "w+") as f:
        f.write("updated content\n")
    urc.stage(scan=True)

    # Record revision before the failed commit attempt
    info_before = urc.revision_info(check=True, no_pager=True)

    # Try to commit with an invalid link-message path — should fail
    output = urc.commit(
        "Main message",
        link_messages={"nonexistent/path": "Some message"},
        non_interactive=True,
        check=False,
    )
    assert "does not match any linked repository" in output, (
        f"Expected a specific error for invalid --link-message path, got: {output}"
    )

    # Verify no new revision was created
    info_after = urc.revision_info(check=True, no_pager=True)
    assert info_before.revision == info_after.revision, (
        "Commit should not have proceeded with an invalid --link-message path"
    )


@pytest.mark.smoke
def test_link_commit_multiple_link_messages(new_lore_repo):
    """Test that multiple --link-message flags work for different links."""
    # Create two link repositories
    link_repo_a: Lore = new_lore_repo()
    with link_repo_a.open_file("file-a.txt", "w+") as f:
        f.write("link A content\n")
    link_repo_a.stage(scan=True)
    link_repo_a.commit("Initial A")
    link_repo_a.push()

    link_repo_b: Lore = new_lore_repo()
    with link_repo_b.open_file("file-b.txt", "w+") as f:
        f.write("link B content\n")
    link_repo_b.stage(scan=True)
    link_repo_b.commit("Initial B")
    link_repo_b.push()

    # Create main repository and add both links
    urc: Lore = new_lore_repo()
    with urc.open_file("main.txt", "w+") as f:
        f.write("main content\n")
    urc.stage(scan=True)
    urc.commit("Initial main")
    urc.push()

    urc.link_add("link-a", link_repo_a.get_id(), "/")
    urc.link_add("link-b", link_repo_b.get_id(), "/")
    urc.commit("Add links")
    urc.push()

    # Modify files in all three repos
    with urc.open_file("main.txt", "w+") as f:
        f.write("updated main\n")
    with urc.open_file("link-a/file-a.txt", "w+") as f:
        f.write("updated A\n")
    with urc.open_file("link-b/file-b.txt", "w+") as f:
        f.write("updated B\n")
    urc.stage(scan=True)

    # Commit with different messages per link
    urc.commit(
        "Main update",
        link_messages={
            "link-a": "Update A specifically",
            "link-b": "Update B specifically",
        },
        non_interactive=True,
    )
    urc.push()

    # Verify main repo message
    main_info = urc.revision_info(check=True, no_pager=True)
    assert main_info.message == "Main update"

    # Verify each link has its specific message
    link_repo_a.sync()
    a_info = link_repo_a.revision_info(check=True, no_pager=True)
    assert a_info.message == "Update A specifically", (
        f"Expected 'Update A specifically', got '{a_info.message}'"
    )

    link_repo_b.sync()
    b_info = link_repo_b.revision_info(check=True, no_pager=True)
    assert b_info.message == "Update B specifically", (
        f"Expected 'Update B specifically', got '{b_info.message}'"
    )


@pytest.mark.smoke
def test_link_commit_partial_link_messages(new_lore_repo):
    """Test that links without a --link-message get the main message as fallback."""
    # Create two link repositories
    link_repo_a: Lore = new_lore_repo()
    with link_repo_a.open_file("file-a.txt", "w+") as f:
        f.write("link A content\n")
    link_repo_a.stage(scan=True)
    link_repo_a.commit("Initial A")
    link_repo_a.push()

    link_repo_b: Lore = new_lore_repo()
    with link_repo_b.open_file("file-b.txt", "w+") as f:
        f.write("link B content\n")
    link_repo_b.stage(scan=True)
    link_repo_b.commit("Initial B")
    link_repo_b.push()

    # Create main repository and add both links
    urc: Lore = new_lore_repo()
    with urc.open_file("main.txt", "w+") as f:
        f.write("main content\n")
    urc.stage(scan=True)
    urc.commit("Initial main")
    urc.push()

    urc.link_add("link-a", link_repo_a.get_id(), "/")
    urc.link_add("link-b", link_repo_b.get_id(), "/")
    urc.commit("Add links")
    urc.push()

    # Modify files in all three repos
    with urc.open_file("main.txt", "w+") as f:
        f.write("updated main\n")
    with urc.open_file("link-a/file-a.txt", "w+") as f:
        f.write("updated A\n")
    with urc.open_file("link-b/file-b.txt", "w+") as f:
        f.write("updated B\n")
    urc.stage(scan=True)

    # Only specify message for link-a, not link-b
    urc.commit(
        "Main fallback message",
        link_messages={"link-a": "Specific A message"},
        non_interactive=True,
    )
    urc.push()

    # Verify link-a got its specific message
    link_repo_a.sync()
    a_info = link_repo_a.revision_info(check=True, no_pager=True)
    assert a_info.message == "Specific A message", (
        f"Expected 'Specific A message', got '{a_info.message}'"
    )

    # Verify link-b fell back to the main message
    link_repo_b.sync()
    b_info = link_repo_b.revision_info(check=True, no_pager=True)
    assert b_info.message == "Main fallback message", (
        f"Expected 'Main fallback message', got '{b_info.message}'"
    )


@pytest.mark.smoke
def test_link_commit_only_main_changes(new_lore_repo):
    """Test that commit with no link changes works normally even with --non-interactive."""
    # Create link repository
    link_repo: Lore = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.write("link content\n")
    link_repo.stage(scan=True)
    link_repo.commit("Initial link")
    link_repo.push()

    # Create main repository and add link
    urc: Lore = new_lore_repo()
    with urc.open_file("main.txt", "w+") as f:
        f.write("main content\n")
    urc.stage(scan=True)
    urc.commit("Initial main")
    urc.push()

    urc.link_add("linked", link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # Only modify main file, not link
    with urc.open_file("main.txt", "w+") as f:
        f.write("updated main only\n")
    urc.stage(scan=True)

    # Commit with --non-interactive — no link changes so no prompting
    urc.commit("Main only change", non_interactive=True)
    urc.push()

    main_info = urc.revision_info(check=True, no_pager=True)
    assert main_info.message == "Main only change"


@pytest.mark.smoke
def test_link_commit_non_interactive_default_behavior(new_lore_repo):
    """Test that --non-interactive with no --link-message produces identical behavior to old commit."""
    # Create link repository
    link_repo: Lore = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.write("link content\n")
    link_repo.stage(scan=True)
    link_repo.commit("Initial link")
    link_repo.push()

    # Create main repository and add link
    urc: Lore = new_lore_repo()
    with urc.open_file("main.txt", "w+") as f:
        f.write("main content\n")
    urc.stage(scan=True)
    urc.commit("Initial main")
    urc.push()

    urc.link_add("linked", link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # Modify both repos
    with urc.open_file("main.txt", "w+") as f:
        f.write("updated main\n")
    with urc.open_file("linked/link-file.txt", "w+") as f:
        f.write("updated link\n")
    urc.stage(scan=True)

    # Non-interactive commit without --link-message = same message for all (backward compat)
    urc.commit("Same message everywhere", non_interactive=True)
    urc.push()

    main_info = urc.revision_info(check=True, no_pager=True)
    assert main_info.message == "Same message everywhere"

    link_repo.sync()
    link_info = link_repo.revision_info(check=True, no_pager=True)
    assert link_info.message == "Same message everywhere", (
        f"Expected backward-compatible behavior, got '{link_info.message}'"
    )


@pytest.mark.smoke
def test_link_list_staged(new_lore_repo):
    """Test that urc link list --staged shows linked repos with staged changes."""
    # Create link repository
    link_repo: Lore = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.write("link content\n")
    link_repo.stage(scan=True)
    link_repo.commit("Initial link")
    link_repo.push()

    # Create main repository and add link
    urc: Lore = new_lore_repo()
    with urc.open_file("main.txt", "w+") as f:
        f.write("main content\n")
    urc.stage(scan=True)
    urc.commit("Initial main")
    urc.push()

    link_path = "linked"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # Modify files in both repos and stage
    with urc.open_file("main.txt", "w+") as f:
        f.write("updated main\n")
    with urc.open_file(os.path.join(link_path, "link-file.txt"), "w+") as f:
        f.write("updated link\n")
    urc.stage(scan=True)

    # link list --staged should show the linked repo with file count
    output = urc.link_list(staged=True)
    assert link_path in output, (
        f"Expected link path '{link_path}' in output, got: {output}"
    )
    assert "file" in output and "changed" in output, (
        f"Expected file count in output, got: {output}"
    )


@pytest.mark.smoke
def test_link_list_staged_no_changes(new_lore_repo):
    """Test that urc link list --staged shows nothing when no links have staged changes."""
    # Create link repository
    link_repo: Lore = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.write("link content\n")
    link_repo.stage(scan=True)
    link_repo.commit("Initial link")
    link_repo.push()

    # Create main repository and add link
    urc: Lore = new_lore_repo()
    with urc.open_file("main.txt", "w+") as f:
        f.write("main content\n")
    urc.stage(scan=True)
    urc.commit("Initial main")
    urc.push()

    urc.link_add("linked", link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # Only modify main file, not link
    with urc.open_file("main.txt", "w+") as f:
        f.write("updated main only\n")
    urc.stage(scan=True)

    # link list --staged should show no links
    output = urc.link_list(staged=True)
    assert "No linked repositories with staged changes" in output, (
        f"Expected no-links message, got: {output}"
    )


@pytest.mark.smoke
def test_link_info_reports_link_details(new_lore_repo):
    """`lore link info <mount_path>` reports the linked repository, its mount
    and source paths, the resolved branch, the pinned revision and no staged
    change for an untouched link.
    """
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"sub/link-file.txt": "link content\n"})
    pinned_revision = link_repo.branch_info().local_latest

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})

    link_path = "libs/shared"
    urc.link_add(link_path, link_repo.get_id(), "/sub")
    urc.commit("Add link")
    urc.push()

    output = urc.link_info(link_path)

    assert link_repo.get_id() in output, (
        f"link info must report the linked repository id.\nOutput:\n{output}"
    )
    assert f"Link path: {link_path}" in output, (
        f"link info must report the mount path.\nOutput:\n{output}"
    )
    assert "Source path: sub" in output, (
        f"link info must report the source path inside the link.\nOutput:\n{output}"
    )
    assert "Branch: main" in output, (
        f"link info must report the resolved branch name.\nOutput:\n{output}"
    )
    assert f"Revision: {pinned_revision}" in output, (
        f"link info must report the pinned revision.\nOutput:\n{output}"
    )
    assert "Flags: None" in output, (
        f"link info must report the link flags.\nOutput:\n{output}"
    )
    assert "Staged: none" in output, (
        f"link info must report no staged change for an untouched link.\nOutput:\n{output}"
    )
    assert "Staged files: 0" in output, (
        f"link info must report zero staged files for an untouched link.\n"
        f"Output:\n{output}"
    )
    assert f"Remote revision: {pinned_revision}" in output, (
        f"link info must report the remote latest revision of the resolved "
        f"branch.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_info_reports_staged_pin_update(new_lore_repo):
    """`lore link info` reports an uncommitted pin bump as a staged update."""
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("v1", {"link-file.txt": "v1\n"})
    pin_v1 = link_repo.branch_info().local_latest

    link_repo.write_commit_push("v2", {"link-file.txt": "v2\n"})
    pin_v2 = link_repo.branch_info().local_latest

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})

    link_path = "linked"
    urc.link_add(link_path, link_repo.get_id(), "/", pin=pin_v1)
    urc.commit("Add link")
    urc.push()

    urc.link_update(link_path, pin=pin_v2)

    output = urc.link_info(link_path)

    assert "Staged: modified" in output, (
        f"link info must report an uncommitted pin bump as a staged update.\n"
        f"Output:\n{output}"
    )
    assert f"Revision: {pin_v2}" in output, (
        f"link info must report the newly staged pin.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_info_errors_for_non_link_path(new_lore_repo):
    """`lore link info` on a path that is not a link fails with a clear error."""
    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"regular-dir/file.txt": "content\n"})

    with pytest.raises(NotALinkError):
        urc.link_info("regular-dir")


@pytest.mark.smoke
def test_link_info_counts_staged_files_inside_link(new_lore_repo):
    """`lore link info` counts staged files inside the link, not just the pin."""
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push(
        "Initial link", {"sub/one.txt": "one\n", "sub/two.txt": "two\n"}
    )

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})

    link_path = "libs/shared"
    urc.link_add(link_path, link_repo.get_id(), "/sub")
    urc.commit("Add link")
    urc.push()

    assert "Staged files: 0" in urc.link_info(link_path)

    urc.write_files({f"{link_path}/one.txt": "one modified\n"})
    urc.stage(f"{link_path}/one.txt")

    output = urc.link_info(link_path)

    assert "Staged files: 1" in output, (
        f"link info must count a staged file inside the link.\nOutput:\n{output}"
    )
    assert "Staged: modified" in output, (
        f"a link with staged content inside it must report a staged update.\n"
        f"Output:\n{output}"
    )


@pytest.mark.smoke
def test_link_info_reports_staged_add(new_lore_repo):
    """`lore link info` reports an uncommitted `link add` as a staged addition
    with no staged files inside it yet.
    """
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"link-file.txt": "content\n"})

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})

    link_path = "libs/shared"
    urc.link_add(link_path, link_repo.get_id(), "/")

    output = urc.link_info(link_path)

    assert "Staged: added" in output, (
        f"link info must report an uncommitted link add as a staged addition.\n"
        f"Output:\n{output}"
    )
    assert "Staged files: 0" in output, (
        f"a link staged for addition has no staged files inside it.\nOutput:\n{output}"
    )
    assert "Source path: /" in output, (
        f"link info must report a root source path as `/`.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_info_reports_staged_removal(new_lore_repo):
    """`lore link info` reports an uncommitted `link remove` as a staged
    removal, describing it from the committed registry entry that staging the
    removal dropped, and does not count files inside a link that is going away.
    """
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"link-file.txt": "content\n"})
    pinned_revision = link_repo.branch_info().local_latest

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})

    link_path = "libs/shared"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    urc.link_remove(link_path)

    output = urc.link_info(link_path)

    assert "Staged: removed" in output, (
        f"link info must report an uncommitted link remove as a staged "
        f"removal.\nOutput:\n{output}"
    )
    assert "Staged files: 0" in output, (
        f"a link staged for removal must not count files inside it.\nOutput:\n{output}"
    )
    assert f"Revision: {pinned_revision}" in output, (
        f"link info must still report the pin of a link staged for removal.\n"
        f"Output:\n{output}"
    )
    assert "Branch: main" in output, (
        f"link info must still report the branch of a link staged for removal.\n"
        f"Output:\n{output}"
    )


@pytest.mark.smoke
def test_link_info_reports_nested_link_with_full_path(new_lore_repo):
    """`lore link info` on a link inside another link reports the mount path
    relative to this repository, not to the intermediate linked repository.
    """
    repo_a, _repo_b, repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "libs/vendor/b", "c"
    )

    output = repo_a.link_info(nested_mount)

    assert f"Link path: {nested_mount}" in output, (
        f"link info must report a nested link's full mount path.\nOutput:\n{output}"
    )
    assert repo_c.get_id() in output, (
        f"link info must report the innermost linked repository.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_info_omits_remote_revision_when_local(new_lore_repo):
    """`lore link info --local` omits the remote revision rather than reaching
    for the remote, and still reports everything held locally.
    """
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"link-file.txt": "content\n"})

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})

    link_path = "libs/shared"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    output = urc.link_info(link_path, local=True)

    assert "Remote revision:" not in output, (
        f"link info --local must not report a remote revision.\nOutput:\n{output}"
    )
    assert link_repo.get_id() in output, (
        f"link info --local must still report the linked repository.\nOutput:\n{output}"
    )
    assert f"Link path: {link_path}" in output, (
        f"link info --local must still report the mount path.\nOutput:\n{output}"
    )
    assert "Staged: none" in output, (
        f"link info --local must still report the staged state.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_info_distinguishes_tracking_from_pinned_branch(new_lore_repo):
    """`lore link info` distinguishes a link that follows its parent's branch
    from one pinned to an explicit branch.
    """
    tracking_target: Lore = new_lore_repo()
    tracking_target.write_commit_push("Initial", {"a.txt": "a\n"})

    pinned_target: Lore = new_lore_repo()
    pinned_target.write_commit_push("Initial", {"b.txt": "b\n"})

    urc: Lore = new_lore_repo()
    urc.write_commit_push("Initial main", {"main.txt": "main content\n"})

    urc.link_add("libs/tracking", tracking_target.get_id(), "/")
    urc.link_add("libs/pinned", pinned_target.get_id(), "/", disable_branching=True)
    urc.commit("Add links")
    urc.push()

    tracking_output = urc.link_info("libs/tracking")
    pinned_output = urc.link_info("libs/pinned")

    assert "[tracking]" in tracking_output, (
        f"a link following its parent's branch must report as tracking.\n"
        f"Output:\n{tracking_output}"
    )
    assert "[pinned]" in pinned_output, (
        f"a link with an explicit branch must report as pinned.\n"
        f"Output:\n{pinned_output}"
    )


@pytest.mark.smoke
def test_link_branching_and_pinning(new_lore_repo):
    """Test that branching and pinning are orthogonal concerns for link add.

    A branch is always created in the linked repo (using the parent repo's
    current branch) unless --disable-branching is specified. --pin only
    controls the starting revision, not whether a branch is created.

    Scenarios:
      Case A: no --pin, no --disable-branching  -> branch created, uses latest
      Case B: --pin, no --disable-branching      -> branch created, uses pinned revision
      Case C: no --pin, --disable-branching      -> no branch created, uses default latest
      Case D: --pin, --disable-branching         -> no branch created, uses pinned revision
    """
    # Create the main (parent) repository
    parent_repo: Lore = new_lore_repo()

    with parent_repo.open_file("parent-file.txt", "w+") as f:
        f.writelines(["parent content\n"])

    parent_repo.stage(scan=True)
    parent_repo.commit("Initial parent commit")
    parent_repo.push()

    # Create a feature branch in the parent repo so the current branch
    # is something other than main (to test that branch creation propagates)
    parent_repo.branch_create("feature-test")

    # Create 4 link target repositories, each with initial content on main

    # --- Link repo A (Case A: no pin, no disable-branching) ---
    link_repo_a = new_lore_repo()
    with link_repo_a.open_file("file-a.txt", "w+") as f:
        f.writelines(["link A content\n"])
    link_repo_a.stage(scan=True)
    link_repo_a.commit("Initial A")
    link_repo_a.push()

    # --- Link repo B (Case B: pin, no disable-branching) ---
    link_repo_b = new_lore_repo()
    with link_repo_b.open_file("file-b.txt", "w+") as f:
        f.writelines(["link B content\n"])
    link_repo_b.stage(scan=True)
    link_repo_b.commit("Initial B")
    link_repo_b.push()

    # Make a second commit so we can pin to the first one
    main_latest_b = link_repo_b.branch_info().local_latest
    with link_repo_b.open_file("file-b2.txt", "w+") as f:
        f.writelines(["link B second file\n"])
    link_repo_b.stage(scan=True)
    link_repo_b.commit("Second B commit")
    link_repo_b.push()

    # --- Link repo C (Case C: no pin, disable-branching) ---
    link_repo_c = new_lore_repo()
    with link_repo_c.open_file("file-c.txt", "w+") as f:
        f.writelines(["link C content\n"])
    link_repo_c.stage(scan=True)
    link_repo_c.commit("Initial C")
    link_repo_c.push()

    # --- Link repo D (Case D: pin, disable-branching) ---
    link_repo_d = new_lore_repo()
    with link_repo_d.open_file("file-d.txt", "w+") as f:
        f.writelines(["link D content\n"])
    link_repo_d.stage(scan=True)
    link_repo_d.commit("Initial D")
    link_repo_d.push()

    # Create a feature branch in repo D so we can pin to it
    link_repo_d.branch_create("pinned-branch")
    with link_repo_d.open_file("file-d2.txt", "w+") as f:
        f.writelines(["link D pinned branch content\n"])
    link_repo_d.stage(scan=True)
    link_repo_d.commit("D pinned branch commit")
    link_repo_d.push()
    pinned_branch_latest_d = link_repo_d.branch_info().local_latest

    # === Case A: link add without --pin, without --disable-branching ===
    parent_repo.link_add("link-a", link_repo_a.get_id(), "/")

    # Verify files are accessible
    assert parent_repo.file_exists("link-a/file-a.txt"), (
        "Case A: linked file should be accessible"
    )

    # Verify branch was created in linked repo
    branch_list_a = link_repo_a.branch_list()
    assert "feature-test" in branch_list_a.remote_branches, (
        "Case A: feature-test branch should be created in linked repo"
    )

    # Verify link list shows the feature-test branch
    link_output = parent_repo.link_list()
    pattern_a = rf"Link\s+{link_repo_a.get_id()}.*?Branch:\s+feature-test"
    assert re.search(pattern_a, link_output, re.DOTALL), (
        "Case A: link list should show feature-test as the branch"
    )

    # Verify no DisableAutoFollow flag
    pattern_a_flags = rf"Link\s+{link_repo_a.get_id()}.*?Flags:\s+None"
    assert re.search(pattern_a_flags, link_output, re.DOTALL), (
        "Case A: link should have no flags set"
    )

    parent_repo.commit("Add link A")
    parent_repo.push()

    # === Case B: link add with --pin, without --disable-branching ===
    parent_repo.link_add("link-b", link_repo_b.get_id(), "/", pin=f"{main_latest_b}")

    # Verify files from the pinned revision are present (only file-b.txt, not file-b2.txt)
    assert parent_repo.file_exists("link-b/file-b.txt"), (
        "Case B: pinned file should be accessible"
    )
    assert not parent_repo.file_exists("link-b/file-b2.txt"), (
        "Case B: file from later revision should NOT be present (pinned to earlier)"
    )

    # Verify branch was created in linked repo (branching enabled)
    branch_list_b = link_repo_b.branch_list()
    assert "feature-test" in branch_list_b.remote_branches, (
        "Case B: feature-test branch should be created in linked repo even with --pin"
    )

    # Verify link list shows the feature-test branch (not the pinned revision's branch)
    link_output = parent_repo.link_list()
    pattern_b = rf"Link\s+{link_repo_b.get_id()}.*?Branch:\s+feature-test"
    assert re.search(pattern_b, link_output, re.DOTALL), (
        "Case B: link list should show feature-test as the branch"
    )

    # Verify the revision is the pinned one
    pattern_b_rev = rf"Link\s+{link_repo_b.get_id()}.*?Revision:\s+{main_latest_b}"
    assert re.search(pattern_b_rev, link_output, re.DOTALL), (
        "Case B: link should be pinned to the specified revision"
    )

    # Verify no DisableAutoFollow flag
    pattern_b_flags = rf"Link\s+{link_repo_b.get_id()}.*?Flags:\s+None"
    assert re.search(pattern_b_flags, link_output, re.DOTALL), (
        "Case B: link should have no flags set"
    )

    parent_repo.commit("Add link B")
    parent_repo.push()

    # === Case C: link add without --pin, with --disable-branching ===
    parent_repo.link_add("link-c", link_repo_c.get_id(), "/", disable_branching=True)

    # Verify files are accessible
    assert parent_repo.file_exists("link-c/file-c.txt"), (
        "Case C: linked file should be accessible"
    )

    # Verify NO branch was created in linked repo
    branch_list_c = link_repo_c.branch_list()
    assert "feature-test" not in branch_list_c.remote_branches, (
        "Case C: feature-test branch should NOT be created when --disable-branching"
    )

    # Verify link list shows the default branch (main), not feature-test
    link_output = parent_repo.link_list()
    pattern_c = rf"Link\s+{link_repo_c.get_id()}.*?Branch:\s+main"
    assert re.search(pattern_c, link_output, re.DOTALL), (
        "Case C: link list should show main as the branch (default branch fallback)"
    )

    # Verify DisableAutoFollow flag is set
    pattern_c_flags = (
        rf"Link\s+{link_repo_c.get_id()}.*?Flags:\s+DisableAutoFollow \(0x1\)"
    )
    assert re.search(pattern_c_flags, link_output, re.DOTALL), (
        "Case C: link should have DisableAutoFollow flag"
    )

    parent_repo.commit("Add link C")
    parent_repo.push()

    # === Case D: link add with --pin, with --disable-branching ===
    parent_repo.link_add(
        "link-d",
        link_repo_d.get_id(),
        "/",
        pin="pinned-branch@LATEST",
        disable_branching=True,
    )

    # Verify files from the pinned branch are present
    assert parent_repo.file_exists("link-d/file-d.txt"), (
        "Case D: base file should be accessible"
    )
    assert parent_repo.file_exists("link-d/file-d2.txt"), (
        "Case D: pinned branch file should be accessible"
    )

    # Verify NO new branch was created in linked repo
    branch_list_d = link_repo_d.branch_list()
    assert "feature-test" not in branch_list_d.remote_branches, (
        "Case D: feature-test branch should NOT be created when --disable-branching"
    )

    # Verify link list shows the pinned branch (pinned-branch), not the parent's branch
    link_output = parent_repo.link_list()
    pattern_d = rf"Link\s+{link_repo_d.get_id()}.*?Branch:\s+pinned-branch"
    assert re.search(pattern_d, link_output, re.DOTALL), (
        "Case D: link list should show pinned-branch as the branch"
    )

    # Verify the revision is from the pinned branch
    pattern_d_rev = (
        rf"Link\s+{link_repo_d.get_id()}.*?Revision:\s+{pinned_branch_latest_d}"
    )
    assert re.search(pattern_d_rev, link_output, re.DOTALL), (
        "Case D: link should be pinned to the pinned-branch revision"
    )

    # Verify DisableAutoFollow flag is set
    pattern_d_flags = (
        rf"Link\s+{link_repo_d.get_id()}.*?Flags:\s+DisableAutoFollow \(0x1\)"
    )
    assert re.search(pattern_d_flags, link_output, re.DOTALL), (
        "Case D: link should have DisableAutoFollow flag"
    )

    parent_repo.commit("Add link D")
    parent_repo.push()

    # === Verify auto-follow only propagates to non-disabled links ===
    # Create a new branch — should propagate to A and B but NOT C and D
    parent_repo.branch_create("auto-follow-test")
    parent_repo.push()

    branch_list_a = link_repo_a.branch_list()
    assert "auto-follow-test" in branch_list_a.remote_branches, (
        "Auto-follow: branch should propagate to link A (no disable-branching)"
    )

    branch_list_b = link_repo_b.branch_list()
    assert "auto-follow-test" in branch_list_b.remote_branches, (
        "Auto-follow: branch should propagate to link B (no disable-branching)"
    )

    branch_list_c = link_repo_c.branch_list()
    assert "auto-follow-test" not in branch_list_c.remote_branches, (
        "Auto-follow: branch should NOT propagate to link C (disable-branching)"
    )

    branch_list_d = link_repo_d.branch_list()
    assert "auto-follow-test" not in branch_list_d.remote_branches, (
        "Auto-follow: branch should NOT propagate to link D (disable-branching)"
    )


def link_scoped_repository(new_lore_repo) -> tuple[Lore, str]:
    """A pushed repository holding one file of its own, with a pushed link mounted
    at the returned path holding one file of its own."""
    repo: Lore = new_lore_repo()

    with repo.open_file("parent-file.txt", "w+") as f:
        f.writelines(["parent content\n"])

    repo.stage(scan=True)
    repo.commit("Initial parent")
    repo.push()

    link_repo = new_lore_repo()

    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["initial link content\n"])

    link_repo.stage(scan=True)
    link_repo.commit("Initial link")
    link_repo.push()

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    return repo, link_path


@pytest.mark.smoke
def test_link_scoped_commit(new_lore_repo):
    """Test committing a single link independently and verifying parent pin is staged."""
    repo, link_path = link_scoped_repository(new_lore_repo)

    # Modify a file inside the link
    linked_file = f"{link_path}/link-file.txt"
    with repo.open_file(linked_file, "w+") as f:
        f.writelines(["modified link content\n"])

    repo.stage(linked_file)

    # Commit only the link
    output = repo.commit("Link-scoped commit", link=link_path)
    assert "Commit succeeded" in output

    # Parent should show staged changes (the updated link pin)
    status = repo.status()
    assert "Changes staged for commit" in status

    # Commit the parent to finalize
    output = repo.commit("Update link pin")
    assert "Commit succeeded" in output


@pytest.mark.smoke
def test_link_scoped_commit_reports_statistics(new_lore_repo):
    """A commit scoped to a link is a commit, and has to report what it cost. The
    scoped paths return before the one every other commit takes, so a report bound
    to that path alone would leave every link and layer commit silent."""
    repo, link_path = link_scoped_repository(new_lore_repo)

    linked_file = f"{link_path}/link-file.txt"
    with repo.open_file(linked_file, "w+") as f:
        f.writelines(["modified link content\n"])
    repo.stage(linked_file)

    stats = parse_commit_stats_json(
        repo.commit("Link-scoped commit", link=link_path, json=True, stats=1)
    )
    assert stats is not None, "a link-scoped commit must emit its statistics event"

    files = stats["files"]
    assert files["modified"] == 1, (
        f"the one file changed inside the link was committed as a modification, "
        f"got {files}"
    )
    assert files["files"] == 1, f"and it is the only file committed, got {files}"
    assert stats["fragments"]["fragmentsProduced"] > 0, (
        f"the commit wrote fragments, got {stats['fragments']}"
    )


@pytest.mark.smoke
def test_link_scoped_commit_no_parent_change(new_lore_repo):
    """Test that link-scoped commit preserves parent's own staged changes."""
    repo, link_path = link_scoped_repository(new_lore_repo)

    # Stage a parent file change
    with repo.open_file("parent-file.txt", "w+") as f:
        f.writelines(["modified parent content\n"])
    repo.stage("parent-file.txt")

    # Also modify a file in the link
    linked_file = f"{link_path}/link-file.txt"
    with repo.open_file(linked_file, "w+") as f:
        f.writelines(["modified link content\n"])
    repo.stage(linked_file)

    # Commit only the link
    output = repo.commit("Link-only commit", link=link_path)
    assert "Commit succeeded" in output

    # Parent should still have staged changes (parent-file.txt + link pin)
    status = repo.status()
    assert "Changes staged for commit" in status
    assert "parent-file.txt" in status

    # Commit the parent — should include both the file change and link pin
    output = repo.commit("Parent commit with file and link pin")
    assert "Commit succeeded" in output


@pytest.mark.smoke
def test_link_scoped_commit_not_a_link(new_lore_repo):
    """Test that --link on a non-link path fails."""
    repo: Lore = new_lore_repo()

    repo.make_dirs("regular-dir")
    with repo.open_file("regular-dir/file.txt", "w+") as f:
        f.writelines(["content\n"])

    repo.stage(scan=True)
    repo.commit("Initial commit")
    repo.push()

    # Modify a file and stage it
    with repo.open_file("regular-dir/file.txt", "w+") as f:
        f.writelines(["modified\n"])
    repo.stage(scan=True)

    # Try to commit with --link pointing to a regular directory
    with pytest.raises(NotALinkError):
        repo.commit("Should fail", link="regular-dir")


@pytest.mark.smoke
def test_link_scoped_commit_nothing_staged(new_lore_repo):
    """Test that --link with no staged changes in the link fails."""
    repo: Lore = new_lore_repo()

    with repo.open_file("parent-file.txt", "w+") as f:
        f.writelines(["parent content\n"])

    repo.stage(scan=True)
    repo.commit("Initial parent")
    repo.push()

    link_repo = new_lore_repo()

    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])

    link_repo.stage(scan=True)
    link_repo.commit("Initial link")
    link_repo.push()

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # No changes in the link — commit should fail
    with pytest.raises(NothingStagedError):
        repo.commit("Should fail", link=link_path)


@pytest.mark.smoke
def test_link_scoped_commit_consecutive(new_lore_repo):
    """Test two consecutive --link commits without committing the parent in between."""
    repo, link_path = link_scoped_repository(new_lore_repo)

    # First file change inside the link
    with repo.open_file(f"{link_path}/first.txt", "w+") as f:
        f.writelines(["first file\n"])
    repo.stage(f"{link_path}/first.txt")

    output = repo.commit("First link commit", link=link_path)
    assert "Commit succeeded" in output

    # Second file change inside the link — no parent commit in between
    with repo.open_file(f"{link_path}/second.txt", "w+") as f:
        f.writelines(["second file\n"])
    repo.stage(f"{link_path}/second.txt")

    output = repo.commit("Second link commit", link=link_path)
    assert "Commit succeeded" in output

    # Finalize parent
    output = repo.commit("Update link pin")
    assert "Commit succeeded" in output


@pytest.mark.smoke
def test_link_scoped_commit_push_propagates_to_link(new_lore_repo):
    """Pushing after a `commit --link` must push the linked repo's new revision.

    Regression test: `commit --link <path>` creates a new revision in the linked
    repo and advances its branch, but does NOT create a new revision in the parent.
    A subsequent `lore push` from the parent checked only whether the parent had
    new revisions to push; if not, it returned early without walking the link list,
    leaving the new link revision unpushed.
    """
    # Create link repository with initial content
    link_repo: Lore = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["initial link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link")
    link_repo.push()

    # Create parent repository and add link
    repo: Lore = new_lore_repo()
    with repo.open_file("parent-file.txt", "w+") as f:
        f.writelines(["parent content\n"])
    repo.stage(scan=True)
    repo.commit("Initial parent")
    repo.push()

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    # Snapshot parent's remote latest — should be unchanged after the link-scoped push
    parent_remote_before = repo.branch_info().remote_latest

    # Make a change inside the link, stage and commit with --link only
    with repo.open_file(f"{link_path}/link-file.txt", "w+") as f:
        f.writelines(["updated link content\n"])
    repo.stage(f"{link_path}/link-file.txt")

    link_commit_message = "Link-only update via --link"
    output = repo.commit(link_commit_message, link=link_path)
    assert "Commit succeeded" in output

    # Parent should have no new revision to push (the commit only advanced the link)
    assert repo.branch_info().local_latest == parent_remote_before, (
        "Parent should have no new revision after commit --link"
    )

    # Push from the parent — should propagate the link's new revision to the remote
    repo.push()

    # Pull the link's remote into the standalone clone we made earlier. If the
    # link revision was pushed, its message will now be the linked repo's latest.
    link_repo.sync()
    link_info = link_repo.revision_info(check=True, no_pager=True)
    assert link_info.message == link_commit_message, (
        f"Link revision created by `commit --link` was not pushed to the linked repository's remote. "
        f"Expected message '{link_commit_message}', got '{link_info.message}'"
    )


@pytest.mark.smoke
def test_link_update_subdirectory_source(new_lore_repo):
    """Test that updating a link pinned to a subdirectory only adds new files.

    Regression test: when a link's source_path is a subdirectory (e.g. TestFolder)
    rather than root, updating the pin to a newer revision used to re-add the entire
    source folder inside the link path (Restricted/TestFolder/...) instead of placing
    only the new files directly under the link path (Restricted/...).

    Reproduces the bug reported where:
      urc link add --pin <rev1> Restricted <remote> ./TestFolder/
      urc link update --pin <rev2> Restricted
    caused TestFolder to appear nested inside Restricted.
    """
    # Create source repository with a subdirectory containing initial files
    source_repo: Lore = new_lore_repo()

    source_repo.make_dirs("TestFolder")
    with source_repo.open_file("TestFolder/A.txt", "w+") as f:
        f.writelines(["file A content\n"])
    with source_repo.open_file("TestFolder/B.txt", "w+") as f:
        f.writelines(["file B content\n"])

    source_repo.stage(scan=True)
    source_repo.commit("Initial files in TestFolder")
    source_repo.push()

    initial_revision = source_repo.branch_info().local_latest

    # Add more files to TestFolder in a second commit
    with source_repo.open_file("TestFolder/C.txt", "w+") as f:
        f.writelines(["file C content\n"])
    with source_repo.open_file("TestFolder/D.txt", "w+") as f:
        f.writelines(["file D content\n"])

    source_repo.stage(scan=True)
    source_repo.commit("Added C and D to TestFolder")
    source_repo.push()

    updated_revision = source_repo.branch_info().local_latest

    # Create main repository with a link to source repo's TestFolder subdirectory
    main_repo: Lore = new_lore_repo()

    main_repo.make_dirs("Restricted")
    main_repo.stage(scan=True)
    main_repo.commit("Create Restricted directory")
    main_repo.push()

    # Add link: Restricted -> source_repo:TestFolder at the initial revision
    main_repo.link_add(
        "Restricted",
        source_repo.get_id(),
        "TestFolder",
        pin=initial_revision,
    )

    # Verify initial files appear directly under Restricted (not Restricted/TestFolder/)
    assert main_repo.file_exists("Restricted/A.txt"), (
        "A.txt should be directly under Restricted"
    )
    assert main_repo.file_exists("Restricted/B.txt"), (
        "B.txt should be directly under Restricted"
    )
    restricted_contents = os.listdir(os.path.join(main_repo.path, "Restricted"))
    assert sorted(restricted_contents) == ["A.txt", "B.txt"], (
        f"Restricted should contain only the linked files, got: {restricted_contents}"
    )

    main_repo.commit("Link added to Restricted folder")
    main_repo.push()

    # Update the link pin to the newer revision that has 2 additional files
    main_repo.link_update("Restricted", pin=updated_revision)

    # Verify all four files appear directly under Restricted
    assert main_repo.file_exists("Restricted/A.txt"), (
        "A.txt should still be under Restricted after update"
    )
    assert main_repo.file_exists("Restricted/B.txt"), (
        "B.txt should still be under Restricted after update"
    )
    assert main_repo.file_exists("Restricted/C.txt"), (
        "C.txt should be directly under Restricted after update"
    )
    assert main_repo.file_exists("Restricted/D.txt"), (
        "D.txt should be directly under Restricted after update"
    )

    # TestFolder contents should be mounted directly at Restricted, not nested
    restricted_contents = os.listdir(os.path.join(main_repo.path, "Restricted"))
    assert sorted(restricted_contents) == ["A.txt", "B.txt", "C.txt", "D.txt"], (
        f"Restricted should contain only the linked files, got: {restricted_contents}"
    )

    main_repo.commit("Link updated")
    main_repo.push()

    # Verify sync also works correctly
    sync_repo = main_repo.clone()

    assert sync_repo.file_exists("Restricted/A.txt"), (
        "Synced repo should have A.txt directly under Restricted"
    )
    assert sync_repo.file_exists("Restricted/B.txt"), (
        "Synced repo should have B.txt directly under Restricted"
    )
    assert sync_repo.file_exists("Restricted/C.txt"), (
        "Synced repo should have C.txt directly under Restricted"
    )
    assert sync_repo.file_exists("Restricted/D.txt"), (
        "Synced repo should have D.txt directly under Restricted"
    )
    sync_contents = os.listdir(os.path.join(sync_repo.path, "Restricted"))
    assert sorted(sync_contents) == ["A.txt", "B.txt", "C.txt", "D.txt"], (
        f"Synced Restricted should contain only the linked files, got: {sync_contents}"
    )


def test_link_merge_all(new_lore_repo):
    """Default merge (no flags) across main + linked repos.

    Both branches add non-conflicting files to the linked repo independently.
    Without proper multi-repo merge orchestration, the linked repo would only
    get one branch's files (whichever pin the main repo merge selects).
    With orchestration, the linked repo is independently merged and both
    branches' files appear.
    """
    urc: Lore = new_lore_repo()

    # Create initial file in main repo
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])

    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    # Create linked repository with initial content
    link_repo = new_lore_repo()

    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link repo base content\n"])

    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    # Add link to main repo
    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)

    urc.commit("Add link")
    urc.push()

    # Create feature branch (auto-follows into linked repo)
    urc.branch_create("feature-branch")

    # On feature branch, add a file in the linked repo
    with urc.open_file("feature-main-file.txt", "w+") as f:
        f.writelines(["feature branch main repo addition\n"])

    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature branch link repo addition\n"])

    urc.stage(scan=True)
    urc.commit("Feature branch additions")
    urc.push()

    # Switch back to main and add a DIFFERENT file in the linked repo
    urc.branch_switch("main")

    with urc.open_file("main-only-file.txt", "w+") as f:
        f.writelines(["main branch only addition\n"])

    with urc.open_file(f"{link_path}/main-link-file.txt", "w+") as f:
        f.writelines(["main branch link repo addition\n"])

    urc.stage(scan=True)
    urc.commit("Main branch additions")
    urc.push()

    # Default merge (no --link flag) — should merge all repos
    urc.branch_merge_start(
        "feature-branch",
        message="Merge feature-branch across all repos",
    )
    urc.push()

    # Verify: files from BOTH branches are present in the linked repo.
    # Without multi-repo merge orchestration, the linked repo's files
    # from the feature branch might appear locally (through the main repo's
    # tree merge) but the link pin would not point to a properly merged
    # linked repo revision.
    assert urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature branch link repo file should be present after default merge"
    )
    assert urc.file_exists(f"{link_path}/main-link-file.txt"), (
        "Main branch link repo file should still be present after default merge"
    )

    # Verify: main repo files from both branches are present
    assert urc.file_exists("feature-main-file.txt"), (
        "Feature branch main repo file should be present after default merge"
    )
    assert urc.file_exists("main-only-file.txt"), (
        "Main branch main repo file should still be present"
    )

    # Verify the link pin was properly merged by syncing to the new revision.
    # A sync re-realizes from the committed state (using link pins), so if the
    # link pin doesn't point to a merged revision, the feature file would vanish.
    urc.sync()
    assert urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "After sync, feature branch link repo file should still be present (pin is correct)"
    )
    assert urc.file_exists(f"{link_path}/main-link-file.txt"), (
        "After sync, main branch link repo file should still be present (pin is correct)"
    )

    # Verify post-merge state is clean
    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after merge commit - Got:\n{status}"
    )


def test_link_merge_abort_all(new_lore_repo):
    """Abort-all rolls back all link pins and main repo changes."""
    urc: Lore = new_lore_repo()

    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])
    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link repo base content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)
    urc.commit("Add link")
    urc.push()

    urc.branch_create("feature-branch")

    with urc.open_file("feature-main-file.txt", "w+") as f:
        f.writelines(["feature branch main repo addition\n"])
    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature branch link repo addition\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch additions")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file("main-only-file.txt", "w+") as f:
        f.writelines(["main branch only addition\n"])
    with urc.open_file(f"{link_path}/main-link-file.txt", "w+") as f:
        f.writelines(["main branch link repo addition\n"])
    urc.stage(scan=True)
    urc.commit("Main branch additions")
    urc.push()

    # Start merge with --no-commit so we can abort
    urc.branch_merge_start(
        "feature-branch",
        message="Merge feature-branch",
        no_commit=True,
    )

    # Verify merge brought in feature branch files
    assert urc.file_exists("feature-main-file.txt")
    assert urc.file_exists(f"{link_path}/feature-link-file.txt")

    # Abort the merge (no --link flag — abort all)
    urc.branch_merge_abort()

    # Verify: feature branch files are gone from BOTH repos
    assert not urc.file_exists("feature-main-file.txt"), (
        "Feature main file should be gone after abort"
    )
    assert not urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature link file should be gone after abort"
    )

    # Verify: main branch files are still present
    assert urc.file_exists("main-only-file.txt"), (
        "Main only file should still be present after abort"
    )
    assert urc.file_exists(f"{link_path}/main-link-file.txt"), (
        "Main link file should still be present after abort"
    )


def test_link_merge_abort_ignore_links(new_lore_repo):
    """Abort --ignore-links strips merge metadata but preserves link pin updates."""
    urc: Lore = new_lore_repo()

    with urc.open_file("shared-file.txt", "w+") as f:
        f.writelines(["base content\n"])
    urc.stage(scan=True)
    urc.commit("Initial commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link repo base content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)
    urc.commit("Add link")
    urc.push()

    urc.branch_create("feature-branch")

    # Feature branch: modify the shared file (will conflict) and add link file
    with urc.open_file("shared-file.txt", "w+") as f:
        f.writelines(["feature branch content\n"])
    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature branch link addition\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    urc.branch_switch("main")

    # Main branch: modify the same shared file (creates conflict)
    with urc.open_file("shared-file.txt", "w+") as f:
        f.writelines(["main branch content\n"])
    with urc.open_file(f"{link_path}/main-link-file.txt", "w+") as f:
        f.writelines(["main branch link addition\n"])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

    # Start merge — should have conflicts in main but link merges cleanly
    urc.branch_merge_start(
        "feature-branch",
        message="Merge feature-branch",
        no_commit=True,
    )

    # Abort only main merge, keeping link pin updates
    urc.branch_merge_abort(ignore_links=True)

    # Verify: link files from both branches are still present
    assert urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature link file should be preserved after --ignore-links abort"
    )
    assert urc.file_exists(f"{link_path}/main-link-file.txt"), (
        "Main link file should be preserved after --ignore-links abort"
    )

    # Verify: the staged state is committable (no merge flags)
    urc.commit("Commit link-only changes after selective abort")
    urc.push()

    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after commit - Got:\n{status}"
    )


def test_link_merge_resume(new_lore_repo):
    """Resume detection: a merge with --no-commit leaves staged state with
    LinkMergeState entries. Re-running merge (after commit) shows that the
    link merge infrastructure correctly tracks merged links."""
    urc: Lore = new_lore_repo()

    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])
    urc.stage(scan=True)
    urc.commit("Initial commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link repo base content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)
    urc.commit("Add link")
    urc.push()

    urc.branch_create("feature-branch")

    with urc.open_file("feature-main-file.txt", "w+") as f:
        f.writelines(["feature main addition\n"])
    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature link addition\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file("main-only-file.txt", "w+") as f:
        f.writelines(["main only addition\n"])
    with urc.open_file(f"{link_path}/main-link-file.txt", "w+") as f:
        f.writelines(["main link addition\n"])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

    # Start merge with --no-commit to keep staged state
    urc.branch_merge_start(
        "feature-branch",
        message="Merge feature-branch",
        no_commit=True,
    )

    # Verify the merge brought in both branches' files
    assert urc.file_exists("feature-main-file.txt")
    assert urc.file_exists(f"{link_path}/feature-link-file.txt")

    # Commit the merge manually (like the user would after reviewing)
    urc.commit("Manual merge commit")
    urc.push()

    # Verify everything persists through commit + sync
    urc.sync()
    assert urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature link file should persist after manual commit and sync"
    )
    assert urc.file_exists(f"{link_path}/main-link-file.txt"), (
        "Main link file should persist after manual commit and sync"
    )

    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean - Got:\n{status}"
    )


def test_link_merge_dry_run(new_lore_repo):
    """Dry-run previews changes across all repos without modifying state."""
    urc: Lore = new_lore_repo()

    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])
    urc.stage(scan=True)
    urc.commit("Initial main repo commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link repo base content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)
    urc.commit("Add link")
    urc.push()

    urc.branch_create("feature-branch")

    with urc.open_file("feature-main-file.txt", "w+") as f:
        f.writelines(["feature branch main repo addition\n"])
    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature branch link repo addition\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch additions")
    urc.push()

    urc.branch_switch("main")

    # Run merge with --dry-run
    urc.branch_merge_start(
        "feature-branch",
        message="Dry run merge",
        dry_run=True,
    )

    # Verify: no files from feature branch appeared
    assert not urc.file_exists("feature-main-file.txt"), (
        "Feature main file should NOT exist after dry-run"
    )
    assert not urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature link file should NOT exist after dry-run"
    )

    # Verify: no staged state remains
    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after dry-run - Got:\n{status}"
    )


def test_link_merge_all_file_conflict_resolve_in_place(new_lore_repo):
    """Default merge stages link file conflicts in place; resolve + commit finishes
    the merge without a `--link` re-entry."""
    import re

    urc: Lore = new_lore_repo()

    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])
    urc.stage(scan=True)
    urc.commit("Initial commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("shared-link-file.txt", "w+") as f:
        f.writelines(["base content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    urc.branch_create("feature-branch")
    with urc.open_file(f"{link_path}/shared-link-file.txt", "w+") as f:
        f.writelines(["feature branch content\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/shared-link-file.txt", "w+") as f:
        f.writelines(["main branch content\n"])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

    # Capture the pre-merge link pin (main's committed link revision)
    # so we can verify it advances after the merge resolves.
    pin_before = re.search(
        rf"{link_repo.get_id()}.*?Revision:\s*(\w+)", urc.link_list(), re.DOTALL
    )
    assert pin_before

    # Default merge no longer fails — it stages the conflict in place.
    # The merge command must succeed (return without exception) even though
    # the linked file has conflict markers; main's merge is skipped, the link
    # pin is updated to the merged-but-conflicted revision, and the parent
    # state carries StateFlags::Conflict so `merge resolve` knows what to do.
    urc.branch_merge_start("feature-branch", message="Merge with link conflict")

    # The conflicting file exists at the mount path with conflict markers
    conflict_file = f"{link_path}/shared-link-file.txt"
    assert urc.file_exists(conflict_file), (
        "Conflicted link file should exist at mount path"
    )
    with urc.open_file(conflict_file, "r") as f:
        content = f.read()
    assert "<<<<<<<" in content or ">>>>>>>" in content, (
        f"Conflict markers should be present in {conflict_file} - Got:\n{content}"
    )

    # Resolve the conflict in place — no `--link` re-entry needed
    with urc.open_file(conflict_file, "w+") as f:
        f.writelines(["resolved content\n"])
    urc.branch_merge_resolve(conflict_file)

    # Commit finishes the merge: link committed first (commit_link_node),
    # then parent commit incorporates the new link pin.
    urc.commit("Merge feature-branch with resolved link conflict")
    urc.push()

    # Working tree clean
    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after commit - Got:\n{status}"
    )

    # Resolved content survived the commit
    with urc.open_file(conflict_file, "r") as f:
        content = f.read()
    assert "resolved content" in content, (
        f"File should have resolved content - Got: {content}"
    )

    # Link pin advanced to a new revision
    link_list_after = urc.link_list()
    pin_after = re.search(
        rf"{link_repo.get_id()}.*?Revision:\s*(\w+)", link_list_after, re.DOTALL
    )
    assert pin_after, f"Post-merge link pin not found in: {link_list_after}"
    assert pin_before.group(1) != pin_after.group(1), (
        f"Link pin should have advanced after merge.\n"
        f"Before: {pin_before.group(1)}\nAfter: {pin_after.group(1)}"
    )


def test_link_merge_all_file_conflict_abort(new_lore_repo):
    """Default merge with a link file conflict can be aborted, restoring everything."""
    import re

    urc: Lore = new_lore_repo()

    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])
    urc.stage(scan=True)
    urc.commit("Initial commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("shared-link-file.txt", "w+") as f:
        f.writelines(["base content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    urc.branch_create("feature-branch")
    with urc.open_file(f"{link_path}/shared-link-file.txt", "w+") as f:
        f.writelines(["feature branch content\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/shared-link-file.txt", "w+") as f:
        f.writelines(["main branch content\n"])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

    # Capture pin AFTER main's commit (this is the value abort should restore to)
    pin_before = re.search(
        rf"{link_repo.get_id()}.*?Revision:\s*(\w+)", urc.link_list(), re.DOTALL
    )
    assert pin_before

    # Default merge stages the conflict in place
    urc.branch_merge_start("feature-branch", message="Merge with link conflict")

    # Abort with no flags — should roll back the conflicted link's pin and
    # restore on-disk content from main's pre-merge state.
    urc.branch_merge_abort()

    # Working tree matches pre-merge state: main's content is back, no markers
    conflict_file = f"{link_path}/shared-link-file.txt"
    with urc.open_file(conflict_file, "r") as f:
        content = f.read()
    assert "main branch content" in content, (
        f"After abort, file should have main's pre-merge content - Got: {content}"
    )
    assert "<<<<<<<" not in content, (
        f"After abort, conflict markers should be gone - Got: {content}"
    )

    # Link pin restored
    pin_after = re.search(
        rf"{link_repo.get_id()}.*?Revision:\s*(\w+)", urc.link_list(), re.DOTALL
    )
    assert pin_after
    assert pin_before.group(1) == pin_after.group(1), (
        f"Link pin should match pre-merge value after abort.\n"
        f"Before: {pin_before.group(1)}\nAfter: {pin_after.group(1)}"
    )

    # No staged state left behind
    status = urc.status()
    assert "merge" not in status.lower() or "in progress" not in status.lower(), (
        f"No merge should be in progress after abort - Got:\n{status}"
    )


def test_link_merge_all_mixed_clean_and_conflict(new_lore_repo):
    """One link merges cleanly, another has a file conflict.
    The clean link's merge survives while the user resolves the conflict in place."""
    import re

    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    # Link A — will merge cleanly (changes on different files per branch)
    link_a = new_lore_repo()
    with link_a.open_file("a-base.txt", "w+") as f:
        f.writelines(["a base\n"])
    link_a.stage(scan=True)
    link_a.commit("Initial link A commit")
    link_a.push()

    # Link B — will conflict (both branches modify the same file)
    link_b = new_lore_repo()
    with link_b.open_file("b-shared.txt", "w+") as f:
        f.writelines(["b base\n"])
    link_b.stage(scan=True)
    link_b.commit("Initial link B commit")
    link_b.push()

    path_a = "libs/a"
    path_b = "libs/b"
    urc.link_add(path_a, link_a.get_id(), "/")
    urc.link_add(path_b, link_b.get_id(), "/")
    urc.commit("Add links")
    urc.push()

    # Feature branch: A gets a new file, B's shared file is changed
    urc.branch_create("feature-branch")
    with urc.open_file(f"{path_a}/feature-only.txt", "w+") as f:
        f.writelines(["feature only\n"])
    with urc.open_file(f"{path_b}/b-shared.txt", "w+") as f:
        f.writelines(["feature B content\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    # Main: A gets a different new file (no conflict with feature),
    # B's shared file is changed differently (conflict with feature)
    urc.branch_switch("main")
    with urc.open_file(f"{path_a}/main-only.txt", "w+") as f:
        f.writelines(["main only\n"])
    with urc.open_file(f"{path_b}/b-shared.txt", "w+") as f:
        f.writelines(["main B content\n"])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

    # Capture pre-merge pins after main's commit
    pin_a_before = re.search(
        rf"{link_a.get_id()}.*?Revision:\s*(\w+)", urc.link_list(), re.DOTALL
    )
    pin_b_before = re.search(
        rf"{link_b.get_id()}.*?Revision:\s*(\w+)", urc.link_list(), re.DOTALL
    )
    assert pin_a_before and pin_b_before

    # Default merge: link A merges cleanly, link B conflicts.
    # Both link pins should be staged (A advanced to merged revision,
    # B advanced to merged-but-conflicted revision). Main is skipped.
    urc.branch_merge_start("feature-branch", message="Merge feature-branch")

    # Both links' file additions visible (A's merged, B's conflicted file present)
    assert urc.file_exists(f"{path_a}/feature-only.txt")
    assert urc.file_exists(f"{path_a}/main-only.txt")
    assert urc.file_exists(f"{path_b}/b-shared.txt")

    # Resolve the B conflict
    with urc.open_file(f"{path_b}/b-shared.txt", "w+") as f:
        f.writelines(["resolved B content\n"])
    urc.branch_merge_resolve(f"{path_b}/b-shared.txt")

    urc.commit("Resolve B; merge done")
    urc.push()

    # Both link pins advanced from pre-merge state
    pin_a_after = re.search(
        rf"{link_a.get_id()}.*?Revision:\s*(\w+)", urc.link_list(), re.DOTALL
    )
    pin_b_after = re.search(
        rf"{link_b.get_id()}.*?Revision:\s*(\w+)", urc.link_list(), re.DOTALL
    )
    assert pin_a_after and pin_b_after
    assert pin_a_before.group(1) != pin_a_after.group(1), (
        "Link A pin should have advanced (clean merge preserved)"
    )
    assert pin_b_before.group(1) != pin_b_after.group(1), (
        "Link B pin should have advanced (conflict resolved)"
    )

    # Final content reflects expected resolution
    with urc.open_file(f"{path_b}/b-shared.txt", "r") as f:
        b_content = f.read()
    assert "resolved B content" in b_content

    # Working tree clean
    status = urc.status()
    assert "local branch in sync with remote" in status.lower()


def test_link_merge_all_abort_specific_link(new_lore_repo):
    """Abort --link during a multi-repo merge rolls back only that link."""
    urc: Lore = new_lore_repo()

    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main repo base content\n"])
    urc.stage(scan=True)
    urc.commit("Initial commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link repo base content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link repo commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/", debug=True)
    urc.commit("Add link")
    urc.push()

    urc.branch_create("feature-branch")

    with urc.open_file("feature-main-file.txt", "w+") as f:
        f.writelines(["feature main addition\n"])
    with urc.open_file(f"{link_path}/feature-link-file.txt", "w+") as f:
        f.writelines(["feature link addition\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch changes")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file("main-only-file.txt", "w+") as f:
        f.writelines(["main only addition\n"])
    with urc.open_file(f"{link_path}/main-link-file.txt", "w+") as f:
        f.writelines(["main link addition\n"])
    urc.stage(scan=True)
    urc.commit("Main branch changes")
    urc.push()

    # Start merge with --no-commit
    urc.branch_merge_start(
        "feature-branch",
        message="Merge feature-branch",
        no_commit=True,
    )

    # Verify merge brought in feature files
    assert urc.file_exists("feature-main-file.txt")
    assert urc.file_exists(f"{link_path}/feature-link-file.txt")

    # Abort only the link merge
    urc.branch_merge_abort(link=link_path)

    # Verify: link feature file is gone (link reverted)
    assert not urc.file_exists(f"{link_path}/feature-link-file.txt"), (
        "Feature link file should be gone after link-specific abort"
    )

    # Verify: main feature file is still present (main merge preserved)
    assert urc.file_exists("feature-main-file.txt"), (
        "Feature main file should still be present (main merge not aborted)"
    )

    # Verify: main branch link file is still present
    assert urc.file_exists(f"{link_path}/main-link-file.txt"), (
        "Main link file should still be present"
    )


@pytest.mark.smoke
def test_implicit_link_branch(new_lore_repo):
    """Test the implicit link branch convention.

    When a link is added with branching enabled (the default), the
    LinkReference stores branch = zero. All operations that read
    the branch resolve zero to the parent's current branch ID.

    Branch creation in a repo with zero-branch links must not produce
    additional revisions in the parent (no bookkeeping revisions).
    """
    # Create the parent repository
    parent: Lore = new_lore_repo()
    parent.write_commit_push("Initial parent", {"parent.txt": "parent content\n"})

    # Create the linked repository
    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"linked.txt": "linked content\n"})

    # Add the link (branching enabled by default — stores zero branch)
    parent.link_add("my-link", link_repo.get_id(), "/")
    parent.commit("Add link")
    parent.push()

    # Verify link list displays correct branch name (not empty/zero)
    link_output = parent.link_list()
    assert link_repo.get_id() in link_output, "Link should appear in link list"
    # The branch name should be the parent's branch name (e.g. 'main'),
    # not empty or a zero UUID
    zero_uuid = "00000000000000000000000000000000"
    assert zero_uuid not in link_output, (
        "link list should resolve zero branch to actual branch, not show zero UUID"
    )

    # Record revision count before branch creation
    history_before = parent.history()
    rev_count_before = len(history_before)

    # Create a new branch — should NOT produce a bookkeeping revision
    parent.branch_create("feature-branch")

    # Check revision count after branch creation
    history_after = parent.history()
    rev_count_after = len(history_after)

    assert rev_count_after == rev_count_before, (
        f"Branch creation should not produce bookkeeping revisions. "
        f"Before: {rev_count_before}, After: {rev_count_after}"
    )

    # Commit changes in the linked repo on the new branch
    with parent.open_file("my-link/new-file.txt", "w+") as f:
        f.write("new file in link\n")
    parent.stage(scan=True)
    parent.commit("Commit in link on feature branch")

    # Push should succeed with zero-branch link
    parent.push()

    # Verify link list still shows correct branch after branch switch
    link_output_feature = parent.link_list()
    assert zero_uuid not in link_output_feature, (
        "link list should still resolve zero branch after branch creation"
    )


@pytest.mark.smoke
def test_implicit_link_branch_disable_branching(new_lore_repo):
    """Test that --disable-branching still stores an explicit branch."""
    parent: Lore = new_lore_repo()
    parent.write_commit_push("Initial parent", {"parent.txt": "parent content\n"})

    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"linked.txt": "linked content\n"})

    # Add with disable-branching — should store explicit branch
    parent.link_add(
        "my-link", link_repo.get_id(), "/", disable_branching=True
    )
    parent.commit("Add link with disable-branching")
    parent.push()

    # link list should still show a valid branch
    link_output = parent.link_list()
    assert link_repo.get_id() in link_output, "Link should appear in link list"


def test_link_merge_all_multiple_links(new_lore_repo):
    """Default merge works across main + multiple linked repos."""
    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_a = new_lore_repo()
    with link_a.open_file("a-file.txt", "w+") as f:
        f.writelines(["a base\n"])
    link_a.stage(scan=True)
    link_a.commit("Initial link A commit")
    link_a.push()

    link_b = new_lore_repo()
    with link_b.open_file("b-file.txt", "w+") as f:
        f.writelines(["b base\n"])
    link_b.stage(scan=True)
    link_b.commit("Initial link B commit")
    link_b.push()

    path_a = "libs/a"
    path_b = "libs/b"
    urc.link_add(path_a, link_a.get_id(), "/")
    urc.link_add(path_b, link_b.get_id(), "/")
    urc.commit("Add both links")
    urc.push()

    # Feature branch: add file in main, link A, and link B
    urc.branch_create("feature-branch")
    with urc.open_file("feature-main.txt", "w+") as f:
        f.writelines(["feature main\n"])
    with urc.open_file(f"{path_a}/feature-a.txt", "w+") as f:
        f.writelines(["feature a\n"])
    with urc.open_file(f"{path_b}/feature-b.txt", "w+") as f:
        f.writelines(["feature b\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch additions across all repos")
    urc.push()

    # Main branch: add different files in all three repos
    urc.branch_switch("main")
    with urc.open_file("main-only.txt", "w+") as f:
        f.writelines(["main only\n"])
    with urc.open_file(f"{path_a}/main-a.txt", "w+") as f:
        f.writelines(["main a\n"])
    with urc.open_file(f"{path_b}/main-b.txt", "w+") as f:
        f.writelines(["main b\n"])
    urc.stage(scan=True)
    urc.commit("Main branch additions across all repos")
    urc.push()

    # Default merge across all three repos
    urc.branch_merge_start("feature-branch", message="Merge all")
    urc.push()

    # All files from both branches, in all three repos, should be present
    for f in [
        "feature-main.txt",
        "main-only.txt",
        f"{path_a}/feature-a.txt",
        f"{path_a}/main-a.txt",
        f"{path_b}/feature-b.txt",
        f"{path_b}/main-b.txt",
    ]:
        assert urc.file_exists(f), f"{f} should be present after multi-link merge"

    # Sync to verify pins point at properly merged linked repo revisions
    urc.sync()
    assert urc.file_exists(f"{path_a}/feature-a.txt"), (
        "Link A feature file should persist after sync (pin is correct)"
    )
    assert urc.file_exists(f"{path_b}/feature-b.txt"), (
        "Link B feature file should persist after sync (pin is correct)"
    )


def test_link_merge_all_mixed_eligibility(new_lore_repo):
    """Default merge skips DisableAutoFollow links but still merges eligible ones."""
    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_follow = new_lore_repo()
    with link_follow.open_file("follow.txt", "w+") as f:
        f.writelines(["follow base\n"])
    link_follow.stage(scan=True)
    link_follow.commit("Initial follow link commit")
    link_follow.push()

    link_fixed = new_lore_repo()
    with link_fixed.open_file("fixed.txt", "w+") as f:
        f.writelines(["fixed base\n"])
    link_fixed.stage(scan=True)
    link_fixed.commit("Initial fixed link commit")
    link_fixed.push()

    path_follow = "libs/follow"
    path_fixed = "libs/fixed"
    urc.link_add(path_follow, link_follow.get_id(), "/")
    # disable_branching → no auto-follow, pin stays fixed even across branches
    urc.link_add(path_fixed, link_fixed.get_id(), "/", disable_branching=True)
    urc.commit("Add links (one follow, one fixed)")
    urc.push()

    # Record the fixed link's pin revision — should be unchanged after merge
    link_list_before = urc.link_list()

    # Feature branch: add file in auto-follow link only
    urc.branch_create("feature-branch")
    with urc.open_file(f"{path_follow}/feature-follow.txt", "w+") as f:
        f.writelines(["feature follow\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch addition to follow link")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file("main-only.txt", "w+") as f:
        f.writelines(["main only\n"])
    urc.stage(scan=True)
    urc.commit("Main branch addition")
    urc.push()

    # Default merge — follow link should merge, fixed link should be skipped
    urc.branch_merge_start("feature-branch", message="Mixed eligibility merge")
    urc.push()

    # Follow link's feature file should be present
    assert urc.file_exists(f"{path_follow}/feature-follow.txt"), (
        "Auto-follow link should have been merged"
    )
    # Main repo's feature file should also be present
    assert urc.file_exists("main-only.txt"), "Main file should still be present"

    # Fixed link should still have only its base content
    assert urc.file_exists(f"{path_fixed}/fixed.txt"), (
        "Fixed link base file should persist"
    )

    # The fixed link's pin revision should be unchanged (captured via link list)
    link_list_after = urc.link_list()
    # Extract fixed link's revision from both snapshots and confirm equal
    # The link list output contains "Revision: <hash>" per link
    import re

    def extract_revs(text: str, repo_id: str) -> list[str]:
        # Find occurrences of the repo_id and capture the following Revision line
        revs = []
        for m in re.finditer(rf"{repo_id}.*?Revision:\s*(\w+)", text, re.DOTALL):
            revs.append(m.group(1))
        return revs

    fixed_before = extract_revs(link_list_before, link_fixed.get_id())
    fixed_after = extract_revs(link_list_after, link_fixed.get_id())
    assert fixed_before and fixed_after, (
        f"Should find fixed link revision in both snapshots"
    )
    assert fixed_before[0] == fixed_after[0], (
        f"Fixed (DisableAutoFollow) link pin should not change.\n"
        f"Before: {fixed_before[0]}\nAfter: {fixed_after[0]}"
    )


def test_link_merge_all_preserves_tracked_branches(new_lore_repo):
    """After default merge, each link's tracked branch stays on main (not feature-branch)."""
    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_a = new_lore_repo()
    with link_a.open_file("a.txt", "w+") as f:
        f.writelines(["a base\n"])
    link_a.stage(scan=True)
    link_a.commit("Initial link A commit")
    link_a.push()

    link_b = new_lore_repo()
    with link_b.open_file("b.txt", "w+") as f:
        f.writelines(["b base\n"])
    link_b.stage(scan=True)
    link_b.commit("Initial link B commit")
    link_b.push()

    urc.link_add("libs/a", link_a.get_id(), "/")
    urc.link_add("libs/b", link_b.get_id(), "/")
    urc.commit("Add links")
    urc.push()

    urc.branch_create("feature-branch")
    with urc.open_file("libs/a/feature-a.txt", "w+") as f:
        f.writelines(["feature a\n"])
    with urc.open_file("libs/b/feature-b.txt", "w+") as f:
        f.writelines(["feature b\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch additions")
    urc.push()

    urc.branch_switch("main")
    urc.branch_merge_start("feature-branch", message="Default merge")
    urc.push()

    # After merge on main, both links should still track main (not feature-branch)
    link_list_after = urc.link_list()
    assert "feature-branch" not in link_list_after, (
        f"No link should track 'feature-branch' after merge.\nGot: {link_list_after}"
    )
    # "main" should appear (as tracked branch of each link)
    assert "main" in link_list_after, (
        f"Links should track 'main'.\nGot: {link_list_after}"
    )


def test_link_merge_abort_all_restores_link_pins(new_lore_repo):
    """After branch merge abort, each link's pin/branch matches the pre-merge snapshot."""
    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_a = new_lore_repo()
    with link_a.open_file("a.txt", "w+") as f:
        f.writelines(["a base\n"])
    link_a.stage(scan=True)
    link_a.commit("Initial link A commit")
    link_a.push()

    link_b = new_lore_repo()
    with link_b.open_file("b.txt", "w+") as f:
        f.writelines(["b base\n"])
    link_b.stage(scan=True)
    link_b.commit("Initial link B commit")
    link_b.push()

    urc.link_add("libs/a", link_a.get_id(), "/")
    urc.link_add("libs/b", link_b.get_id(), "/")
    urc.commit("Add links")
    urc.push()

    # Snapshot before any feature branch activity
    link_list_before = urc.link_list()

    urc.branch_create("feature-branch")
    with urc.open_file("libs/a/feature-a.txt", "w+") as f:
        f.writelines(["feature a\n"])
    with urc.open_file("libs/b/feature-b.txt", "w+") as f:
        f.writelines(["feature b\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch additions")
    urc.push()

    urc.branch_switch("main")
    urc.branch_merge_start(
        "feature-branch", message="Default merge", no_commit=True
    )

    # Verify feature files are present during the pending merge
    assert urc.file_exists("libs/a/feature-a.txt")
    assert urc.file_exists("libs/b/feature-b.txt")

    urc.branch_merge_abort()

    # Files should be rolled back for both links
    assert not urc.file_exists("libs/a/feature-a.txt"), (
        "Link A feature file should be gone after abort"
    )
    assert not urc.file_exists("libs/b/feature-b.txt"), (
        "Link B feature file should be gone after abort"
    )

    # Link list should match the pre-merge snapshot for both links
    link_list_after = urc.link_list()
    import re

    def extract_revs(text: str, repo_id: str) -> list[str]:
        revs = []
        for m in re.finditer(rf"{repo_id}.*?Revision:\s*(\w+)", text, re.DOTALL):
            revs.append(m.group(1))
        return revs

    for repo_id in [link_a.get_id(), link_b.get_id()]:
        before_revs = extract_revs(link_list_before, repo_id)
        after_revs = extract_revs(link_list_after, repo_id)
        assert before_revs and after_revs, (
            f"Should find {repo_id} revision in both snapshots"
        )
        assert before_revs[0] == after_revs[0], (
            f"Link {repo_id} pin should be restored after abort.\n"
            f"Before: {before_revs[0]}\nAfter: {after_revs[0]}"
        )

    # No merge should still be in progress
    status = urc.status()
    assert "pending merge" not in status.lower(), (
        f"No merge should be in progress after abort - Got:\n{status}"
    )


def test_link_merge_all_no_link_changes(new_lore_repo):
    """Default merge succeeds when only the main repo has diverged and linked repo is untouched."""
    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # Capture link pin revision before any divergence — should stay unchanged
    link_list_before = urc.link_list()

    # Feature branch: modify main repo only, don't touch the link
    urc.branch_create("feature-branch")
    with urc.open_file("feature-main.txt", "w+") as f:
        f.writelines(["feature main\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch main-only addition")
    urc.push()

    # Main branch: modify main repo only too, don't touch the link
    urc.branch_switch("main")
    with urc.open_file("main-only.txt", "w+") as f:
        f.writelines(["main only\n"])
    urc.stage(scan=True)
    urc.commit("Main branch main-only addition")
    urc.push()

    # Default merge — linked repo has no divergence, should succeed cleanly
    urc.branch_merge_start("feature-branch", message="No-link-divergence merge")
    urc.push()

    # Main files from both branches present
    assert urc.file_exists("feature-main.txt")
    assert urc.file_exists("main-only.txt")

    # Link pin should not have changed (no divergence → no new linked revision)
    link_list_after = urc.link_list()
    import re

    before = re.search(rf"{link_repo.get_id()}.*?Revision:\s*(\w+)", link_list_before, re.DOTALL)
    after = re.search(rf"{link_repo.get_id()}.*?Revision:\s*(\w+)", link_list_after, re.DOTALL)
    assert before and after
    assert before.group(1) == after.group(1), (
        f"Link pin should be unchanged when linked repo hasn't diverged.\n"
        f"Before: {before.group(1)}\nAfter: {after.group(1)}"
    )


def test_link_merge_all_sequential(new_lore_repo):
    """Two sequential default merges from the same feature branch succeed."""
    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link base\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # Round 1: feature branch → main, both via default merge
    urc.branch_create("feature-branch")
    with urc.open_file("round1-main.txt", "w+") as f:
        f.writelines(["round1 main\n"])
    with urc.open_file(f"{link_path}/round1-link.txt", "w+") as f:
        f.writelines(["round1 link\n"])
    urc.stage(scan=True)
    urc.commit("Round 1 additions")
    urc.push()

    urc.branch_switch("main")
    urc.branch_merge_start("feature-branch", message="Round 1 merge")
    urc.push()

    assert urc.file_exists("round1-main.txt")
    assert urc.file_exists(f"{link_path}/round1-link.txt")

    # Round 2: sync feature from main, add more, merge again
    urc.branch_switch("feature-branch")
    urc.sync()
    urc.branch_merge_start("main", message="Pull main into feature")
    urc.push()

    with urc.open_file("round2-main.txt", "w+") as f:
        f.writelines(["round2 main\n"])
    with urc.open_file(f"{link_path}/round2-link.txt", "w+") as f:
        f.writelines(["round2 link\n"])
    urc.stage(scan=True)
    urc.commit("Round 2 additions")
    urc.push()

    urc.branch_switch("main")
    urc.branch_merge_start("feature-branch", message="Round 2 merge")
    urc.push()

    # All files from both rounds should be present
    for f in [
        "round1-main.txt",
        "round2-main.txt",
        f"{link_path}/round1-link.txt",
        f"{link_path}/round2-link.txt",
    ]:
        assert urc.file_exists(f), f"{f} should be present after sequential merges"

    # Verify the pin is correct (sync realizes from link pin)
    urc.sync()
    assert urc.file_exists(f"{link_path}/round1-link.txt")
    assert urc.file_exists(f"{link_path}/round2-link.txt")


def test_link_merge_all_push_and_clone(new_lore_repo):
    """After default merge and push, a fresh clone correctly follows link pins."""
    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link base\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # Divergent content in the linked repo across branches
    urc.branch_create("feature-branch")
    with urc.open_file(f"{link_path}/feature-only.txt", "w+") as f:
        f.writelines(["feature only\n"])
    urc.stage(scan=True)
    urc.commit("Feature addition in linked repo")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/main-only.txt", "w+") as f:
        f.writelines(["main only\n"])
    urc.stage(scan=True)
    urc.commit("Main addition in linked repo")
    urc.push()

    urc.branch_merge_start("feature-branch", message="Default merge")
    urc.push()

    # A fresh clone of the parent should see both branches' files in the linked repo
    clone = urc.clone()
    assert clone.file_exists(f"{link_path}/feature-only.txt"), (
        "Clone should have feature branch linked file (pin points at merged revision)"
    )
    assert clone.file_exists(f"{link_path}/main-only.txt"), (
        "Clone should have main branch linked file (pin points at merged revision)"
    )


def test_link_merge_abort_ignore_links_no_conflicts(new_lore_repo):
    """abort --ignore-links on a clean merge keeps link pin updates as staged changes."""
    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link base\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # No conflicts: both branches add independent files in the linked repo
    urc.branch_create("feature-branch")
    with urc.open_file(f"{link_path}/feature-link.txt", "w+") as f:
        f.writelines(["feature link\n"])
    with urc.open_file("feature-main.txt", "w+") as f:
        f.writelines(["feature main\n"])
    urc.stage(scan=True)
    urc.commit("Feature additions")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/main-link.txt", "w+") as f:
        f.writelines(["main link\n"])
    urc.stage(scan=True)
    urc.commit("Main addition")
    urc.push()

    # Start merge with --no-commit; no conflicts in main or link
    urc.branch_merge_start(
        "feature-branch", message="Clean default merge", no_commit=True
    )

    # Before selective abort: feature files are present
    assert urc.file_exists(f"{link_path}/feature-link.txt")
    assert urc.file_exists("feature-main.txt")

    # Selective main abort — keeps link pin update, drops main merge
    urc.branch_merge_abort(ignore_links=True)

    # Link merge result should remain
    assert urc.file_exists(f"{link_path}/feature-link.txt"), (
        "Feature link file should be preserved after --ignore-links abort"
    )
    assert urc.file_exists(f"{link_path}/main-link.txt"), (
        "Main link file should still be present after --ignore-links abort"
    )

    # Main merge should be reverted
    assert not urc.file_exists("feature-main.txt"), (
        "Feature main file should be gone after --ignore-links abort"
    )

    # The remaining staged state should be committable without merge flags
    urc.commit("Commit only the link pin updates")
    urc.push()

    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after commit - Got:\n{status}"
    )


def test_link_merge_abort_ignore_links_with_link_conflicts(new_lore_repo):
    """A link merge that produced file conflicts, then aborted with --ignore-links,
    must not leave .mine/.theirs/.base sidecars or marker bytes orphaned in the
    link mount. The parent has no merge metadata after the abort+re-pin, so any
    leftover artifacts would be unattributable."""
    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("shared.txt", "w+") as f:
        f.writelines(["link base\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # Feature branch: modify the shared file via mount
    urc.branch_create("feature-branch")
    with urc.open_file(f"{link_path}/shared.txt", "w+") as f:
        f.writelines(["feature content\n"])
    urc.stage(scan=True)
    urc.commit("Feature modifies shared.txt")
    urc.push()

    # Main branch: modify the same file differently via mount
    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/shared.txt", "w+") as f:
        f.writelines(["main content\n"])
    urc.stage(scan=True)
    urc.commit("Main modifies shared.txt")
    urc.push()

    # Default merge — link conflicts, parent state set as merge in conflict
    urc.branch_merge_start("feature-branch", message="Conflicting merge", no_commit=True)

    file_path = f"{link_path}/shared.txt"
    mine_sidecar = f"{file_path}.mine"
    theirs_sidecar = f"{file_path}.theirs"
    base_sidecar = f"{file_path}.base"

    # Sanity: after the merge, the file should have either inline markers (for
    # text conflicts the text merger handled) OR sidecars (for unmergeable
    # binary conflicts). Either form is an artifact that abort must clean up.
    with urc.open_file(file_path, "r") as f:
        mid_merge_content = f.read()
    has_inline_markers = ("<<<<<<<" in mid_merge_content) or (">>>>>>>" in mid_merge_content)
    has_sidecars = (
        urc.file_exists(mine_sidecar)
        or urc.file_exists(theirs_sidecar)
        or urc.file_exists(base_sidecar)
    )
    assert has_inline_markers or has_sidecars, (
        f"Expected mid-merge artifacts (inline markers or sidecars). Content was:\n{mid_merge_content}"
    )

    # Abort with --ignore-links: parent merge metadata stripped, link pins kept.
    # The on-disk markers/sidecars must not be left orphaned.
    urc.branch_merge_abort(ignore_links=True)

    assert not urc.file_exists(mine_sidecar), (
        f"Mine sidecar {mine_sidecar} should be cleaned after abort --ignore-links"
    )
    assert not urc.file_exists(theirs_sidecar), (
        f"Theirs sidecar {theirs_sidecar} should be cleaned after abort --ignore-links"
    )
    assert not urc.file_exists(base_sidecar), (
        f"Base sidecar {base_sidecar} should be cleaned after abort --ignore-links"
    )

    # The file's inline markers should also be gone — abort --ignore-links keeps
    # the link pin, but it should not leave merge-conflict markers in committed
    # link content. The link's pin should resolve to a clean state, not a
    # merged-but-conflicted one with markers.
    if urc.file_exists(file_path):
        with urc.open_file(file_path, "r") as f:
            post_abort_content = f.read()
        assert "<<<<<<<" not in post_abort_content and ">>>>>>>" not in post_abort_content, (
            f"Inline conflict markers should be cleaned. Content was:\n{post_abort_content}"
        )


def test_link_merge_start_ignore_links(new_lore_repo):
    """`merge start --ignore-links` merges only the parent repo. The link pin
    is unchanged and the source-side link files do not appear at the mount.
    """
    import re

    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link base\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    urc.branch_create("feature-branch")
    with urc.open_file("feature-only.txt", "w+") as f:
        f.writelines(["feature only main\n"])
    with urc.open_file(f"{link_path}/feature-link.txt", "w+") as f:
        f.writelines(["feature only in link\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch additions")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file("main-only.txt", "w+") as f:
        f.writelines(["main only\n"])
    urc.stage(scan=True)
    urc.commit("Main branch additions")
    urc.push()

    pin_before = re.search(
        rf"{link_repo.get_id()}.*?Revision:\s*(\w+)", urc.link_list(), re.DOTALL
    )
    assert pin_before

    # Merge with --ignore-links — should merge only the parent
    urc.branch_merge_start(
        "feature-branch", message="Merge main only", ignore_links=True
    )

    # Parent's feature file is now on main, and main's own file remains
    assert urc.file_exists("feature-only.txt")
    assert urc.file_exists("main-only.txt")

    # Link pin should be unchanged — link wasn't touched
    pin_after = re.search(
        rf"{link_repo.get_id()}.*?Revision:\s*(\w+)", urc.link_list(), re.DOTALL
    )
    assert pin_after
    assert pin_before.group(1) == pin_after.group(1), (
        f"Link pin should be unchanged with --ignore-links.\n"
        f"Before: {pin_before.group(1)}\nAfter: {pin_after.group(1)}"
    )

    # The link's feature-side content should NOT have been realized at the
    # mount path (link merge skipped entirely).
    assert not urc.file_exists(f"{link_path}/feature-link.txt"), (
        "Feature-side link file should not appear at mount with --ignore-links"
    )

    urc.push()
    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean after --ignore-links merge - Got:\n{status}"
    )


def test_link_merge_start_ignore_links_link_conflict(new_lore_repo):
    """`merge start --ignore-links` succeeds even when the link would have a
    file conflict, because the link is not consulted at all.
    """
    import re

    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("shared.txt", "w+") as f:
        f.writelines(["base\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    urc.branch_create("feature-branch")
    with urc.open_file(f"{link_path}/shared.txt", "w+") as f:
        f.writelines(["feature\n"])
    urc.stage(scan=True)
    urc.commit("Feature branch link change")
    urc.push()

    urc.branch_switch("main")
    with urc.open_file(f"{link_path}/shared.txt", "w+") as f:
        f.writelines(["main\n"])
    urc.stage(scan=True)
    urc.commit("Main branch link change")
    urc.push()

    pin_before = re.search(
        rf"{link_repo.get_id()}.*?Revision:\s*(\w+)", urc.link_list(), re.DOTALL
    )
    assert pin_before

    # Merge with --ignore-links — the link's would-be conflict is ignored
    # since the link is skipped entirely. The merge succeeds with no
    # conflicts to resolve.
    urc.branch_merge_start(
        "feature-branch", message="Merge main only despite link conflict",
        ignore_links=True,
    )

    # No conflict markers anywhere — link wasn't touched
    with urc.open_file(f"{link_path}/shared.txt", "r") as f:
        content = f.read()
    assert "<<<<<<<" not in content, (
        f"No conflict markers expected with --ignore-links - Got: {content}"
    )

    pin_after = re.search(
        rf"{link_repo.get_id()}.*?Revision:\s*(\w+)", urc.link_list(), re.DOTALL
    )
    assert pin_after
    assert pin_before.group(1) == pin_after.group(1)

    urc.push()
    status = urc.status()
    assert "local branch in sync with remote" in status.lower(), (
        f"Working tree should be clean - Got:\n{status}"
    )


def test_link_merge_into_ignore_links(new_lore_repo):
    """`merge into <branch> <message> --ignore-links` skips link content and
    merges only the parent repo, with the auto-commit semantics of `merge into`.
    """
    import re

    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link base\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    # Capture main's link pin BEFORE branching — this is what `--ignore-links`
    # `merge into main` should leave unchanged on the main branch.
    main_pin_before = re.search(
        rf"{link_repo.get_id()}.*?Revision:\s*(\w+)", urc.link_list(), re.DOTALL
    )
    assert main_pin_before

    urc.branch_create("feature-branch")
    with urc.open_file("feature-only.txt", "w+") as f:
        f.writelines(["feature only\n"])
    with urc.open_file(f"{link_path}/feature-link.txt", "w+") as f:
        f.writelines(["feature link\n"])
    urc.stage(scan=True)
    urc.commit("Feature changes")
    urc.push()

    # `merge into main --ignore-links` from feature-branch — folds
    # feature-branch's parent changes into main without touching the link.
    urc.branch_merge_into(
        "main", "Merge feature into main, ignore links", ignore_links=True
    )

    # We're still on feature-branch (merge into pushes into the OTHER branch
    # without switching). Switch to main to verify what landed.
    urc.branch_switch("main")
    urc.sync()

    # Feature's parent file is on main
    assert urc.file_exists("feature-only.txt")

    # Link's source-side file is NOT realized at the mount path on main
    assert not urc.file_exists(f"{link_path}/feature-link.txt"), (
        "Feature-side link file should not appear with --ignore-links"
    )

    # Link pin on main is unchanged from its pre-branch state — `--ignore-links`
    # didn't merge any link revisions.
    main_pin_after = re.search(
        rf"{link_repo.get_id()}.*?Revision:\s*(\w+)", urc.link_list(), re.DOTALL
    )
    assert main_pin_after
    assert main_pin_before.group(1) == main_pin_after.group(1), (
        f"Main's link pin should be unchanged after --ignore-links merge_into.\n"
        f"Before: {main_pin_before.group(1)}\nAfter: {main_pin_after.group(1)}"
    )


def test_link_merge_start_ignore_links_link_mutex(new_lore_repo):
    """Clap rejects the `--ignore-links --link <path>` combination at the CLI
    layer."""
    from error_types import LoreException

    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_repo = new_lore_repo()
    with link_repo.open_file("link-file.txt", "w+") as f:
        f.writelines(["link\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Initial link commit")
    link_repo.push()

    link_path = "linked/repo"
    urc.link_add(link_path, link_repo.get_id(), "/")
    urc.commit("Add link")
    urc.push()

    urc.branch_create("feature-branch")
    with urc.open_file("f.txt", "w+") as f:
        f.writelines(["f\n"])
    urc.stage(scan=True)
    urc.commit("Feature change")
    urc.push()

    urc.branch_switch("main")

    # Clap should reject both flags together — verify the command fails
    # with an error mentioning the conflicting flags.
    try:
        urc.branch_merge_start(
            "feature-branch",
            message="Should fail",
            link=link_path,
            ignore_links=True,
        )
        assert False, "Expected merge start to reject --ignore-links --link"
    except LoreException as e:
        msg = str(e).lower()
        assert "ignore-links" in msg or "ignore_links" in msg or "conflict" in msg, (
            f"Error should mention the conflicting flags - Got: {e}"
        )


def test_link_merge_abort_all_then_resume_multiple_links(new_lore_repo):
    """Default abort with multiple links cleanly rolls everything back, and a
    subsequent re-attempted merge succeeds — exercises the abort-then-resume
    composition with N>1 links."""
    urc: Lore = new_lore_repo()
    with urc.open_file("main-file.txt", "w+") as f:
        f.writelines(["main base\n"])
    urc.stage(scan=True)
    urc.commit("Initial main commit")
    urc.push()

    link_a = new_lore_repo()
    with link_a.open_file("a-file.txt", "w+") as f:
        f.writelines(["a base\n"])
    link_a.stage(scan=True)
    link_a.commit("Initial link A commit")
    link_a.push()

    link_b = new_lore_repo()
    with link_b.open_file("b-file.txt", "w+") as f:
        f.writelines(["b base\n"])
    link_b.stage(scan=True)
    link_b.commit("Initial link B commit")
    link_b.push()

    path_a = "libs/a"
    path_b = "libs/b"
    urc.link_add(path_a, link_a.get_id(), "/")
    urc.link_add(path_b, link_b.get_id(), "/")
    urc.commit("Add both links")
    urc.push()

    # Feature branch: add file in main, link A, and link B
    urc.branch_create("feature-branch")
    with urc.open_file("feature-main.txt", "w+") as f:
        f.writelines(["feature main\n"])
    with urc.open_file(f"{path_a}/feature-a.txt", "w+") as f:
        f.writelines(["feature a\n"])
    with urc.open_file(f"{path_b}/feature-b.txt", "w+") as f:
        f.writelines(["feature b\n"])
    urc.stage(scan=True)
    urc.commit("Feature additions across all repos")
    urc.push()

    # Main branch: add different files in all three repos
    urc.branch_switch("main")
    with urc.open_file("main-only.txt", "w+") as f:
        f.writelines(["main only\n"])
    with urc.open_file(f"{path_a}/main-a.txt", "w+") as f:
        f.writelines(["main a\n"])
    with urc.open_file(f"{path_b}/main-b.txt", "w+") as f:
        f.writelines(["main b\n"])
    urc.stage(scan=True)
    urc.commit("Main additions across all repos")
    urc.push()

    # Start merge with --no-commit so we can abort
    urc.branch_merge_start(
        "feature-branch",
        message="First attempt",
        no_commit=True,
    )

    # Both links and main staged the feature files
    assert urc.file_exists("feature-main.txt")
    assert urc.file_exists(f"{path_a}/feature-a.txt")
    assert urc.file_exists(f"{path_b}/feature-b.txt")

    # Default abort — rolls back main and BOTH link pins
    urc.branch_merge_abort()

    # Feature files gone from main and both links
    assert not urc.file_exists("feature-main.txt"), (
        "Feature main file should be gone after abort"
    )
    assert not urc.file_exists(f"{path_a}/feature-a.txt"), (
        "Feature link A file should be gone after abort"
    )
    assert not urc.file_exists(f"{path_b}/feature-b.txt"), (
        "Feature link B file should be gone after abort"
    )

    # Main files still present in all three repos
    assert urc.file_exists("main-only.txt")
    assert urc.file_exists(f"{path_a}/main-a.txt")
    assert urc.file_exists(f"{path_b}/main-b.txt")

    # Re-attempt the merge — must succeed and produce the same end state
    urc.branch_merge_start("feature-branch", message="Second attempt")
    urc.push()

    for f in [
        "feature-main.txt",
        "main-only.txt",
        f"{path_a}/feature-a.txt",
        f"{path_a}/main-a.txt",
        f"{path_b}/feature-b.txt",
        f"{path_b}/main-b.txt",
    ]:
        assert urc.file_exists(f), f"{f} should be present after re-attempted merge"

    urc.sync()
    assert urc.file_exists(f"{path_a}/feature-a.txt"), (
        "Link A feature file should persist after sync (pin is correct)"
    )
    assert urc.file_exists(f"{path_b}/feature-b.txt"), (
        "Link B feature file should persist after sync (pin is correct)"
    )


@pytest.mark.smoke
def test_link_scoped_commit_subdirectory_source_path_translation(new_lore_repo):
    """When a link's source_path is a subdirectory of the source repo (e.g.
    FolderProvidingLink) and the link is mounted under a different name in
    the parent (e.g. FolderReceivingLink), a link-scoped commit (--link
    FolderReceivingLink) for a newly-added file used to fail with:

      Failed writing file FolderProvidingLink/<file> to immutable store:
      ... <link_path>/FolderProvidingLink/<file>: cannot find the path

    The path translation from remote tree path -> local filesystem path
    erroneously concatenated the source folder name onto the local link
    folder, instead of substituting it.
    """
    # Source repo with a subdirectory we will link out of.
    source_repo: Lore = new_lore_repo()
    source_repo.make_dirs("FolderProvidingLink")
    with source_repo.open_file("FolderProvidingLink/SharedFile.txt", "w+") as f:
        f.writelines(["AAAA\n"])
    source_repo.stage(scan=True)
    source_repo.commit("Initial shared file in FolderProvidingLink")
    source_repo.push()

    pinned_revision = source_repo.branch_info().local_latest

    # Receiving repo with a folder that will receive the link, mounted under
    # a different name than the source folder.
    receiving_repo: Lore = new_lore_repo()
    receiving_repo.make_dirs("FolderReceivingLink")
    receiving_repo.stage(scan=True)
    receiving_repo.commit("Create FolderReceivingLink")
    receiving_repo.push()

    receiving_repo.link_add(
        "FolderReceivingLink",
        source_repo.get_id(),
        "FolderProvidingLink",
        pin=pinned_revision,
    )
    receiving_repo.commit("Add link FolderReceivingLink -> FolderProvidingLink")
    receiving_repo.push()

    # Sanity: linked file is mounted directly under the link path, not
    # nested inside FolderProvidingLink.
    assert receiving_repo.file_exists("FolderReceivingLink/SharedFile.txt"), (
        "SharedFile.txt should be mounted directly under FolderReceivingLink"
    )
    assert not receiving_repo.file_exists(
        "FolderReceivingLink/FolderProvidingLink/SharedFile.txt"
    ), "Source folder name must not appear nested inside the link path"

    # Add a new file under the link path and stage it.
    new_file = "FolderReceivingLink/SharedFile2.txt"
    with receiving_repo.open_file(new_file, "w+") as f:
        f.writelines(["BBBB\n"])
    receiving_repo.stage(new_file)

    # Link-scoped commit must succeed. The bug caused this to fail because
    # the commit code looked for the file at
    # FolderReceivingLink/FolderProvidingLink/SharedFile2.txt on disk.
    output = receiving_repo.commit(
        "File added to linked folder", link="FolderReceivingLink"
    )
    assert "Commit succeeded" in output, (
        f"Link-scoped commit should succeed, got output: {output}"
    )

    # Finalize the parent so the new link pin is recorded.
    output = receiving_repo.commit("Update link pin")
    assert "Commit succeeded" in output
    receiving_repo.push()

    # The new file must be reachable through the link in a fresh clone.
    sync_repo = receiving_repo.clone()
    assert sync_repo.file_exists("FolderReceivingLink/SharedFile2.txt"), (
        "Newly committed file must be present under the link in a fresh clone"
    )
    assert sync_repo.file_exists("FolderReceivingLink/SharedFile.txt"), (
        "Original linked file must still be present under the link"
    )


def _make_link_target_repo(new_lore_repo, file_name: str, content: str):
    """Build a small repo to be used as a link target, with one file at root.

    Returns (repo, pinned_revision_of_main).
    """
    target_repo: Lore = new_lore_repo()
    with target_repo.open_file(file_name, "w+") as f:
        f.writelines([content])
    target_repo.stage(scan=True)
    target_repo.commit("Initial linked content")
    target_repo.push()
    return target_repo, target_repo.branch_info().local_latest


def _make_parent_repo_with_baseline(new_lore_repo):
    """Build a parent repo with one baseline commit. Returns (repo, baseline_revision)."""
    parent_repo: Lore = new_lore_repo()
    with parent_repo.open_file("README.txt", "w+") as f:
        f.writelines(["baseline\n"])
    parent_repo.stage(scan=True)
    parent_repo.commit("Baseline commit")
    parent_repo.push()
    return parent_repo, parent_repo.branch_info().local_latest


@pytest.mark.smoke
def test_link_add_diff_reports_link_only(new_lore_repo):
    """Adding a link is a parent-tree change, not a per-file addition of
    the link's mounted contents.

    This test documents the user-visible contract for `lore link add`. It
    pairs two independent assertions that reach two different code paths:

      - Positive proof (parent-tree path): `lore status` reports the link
        path as a staged addition. This signal comes from
        `state::diff_collect` (state-vs-staged), the change stream used by
        status.

      - Negative proof (filesystem-walker path): `lore file diff` against
        the pre-link revision emits no unified-diff hunk for any file
        inside the link. This is the surface where the regression appeared.

    The negative-proof half is a contract assertion; on its own it would
    also pass if `file diff` were broken in some unrelated way. The
    walker's liveness is pinned by the symmetric content tests
    (`test_link_diff_file_added_in_linked_repo` and siblings), which prove
    `file diff` does emit hunks when the linked repository's content
    actually changes.
    """
    link_repo, pinned_revision = _make_link_target_repo(
        new_lore_repo, "shared.txt", "main content\n"
    )
    parent_repo, baseline_revision = _make_parent_repo_with_baseline(new_lore_repo)

    link_path = "libs/shared"
    linked_file = f"{link_path}/shared.txt"

    parent_repo.link_add(
        link_path, link_repo.get_id(), "/", pin=pinned_revision
    )

    # Positive proof: before committing, `lore status` must report the link
    # path itself as a staged addition, and must not list files inside the
    # link as separate staged adds.
    staged_paths = [
        entry.get("path", "")
        for entry in parse_status_json(parent_repo.status(json=True))
    ]
    assert link_path in staged_paths, (
        f"`lore status` must report the link path {link_path!r} as staged "
        f"after `lore link add`. Staged paths: {staged_paths}"
    )
    assert linked_file not in staged_paths, (
        f"`lore status` must not list files inside the link as separate "
        f"staged adds. Staged paths: {staged_paths}"
    )

    parent_repo.commit("Add link libs/shared")
    parent_repo.push()

    output = parent_repo.file_diff(source=baseline_revision, no_pager=True)

    # Negative proof: the file under the link must not appear in any unified-
    # diff header. We assert on the diff headers specifically so the test
    # fails for the exact format produced by the bug
    # (`--- /dev/null\n+++ libs/shared/...`).
    assert f"+++ {linked_file}" not in output, (
        "lore file diff must not emit a '+++ <link>/<file>' header for a "
        f"file inside a newly-added link.\nOutput:\n{output}"
    )
    assert f"--- {linked_file}" not in output, (
        "lore file diff must not emit a '--- <link>/<file>' header for a "
        f"file inside a newly-added link.\nOutput:\n{output}"
    )
    # The parent's baseline file was not touched -> no diff for it either.
    assert "+++ README.txt" not in output and "--- README.txt" not in output, (
        f"Parent's baseline file must not appear in the diff.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_update_diff_reports_link_only(new_lore_repo):
    """Updating a link's pin is a parent-tree change. When the new pin
    leaves the mounted subtree byte-identical, no file under the link path
    is touched in the parent.

    Setup: the link target has a subdirectory `mounted/` containing the
    file the parent's link exposes, plus an unrelated `outside/` file.
    Between pin P1 and pin P2 only `outside/` is modified; the subtree
    `mounted/` is byte-identical. The parent pins the link to `mounted/`,
    so the new pin is a metadata change (different signature) but the
    mounted tree is unchanged.

    This test pairs two independent assertions reaching different code paths:

      - Positive proof (parent-tree path): `lore status` reports the link
        path as a staged change after `lore link update`. This signal
        comes from `state::diff_collect` (state-vs-staged).

      - Negative proof (filesystem-walker path): `lore file diff` across
        the pin bump emits no unified-diff hunk for any file under the
        link path.

    The negative-proof half is a contract assertion. The walker's
    liveness is pinned by the symmetric content tests, which prove
    `file diff` does emit hunks when the linked repository's content
    actually changes between pins.
    """
    # Link target with a stable mounted/ subtree and an evolving outside/
    # file. The parent will mount only mounted/.
    link_repo: Lore = new_lore_repo()
    link_repo.make_dirs("mounted")
    with link_repo.open_file("mounted/stable.txt", "w+") as f:
        f.writelines(["stable mounted content\n"])
    link_repo.make_dirs("outside")
    with link_repo.open_file("outside/v1.txt", "w+") as f:
        f.writelines(["outside v1\n"])
    link_repo.stage(scan=True)
    link_repo.commit("v1 with mounted/ + outside/")
    link_repo.push()
    pin_v1 = link_repo.branch_info().local_latest

    # Modify only outside/, leaving the mounted/ subtree untouched, so
    # v1 and v2 resolve to the same mounted/ tree.
    with link_repo.open_file("outside/v2.txt", "w+") as f:
        f.writelines(["outside v2\n"])
    link_repo.stage(scan=True)
    link_repo.commit("v2 changes only outside/")
    link_repo.push()
    pin_v2 = link_repo.branch_info().local_latest

    # Parent: baseline -> add link pinned to v1 mounting only mounted/ ->
    # re-pin to v2 (mounted/ subtree unchanged).
    parent_repo, _ = _make_parent_repo_with_baseline(new_lore_repo)
    link_path = "libs/shared"
    linked_file = f"{link_path}/stable.txt"

    parent_repo.link_add(link_path, link_repo.get_id(), "/mounted", pin=pin_v1)
    parent_repo.commit("Add link at v1, mounting mounted/")
    parent_repo.push()
    pre_update_revision = parent_repo.branch_info().local_latest

    parent_repo.link_update(link_path, pin=pin_v2)

    # Positive proof: before committing, `lore status` must report the link
    # path as a staged change (the link node's pin moved), and must not list
    # files under the link path as separate staged entries.
    staged_paths = [
        entry.get("path", "")
        for entry in parse_status_json(parent_repo.status(json=True))
    ]
    assert link_path in staged_paths, (
        f"`lore status` must report the link path {link_path!r} as staged "
        f"after a `lore link update` pin move. Staged paths: {staged_paths}"
    )
    assert linked_file not in staged_paths, (
        f"`lore status` must not list files inside the link as separate "
        f"staged entries after a pin move. Staged paths: {staged_paths}"
    )

    parent_repo.commit("Bump link pin to v2 (no mounted/ change)")
    parent_repo.push()

    output = parent_repo.file_diff(source=pre_update_revision, no_pager=True)

    # No file content changed under the link path, so the diff must not
    # emit any unified-diff hunks for files under the link.
    assert f"+++ {linked_file}" not in output, (
        "lore file diff must not emit file edits when the link pin moved "
        f"but no mounted file changed.\nOutput:\n{output}"
    )
    assert f"--- {linked_file}" not in output, (
        "lore file diff must not emit file edits when the link pin moved "
        f"but no mounted file changed.\nOutput:\n{output}"
    )
    assert "stable mounted content" not in output, (
        "lore file diff must not surface stable mounted content as a "
        f"change when only the link pin signature moved.\nOutput:\n{output}"
    )
    # The unrelated outside/ file lives outside the link's source_path and
    # must never appear in the parent's diff.
    assert "outside" not in output, (
        "Files outside the link's mounted source_path must not appear in "
        f"the parent's file diff.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_remove_diff_reports_link_only(new_lore_repo):
    """Removing a link is a parent-tree change, not per-file deletions of
    the files that were visible through it.

    This test pairs two independent assertions reaching different code paths:

      - Positive proof (parent-tree path): `lore status` reports the link
        path as a staged removal after `lore link remove`. This signal
        comes from `state::diff_collect` (state-vs-staged).

      - Negative proof (filesystem-walker path): `lore file diff` against
        the pre-remove revision emits no unified-diff hunk for any file
        that was previously visible through the link.

    The negative-proof half is a contract assertion. The walker's
    liveness is pinned by the symmetric content tests, which prove
    `file diff` does emit hunks when the linked repository's content
    actually changes.
    """
    link_repo, pinned_revision = _make_link_target_repo(
        new_lore_repo, "shared.txt", "main content\n"
    )
    parent_repo, _ = _make_parent_repo_with_baseline(new_lore_repo)

    link_path = "libs/shared"
    linked_file = f"{link_path}/shared.txt"

    parent_repo.link_add(
        link_path, link_repo.get_id(), "/", pin=pinned_revision
    )
    parent_repo.commit("Add link")
    parent_repo.push()
    pre_remove_revision = parent_repo.branch_info().local_latest

    parent_repo.link_remove(link_path)

    # Positive proof: before committing, `lore status` must report the link
    # path as a staged removal, and must not list files that were visible
    # through the link as separate staged deletions.
    staged_paths = [
        entry.get("path", "")
        for entry in parse_status_json(parent_repo.status(json=True))
    ]
    assert link_path in staged_paths, (
        f"`lore status` must report the link path {link_path!r} as staged "
        f"after `lore link remove`. Staged paths: {staged_paths}"
    )
    assert linked_file not in staged_paths, (
        f"`lore status` must not list files inside the link as separate "
        f"staged deletions. Staged paths: {staged_paths}"
    )

    parent_repo.commit("Remove link")
    parent_repo.push()

    output = parent_repo.file_diff(source=pre_remove_revision, no_pager=True)

    # The diff must not show files inside the (now-removed) link as
    # individual file deletions in the parent.
    assert f"--- {linked_file}" not in output, (
        "lore file diff must not emit a '--- <link>/<file>' header for a "
        f"file inside a removed link.\nOutput:\n{output}"
    )
    assert "-main content" not in output, (
        "lore file diff must not surface deleted-link content as a parent "
        f"file deletion.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_diff_file_added_in_linked_repo(new_lore_repo):
    """When a link is updated and the new pin adds a new file inside the
    linked repository, `lore file diff` against the pin-bump must show the
    added file as an addition under the link path.
    """
    # Link target with two revisions: P1 has only "existing.txt"; P2 also
    # contains "new.txt".
    link_repo: Lore = new_lore_repo()
    with link_repo.open_file("existing.txt", "w+") as f:
        f.writelines(["already here\n"])
    link_repo.stage(scan=True)
    link_repo.commit("existing only")
    link_repo.push()
    pin_v1 = link_repo.branch_info().local_latest

    with link_repo.open_file("new.txt", "w+") as f:
        f.writelines(["brand new content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("add new file")
    link_repo.push()
    pin_v2 = link_repo.branch_info().local_latest

    parent_repo, _ = _make_parent_repo_with_baseline(new_lore_repo)
    link_path = "libs/shared"
    added_file = f"{link_path}/new.txt"

    parent_repo.link_add(link_path, link_repo.get_id(), "/", pin=pin_v1)
    parent_repo.commit("Add link@v1")
    parent_repo.push()
    pre_update_revision = parent_repo.branch_info().local_latest

    parent_repo.link_update(link_path, pin=pin_v2)
    parent_repo.commit("Bump link to v2")
    parent_repo.push()

    output = parent_repo.file_diff(source=pre_update_revision, no_pager=True)

    expected_header = f"--- /dev/null\n+++ {added_file}"
    assert expected_header in output, (
        "lore file diff must report the newly-added file inside the link "
        f"as added.\nExpected to contain:\n{expected_header}\n"
        f"Output:\n{output}"
    )
    assert "+brand new content" in output, (
        "lore file diff must include the added file's content as additions."
        f"\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_diff_file_modified_in_linked_repo(new_lore_repo):
    """When a link is updated and the new pin modifies an existing file
    inside the linked repository, `lore file diff` must show that file as
    modified under the link path with the correct hunk content.
    """
    link_repo: Lore = new_lore_repo()
    with link_repo.open_file("shared.txt", "w+") as f:
        f.writelines(["before\n"])
    link_repo.stage(scan=True)
    link_repo.commit("v1")
    link_repo.push()
    pin_v1 = link_repo.branch_info().local_latest

    with link_repo.open_file("shared.txt", "w+") as f:
        f.writelines(["after\n"])
    link_repo.stage(scan=True)
    link_repo.commit("v2")
    link_repo.push()
    pin_v2 = link_repo.branch_info().local_latest

    parent_repo, _ = _make_parent_repo_with_baseline(new_lore_repo)
    link_path = "libs/shared"
    modified_file = f"{link_path}/shared.txt"

    parent_repo.link_add(link_path, link_repo.get_id(), "/", pin=pin_v1)
    parent_repo.commit("Add link@v1")
    parent_repo.push()
    pre_update_revision = parent_repo.branch_info().local_latest

    parent_repo.link_update(link_path, pin=pin_v2)
    parent_repo.commit("Bump link to v2")
    parent_repo.push()

    output = parent_repo.file_diff(source=pre_update_revision, no_pager=True)

    assert f"+++ {modified_file}" in output, (
        "lore file diff must report the modified linked file under its "
        f"link path.\nOutput:\n{output}"
    )
    assert f"--- {modified_file}" in output, (
        "lore file diff must report the modified linked file's source "
        f"side under its link path.\nOutput:\n{output}"
    )
    assert "-before" in output and "+after" in output, (
        f"lore file diff must include the modified content hunk.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_diff_file_removed_in_linked_repo(new_lore_repo):
    """When a link is updated and the new pin removes a file inside the
    linked repository, `lore file diff` must show that file as removed
    under the link path.
    """
    link_repo: Lore = new_lore_repo()
    with link_repo.open_file("keep.txt", "w+") as f:
        f.writelines(["keep me\n"])
    with link_repo.open_file("doomed.txt", "w+") as f:
        f.writelines(["delete me\n"])
    link_repo.stage(scan=True)
    link_repo.commit("v1 with both files")
    link_repo.push()
    pin_v1 = link_repo.branch_info().local_latest

    link_repo.remove_file("doomed.txt")
    link_repo.stage(scan=True)
    link_repo.commit("v2 removes doomed.txt")
    link_repo.push()
    pin_v2 = link_repo.branch_info().local_latest

    parent_repo, _ = _make_parent_repo_with_baseline(new_lore_repo)
    link_path = "libs/shared"
    removed_file = f"{link_path}/doomed.txt"

    parent_repo.link_add(link_path, link_repo.get_id(), "/", pin=pin_v1)
    parent_repo.commit("Add link@v1")
    parent_repo.push()
    pre_update_revision = parent_repo.branch_info().local_latest

    parent_repo.link_update(link_path, pin=pin_v2)
    parent_repo.commit("Bump link to v2")
    parent_repo.push()

    output = parent_repo.file_diff(source=pre_update_revision, no_pager=True)

    assert f"--- {removed_file}" in output, (
        "lore file diff must report the removed linked file under its "
        f"link path.\nOutput:\n{output}"
    )
    assert "+++ /dev/null" in output, (
        "lore file diff must report a deletion target of /dev/null for the "
        f"removed linked file.\nOutput:\n{output}"
    )
    assert "-delete me" in output, (
        f"lore file diff must include the removed file's content as a deletion."
        f"\nOutput:\n{output}"
    )


_ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*m")
_DIFF_ACTIONS = frozenset({"A", "D", "M", "V", "C"})


def _parse_revision_diff(output: str) -> list[tuple[str, str]]:
    """Parse `lore revision diff` output into (action, path) pairs.

    The CLI colourizes the action letter, so ANSI escapes are stripped first.
    """
    entries: list[tuple[str, str]] = []
    for raw_line in _ANSI_ESCAPE.sub("", output).splitlines():
        parts = raw_line.strip().split(" ", 1)
        if len(parts) == 2 and parts[0] in _DIFF_ACTIONS:
            entries.append((parts[0], parts[1].strip()))
    return entries


@pytest.mark.smoke
def test_link_pin_update_revision_diff_reports_link_and_content(new_lore_repo):
    """A pin update must surface both the link itself and the linked
    repository's file changes.

    The contract for the "pin updated" case:

      - the link path is reported as a modification (the pin moved),
      - the linked repository's changed files are reported under the mount
        path,
      - an unrelated parent-side change in the same revision is still
        reported.
    """
    link_repo: Lore = new_lore_repo()
    with link_repo.open_file("a.txt", "w+") as f:
        f.writelines(["v1\n"])
    link_repo.make_dirs("sub")
    with link_repo.open_file("sub/b.txt", "w+") as f:
        f.writelines(["b1\n"])
    link_repo.stage(scan=True)
    link_repo.commit("v1")
    link_repo.push()
    pin_v1 = link_repo.branch_info().local_latest

    # v2 modifies one mounted file and adds another.
    with link_repo.open_file("a.txt", "w+") as f:
        f.writelines(["v2\n"])
    with link_repo.open_file("sub/c.txt", "w+") as f:
        f.writelines(["c1\n"])
    link_repo.stage(scan=True)
    link_repo.commit("v2 modifies a.txt and adds sub/c.txt")
    link_repo.push()
    pin_v2 = link_repo.branch_info().local_latest

    parent_repo, _ = _make_parent_repo_with_baseline(new_lore_repo)
    link_path = "libs/shared"

    parent_repo.link_add(link_path, link_repo.get_id(), "/", pin=pin_v1)
    parent_repo.commit("Add link@v1")
    parent_repo.push()
    pre_update_revision = parent_repo.branch_info().local_latest

    # The pin update has to come first: `lore link update` refuses to run once
    # a scan has marked the mount point dirty.
    parent_repo.link_update(link_path, pin=pin_v2)
    with parent_repo.open_file("README.txt", "w+") as f:
        f.writelines(["baseline touched on this revision\n"])
    parent_repo.stage("README.txt")
    parent_repo.commit("Bump link to v2 and touch README")
    parent_repo.push()

    output = parent_repo.revision_diff(pre_update_revision, no_pager=True)
    entries = _parse_revision_diff(output)
    paths = {path: action for action, path in entries}

    assert paths.get(link_path) == "M", (
        f"`lore revision diff` must report the link path {link_path!r} as a "
        "modification when its pin moved, so a consumer can tell the pin "
        f"changed and which paths belong to the link.\nOutput:\n{output}"
    )
    assert paths.get(f"{link_path}/a.txt") == "M", (
        "`lore revision diff` must report the modified file inside the "
        f"linked repository under the mount path.\nOutput:\n{output}"
    )
    assert paths.get(f"{link_path}/sub/c.txt") == "A", (
        "`lore revision diff` must report the file added inside the linked "
        f"repository under the mount path.\nOutput:\n{output}"
    )
    assert paths.get("README.txt") == "M", (
        "`lore revision diff` must still report the parent's own change in a "
        f"revision that also moved a link pin.\nOutput:\n{output}"
    )
    assert f"{link_path}/sub/b.txt" not in paths, (
        "`lore revision diff` must not report unchanged files inside the "
        f"linked repository.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_pin_update_revision_diff_reports_link_when_content_identical(
    new_lore_repo,
):
    """A pin bump that leaves the mounted subtree byte-identical must still
    be visible in `lore revision diff`.

    Setup mirrors `test_link_update_diff_reports_link_only`: the parent mounts
    only `mounted/`, and between the two pins nothing inside it changes, so the
    content walk finds nothing. Without an entry for the link node the diff
    comes back empty, reading as "this revision changed nothing".
    """
    link_repo: Lore = new_lore_repo()
    link_repo.make_dirs("mounted")
    with link_repo.open_file("mounted/stable.txt", "w+") as f:
        f.writelines(["stable mounted content\n"])
    link_repo.make_dirs("outside")
    with link_repo.open_file("outside/v1.txt", "w+") as f:
        f.writelines(["outside v1\n"])
    link_repo.stage(scan=True)
    link_repo.commit("v1 with mounted/ + outside/")
    link_repo.push()
    pin_v1 = link_repo.branch_info().local_latest

    with link_repo.open_file("outside/v2.txt", "w+") as f:
        f.writelines(["outside v2\n"])
    link_repo.stage(scan=True)
    link_repo.commit("v2 changes only outside/")
    link_repo.push()
    pin_v2 = link_repo.branch_info().local_latest

    parent_repo, _ = _make_parent_repo_with_baseline(new_lore_repo)
    link_path = "libs/shared"

    parent_repo.link_add(link_path, link_repo.get_id(), "/mounted", pin=pin_v1)
    parent_repo.commit("Add link at v1, mounting mounted/")
    parent_repo.push()
    pre_update_revision = parent_repo.branch_info().local_latest

    parent_repo.link_update(link_path, pin=pin_v2)
    parent_repo.commit("Bump link pin to v2 (no mounted/ change)")
    parent_repo.push()

    output = parent_repo.revision_diff(pre_update_revision, no_pager=True)
    entries = _parse_revision_diff(output)
    paths = {path: action for action, path in entries}

    assert paths.get(link_path) == "M", (
        "`lore revision diff` must report the link path as a modification "
        "when the pin moved, even though no mounted file changed. Otherwise "
        f"the revision looks empty.\nOutput:\n{output}"
    )
    under_link = [path for path in paths if path.startswith(f"{link_path}/")]
    assert not under_link, (
        "`lore revision diff` must not report any file under the link path "
        f"when the mounted subtree is unchanged. Got: {under_link}"
        f"\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_add_remove_revision_diff_reports_link_only(new_lore_repo):
    """Adding or removing a link is reported as a single entry for the link
    path; the mounted contents are never expanded into per-file changes.

    Guards the asymmetry with the pin-update tests above: only an *updated*
    link expands into the linked repository's changes.
    """
    link_repo, pinned_revision = _make_link_target_repo(
        new_lore_repo, "shared.txt", "main content\n"
    )
    parent_repo, baseline_revision = _make_parent_repo_with_baseline(new_lore_repo)

    link_path = "libs/shared"
    linked_file = f"{link_path}/shared.txt"

    parent_repo.link_add(link_path, link_repo.get_id(), "/", pin=pinned_revision)
    parent_repo.commit("Add link libs/shared")
    parent_repo.push()
    post_add_revision = parent_repo.branch_info().local_latest

    add_output = parent_repo.revision_diff(baseline_revision, no_pager=True)
    add_paths = {path: action for action, path in _parse_revision_diff(add_output)}

    assert add_paths.get(link_path) == "A", (
        f"Adding a link must report {link_path!r} as an addition."
        f"\nOutput:\n{add_output}"
    )
    assert linked_file not in add_paths, (
        "Adding a link must not expand the mounted contents into per-file "
        f"additions.\nOutput:\n{add_output}"
    )

    parent_repo.link_remove(link_path)
    parent_repo.commit("Remove link libs/shared")
    parent_repo.push()

    remove_output = parent_repo.revision_diff(post_add_revision, no_pager=True)
    remove_paths = {
        path: action for action, path in _parse_revision_diff(remove_output)
    }

    assert remove_paths.get(link_path) == "D", (
        f"Removing a link must report {link_path!r} as a deletion."
        f"\nOutput:\n{remove_output}"
    )
    assert linked_file not in remove_paths, (
        "Removing a link must not expand the previously mounted contents "
        f"into per-file deletions.\nOutput:\n{remove_output}"
    )


@pytest.mark.smoke
def test_nested_link_probe(new_lore_repo):
    """Add a nested link (a link whose path is inside another link).

    NOT layers. This is A -> B -> C: repo A links repo B at some path, and a
    link to repo C is added *inside* B's mounted subtree. Verifies C's content
    clones in, C appears in the link list, and the nesting survives a commit +
    fresh clone round-trip.
    """
    # --- Repo C: the innermost target -------------------------------------
    repo_c = new_lore_repo()
    c_file = "c-data/inner.txt"
    repo_c.make_dirs(os.path.dirname(c_file))
    with repo_c.open_file(c_file, "w+") as f:
        f.writelines(["content from repository C\n"])
    repo_c.stage(scan=True)
    repo_c.commit("C initial")
    repo_c.push()

    # --- Repo B: the middle repo that A will link ------------------------
    repo_b = new_lore_repo()
    b_file = "b-root.txt"
    b_subdir_file = "b-dir/nested.txt"
    with repo_b.open_file(b_file, "w+") as f:
        f.writelines(["content from repository B\n"])
    repo_b.make_dirs(os.path.dirname(b_subdir_file))
    with repo_b.open_file(b_subdir_file, "w+") as f:
        f.writelines(["nested content from repository B\n"])
    repo_b.stage(scan=True)
    repo_b.commit("B initial")
    repo_b.push()

    # --- Repo A: the outermost parent ------------------------------------
    repo_a = new_lore_repo()
    a_file = "a-root.txt"
    with repo_a.open_file(a_file, "w+") as f:
        f.writelines(["content from repository A\n"])
    repo_a.stage(scan=True)
    repo_a.commit("A initial")
    repo_a.push()

    # Step 1: link B into A. This is an ordinary (non-nested) link and must
    # succeed — establishes the mount we then try to nest inside.
    b_mount = "vendor/b"
    repo_a.link_add(b_mount, repo_b.get_id(), "/")

    b_root_via_a = f"{b_mount}/b-root.txt"
    b_nested_via_a = f"{b_mount}/b-dir/nested.txt"
    assert repo_a.compare_file(repo_a, b_root_via_a), (
        "B's root file should be accessible through A's link"
    )
    assert repo_a.compare_file(repo_a, b_nested_via_a), (
        "B's nested file should be accessible through A's link"
    )

    repo_a.commit("A links B")
    repo_a.push()

    # Step 2: resolve/read machinery through the link works — confirm A can
    # still walk B's subtree via link list and status without error.
    link_list = repo_a.link_list()
    assert repo_b.get_id() in link_list, "B should appear in A's link list"
    assert b_mount in link_list, "B's mount path should appear in A's link list"

    # Step 3: the nested link — add a link to C at a path inside B's mounted
    # subtree (vendor/b/vendor/c). `link add` resolves through B's link, stages
    # the sub-link in B's state and registry, and folds B's new revision up
    # into A's pin.
    nested_mount = f"{b_mount}/vendor/c"
    expect_c_via_nested = f"{nested_mount}/c-data/inner.txt"

    repo_a.link_add(nested_mount, repo_c.get_id(), "/")

    # C's content must materialize under the nested mount.
    assert repo_a.file_exists(expect_c_via_nested), (
        f"Nested linked file did not clone in at {expect_c_via_nested}"
    )
    assert repo_a.compare_file(repo_a, expect_c_via_nested), (
        "Nested linked file content mismatch"
    )

    # C should appear in the link registry alongside B.
    nested_link_list = repo_a.link_list()
    assert repo_c.get_id() in nested_link_list, (
        "C should appear in the link list once nested-linked"
    )
    assert nested_mount in nested_link_list, (
        "Nested mount path should appear in the link list"
    )

    # And it must survive a round-trip through commit + fresh clone, which
    # exercises the commit-recursion and server tree-walk nesting paths (both
    # already nesting-capable).
    repo_a.commit("A links B links C")
    repo_a.push()

    fresh = repo_a.clone()
    assert fresh.file_exists(expect_c_via_nested), (
        "Nested linked file did not survive commit + clone round-trip"
    )
    assert fresh.compare_file(repo_a, expect_c_via_nested), (
        "Nested linked file content mismatch after clone"
    )


def _build_nested_link_repos(new_lore_repo):
    """Build and commit an A -> B -> C nested-link structure.

    A links B at `vendor/b`; C is linked at `vendor/b/vendor/c` (inside B's
    mounted subtree). Returns (repo_a, repo_b, repo_c, b_mount, nested_mount).
    """
    # Repo C: innermost target.
    repo_c = new_lore_repo()
    repo_c.make_dirs("c-data")
    with repo_c.open_file("c-data/inner.txt", "w+") as f:
        f.writelines(["content from repository C\n"])
    with repo_c.open_file("c-root.txt", "w+") as f:
        f.writelines(["c root\n"])
    repo_c.stage(scan=True)
    repo_c.commit("C initial")
    repo_c.push()

    # Repo B: middle repo.
    repo_b = new_lore_repo()
    with repo_b.open_file("b-root.txt", "w+") as f:
        f.writelines(["content from repository B\n"])
    repo_b.stage(scan=True)
    repo_b.commit("B initial")
    repo_b.push()

    # Repo A: outermost parent.
    repo_a = new_lore_repo()
    with repo_a.open_file("a-root.txt", "w+") as f:
        f.writelines(["content from repository A\n"])
    repo_a.stage(scan=True)
    repo_a.commit("A initial")
    repo_a.push()

    # A links B.
    b_mount = "vendor/b"
    repo_a.link_add(b_mount, repo_b.get_id(), "/")
    repo_a.commit("A links B")
    repo_a.push()

    # Nested: link C inside B's mounted subtree.
    nested_mount = f"{b_mount}/vendor/c"
    repo_a.link_add(nested_mount, repo_c.get_id(), "/")
    repo_a.commit("A links B links C")
    repo_a.push()

    return repo_a, repo_b, repo_c, b_mount, nested_mount


@pytest.mark.smoke
def test_nested_link_stage_unstage_reset(new_lore_repo):
    """Stage / unstage / reset of content INSIDE a nested link (A -> B -> C).

    Exercises that the stage machinery propagates a change made two link
    levels deep (a file owned by C, mounted under B, mounted under A) back up
    through B's pin to A's staged anchor, and that unstage/reset undo it.
    """
    repo_a, _repo_b, _repo_c, _b_mount, nested_mount = _build_nested_link_repos(
        new_lore_repo
    )

    inner_file = f"{nested_mount}/c-data/inner.txt"
    new_inner_file = f"{nested_mount}/c-data/added.txt"

    # --- stage a modification two levels deep -----------------------------
    with repo_a.open_file(inner_file, "w+") as f:
        f.writelines(["modified content two levels deep\n"])
    with repo_a.open_file(new_inner_file, "w+") as f:
        f.writelines(["a brand new file inside C\n"])

    repo_a.stage(scan=True)

    status = repo_a.status()
    assert "Changes staged for commit" in status, "Deep change should stage"
    assert "M " + inner_file in status, "Modified inner file should be staged"
    assert "A " + new_inner_file in status, "Added inner file should be staged"

    # --- unstage the addition, keep the modification ----------------------
    repo_a.unstage(new_inner_file)
    status_after_unstage = repo_a.status()

    # The unstaged addition must drop out of the staged section (it becomes an
    # untracked file), while the modification stays staged.
    staged_section = status_after_unstage.split("Untracked files:")[0]
    assert "A " + new_inner_file not in staged_section, (
        "Unstaged addition should no longer be in the staged section"
    )
    assert "M " + inner_file in staged_section, (
        "Modification should remain staged after unstaging the addition"
    )
    assert new_inner_file in status_after_unstage, (
        "Unstaged addition should still show as an untracked file"
    )

    # --- commit the modification, then reset a fresh deep change ----------
    repo_a.stage(scan=True)
    repo_a.commit("Modify inner file two levels deep")
    repo_a.push()

    assert repo_a.compare_file(repo_a, inner_file), (
        "Committed deep modification should match on disk"
    )

    # A fresh clone must reproduce the committed deep modification.
    fresh = repo_a.clone()
    assert fresh.compare_file(repo_a, inner_file), (
        "Deep modification should survive commit + clone"
    )

    # reset a new deep modification back to committed content
    with repo_a.open_file(inner_file, "w+") as f:
        f.writelines(["TEMP deep modification to be reset\n"])
    repo_a.reset(inner_file)
    with repo_a.open_file(inner_file, "r") as f:
        content = f.read()
    assert "TEMP deep modification" not in content, "reset should discard deep change"
    assert "modified content two levels deep" in content, (
        "reset should restore committed deep content"
    )


@pytest.mark.smoke
def test_nested_link_update(new_lore_repo):
    """`link update` re-pins a NESTED link (the B -> C link) to a new revision.

    The re-pin and its registry write must target B's registry, not A's, and
    the new B revision must propagate up to A's pin.
    """
    repo_a, _repo_b, repo_c, _b_mount, nested_mount = _build_nested_link_repos(
        new_lore_repo
    )

    # Advance C with a new revision the nested link can be re-pinned to.
    clone_c = repo_c.clone()
    with clone_c.open_file("c-data/inner.txt", "w+") as f:
        f.writelines(["C revision two\n"])
    with clone_c.open_file("c-v2-only.txt", "w+") as f:
        f.writelines(["only present in C v2\n"])
    clone_c.stage(scan=True)
    clone_c.commit("C v2")
    clone_c.push()
    c_v2 = clone_c.branch_info().local_latest

    # Re-pin the nested link to C's v2.
    output = repo_a.link_update(nested_mount, pin=c_v2)
    assert "updated" in output.lower(), "Nested link update should succeed"

    v2_only = f"{nested_mount}/c-v2-only.txt"
    assert repo_a.file_exists(v2_only), "C v2-only file should appear after re-pin"

    link_list = repo_a.link_list()
    assert c_v2 in link_list, "Nested link should show the new C v2 pin"

    repo_a.commit("Re-pin nested link to C v2")
    repo_a.push()

    fresh = repo_a.clone()
    assert fresh.file_exists(v2_only), "Re-pinned nested content should survive clone"


@pytest.mark.smoke
def test_nested_link_remove(new_lore_repo):
    """`link remove` removes a NESTED link (B -> C) and its registry entry.

    Removal must delete C's registry entry from B's registry, drop C's content
    from the mount, and propagate the new B revision up to A.
    """
    repo_a, _repo_b, repo_c, _b_mount, nested_mount = _build_nested_link_repos(
        new_lore_repo
    )

    inner_file = f"{nested_mount}/c-data/inner.txt"
    assert repo_a.file_exists(inner_file), "Precondition: nested content present"

    output = repo_a.link_remove(nested_mount)
    assert "Removed link" in output, "Nested link removal should succeed"

    assert not repo_a.file_exists(inner_file), (
        "Nested linked content should be gone after removal"
    )

    link_list = repo_a.link_list()
    assert repo_c.get_id() not in link_list, (
        "C should no longer appear in link list after nested removal"
    )

    repo_a.commit("Remove nested link to C")
    repo_a.push()

    # Removal must sync to a fresh clone.
    fresh = repo_a.clone()
    assert not fresh.file_exists(inner_file), (
        "Nested link removal should propagate to a fresh clone"
    )
    fresh_link_list = fresh.link_list()
    assert repo_c.get_id() not in fresh_link_list, (
        "C should not appear in a fresh clone's link list after removal"
    )


@pytest.mark.smoke
def test_nested_link_list_shows_nesting(new_lore_repo):
    """`link list` enumerates nested links with their full A-relative paths.

    Both the B link (top-level) and the C link (nested in B) must appear, each
    with the correct full mount path.
    """
    repo_a, repo_b, repo_c, b_mount, nested_mount = _build_nested_link_repos(
        new_lore_repo
    )

    output = repo_a.link_list()

    assert repo_b.get_id() in output, "B (top-level link) should be listed"
    assert repo_c.get_id() in output, "C (nested link) should be listed"
    assert b_mount in output, "B's mount path should be listed"
    assert nested_mount in output, (
        "C's full nested mount path (parent mount + inner path) should be listed"
    )

    # A fresh clone should list the same nesting.
    fresh = repo_a.clone()
    fresh_output = fresh.link_list()
    assert repo_c.get_id() in fresh_output, (
        "Nested link should be listed in a fresh clone too"
    )
    assert nested_mount in fresh_output, (
        "Nested mount path should be listed in a fresh clone too"
    )


def _build_nested_link_repos_at(new_lore_repo, b_mount, c_inner_mount):
    """Build A -> B -> C with configurable mount paths.

    A links B at `b_mount`; C is linked at `<b_mount>/<c_inner_mount>` (i.e.
    `c_inner_mount` is C's path *relative to B's root*). Returns
    (repo_a, repo_b, repo_c, b_mount, nested_mount).

    Lets tests control the relative depths of the outer and inner mounts, which
    matters for the tracker's inner-first ordering.
    """
    repo_c = new_lore_repo()
    repo_c.make_dirs("c-data")
    with repo_c.open_file("c-data/inner.txt", "w+") as f:
        f.writelines(["content from repository C\n"])
    repo_c.stage(scan=True)
    repo_c.commit("C initial")
    repo_c.push()

    repo_b = new_lore_repo()
    with repo_b.open_file("b-root.txt", "w+") as f:
        f.writelines(["content from repository B\n"])
    repo_b.stage(scan=True)
    repo_b.commit("B initial")
    repo_b.push()

    repo_a = new_lore_repo()
    with repo_a.open_file("a-root.txt", "w+") as f:
        f.writelines(["content from repository A\n"])
    repo_a.stage(scan=True)
    repo_a.commit("A initial")
    repo_a.push()

    repo_a.link_add(b_mount, repo_b.get_id(), "/")
    repo_a.commit("A links B")
    repo_a.push()

    nested_mount = f"{b_mount}/{c_inner_mount}"
    repo_a.link_add(nested_mount, repo_c.get_id(), "/")
    repo_a.commit("A links B links C")
    repo_a.push()

    return repo_a, repo_b, repo_c, b_mount, nested_mount


@pytest.mark.smoke
def test_nested_link_unstage_deep_outer_shallow_inner(new_lore_repo):
    """Nested unstage when the OUTER mount is deeper than the INNER mount.

    Regression test for the tracker's inner-first ordering: B is mounted deep
    (`libs/vendor/b`, 2 slashes) and C is mounted at B's root (`c`, 0 slashes).
    A naive deepest-first sort keyed on the link's own mount-path slash count
    would order B (outer) before C (inner) and fold a stale B into A's pin.

    Modify a file two levels deep, stage it, then unstage it, and verify the
    change fully reverts (both the deep file and, after commit of a real deep
    change and a fresh clone, that the intermediate pins are consistent).
    """
    repo_a, _repo_b, _repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "libs/vendor/b", "c"
    )

    inner_file = f"{nested_mount}/c-data/inner.txt"
    added_file = f"{nested_mount}/c-data/added.txt"

    with repo_a.open_file(inner_file, "w+") as f:
        f.writelines(["deep modification\n"])
    with repo_a.open_file(added_file, "w+") as f:
        f.writelines(["a new deep file\n"])

    repo_a.stage(scan=True)
    staged = repo_a.status()
    assert "M " + inner_file in staged, "Deep modification should stage"
    assert "A " + added_file in staged, "Deep addition should stage"

    # Unstage everything; both changes must leave the staged section.
    repo_a.unstage(".")
    after = repo_a.status()
    staged_section = after.split("Untracked files:")[0]
    assert "M " + inner_file not in staged_section, (
        "Deep modification should be unstaged (not folded via a stale pin)"
    )
    assert "A " + added_file not in staged_section, (
        "Deep addition should be unstaged"
    )

    # Now re-stage and commit a real deep change; a fresh clone must reproduce
    # it, proving the intermediate B pin was folded correctly (not stale).
    with repo_a.open_file(inner_file, "w+") as f:
        f.writelines(["committed deep modification\n"])
    repo_a.stage(scan=True)
    repo_a.commit("Modify inner file two levels deep")
    repo_a.push()

    fresh = repo_a.clone()
    assert fresh.compare_file(repo_a, inner_file), (
        "Deep modification should survive commit + clone (pins folded correctly)"
    )


@pytest.mark.smoke
def test_nested_link_list_staged_shows_nested(new_lore_repo):
    """`link list --staged` reports a nested link with staged content.

    Stage a change two levels deep, then `link list --staged` must include the
    nested C link (not only the top-level B link).
    """
    repo_a, _repo_b, repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    inner_file = f"{nested_mount}/c-data/inner.txt"
    with repo_a.open_file(inner_file, "w+") as f:
        f.writelines(["staged deep change\n"])
    repo_a.stage(scan=True)

    # `link list --staged` reports by mount path + staged file count (no repo
    # id). The nested link must appear with its full A-relative path.
    staged_list = repo_a.link_list(staged=True)
    assert nested_mount in staged_list, (
        "Nested link's full mount path should appear in link list --staged"
    )


@pytest.mark.smoke
def test_nested_link_commit_scoped(new_lore_repo):
    """`commit --link <nested-path>` commits only the nested link's changes.

    Scoped commit must resolve the nested path to the innermost repo (C),
    commit C's staged change, and fold the pin up to A.
    """
    repo_a, _repo_b, _repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    inner_file = f"{nested_mount}/c-data/inner.txt"
    with repo_a.open_file(inner_file, "w+") as f:
        f.writelines(["scoped commit change\n"])
    repo_a.stage(scan=True)

    output = repo_a.commit("Scoped nested commit", link=nested_mount)
    assert "Commit succeeded" in output, (
        f"Scoped commit of a nested link should succeed.\nOutput:\n{output}"
    )

    # The scoped commit advances C (and the intermediate B) to real revisions
    # and stages the resulting pin change on the top-level parent. Committing
    # the parent records it on A's branch; the change then survives a clone.
    repo_a.commit("Record scoped nested commit on parent")
    repo_a.push()

    fresh = repo_a.clone()
    assert fresh.compare_file(repo_a, inner_file), (
        "Scoped-committed deep change should survive a fresh clone"
    )


@pytest.mark.smoke
def test_nested_link_stage_delete_deep(new_lore_repo):
    """Staging a DELETE of a file two levels deep works.

    Regression for the stage delete-fallback that used a top-level-only parent
    lookup. Delete a file owned by C (mounted under B under A), stage it, and
    verify it commits and disappears from a fresh clone.
    """
    repo_a, _repo_b, _repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    deep_file = f"{nested_mount}/c-data/inner.txt"
    assert repo_a.file_exists(deep_file), "Precondition: deep file present"

    repo_a.remove_file(deep_file)
    output = repo_a.stage(scan=True)
    status = repo_a.status()
    assert "D " + deep_file in status, (
        f"Deep delete should be staged.\nStage output:\n{output}\nStatus:\n{status}"
    )

    repo_a.commit("Delete a file two levels deep")
    repo_a.push()

    fresh = repo_a.clone()
    assert not fresh.file_exists(deep_file), (
        "Deep delete should propagate to a fresh clone"
    )


@pytest.mark.smoke
def test_nested_link_reset_deep(new_lore_repo):
    """`reset` of a modified file two levels deep restores committed content."""
    repo_a, _repo_b, _repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    deep_file = f"{nested_mount}/c-data/inner.txt"
    with repo_a.open_file(deep_file, "r") as f:
        original = f.read()

    with repo_a.open_file(deep_file, "w+") as f:
        f.writelines(["temporary deep change to be reset\n"])

    repo_a.reset(deep_file)
    with repo_a.open_file(deep_file, "r") as f:
        content = f.read()
    assert "temporary deep change" not in content, "reset should discard deep change"
    assert content == original, "reset should restore committed deep content exactly"


@pytest.mark.smoke
def test_nested_link_commit_message(new_lore_repo):
    """`--link-message <nested-path> <msg>` applies to the nested link's commit.

    Per-link commit messages are keyed by the full mount path, so a nested
    link (C, at `vendor/b/vendor/c`) receives its own message rather than the
    main message. Verified by reading C's committed revision message.
    """
    repo_a, _repo_b, repo_c, _b_mount, nested_mount = _build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "vendor/c"
    )

    inner_file = f"{nested_mount}/c-data/inner.txt"
    with repo_a.open_file(inner_file, "w+") as f:
        f.writelines(["change for per-link message test\n"])
    repo_a.stage(scan=True)

    nested_message = "Dedicated message for nested link C"
    repo_a.commit(
        "Main parent message",
        link_messages={nested_mount: nested_message},
    )
    repo_a.push()

    # C's latest committed revision must carry the nested message, not the main
    # message. Read it from a fresh clone of C.
    clone_c = repo_c.clone()
    c_message = clone_c.revision_metadata_get("message")
    assert nested_message in c_message, (
        f"Nested link commit should use its --link-message.\n"
        f"Expected: {nested_message!r}\nGot: {c_message!r}"
    )
    assert "Main parent message" not in c_message, (
        "Nested link commit should not fall back to the main message when a "
        "per-link message was supplied"
    )


@pytest.mark.smoke
def test_link_branch_create_reuses_existing_link_branch_id(new_lore_repo):
    """Branch creation succeeds when the linked repo already holds the
    requested branch ID, and the existing linked branch is reused.

    The auto-follow cascade creates the parent's branch ID in every linked
    repository. When that ID is already present there, the cascade adopts it:
    the parent branch is created, and the linked branch keeps its identity
    and its latest revision.
    """
    parent, link_repo = _make_parent_with_link(new_lore_repo)

    branch_id = "5b9c1d2e3f4a4b5c8d9e0f1a2b3c4d5e"
    branch_name = "shared-feature"

    link_repo.branch_create(branch_name, id=branch_id)
    link_repo.write_commit_push("Link work on shared branch", {"on-branch.txt": "x\n"})
    link_latest_before = link_repo.branch_info(branch_id).local_latest

    parent.branch_create(branch_name, id=branch_id)

    parent_branch = parent.branch_info()
    assert parent_branch.name == branch_name, (
        f"Parent must be on the newly created branch.\nGot: {parent_branch.name!r}"
    )
    assert parent_branch.id == branch_id, (
        "Parent branch must carry the requested branch ID.\n"
        f"Expected: {branch_id}\nGot: {parent_branch.id}"
    )

    link_branch = link_repo.branch_info(branch_id)
    assert link_branch.name == branch_name, (
        "The pre-existing linked branch must be reused under its own name.\n"
        f"Expected: {branch_name!r}\nGot: {link_branch.name!r}"
    )
    assert link_branch.local_latest == link_latest_before, (
        "Reusing the linked branch must leave its latest revision untouched.\n"
        f"Expected: {link_latest_before}\nGot: {link_branch.local_latest}"
    )

    link_output = parent.link_list()
    assert branch_name in link_output, (
        f"link list must resolve the link to {branch_name!r}.\nOutput:\n{link_output}"
    )


@pytest.mark.smoke
def test_link_branch_create_reports_reuse_in_event(new_lore_repo):
    """Reusing a linked branch is reported as `linkBranchCreate` with
    `reused` set.

    The event carries the mount path, the linked repository, the branch ID,
    and that branch's latest revision, so a caller can tell an adopted branch
    from a freshly created one without parsing text output.
    """
    parent, link_repo = _make_parent_with_link(new_lore_repo)

    branch_id = "3d4e5f60718293a4b5c6d7e8f9a0b1c2"
    branch_name = "evented-feature"

    link_repo.branch_create(branch_name, id=branch_id)
    link_repo.write_commit_push("Link work", {"evented.txt": "w\n"})
    link_head = link_repo.branch_info(branch_id).local_latest

    output = parent.branch_create(branch_name, id=branch_id, json=True)

    events = parse_jsonl(output, "linkBranchCreate")
    assert len(events) == 1, (
        f"Expected exactly one linkBranchCreate event.\nOutput:\n{output}"
    )
    event = events[0]
    assert event["reused"] is True, (
        f"Event must mark the branch as reused.\nGot: {event}"
    )
    assert event["linkPath"] == _DEFAULT_LINK_MOUNT, (
        f"Event must name the mount path.\nExpected: {_DEFAULT_LINK_MOUNT}\n"
        f"Got: {event['linkPath']}"
    )
    assert event["linkRepository"] == link_repo.get_id(), (
        "Event must name the linked repository holding the reused branch.\n"
        f"Expected: {link_repo.get_id()}\nGot: {event['linkRepository']}"
    )
    assert event["branch"] == branch_id, (
        f"Event must name the reused branch ID.\nExpected: {branch_id}\n"
        f"Got: {event['branch']}"
    )
    assert event["revision"] == link_head, (
        "Event must carry the reused branch's latest revision.\n"
        f"Expected: {link_head}\nGot: {event['revision']}"
    )

    created = parse_jsonl(output, "branchCreate")
    assert [entry["name"] for entry in created] == [branch_name], (
        f"The parent branch must still be reported as created.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_branch_create_reports_creation_in_event(new_lore_repo):
    """Creating a link's branch is reported as `linkBranchCreate` with
    `reused` clear.

    The cascade reports an outcome for every link it touches, not only the
    reuse case, so a caller sees what happened in each linked repository.
    """
    parent, link_repo = _make_parent_with_link(new_lore_repo)

    branch_name = "fresh-feature"
    output = parent.branch_create(branch_name, json=True)

    events = parse_jsonl(output, "linkBranchCreate")
    assert len(events) == 1, (
        f"Expected exactly one linkBranchCreate event.\nOutput:\n{output}"
    )
    event = events[0]
    assert event["reused"] is False, (
        f"Event must mark the branch as newly created.\nGot: {event}"
    )
    assert event["linkPath"] == _DEFAULT_LINK_MOUNT, (
        f"Event must name the mount path.\nExpected: {_DEFAULT_LINK_MOUNT}\n"
        f"Got: {event['linkPath']}"
    )
    assert event["linkRepository"] == link_repo.get_id(), (
        f"Event must name the linked repository.\nGot: {event['linkRepository']}"
    )

    branch_list = link_repo.branch_list()
    assert branch_name in branch_list.remote_branches, (
        f"The branch must exist in the linked repository.\nGot: {branch_list}"
    )


@pytest.mark.smoke
def test_link_branch_create_reports_each_mount_of_same_repo(new_lore_repo):
    """One outcome event per mount when a repository is linked twice.

    A repository can be mounted at more than one path, so the mount path is
    what distinguishes the two cascade entries - the repository ID is the same
    for both.
    """
    parent: Lore = new_lore_repo()
    parent.write_commit_push("Initial parent", {"parent.txt": "parent content\n"})

    link_repo: Lore = new_lore_repo()
    link_repo.write_commit_push("Initial link", {"linked.txt": "linked content\n"})

    mounts = ["vendor/first", "vendor/second"]
    for mount in mounts:
        parent.link_add(mount, link_repo.get_id(), "/")
        parent.commit(f"Add link at {mount}")
        parent.push()

    output = parent.branch_create("dual-mount", json=True)

    events = parse_jsonl(output, "linkBranchCreate")
    assert sorted(event["linkPath"] for event in events) == mounts, (
        "Each mount of the linked repository must report its own outcome "
        f"event.\nOutput:\n{output}"
    )
    assert {event["linkRepository"] for event in events} == {link_repo.get_id()}, (
        "Both events must name the same linked repository, which is why the "
        f"mount path is needed to tell them apart.\nOutput:\n{output}"
    )
    assert len({event["revision"] for event in events}) == 1, (
        "Both mounts address one branch in one repository, so the cascade must "
        "resolve it once and report the same revision for every mount rather "
        f"than creating it per mount.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_branch_create_reuses_link_branch_id_under_other_name(new_lore_repo):
    """A linked branch that already owns the requested ID keeps its own name.

    Adoption is keyed on the branch ID, so a linked branch created earlier
    under a different name is reused as-is rather than renamed or recreated.
    """
    parent, link_repo = _make_parent_with_link(new_lore_repo)

    branch_id = "7a1b2c3d4e5f6071829304a5b6c7d8e9"
    link_branch_name = "link-local-name"
    parent_branch_name = "parent-name"

    link_repo.branch_create(link_branch_name, id=branch_id)
    link_repo.write_commit_push("Link work", {"link-only.txt": "y\n"})
    link_latest_before = link_repo.branch_info(branch_id).local_latest

    parent.branch_create(parent_branch_name, id=branch_id)

    assert parent.branch_info().name == parent_branch_name, (
        "Parent branch must be created under the requested name"
    )

    link_branch = link_repo.branch_info(branch_id)
    assert link_branch.name == link_branch_name, (
        "The linked branch must keep the name it was created with.\n"
        f"Expected: {link_branch_name!r}\nGot: {link_branch.name!r}"
    )
    assert link_branch.local_latest == link_latest_before, (
        "Adopting the linked branch must leave its latest revision untouched.\n"
        f"Expected: {link_latest_before}\nGot: {link_branch.local_latest}"
    )


@pytest.mark.smoke
def test_link_branch_create_commits_onto_reused_link_branch(new_lore_repo):
    """Work committed through the mount lands on the reused linked branch.

    Proves adoption leaves a usable link: after the parent branch is created
    on top of a pre-existing linked branch, a commit through the mount path
    advances that same linked branch.
    """
    parent, link_repo = _make_parent_with_link(new_lore_repo)

    branch_id = "1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f"
    branch_name = "adopted-feature"

    link_repo.branch_create(branch_name, id=branch_id)
    link_repo.push()
    link_latest_before = link_repo.branch_info().local_latest

    parent.branch_create(branch_name, id=branch_id)

    mounted_file = f"{_DEFAULT_LINK_MOUNT}/through-mount.txt"
    with parent.open_file(mounted_file, "w+") as f:
        f.write("written through the mount\n")
    parent.stage(scan=True)
    parent.commit("Commit into adopted link branch")
    parent.push()

    link_repo.sync()
    link_branch_after = link_repo.branch_info()
    assert link_branch_after.id == branch_id, (
        "The linked repo must still be on the reused branch"
    )
    assert link_branch_after.local_latest != link_latest_before, (
        "Committing through the mount must advance the reused linked branch"
    )
    assert link_repo.file_exists("through-mount.txt"), (
        "The file committed through the mount must be present on the reused "
        "linked branch"
    )


@pytest.mark.smoke
def test_link_update_after_reusing_link_branch_pulls_branch_head(new_lore_repo):
    """`link update` after adoption re-pins to the reused branch's head.

    The adopted branch may already carry revisions the parent's pin predates.
    Those become available the moment the link is updated, which is the normal
    way a pin advances.
    """
    parent, link_repo = _make_parent_with_link(new_lore_repo)

    branch_id = "2f3e4d5c6b7a8908172635445362718f"
    branch_name = "ahead-feature"

    link_repo.branch_create(branch_name, id=branch_id)
    link_repo.write_commit_push(
        "Work already on the linked branch", {"ahead.txt": "z\n"}
    )
    link_head = link_repo.branch_info(branch_id).local_latest

    parent.branch_create(branch_name, id=branch_id)

    parent.link_update(_DEFAULT_LINK_MOUNT)
    parent.commit("Update link to adopted branch head")
    parent.push()

    assert parent.file_exists(f"{_DEFAULT_LINK_MOUNT}/ahead.txt"), (
        "link update must materialise content from the reused branch's head"
    )

    link_output = parent.link_list()
    assert link_head in link_output, (
        "link list must report the reused branch's head as the pin.\n"
        f"Expected: {link_head}\nOutput:\n{link_output}"
    )


def test_push_names_parent_branch_for_parent_revision(new_lore_repo):
    """`lore push` must attribute the parent's revision to the parent's branch.

    When a parent repository links a child and both create a branch of the same
    name, the cascade disambiguates the child's
    branch by appending the parent branch id (child ends up with
    ``<name>-<parent branch id>``). During push the parent walks its own history
    and, for each revision, pushes the link first before pushing the parent's own
    revision. The CLI stored the branch name of the most recent BranchPush event
    in shared state, so the child's BranchPush (fired while pushing the link)
    overwrote it. The parent's subsequent "Pushing <rev> to branch <name>" /
    "Pushed revision N -> <rev> to branch <name>" lines then printed the child's
    disambiguated branch name even though the revision was the parent's.

    This asserts the correct, positive behaviour: the push line that reports the
    parent's own revision names the parent's branch (``test``).
    """
    child_repo: Lore = new_lore_repo()
    with child_repo.open_file("child-file.txt", "w+") as f:
        f.writelines(["initial child content\n"])
    child_repo.stage(scan=True)
    child_repo.commit("Initial child")
    child_repo.push()

    # Occupy `test` in the child so the parent's cascade must disambiguate it.
    child_repo.branch_create("test")
    child_repo.push("test")
    child_repo.branch_switch("main")

    parent_repo: Lore = new_lore_repo()
    with parent_repo.open_file("parent-file.txt", "w+") as f:
        f.writelines(["parent content\n"])
    parent_repo.stage(scan=True)
    parent_repo.commit("Initial parent")
    parent_repo.push()

    link_path = "linked"
    parent_repo.link_add(link_path, child_repo.get_id(), "/")
    parent_repo.commit("Add link")
    parent_repo.push()

    parent_repo.branch_create("test")
    parent_branch_id = parent_repo.branch_info().id
    disambiguated_child_branch = f"test-{parent_branch_id}"

    with parent_repo.open_file(f"{link_path}/child-file.txt", "w+") as f:
        f.writelines(["updated via parent link mount\n"])
    parent_repo.stage(f"{link_path}/child-file.txt")
    parent_repo.commit("probe")

    parent_revision = parent_repo.branch_info().local_latest

    push_output = parent_repo.push()

    # Key on the parent's revision signature to isolate the parent's push lines.
    begin_match = re.search(
        rf"Pushing {parent_revision} to branch (\S+)", push_output
    )
    assert begin_match, (
        f"Expected a push line for the parent's revision {parent_revision}.\n"
        f"Push output:\n{push_output}"
    )
    assert begin_match.group(1) == "test", (
        f"The parent's revision {parent_revision} must be reported as pushed to "
        f"the parent's branch 'test', but it named '{begin_match.group(1)}'. "
        f"'{disambiguated_child_branch}' is the child's branch, which exists only "
        f"in the linked repository.\nPush output:\n{push_output}"
    )

    end_match = re.search(
        rf"Pushed revision \d+ -> {parent_revision} to branch (\S+)", push_output
    )
    assert end_match, (
        f"Expected a 'Pushed revision' line for the parent's revision "
        f"{parent_revision}.\nPush output:\n{push_output}"
    )
    assert end_match.group(1) == "test", (
        f"The parent's revision {parent_revision} must be reported as pushed to "
        f"the parent's branch 'test', but it named '{end_match.group(1)}'. "
        f"'{disambiguated_child_branch}' is the child's branch, which exists only "
        f"in the linked repository.\nPush output:\n{push_output}"
    )


def _remote_branch_entries(repo: Lore) -> list[dict]:
    """Remote branch list entries for a repository.

    Archived branches are deliberately not requested. The remote leg reports
    every entry as unarchived and the client asks for archived branches to be
    excluded, so an archived branch is absent from this list entirely rather than
    present with a flag set — its absence is the signal, and asking for archived
    entries would only cost an extra scan.

    Read from JSON rather than the text parser because the text form drops the
    branch id, and the id is what branch creation shares between a parent branch
    and the branch it creates in a linked repository.
    """
    output = repo.branch_list(json=True)
    return [
        e for e in parse_jsonl(output, "branchListEntry") if e["location"] == "remote"
    ]


@pytest.mark.smoke
def test_link_branch_archive_leaves_child_branch(new_lore_repo):
    """Archiving a branch in a parent does not remove the linked repository's.

    Branch create cascades into the linked repository using the same branch ID.
    Archiving in the parent then touches only the parent.
    """
    parent, link_repo = _make_parent_with_link(new_lore_repo)

    # Branch create switches the parent onto the new branch and cascades.
    parent.branch_create("feature")
    parent_branch_id = parent.branch_info("feature").id

    cascaded = [
        e for e in _remote_branch_entries(link_repo) if e["name"] == "feature"
    ]
    assert len(cascaded) == 1, (
        f"Branch create should cascade one 'feature' branch into the linked "
        f"repository, got {cascaded}"
    )
    assert cascaded[0]["id"] == parent_branch_id, (
        f"Cascaded branch should carry the parent branch ID {parent_branch_id}, "
        f"got {cascaded[0]['id']}"
    )

    # The current branch cannot be archived, so step off it first.
    parent.branch_switch("main")
    parent.branch_archive("feature")

    assert not parent.has_branch("feature"), (
        "Archived branch should be gone from the parent's branch list"
    )

    survivor = [
        e for e in _remote_branch_entries(link_repo) if e["name"] == "feature"
    ]
    assert len(survivor) == 1, (
        f"The linked repository should keep its branch when the parent "
        f"archives, got {survivor}"
    )
    assert survivor[0]["id"] == parent_branch_id, (
        "The linked branch should be untouched by the parent archive, ID included"
    )


@pytest.mark.smoke
def test_link_archived_parent_branch_does_not_disturb_link_resolution(new_lore_repo):
    """An archived parent branch is inert while the linked branch still exists.

    A link is pinned to a revision in the parent's state, not to a branch name,
    so switch, sync and link resolution never go through the archived branch.
    """
    parent, link_repo = _make_parent_with_link(new_lore_repo)

    # A sibling branch to switch through after the archive, so the switch is a
    # real branch change rather than a switch onto the current branch.
    parent.branch_create("sibling")
    parent.branch_switch("main")

    parent.branch_create("feature")
    parent.write_commit_push(
        "Commit inside the link mount on feature",
        {f"{_DEFAULT_LINK_MOUNT}/on-feature.txt": "written on the feature branch\n"},
    )

    parent.branch_switch("main")
    parent.branch_archive("feature")

    assert any(e["name"] == "feature" for e in _remote_branch_entries(link_repo)), (
        "Precondition: the linked branch outlives the parent's archive"
    )

    # main is unaffected: the link still resolves and its content is present.
    parent.sync()
    assert parent.file_exists(f"{_DEFAULT_LINK_MOUNT}/linked.txt"), (
        "Linked content should still be present on main after archiving another branch"
    )
    assert not parent.file_exists(f"{_DEFAULT_LINK_MOUNT}/on-feature.txt"), (
        "main should see its own pinned revision of the link, not the archived branch's"
    )

    links = parent.link_list()
    assert link_repo.get_id() in links, (
        f"Link should still resolve after the archive: {links}"
    )
    assert "[Error]" not in links, f"link list should not report an error: {links}"

    # Switching away and back still works with the archived branch present.
    parent.branch_switch("sibling")
    parent.branch_switch("main")
    assert parent.branch_list().current_branch == "main", (
        "Branch switch should work with an archived branch present"
    )


@pytest.mark.smoke
def test_link_stage_move_on_mount_is_refused(new_lore_repo):
    """Renaming a link mount is refused, and the link survives the attempt.

    `stage move` does not perform the rename itself, so the on-disk move has to
    happen first; without it the command fails on the missing destination with a
    generic os error and never reaches any node checks.

    The refusal is not link-aware: it is the generic non-directory guard, which
    fires because a mount is not a directory node — the same message a plain
    tracked file produces when moved onto a directory. So the message is not
    asserted here, and it will change if renaming a mount is ever implemented. The
    load-bearing assertions are the link registry checks below: the mount is still
    registered at its original path, and the rename was not recorded.
    """
    parent, link_repo = _make_parent_with_link(new_lore_repo)

    parent.move(_DEFAULT_LINK_MOUNT, "vendor/renamed")
    output = parent.stage_move(_DEFAULT_LINK_MOUNT, "vendor/renamed", check=False)

    assert "[Error]" in output, (
        f"Renaming a link mount should not succeed, got: {output}"
    )

    # The link is intact and still registered at its original path.
    links = parent.link_list()
    assert link_repo.get_id() in links, (
        f"Link should survive a refused stage move: {links}"
    )
    assert _DEFAULT_LINK_MOUNT in links or _DEFAULT_LINK_MOUNT.replace("/", "\\") in links, (
        f"Link path should be unchanged after a refused stage move: {links}"
    )
    assert "vendor/renamed" not in links and "vendor\\renamed" not in links, (
        f"The rename should not be recorded against the link: {links}"
    )



# ---------------------------------------------------------------------------
# Link tracking on the thin-client wire.
# ---------------------------------------------------------------------------


def _wire_identity(repo: Lore) -> tuple[bytes, bytes]:
    """The repository id and latest revision signature as the raw bytes the
    thin-client wire expects."""
    latest = repo.branch_info().local_latest
    assert len(latest) == 64, f"Expected a full revision signature, got {latest!r}"
    return bytes.fromhex(repo.get_id()), bytes.fromhex(latest)


def _mount_changes(changes: list, link_path: str) -> list:
    """Every diff entry describing the link node at `link_path`."""
    return [
        change
        for change in changes
        if change.path == link_path and change.node_type == NODE_TYPE_LINK
    ]


@pytest.mark.smoke
def test_thin_client_tree_discriminates_tracking_from_pinned(
    new_lore_repo, lore_grpc_target
):
    """One tree holding both link kinds and a plain file: only the link left on
    its parent's branch reports tracking. Asserting all three off a single
    response is what proves the field is populated rather than left at its
    default."""
    tracking_source = _commit_initial_main(new_lore_repo, "inner.txt")
    pinned_source = _commit_initial_main(new_lore_repo, "inner.txt")
    repo = _commit_initial_main(new_lore_repo, "own.txt")

    repo.link_add("tracked", tracking_source.get_id(), "/")
    repo.link_add("pinned", pinned_source.get_id(), "/", disable_branching=True)
    repo.commit()
    repo.push()

    repository_id, signature = _wire_identity(repo)
    nodes = {
        node.path: node
        for node in revision_tree(lore_grpc_target, repository_id, signature)
    }
    for path in ("tracked", "pinned", "own.txt"):
        assert path in nodes, f"{path} missing from tree: {sorted(nodes)}"

    assert nodes["tracked"].node_type == NODE_TYPE_LINK, (
        f"Mount path must be reported as a link, got {nodes['tracked']}"
    )
    assert nodes["tracked"].tracking, (
        f"A link following its parent's branch must report tracking, "
        f"got {nodes['tracked']}"
    )

    assert nodes["pinned"].node_type == NODE_TYPE_LINK, (
        f"Mount path must be reported as a link, got {nodes['pinned']}"
    )
    assert nodes["pinned"].tracking is False, (
        f"A link pinned to its own branch must report tracking False, "
        f"got {nodes['pinned']}"
    )

    assert nodes["own.txt"].node_type == NODE_TYPE_FILE, (
        f"Parent's own file must be reported as a file, got {nodes['own.txt']}"
    )
    assert nodes["own.txt"].tracking is False, (
        f"Tracking is a link property, so a file must report False, "
        f"got {nodes['own.txt']}"
    )


@pytest.mark.smoke
def test_thin_client_diff_reports_tracking_on_added_link(
    new_lore_repo, lore_grpc_target
):
    """Adding a tracking link is reported as a tracking link entry on the
    thin-client revision diff."""
    link_repo = _commit_initial_main(new_lore_repo, "inner.txt")
    repo = _commit_initial_main(new_lore_repo, "own.txt")

    repository_id, before = _wire_identity(repo)

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit()
    repo.push()

    _, after = _wire_identity(repo)
    changes = revision_diff(lore_grpc_target, repository_id, before, after)

    mount_changes = _mount_changes(changes, link_path)
    assert mount_changes, f"Mount path must be reported as a link entry, got {changes}"
    assert all(change.action == ACTION_ADD for change in mount_changes), (
        f"A newly mounted link must be reported as an add, got {mount_changes}"
    )
    assert all(change.tracking for change in mount_changes), (
        f"A link following its parent's branch must report tracking, "
        f"got {mount_changes}"
    )


@pytest.mark.smoke
def test_thin_client_diff_reports_tracking_on_removed_link(
    new_lore_repo, lore_grpc_target
):
    """Removing a tracking link reports tracking on the delete entry. A delete
    resolves against the "from" side, the only action that does."""
    link_repo = _commit_initial_main(new_lore_repo, "inner.txt")
    repo = _commit_initial_main(new_lore_repo, "own.txt")

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit()
    repo.push()

    repository_id, before = _wire_identity(repo)

    repo.link_remove(link_path)
    repo.commit()
    repo.push()

    _, after = _wire_identity(repo)
    changes = revision_diff(lore_grpc_target, repository_id, before, after)

    mount_changes = _mount_changes(changes, link_path)
    assert mount_changes, f"Mount path must be reported as a link entry, got {changes}"
    assert all(change.action == ACTION_DELETE for change in mount_changes), (
        f"A removed link must be reported as a delete, got {mount_changes}"
    )
    assert all(change.tracking for change in mount_changes), (
        f"A link following its parent's branch must report tracking, "
        f"got {mount_changes}"
    )


@pytest.mark.smoke
def test_thin_client_diff_reports_tracking_on_moved_pin(
    new_lore_repo, lore_grpc_target
):
    """Committing content through a tracking link moves its pin, and the pin
    move is reported as a tracking link entry on the thin-client revision
    diff, while the parent's own file change is not."""
    link_repo = _commit_initial_main(new_lore_repo, "inner.txt")
    repo = _commit_initial_main(new_lore_repo, "own.txt")

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit()
    repo.push()

    repository_id, before = _wire_identity(repo)

    with repo.open_file(os.path.join(link_path, "inner.txt"), "w+") as output_file:
        output_file.writelines(["linked content, revised\n"])
    with repo.open_file("own.txt", "w+") as output_file:
        output_file.writelines(["parent content, revised\n"])
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    _, after = _wire_identity(repo)
    changes = revision_diff(lore_grpc_target, repository_id, before, after)

    mount_changes = _mount_changes(changes, link_path)
    assert mount_changes, (
        f"Moved pin must be reported as a link entry for the mount path, got {changes}"
    )
    assert all(change.tracking for change in mount_changes), (
        f"A link following its parent's branch must report tracking, "
        f"got {mount_changes}"
    )

    own_changes = [change for change in changes if change.path == "own.txt"]
    assert own_changes, f"Parent's own file change missing from diff: {changes}"
    assert all(change.tracking is False for change in own_changes), (
        f"Tracking is a link property, so a file change must report False, "
        f"got {own_changes}"
    )


# ---------------------------------------------------------------------------
# Link partitions on the thin-client wire.
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_thin_client_diff_partitions_added_link_under_linked_repository(
    new_lore_repo, lore_grpc_target
):
    """A newly mounted link is the only diff entry for its mount path, and the
    content it names is a revision of the linked repository, so the entry must
    be partitioned there for a consumer to find that revision."""
    link_repo = _commit_initial_main(new_lore_repo, "inner.txt")
    repo = _commit_initial_main(new_lore_repo, "own.txt")

    repository_id, before = _wire_identity(repo)

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit()
    repo.push()

    _, after = _wire_identity(repo)
    changes = revision_diff(lore_grpc_target, repository_id, before, after)

    mount_changes = _mount_changes(changes, link_path)
    assert mount_changes, f"Mount path must be reported as a link entry, got {changes}"
    assert all(change.action == ACTION_ADD for change in mount_changes), (
        f"A newly mounted link must be reported as an add, got {mount_changes}"
    )
    assert all(change.partition == link_repo.get_id() for change in mount_changes), (
        f"A link entry's content is a revision of the linked repository "
        f"{link_repo.get_id()}, so it must be partitioned there, got {mount_changes}"
    )


@pytest.mark.smoke
def test_thin_client_diff_partitions_removed_link_under_linked_repository(
    new_lore_repo, lore_grpc_target
):
    """A removed link names the revision it was pinned to, which still lives in
    the linked repository, so the delete entry must be partitioned there. A
    delete resolves against the "from" side, the only action that does."""
    link_repo = _commit_initial_main(new_lore_repo, "inner.txt")
    repo = _commit_initial_main(new_lore_repo, "own.txt")

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit()
    repo.push()

    repository_id, before = _wire_identity(repo)

    repo.link_remove(link_path)
    repo.commit()
    repo.push()

    _, after = _wire_identity(repo)
    changes = revision_diff(lore_grpc_target, repository_id, before, after)

    mount_changes = _mount_changes(changes, link_path)
    assert mount_changes, f"Mount path must be reported as a link entry, got {changes}"
    assert all(change.action == ACTION_DELETE for change in mount_changes), (
        f"A removed link must be reported as a delete, got {mount_changes}"
    )
    assert all(change.partition == link_repo.get_id() for change in mount_changes), (
        f"A removed link's content is the revision of the linked repository "
        f"{link_repo.get_id()} it was pinned to, so it must be partitioned there, "
        f"got {mount_changes}"
    )


@pytest.mark.smoke
def test_thin_client_diff_partitions_moved_pin_under_linked_repository(
    new_lore_repo, lore_grpc_target
):
    """Committing through a link moves its pin, and every entry the diff reports
    for the mount path must agree on the linked repository as its partition,
    while the parent's own file stays in the parent's."""
    link_repo = _commit_initial_main(new_lore_repo, "inner.txt")
    repo = _commit_initial_main(new_lore_repo, "own.txt")

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit()
    repo.push()

    repository_id, before = _wire_identity(repo)

    with repo.open_file(os.path.join(link_path, "inner.txt"), "w+") as output_file:
        output_file.writelines(["linked content, revised\n"])
    with repo.open_file("own.txt", "w+") as output_file:
        output_file.writelines(["parent content, revised\n"])
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    _, after = _wire_identity(repo)
    changes = revision_diff(lore_grpc_target, repository_id, before, after)

    mount_changes = _mount_changes(changes, link_path)
    assert mount_changes, (
        f"Moved pin must be reported as a link entry for the mount path, got {changes}"
    )
    assert all(change.partition == link_repo.get_id() for change in mount_changes), (
        f"A moved pin names revisions of the linked repository "
        f"{link_repo.get_id()}, so every entry for the mount path must be "
        f"partitioned there, got {mount_changes}"
    )

    own_changes = [change for change in changes if change.path == "own.txt"]
    assert own_changes, f"Parent's own file change missing from diff: {changes}"
    assert all(change.partition == repo.get_id() for change in own_changes), (
        f"The parent's own file lives in {repo.get_id()}, so its change must be "
        f"partitioned there, got {own_changes}"
    )


@pytest.mark.smoke
def test_thin_client_diff_partitions_each_link_under_its_own_repository(
    new_lore_repo, lore_grpc_target
):
    """Two links added in one commit take separate partition indices, and each
    mount entry resolves to the repository it mounts."""
    first_repo = _commit_initial_main(new_lore_repo, "first.txt")
    second_repo = _commit_initial_main(new_lore_repo, "second.txt")
    repo = _commit_initial_main(new_lore_repo, "own.txt")

    repository_id, before = _wire_identity(repo)

    repo.link_add("first", first_repo.get_id(), "/")
    repo.link_add("second", second_repo.get_id(), "/")
    repo.commit()
    repo.push()

    _, after = _wire_identity(repo)
    changes = revision_diff(lore_grpc_target, repository_id, before, after)

    for link_path, link_repo in (("first", first_repo), ("second", second_repo)):
        mount_changes = _mount_changes(changes, link_path)
        assert mount_changes, (
            f"Mount path {link_path} must be reported as a link entry, got {changes}"
        )
        assert all(
            change.partition == link_repo.get_id() for change in mount_changes
        ), (
            f"Mount path {link_path} mounts {link_repo.get_id()}, so its entries "
            f"must be partitioned there, got {mount_changes}"
        )


# ---------------------------------------------------------------------------
# Branch archive cascading into links.
# ---------------------------------------------------------------------------


def _setup_repo_with_two_links(new_lore_repo):
    """Set up a parent repo with two links (sec, thr) and content in each.

    Returns (parent_repo, second_repo, third_repo).
    """
    repo: Lore = new_lore_repo()
    second_repo: Lore = new_lore_repo(repo.name + "_second")
    third_repo: Lore = new_lore_repo(repo.name + "_third")

    repo.write_commit_push(None, {"main.txt": b"main content"})
    second_repo.write_commit_push(None, {"second.txt": b"second content"})
    third_repo.write_commit_push(None, {"third.txt": b"third content"})

    repo.link_add("sec", second_repo.get_id(), "/")
    repo.link_add("thr", third_repo.get_id(), "/")
    repo.commit("add links")
    repo.push()
    return repo, second_repo, third_repo


@pytest.mark.smoke
def test_link_branch_archive_leaves_link_by_default(new_lore_repo):
    """`branch archive` touches only the repository it ran in."""
    repo, link_repo = _make_parent_with_link(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    assert link_repo.branch_list().has_remote_branch("feature"), (
        "Expected branch create to cascade into the linked repository"
    )

    repo.branch_switch("main")
    repo.branch_archive("feature")

    assert sorted(repo.branch_list().remote_branches) == ["main"], (
        f"Expected only 'main' remaining in the parent, got: {repo.branch_list()}"
    )
    assert link_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the link branch to be left alone, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_include_links(new_lore_repo):
    """`--include-links` archives the branch in the linked repository too, so
    the link is left with exactly the branches it had before the create.
    """
    repo, link_repo = _make_parent_with_link(new_lore_repo)

    branches_before = sorted(link_repo.branch_list().remote_branches)

    repo.branch_create("feature")
    repo.push()
    assert link_repo.branch_list().has_remote_branch("feature"), (
        "Expected branch create to cascade into the linked repository"
    )

    repo.branch_switch("main")
    repo.branch_archive("feature", include_links=True)

    assert sorted(repo.branch_list().remote_branches) == ["main"], (
        f"Expected only 'main' remaining in the parent, got: {repo.branch_list()}"
    )
    assert sorted(link_repo.branch_list().remote_branches) == branches_before, (
        f"Expected link branches {branches_before}, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_multiple_links(new_lore_repo):
    """`--include-links` archives the branch in every configured link."""
    repo, second_repo, third_repo = _setup_repo_with_two_links(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    for link in (second_repo, third_repo):
        assert link.branch_list().has_remote_branch("feature"), (
            f"Expected branch create to cascade into {link.name}"
        )

    repo.branch_switch("main")
    repo.branch_archive("feature", include_links=True)

    for link in (second_repo, third_repo):
        assert sorted(link.branch_list().remote_branches) == ["main"], (
            f"Expected only 'main' remaining in {link.name}, got: {link.branch_list()}"
        )


@pytest.mark.smoke
def test_link_branch_archive_single_link(new_lore_repo):
    """`--link <path>` archives the branch in that link and no other."""
    repo, second_repo, third_repo = _setup_repo_with_two_links(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    repo.branch_archive("feature", link="sec")

    assert sorted(second_repo.branch_list().remote_branches) == ["main"], (
        f"Expected the scoped link to be archived, got: {second_repo.branch_list()}"
    )
    assert third_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the other link to be left alone, got: {third_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_unknown_link_errors(new_lore_repo):
    """`--link` naming a path that is not a link is an error, not a silent no-op."""
    repo, link_repo = _make_parent_with_link(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    output = repo.branch_archive("feature", link=_DEFAULT_PARENT_FILE, check=False)

    assert "not a link" in output.lower(), (
        f"Expected a non-link path to be reported, got: {output}"
    )
    assert link_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the link to be left alone, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_link_flags_conflict(new_lore_repo):
    """`--include-links` and `--link` are mutually exclusive."""
    repo, link_repo = _make_parent_with_link(new_lore_repo)

    output = repo.branch_archive(
        "feature", include_links=True, link=_DEFAULT_LINK_MOUNT, check=False
    )

    assert "cannot be used with" in output.lower(), (
        f"Expected clap to reject the flag combination, got: {output}"
    )


@pytest.mark.smoke
def test_link_branch_archive_local_keeps_link_remote(new_lore_repo):
    """`--local --include-links` archives the link's local cache only, leaving
    the link's remote branch in place.
    """
    repo, link_repo = _make_parent_with_link(new_lore_repo)

    repo.branch_create("feature")
    repo.push()

    repo.branch_switch("main")
    repo.branch_archive("feature", local=True, include_links=True)

    assert sorted(repo.branch_list().local_branches) == ["main"], (
        f"Expected only 'main' remaining locally, got: {repo.branch_list()}"
    )
    assert link_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the link remote branch to remain, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_tolerates_already_archived_link(new_lore_repo):
    """Archiving a branch a link already archived is not an error."""
    repo, link_repo = _make_parent_with_link(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    link_repo.branch_switch("main")
    link_repo.branch_archive("feature")

    repo.branch_archive("feature", include_links=True)

    assert sorted(repo.branch_list().remote_branches) == ["main"], (
        f"Expected only 'main' remaining in the parent, got: {repo.branch_list()}"
    )
    assert sorted(link_repo.branch_list().remote_branches) == ["main"], (
        f"Expected only 'main' remaining in the link, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_reports_once(new_lore_repo):
    """Archiving reports a single branch, not one line per link."""
    repo, second_repo, third_repo = _setup_repo_with_two_links(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    output = repo.branch_archive("feature", include_links=True)

    assert output.count("Archived branch") == 1, (
        f"Expected one archive line for the outer repository, got: {output}"
    )


@pytest.mark.smoke
def test_link_branch_archive_current_leaves_links(new_lore_repo):
    """Refusing to archive the current branch leaves the links alone."""
    repo, link_repo = _make_parent_with_link(new_lore_repo)

    repo.branch_create("feature")
    repo.push()

    repo.branch_archive("feature", include_links=True, check=False)

    assert link_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the link branch to be untouched, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_skips_auto_follow_disabled_link(new_lore_repo):
    """A link with auto-follow disabled never received the branch from the
    create cascade, so `--include-links` leaves its branches as they were.
    """
    repo, link_repo = _make_parent_with_link(new_lore_repo, disable_branching=True)

    branches_before = sorted(link_repo.branch_list().remote_branches)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    repo.branch_archive("feature", include_links=True)

    assert sorted(repo.branch_list().remote_branches) == ["main"], (
        f"Expected only 'main' remaining in the parent, got: {repo.branch_list()}"
    )
    assert sorted(link_repo.branch_list().remote_branches) == branches_before, (
        f"Expected link branches {branches_before}, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_single_auto_follow_disabled_link_errors(new_lore_repo):
    """Naming an auto-follow-disabled link with `--link` is refused, rather than
    deleting a branch the create cascade never put there.
    """
    repo, link_repo = _make_parent_with_link(new_lore_repo, disable_branching=True)

    branches_before = sorted(link_repo.branch_list().remote_branches)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    output = repo.branch_archive("feature", link=_DEFAULT_LINK_MOUNT, check=False)

    assert "does not follow the parent's branches" in output.lower(), (
        f"Expected the opted-out link to be reported, got: {output}"
    )
    assert sorted(link_repo.branch_list().remote_branches) == branches_before, (
        f"Expected link branches {branches_before}, got: {link_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_include_links_reaches_nested_link(new_lore_repo):
    """`--include-links` descends into a link nested inside another link.

    `branch create` only seeds the top level, so the nested repository holds no
    branch to remove; the cascade is observed reaching it through the debug log
    rather than through a branch that disappears.
    """
    repo_a, repo_b, repo_c, _b_mount, _nested_mount = _build_nested_link_repos(
        new_lore_repo
    )

    nested_before = sorted(repo_c.branch_list().remote_branches)

    repo_a.branch_create("feature")
    repo_a.push()
    assert repo_b.branch_list().has_remote_branch("feature"), (
        f"Expected create to cascade into the top-level link, got: {repo_b.branch_list()}"
    )

    repo_a.branch_switch("main")
    output = repo_a.branch_archive("feature", include_links=True, level="debug")

    assert repo_c.get_id() in output, (
        f"Expected the cascade to reach nested link {repo_c.get_id()}, got: {output}"
    )
    assert sorted(repo_c.branch_list().remote_branches) == nested_before, (
        f"Expected nested link branches {nested_before}, got: {repo_c.branch_list()}"
    )


@pytest.mark.smoke
def test_link_branch_archive_repository_linked_twice(new_lore_repo):
    """A repository mounted at two paths is archived once and reaches the
    branches it had before the create.
    """
    repo: Lore = new_lore_repo()
    link_repo: Lore = new_lore_repo(repo.name + "_link")

    repo.write_commit_push(None, {"main.txt": b"main content"})
    link_repo.write_commit_push(None, {"link.txt": b"link content"})

    repo.link_add("first", link_repo.get_id(), "/")
    repo.link_add("second", link_repo.get_id(), "/")
    repo.commit("add links")
    repo.push()

    branches_before = sorted(link_repo.branch_list().remote_branches)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    output = repo.branch_archive("feature", include_links=True)

    assert output.count("Archived branch") == 1, (
        f"Expected one archive line for the outer repository, got: {output}"
    )
    assert sorted(link_repo.branch_list().remote_branches) == branches_before, (
        f"Expected link branches {branches_before}, got: {link_repo.branch_list()}"
    )
