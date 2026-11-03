# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""A peer that answers every storage copy with a slow-down.

Content the peer already holds under one file is committed again under a second, so the client
asks the peer to duplicate the association rather than send the payload. The peer keeps
answering that request with a slow-down, and the client is expected to back off, run out of
retries, and still not send the payload.
"""

import logging
import os

import pytest
from error_types import LoreException
from grpc_admin import fetch_server_features
from lore_parsers import parse_commit_stats_json, parse_jsonl
from lore_server import (
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)

from lore import Lore

logger = logging.getLogger(__name__)

#: Large enough to distinguish its transfer from the revision tree's own blocks. Random, so
#: compression can't shrink it into that size range.
BLOB_SIZE = 1024 * 1024

#: The error code a slow-down is reported with.
SLOW_DOWN = 31


@pytest.fixture()
def throttled_copy_remote_url(request, tmp_path_factory, lore_server_executable_path):
    """Yield a remote URL pointing to a dedicated loreserver whose store answers every copy
    with a slow-down."""
    shared_port = allocate_free_port()
    ports = {
        "quic": shared_port,
        "grpc": shared_port,
        "http": allocate_free_port(),
        "internal": allocate_free_port(),
    }
    server_root, server_env = generate_server_config(request, tmp_path_factory, ports)
    server_env["LORE_THROTTLE_COPIES"] = "1"
    proc, log_path, log_fd = launch_lore_server(
        server_root, server_env, lore_server_executable_path
    )
    host = "127.0.0.1"
    try:
        # LORE_THROTTLE_COPIES is only honored when the server is built with the
        # `failure_generator` cargo feature. Without it the copies succeed and the
        # test's premise doesn't hold.
        features = fetch_server_features(f"{host}:{ports['grpc']}")
        if "failure_generator" not in features:
            pytest.skip(
                "server not built with the 'failure_generator' feature "
                "(build loreserver with --features failure_generator)"
            )
        yield f"lore://{host}:{ports['quic']}/"
    finally:
        _kill_server_by_pid(proc.pid, log_path, label="throttled-copy server")
        log_fd.close()


def push_totals(output: str) -> tuple[int, int]:
    """The `(fragments, bytes_transferred)` a push finished with."""
    events = parse_jsonl(output, "branchPushFragmentEnd")
    assert events, f"push emitted no branchPushFragmentEnd event:\n{output}"
    end = events[-1]
    return end["fragments"], end["bytesTransferred"]


def held_under_two_files(repo: Lore) -> None:
    """Push content to the peer, then stage the same bytes under a second file."""
    with repo.open_file("first.bin", "wb+") as output_file:
        output_file.write(os.urandom(BLOB_SIZE))
    repo.stage(scan=True)
    repo.commit("Commit the content offline", offline=True)
    _, transferred = push_totals(repo.push(json=True))
    assert transferred >= BLOB_SIZE // 2, (
        f"the first push must upload the payload, transferred {transferred} of {BLOB_SIZE}"
    )

    repo.copy2("first.bin", "second.bin")
    repo.stage(scan=True)


@pytest.mark.smoke
@pytest.mark.requires_instrumented_lore
def test_a_commit_keeps_a_throttled_copy_local(
    new_lore_repo, throttled_copy_remote_url
):
    """The commit succeeds without uploading the payload. The payload stays in the local store,
    not durable, so the branch is left for a later push to deliver."""
    repo: Lore = new_lore_repo(
        "CopySlowDownCommit", remote_url=throttled_copy_remote_url
    )
    held_under_two_files(repo)

    output = repo.commit(
        "Commit the same content under a second file", json=True, stats=1
    )

    stats = parse_commit_stats_json(output)
    assert stats is not None, "commit must emit a revisionCommitStats event"
    fragments = stats["fragments"]
    assert fragments["remoteCopyWrites"] == 0, (
        f"the peer throttles every copy, so none may land, got {fragments}"
    )
    assert fragments["remoteUploadFailed"] > 0, (
        f"the throttled fragments are left local and counted as not taken, got {fragments}"
    )
    assert fragments["remotePutBytes"] < BLOB_SIZE // 10, (
        f"the throttled payload must not be uploaded instead, got {fragments}"
    )
    assert fragments["localPayloadBytes"] >= BLOB_SIZE // 2, (
        f"the payload must be kept in the local store, got {fragments}"
    )


@pytest.mark.smoke
@pytest.mark.requires_instrumented_lore
def test_a_push_fails_when_its_copy_stays_throttled(
    new_lore_repo, throttled_copy_remote_url
):
    """The push fails with a slow-down error rather than uploading the payload instead."""
    repo: Lore = new_lore_repo("CopySlowDownPush", remote_url=throttled_copy_remote_url)
    held_under_two_files(repo)
    repo.commit("Commit the same content under a second file", offline=True)

    with pytest.raises(LoreException) as error:
        repo.push(json=True)

    output = str(error.value)
    [complete] = parse_jsonl(output, "complete")
    assert complete["error"]["errorCode"] == SLOW_DOWN, (
        f"the push must fail with a slow-down error, got {complete}"
    )
    progress = parse_jsonl(output, "branchPushFragmentProgress")
    assert progress, f"the push must report its fragment progress:\n{output}"
    transferred = progress[-1]["bytesTransferred"]
    assert transferred < BLOB_SIZE // 10, (
        f"the throttled payload must not be uploaded instead, transferred {transferred}"
    )
