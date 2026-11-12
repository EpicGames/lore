# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""A push run again after a push killed once the remote accepted its revision, while the branch's
local latest is behind the revision pushed, as a commit or a sync killed before its latest reached
disk leaves it. A second instance on the same shared store puts the branch's latest back, and the
instance that pushed still stands at the revision it pushed."""

import pytest
from lore_parsers import parse_jsonl

from lore import Lore


def _instances_on_one_store(new_lore_repo, scratch_dir) -> tuple[Lore, Lore, str]:
    """Two instances on one shared store, both at a first pushed revision, answered with it."""
    store = str(scratch_dir("store"))
    repo: Lore = new_lore_repo(create_repo=False)
    repo.repository_create(use_shared_store=True, shared_store_path=store)
    repo.write_commit_push("First", {"a.txt": "first"})
    first = repo.branch_info("main").local_latest
    other = repo.clone(use_shared_store=True, shared_store_path=store)
    return repo, other, first


def _ahead(repo: Lore) -> tuple[int, int]:
    """Whether the branch is ahead of the remote, and whether the remote is ahead of it."""
    status = parse_jsonl(repo.status(json=True), "repositoryStatusRevision")[0]
    return status["isLocalAhead"], status["isRemoteAhead"]


@pytest.mark.own_vfs("puts a branch latest back through a second instance on one shared store")
@pytest.mark.smoke
def test_push_records_the_revision_the_remote_holds(new_lore_repo, scratch_dir):
    """The push finds the revision already pushed and records it as the branch's latest."""
    repo, other, first = _instances_on_one_store(new_lore_repo, scratch_dir)
    repo.write_commit_push("Second", {"a.txt": "second"})
    second = repo.branch_info("main").local_latest
    other.branch_reset(first)
    assert _ahead(repo) != (0, 0)

    repo.push()

    assert repo.branch_info("main").local_latest == second
    assert _ahead(repo) == (0, 0)
