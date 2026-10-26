# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import pytest
from lore_parsers import parse_status_json
from test_utils import to_posix

from lore import Lore

FILES = {
    "Game/Modules/ModuleA/a1.txt": "a1\n",
    "Game/Modules/ModuleA/a2.txt": "a2\n",
    "Game/Modules/ModuleA/a3.txt": "a3\n",
    "Game/Modules/ModuleB/b1.txt": "b1\n",
    "Game/Modules/ModuleB/b2.txt": "b2\n",
    "Game/Modules/ModuleB/b3.txt": "b3\n",
}
NAMED = ["Game/Modules/ModuleA/a1.txt", "Game/Modules/ModuleA/a2.txt"]


@pytest.mark.regression
@pytest.mark.parametrize("working_copy", ["removed", "bare"])
def test_stage_paths_under_missing_ancestor(new_lore_repo, working_copy):
    """Staging two paths whose common ancestor is missing from the working
    directory must stage only those two paths, not the whole ancestor.

    "bare" is the P4 mirror case, where the files were never fetched rather
    than deleted.
    """
    repo: Lore = new_lore_repo()
    repo.write_commit_push("base", FILES)

    if working_copy == "bare":
        repo = repo.clone(bare=True)
    else:
        repo.rmtree("Game")

    repo.stage(NAMED)

    entries = parse_status_json(repo.status(json=True, offline=True))
    staged = sorted(to_posix(e["path"]) for e in entries if e.get("flagStaged"))
    assert staged == NAMED, f"only the named paths may be staged, got {entries}"
