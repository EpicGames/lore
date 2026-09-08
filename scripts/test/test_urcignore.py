# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_urcignore_committed_file(new_lore_repo):
    """
    Verify that .loreignore only affects outbound (local) operations, not
    inbound (committed-state) operations.

    Scenario:
    1. Commit a file (data.bin) normally.
    2. Add a .loreignore rule that matches that file.
    3. Verify outbound commands (status, stage) skip the ignored file.
    4. Verify inbound commands (sync, branch switch, revision diff between
       two revisions, restore) still handle the ignored-but-committed file.
    """
    repo: Lore = new_lore_repo()

    # -- Setup: commit a file before any ignore rules exist --
    committed_file = "data.bin"
    other_file = "other.txt"

    with repo.open_file(committed_file, "w+b") as f:
        f.write(os.urandom(1024))
    with repo.open_file(other_file, "w+") as f:
        f.write("hello\n")

    repo.stage(scan=True)
    repo.commit("Initial commit with data.bin and other.txt")
    repo.push()

    # Modify the file so we have a second revision
    with repo.open_file(committed_file, "w+b") as f:
        f.write(os.urandom(2048))
    with repo.open_file(other_file, "w+") as f:
        f.write("updated\n")

    repo.stage(scan=True)
    repo.commit("Second commit - modify data.bin and other.txt")
    repo.push()

    # Record revisions
    history = repo.revision_history()
    rev2 = history[0].signature  # latest (second commit)
    rev1 = history[1].signature  # first commit

    # -- Add ignore file that matches the committed file --
    with repo.open_file(repo.ignore_file(), "w+") as f:
        f.write("data.bin\n")

    # Stage and commit the ignore file itself
    repo.stage(scan=True)
    repo.commit("Add ignore file excluding data.bin")
    repo.push()

    # -- Outbound: status should NOT show data.bin as unstaged/untracked --
    # Modify the ignored file on disk
    with repo.open_file(committed_file, "w+b") as f:
        f.write(os.urandom(512))

    output = repo.repository_status(unstaged=True)
    assert "data.bin" not in output, "status should not report ignored file data.bin"

    # -- Outbound: stage should skip the ignored file --
    repo.stage(scan=True)
    output = repo.repository_status()
    assert "data.bin" not in output, (
        "stage should not have staged the ignored file data.bin"
    )

    # Reset the local modification so it doesn't interfere with sync
    repo.file_reset()

    # -- Inbound: revision diff between two committed revisions should include data.bin --
    diff_output = repo.revision_diff(rev1, target=rev2)
    assert "data.bin" in diff_output, (
        "revision diff between two committed revisions should include "
        "ignored-but-committed file data.bin"
    )

    # -- Inbound: sync to an older revision should materialize data.bin --
    repo.sync(rev1)
    assert repo.file_exists(committed_file), (
        "sync to older revision should materialize ignored-but-committed file data.bin"
    )

    # -- Inbound: restore should include data.bin --
    # We're at rev1, restore fast-forwards to head including data.bin changes
    repo.revision_restore("Restore to latest")

    assert repo.file_exists(committed_file), (
        "restore should materialize ignored-but-committed file data.bin"
    )


@pytest.mark.smoke
def test_urcignore_merge_committed_file(new_lore_repo):
    """
    Verify that branch merge still processes ignored-but-committed files.

    Scenario:
    1. Commit data.bin on main.
    2. Create a feature branch, modify data.bin, push.
    3. On main, add .loreignore for data.bin, commit.
    4. Merge feature branch into main - data.bin changes should come through.
    """
    repo: Lore = new_lore_repo()

    committed_file = "data.bin"

    # Initial commit with data.bin
    with repo.open_file(committed_file, "w+b") as f:
        f.write(os.urandom(1024))

    repo.stage(scan=True)
    repo.commit("Initial commit with data.bin")
    repo.push()

    # Create feature branch and modify data.bin
    repo.branch_create("feature")
    repo.branch_switch("feature")

    with repo.open_file(committed_file, "w+b") as f:
        f.write(os.urandom(2048))

    repo.stage(scan=True)
    repo.commit("Feature: modify data.bin")
    repo.push()

    # Switch back to main, add ignore file
    repo.branch_switch("main")

    with repo.open_file(repo.ignore_file(), "w+") as f:
        f.write("data.bin\n")

    repo.stage(scan=True)
    repo.commit("Add ignore file excluding data.bin")
    repo.push()

    # Merge feature into main
    repo.branch_merge_start("feature")

    # data.bin should still exist after merge (committed state came through)
    assert repo.file_exists(committed_file), (
        "merge should bring in changes to ignored-but-committed file data.bin"
    )


