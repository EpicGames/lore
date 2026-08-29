# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Tests for server maintenance mode (LORE_SERVER_MAINTENANCE=1).

Covers:
  - Public gRPC: maintenance listener is bound; only the environment service is
    registered (AdminService returns UNIMPLEMENTED, environment service returns
    UNAVAILABLE).
  - Public QUIC: not started in maintenance mode.
  - gRPC internal: when enabled, a maintenance listener is bound on the internal
    port
  - QUIC internal: not started in maintenance mode even when configured
"""

import logging
import socket
import subprocess
import sys
from pathlib import Path

import grpc
import pytest

from lore_server import (
    _kill_server_by_pid,
    _wait_for_grpc_port,
    _wait_for_health_check,
    allocate_free_port,
    generate_server_config,
    release_reserved_ports,
)

logger = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _launch_maintenance_server(server_root, server_env, executable_path):
    """Start a loreserver in maintenance mode and wait until its ports are ready.

    Waits for:
      - HTTP health check (always present in maintenance mode)
      - Public gRPC (always present in maintenance mode)
      - Internal gRPC when grpc_internal is enabled
        (LORE__SERVER__GRPC_INTERNAL__ENABLED=true).  Infrastructure health
        checks may probe this port to determine readiness; the wait here mirrors
        that expectation.

    Does NOT wait for any QUIC port — maintenance mode omits both the public
    and internal QUIC listeners.

    Returns (proc, log_path, log_fd).  Caller is responsible for teardown via
    _kill_server_by_pid.
    """
    server_log_path = server_root / "server.log"
    server_log_fd = server_log_path.open("w", buffering=1, encoding="utf-8")

    platform_kwargs = {}
    if sys.platform == "win32":
        platform_kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        platform_kwargs["start_new_session"] = True

    # This launches the server itself rather than going through
    # launch_lore_server, so it owns the release of the port reservations that
    # allocate_free_port is still holding.  Without it the server cannot bind,
    # and the "endpoint is not started" assertions below would see our own
    # reservation and report the port as taken.
    release_reserved_ports(server_env, label="maintenance server")

    server_proc = subprocess.Popen(
        [str(Path(executable_path).expanduser().resolve())],
        stdout=server_log_fd,
        stderr=subprocess.STDOUT,
        env=server_env,
        cwd=server_root,
        **platform_kwargs,
    )

    http_port = server_env["LORE__SERVER__HTTP__PORT"]
    grpc_port = server_env["LORE__SERVER__GRPC__PORT"]
    grpc_internal_enabled = (
        server_env.get("LORE__SERVER__GRPC_INTERNAL__ENABLED", "false").lower()
        == "true"
    )
    internal_port = server_env.get("LORE__SERVER__GRPC_INTERNAL__PORT")

    try:
        _wait_for_health_check("127.0.0.1", http_port)
        _wait_for_grpc_port("127.0.0.1", grpc_port)
        if grpc_internal_enabled and internal_port:
            _wait_for_grpc_port("127.0.0.1", internal_port)
    except Exception:
        _kill_server_by_pid(
            server_proc.pid, server_log_path, label="maintenance server"
        )
        server_log_fd.close()
        raise

    return server_proc, server_log_path, server_log_fd


def _is_tcp_port_bound(host: str, port: int, timeout: float = 1.0) -> bool:
    """Return True if a TCP listener is accepting connections on (host, port)."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(timeout)
    try:
        sock.connect((host, int(port)))
        return True
    except (ConnectionRefusedError, OSError):
        return False
    finally:
        sock.close()


def _is_udp_port_bound(host: str, port: int) -> bool:
    """Return True if a UDP listener is already bound on (host, port).

    Attempts to bind a probe socket to the same address and port.  If the
    bind succeeds the port is free; if it fails with EADDRINUSE something else
    holds it.

    This approach is portable across macOS, Linux, and Windows without
    requiring any external tools or /proc access.  It avoids the unreliable
    ICMP port-unreachable path used by UDP probes, which does not work on
    macOS loopback.
    """
    import errno

    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        probe.bind((host, int(port)))
        return False  # bind succeeded: nothing was holding the port
    except OSError as e:
        if e.errno == errno.EADDRINUSE:
            return True  # port is taken
        raise
    finally:
        probe.close()


