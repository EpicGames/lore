# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import re
import shutil

import pytest
from error_types import (
    LocalChanges,
    NestedLinkError,
    PathExistChildrenLinkError,
    PathExistLinkError,
)
from link_helpers import DEFAULT_LINK_MOUNT, make_parent_with_link
from lore_parsers import parse_jsonl, parse_status_json
from test_utils import unstaged_entries

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
    assert repo.compare_file(repo, main_update_path), (
        "Main update file should be present"
    )
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

    unstaged_entries_after_add = unstaged_entries(main_repo)
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

    unstaged_entries_after_remove = unstaged_entries(main_repo)

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
    parent, link_repo = make_parent_with_link(new_lore_repo)

    parent.move(DEFAULT_LINK_MOUNT, "vendor/renamed")
    output = parent.stage_move(DEFAULT_LINK_MOUNT, "vendor/renamed", check=False)

    assert "[Error]" in output, (
        f"Renaming a link mount should not succeed, got: {output}"
    )

    # The link is intact and still registered at its original path.
    links = parent.link_list()
    assert link_repo.get_id() in links, (
        f"Link should survive a refused stage move: {links}"
    )
    assert (
        DEFAULT_LINK_MOUNT in links or DEFAULT_LINK_MOUNT.replace("/", "\\") in links
    ), f"Link path should be unchanged after a refused stage move: {links}"
    assert "vendor/renamed" not in links and "vendor\\renamed" not in links, (
        f"The rename should not be recorded against the link: {links}"
    )


@pytest.mark.smoke
def test_link_add_accepts_a_scoped_bare_name(new_lore_repo, lore_remote_url):
    """A scoped name such as `org/project` is a name, not a host and a path.

    `is_valid_name` permits slash-separated names, so a slash cannot be what tells a URL
    from a bare identifier — only a scheme can. `link add org/project` therefore has to
    resolve the whole argument against this repository's configured remote. Reading the
    first segment as a host instead sent the lookup to `lores://org`, a server that was
    never configured and, in this test, does not exist.
    """
    scoped_name = f"scoped/{Lore.generate_random_name('')}"
    link_repo: Lore = new_lore_repo(
        remote_path=f"{lore_remote_url.rstrip('/')}/{scoped_name}"
    )

    linked_file = "linked-content.txt"
    with link_repo.open_file(linked_file, "w+") as output_file:
        output_file.writelines(["content behind a scoped name\n"])
    link_repo.stage(scan=True)
    link_repo.commit("Seed the scoped repository")
    link_repo.push()

    repo: Lore = new_lore_repo()
    # The bare, schemeless, slash-carrying identifier is the point of the test.
    repo.link_add("vendor", scoped_name, "/")

    assert link_repo.get_id() in repo.link_list(), (
        f"link add should have resolved {scoped_name!r} against this repository's remote"
    )


