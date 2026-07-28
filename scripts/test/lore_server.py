# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import http.client
import json
import logging
import os
import signal
import shutil
import socket
import subprocess
import sys
from pathlib import Path
from time import sleep

from error_types import ServerException

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


class _XdistControllerCleanup:
    """Pytest plugin registered on the xdist controller to kill the shared
    Lore server after all workers complete.  Registered via pytest_configure
    so it is guaranteed to run on the controller process."""

    @staticmethod
    def pytest_sessionfinish(session, exitstatus):
        # Only run on the xdist controller, not on workers or non-xdist runs
        if hasattr(session.config, "workerinput"):
            return
        if not session.config.pluginmanager.has_plugin("dsession"):
            return

        basetemp = session.config._tmp_path_factory.getbasetemp()
        info_path = basetemp / "lore_server_info.json"
        if not info_path.exists():
            return

        info = json.loads(info_path.read_text())
        if info.get("status") != "running":
            return

        pid = info["pid"]
        log_path = Path(info["log_path"])
        _kill_server_by_pid(pid, log_path, label="xdist controller")


# ---------------------------------------------------------------------------
# Server operations
# ---------------------------------------------------------------------------


def allocate_free_port(host: str = "127.0.0.1") -> int:
    """Ask the OS for a loopback port free for both TCP and UDP.

    gRPC (TCP) and QUIC (UDP) share one port number, so a TCP-only probe is
    not enough: on Windows a TCP-free port can be reserved for UDP, failing
    the QUIC bind with WSAEACCES. We pick a TCP port and confirm the same
    number is UDP-bindable, retrying otherwise.
    """
    assert host == "127.0.0.1", (
        f"allocate_free_port only supports 127.0.0.1, got {host!r}"
    )
    last_err: OSError | None = None
    for _ in range(20):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as tcp:
            tcp.bind((host, 0))
            port = tcp.getsockname()[1]
            udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            try:
                udp.bind((host, port))
            except OSError as e:
                last_err = e
                continue
            finally:
                udp.close()
            return port
    raise ServerException(
        f"Could not find a port free for both TCP and UDP on {host} "
        f"after 20 attempts; last UDP bind error: {last_err}"
    )


def generate_server_config(request, tmp_path_factory, ports: dict):
    def copy_server_configs(base_dir: Path, dest_dir: Path) -> None:
        cfg_src = base_dir / "lore-server" / "config"
        cfg_dst = dest_dir / "lore-server" / "config"
        cfg_dst.mkdir(parents=True, exist_ok=True)
        for name in ("default.toml", "gha.toml"):
            shutil.copy2(cfg_src / name, cfg_dst / name)

    def copy_server_keys(key_dir, server_dir):
        shutil.copy2(
            key_dir / request.config.getoption("--lore-server-creds-key-path"),
            server_dir / "key.pem",
        )
        shutil.copy2(
            key_dir / request.config.getoption("--lore-server-creds-cert-path"),
            server_dir / "cert.pem",
        )

    test_base_directory = request.config.getoption("--test-base-directory")
    if test_base_directory is None:
        test_base_directory = Path.cwd()
    else:
        test_base_directory = Path(test_base_directory)

    server_root = tmp_path_factory.mktemp("lore-server")
    server_root.mkdir(parents=True, exist_ok=True)

    copy_server_configs(test_base_directory, server_root)
    try:
        copy_server_keys(test_base_directory, server_root)
    except Exception as e:
        logger.error(
            f"Could not copy openssl keys: {e}, generating keys and trying again"
        )
        generate_ssl_cert()
        copy_server_keys(test_base_directory, server_root)

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
            "LORE_ENV": "gha",
        }
    )

    return server_root, server_env


def launch_lore_server(server_root, server_env, executable_path):
    server_log_path = server_root / "server.log"
    server_log_fd = server_log_path.open("w", buffering=1, encoding="utf-8")

    server_name = f"Local Lore Server Quic:{server_env['LORE__SERVER__QUIC__PORT']}  GRPC: {server_env['LORE__SERVER__GRPC__PORT']}"

    print()
    print(f"Launching server '{server_name}' in '{server_root}'")

    # Fail fast if something is already listening on any of our ports
    for port_key in (
        "LORE__SERVER__HTTP__PORT",
        "LORE__SERVER__GRPC__PORT",
        "LORE__SERVER__QUIC__PORT",
        "LORE__SERVER__QUIC_INTERNAL__PORT",
        "LORE__SERVER__GRPC_INTERNAL__PORT",
    ):
        _check_port_free(
            "127.0.0.1", server_env[port_key], label=f"{server_name} ({port_key})"
        )

    http_port = server_env["LORE__SERVER__HTTP__PORT"]

    server_binary_path: Path = Path(executable_path).expanduser().resolve(strict=False)

    platform_kwargs = {}
    if sys.platform == "win32":
        platform_kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        platform_kwargs["start_new_session"] = True

    server_proc = subprocess.Popen(
        [str(server_binary_path)],
        stdout=server_log_fd,
        stderr=subprocess.STDOUT,
        env=server_env,
        cwd=server_root,
        **platform_kwargs,
    )

    # Wait for the server to be ready via health check instead of a blind sleep
    quic_port = server_env["LORE__SERVER__QUIC__PORT"]
    grpc_port = server_env["LORE__SERVER__GRPC__PORT"]
    try:
        _wait_for_health_check("127.0.0.1", http_port)
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


def _check_port_free(host, port, label=""):
    """Verify that nothing is already listening on the given port.

    Raises ServerException if a connection succeeds, which indicates a stale
    server (or some other process) is occupying the port.
    """
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(2)
    try:
        sock.connect((host, int(port)))
        sock.close()
        raise ServerException(
            f"Port {port} is already in use before launching {label}. "
            "A stale server process may be running from a previous session."
        )
    except (ConnectionRefusedError, OSError):
        pass  # Port is free — expected
    finally:
        sock.close()


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


# TODO(jamie): This should be converted to portable python code instead of using a subprocess
def generate_ssl_cert():
    import platform
    import subprocess

    current_os = platform.system()
    if current_os == "Darwin":
        config = ["-config", "/System/Library/OpenSSL/openssl.cnf"]
    else:
        config = []

    cmd = [
        "openssl",
        "req",
        *config,
        "-x509",
        "-newkey",
        "rsa:4096",
        "-keyout",
        "key.pem",
        "-out",
        "cert.pem",
        "-sha256",
        "-days",
        "3650",
        "-nodes",
        "-subj",
        "/C=XX/ST=NC/L=Cary/O=EpicGames/OU=UCS/CN=URCQD",
    ]

    subprocess.check_call(cmd)
