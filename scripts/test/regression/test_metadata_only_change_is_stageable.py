# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import pytest

from lore import Lore

@pytest.mark.regression
@pytest.mark.bug_reproduction
def test_metadata_only_change_is_stageable(new_lore_repo):
    """UCS-21581: setting metadata on a tracked file must be a stageable change,
    not silently ignored ('No changes staged').
    """
    repo: Lore = new_lore_repo()
    # 1. create and commit a file
    # 2. set metadata on it (no content change)
    # 3. stage it, capture the output
    # 4. assert the change was detected (output must not say "No changes staged")
    repo.write_commit_push("base", {"f.txt": b"hello\n"})
    repo.file_metadata_set("f.txt", ["Temperature", "99"])
    output = repo.stage("f.txt")
    assert "No changes staged" not in output, "metadata-only change was not detected by stage"
