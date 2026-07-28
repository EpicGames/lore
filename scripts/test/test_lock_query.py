# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest

from error_types import (
    LockInvalidPath,
    UserNotAuthenticated,
    LockQueryFailed,
    InvalidBranch,
)
from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_lock_query(new_lore_repo):
    repo: Lore = new_lore_repo("LockQuery")
    paths = [
        "ignore.txt",
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

    # Lock acquire @ main
    with pytest.raises(LockInvalidPath):
        repo.lock_acquire("file_a.txt")
    repo.lock_acquire("subdir_a/file_a.txt")
    repo.lock_acquire("ignore.txt")

    repo.branch_create("dev")
    repo.lock_acquire("subdir_c/subdir/file_c.txt")

    # Lock acquire @ release
    repo.branch_create("release")
    repo.lock_acquire("subdir_b/file_b.txt")

    # Lock query being on main
    repo.branch_switch("main")
    output = repo.lock_query()
    assert (
        "subdir_a/file_a.txt" in output[0].file
        and "subdir_b/file_b.txt" in output[1].file
        and "subdir_c/subdir/file_c.txt" in output[2].file
    )

    output = repo.lock_query("dev")
    assert "subdir_c/subdir/file_c.txt" in output[0].file and len(output) == 1

    output = repo.lock_query("release", path="subdir_b/file_b.txt")
    assert "subdir_b/file_b.txt" in output[0].file and len(output) == 1

    outside_repo_path = os.path.join(os.path.dirname(repo.path), "ignore.txt")

    # Fails because path is not relative to repo
    with pytest.raises(LockInvalidPath):
        repo.run(["lock", "query", "--branch", "main", "--path", outside_repo_path])

    # Fails because the call to auth::userinfo::user_id returns UserInfoError::Authenticate
    with pytest.raises(UserNotAuthenticated):
        repo.lock_query("main", owner="<unknown>")

    with pytest.raises(LockQueryFailed):
        repo.lock_query(path="non_extant_file.txt")

    with pytest.raises(InvalidBranch):
        repo.lock_query("notdev")

    assert len(repo.lock_query("release", path="subdir_b/file_c.txt")) == 0
