# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Smoke coverage for nested-repository handling during a working-tree scan.

A child directory that is itself a Lore working copy (it carries its own
`.lore/`) is an implicit boundary: the parent's `status --scan` must not index
it or pull its contents into the parent tree, and removing it must not leave an
unremovable "zombie" delete entry behind. Also covers `stage --scan` with no
path defaulting to the repository root.
"""

import logging
import os

import pytest

from lore import Lore
from lore_parsers import parse_status_json

logger = logging.getLogger(__name__)


def _scan_paths(repo: Lore) -> list[str]:
    """Return the set of status-file paths reported by `status --scan --json`."""
    entries = parse_status_json(repo.status(scan=True, json=True, offline=True))
    return [e.get("path", "").replace("\\", "/") for e in entries]


@pytest.mark.smoke
def test_nested_repository_not_indexed_and_no_zombie(new_lore_repo):
    repo: Lore = new_lore_repo()

    # A file that legitimately belongs to the parent.
    with repo.open_file("parent_file.txt", "w+b") as handle:
        handle.write(os.urandom(32))

    # A nested repository: its own working copy with its own `.lore/`.
    nested_abs = os.path.join(repo.path, "nested")
    os.makedirs(nested_abs, exist_ok=True)
    repo.run(["repository", "create", "nested"], path=nested_abs, offline=True)
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

    # Removing the nested repository leaves no "zombie" delete entry, because the
    # parent never indexed it and has no committed base it could be deleted from.
    repo.rmtree("nested")
    paths = _scan_paths(repo)
    assert not any(p == "nested" or p.startswith("nested/") for p in paths), (
        f"removed nested repository must not leave a status entry, got {paths}"
    )


@pytest.mark.smoke
def test_stage_scan_without_path_defaults_to_root(new_lore_repo):
    repo: Lore = new_lore_repo()
    with repo.open_file("tracked.txt", "w+b") as handle:
        handle.write(os.urandom(32))

    # `lore stage --scan` with no path reconciles and stages the whole tree.
    repo.run(["stage", "--scan"], offline=True)

    staged = [
        e.get("path", "").replace("\\", "/")
        for e in parse_status_json(repo.status(json=True, offline=True))
    ]
    assert any(p == "tracked.txt" for p in staged), (
        f"`stage --scan` with no path should stage the whole tree, got {staged}"
    )

    # Without `--scan`, a bare `lore stage` still requires a path.
    output = repo.run(["stage"], offline=True, check=False)
    assert "a path is required" in output, (
        f"bare `stage` with no path should report a required-path error, got: {output}"
    )