@pytest.mark.smoke
def test_link_update_of_a_subtree_reports_paths_at_the_mount(new_lore_repo):
    """A link exposing a subtree reports its changes at the mount it is materialized at.

    The exposed subtree and the mount share no prefix, so a path spelled from the linked
    repository's own root names nothing in the working tree.
    """
    repo: Lore = new_lore_repo()
    source_repo = new_lore_repo()

    source_dir = "content/assets"
    kept_file = f"{source_dir}/rock.mesh"
    added_file = f"{source_dir}/tree.mesh"
    unexposed_file = "docs/readme.md"

    source_repo.make_dirs(source_dir)
    source_repo.make_dirs("docs")
    with source_repo.open_file(kept_file, "w+") as output_file:
        output_file.writelines(["rock\n"])
    with source_repo.open_file(unexposed_file, "w+") as output_file:
        output_file.writelines(["readme\n"])
    source_repo.stage(scan=True)
    source_repo.commit("Seed the exposed subtree")
    source_repo.push()
    pinned = source_repo.branch_info().local_latest

    with source_repo.open_file(added_file, "w+") as output_file:
        output_file.writelines(["tree\n"])
    source_repo.stage(scan=True)
    source_repo.commit("Add a mesh to the exposed subtree")
    source_repo.push()
    updated = source_repo.branch_info().local_latest

    mount = "linked/meshes"
    repo.link_add(mount, source_repo.get_id(), source_dir, pin=pinned)
    repo.commit("Add the link")
    repo.push()
    before_update = repo.branch_info().local_latest

    assert repo.compare_file(source_repo, f"{mount}/rock.mesh", kept_file), (
        "the exposed subtree's file belongs at the mount"
    )

    output = repo.link_update(mount, pin=updated, json=True)
    realized = [entry["path"] for entry in parse_jsonl(output, "revisionSyncFile")]

    assert realized, "updating the pin should report the files it realized"
    assert f"{mount}/tree.mesh" in realized, (
        f"the added file should be reported at the mount, got {realized}"
    )
    for path in realized:
        assert path.startswith(f"{mount}/"), (
            f"every reported path belongs under the mount, got {path!r}"
        )
        assert source_dir not in path, (
            f"no reported path carries the linked repository's own spelling, got {path!r}"
        )

    staged = [
        entry.get("path", "") for entry in parse_status_json(repo.status(json=True))
    ]
    assert mount in staged, f"the mount should be staged after the update, got {staged}"
    for path in staged:
        assert source_dir not in path, (
            f"no staged path carries the linked repository's own spelling, got {path!r}"
        )

    repo.commit("Update the link pin")
    repo.push()

    # The diff of the two revisions crosses the mount, so every path it reports is spelled from
    # the working tree root rather than from the linked repository's own.
    diffed = [
        entry["path"]
        for entry in parse_jsonl(
            repo.revision_diff(before_update, json=True),
            "revisionDiffFile",
        )
    ]
    assert diffed, "diffing across the pin change should report the files that differ"
    for path in diffed:
        assert source_dir not in path, (
            f"no diffed path carries the linked repository's own spelling, got {path!r}"
        )
    assert f"{mount}/tree.mesh" in diffed, (
        f"the added file should be diffed at the mount, got {diffed}"
    )

    assert repo.compare_file(source_repo, f"{mount}/tree.mesh", added_file), (
        "the added file belongs at the mount"
    )
    assert not repo.path_exists(f"{mount}/{source_dir}"), (
        "the linked repository's own spelling should reach no path on disk"
    )
    assert not repo.path_exists(f"{mount}/docs"), (
        "a path outside the exposed subtree should reach no path on disk"
    )

    # A file changed on disk inside the mount is reached by the walk of the working tree rather
    # than by the diff of two revisions, and is reported at the mount just the same.
    with repo.open_file(f"{mount}/rock.mesh", "w+") as output_file:
        output_file.writelines(["rock, modified\n"])

    scanned = [
        entry.get("path", "")
        for entry in parse_status_json(repo.status(json=True, scan=True))
    ]
    assert f"{mount}/rock.mesh" in scanned, (
        f"a file changed inside the mount is scanned at the mount, got {scanned}"
    )
    for path in scanned:
        assert source_dir not in path, (
            f"no scanned path carries the linked repository's own spelling, got {path!r}"
        )

    staged_inside = [
        entry["path"]
        for entry in parse_jsonl(
            repo.stage(f"{mount}/rock.mesh", json=True), "fileStageFile"
        )
    ]
    assert f"{mount}/rock.mesh" in staged_inside, (
        f"staging inside the mount reports at the mount, got {staged_inside}"
    )
    for path in staged_inside:
        assert source_dir not in path, (
            f"no staged path carries the linked repository's own spelling, got {path!r}"
        )


@pytest.mark.smoke
def test_link_move_realizes_at_the_mount(new_lore_repo):
    """A file moved inside a linked repository is realized at the mount, old path and all.

    Advancing the pin diffs the two linked revisions, which report the move as a delete and an add
    that are coalesced by the identity the two share. Both halves have to reach the mount: the new
    path materialized there and the old one gone.
    """
    link_path = "vendor/b"
    moved_from = f"{link_path}/dir/f1.txt"
    moved_to = f"{link_path}/dir/f2.txt"
    parent_repo, link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"dir/f1.txt": "content\n"}
    )

    link_repo.move("dir/f1.txt", "dir/f2.txt")
    link_repo.file_stage_move("dir/f1.txt", "dir/f2.txt")
    link_repo.commit("Move a file")
    link_repo.push()

    parent_repo.link_update(link_path)

    assert parent_repo.file_exists(moved_to), (
        f"the move should land at the mount.\nStatus:\n{parent_repo.status()}"
    )
    assert not parent_repo.file_exists(moved_from), (
        f"the path moved from should not survive at the mount.\nStatus:\n{parent_repo.status()}"
    )


@pytest.mark.smoke
def test_link_stage_move_inside_a_link_is_refused(new_lore_repo):
    """Staging a move of a path inside a link is refused rather than acted on.

    Resolving the path crosses the link, so the node it answers with is numbered by the linked
    repository's state and names nothing in the parent's. The move reads and relinks it in the
    parent, so it has to stop at the boundary, as the destination side already does.
    """
    link_path = "vendor/b"
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"dir/f1.txt": "content\n"}
    )

    moved_from = f"{link_path}/dir/f1.txt"
    moved_to = f"{link_path}/dir/f2.txt"
    parent_repo.move(moved_from, moved_to)

    output = parent_repo.stage_move(moved_from, moved_to, check=False)
    assert "Links not yet implemented" in output, (
        f"A move across a link boundary should be refused, got: {output}"
    )