@pytest.mark.smoke
def test_urcignore_merge_new_file_from_incoming_branch(new_lore_repo):
    """
    Verify that merge brings in a file in an ignored subdirectory committed
    only on the incoming branch.

    Scenario:
    1. On main, commit an initial file (no artifacts/, no .loreignore).
    2. Create a feature branch from that point.
    3. On main, add .loreignore for artifacts/, commit, push.
    4. Switch to feature branch, commit artifacts/data.bin (no .loreignore there), push.
    5. Switch back to main, merge feature branch.
    6. artifacts/data.bin should appear on main despite the ignore rule.
    """
    repo: Lore = new_lore_repo()

    committed_file = "artifacts/data.bin"

    # -- Main: initial commit without artifacts/ or ignore file --
    with repo.open_file("readme.txt", "w+") as f:
        f.write("initial\n")

    repo.stage(scan=True)
    repo.commit("Initial commit without artifacts/")
    repo.push()

    # -- Create feature branch before ignore file exists, then switch back to main --
    repo.branch_create("feature")
    repo.branch_switch("main")

    # -- Main: add ignore file that ignores the entire artifacts/ directory --
    with repo.open_file(repo.ignore_file(), "w+") as f:
        f.write("artifacts\n")

    repo.stage(scan=True)
    repo.commit("Add ignore file excluding artifacts/")
    repo.push()

    # -- Feature branch: commit artifacts/data.bin (no ignore file on this branch) --
    repo.branch_switch("feature")
    repo.make_dirs("artifacts/")

    with repo.open_file(committed_file, "w+b") as f:
        f.write(os.urandom(1024))

    repo.stage(scan=True)
    repo.commit("Feature: add artifacts/data.bin")
    repo.push()

    # -- Back to main: merge feature --
    repo.branch_switch("main")
    repo.branch_merge_start("feature", debug=True)

    # artifacts/data.bin should exist after merge even though main ignores artifacts/
    assert repo.file_exists(committed_file), (
        "merge should bring in artifacts/data.bin from incoming branch even when "
        "current branch has a .loreignore rule for the artifacts/ directory"
    )


@pytest.mark.smoke
def test_urcignore_branch_switch_preserves_ignored_file(new_lore_repo):
    """
    Verify that branch switch with reset does not delete files excluded by
    .loreignore that were never committed.

    The branch switch sync uses FilterMode::View which only checks the
    .lore/view filter, not .loreignore. Files matched by .loreignore that
    exist on the filesystem but are not in the revision tree get incorrectly
    identified as "extra" files and deleted during the reset diff reversal.

    Scenario:
    1. Create a repo with .loreignore excluding *.code-workspace.
    2. Create a .code-workspace file on disk (never committed).
    3. Commit other tracked files, push.
    4. Create a feature branch, add a new file, commit, push.
    5. Merge feature branch back into main, push.
    6. Switch to main with --reset.
    7. Assert the .code-workspace file still exists on disk.
    """
    repo: Lore = new_lore_repo()

    ignored_file = "project.code-workspace"

    # -- Setup: create ignore file before any commits --
    with repo.open_file(repo.ignore_file(), "w+") as f:
        f.write("*.code-workspace\n")

    # Create the ignored file on disk
    with repo.open_file(ignored_file, "w+") as f:
        f.write('{"folders": [{"path": "."}]}\n')

    # Create a tracked file
    with repo.open_file("readme.txt", "w+") as f:
        f.write("initial\n")

    repo.stage(scan=True)
    repo.commit("Initial commit with ignore file")
    repo.push()

    # Verify the ignored file is not tracked
    output = repo.repository_status(unstaged=True)
    assert "code-workspace" not in output, "ignored file should not appear in status"

    # -- Create feature branch and add a tracked file --
    repo.branch_create("feature")
    repo.branch_switch("feature")

    with repo.open_file("feature.txt", "w+") as f:
        f.write("feature work\n")

    repo.stage(scan=True)
    repo.commit("Feature: add feature.txt")
    repo.push()

    # -- Merge feature into main --
    repo.branch_switch("main")
    repo.branch_merge_start("feature")

    # -- Switch to main with reset --
    repo.branch_switch("main", reset=True, force=True)

    # The ignored file must survive the branch switch
    assert repo.file_exists(ignored_file), (
        "branch switch with --reset should not delete files excluded by "
        ".loreignore (FilterMode::View does not check .loreignore)"
    )


@pytest.mark.smoke
def test_urcignore_legacy_fallback(new_lore_repo):
    """A legacy `.urcignore` is honored as a fallback when no `.loreignore`
    exists, and `.loreignore` takes precedence when both are present.

    The repository uses the current `.lore/` format, whose primary ignore file
    is `.loreignore`. With only a legacy `.urcignore` on disk its rules must
    still apply to outbound operations. Once a `.loreignore` is added it becomes
    authoritative and the `.urcignore` is no longer consulted.
    """
    repo: Lore = new_lore_repo()

    # Only a legacy .urcignore present: its rule must be honored as a fallback.
    with repo.open_file(".urcignore", "w+") as f:
        f.write("legacy.tmp\n")

    with repo.open_file("legacy.tmp", "w+") as f:
        f.write("from urcignore\n")
    with repo.open_file("kept.txt", "w+") as f:
        f.write("tracked\n")

    output = repo.repository_status(unstaged=True)
    assert "legacy.tmp" not in output, (
        "legacy .urcignore should be honored as a fallback when no .loreignore exists"
    )
    assert "kept.txt" in output, "non-ignored file should still be reported"

    # Adding a .loreignore makes it authoritative: it no longer ignores
    # legacy.tmp (now reported), while its own rule excludes primary.tmp.
    with repo.open_file(".loreignore", "w+") as f:
        f.write("primary.tmp\n")

    with repo.open_file("primary.tmp", "w+") as f:
        f.write("from loreignore\n")

    output = repo.repository_status(unstaged=True)
    assert "primary.tmp" not in output, (
        ".loreignore should take precedence and exclude its own rules"
    )
    assert "legacy.tmp" in output, (
        ".urcignore must no longer be consulted once .loreignore exists"
    )
