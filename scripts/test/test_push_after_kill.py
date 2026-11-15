# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""A push run again after a push killed once the remote accepted its revision, while the branch's
local latest is behind the revision pushed, as a commit or a sync killed before its latest reached
disk leaves it. A second instance on the same shared store puts the branch's latest back, and the
instance that pushed still stands at the revision it pushed."""

import pytest
from error_types import BranchDivergedError
from lore_parsers import parse_jsonl

from lore import Lore

MOVED_FORWARD = "Revision is already pushed and remote has moved forward"


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


@pytest.mark.own_vfs("puts a branch latest back through a second instance on one shared store")
@pytest.mark.smoke
def test_push_after_the_remote_moved_on_records_the_revision(new_lore_repo, scratch_dir):
    """With the remote moved past the revision pushed, the push sends nothing, records the
    revision as the branch's latest and leaves the branch behind the remote. The CLI reports the
    remote moved forward once the push has succeeded, which is the output under test."""
    repo, other, first = _instances_on_one_store(new_lore_repo, scratch_dir)
    repo.write_commit_push("Second", {"a.txt": "second"})
    second = repo.branch_info("main").local_latest
    third = repo.clone()
    third.write_commit_push("Third", {"a.txt": "third"})
    other.branch_reset(first)
    assert _ahead(repo) == (1, 1)

    output = repo.push()

    assert MOVED_FORWARD in output, output
    assert repo.branch_info("main").local_latest == second
    assert _ahead(repo) == (0, 1)


@pytest.mark.smoke
def test_push_of_a_revision_the_remote_moved_past_diverges(new_lore_repo):
    """An instance whose branch latest is its current revision, left behind by another pusher,
    fails to push as diverged, and the CLI does not report the remote moved forward."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("First", {"a.txt": "first"})
    other = repo.clone()
    other.write_commit_push("Second", {"a.txt": "second"})

    with pytest.raises(BranchDivergedError) as error:
        repo.push()

    assert MOVED_FORWARD not in str(error.value), error.value


@pytest.mark.own_vfs("puts a branch latest back through a second instance on one shared store")
@pytest.mark.smoke
def test_push_to_a_branch_deleted_on_the_remote_diverges(new_lore_repo, scratch_dir):
    """With the branch deleted on the remote after another instance moved it on, the push keeps
    the path that restores the branch, which diverges, rather than recording the revision."""
    repo, other, _first = _instances_on_one_store(new_lore_repo, scratch_dir)
    repo.branch_create("feature")
    repo.write_commit_push("Feature", {"a.txt": "feature"})
    feature = repo.branch_info("feature").local_latest
    other.branch_switch("feature")
    repo.write_commit_push("More", {"a.txt": "more"})
    third = repo.clone()
    third.branch_switch("feature")
    third.write_commit_push("Third", {"a.txt": "third"})
    third.branch_switch("main")
    third.branch_archive("feature")
    other.branch_reset(feature)

    with pytest.raises(BranchDivergedError):
        repo.push()


@pytest.mark.smoke
def test_push_of_a_new_branch_the_remote_moved_past_records_it(new_lore_repo):
    """A branch pushed with nothing of its own, then moved on by another instance, is recorded
    at its branch point and left behind the remote, as a push killed once the remote accepted it
    would be: the remote holds that revision on the branch's history."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("First", {"a.txt": "first"})
    repo.branch_create("feature")
    repo.push()
    other = repo.clone()
    other.branch_switch("feature")
    other.write_commit_push("Second", {"a.txt": "second"})

    output = repo.push()

    assert MOVED_FORWARD in output, output
    assert _ahead(repo) == (0, 1)


@pytest.mark.smoke
def test_push_of_a_branch_reset_to_its_branch_point_diverges(new_lore_repo):
    """A branch reset to its branch point past a local commit, with the remote moved past the
    branch point, fails to push as diverged: the reset clears the last synchronized revision, and
    the branch point does not stand in for it."""
    repo: Lore = new_lore_repo()
    repo.write_commit_push("First", {"a.txt": "first"})
    first = repo.branch_info("main").local_latest
    repo.branch_create("feature")
    repo.push()
    repo.write_commit_push("Local", {"b.txt": "local"}, offline=True)
    other = repo.clone()
    other.branch_switch("feature")
    other.write_commit_push("Second", {"a.txt": "second"})
    repo.branch_reset(first)

    with pytest.raises(BranchDivergedError):
        repo.push()
