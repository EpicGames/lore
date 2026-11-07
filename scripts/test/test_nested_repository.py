# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Smoke coverage for nested-repository handling during a filesystem walk.

A child directory that is itself a Lore working copy (it carries its own
`.lore/`) is an implicit boundary on every walk: the parent must not index it or
pull its contents into the parent tree. A directory the parent had already
staged before becoming one is discarded with its subtree, since the parent has
no committed base it could report a deletion against.
"""

import logging
import os

import pytest

from error_types import NestedRepositoryError
from lore import Lore
from lore_parsers import parse_status_json

pytestmark = pytest.mark.own_vfs(
    "nests a repository in the working tree, which a virtual clone serves as its own"
)

logger = logging.getLogger(__name__)


def _scan_paths(repo: Lore) -> list[str]:
    """Return the set of status-file paths reported by `status --scan --json`."""
    entries = parse_status_json(repo.status(scan=True, json=True, offline=True))
    return [e.get("path", "").replace("\\", "/") for e in entries]


def _create_nested_repository(repo: Lore, name: str) -> None:
    """Make `name` under `repo` a working copy of its own."""
    repo.make_dirs(name)
    repo.run(
        ["repository", "create", name],
        path=os.path.join(repo.path, name),
        offline=True,
    )


@pytest.mark.smoke
def test_nested_repository_not_indexed(new_lore_repo):
    repo: Lore = new_lore_repo()

    # A file that legitimately belongs to the parent.
    with repo.open_file("parent_file.txt", "w+b") as handle:
        handle.write(os.urandom(32))

    _create_nested_repository(repo, "nested")
    with repo.open_file(os.path.join("nested", "inner.txt"), "w+b") as handle:
        handle.write(os.urandom(32))

    # The parent indexes its own file but nothing under the nested repository.
    paths = _scan_paths(repo)
    assert any(p == "parent_file.txt" for p in paths), (
        f"expected the parent's own file to be indexed, got {paths}"
    )
    assert not any(p == "nested" or p.startswith("nested/") for p in paths), (
        f"nested repository contents must not be indexed, got {paths}"
    )


@pytest.mark.smoke
def test_stage_scan_does_not_stage_nested_repository(new_lore_repo):
    repo: Lore = new_lore_repo()

    # A file that legitimately belongs to the parent.
    with repo.open_file("parent_file.txt", "w+b") as handle:
        handle.write(os.urandom(32))

    _create_nested_repository(repo, "nested")
    with repo.open_file(os.path.join("nested", "inner.txt"), "w+b") as handle:
        handle.write(os.urandom(32))

    # `stage --scan` walks the filesystem itself rather than through the diff
    # scanner, so the boundary has to hold on that walk too.
    repo.stage(".", scan=True, offline=True)

    staged = [
        e.get("path", "").replace("\\", "/")
        for e in parse_status_json(repo.status(json=True, offline=True))
    ]
    assert any(p == "parent_file.txt" for p in staged), (
        f"expected the parent's own file to be staged, got {staged}"
    )
    assert not any(p == "nested" or p.startswith("nested/") for p in staged), (
        f"nested repository contents must not be staged, got {staged}"
    )


@pytest.mark.smoke
def test_staged_directory_becoming_nested_repository_is_discarded(new_lore_repo):
    repo: Lore = new_lore_repo()

    # A plain directory with content, which the first scan stages as an
    # ordinary dirty-add subtree.
    repo.make_dirs("plain")
    with repo.open_file(os.path.join("plain", "inner.txt"), "w+b") as handle:
        handle.write(os.urandom(32))

    paths = _scan_paths(repo)
    assert any(p == "plain" or p.startswith("plain/") for p in paths), (
        f"expected the plain directory to be indexed by the first scan, got {paths}"
    )

    # It becomes a nested repository, as when `lore repository create` runs
    # inside a directory the parent has already staged.
    _create_nested_repository(repo, "plain")

    # The rescan discards the staged entry instead of reporting a delete the
    # parent has no committed base for.
    paths = _scan_paths(repo)
    assert not any(p == "plain" or p.startswith("plain/") for p in paths), (
        f"staged directory turned nested repository must leave no entry, got {paths}"
    )

    # A further scan stays clean: the entry was discarded, not hidden.
    paths = _scan_paths(repo)
    assert not any(p == "plain" or p.startswith("plain/") for p in paths), (
        f"discarded entry must not resurface on a later scan, got {paths}"
    )


@pytest.mark.smoke
def test_stage_scan_discards_staged_directory_turned_nested(new_lore_repo):
    repo: Lore = new_lore_repo()

    # A file that legitimately belongs to the parent, so the walk has real work
    # to do alongside the entry it has to drop.
    with repo.open_file("parent_file.txt", "w+b") as handle:
        handle.write(os.urandom(32))

    # A plain directory the first scan indexes as an ordinary dirty-add subtree.
    repo.make_dirs("plain")
    with repo.open_file(os.path.join("plain", "inner.txt"), "w+b") as handle:
        handle.write(os.urandom(32))
    paths = _scan_paths(repo)
    assert any(p == "plain" or p.startswith("plain/") for p in paths), (
        f"expected the plain directory to be indexed by the first scan, got {paths}"
    )

    _create_nested_repository(repo, "plain")

    # `stage --scan` walks the filesystem itself, so it has to reach the same
    # verdict the diff scanner does on an entry an earlier walk left indexed:
    # drop it rather than promote the nested repository's contents to staged.
    repo.stage(".", scan=True, offline=True)

    staged = [
        e.get("path", "").replace("\\", "/")
        for e in parse_status_json(repo.status(json=True, offline=True))
    ]
    assert any(p == "parent_file.txt" for p in staged), (
        f"expected the parent's own file to be staged, got {staged}"
    )
    assert not any(p == "plain" or p.startswith("plain/") for p in staged), (
        f"staged directory turned nested repository must be discarded, got {staged}"
    )


@pytest.mark.smoke
def test_stage_refuses_a_named_nested_repository(new_lore_repo):
    repo: Lore = new_lore_repo()

    _create_nested_repository(repo, "nested")
    with repo.open_file(os.path.join("nested", "inner.txt"), "w+b") as handle:
        handle.write(os.urandom(32))

    # An explicitly named path never reaches the child loop holding the
    # boundary, so naming one is refused rather than silently staged.
    with pytest.raises(NestedRepositoryError):
        repo.stage("nested", scan=True, offline=True)

    with pytest.raises(NestedRepositoryError):
        repo.stage(os.path.join("nested", "inner.txt"), scan=True, offline=True)

    staged = [
        e.get("path", "").replace("\\", "/")
        for e in parse_status_json(repo.status(json=True, offline=True))
    ]
    assert not any(p == "nested" or p.startswith("nested/") for p in staged), (
        f"a refused stage must leave nothing staged, got {staged}"
    )