def _grpc_status_code(
    target: str, method: str, timeout: float = 5.0
) -> grpc.StatusCode:
    """Call a gRPC method with an empty request and return the status code.

    Returns grpc.StatusCode.OK if the call succeeds without error, otherwise
    the code from the RpcError.
    """
    with grpc.insecure_channel(target) as channel:
        stub = channel.unary_unary(
            method,
            request_serializer=lambda _: b"",
            response_deserializer=lambda b: b,
        )
        try:
            stub(None, timeout=timeout)
            return grpc.StatusCode.OK
        except grpc.RpcError as exc:
            return exc.code()


# gRPC method paths used for probing server state
_ADMIN_SERVER_INFO = "/urc.rpc.AdminService/ServerInfo"
_ENV_GET = "/urc.rpc.EnvironmentService/Get"


# ---------------------------------------------------------------------------
# Public-endpoints maintenance tests
# ---------------------------------------------------------------------------


@pytest.mark.smoke
@pytest.mark.xdist_group("maintenance_mode_public")
class TestMaintenanceModePublic:
    """Maintenance mode with no internal endpoints enabled (the default config)."""

    @pytest.fixture(scope="class")
    def maintenance_server_config(self, request, tmp_path_factory):
        shared_port = allocate_free_port()
        ports = {
            "quic": shared_port,
            "grpc": shared_port,
            "http": allocate_free_port(),
            "internal": allocate_free_port(),
        }
        server_root, server_env = generate_server_config(
            request, tmp_path_factory, ports
        )
        server_env["LORE_SERVER_MAINTENANCE"] = "1"
        return server_root, server_env, ports

    @pytest.fixture(scope="class")
    def maintenance_server(
        self, maintenance_server_config, lore_server_executable_path
    ):
        server_root, server_env, _ = maintenance_server_config
        server_proc, log_path, log_fd = _launch_maintenance_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc
        _kill_server_by_pid(
            server_proc.pid, log_path, label="maintenance public server"
        )
        log_fd.close()

    def test_public_grpc_is_bound(self, maintenance_server, maintenance_server_config):
        """The public gRPC port must accept TCP connections in maintenance mode."""
        _, _, ports = maintenance_server_config
        assert _is_tcp_port_bound("127.0.0.1", ports["grpc"]), (
            f"Expected maintenance gRPC listener on port {ports['grpc']}"
        )

    def test_public_grpc_admin_service_is_not_registered(
        self, maintenance_server, maintenance_server_config
    ):
        """AdminService must not be registered on the maintenance gRPC server.

        In normal mode AdminService/ServerInfo returns OK; in maintenance mode
        only the environment service is registered, so the call returns
        UNIMPLEMENTED.  This confirms the port is serving the maintenance gRPC
        server, not the full server.
        """
        _, _, ports = maintenance_server_config
        target = f"127.0.0.1:{ports['grpc']}"
        code = _grpc_status_code(target, _ADMIN_SERVER_INFO)
        assert code == grpc.StatusCode.UNIMPLEMENTED, (
            f"Expected UNIMPLEMENTED from AdminService in maintenance mode, got {code}"
        )

    def test_public_grpc_environment_service_returns_unavailable(
        self, maintenance_server, maintenance_server_config
    ):
        """EnvironmentService/Get must return UNAVAILABLE in maintenance mode."""
        _, _, ports = maintenance_server_config
        target = f"127.0.0.1:{ports['grpc']}"
        code = _grpc_status_code(target, _ENV_GET)
        assert code == grpc.StatusCode.UNAVAILABLE, (
            f"Expected UNAVAILABLE from EnvironmentService in maintenance mode, got {code}"
        )

    def test_public_quic_is_not_started(
        self, maintenance_server, maintenance_server_config
    ):
        """The public QUIC (UDP) endpoint must not be started in maintenance mode."""
        _, _, ports = maintenance_server_config
        assert not _is_udp_port_bound("127.0.0.1", ports["quic"]), (
            f"Expected no QUIC listener on port {ports['quic']} in maintenance mode"
        )

    def test_internal_port_is_not_bound_when_disabled(
        self, maintenance_server, maintenance_server_config
    ):
        """No internal listener should be bound when grpc_internal is disabled (default)."""
        _, _, ports = maintenance_server_config
        assert not _is_tcp_port_bound("127.0.0.1", ports["internal"]), (
            f"Expected no TCP listener on internal port {ports['internal']} "
            "when grpc_internal is disabled"
        )


# ---------------------------------------------------------------------------
# gRPC-internal maintenance tests
# ---------------------------------------------------------------------------


