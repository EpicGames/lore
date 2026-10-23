# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import os

import pytest

from lore import Lore


def _repository_state(repo: Lore) -> dict[str, list[str]]:
    """The lines the commands that report the repository's state print, which a dry run must
    leave as it found them. Sorted, since `branch list` prints branches in no fixed order."""
    return {
        " ".join(command): sorted(
            repo.run(command, offline=True, no_pager=True).splitlines()
        )
        for command in (
            ["branch", "list"],
            ["branch", "info"],
            ["history"],
            ["status"],
            ["file", "dependency", "list", os.path.join(repo.path, "a.txt")],
        )
    }


def _stage_changes(repo: Lore):
    repo.write_files({"a.txt": b"staged\n", "c.txt": b"staged\n"})
    repo.stage(["a.txt", "c.txt"], offline=True)


DRY_RUN_COMMANDS = {
    "branch create": (None, ["branch", "create", "dry-run"]),
    "branch reset": (None, ["branch", "reset", "@1"]),
    "branch archive": (None, ["branch", "archive", "other"]),
    "revision amend": (None, ["revision", "amend", "Amended"]),
    "unstage": (_stage_changes, ["unstage", "a.txt"]),
    "file dependency add": (
        _stage_changes,
        ["file", "dependency", "add", "a.txt", "c.txt"],
    ),
}


@pytest.mark.smoke
@pytest.mark.parametrize("command", DRY_RUN_COMMANDS)
def test_dry_run_leaves_the_repository_state(new_lore_repo, command):
    """A command run with `--dry-run` changes nothing the repository reports, where the same
    command run without it does."""
    repo: Lore = new_lore_repo("DryRun")
    repo.write_commit_push("First", {"a.txt": b"first\n"}, offline=True)
    repo.write_commit_push("Second", {"a.txt": b"second\n"}, offline=True)
    repo.branch_create("other", offline=True)
    repo.write_commit_push("Other", {"b.txt": b"other\n"}, offline=True)
    repo.branch_switch("main", offline=True)

    setup, args = DRY_RUN_COMMANDS[command]
    if setup:
        setup(repo)
    args = [
        os.path.join(repo.path, arg) if arg.endswith(".txt") else arg for arg in args
    ]

    before = _repository_state(repo)
    repo.run(args, dry_run=True, offline=True)
    assert _repository_state(repo) == before, (
        f"`{command} --dry-run` changed the repository state"
    )

    repo.run(args, offline=True)
    assert _repository_state(repo) != before, f"`{command}` had nothing to change"
