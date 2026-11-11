# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import ctypes
import functools
import http.client
import json
import logging
import os
import random
import re
import signal
import shutil
import socket
import subprocess
import sys
import tempfile
from pathlib import Path
from time import sleep

import pytest
from cleanup_util import remove_tree, remove_tree_contents
from error_types import ServerException

if sys.platform == "win32":
    import msvcrt
else:
    import fcntl

logger = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# Server lifecycle
# ---------------------------------------------------------------------------


def lore_local_server(server_root, server_env, executable_path):
    server_proc, server_log_path, server_log_fd = launch_lore_server(
        server_root, server_env, executable_path
    )

    yield

    # Server teardown
    _kill_server_by_pid(server_proc.pid, server_log_path, label="local server")
    server_log_fd.close()


def _kill_shared_xdist_server(basetemp: Path) -> None:
    """Kill the server one worker launched for every worker to share, if it is
    running."""
    info_path = basetemp / "lore_server_info.json"
    if not info_path.exists():
        return

    info = json.loads(info_path.read_text())
    if info.get("status") != "running":
        return

    pid = info["pid"]
    log_path = Path(info["log_path"])
    _kill_server_by_pid(pid, log_path, label="xdist controller")


class _SessionCleanup:
    """Pytest plugin that stops the shared Lore server and removes the session's
    test data once every test has finished.  Registered via pytest_configure so
    the hook is guaranteed to run on the xdist controller, which also stops the
    server in a cleanup of its configuration: a session-finish hook that raises,
    as xdist's does on reaching a worker that is gone, skips the hooks after it,
    and cleanups run regardless.

    The sweep lives here rather than in a session fixture because the controller
    is the only process that runs after all workers are done. A worker finishing
    its own tests says nothing about the others, and the launching worker's
    basetemp holds the store of the server every worker is still talking to.
    """

    @staticmethod
    def pytest_sessionstart(session):
        if hasattr(session.config, "workerinput"):
            return
        factory = session.config._tmp_path_factory
        session.config.add_cleanup(
            lambda: _kill_shared_xdist_server(factory.getbasetemp())
        )

    @staticmethod
    @pytest.hookimpl(trylast=True)
    def pytest_sessionfinish(session, exitstatus):
        # trylast, so this runs after _pytest.runner's own pytest_sessionfinish.
        # On a normal run the session fixtures are already finalized by then --
        # the last item's teardown does it -- but on an interrupted or
        # collection-error run that hook is what finalizes them, and it is
        # registered earlier than this plugin, so without trylast the sweep
        # would delete the session server's root while the server was still up.
        #
        # An xdist worker owns none of this: the controller sweeps every
        # worker's basetemp below, once they have all stopped.
        if hasattr(session.config, "workerinput"):
            return

        basetemp = session.config._tmp_path_factory.getbasetemp()

        if session.config.pluginmanager.has_plugin("dsession"):
            _kill_shared_xdist_server(basetemp)

        if session.config.getoption("--keep-test-data"):
            logger.info("Leaving test data in %s (--keep-test-data)", basetemp)
            return

        # Backstop for everything the per-test fixtures could not take: the
        # server roots and logs that had to outlive the tests using them, any
        # path a test made for itself, and anything a locked file left behind.
        # A run without xdist arrives here with its server already stopped,
        # since session fixtures finalize before this hook.
        remove_tree_contents(basetemp, label="session test data")


# ---------------------------------------------------------------------------
# Server operations
# ---------------------------------------------------------------------------


# Ports this process has handed out, whether or not a server has bound them
# yet. Entries are never removed: a released reservation is still spoken for by
# the server about to launch on it, so it must not be offered again.
_handed_out_ports: set[int] = set()

# Ports handed out whose TCP+UDP probe sockets are still held open, keyed by
# port number. Holding the sockets turns the probe into a real reservation: the
# fixtures allocate their ports up front and then spend a while writing configs
# before any server binds, and the OS refuses the number to anyone else — other
# xdist workers included — for that whole window. `release_reserved_ports`
# hands the port back immediately before the launch.
_reserved_ports: dict[int, tuple[socket.socket, socket.socket]] = {}

