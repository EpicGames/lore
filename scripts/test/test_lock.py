# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest

from error_types import LockInvalidPath
from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_lock(new_lore_repo):
    repo: Lore = new_lore_repo("Lock")
    paths = [
        "ignore.txt",
        "duplicate.txt",
        "subdir_a/file_a.txt",
        "subdir_b/file_b.txt",
        "subdir_c/subdir/file_c.txt",
    ]
    for path in paths:
        repo.make_dirs(os.path.dirname(path))
        with repo.open_file(path, "w+b") as output_file:
            output_file.write(os.urandom(1024))

    with repo.open_file(repo.ignore_file(), "w") as output_file:
        output_file.writelines("ignore.txt")

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    # Lock acquire
    with pytest.raises(LockInvalidPath):
        repo.lock_acquire("file_a.txt")
    assert "subdir_a/file_a.txt" in repo.lock_acquire("subdir_a/file_a.txt").acquired
    assert len(repo.lock_acquire("ignore.txt").acquired) == 0
    assert (
        "subdir_c/subdir/file_c.txt"
        in repo.lock_acquire("subdir_c/subdir/file_c.txt").acquired
    )
    assert len(repo.lock_acquire(["duplicate.txt", "duplicate.txt"]).acquired) == 1

    # Lock status
    assert repo.lock_status("file_a.txt")[0].invalid_path
    assert (
        len(repo.lock_status(["subdir_a/file_a.txt", "subdir_c/subdir/file_c.txt"]))
        == 2
    )
    assert len(repo.lock_status("ignore.txt")) == 0
    assert len(repo.lock_status(["duplicate.txt", "duplicate.txt"])) == 1

    # Lock release
    with pytest.raises(LockInvalidPath):
        repo.lock_release("file_a.txt")
    # Should succeed - Release lock by the same user that acquired it
    assert "subdir_a/file_a.txt" in repo.lock_release("subdir_a/file_a.txt").released
    assert len(repo.lock_release(["duplicate.txt", "duplicate.txt"]).released) == 1

    clone = repo.clone()

    # Lock acquire in clone repo
    assert "subdir_a/file_a.txt" in clone.lock_acquire("subdir_a/file_a.txt").acquired
    assert (
        "subdir_c/subdir/file_c.txt"
        in clone.lock_acquire("subdir_c/subdir/file_c.txt").already_owned
    )

    results = clone.lock_status(
        ["file_a.txt", "subdir_a/file_a.txt", "subdir_c/subdir/file_c.txt"]
    )
    assert (
        results[0].invalid_path
        and not results[1].invalid_path
        and not results[2].invalid_path
    )
