# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import pytest

from lore import Lore


def _tracked_ignored_repo(new_lore_repo) -> Lore:
    """A repository whose `debug.log` was committed before `.loreignore` named
    `*.log`, and whose working tree holds `new.log`, which the rule names and
    the tree does not track.
    """
    repo: Lore = new_lore_repo()
    repo.write_commit_push(None, {"debug.log": "committed\n"})
    repo.write_commit_push(None, {".loreignore": "*.log\n"})
    repo.write_files({"new.log": "never tracked\n"})
    return repo


@pytest.mark.regression
def test_lock_acquire_of_a_tracked_ignored_file(new_lore_repo):
    """The ignore file governs only what the tree does not track, so a lock on
    a committed file it names is acquired without `--force`, and one on an
    untracked file it names is not.
    """
    repo = _tracked_ignored_repo(new_lore_repo)

    assert repo.lock_acquire(["debug.log", "new.log"]).acquired == ["debug.log"]


@pytest.mark.regression
def test_lock_status_of_a_tracked_ignored_file(new_lore_repo):
    """`lock status` reports the lock on a committed file the ignore file names
    without `--force`, and leaves out an untracked file it names.
    """
    repo = _tracked_ignored_repo(new_lore_repo)
    repo.lock_acquire("debug.log", force=True)

    assert [status.file for status in repo.lock_status(["debug.log", "new.log"])] == [
        "debug.log"
    ]


@pytest.mark.regression
def test_lock_query_of_a_tracked_ignored_file(new_lore_repo):
    """`lock query --path` reports the lock on a committed file the ignore file
    names without `--force`, and nothing for an untracked file it names. A path
    given without a branch is queried on the branch the instance is on, forced
    or not.
    """
    repo = _tracked_ignored_repo(new_lore_repo)
    repo.lock_acquire("debug.log", force=True)

    for branch, force in [("main", False), (None, False), (None, True)]:
        queried = repo.lock_query(branch, path="debug.log", force=force)
        assert [query.file for query in queried] == ["debug.log"], (branch, force)
    assert repo.lock_query("main", path="new.log") == []
    assert repo.lock_query(path="new.log") == []


@pytest.mark.regression
def test_lock_release_of_a_tracked_ignored_file(new_lore_repo):
    """`lock release` releases the lock on a committed file the ignore file
    names without `--force`, and leaves out an untracked file it names.
    """
    repo = _tracked_ignored_repo(new_lore_repo)
    repo.lock_acquire("debug.log", force=True)

    assert repo.lock_release(["debug.log", "new.log"]).released == ["debug.log"]
