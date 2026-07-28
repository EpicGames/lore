# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Smoke test: a long sync must survive a mid-flight QUIC connection drop.

Regression guard for a user-reported bug where during
`lore branch switch main --force` on a large repo, the QUIC connection to
the storage server timed out mid-sync. The client auto-reconnected in
241 ms, but two file reads that were in flight at the moment of disconnect
failed permanently with::

    Not connected to remote: get: Failed sending command:
    Server returned error code 3

while the other six in-flight reads recovered. The overall operation
aborted with "Failed to synchronize state during branch switch".

Root cause: after a QUIC reconnect, the client's cached `StorageSession`
holds a `session_id` issued by the *old* server connection. The newly
established connection has a fresh `SessionMap` server-side that doesn't
know that `session_id`, so retried `get` requests get
`MessageHandleError::NotConnected` -> `QuicServiceError::Failed (3)`. Fix:
`StorageSession::invalidate` drops the cached session and clears the
parent `Connection`'s session-pool pin, and `remote_get_retry` calls it
when it sees `StorageError::NotConnected`, forcing a fresh
`session_start` on the next attempt.

The test reproduces the failure mode by spawning `lore repository clone`
(same `realize_state` -> `read_into_file` -> `remote_get_retry` path as
branch switch) and force-killing the storage server mid-sync, then
immediately relaunching it on the same ports with the same data
directory. The client's QUIC connection dies, the auto-reconnect
re-handshakes against the restarted server, and every in-flight read
must recover.
"""

import logging
import os
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

import pytest

from lore_server import (
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)

logger = logging.getLogger(__name__)


# Workload sized to keep multiple in-flight `get` requests active for a few
# seconds, so the kill-the-server window is wide enough to land mid-flight
# on any machine. Matches the production failure shape: ≥ 8 concurrent
# in-flight reads when the connection drops. We pair the size with a
# server-side bandwidth throttle (see SERVER_BPS) — on an unthrottled
# loopback this volume transfers in under a second.
NUM_FILES = 32
FILE_SIZE = 4 * 1024 * 1024  # 4 MiB per file -> 128 MiB total

# Throttle the test server's QUIC transport so a from-scratch sync takes
# several seconds, making the kill window deterministic across machines.
# 50 Mbit/s -> 128 MiB takes ~20 s of wall clock.
SERVER_BPS = 50_000_000

# stdout marker that the clone subprocess is mid-FILE-transfer. The
# progress format emitted by `lore repository clone` switches from the
# initial state-fragment phase ("Cloning N files ...") to the file-payload
# phase ("Cloning N/M files (X bytes/Y MiB)") once the QUIC service is
# dispatching parallel `get` requests for actual file content. The "/M
# files" pattern is unique to the second phase — the right moment to yank
# the server.
SYNC_MARKER_TEMPLATE = "/{n} files ("

# The user-visible error string we expect when the bug fires. Originates at
# lore-transport/src/quic/storage_service/client.rs:240
# (ProtocolError::internal "{name}: Failed sending command: Server returned
# error code {status}") for QuicServiceError::Failed = 3.
BUG_ERROR_STRING = "Server returned error code 3"


def _force_kill_server(server_proc: subprocess.Popen, server_log_fd) -> None:
    """Kill the server process group with SIGKILL immediately, no SIGTERM
    grace period, then block until the kernel has reaped it.

    Why SIGKILL: the production failure shape is an abrupt connection drop
    (the client gave up on idle timeout). `_kill_server_by_pid` in
    lore_server.py sends SIGTERM, sleeps 5 s, then SIGKILL — that grace
    window risks the server cleanly closing its QUIC streams, delivering
    ApplicationClosed to the client instead of the abrupt drop we want.

    Why wait: the relaunch's _check_port_free does a TCP connect against
    HTTP/gRPC ports; if we return before the kernel has torn down those
    listeners, the relaunch's port check spuriously succeeds and refuses
    to start.
    """
    if sys.platform == "win32":
        subprocess.run(
            ["taskkill", "/F", "/T", "/PID", str(server_proc.pid)],
            capture_output=True,
        )
    else:
        try:
            os.killpg(server_proc.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            try:
                os.kill(server_proc.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
    try:
        server_proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    try:
        server_log_fd.close()
    except Exception:
        pass


def _wait_for_port_free(host: str, port: int, deadline_s: float = 10.0) -> None:
    """Block until nothing accepts on (host, port). Mirror of
    `_check_port_free` in scripts/test/lore_server.py:297 but as a poll
    loop with a deadline."""
    end = time.monotonic() + deadline_s
    while time.monotonic() < end:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(0.5)
        try:
            sock.connect((host, int(port)))
            sock.close()
        except (ConnectionRefusedError, OSError):
            return  # nothing listening — port is free
        finally:
            sock.close()
        time.sleep(0.1)
    raise RuntimeError(f"Port {port} still in use after {deadline_s}s")


@pytest.mark.smoke
def test_sync_survives_mid_flight_disconnect(
    request,
    tmp_path_factory,
    global_dir_name,
    lore_executable_path,
    lore_server_executable_path,
    new_lore_repo,
):
    # Dedicated server for this test so we can kill+relaunch freely
    # without disrupting other tests that share the session-scoped
    # autouse server. Mirrors the per-class pattern in
    # scripts/test/test_replicated_store.py:50-90.
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
    # Slow the server's outbound throughput so the clone's transfer phase
    # spans multiple seconds — without this, even 128 MiB on loopback
    # completes before we can fire the interrupt.
    server_env["LORE__SERVER__QUIC__TRANSPORT_BITS_PER_SECOND"] = str(SERVER_BPS)
    # Force aggressive store flushing so the SIGKILL doesn't take freshly
    # pushed fragments down with it. The default 10 s delay is long enough
    # that a kill within ~30 s of the push loses data that's still in
    # memory — the relaunched server then returns "Address not found"
    # rather than the production failure pattern we're after.
    server_env["LORE__IMMUTABLE_STORE__LOCAL__FLUSH_DELAY_SECONDS"] = "1"
    server_env["LORE__MUTABLE_STORE__LOCAL__FLUSH_DELAY_SECONDS"] = "1"

    server_proc, server_log_path, server_log_fd = launch_lore_server(
        server_root, server_env, lore_server_executable_path
    )
    try:
        test_remote_url = f"lore://127.0.0.1:{server_ports['quic']}/"

        # Populate the test server: create a repo, write NUM_FILES random
        # binary blobs, push.
        source = new_lore_repo(remote_url=test_remote_url)
        files = {
            f"data/file_{i:03d}.bin": os.urandom(FILE_SIZE) for i in range(NUM_FILES)
        }
        source.write_commit_push(None, files)
        logger.info(
            "Source repo %s populated with %d files (%d bytes each) and pushed to %s",
            source.name,
            NUM_FILES,
            FILE_SIZE,
            source.remote_path,
        )

        # Give the server's stores a moment to flush to disk before the kill,
        # so the relaunched server picks up every fragment. With the
        # FLUSH_DELAY_SECONDS=1 override above, a 2 s wait is sufficient.
        time.sleep(2.0)

        # Spawn a fresh clone in a subprocess. This drives the multi-file
        # sync we want to interrupt, exercising the same realize_state ->
        # read_into_file -> remote_get_retry path as branch switch.
        target_path = Path(tmp_path_factory.getbasetemp()) / f"target-{source.name}"
        target_path.mkdir(exist_ok=True)

        client_env = os.environ.copy()
        client_env["LORE_REMOTE_URL"] = test_remote_url
        client_env["LORE_GLOBAL_PATH"] = global_dir_name
        client_env.setdefault("RUST_LOG", "info")

        clone_cmd = [
            lore_executable_path,
            "repository",
            "clone",
            source.remote_path,
            str(target_path),
        ]
        logger.info("Spawning clone subprocess: %s", " ".join(clone_cmd))
        clone_proc = subprocess.Popen(
            clone_cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            env=client_env,
        )

        try:
            # Read stdout line-by-line until we see the sync marker, then
            # give parallel `get` requests a brief moment to fan out before
            # killing the server. Pattern matches the deterministic kill
            # window in scripts/test/test_manyfiles.py:73-90.
            sync_marker = SYNC_MARKER_TEMPLATE.format(n=NUM_FILES)
            captured = []
            sync_started = False
            start_deadline = time.monotonic() + 60
            assert clone_proc.stdout is not None
            while time.monotonic() < start_deadline:
                line = clone_proc.stdout.readline()
                if not line:
                    break
                captured.append(line)
                logger.info("clone> %s", line.rstrip())
                if sync_marker in line:
                    sync_started = True
                    break
            assert sync_started, (
                f"Clone did not reach {sync_marker!r} within 60s. "
                f"Captured output:\n{''.join(captured)}"
            )

            # Let parallel reads ramp up before yanking the server. Half a
            # second on a throttled link is enough for the client's
            # concurrent `get` requests to fan out against the QUIC
            # service.
            time.sleep(0.5)

            # Pull the rug. SIGKILL the server immediately; the client's
            # existing QUIC connection has no clean shutdown path.
            logger.info(
                "Force-killing server PID %s to drop QUIC connection mid-sync",
                server_proc.pid,
            )
            _force_kill_server(server_proc, server_log_fd)

            # Wait for the TCP listener sockets to actually release before
            # relaunching — launch_lore_server's _check_port_free uses
            # connect() against the HTTP/gRPC ports.
            for port_key in (
                "LORE__SERVER__HTTP__PORT",
                "LORE__SERVER__GRPC__PORT",
                "LORE__SERVER__GRPC_INTERNAL__PORT",
            ):
                _wait_for_port_free("127.0.0.1", int(server_env[port_key]))

            # Bring the server back up on the same ports with the same data
            # directory so the client's reconnect lands on a working
            # endpoint. The data is preserved on disk; any in-flight `get`
            # request that the client retries on the new connection should
            # find its fragment.
            logger.info("Relaunching server on same ports (data preserved)")
            server_proc, server_log_path, server_log_fd = launch_lore_server(
                server_root, server_env, lore_server_executable_path
            )

            # Drain the rest of the client's output and wait for exit.
            tail, _ = clone_proc.communicate(timeout=180)
            captured.append(tail)
        finally:
            if clone_proc.poll() is None:
                clone_proc.kill()
                clone_proc.wait(5)

        full_output = "".join(captured)
        logger.info("Clone exited with code %s", clone_proc.returncode)
        logger.info("Full clone output:\n%s", full_output)

        # The clone MUST recover all files despite the mid-flight server
        # restart. Before the fix this surfaced the production error
        # "Server returned error code 3" from in-flight `get` requests
        # at the moment of the QUIC reconnect, aborting the sync. The
        # fix landed in `StorageSession::invalidate` + the storage
        # layer's stale-session retry — this assertion is the
        # regression guard for it.
        assert BUG_ERROR_STRING not in full_output, (
            f"Bug still present — clone surfaced {BUG_ERROR_STRING!r}, "
            "matching the user-reported failure pattern.\n"
            f"Output:\n{full_output}"
        )
        assert clone_proc.returncode == 0, (
            f"Clone exited non-zero ({clone_proc.returncode}) after "
            f"mid-flight server restart.\nOutput:\n{full_output}"
        )
        for i in range(NUM_FILES):
            expected = target_path / "data" / f"file_{i:03d}.bin"
            assert expected.is_file() and expected.stat().st_size == FILE_SIZE, (
                f"Expected file {expected} of size {FILE_SIZE} not present "
                "after clone completed."
            )
    finally:
        _force_kill_server(server_proc, server_log_fd)