# The ports `allocate_free_port` hands out: below the default ephemeral range of
# Linux (from 32768) and of macOS and Windows (from 49152), so the scan skips
# none of them as ephemeral by default, and above the ports common services
# listen on.
_SCAN_RANGE_START = 20000
_SCAN_RANGE_END = 32000


@functools.cache
def _excluded_port_ranges() -> tuple[tuple[int, int], ...]:
    """Port ranges the OS refuses to bind, best effort (Windows only).

    Hyper-V, WSL and Docker reserve blocks of ports through winnat; binding
    inside one fails with WSAEACCES (WinError 10013) even though nothing is
    listening. netsh reports them, which lets the scan skip whole blocks
    instead of probing every port in them. If netsh fails or its output does
    not parse, the scan probes every port.
    """
    if sys.platform != "win32":
        return ()

    ranges: list[tuple[int, int]] = []
    for protocol in ("tcp", "udp"):
        try:
            result = subprocess.run(
                [
                    "netsh",
                    "interface",
                    "ipv4",
                    "show",
                    "excludedportrange",
                    f"protocol={protocol}",
                ],
                capture_output=True,
                text=True,
                timeout=10,
            )
        except (OSError, subprocess.SubprocessError) as e:
            logger.warning("Could not query excluded %s port ranges: %s", protocol, e)
            continue
        # Data rows are two integers; headers and footnotes never parse as such.
        for line in result.stdout.splitlines():
            fields = line.split()
            if len(fields) == 2 and all(field.isdigit() for field in fields):
                start, end = int(fields[0]), int(fields[1])
                if 0 < start <= end <= 65535:
                    ranges.append((start, end))

    if ranges:
        logger.info("Skipping %d OS-excluded port ranges", len(ranges))
    return tuple(ranges)


@functools.cache
def _ephemeral_port_ranges() -> tuple[tuple[int, int], ...]:
    """Port ranges the OS assigns a port from on its own, to a bind to port 0 or
    an outgoing connection, best effort.

    Read from /proc on Linux, from sysctl on macOS and from netsh on Windows. If
    the read fails, there are none, and the scan relies on the default ranges
    lying above it.
    """
    try:
        if sys.platform == "linux":
            shown = Path("/proc/sys/net/ipv4/ip_local_port_range").read_text()
        elif sys.platform == "darwin":
            shown = subprocess.run(
                ["sysctl", "-n"]
                + [
                    f"net.inet.ip.portrange.{key}"
                    for key in ("first", "last", "hifirst", "hilast")
                ],
                capture_output=True,
                text=True,
                timeout=10,
                check=True,
            ).stdout
        elif sys.platform == "win32":
            ranges = []
            for protocol in ("tcp", "udp"):
                shown = subprocess.run(
                    ["netsh", "interface", "ipv4", "show", "dynamicport", protocol],
                    capture_output=True,
                    text=True,
                    timeout=10,
                    check=True,
                ).stdout
                start, count = (int(n) for n in re.findall(r":\s*(\d+)", shown))
                ranges.append((start, start + count - 1))
            return tuple(ranges)
        else:
            return ()
        bounds = [int(n) for n in shown.split()]
        return tuple(zip(bounds[::2], bounds[1::2], strict=True))
    except (OSError, subprocess.SubprocessError, ValueError) as e:
        logger.warning("Could not read the ephemeral port ranges: %s", e)
        return ()


def _is_excluded_port(port: int) -> bool:
    """Whether the scan skips `port`: the OS refuses to bind it, or may assign it
    on its own."""
    ranges = (*_excluded_port_ranges(), *_ephemeral_port_ranges())
    return any(start <= port <= end for start, end in ranges)


def _reserve_port(
    host: str, port: int
) -> tuple[socket.socket, socket.socket] | OSError:
    """Bind `port` for both TCP and UDP; the held sockets, or the bind error.

    gRPC (TCP) and QUIC (UDP) share one port number, so a TCP-only probe is not
    enough: on Windows a TCP-free port can be reserved for UDP, failing the
    QUIC bind with WSAEACCES. Both sockets are bound at the same time so a port
    that is only free for one protocol at a time is rejected.

    On success the caller owns both sockets and must keep them open until the
    server is about to bind the port, then close them.
    """
    tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        _set_exclusive_addr(tcp, udp)
        tcp.bind((host, port))
        udp.bind((host, port))
    except OSError as e:
        tcp.close()
        udp.close()
        return e
    return tcp, udp