@pytest.mark.smoke
@pytest.mark.xdist_group("maintenance_mode_grpc_internal")
class TestMaintenanceModeGrpcInternal:
    """Maintenance mode with grpc_internal enabled.

    Verifies that a maintenance gRPC server is served on the internal port
    (not a full gRPC server, and not nothing).
    """

    @pytest.fixture(scope="class")
    def maintenance_server_config(self, request, tmp_path_factory):
        shared_port = allocate_free_port()
        ports = {
            "quic": shared_port,
            "grpc": shared_port,
            "http": allocate_free_port(),
            "internal": allocate_free_port(),
        }
        server_root, server_env = generate_server_config(
            request, tmp_path_factory, ports
        )
        server_env["LORE_SERVER_MAINTENANCE"] = "1"
        server_env["LORE__SERVER__GRPC_INTERNAL__ENABLED"] = "true"
        # Disable mTLS so the test environment needs no client certs
        server_env["LORE__SERVER__GRPC_INTERNAL__VERIFY_CLIENT_CERTS"] = "false"
        return server_root, server_env, ports

    @pytest.fixture(scope="class")
    def maintenance_server(
        self, maintenance_server_config, lore_server_executable_path
    ):
        server_root, server_env, _ = maintenance_server_config
        server_proc, log_path, log_fd = _launch_maintenance_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc
        _kill_server_by_pid(
            server_proc.pid, log_path, label="maintenance grpc_internal server"
        )
        log_fd.close()

    def test_internal_grpc_is_bound(
        self, maintenance_server, maintenance_server_config
    ):
        """The internal gRPC port must be bound when grpc_internal is enabled."""
        _, _, ports = maintenance_server_config
        assert _is_tcp_port_bound("127.0.0.1", ports["internal"]), (
            f"Expected maintenance gRPC listener on internal port {ports['internal']}"
        )

    def test_internal_grpc_admin_service_is_not_registered(
        self, maintenance_server, maintenance_server_config
    ):
        """AdminService must not be registered on the internal maintenance gRPC server.

        Confirms the internal port is serving the maintenance gRPC server, not
        the full gRPC internal server.
        """
        _, _, ports = maintenance_server_config
        target = f"127.0.0.1:{ports['internal']}"
        code = _grpc_status_code(target, _ADMIN_SERVER_INFO)
        assert code == grpc.StatusCode.UNIMPLEMENTED, (
            f"Expected UNIMPLEMENTED from AdminService on internal port in "
            f"maintenance mode, got {code}"
        )


# ---------------------------------------------------------------------------
# QUIC-internal maintenance tests
# ---------------------------------------------------------------------------


@pytest.mark.smoke
@pytest.mark.xdist_group("maintenance_mode_quic_internal")
class TestMaintenanceModeQuicInternal:
    """Maintenance mode with quic_internal enabled.

    Verifies that the QUIC internal endpoint is NOT started even when
    quic_internal is configured and enabled.  Before the fix, the replication
    store service was started on the internal UDP port regardless of maintenance
    mode; this test guards against regression.
    """

    @pytest.fixture(scope="class")
    def maintenance_server_config(self, request, tmp_path_factory):
        shared_port = allocate_free_port()
        ports = {
            "quic": shared_port,
            "grpc": shared_port,
            "http": allocate_free_port(),
            "internal": allocate_free_port(),
        }
        server_root, server_env = generate_server_config(
            request, tmp_path_factory, ports
        )
        server_env["LORE_SERVER_MAINTENANCE"] = "1"
        server_env["LORE__SERVER__QUIC_INTERNAL__ENABLED"] = "true"
        # Disable mTLS so the server does not reject the config on startup
        server_env["LORE__SERVER__QUIC_INTERNAL__VERIFY_CLIENT_CERTS"] = "false"
        return server_root, server_env, ports

    @pytest.fixture(scope="class")
    def maintenance_server(
        self, maintenance_server_config, lore_server_executable_path
    ):
        server_root, server_env, _ = maintenance_server_config
        server_proc, log_path, log_fd = _launch_maintenance_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc
        _kill_server_by_pid(
            server_proc.pid, log_path, label="maintenance quic_internal server"
        )
        log_fd.close()

    def test_quic_internal_is_not_started(
        self, maintenance_server, maintenance_server_config
    ):
        """The internal QUIC (UDP) endpoint must not be started in maintenance mode.

        Even when quic_internal.enabled = true, the server must skip the
        replication store service when LORE_SERVER_MAINTENANCE=1.
        """
        _, _, ports = maintenance_server_config
        assert not _is_udp_port_bound("127.0.0.1", ports["internal"]), (
            f"Expected no QUIC listener on internal port {ports['internal']} "
            "in maintenance mode even when quic_internal is enabled"
        )
