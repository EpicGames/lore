# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest

from error_types import (
    LockInvalidPath,
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

    # The authless server records every lock owner as the `<unknown>`
    # placeholder identity and advertises no user directory, so the owner
    # name is not resolved to an ID: the token-only service treats the name
    # as the ID itself, and querying by the placeholder finds the main lock.
    output = repo.lock_query("main", owner="<unknown>")
    assert [lock.file for lock in output] == ["subdir_a/file_a.txt"]
    assert output[0].owner == "<unknown>"

    with pytest.raises(LockQueryFailed):
        repo.lock_query(path="non_extant_file.txt")

    with pytest.raises(InvalidBranch):
        repo.lock_query("notdev")

    assert len(repo.lock_query("release", path="subdir_b/file_c.txt")) == 0


def _unsigned_jwt(claims: dict) -> str:
    """A JWT with a placeholder signature. The client only decodes it for
    its claims here, and the authless server never reads it."""
    import base64
    import json

    def part(obj: dict) -> str:
        raw = json.dumps(obj, separators=(",", ":")).encode()
        return base64.urlsafe_b64encode(raw).decode().rstrip("=")

    return f"{part({'alg': 'HS256', 'typ': 'JWT'})}.{part(claims)}.signature"


@pytest.mark.smoke
def test_lock_query_owner_name_resolves_from_a_supplied_identity_token(new_lore_repo):
    """Against the authless server the token-only user service is the only
    directory, and a caller who supplied `--identity-token` alone is the one
    bearer it can name. Querying locks by that token's display name must
    resolve to the token's subject, not treat the name as the ID."""
    repo: Lore = new_lore_repo("LockQueryOwner")
    with repo.open_file("file.txt", "w") as output_file:
        output_file.write("content")
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    token = _unsigned_jwt(
        {
            "iss": "lore",
            "sub": "token-subject",
            "name": "Token Bearer",
            "preferred_username": "token.bearer",
            "exp": 2_000_000_000,
            "aud": ["127.0.0.1"],
        }
    )

    output = repo.run(
        ["lock", "query", "--owner", "Token Bearer"],
        identity_token=token,
        debug=True,
    )

    assert "Token Bearer resolved to user id token-subject" in output