def _set_exclusive_addr(*socks: socket.socket) -> None:
    """Refuse ports another socket already holds with SO_REUSEADDR (Windows).

    Without this a probe bind can succeed on a port that is already in use,
    and the server's own bind is the one that fails.
    """
    if sys.platform != "win32":
        return
    for sock in socks:
        try:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
        except (AttributeError, OSError):
            pass


@functools.cache
def _port_lock_fd() -> int:
    """The descriptor of this user's port lock file, open for the life of the
    process.

    Each harness process locks the byte at a port's offset in the file before
    it probes the port. The OS releases a process's locks when it exits,
    however it exits. On POSIX, closing any descriptor of the file also
    releases them, so the process opens the file once. The file is never
    removed: a run that recreated it would not see the locks of a run still
    holding the old one.

    On POSIX the file is in /tmp, named for the user's ID: a process can lock
    only a file it can open for writing, and /tmp holds it whatever the
    process's temporary directory is. On Windows it is in the user's temporary
    directory.
    """
    if sys.platform == "win32":
        path = Path(tempfile.gettempdir()) / "lore-smoke-ports.lock"
    else:
        path = Path("/tmp") / f"lore-smoke-ports-{os.getuid()}.lock"
    return os.open(path, os.O_RDWR | os.O_CREAT, 0o600)


def _lock_port(port: int) -> bool:
    """Take this process's lock on `port`, held until the process exits; False
    if the port is locked already: by another process, or on Windows by this
    one. The locked byte lies past the end of the empty lock file, which both
    `fcntl.lockf` and `msvcrt.locking` allow."""
    fd = _port_lock_fd()
    try:
        if sys.platform == "win32":
            os.lseek(fd, port, os.SEEK_SET)
            msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
        else:
            fcntl.lockf(fd, fcntl.LOCK_EX | fcntl.LOCK_NB, 1, port)
    except (BlockingIOError, PermissionError):
        return False
    return True


def allocate_free_port(host: str = "127.0.0.1") -> int:
    """Reserve a loopback port free for both TCP and UDP.

    The port comes from a scan of `_SCAN_RANGE_START` to `_SCAN_RANGE_END`,
    from a randomized offset so concurrent workers don't converge on one port.
    Ports this process already handed out, ports the OS refuses to bind or may
    assign on its own, and ports another harness process has locked are
    skipped without a probe.

    The returned port stays bound by this process until the server that will
    use it is launched, so nothing else can claim it in the meantime. Callers
    that launch through `launch_lore_server` get that release for free;
    anything launching a server itself must call `release_reserved_ports`.

    Between that release and the server's bind, only a process outside the
    harness that binds the number explicitly can take the port: the OS assigns
    no port outside its ephemeral ranges on its own, to a bind to port 0 or to
    an outgoing connection, and this process holds the port's lock until it
    exits, so no other harness process of the same user probes it.
    """
    assert host == "127.0.0.1", (
        f"allocate_free_port only supports 127.0.0.1, got {host!r}"
    )
    probes = 0
    last_err: OSError | None = None
    span = _SCAN_RANGE_END - _SCAN_RANGE_START
    start_offset = random.randrange(span)
    for step in range(span):
        port = _SCAN_RANGE_START + (start_offset + step) % span
        if port in _handed_out_ports or _is_excluded_port(port) or not _lock_port(port):
            continue
        probes += 1
        reserved = _reserve_port(host, port)
        if isinstance(reserved, OSError):
            last_err = reserved
            continue
        _handed_out_ports.add(port)
        _reserved_ports[port] = reserved
        return port

    raise ServerException(
        f"Could not find a port free for both TCP and UDP on {host} in "
        f"{_SCAN_RANGE_START}-{_SCAN_RANGE_END - 1} after {probes} probes, "
        f"skipping {len(_excluded_port_ranges())} OS-excluded ranges, the "
        f"ephemeral ranges {_ephemeral_port_ranges()} and the ports other "
        f"harness processes hold; last bind error: {last_err}"
    )


