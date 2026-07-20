# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Regression test: --local read verbs must not stall on a remote connect
when the server is unreachable.

Follow-up to the remote-resolution-timeout fix, which bounded the connect
stall with 5 s transport timeouts but left several read paths driving a
remote connect whose result is never used in the --local case:

- ``revision::resolve`` drove the connect before checking
  ``should_search_remote``, so ``branch@LATEST`` under --local/--offline
  connected anyway.
- explicit-branch ``revision history`` connected unconditionally and then
  discarded the remote latest unless --remote was set.
- ``file history`` connected before taking the --local early-return.
- ``auth::resolve_user_info`` (driven by the CLI after every history
  listing to prettify user ids) connected regardless of --local.

With those paths fixed, each --local read below must return the local
latest promptly against a killed server. Before the fix each call paid a
bounded multi-second connect timeout; the threshold sits well under that.

The default (no-flags) read is value-checked but only loosely bounded: in
default mode the CLI's user-info resolution may still legitimately try the
remote and pay the bounded connect timeout when the server is down.
"""

import logging
import time

import pytest

from lore_server import (
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)
from test_branch_switch_reconnect import _force_kill_server

logger = logging.getLogger(__name__)

# --local reads must complete well under the ~5 s transport connect timeout
# the pre-fix code paid against an unreachable server.
LOCAL_READ_DEADLINE_S = 3.0

# The default read may pay one bounded connect timeout (user-info
# resolution) but must not stack several of them.
DEFAULT_READ_DEADLINE_S = 8.0


def _timed(label: str, deadline_s: float, fn):
    start = time.monotonic()
    result = fn()
    elapsed = time.monotonic() - start
    logger.info("%s completed in %.2fs", label, elapsed)
    assert elapsed < deadline_s, (
        f"{label} took {elapsed:.2f}s against a killed server — "
        "it is still driving a needless remote connect"
    )
    return result


@pytest.mark.smoke
def test_local_reads_skip_remote_connect(
    request,
    tmp_path_factory,
    lore_server_executable_path,
    new_lore_repo,
):
    # Dedicated server for this test so killing it doesn't disrupt tests
    # that share the session-scoped autouse server. Mirrors the pattern in
    # scripts/test/test_branch_switch_reconnect.py.
    shared_port = allocate_free_port()
    server_ports = {
        "quic": shared_port,
        "grpc": shared_port,
        "http": allocate_free_port(),
        "internal": allocate_free_port(),
    }
    server_root, server_env = generate_server_config(
        request, tmp_path_factory, server_ports
    )
    server_proc, _server_log_path, server_log_fd = launch_lore_server(
        server_root, server_env, lore_server_executable_path
    )
    try:
        repo = new_lore_repo(remote_url=f"lore://127.0.0.1:{server_ports['quic']}/")
        text_file = "file.txt"
        repo.write_commit_push("Initial commit", {text_file: ["Line one\n"]})
        local_latest = repo.revision_history(1, offline=True)[0].signature
    finally:
        # Kill the server outright: every read below must succeed on local
        # data alone, without waiting out remote connect timeouts.
        _force_kill_server(server_proc, server_log_fd)

    # Explicit-branch history under --local: the search-location contract
    # forbids any remote traffic, so the read must be near-instant.
    revisions = _timed(
        "history(branch=main, --local)",
        LOCAL_READ_DEADLINE_S,
        lambda: repo.history(1, branch="main", local=True),
    )
    assert revisions and revisions[0].signature == local_latest, (
        "--local explicit-branch history must return the local latest"
    )

    # branch@LATEST resolve under --local exercises revision::resolve,
    # which previously drove the connect before checking the search
    # location.
    revisions = _timed(
        "history(revision=main@LATEST, --local)",
        LOCAL_READ_DEADLINE_S,
        lambda: repo.history(1, revision="main@LATEST", local=True),
    )
    assert revisions and revisions[0].signature == local_latest, (
        "--local branch@LATEST resolve must return the local latest"
    )

    # File history under --local previously connected before taking the
    # --local early-return.
    output = _timed(
        "file history(--local)",
        LOCAL_READ_DEADLINE_S,
        lambda: repo.file_history(text_file, branch="main", local=True),
    )
    assert "Initial commit" in output, (
        "--local file history must list the committing revision"
    )

    # Explicit-branch history with no flags returns the local latest
    # without the history lookup itself connecting; only the CLI's
    # user-info resolution may still pay one bounded connect timeout.
    revisions = _timed(
        "history(branch=main)",
        DEFAULT_READ_DEADLINE_S,
        lambda: repo.history(1, branch="main"),
    )
    assert revisions and revisions[0].signature == local_latest, (
        "default explicit-branch history must return the local latest"
    )
