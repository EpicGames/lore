# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import shutil
from pathlib import Path

import pytest
from lore_parsers import parse_jsonl, parse_status_json, parse_status_summary_json

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_clone_status_behind_remote_not_divergent(new_lore_repo):
    """
    Regression: a freshly-cloned repository whose remote subsequently advances
    must report "behind remote", not "diverged". The previous status logic
    inferred divergence from a stale LAST_SYNC mutable-store entry that clone
    never wrote, producing a false-positive on the very first status call.
    """
    repo: Lore = new_lore_repo()

    # Seed the source repo with one revision and push to remote.
    with repo.open_file("seed.txt", "w+") as f:
        f.write("seed\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Seed", offline=True)
    repo.push()

    # Two independent clones of the same remote.
    clone_a = repo.clone()
    clone_b = repo.clone()

    # Advance the remote main branch via clone_b.
    with clone_b.open_file("from_b.txt", "w+") as f:
        f.write("from clone_b\n")
    clone_b.stage(scan=True, offline=True)
    clone_b.commit("Advance from clone_b", offline=True)
    clone_b.push()

    # Status in clone_a should report remote-ahead/behind, NOT divergent.
    output = clone_a.status(json=True)
    revision_entries = parse_jsonl(output, "repositoryStatusRevision")
    assert len(revision_entries) == 1, (
        f"Expected exactly one repositoryStatusRevision entry, got {revision_entries}"
    )
    entry = revision_entries[0]

    assert entry["remoteAvailable"] == 1, f"Remote should be available: {entry}"
    assert entry["remoteBranchExist"] == 1, f"Remote branch should exist: {entry}"
    assert entry["revisionRemoteNumber"] > entry["revisionLocalNumber"], (
        f"Remote should be strictly ahead by revision number: {entry}"
    )
    assert entry["isRemoteAhead"] == 1, (
        f"Status should report remote ahead: {entry}"
    )
    assert entry["isLocalAhead"] == 0, (
        f"Status must NOT report local ahead — clone_a has no local commits "
        f"beyond what the remote has: {entry}"
    )


@pytest.mark.smoke
def test_clone_no_tracking_creates_local_branch(new_lore_repo):
    """
    Regression: `clone --no-tracking` (in-memory stores only) must succeed.
    Previously failed with `WriteRequired` because the NoStore context was
    forced to a None write_token by the parallel-commands refactor, so
    branch::create's internal mutable-store helpers refused to write the
    local branch metadata.
    """
    repo: Lore = new_lore_repo()

    # Seed the source repo with one revision and push to remote.
    with repo.open_file("seed.txt", "w+") as f:
        f.write("seed\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Seed", offline=True)
    repo.push()

    # Cloning with no_tracking must succeed without WriteRequired.
    cloned = repo.clone(no_tracking=True)

    # The clone command itself returning is the regression assertion; if it
    # fails, run() raises. We additionally sanity-check the cloned dir was
    # created on disk (the repository state itself is in-memory and dies
    # with the process, so we can't query it after the command returns).
    assert Path(cloned.path).exists(), (
        f"Clone destination should exist on disk: {cloned.path}"
    )