# Endpoint port variables, and whether the endpoint listens on UDP. The two
# internal endpoints deliberately share one number: gRPC internal speaks TCP,
# QUIC internal speaks UDP.
_SERVER_PORT_KEYS = (
    ("LORE__SERVER__HTTP__PORT", False),
    ("LORE__SERVER__GRPC__PORT", False),
    ("LORE__SERVER__QUIC__PORT", True),
    ("LORE__SERVER__GRPC_INTERNAL__PORT", False),
    ("LORE__SERVER__QUIC_INTERNAL__PORT", True),
)


def _server_ports(server_env) -> dict[int, tuple[str, bool]]:
    """Map each distinct port in `server_env` to a naming key and whether any
    endpoint on it needs UDP. Endpoints sharing a number collapse to one entry
    and the UDP requirement is the union, so a shared port is never checked as
    TCP-only just because the TCP endpoint came first."""
    ports: dict[int, tuple[str, bool]] = {}
    for port_key, udp in _SERVER_PORT_KEYS:
        port = int(server_env[port_key])
        label, needs_udp = ports.get(port, (port_key, False))
        ports[port] = (label, needs_udp or udp)
    return ports


def release_reserved_ports(server_env, label: str = "") -> None:
    """Hand this server's ports back to the OS immediately before it launches.

    Ports come from `allocate_free_port`, which keeps them bound so no other
    process can take the number while the caller writes configs. The server
    cannot bind a port we are still holding, so the reservation is dropped
    here, immediately before the launch. `allocate_free_port` describes what
    keeps the port free from here to the server's bind.

    A port with no reservation was either supplied on the command line or
    already released by an earlier launch on the same ports, so it is probed
    instead: nothing has been holding it and a stale server may own it.
    """
    for port, (port_key, udp) in _server_ports(server_env).items():
        reserved = _reserved_ports.pop(port, None)
        if reserved is None:
            _check_port_free("127.0.0.1", port, label=f"{label} ({port_key})", udp=udp)
            continue
        for sock in reserved:
            sock.close()


def generate_server_config(
    request, tmp_path_factory, ports: dict, *, remove_when_done: bool = True
):
    def copy_server_configs(base_dir: Path, dest_dir: Path) -> None:
        cfg_src = base_dir / "lore-server" / "config"
        cfg_dst = dest_dir / "lore-server" / "config"
        cfg_dst.mkdir(parents=True, exist_ok=True)
        for name in ("default.toml", "gha.toml"):
            shutil.copy2(cfg_src / name, cfg_dst / name)

    test_base_directory = request.config.getoption("--test-base-directory")
    if test_base_directory is None:
        test_base_directory = Path.cwd()
    else:
        test_base_directory = Path(test_base_directory)

    server_root = tmp_path_factory.mktemp("lore-server")
    server_root.mkdir(parents=True, exist_ok=True)

    if remove_when_done and not request.config.getoption("--keep-test-data"):
        # A server store is the largest thing a test writes, so it is removed at
        # the scope of whoever asked for the config: a class-scoped server goes
        # when its class ends, one built inside a test body when that test ends,
        # pass or fail. The server is always stopped first -- the fixture that
        # launched it depends on this one, so it finalizes first, and a
        # generator fixture's own teardown is registered after this finalizer
        # and therefore runs before it.
        #
        # The session-wide server opts out: under xdist it outlives every
        # worker, so the controller's sweep is the only safe place to take it.
        request.addfinalizer(lambda: remove_tree(server_root, label="server root"))

    copy_server_configs(test_base_directory, server_root)

    rust_log = request.config.getoption("--lore-server-log-level")

    server_env = os.environ.copy()
    server_env.update(
        {
            "RUST_LOG": rust_log,
            "RUST_BACKTRACE": "1",
            "LORE__SERVER__QUIC__PORT": str(ports["quic"]),
            # QUIC internal runs on the same port as gRPC internal (UDP vs TCP)
            "LORE__SERVER__QUIC_INTERNAL__PORT": str(ports["internal"]),
            "LORE__SERVER__GRPC__PORT": str(ports["grpc"]),
            "LORE__SERVER__GRPC_INTERNAL__PORT": str(ports["internal"]),
            "LORE__SERVER__HTTP__PORT": str(ports["http"]),
            # Bind loopback rather than the shipped 0.0.0.0. Ports are reserved
            # on 127.0.0.1, so this makes the reservation cover exactly the
            # address the server goes on to bind — a port free on loopback but
            # taken on another interface would otherwise pass the reservation
            # and then fail the server's wildcard bind. It also keeps a test
            # server off the machine's other interfaces.
            "LORE__SERVER__QUIC__HOST": "127.0.0.1",
            "LORE__SERVER__QUIC_INTERNAL__HOST": "127.0.0.1",
            "LORE__SERVER__GRPC__HOST": "127.0.0.1",
            "LORE__SERVER__GRPC_INTERNAL__HOST": "127.0.0.1",
            "LORE__SERVER__HTTP__HOST": "127.0.0.1",
            "LORE_ENV": "gha",
        }
    )

    return server_root, server_env


