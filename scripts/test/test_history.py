# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import time

import pytest

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_history(new_lore_repo):
    repo: Lore = new_lore_repo()

    # Generate a file
    text_file = "text.txt"

    with repo.open_file(text_file, "w+") as output_file:
        output_file.writelines(["One line\n", "Another line\n", "Third line\n"])

    # Stage and commit the file
    repo.stage(scan=True)
    repo.commit(local=True)

    # Add some more revisions
    for i in range(5):
        with repo.open_file(text_file, "a+") as output_file:
            output_file.writelines([f"Adding line {i + 4}\n"])
        repo.stage(scan=True)
        repo.commit(f"Test commit {i + 2}", local=True)

    # Push changes
    repo.push()
    # Clone repository
    clone = repo.clone()

    # Add some more revisions
    for i in range(5):
        with clone.open_file(text_file, "a+") as output_file:
            output_file.writelines([f"Adding line {i + 9}\n"])
        clone.stage(scan=True)
        clone.commit(f"Test commit {i + 7}", local=True)

    clone.sync("main@4")
    assert clone.revision_info().revision == "4"
    assert clone.history(1)[0].revision == "4"
    assert clone.history(1, "main@head", remote=True)[0].revision == "6"
    assert clone.history(1, "main@head", local=True)[0].revision == "11"
    assert clone.history(1, "main@head")[0].revision == "11"


@pytest.mark.smoke
def test_file_history_oneline(new_lore_repo):
    repo: Lore = new_lore_repo()

    text_file = "text.txt"

    # Create the file and commit with a message
    with repo.open_file(text_file, "w+") as f:
        f.write("Initial content\n")
    repo.stage(scan=True)
    repo.commit("First commit", local=True)

    # Modify and commit a few more times
    for i in range(3):
        with repo.open_file(text_file, "a+") as f:
            f.write(f"Line {i + 2}\n")
        repo.stage(scan=True)
        repo.commit(f"Commit number {i + 2}", local=True)

    repo.push()

    # Get file history in oneline mode
    output = repo.file_history(text_file, oneline=True)
    lines = [line for line in output.strip().splitlines() if line.strip()]

    # Should have 4 entries (one per commit that touched the file)
    assert len(lines) == 4, f"Expected 4 oneline entries, got {len(lines)}: {lines}"

    # Each line should match the format: "{revision_number} {message}"
    for line in lines:
        parts = line.split(maxsplit=1)
        assert len(parts) == 2, f"Expected 'revision message' format, got: {line}"
        revision_number, message = parts
        assert revision_number.isdigit(), f"Revision should be numeric, got: {revision_number}"
        assert len(message) > 0, f"Message should not be empty for revision {revision_number}"

    # Verify the messages match what we committed (newest first)
    parts = [line.split(maxsplit=1) for line in lines]
    messages = [p[1] for p in parts]
    assert messages[0] == "Commit number 4"
    assert messages[1] == "Commit number 3"
    assert messages[2] == "Commit number 2"
    assert messages[3] == "First commit"


@pytest.mark.smoke
def test_history_only_branch(new_lore_repo):
    repo: Lore = new_lore_repo()

    text_file = "text.txt"

    # Create initial commits on main
    for i in range(3):
        with repo.open_file(text_file, "w+") as f:
            f.write(f"Main content {i}\n")
        repo.stage(scan=True)
        repo.commit(f"Main commit {i + 1}", local=True)

    repo.push()

    main_history = repo.history(branch="main")
    main_head_signature = main_history[-1].signature

    # Create a feature branch and add commits
    repo.branch_create("feature")
    repo.branch_switch("feature")

    for i in range(4):
        with repo.open_file(text_file, "w+") as f:
            f.write(f"Feature content {i}\n")
        repo.stage(scan=True)
        repo.commit(f"Feature commit {i + 1}", local=True)

    repo.push()

    feature_history = repo.history()
    feature_branch = feature_history[-1].branch

    # --- Case 1: No starting revision (uses current anchor branch) ---
    branch_history = repo.history(only_branch=True)
    assert len(branch_history) == 5, (
        f"Case 1: Expected 5 revisions (4 feature + 1 branch point), got {len(branch_history)}"
    )
    assert branch_history[0].signature == main_head_signature, (
        "Case 1: First entry (branch point) should match main head"
    )
    for entry in branch_history[1:]:
        assert entry.branch == feature_branch, (
            f"Case 1: Expected feature branch, got {entry.branch}"
        )

    # --- Case 2: --branch option ---
    branch_history = repo.history(branch="feature", only_branch=True)
    assert len(branch_history) == 5, (
        f"Case 2: Expected 5 revisions, got {len(branch_history)}"
    )
    assert branch_history[0].signature == main_head_signature, (
        "Case 2: First entry (branch point) should match main head"
    )

    # --- Case 3: branch@revnr specifier from latest ---
    # Revision numbers continue from main (main has 1-3, feature has 4-7)
    branch_history = repo.revision_history(revision="feature@7", only_branch=True)
    assert len(branch_history) == 5, (
        f"Case 3: Expected 5 revisions (4 feature + 1 branch point), got {len(branch_history)}"
    )
    assert branch_history[0].signature == main_head_signature, (
        "Case 3: First entry (branch point) should match main head"
    )

    # --- Case 4: branch@revnr starting mid-branch ---
    branch_history = repo.revision_history(revision="feature@5", only_branch=True)
    # feature@5 is the second feature commit, walk: rev5 -> rev4 -> branch point
    assert len(branch_history) == 3, (
        f"Case 4: Expected 3 revisions, got {len(branch_history)}"
    )
    assert branch_history[0].signature == main_head_signature, (
        "Case 4: First entry (branch point) should match main head"
    )

    # --- Case 5: Raw hash signature (branch inferred from first revision metadata) ---
    # Use the signature of the latest feature commit
    latest_sig = feature_history[-1].signature
    branch_history = repo.revision_history(revision=latest_sig, only_branch=True)
    assert len(branch_history) == 5, (
        f"Case 5: Expected 5 revisions, got {len(branch_history)}"
    )
    assert branch_history[0].signature == main_head_signature, (
        "Case 5: First entry (branch point) should match main head"
    )

    # --- Case 6: Empty branch (no commits, anchor at branch point) ---
    repo.branch_create("empty-branch")
    repo.branch_switch("empty-branch")

    branch_history = repo.history(only_branch=True)
    assert len(branch_history) == 1, (
        f"Case 6: Expected 1 revision (branch point only), got {len(branch_history)}"
    )
    assert branch_history[0].signature == feature_history[-1].signature, (
        "Case 6: Single entry should be the branch point (feature head)"
    )

    # --- Case 7: --branch on empty branch ---
    branch_history = repo.history(branch="empty-branch", only_branch=True)
    assert len(branch_history) == 1, (
        f"Case 7: Expected 1 revision (branch point only), got {len(branch_history)}"
    )

    # --- Case 8: branch@revnr targeting the branch point revision ---
    # feature@3 is the branch point (main rev 3), which is on main, not feature.
    # Should return just that one revision.
    repo.branch_switch("feature")
    branch_history = repo.revision_history(revision="feature@3", only_branch=True)
    assert len(branch_history) == 1, (
        f"Case 8: Expected 1 revision (branch point only), got {len(branch_history)}"
    )
    assert branch_history[0].signature == main_head_signature, (
        "Case 8: Single entry should be the branch point (main head)"
    )


