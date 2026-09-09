# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging

import pytest
from error_types import NotFound
from lore_parsers import parse_jsonl, parse_revision_list_json

from lore import Lore

logger = logging.getLogger(__name__)


# Given a clone of repository with 25 commits and a history step size of 10
# When
#   - revision list is called for revision id 15
#   - revision describe is called for a revision in the same segment in offline mode
# Then revision describe succeeds as the revision list call cached the segment.
# The lore.revision.v1 protocol returns segment-aligned pages, so the cache is
# scoped to the segment containing the anchor (here, revisions 11..15 for a
# history step size of 10 and anchor 15). That cache holds the states alone, so
# the branch, date and message their metadata carries is not there to read, and
# both revision info and revision history report the revision without those
# rather than failing.
@pytest.mark.smoke
def test_revision_list(new_lore_repo):
    repo: Lore = new_lore_repo()

    text_file = "text-File.txt"

    for i in range(25):
        with repo.open_file(text_file, "a+") as output_file:
            output_file.writelines([f"Line {i}\n"])

        # Stage the files
        repo.stage(scan=True)

        # Commit offline
        repo.commit(offline=True)

    repo.branch_push()

    clone = repo.clone()

    with pytest.raises(NotFound):
        clone.revision_info("@13", offline=True, local=True)

    output = clone.revision_history(1, revision="@15", debug=True)
    assert "Revision list strategy: full-iteration" not in output

    output = clone.revision_info("@13", offline=True, local=True, json=True)
    logged = [entry["message"] for entry in parse_jsonl(output, "log")]
    assert any("metadata" in message for message in logged), (
        f"reporting the revision without its metadata went unsaid: {logged}"
    )

    cached_history = parse_revision_list_json(
        clone.revision_history(1, revision="@13", offline=True, local=True, json=True)
    )
    assert any(entry["revisionNumber"] == 13 for entry in cached_history), (
        f"history off the cached segment did not report revision 13: {cached_history}"
    )