# `prctl` option naming the signal the kernel sends a process when its parent dies.
PR_SET_PDEATHSIG = 1

# Resolved here rather than in the child, which runs between fork and exec while
# the suite's other threads may hold the locks a lookup takes.
_prctl = ctypes.CDLL(None, use_errno=True).prctl if sys.platform == "linux" else None


def _stop_with_parent():
    """Has the kernel send this process SIGTERM when the thread that started it
    exits."""
    _prctl(PR_SET_PDEATHSIG, signal.SIGTERM)


def ends_with_launcher() -> dict:
    """`Popen` arguments that stop the process when the one launching it ends,
    however it ends, so that an aborted run leaves nothing behind. Linux only,
    and empty elsewhere.

    The kernel acts on the exit of the launching thread, so a process launched
    this way must be launched from a thread that lives as long as it should.
    """
    return {"preexec_fn": _stop_with_parent} if _prctl else {}


# Stops a server once the process running the session ends, given that process's
# PID and the server's. Process file descriptors rather than PIDs, so that a PID
# reused once either process is gone is never signalled: `pidfd_open` (434) and
# `pidfd_send_signal` (424) by syscall number, the same on every Linux
# architecture, since the interpreters the suite runs under can lack
# `os.pidfd_open`.
_SESSION_WATCHDOG = """
import ctypes, select, signal, sys
syscall = ctypes.CDLL(None, use_errno=True).syscall
session, server = (syscall(434, int(pid), 0) for pid in sys.argv[1:])
if min(session, server) >= 0 and server not in select.select([session, server], [], [])[0]:
    syscall(424, server, signal.SIGTERM, None, 0)
    if not select.select([server], [], [], 10)[0]:
        syscall(424, server, signal.SIGKILL, None, 0)
"""


def ends_with_session(server_pid: int):
    """Stops the server `server_pid` when the process running the session ends,
    however it ends, for the server every xdist worker shares: it outlives the
    worker that launched it, so `ends_with_launcher` cannot tie it to that
    worker. A worker's parent is the process running the session. Linux only.
    """
    if not _prctl:
        return
    subprocess.Popen(
        [sys.executable, "-c", _SESSION_WATCHDOG, str(os.getppid()), str(server_pid)],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )


def server_popen_kwargs(outlives_launcher: bool = False) -> dict:
    """`Popen` arguments for a server: a session of its own, so that a Ctrl+C at
    the terminal reaches only the harness, which stops the server, and unless
    `outlives_launcher`, `ends_with_launcher`."""
    kwargs = {} if outlives_launcher else ends_with_launcher()
    if sys.platform == "win32":
        kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        kwargs["start_new_session"] = True
    return kwargs