@pytest.mark.smoke
def test_clone_by_branch_id(new_lore_repo):
    """
    Verifies that `lore clone --branch <branchId>` succeeds when the value is
    a hex branch ID rather than a branch name, for a non-default branch. The
    resulting clone is on the requested branch with that branch's content.
    """
    repo: Lore = new_lore_repo()

    repo.write_commit_push("Initial commit", {"file.txt": ["main content\n"]})

    feature_branch = "feature-branch"
    repo.branch_create(feature_branch)
    repo.write_commit_push(
        "Feature commit", {"file.txt": ["main content\nfeature\n"]}
    )

    # Read the branch ID for the feature branch
    info_output = repo.branch_info(feature_branch, json=True)
    info_entries = parse_jsonl(info_output, "branchInfo")
    assert len(info_entries) == 1, (
        f"Expected one branchInfo entry, got {info_entries}"
    )
    branch_id = info_entries[0]["id"]

    # Clone by branch ID — must succeed and end up on the requested branch
    clone = repo.clone(branch=branch_id)

    # Verify the clone landed on the feature branch with the right content
    info_output = clone.branch_info(json=True)
    info_entries = parse_jsonl(info_output, "branchInfo")
    assert len(info_entries) == 1, (
        f"Expected one branchInfo entry from clone, got {info_entries}"
    )
    assert info_entries[0]["id"] == branch_id, (
        f"Clone should be on branch ID {branch_id}, got {info_entries[0]['id']}"
    )
    assert info_entries[0]["name"] == feature_branch, (
        f"Clone should be on branch '{feature_branch}', got '{info_entries[0]['name']}'"
    )

    with clone.open_file("file.txt") as f:
        assert f.read() == "main content\nfeature\n"


@pytest.mark.smoke
def test_clone_direct_file_write(new_lore_repo):
    """
    Verify that clone with --direct-file-write produces correct files on disk.

    --direct-file-write skips the default atomic write strategy (write to temp
    file then rename) and writes directly to the destination path. The result
    must be identical to a normal clone.
    """
    repo: Lore = new_lore_repo()

    repo.write_commit_push("Initial commit", {
        "text.txt": "hello world\n",
        "subdir/nested.txt": "nested content\n",
        "binary.bin": b"\x00\x01\x02\xff" * 256,
    })

    clone = repo.clone(direct_file_write=True)

    # Verify text files
    with clone.open_file("text.txt") as f:
        assert f.read() == "hello world\n", "text file content mismatch"

    with clone.open_file("subdir/nested.txt") as f:
        assert f.read() == "nested content\n", "nested file content mismatch"

    # Verify binary file
    with clone.open_file("binary.bin", "rb") as f:
        content = f.read()
    assert content == b"\x00\x01\x02\xff" * 256, (
        f"binary file content mismatch, got {len(content)} bytes"
    )


@pytest.mark.smoke
def test_reclone_over_existing_files_records_their_modified_times(new_lore_repo):
    """A clone into a directory that already holds identical files retains them rather than
    writing them, and establishes that they match by measuring their content. Recording the
    modified time it measured them at is what leaves the next scan nothing to measure.

    The re-clone mints a fresh instance, so none of the times the first clone recorded can
    be read back: whatever answers the scan afterwards must have been recorded by the
    retain path itself.
    """
    repo: Lore = new_lore_repo()

    file_count = 8
    for index in range(file_count):
        with repo.open_file(f"retained{index}.bin", "w+b") as f:
            f.write(os.urandom(4096))
    repo.stage(scan=True)
    repo.commit("Base")
    repo.push()

    clone: Lore = repo.clone()

    # Drop the repository metadata and keep the working files, so the re-clone finds every
    # file present and identical.
    shutil.rmtree(clone.dot_path(), ignore_errors=True)

    output = repo.run(["repository", "clone", repo.remote_path, clone.path], json=True)
    clone_end = parse_jsonl(output, "repositoryCloneEnd")
    assert clone_end, "clone must emit a repositoryCloneEnd event"
    count = clone_end[-1]["count"]
    assert count["fileRetain"] == file_count, (
        f"every existing file should have been retained, got {count}"
    )
    assert count["fileReplace"] == 0, (
        f"no existing file should have been rewritten, got {count}"
    )

    # One scan only: a second would record whatever the first measured, and so would report
    # nothing left to measure even if the retain path had recorded nothing.
    scan = clone.status(scan=True, json=True, offline=True)
    assert not parse_status_json(scan), (
        "a re-clone over identical files leaves nothing changed"
    )

    summary = parse_status_summary_json(scan)
    assert summary is not None, "scan must emit a repositoryStatusSummary event"
    assert summary["hashChecks"] == 0, (
        f"the retained files must be answered by their recorded times, got {summary}"
    )
    assert summary["mtimeMatches"] == file_count, summary
