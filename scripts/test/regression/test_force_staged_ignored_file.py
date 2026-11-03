# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import pytest
from lore_parsers import parse_status_json
from test_utils import to_posix

from lore import Lore


def _status_paths(repo: Lore, flag: str, scan: bool = False) -> list[str]:
    entries = parse_status_json(repo.status(json=True, scan=scan, offline=True))
    return sorted(to_posix(e["path"]) for e in entries if e.get(flag))


@pytest.mark.regression
def test_force_staged_ignored_file(new_lore_repo):
    """A file staged past `.loreignore` with `--force` is in the tree, and the
    ignore file governs only what is new to it: status reports the file,
    unstage takes it back, and once committed its changes are staged without
    `--force`. A new file the same rule names stays ignored.
    """
    repo: Lore = new_lore_repo()
    repo.write_files({".loreignore": "*.log\n", "keep.txt": "base\n", "debug.log": "noise\n"})

    repo.stage(".", scan=True, force=True, offline=True)
    assert "debug.log" in _status_paths(repo, "flagStaged"), "a force-staged file must be reported"

    repo.unstage(".", offline=True)
    assert "debug.log" not in _status_paths(repo, "flagStaged"), "unstage must take it back"

    repo.stage(".", scan=True, force=True, offline=True)
    repo.commit(message="force staged", offline=True)

    repo.write_files({"debug.log": "changed\n", "new.log": "never tracked\n"})
    assert _status_paths(repo, "flagDirty", scan=True) == ["debug.log"], (
        "a change to the tracked file must be reported, and the new one ignored"
    )

    repo.stage(".", scan=True, offline=True)
    assert _status_paths(repo, "flagStaged") == ["debug.log"], (
        "the tracked file must be staged without --force, and the new one ignored"
    )


@pytest.mark.regression
def test_dirty_tracked_ignored_file_in_layer(new_lore_repo):
    """`lore dirty` on a path inside a layer looks it up in the layer's tree, so
    a tracked file the parent's `.loreignore` names is marked without
    `--force`, while an untracked one beside it stays ignored.
    """
    repo: Lore = new_lore_repo()
    layer_repo: Lore = new_lore_repo(repo.name + "_layer")

    repo.write_commit_push(None, {".loreignore": "*.log\n"})
    layer_repo.make_dirs("lay")
    layer_repo.write_commit_push(None, {"lay/debug.log": "committed\n"})
    repo.layer_add("lay", layer_repo, "lay/")

    repo.write_files({"lay/debug.log": "changed\n", "lay/new.log": "never tracked\n"})
    repo.dirty(["lay/debug.log", "lay/new.log"])

    assert _status_paths(repo, "flagDirty") == ["lay/debug.log"], (
        "the tracked layer file must be marked, and the new one ignored"
    )