def launch_lore_server(
    server_root, server_env, executable_path, outlives_launcher: bool = False
):
    """Starts a server and waits until it serves.

    `outlives_launcher` keeps it running when the process launching it ends, for
    the server every xdist worker shares: the worker that launches it can finish
    before the others, and the run stops the server once they all have.
    """
    server_log_path = server_root / "server.log"
    server_log_fd = server_log_path.open("w", buffering=1, encoding="utf-8")

    server_name = f"Local Lore Server Quic:{server_env['LORE__SERVER__QUIC__PORT']}  GRPC: {server_env['LORE__SERVER__GRPC__PORT']}"

    print()
    print(f"Launching server '{server_name}' in '{server_root}'")

    http_port = server_env["LORE__SERVER__HTTP__PORT"]

    server_binary_path: Path = Path(executable_path).expanduser().resolve(strict=False)

    release_reserved_ports(server_env, label=server_name)

    server_proc = subprocess.Popen(
        [str(server_binary_path)],
        stdout=server_log_fd,
        stderr=subprocess.STDOUT,
        env=server_env,
        cwd=server_root,
        **server_popen_kwargs(outlives_launcher),
    )

    quic_port = server_env["LORE__SERVER__QUIC__PORT"]
    grpc_port = server_env["LORE__SERVER__GRPC__PORT"]
    http_enabled = (
        server_env.get("LORE__SERVER__HTTP__ENABLED", "true").lower() != "false"
    )
    quic_enabled = (
        server_env.get("LORE__SERVER__QUIC__ENABLED", "true").lower() != "false"
    )
    try:
        if http_enabled:
            _wait_for_health_check("127.0.0.1", http_port)
        if quic_enabled:
            _wait_for_quic_port("127.0.0.1", quic_port)
        _wait_for_grpc_port("127.0.0.1", grpc_port)
    except ServerException:
        if server_proc.returncode is not None:
            print(
                f"Server {server_name} failed to start (exited with {server_proc.returncode}):"
            )
        else:
            print(f"Server {server_name} not responding to health checks:")
        print(server_log_path.read_text(encoding="utf-8", errors="ignore"))
        raise

    if server_proc.returncode is not None:
        print(f"Server {server_name} failed to start:")
        print(server_log_path.read_text(encoding="utf-8", errors="ignore"))

        raise ServerException(f"Server {server_name} failed to start")

    return server_proc, server_log_path, server_log_fd


def _kill_server_by_pid(
    pid: int, log_path: Path | None = None, label: str = ""
) -> None:
    """Kill a server process by PID. Safe to call multiple times."""
    if sys.platform == "win32":
        _kill_server_by_pid_windows(pid, log_path, label)
    else:
        _kill_server_by_pid_unix(pid, log_path, label)


def _kill_server_by_pid_windows(
    pid: int, log_path: Path | None = None, label: str = ""
) -> None:
    """Kill a server process tree on Windows using taskkill."""
    result = subprocess.run(
        ["tasklist", "/FI", f"PID eq {pid}", "/NH"],
        capture_output=True,
        text=True,
    )
    if str(pid) not in result.stdout:
        return  # already dead

    if label:
        print(f"\n\nCleaning up server ({label})")

    subprocess.run(
        ["taskkill", "/F", "/T", "/PID", str(pid)],
        capture_output=True,
    )

    if log_path and log_path.exists():
        print("Server log:")
        print(log_path.read_text(encoding="utf-8", errors="ignore"))