@pytest.mark.smoke
def test_history_branch_remote_fallback(new_lore_repo):
    repo: Lore = new_lore_repo()

    text_file = "text.txt"

    # A couple of revisions on main so the branch has a branch point
    for i in range(2):
        with repo.open_file(text_file, "w+") as f:
            f.write(f"Main content {i}\n")
        repo.stage(scan=True)
        repo.commit(f"Main commit {i + 1}", local=True)

    repo.push()

    # A branch that the clone below will only ever know from the remote
    repo.branch_create("remote-only")
    repo.branch_switch("remote-only")

    for i in range(3):
        with repo.open_file(text_file, "w+") as f:
            f.write(f"Remote content {i}\n")
        repo.stage(scan=True)
        repo.commit(f"Remote commit {i + 1}", local=True)

    repo.push()

    remote_head = repo.history(branch="remote-only")[-1].signature

    # The clone only syncs main, so it has no local history for "remote-only"
    clone = repo.clone()

    # --local pins the lookup to the local branch, which does not exist here
    assert clone.history(branch="remote-only", local=True) == [], (
        "--local should not fall back to the remote branch"
    )

    # Without a location flag the missing local branch falls back to the remote
    fallback = clone.history(branch="remote-only")
    assert fallback, "expected remote history for a branch missing locally"
    assert fallback[-1].signature == remote_head
    assert clone.history(branch="remote-only", remote=True)[-1].signature == remote_head

    # `history --branch <name>` matches `branch switch <name>; history`
    clone.branch_switch("remote-only")
    assert [entry.signature for entry in clone.history()] == [
        entry.signature for entry in fallback
    ]

    # Now that the branch exists locally the local tip wins over the remote tip
    with clone.open_file(text_file, "w+") as f:
        f.write("Local only content\n")
    clone.stage(scan=True)
    clone.commit("Local only commit", local=True)

    local_head = clone.history(branch="remote-only")[-1].signature
    assert local_head != remote_head, "expected the unpushed local revision"
    assert clone.history(branch="remote-only", remote=True)[-1].signature == remote_head
    assert clone.history(branch="remote-only", local=True)[-1].signature == local_head


@pytest.mark.smoke
def test_history_date_filter(new_lore_repo):
    repo: Lore = new_lore_repo()

    text_file = "text.txt"

    # Make an initial batch of commits
    for i in range(3):
        with repo.open_file(text_file, "w+") as f:
            f.write(f"Content {i}\n")
        repo.stage(scan=True)
        repo.commit(f"Early commit {i + 1}", local=True)

    repo.push()

    # Record a timestamp after the first batch — all subsequent commits will be newer
    mid_ts = time.time_ns() // 1_000_000

    # Make a second batch of commits
    for i in range(2):
        with repo.open_file(text_file, "w+") as f:
            f.write(f"Later content {i}\n")
        repo.stage(scan=True)
        repo.commit(f"Late commit {i + 1}", local=True)

    repo.push()

    # Without a date filter all 5 revisions are returned
    all_history = repo.history()
    assert len(all_history) == 5, f"Expected 5 revisions, got {len(all_history)}"

    # With the mid-point filter only the 2 later commits should be returned
    filtered_history = repo.history(date=mid_ts)
    assert len(filtered_history) == 2, (
        f"Expected 2 revisions after date filter, got {len(filtered_history)}"
    )
    assert filtered_history[0].message == "Late commit 1"
    assert filtered_history[1].message == "Late commit 2"

    # A far-future timestamp should return nothing
    future_ts = (time.time_ns() // 1_000_000) + 99_999_999
    assert repo.history(date=future_ts) == []
