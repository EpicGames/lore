# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Push duplicating an association rather than transferring a payload the peer holds.

A push queries the peer before it sends anything. Where the answer is that the partition
already holds the hash under another context, the peer is asked to duplicate that
association instead of being sent bytes it has. What that looks like from outside is a push
that reports fragments to register and almost no bytes to transfer.

The content is committed offline so the push is what carries it: with a remote configured,
the commit itself uploads, which would leave the push nothing to do.
"""

import logging
import os

import pytest

from lore import Lore
from lore_parsers import parse_jsonl, parse_push_stats_json

logger = logging.getLogger(__name__)

#: Large enough that transferring it cannot be confused with the revision tree's own blocks,
#: and random so compression cannot shrink it into that range either.
BLOB_SIZE = 1024 * 1024


def push_totals(output: str) -> tuple[int, int]:
    """The `(fragments, bytes_transferred)` a push finished with."""
    events = parse_jsonl(output, "branchPushFragmentEnd")
    assert events, f"push emitted no branchPushFragmentEnd event:\n{output}"
    end = events[-1]
    return end["fragments"], end["bytesTransferred"]


@pytest.mark.smoke
def test_push_copies_content_the_peer_already_holds(new_lore_repo):
    repo: Lore = new_lore_repo("PushUseCopy")
    blob = os.urandom(BLOB_SIZE)

    with repo.open_file("first.bin", "wb+") as output_file:
        output_file.write(blob)

    repo.stage(scan=True)
    repo.commit("Commit the content offline", offline=True)
    fragments, transferred = push_totals(repo.push(json=True))

    # The control. Nothing on the peer matches, so this push has to carry the payload — and a
    # push that transferred nothing here would make the assertion below meaningless.
    assert fragments > 0, "the first push must carry the content it committed offline"
    assert transferred >= BLOB_SIZE // 2, (
        f"the first push must transfer the payload, transferred {transferred} of {BLOB_SIZE}"
    )

    # The same bytes under a second file, so the peer holds the hash under another context.
    repo.copy2("first.bin", "second.bin")
    repo.stage(scan=True)
    repo.commit("Commit the same content under a second file", offline=True)
    fragments, transferred = push_totals(repo.push(json=True))

    assert fragments > 0, "the second push must still register the new file's fragments"
    assert transferred < BLOB_SIZE // 10, (
        f"the peer already holds these bytes, so the push must duplicate the association "
        f"rather than transfer {transferred} bytes of {BLOB_SIZE}"
    )

    # A duplicated association has to be a real one: a clone reads both files back, so a copy
    # that reported success while registering nothing fails here rather than passing quietly.
    cloned = repo.clone(name="PushUseCopy-Clone")
    assert repo.compare_file(cloned, "first.bin", "first.bin"), (
        "the uploaded file must read back from the clone"
    )
    assert repo.compare_file(cloned, "first.bin", "second.bin"), (
        "the copied file must read back from the clone with the same content"
    )


@pytest.mark.smoke
def test_push_stats_account_for_copies_and_uploads(new_lore_repo):
    """The statistics a push reports must distinguish an association the peer
    duplicated from a payload it was sent. The two cost very different amounts and
    the copy path exists to turn one into the other, so folding them together
    would hide the saving it was made to show.
    """
    repo: Lore = new_lore_repo("PushStats")
    blob = os.urandom(BLOB_SIZE)

    with repo.open_file("uploaded.bin", "wb+") as output_file:
        output_file.write(blob)
    repo.stage(scan=True)
    repo.commit("Commit the content offline", offline=True)

    output = repo.push(json=True, stats=1)
    events = parse_jsonl(output, "branchPushStats")
    assert len(events) == 1, (
        f"the statistics event is emitted once, when the push finishes, got "
        f"{len(events)}"
    )

    stats = parse_push_stats_json(output)
    assert stats is not None, "push must emit a branchPushStats event"

    assert stats["put"] > 0, (
        f"the peer holds nothing yet, so the payload must be uploaded, got {stats}"
    )
    assert stats["copied"] == 0, (
        f"the peer has nothing to duplicate an association from, got {stats}"
    )

    # The same bytes under a second file, so the peer holds the hash under another
    # context and can duplicate the association instead of being sent it again.
    repo.copy2("uploaded.bin", "copied.bin")
    repo.stage(scan=True)
    repo.commit("Commit the same content under a second file", offline=True)

    stats = parse_push_stats_json(repo.push(json=True, stats=1))
    assert stats is not None, "the second push must report too"

    assert stats["copied"] > 0, (
        "the peer holds these hashes under another context, so the associations "
        f"must be duplicated rather than uploaded, got {stats}"
    )

    assert stats["copied"] + stats["put"] > 0, (
        f"and every fragment offered was registered one way or the other, got {stats}"
    )
