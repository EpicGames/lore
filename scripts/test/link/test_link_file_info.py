# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import re

import pytest
from link_helpers import make_parent_with_link

pytestmark = pytest.mark.own_vfs(
    "links a repository, which a virtual clone does not hold"
)


@pytest.mark.smoke
def test_file_info_measures_a_tracked_file_the_ignore_file_names_below_a_link(
    new_lore_repo,
):
    """`lore file info --local` of a directory holding a link mount measures a
    file the linked revision holds that the parent's `.loreignore` names, and
    leaves out an untracked file beside it that the rule names.
    """
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_path="vendor/lib",
        link_files={"debug.log": "x" * 100, "keep.txt": "y" * 10},
    )
    parent_repo.write_commit_push("Ignore logs", {".loreignore": "*.log\n"})
    parent_repo.write_files({"vendor/lib/new.log": "z" * 1000})

    output = parent_repo.run(["file", "info", "vendor", "--local"])

    assert re.search(r"Local size: +110\n", output), output
