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


def _remote_branches(repo: Lore) -> list[str]:
    return sorted(repo.run(["branch", "list"], remote=True, no_pager=True).splitlines())


def _committed_repository(new_lore_repo) -> Lore:
    repo: Lore = new_lore_repo("DryRunRemote")
    repo.write_commit_push("First", {"a.txt": b"first\n"})
    return repo


@pytest.mark.smoke
def test_dry_run_branch_create_force_keeps_the_remote_branch(new_lore_repo):
    """`branch create --force --dry-run` of an existing branch reports the branch it would
    create and leaves the existing one on the remote."""
    repo = _committed_repository(new_lore_repo)
    repo.branch_create("feature")
    repo.write_commit_push("Feature", {"b.txt": b"feature\n"})
    repo.branch_switch("main")
    branches = _remote_branches(repo)

    output = repo.branch_create("feature", force=True, dry_run=True)

    assert "Would create branch feature" in output, output
    assert _remote_branches(repo) == branches, "a dry run deleted the remote branch"


@pytest.mark.smoke
def test_dry_run_layer_add_creates_no_remote_branch(new_lore_repo):
    """`layer add --dry-run` on a branch the layer repository lacks creates no branch there,
    where `layer add` without it does."""
    repo = _committed_repository(new_lore_repo)
    source: Lore = new_lore_repo(repo.name + "_source")
    source.write_commit_push("Source", {"lay/source.txt": b"source\n"})
    repo.branch_create("feature")
    repo.write_commit_push("Feature", {"b.txt": b"feature\n"})
    branches = _remote_branches(source)

    repo.layer_add("lay", source, "lay/", dry_run=True)
    assert _remote_branches(source) == branches, "a dry run created the layer branch"

    repo.layer_add("lay", source, "lay/")
    assert _remote_branches(source) != branches


@pytest.mark.smoke
@pytest.mark.parametrize("scope", ["branch", "repository"])
def test_dry_run_metadata_set_changes_no_remote_metadata(new_lore_repo, scope):
    """`metadata set --dry-run` stores no value on the remote, where `metadata set` does."""
    repo = _committed_repository(new_lore_repo)
    set_metadata = getattr(repo, f"{scope}_metadata_set")
    get_metadata = getattr(repo, f"{scope}_metadata_get")

    set_metadata(["probe", "dry-run"], dry_run=True)
    assert "dry-run" not in get_metadata("probe", check=False)

    set_metadata(["probe", "real"])
    assert "real" in get_metadata("probe")


@pytest.mark.smoke
def test_dry_run_merge_into_pushes_nothing(new_lore_repo):
    """`branch merge into --dry-run` leaves the target branch on the remote where it was,
    where `branch merge into` moves it."""
    repo = _committed_repository(new_lore_repo)
    repo.branch_create("feature")
    repo.write_commit_push("Feature", {"b.txt": b"feature\n"})

    def remote_latest() -> list[str]:
        info = repo.run(["branch", "info", "main"], no_pager=True)
        return [line for line in info.splitlines() if "Remote Latest" in line]

    latest = remote_latest()

    repo.branch_merge_into("main", "Merge feature", dry_run=True)
    assert remote_latest() == latest, "a dry run moved the target branch on the remote"

    repo.branch_merge_into("main", "Merge feature")
    assert remote_latest() != latest
