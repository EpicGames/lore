# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""
Identity propagation smoke tests.

Covers the contract that any operation producing a revision must record
a creator/committer, and that the clone + repo-load paths populate
`.lore/config.toml` so subsequent commands have an identity available.
"""

import logging
from pathlib import Path

import pytest

from lore import Lore

logger = logging.getLogger(__name__)


def _read_identity_in_config(repo: Lore) -> str | None:
    """Returns the `identity = "..."` value from .lore/config.toml, or None
    if the key is absent. Avoids pulling a TOML lib for a single key."""
    config = Path(repo.dot_path()) / "config.toml"
    if not config.exists():
        return None
    for line in config.read_text().splitlines():
        line = line.strip()
        if line.startswith("identity"):
            _, _, value = line.partition("=")
            return value.strip().strip('"')
    return None


def _strip_identity_from_config(repo: Lore) -> None:
    """Removes any `identity = ...` line from .lore/config.toml to simulate
    a clone created before the identity-propagation fix landed."""
    config = Path(repo.dot_path()) / "config.toml"
    if not config.exists():
        return
    kept = [
        line
        for line in config.read_text().splitlines()
        if not line.strip().startswith("identity")
    ]
    config.write_text("\n".join(kept) + "\n")


@pytest.mark.smoke
def test_commit_without_identity_succeeds_against_unauth_remote(new_lore_repo):
    """
    Authorship is required only when the active remote authenticates.
    Against an unauthenticated server (or offline / no remote), committing
    without `--identity` and without a config identity must succeed, just
    producing a revision with no `created-by` / `committed-by` fields.
    """
    repo: Lore = new_lore_repo()
    _strip_identity_from_config(repo)
    with repo.open_file("seed.txt", "w+") as f:
        f.write("seed\n")
    repo.stage(scan=True, offline=True, identity="")
    repo.commit("seed", offline=True, identity="")

    rev = repo.revision_info(metadata=True)
    assert rev.creator == "", f"Expected empty creator, got {rev.creator!r}"
    assert rev.committer == "", f"Expected empty committer, got {rev.committer!r}"


@pytest.mark.smoke
def test_commit_with_identity_stamps_creator_and_committer(new_lore_repo):
    """
    Happy path: `--identity X` stamps both Creator and Committer = X on
    the resulting revision metadata.
    """
    repo: Lore = new_lore_repo()
    with repo.open_file("seed.txt", "w+") as f:
        f.write("seed\n")
    repo.stage(scan=True, offline=True)
    repo.commit("seed", offline=True, identity="alice")

    rev = repo.revision_info(metadata=True)
    assert rev.creator == "alice", f"Expected creator='alice', got {rev.creator!r}"
    assert rev.committer == "alice", (
        f"Expected committer='alice', got {rev.committer!r}"
    )


@pytest.mark.smoke
def test_amend_updates_committer_keeps_creator(new_lore_repo):
    """
    Amending a revision must update Committer to the amender's identity
    while leaving Creator untouched (matches Git author/committer semantics).
    """
    repo: Lore = new_lore_repo()
    with repo.open_file("seed.txt", "w+") as f:
        f.write("seed\n")
    repo.stage(scan=True, offline=True)
    repo.commit("seed", offline=True, identity="alice")

    repo.revision_amend("amended", identity="bob", offline=True)

    rev = repo.revision_info(metadata=True)
    assert rev.creator == "alice", (
        f"Creator should be preserved across amend; got {rev.creator!r}"
    )
    assert rev.committer == "bob", (
        f"Committer should reflect amender identity; got {rev.committer!r}"
    )
    # revision_info prints file changes as additional message lines; only the
    # first line is the actual commit message.
    first_line = rev.message.splitlines()[0] if rev.message else ""
    assert first_line == "amended", f"Message should be amended; got {rev.message!r}"


@pytest.mark.smoke
def test_clone_persists_explicit_identity_to_config(new_lore_repo):
    """
    `lore clone --identity X` writes `identity = "X"` into the new clone's
    config.toml so subsequent commands in that clone pick it up automatically.
    """
    repo: Lore = new_lore_repo()
    with repo.open_file("seed.txt", "w+") as f:
        f.write("seed\n")
    repo.stage(scan=True, offline=True)
    repo.commit("seed", offline=True)
    repo.push()

    cloned = repo.clone(identity="charlie")
    assert _read_identity_in_config(cloned) == "charlie"


@pytest.mark.smoke
def test_explicit_identity_arg_does_not_persist_to_config(new_lore_repo):
    """
    `--identity X` is a one-shot override, not a sticky setting: running a
    write-capable command with `--identity X` against a config that has no
    `identity` field must NOT auto-populate the field. Persistence is
    reserved for the auth-fallback path so users can still pin their
    identity explicitly without each invocation rewriting config.toml.
    """
    repo: Lore = new_lore_repo()
    _strip_identity_from_config(repo)
    assert _read_identity_in_config(repo) is None

    with repo.open_file("seed.txt", "w+") as f:
        f.write("seed\n")
    repo.stage(scan=True, offline=True, identity="dora")
    repo.commit("seed", offline=True, identity="dora")

    assert _read_identity_in_config(repo) is None, (
        "Explicit --identity must not silently rewrite config.toml; "
        "auto-population is reserved for auth-fallback resolution."
    )
