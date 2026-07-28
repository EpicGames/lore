# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import pytest

from error_types import LoreException
from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_search_nearest_layer_metadata(new_lore_repo):
    """--search-nearest searches through source revision history to find
    metadata matches when the current revision doesn't match directly.

    Setup produces diverged histories linked by commit message:

        Main:  R1("Test commit 1") -> R2("no_match")
        Layer: L1("Test commit 1") -> L2("linked_tag") -> L3("layer_diverged")

    The "linked_tag" commit only touches the layer file, so main stays
    at R1 â€” only the layer receives L2.

    Without search_nearest: only R2 ("no_match") is checked against
    layer history â€” no match found, branch switch FAILS.
    With search_nearest: R2, R1 are checked -> R1 matches L1
    by the shared "Test commit 1" message -> branch switch succeeds
    and layer syncs to L1.

    Code path: layer.rs:1008-1108 â€” find_revision_match() with
    search_nearest flag controls whether source revision history is
    batch-loaded and iterated beyond the current revision.
    """
    repo: Lore = new_lore_repo()
    layer_repo: Lore = new_lore_repo(repo.name + "_layer")

    repo.write_commit_push(None, {"main.txt": b"v1"})
    layer_repo.make_dirs("lay")
    layer_repo.write_commit_push(None, {"lay/data.txt": b"initial"})
    repo.layer_add("lay", layer_repo, "lay/", metadata="message")

    # Only the layer receives a new revision L2 with message "linked_tag".
    # Main stays at R1 because no main files were changed.
    with repo.open_file(os.path.join("lay", "data.txt"), "wb") as f:
        f.write(b"linked")
    repo.stage(scan=True)
    repo.commit("linked_tag", non_interactive=True)
    repo.push()

    with repo.open_file("main.txt", "wb") as f:
        f.write(b"v2")
    with repo.open_file(os.path.join("lay", "data.txt"), "wb") as f:
        f.write(b"updated")
    repo.stage(scan=True)
    repo.commit("no_match", layer_messages={"lay": "layer_diverged"}, non_interactive=True)
    repo.push()

    repo.branch_create("sn-pass")
    repo.push()
    output = repo.branch_switch("main", search_nearest=True, level="debug")
    assert "search nearest: true" in output, (
        "Expected debug log 'search nearest: true'"
    )
    assert "Found matching metadata" in output, (
        "Expected debug log confirming metadata match"
    )

    repo.branch_create("sn-fail")
    repo.push()
    with pytest.raises(LoreException):
        repo.branch_switch("main")
