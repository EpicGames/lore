# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import sys

import pytest
from lore_parsers import parse_status_json
from test_utils import to_posix

from lore import Lore


@pytest.mark.regression
@pytest.mark.skipif(
    sys.platform != "linux", reason="Requires case-sensitive filesystem"
)
def test_stage_paths_in_case_variant_directory(new_lore_repo):
    """Staging several paths written under a directory whose case differs from
    the repository, with --case keep, must stage them as modified under the
    repository's case and never stage the repository files as deleted.

    This is the P4 mirror case: a bare clone that writes only the files a
    changelist touches, spelled in the changelist's case.
    """
    repo: Lore = new_lore_repo()
    repo.write_commit_push(
        "base",
        {
            "Game/Fyre/a.txt": "a\n",
            "Game/Fyre/b.txt": "b\n",
            "Game/Fyre/c.txt": "c\n",
        },
    )

    bare = repo.clone(bare=True)
    bare.write_files({"Game/fyre/a.txt": "a changed\n", "Game/fyre/b.txt": "b changed\n"})
    bare.stage(["Game/fyre/a.txt", "Game/fyre/b.txt"], case="keep", scan=True)

    entries = parse_status_json(bare.status(json=True, offline=True))
    staged = sorted(
        (to_posix(e["path"]), e["action"]) for e in entries if e.get("flagStaged")
    )
    assert staged == [("Game/Fyre/a.txt", "keep"), ("Game/Fyre/b.txt", "keep")], (
        f"only the two files may be staged, as modified, got {entries}"
    )
