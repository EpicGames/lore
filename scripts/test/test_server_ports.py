# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""The ports the harness hands a server stay free from their release to the
server's bind."""

import contextlib
import socket
import struct
import subprocess
import sys
from pathlib import Path

import lore_server
import pytest
from lore_server import (
    _SCAN_RANGE_START,
    _SERVER_PORT_KEYS,
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
    release_reserved_ports,
)

# The socket option of <linux/in.h> that narrows the range the OS picks the
# socket's local port from, Linux 6.3 and later.
_IP_LOCAL_PORT_RANGE = 51


def _can_narrow_local_port_range() -> bool:
    with socket.socket() as probe:
        try:
            probe.setsockopt(socket.IPPROTO_IP, _IP_LOCAL_PORT_RANGE, b"\0" * 4)
        except OSError:
            return False
    return True


def _connect_from(port: int, connections: contextlib.ExitStack) -> None:
    """Make an outgoing loopback connection, held by `connections`. The OS picks
    its local port from `port` alone if its ephemeral range holds `port`, and
    from the whole range otherwise."""
    listener = connections.enter_context(socket.create_server(("127.0.0.1", 0)))
    connection = connections.enter_context(socket.socket())
    only_port = struct.pack("I", port | port << 16)
    connection.setsockopt(socket.IPPROTO_IP, _IP_LOCAL_PORT_RANGE, only_port)
    connection.connect(listener.getsockname())


def _release(port: int) -> None:
    """Release `port` as a launch on it does."""
    release_reserved_ports({key: str(port) for key, _ in _SERVER_PORT_KEYS})


@pytest.mark.smoke
@pytest.mark.skipif(sys.platform != "linux", reason="IP_LOCAL_PORT_RANGE is Linux's")
def test_outgoing_connection_after_the_release_leaves_the_server_its_port(
    request, tmp_path_factory, lore_server_executable_path, monkeypatch
):
    """An outgoing connection made between a launch's release of its ports and
    the server's bind, steered to the server's HTTP port wherever the OS may
    assign that port, leaves the server free to bind it."""
    if not _can_narrow_local_port_range():
        pytest.skip("the kernel predates IP_LOCAL_PORT_RANGE")
    shared_port = allocate_free_port()
    ports = {
        "quic": shared_port,
        "grpc": shared_port,
        "http": allocate_free_port(),
        "internal": allocate_free_port(),
    }
    server_root, server_env = generate_server_config(request, tmp_path_factory, ports)
    with contextlib.ExitStack() as connections:

        def release_then_connect(env, label=""):
            release_reserved_ports(env, label)
            _connect_from(ports["http"], connections)

        monkeypatch.setattr(lore_server, "release_reserved_ports", release_then_connect)
        server_proc, _, server_log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
    _kill_server_by_pid(server_proc.pid)
    server_log_fd.close()


@pytest.mark.smoke
def test_allocated_port_is_outside_the_ephemeral_range():
    """A released port in the ephemeral range can go to any outgoing connection
    or bind to port 0 on the machine before the server binds it."""
    port = allocate_free_port()
    _release(port)
    ranges = lore_server._ephemeral_port_ranges()
    assert ranges, "the OS's ephemeral port ranges could not be read"
    assert not any(low <= port <= high for low, high in ranges), (
        f"port {port} is in the ephemeral ranges {ranges}"
    )


@pytest.mark.smoke
def test_released_port_is_skipped_by_another_harness_process():
    """Another harness process, scanning from a released port, takes another
    one. The child starts its scan there through `random.randrange`, and fails
    if the scan stops calling it."""
    port = allocate_free_port()
    _release(port)
    offset = port - _SCAN_RANGE_START
    allocate_from_port = (
        "import random, lore_server\n"
        "starts = []\n"
        f"random.randrange = lambda span: starts.append(span) or {offset}\n"
        "allocated = lore_server.allocate_free_port()\n"
        "assert starts, 'the scan no longer starts at random.randrange'\n"
        "print(allocated)\n"
    )
    child = subprocess.run(
        [sys.executable, "-c", allocate_from_port],
        cwd=Path(__file__).parent,
        capture_output=True,
        text=True,
        check=False,
    )
    assert child.returncode == 0, child.stderr
    assert int(child.stdout) != port
