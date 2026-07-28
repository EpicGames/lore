# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging

import pytest

from error_types import ImproperArgumentsError
from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_revision_metadata_get_default_keys(new_lore_repo):
    """
    Verify that auto-set metadata keys are present after a commit.
    """
    repo: Lore = new_lore_repo()

    repo.write_commit_push("Initial commit", {"file.txt": "content\n"})

    metadata = repo.revision_metadata_get()
    # The "list all" output uses display labels: "Branch", "Date", and an
    # indented commit message.
    assert "Branch" in metadata, (
        f"Expected 'Branch' label in metadata output.\nGot:\n{metadata}"
    )
    assert "Date" in metadata, (
        f"Expected 'Date' label in metadata output.\nGot:\n{metadata}"
    )
    assert "Initial commit" in metadata, (
        f"Expected commit message in metadata output.\nGot:\n{metadata}"
    )


@pytest.mark.smoke
def test_revision_metadata_get_specific_key(new_lore_repo):
    """
    Verify fetching individual metadata keys returns correct values.
    """
    repo: Lore = new_lore_repo()

    commit_message = "Specific key test commit"
    branch_name = "specific-key-branch"
    repo.write_commit_push("Initial commit", {"file.txt": "content\n"})
    repo.branch_create(branch_name)
    repo.write_commit_push(commit_message, {"file.txt": "content modification\n"})

    timestamp = repo.revision_metadata_get("timestamp")
    assert timestamp.strip(), "timestamp should be non-empty"

    message = repo.revision_metadata_get("message")
    assert commit_message in message, (
        f"Expected commit message in metadata.\nExpected: {commit_message}\nGot: {message}"
    )

    # Branch metadata stores the branch context ID, not the name
    branch_info = repo.branch_info(branch_name)
    expected_branch_id = branch_info.id
    branch = repo.revision_metadata_get("branch")
    assert expected_branch_id in branch.strip(), (
        f"Expected branch ID '{expected_branch_id}' in branch metadata.\nGot: {branch}"
    )


@pytest.mark.smoke
def test_revision_metadata_get_by_hash(new_lore_repo):
    """
    Exercise --revision with a hash signature to fetch metadata from
    specific revisions.
    """
    repo: Lore = new_lore_repo()

    # First commit
    first_message = "First commit message"
    repo.write_commit_push(first_message, {"file.txt": "first\n"})

    # Second commit
    second_message = "Second commit message"
    repo.write_commit_push(second_message, {"file.txt": "second\n"})

    revisions = repo.history()
    assert len(revisions) >= 2, f"Expected at least 2 revisions, got {len(revisions)}"

    # history() returns oldest-first, so [0] is first commit, [-1] is newest
    first_metadata = repo.revision_metadata_get(
        "message", revision=revisions[0].signature
    )
    assert first_message in first_metadata, (
        f"Expected first commit message via hash lookup.\n"
        f"Expected: {first_message}\nGot: {first_metadata}"
    )

    second_metadata = repo.revision_metadata_get(
        "message", revision=revisions[1].signature
    )
    assert second_message in second_metadata, (
        f"Expected second commit message via hash lookup.\n"
        f"Expected: {second_message}\nGot: {second_metadata}"
    )


@pytest.mark.smoke
def test_revision_metadata_get_by_branch_at_revision(new_lore_repo):
    """
    Exercise --revision with <branch>@<number> notation.
    """
    repo: Lore = new_lore_repo()

    # First commit
    first_message = "Branch-at first"
    repo.write_commit_push(first_message, {"file.txt": "first\n"})

    # Second commit
    second_message = "Branch-at second"
    repo.write_commit_push(second_message, {"file.txt": "second\n"})

    first_metadata = repo.revision_metadata_get("message", revision="main@1")
    assert first_message in first_metadata, (
        f"Expected first commit message via main@1.\n"
        f"Expected: {first_message}\nGot: {first_metadata}"
    )

    second_metadata = repo.revision_metadata_get("message", revision="main@2")
    assert second_message in second_metadata, (
        f"Expected second commit message via main@2.\n"
        f"Expected: {second_message}\nGot: {second_metadata}"
    )


@pytest.mark.smoke
def test_revision_metadata_set_and_get(new_lore_repo):
    """
    Verify user-set metadata roundtrips through commit and push.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w") as f:
        f.write("content\n")
    repo.stage("file.txt")
    repo.revision_metadata_set(["reviewed-by", "tester@example.com"])
    repo.commit("Commit with custom metadata")
    repo.push()

    reviewed_by = repo.revision_metadata_get("reviewed-by")
    assert "tester@example.com" in reviewed_by, (
        f"Expected 'tester@example.com' in reviewed-by metadata.\nGot: {reviewed_by}"
    )


@pytest.mark.smoke
def test_revision_metadata_get_across_branches(new_lore_repo):
    """
    Verify --revision with branch notation works across different branches.
    """
    repo: Lore = new_lore_repo()

    # Initial commit on main
    repo.write_commit_push("Initial commit", {"main.txt": "main content\n"})

    # Create feature branch with its own commit
    feature_message = "Feature branch commit"
    repo.branch_create("feature")
    repo.write_commit_push(feature_message, {"feature.txt": "feature content\n"})

    # Switch back to main and make another commit
    repo.branch_switch("main")
    main_second_message = "Main second commit"
    repo.write_commit_push(main_second_message, {"main.txt": "updated main content\n"})

    # Verify feature branch metadata via branch@LATEST
    feature_metadata = repo.revision_metadata_get("message", revision="feature@LATEST")
    assert feature_message in feature_metadata, (
        f"Expected feature commit message via feature@LATEST.\n"
        f"Expected: {feature_message}\nGot: {feature_metadata}"
    )

    # Verify main@1 (no key) returns the initial (non-latest) commit
    main_first_metadata = repo.revision_metadata_get(revision="main@1")
    assert "Initial commit" in main_first_metadata, (
        f"Expected initial commit message via main@1.\n"
        f"Expected: 'Initial commit'\nGot: {main_first_metadata}"
    )
    assert main_second_message not in main_first_metadata, (
        f"main@1 should not contain the second commit message.\nGot: {main_first_metadata}"
    )

    # Verify main branch metadata via main@LATEST
    main_metadata = repo.revision_metadata_get("message", revision="main@LATEST")
    assert main_second_message in main_metadata, (
        f"Expected main second commit message via main@LATEST.\n"
        f"Expected: {main_second_message}\nGot: {main_metadata}"
    )


@pytest.mark.smoke
def test_revision_metadata_set_single_arg_rejected(new_lore_repo):
    """A lone argument has no value; the set must be rejected, not panic."""
    repo: Lore = new_lore_repo()

    with pytest.raises(ImproperArgumentsError):
        repo.revision_metadata_set(["lonely-key"])


@pytest.mark.smoke
def test_revision_metadata_set_odd_args_rejected(new_lore_repo):
    """An odd number of arguments leaves the trailing key without a value and
    must be rejected rather than dropping the key or panicking."""
    repo: Lore = new_lore_repo()

    with pytest.raises(ImproperArgumentsError):
        repo.revision_metadata_set(["key1", "value1", "key2"])
