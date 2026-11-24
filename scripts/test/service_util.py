# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import contextlib
import logging
import os
import platform
import socket
import struct
import subprocess
import sys
from pathlib import Path

logger = logging.getLogger(__name__)


LORE_SERVICE_ENVIRONMENT = {"LORE_USE_SERVICE": "1"}

# `LoreError::ServiceUnavailable`: what a command carried out by the service
# reports when none could be reached or started, so it is distinct from the
# command itself having run and failed. Paired with the value in
# `lore-revision/src/interface.rs` and has to change with it.
SERVICE_UNAVAILABLE = 32


def name_service_executable(
    env: dict[str, str], lore_executable_path: str
) -> dict[str, str]:
    """Names the executable under test as the service.

    Commands relaying to a service need an executable to start one when none is
    running, and `service start` and a virtual clone's commands start the named
    one in place of the `lore` that ran them. Setting it here rather than in
    each test ensures no test can leave it out, and that the service serving a
    test is always the build being tested rather than whichever Lore is
    installed on the machine running the suite.

    A test that names one itself keeps it.
    """
    if not env.get("LORE_SERVICE_EXECUTABLE"):
        env["LORE_SERVICE_EXECUTABLE"] = lore_executable_path
    return env


# Names the socket a service listens on. Set once per test run so the suite gets
# a service of its own: without it every service on a machine answers on the same
# per-user socket, so a run would stop a service the developer is using, and two
# runs could not proceed at once.
LORE_SERVICE_SOCKET_VAR = "LORE_SERVICE_SOCKET"

# Names the socket of a service started before the run, which then carries out
# every command of every test that neither manages a service of its own nor is
# marked `runs_in_process`. See `docs/developing/code-standards/testing.md`.
LORE_TEST_SERVICE_SOCKET_VAR = "LORE_TEST_SERVICE_SOCKET"

# Set to have the suite start that service itself, from the build under test,
# and stop it when the run ends.
LORE_TEST_SHARED_SERVICE_VAR = "LORE_TEST_SHARED_SERVICE"

# Names the VFS, such as `fuse`, of every repository a test creates or clones where
# the test names none. A virtual clone is mounted by the service, so a run setting
# it takes one too. See `docs/developing/code-standards/testing.md`.
LORE_TEST_VFS_VAR = "LORE_TEST_VFS"

# Lines the client prints about the service. They are matched rather than
# parsed, so they are kept here next to each other: each one pairs with a string
# in the Rust sources and has to be changed with it.
#
# `lore service run` prints this once it has bound its socket.
LORE_SERVICE_LISTENING_MESSAGE = "Lore service listening"
# `lore service run` exits with this when another service has claimed the socket.
LORE_SERVICE_CLAIM_REFUSED_MESSAGE = "Another Lore service has claimed the socket"
# `lore service stop` prints this when there was no service to stop.
LORE_NO_SERVICE_MESSAGE = "No Lore service is running"
# `lore service start` prints this once a service is reachable.
LORE_SERVICE_RUNNING_MESSAGE = "Lore service is running"
# Printed by the `service` setters when the two settings relaying needs are left
# in a state that will not relay.
LORE_NO_SERVICE_EXECUTABLE_MESSAGE = "No service executable is set"
# Headings `lore service status` prints for the metadata of a running service.
# Only the labels, because the values are the machine's rather than the suite's.
LORE_SERVICE_STATUS_LABELS = (
    "Executable:",
    "Uptime:",
    "Connections:",
    "SWFS mounts:",
    "Process ID:",
    "Runs inside another program:",
    "Mounted virtual instances:",
    "Global data directories of the mounts:",
)
# Headings `lore service status` prints for the external directories of the
# caller's global data directory, whether or not a service is running.
LORE_EXTERNAL_ROOT_STATUS_LABELS = (
    "External directories:",
    "State:",
    "Old directories to remove:",
)


def service_supported():
    return platform.system() in ("Windows", "Linux", "Darwin")


def service_socket_path(socket_name: str) -> Path:
    """The socket a service named `socket_name` listens on. Paired with
    `uds_sock_dir` in `lore/src/remote/network/unix.rs` and `uds_sock_path` in
    `lore/src/remote/network/windows.rs`."""
    if sys.platform == "win32":
        return _windows_temp_path() / socket_name
    base = os.environ.get("XDG_RUNTIME_DIR") or os.environ.get("TMPDIR") or "/tmp"
    return Path(base) / f"lore-{os.getuid()}" / socket_name


def service_claim_path(socket_name: str) -> Path:
    """The lock file a service claims `socket_name` with, beside the socket."""
    socket_path = service_socket_path(socket_name)
    return socket_path.with_name(f"{socket_path.name}.lock")


def _windows_temp_path() -> Path:
    """The directory `GetTempPathW` names, where the Windows service socket
    lies. Asked of Windows rather than read from the environment, whose
    variables it consults in an order of its own."""
    import ctypes

    length = ctypes.windll.kernel32.GetTempPathW(0, None)
    buffer = ctypes.create_unicode_buffer(length)
    ctypes.windll.kernel32.GetTempPathW(length, buffer)
    return Path(buffer.value)


def remove_service_claim(socket_name: str):
    """Removes the lock file a service leaves beside the socket when it exits.

    Left in place on Windows while a service still holds it, which cannot
    delete a file another process has open: the lock outlives that service as
    it would on Unix, and the run carries on."""
    with contextlib.suppress(PermissionError):
        service_claim_path(socket_name).unlink(missing_ok=True)


