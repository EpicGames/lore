# Copyright Epic Games, Inc. All Rights Reserved.
# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import os

import pytest

from lore import Lore
from test_nested_repository import _create_nested_repository


@pytest.mark.regression
@pytest.mark.bug_reproduction
# "." walks the tree and deletes every child the target revision does not list,
# "nested" is looked up directly, missed, and deleted as a path absent from the target.
# The same outcome, two scenarios, a guard added at one site does not cover the other.
@pytest.mark.parametrize("purged", [".", "nested"])
def test_reset_purge_skips_nested_repository(new_lore_repo, purged):
    """A repository created inside another repository's working tree must survive
    `reset --purge` in the outer repository, untracked work included. The scan
    already treats a nested repository as a boundary; the purge walk does not.
    """
    outer: Lore = new_lore_repo()
    outer.write_commit_push("base", {"kept.txt": b"outer\n"})

    _create_nested_repository(outer, "nested")
    nested_repo = os.path.join("nested", ".lore")
    inner_file = os.path.join("nested", "inner.txt")
    outer.write_files({inner_file: b"inner\n", "junk.txt": b"junk\n"})

    # A refusal is a valid outcome, so the exit code is not asserted — only what
    # survives on disk.
    outer.reset(purged, purge=True, check=False)

    # Untracked and outside the nested repository, so purge is meant to remove
    # it. Without this, a purge that did nothing passes everything below.
    if purged == ".":
        assert not outer.file_exists("junk.txt"), (
            "reset --purge removed nothing, so the assertions below prove nothing"
        )

    assert outer.path_exists(nested_repo), (
        "reset --purge deleted the nested repository"
    )
    assert outer.file_exists(inner_file), (
        "reset --purge deleted untracked content in the nested repository"
    )
    assert outer.file_exists("kept.txt"), (
        "reset --purge deleted tracked files in the root repository"
    )