def _kill_server_by_pid_unix(
    pid: int, log_path: Path | None = None, label: str = ""
) -> None:
    """Kill a server process group on Unix. Safe to call multiple times."""
    try:
        os.kill(pid, 0)  # check if process exists
    except ProcessLookupError:
        return  # already dead
    except PermissionError:
        pass  # exists but we might not be able to query — try to kill anyway

    if label:
        print(f"\n\nCleaning up server ({label})")

    try:
        # Kill the process group (since we use start_new_session=True)
        os.killpg(pid, signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
        try:
            os.kill(pid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            pass

    sleep(5)

    try:
        os.killpg(pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        try:
            os.kill(pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass

    if log_path and log_path.exists():
        print("Server log:")
        print(log_path.read_text(encoding="utf-8", errors="ignore"))


# ---------------------------------------------------------------------------
# Utilities
# ---------------------------------------------------------------------------


def _get_worker_id(request) -> str | None:
    """Return xdist worker id, or None if not running under xdist."""
    if hasattr(request.config, "workerinput"):
        return request.config.workerinput["workerid"]
    return None


def _get_shared_tmp_dir(tmp_path_factory) -> Path:
    """Return temp directory shared across all xdist workers.
    Must only be called when running under xdist.
    Under xdist each worker's basetemp is a subdirectory of the controller's
    basetemp (e.g. .../pytest-NNN/popen-gw0/), so .parent is the shared root."""
    return tmp_path_factory.getbasetemp().parent


def _check_port_free(host, port, label="", udp=False):
    """Verify that the given port is usable before we launch on it.

    Used for ports fixed on the command line, which nothing has been holding —
    a stale server from a previous session, or any other process, may own one.
    Raises ServerException if a TCP connection succeeds, which means something
    is already listening. With `udp`, also confirms the number is UDP-bindable
    — the only way to catch an unusable QUIC port, since there is nothing to
    connect to over UDP.
    """
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(2)
    try:
        sock.connect((host, int(port)))
        in_use = True
    except (ConnectionRefusedError, OSError):
        in_use = False  # Port is free — expected
    finally:
        sock.close()

    if in_use:
        raise ServerException(
            f"Port {port} is already in use before launching {label}. "
            "A stale server process may be running from a previous session."
        )

    if not udp:
        return

    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    _set_exclusive_addr(probe)
    try:
        probe.bind((host, int(port)))
    except OSError as e:
        raise ServerException(
            f"UDP port {port} is not bindable before launching {label}: {e}. "
            "Another process may have taken it, or the OS reserved the range "
            "since the port was allocated."
        ) from e
    finally:
        probe.close()


def _wait_for_health_check(host, port, retries=10, delay=1):
    """Poll the server's /health_check endpoint until it responds 200.

    Raises ServerException if the server does not become healthy within the
    retry window.
    """
    for attempt in range(retries):
        try:
            conn = http.client.HTTPConnection(host, int(port), timeout=2)
            conn.request("GET", "/health_check")
            response = conn.getresponse()
            conn.close()
            if response.status == 200:
                logger.info(
                    "Server health check passed on attempt %d (port %s)",
                    attempt + 1,
                    port,
                )
                return
            logger.warning(
                "Server health check returned %d on attempt %d",
                response.status,
                attempt + 1,
            )
        except Exception:
            pass
        sleep(delay)

    raise ServerException(
        f"Server on port {port} did not pass health check after {retries} attempts. "
        "The launched server may have failed to bind or crashed silently."
    )


def _wait_for_quic_port(host, port, retries=10, delay=0.5):
    """Poll until the QUIC (UDP) port is bound and listening.

    The HTTP health check can pass before the QUIC listener is ready,
    causing the first Lore command to get Connection Refused.
    """
    for attempt in range(retries):
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.settimeout(1)
        try:
            # Send a dummy datagram — if the port is not bound the OS
            # replies with ICMP port-unreachable, which surfaces as a
            # ConnectionRefusedError on the next recv.
            sock.sendto(b"\x00", (host, int(port)))
            sock.recvfrom(1)
        except ConnectionRefusedError:
            # Port not bound yet
            sleep(delay)
            continue
        except (socket.timeout, OSError):
            # Timeout means the packet was accepted (no ICMP reject) —
            # the QUIC server is listening but didn't reply to garbage.
            logger.info(
                "QUIC port %s ready on attempt %d",
                port,
                attempt + 1,
            )
            return
        finally:
            sock.close()

    raise ServerException(
        f"QUIC port {port} did not become ready after {retries} attempts."
    )


def _wait_for_grpc_port(host, port, retries=20, delay=0.5):
    """Poll until the gRPC (TCP) port accepts connections.

    gRPC shares the QUIC port number but listens over TCP, so it is a separate
    listener that can bind slightly later than the HTTP health check and the
    QUIC (UDP) port both pass. A gRPC operation issued in that window — e.g.
    `repository create`, used to set up the topology fixtures — would otherwise
    hit a transport error. A successful TCP connect confirms the listener is
    accepting; this races most under parallel workers.
    """
    for attempt in range(retries):
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(1)
        try:
            sock.connect((host, int(port)))
            logger.info(
                "gRPC port %s ready on attempt %d",
                port,
                attempt + 1,
            )
            return
        except (ConnectionRefusedError, OSError):
            # Listener not bound yet
            sleep(delay)
        finally:
            sock.close()

    raise ServerException(
        f"gRPC port {port} did not become ready after {retries} attempts."
    )