def stop_lore_service(lore_executable_path: str, global_dir_name: str) -> str:
    """Stops the running service, if there is one, and reports the output.

    A test that starts a service without a process of its own to end it - by
    running a command that starts one - stops it through this. The command
    returns once the socket is free, so nothing has to wait afterwards.
    """
    env = os.environ.copy()
    env["LORE_GLOBAL_PATH"] = global_dir_name
    env["LORE_AUTH_PATH"] = global_dir_name
    stop = subprocess.run(
        [lore_executable_path, "service", "stop"],
        capture_output=True,
        text=True,
        env=env,
    )
    output = stop.stdout + stop.stderr
    logger.info("Stopping Lore service: %s", output)
    return output


# The service's wire protocol, as `lore/src/remote/message.rs` writes it, for a
# test to speak to a service, or to a client, of another build. A frame is the
# protocol version, the payload's size as a little-endian u32, the frame's kind,
# then the payload. Each value pairs with one there and has to change with it.
MESSAGE_PROTOCOL = 3
FRAME_MESSAGE = 0
FRAME_HELLO = 1
FRAME_REFUSAL = 2
FRAME_STOP = 3
FRAME_STOP_RESULT = 4
_FRAME_HEADER = struct.Struct("<BIB")


def write_frame(connection: socket.socket, kind: int, payload: bytes):
    """Writes one frame of `kind` carrying `payload`."""
    connection.sendall(
        _FRAME_HEADER.pack(MESSAGE_PROTOCOL, len(payload), kind) + payload
    )


def write_hello(connection: socket.socket, to_server: int, to_client: int):
    """Writes a hello carrying the schemas of a build's two message types."""
    write_frame(connection, FRAME_HELLO, struct.pack("<QQ", to_server, to_client))


def read_frame(connection: socket.socket) -> tuple[int, bytes] | None:
    """Reads one frame as its kind and payload, or `None` where the peer closed
    the connection before it, as a reset reads too."""
    try:
        header = _read_exactly(connection, _FRAME_HEADER.size)
    except (ConnectionResetError, ConnectionAbortedError):
        return None
    if header is None:
        return None
    protocol, size, kind = _FRAME_HEADER.unpack(header)
    assert protocol == MESSAGE_PROTOCOL, f"a frame of protocol {protocol}"
    payload = _read_exactly(connection, size)
    assert payload is not None, "the connection closed inside a frame"
    return kind, payload


def _read_exactly(connection: socket.socket, size: int) -> bytes | None:
    """Reads `size` bytes, or `None` where the connection closes before the first."""
    data = b""
    while len(data) < size:
        chunk = connection.recv(size - len(data))
        if not chunk:
            assert not data, "the connection closed inside a frame"
            return None
        data += chunk
    return data


def uds_connect(path: Path) -> socket.socket:
    """A connection to the Unix domain socket at `path`."""
    if sys.platform != "win32":
        connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        connection.connect(str(path))
        return connection
    return _winsock_uds(path, "connect")


def uds_listen(path: Path) -> socket.socket:
    """A socket listening at `path`, whose connections `uds_accept` takes."""
    if sys.platform != "win32":
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(path))
    else:
        listener = _winsock_uds(path, "bind")
    listener.listen()
    return listener


def uds_accept(listener: socket.socket) -> socket.socket:
    """The next connection to `listener`, waiting for one."""
    if sys.platform != "win32":
        return listener.accept()[0]
    # Through Winsock, because Python's `accept` can't read an address of a
    # family it wasn't built with.
    handle = _winsock().accept(listener.fileno(), None, None)
    if handle == _INVALID_SOCKET:
        raise OSError(_ctypes().get_last_error(), "accepting a connection")
    return socket.socket(fileno=handle)


# Windows has Unix domain sockets, but Python's `socket` module on Windows is
# built without `AF_UNIX`: a socket is made and addressed through Winsock, then
# wrapped, which its reads and writes work on.
_WINDOWS_AF_UNIX = 1
_INVALID_SOCKET = (1 << 64) - 1


def _ctypes():
    import ctypes

    return ctypes


def _winsock():
    ctypes = _ctypes()
    winsock = ctypes.WinDLL("ws2_32", use_last_error=True)
    winsock.socket.restype = ctypes.c_size_t
    winsock.socket.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_int]
    for addressed in (winsock.connect, winsock.bind):
        addressed.argtypes = [ctypes.c_size_t, ctypes.c_void_p, ctypes.c_int]
    winsock.accept.restype = ctypes.c_size_t
    winsock.accept.argtypes = [ctypes.c_size_t, ctypes.c_void_p, ctypes.c_void_p]
    winsock.closesocket.argtypes = [ctypes.c_size_t]
    return winsock


def _winsock_uds(path: Path, operation: str) -> socket.socket:
    """A Winsock Unix domain socket that `operation`, `connect` or `bind`, has
    addressed to `path`."""
    ctypes = _ctypes()

    class SockaddrUn(ctypes.Structure):
        _fields_ = (
            ("sun_family", ctypes.c_ushort),
            ("sun_path", ctypes.c_char * 108),
        )

    winsock = _winsock()
    handle = winsock.socket(_WINDOWS_AF_UNIX, socket.SOCK_STREAM, 0)
    if handle == _INVALID_SOCKET:
        raise OSError(ctypes.get_last_error(), "creating a socket")
    address = SockaddrUn(_WINDOWS_AF_UNIX, os.fsencode(path))
    if getattr(winsock, operation)(
        handle, ctypes.byref(address), ctypes.sizeof(address)
    ):
        error = ctypes.get_last_error()
        winsock.closesocket(handle)
        raise OSError(error, f"{operation} to {path}")
    return socket.socket(fileno=handle)
