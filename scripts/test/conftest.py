# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import json
import logging
import os
import platform
import subprocess
import sys
from pathlib import Path
from time import sleep

import pytest

from lore import Lore
from lore_server import (
    _get_shared_tmp_dir,
    _get_worker_id,
    _XdistControllerCleanup,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
    lore_local_server,
)

logger = logging.getLogger(__name__)


def pytest_addoption(parser):
    """
    get lore server and client executable locations from command line
    """

    parser.addoption(
        "--lore-client-binary",
        action="store",
        default="release",
        help="Which version of lore client binary to use. Options include release, debug, or path to the binary file.",
    )
    parser.addoption(
        "--lore-server-binary",
        action="store",
        default="release",
        help="Which version of lore server binary to use. Options include release, debug, or path to the binary file.",
    )
    parser.addoption(
        "--test-base-directory",
        action="store",
        default=None,
        help="The directory where test agnostic/setup files are created",
    )
    parser.addoption(
        "--lore-server-hostname",
        action="store",
        default="127.0.0.1",
        help="The host name Lore Server has",
    )
    parser.addoption(
        "--lore-server-creds-key-path",
        action="store",
        default="key.pem",
        help="The path to key.pem in the test directory",
    )
    parser.addoption(
        "--lore-server-creds-cert-path",
        action="store",
        default="cert.pem",
        help="The path to cert.pem in the test directory",
    )
    parser.addoption(
        "--lore-remote-url",
        action="store",
        default=None,
        help="Which remote url to point Lore at. If unset, composed from resolved ports.",
    )
    parser.addoption(
        "--use-grpc",
        action="store_true",
        default=False,
        help="Use gRPC protocol instead of QUIC for storage operations.",
    )
    parser.addoption(
        "--lore-remote-http-port",
        action="store",
        default=None,
        help="Which remote http port to point Lore at. If unset, an OS-allocated free port is used.",
    )
    parser.addoption(
        "--lore-remote-quic-port",
        action="store",
        default=None,
        help="Which remote UDP port to point Lore at. If unset, an OS-allocated free port is used.",
    )
    parser.addoption(
        "--lore-remote-grpc-port",
        action="store",
        default=None,
        help="Which remote TCP port to point Lore at. If unset, an OS-allocated free port is used.",
    )
    parser.addoption(
        "--lore-remote-internal-port",
        action="store",
        default=None,
        help="Which remote ports to point Lore Server internal gRPC (TCP) and QUIC (UDP) endpoints at. If unset, an OS-allocated free port is used.",
    )
    parser.addoption(
        "--disable-local-server",
        action="store_true",
        default=False,
        help="Whether or not to ever run an instance of loreserver",
    )
    parser.addoption(
        "--disable-auto-server",
        action="store_true",
        default=False,
        help="Whether or not to automatically run a session instance of loreserver",
    )
    parser.addoption(
        "--lore-server-log-level",
        action="store",
        default="info",
        help="RUST_LOG level for the Lore server (e.g. debug, info, warn)",
    )


@pytest.fixture(scope="function")
def new_lore_repo(
    lore_executable_path, lore_remote_url, tmp_path_factory, global_dir_name
):
    """
    Returns a function that can be used to create a new lore repo
    """

    def _new_lore_repo(
        name=None,
        remote_path=None,
        repo_id=None,
        create_repo=True,
        remote_url=None,
        environment_vars: dict[str, str] | None = None,
    ):
        if name is None:
            name = ""
        name = Lore.generate_random_name(name)
        path = str(tmp_path_factory.getbasetemp() / name)
        return Lore(
            lore_executable_path=lore_executable_path,
            path=path,
            name=name,
            global_dir=global_dir_name,
            environment_vars=environment_vars,
            remote_path=remote_path,
            remote_url=remote_url,
            repo_id=repo_id,
            create_repo=create_repo,
        )

    return _new_lore_repo


@pytest.fixture(scope="function")
def global_dir_name(tmp_path_factory):
    path = str(
        tmp_path_factory.getbasetemp() / Lore.generate_random_name("lore_global")
    )
    logger.info(f"Setting global directory for test to {path}")
    os.makedirs(path)

    yield path


def _service_unreachable(output):
    """Whether a probe failed because it could not reach the service.

    On Windows the failure also carries WinSock error 10022, which is kept as a
    separate marker rather than relying on the wrapping context alone. It is
    scoped to Windows so that a repository path echoed back in the output cannot
    match it by accident on the other platforms.
    """
    if "connecting to local socket" in output:
        return True
    return platform.system() == "Windows" and "10022" in output


def _wait_for_service_ready(lore_executable_path, service_process, attempts=30):
    """Block until the background service answers a probe."""
    probe_env = os.environ.copy()
    probe_env["LORE_USE_SERVICE"] = "1"
    for _ in range(attempts):
        if service_process.poll() is not None:
            pytest.fail(
                "Lore service process exited during startup with code "
                f"{service_process.returncode}"
            )
        probe = subprocess.run(
            [lore_executable_path, "status"],
            capture_output=True,
            text=True,
            env=probe_env,
        )
        out = probe.stdout + probe.stderr
        if not _service_unreachable(out):
            return
        sleep(1)
    pytest.fail("Timed out waiting for Lore background service to accept connections")


@pytest.fixture(scope="function")
def background_lore_service(lore_executable_path):
    command_args = [lore_executable_path, "service", "run"]
    logger.info("Executing Lore service command: %s", command_args)
    service_process = subprocess.Popen(command_args)

    _wait_for_service_ready(lore_executable_path, service_process)

    yield service_process

    service_process.terminate()
    try:
        service_process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        logger.warning("Lore service did not exit on terminate, killing it")
        service_process.kill()


@pytest.fixture(scope="session")
def lore_executable_path(request):
    """
    Validates and returns the path of the Lore executable
    """
    executable_path = os.getenv("LORE_EXECUTABLE_PATH")
    if not executable_path:
        binary = request.config.getoption("--lore-client-binary")
        if binary in ("release", "debug"):
            executable = "lore.exe" if sys.platform == "win32" else "lore"
            executable_path = str(Path.cwd() / "target" / binary / executable)
        else:
            executable_path = binary
    executable_path = str(Path(executable_path).resolve())
    logger.debug("lore client executable path: %s", executable_path)
    if not os.path.exists(executable_path):
        pytest.exit(
            f"Lore executable at the given path: {executable_path} does not exist."
            "If you're not intending to test against locally built release binaries, "
            "set either LORE_EXECUTABLE_PATH to your Lore executable path,"
            " or pass the path via --lore-client-binary when invoking tests."
        )

    return executable_path


@pytest.fixture(scope="session")
def lore_server_executable_path(request):
    """
    Validates and returns the path of the Lore Server executable
    """
    executable_path = os.getenv("LORE_SERVER_EXECUTABLE_PATH")
    if not executable_path:
        binary = request.config.getoption("--lore-server-binary")
        if binary in ("release", "debug"):
            executable = "loreserver.exe" if sys.platform == "win32" else "loreserver"
            executable_path = str(Path.cwd() / "target" / binary / executable)
        else:
            executable_path = binary
        executable_path = str(Path(executable_path).resolve())
        logger.debug("lore server executable path: %s", executable_path)
        if not os.path.exists(executable_path):
            pytest.exit(
                f"Lore server executable at the given path: {executable_path} does not exist."
                "If you're not intending to test against locally built release binaries, "
                "set either LORE_SERVER_EXECUTABLE_PATH to your Lore executable path,"
                " or pass the path via --lore-server-binary when invoking tests."
            )

    return executable_path


@pytest.fixture(scope="session")
def lore_remote_url(request, lore_main_server_ports):
    """
    Validates and returns the Lore remote URL.
    If --lore-remote-url is set explicitly, it wins (client URL override only;
    the launched server still uses ports resolved by lore_main_server_ports).
    Otherwise the URL is composed from resolved ports. When --use-grpc is
    passed, the URL uses the grpc:// scheme and gRPC port.
    """
    override = request.config.getoption("--lore-remote-url")
    if override is not None:
        remote_url = override
    elif request.config.getoption("--use-grpc"):
        remote_url = f"grpc://127.0.0.1:{lore_main_server_ports['grpc']}"
    else:
        remote_url = f"lore://127.0.0.1:{lore_main_server_ports['quic']}"
    remote_url = remote_url if remote_url.endswith("/") else remote_url + "/"
    # TODO: Seems like having this set as an env var is required for repo creation?
    os.environ["LORE_REMOTE_URL"] = remote_url
    return remote_url


@pytest.fixture(scope="session")
def lore_main_server_ports(request, tmp_path_factory):
    """Resolve the main loreserver's {quic, grpc, http, internal} ports.

    For each port: if its CLI option was explicitly passed, use that value;
    otherwise ask the OS for a free ephemeral port. Under pytest-xdist, gw0
    resolves first and publishes the ports via lore_server_info.json so
    secondary workers connect to the ports gw0 actually launched on.
    """
    cli_values = {
        "quic": request.config.getoption("--lore-remote-quic-port"),
        "grpc": request.config.getoption("--lore-remote-grpc-port"),
        "http": request.config.getoption("--lore-remote-http-port"),
        "internal": request.config.getoption("--lore-remote-internal-port"),
    }

    def resolve_locally():
        # QUIC (UDP) and GRPC (TCP) run on the same port number by convention —
        # the protocols don't collide, and lore:// URLs in several places use
        # the GRPC__PORT env var expecting it to equal the QUIC port. Keep
        # the convention: if neither is set via CLI, allocate one port and
        # share it.
        cli_quic = cli_values["quic"]
        cli_grpc = cli_values["grpc"]
        if cli_quic is None and cli_grpc is None:
            shared = allocate_free_port()
            quic = grpc = shared
        elif cli_quic is None:
            quic = grpc = int(cli_grpc)
        elif cli_grpc is None:
            quic = grpc = int(cli_quic)
        else:
            quic = int(cli_quic)
            grpc = int(cli_grpc)
        return {
            "quic": quic,
            "grpc": grpc,
            "http": (
                int(cli_values["http"])
                if cli_values["http"] is not None
                else allocate_free_port()
            ),
            "internal": (
                int(cli_values["internal"])
                if cli_values["internal"] is not None
                else allocate_free_port()
            ),
        }

    worker_id = _get_worker_id(request)

    # Fast path: if all four ports are CLI-supplied, every worker can compute
    # the same values from CLI args alone — no cross-worker port discovery
    # needed. The server-readiness wait still happens later in
    # auto_lore_local_server, where it benefits from generate_server_config's
    # setup (key copy / openssl) as buffer time before its own polling clock
    # starts.
    if all(v is not None for v in cli_values.values()):
        if worker_id == "gw0":
            shared_tmp = _get_shared_tmp_dir(tmp_path_factory)
            (shared_tmp / "lore_server_info.json").unlink(missing_ok=True)
        return resolve_locally()

    if worker_id is None:
        return resolve_locally()

    shared_tmp = _get_shared_tmp_dir(tmp_path_factory)
    info_path = shared_tmp / "lore_server_info.json"

    if worker_id == "gw0":
        # Clear any stale info file from a killed prior session before allocating.
        info_path.unlink(missing_ok=True)
        return resolve_locally()

    # Dynamic-allocation mode: secondary workers must wait for gw0 to publish
    # the ports it actually picked, since they can't predict them.
    for _ in range(30):
        if info_path.exists():
            try:
                info = json.loads(info_path.read_text())
            except json.JSONDecodeError:
                sleep(1)
                continue
            if info.get("status") == "failed":
                pytest.fail("Lore server failed to start on gw0")
            if info.get("status") == "running" and "ports" in info:
                return info["ports"]
        sleep(1)
    pytest.fail("Timed out waiting for Lore server ports from gw0")


@pytest.fixture(scope="session")
def lore_local_server_config(request, tmp_path_factory, lore_main_server_ports):
    return generate_server_config(request, tmp_path_factory, lore_main_server_ports)


@pytest.fixture(autouse=True, scope="session")
def auto_lore_local_server(
    request,
    lore_local_server_config,
    lore_server_executable_path,
    tmp_path_factory,
    lore_main_server_ports,
):
    """
    Runs loreserver locally.

    Under pytest-xdist, gw0 launches the server and writes a status file;
    other workers block in lore_main_server_ports until that file appears.
    The xdist controller's pytest_sessionfinish hook handles teardown after
    all workers complete. Without xdist, behavior is identical to before
    (fixture owns full lifecycle).
    """
    disabled = request.config.getoption(
        "--disable-local-server"
    ) or request.config.getoption("--disable-auto-server")
    if disabled:
        yield
        return

    worker_id = _get_worker_id(request)

    if worker_id is None:
        # Not xdist — original behavior (fixture owns full lifecycle)
        (server_root, server_env) = lore_local_server_config
        yield from lore_local_server(
            server_root, server_env, lore_server_executable_path
        )
        return

    shared_tmp = _get_shared_tmp_dir(tmp_path_factory)
    info_path = shared_tmp / "lore_server_info.json"

    if worker_id == "gw0":
        # Primary: launch server, publish readiness + ports in one atomic write.
        (server_root, server_env) = lore_local_server_config
        try:
            server_proc, server_log_path, server_log_fd = launch_lore_server(
                server_root, server_env, lore_server_executable_path
            )
            info_path.write_text(
                json.dumps(
                    {
                        "status": "running",
                        "pid": server_proc.pid,
                        "log_path": str(server_log_path),
                        "ports": lore_main_server_ports,
                    }
                )
            )
        except Exception:
            info_path.write_text(json.dumps({"status": "failed"}))
            raise
        yield
        # Do NOT kill here — controller's pytest_sessionfinish handles it.
        server_log_fd.close()
    else:
        # Secondary: wait for gw0's server to be ready before tests start.
        # In dynamic-allocation mode, lore_main_server_ports already blocked
        # until ports were published, so this poll typically returns instantly.
        # In fast-path (all CLI ports set), this is the only wait — and it
        # runs *after* lore_local_server_config has done its file-copy /
        # keygen work, giving gw0 buffer time before this clock starts.
        for _ in range(30):
            if info_path.exists():
                try:
                    info = json.loads(info_path.read_text())
                except json.JSONDecodeError:
                    sleep(1)
                    continue
                if info.get("status") == "failed":
                    pytest.fail("Lore server failed to start on gw0")
                if info.get("status") == "running":
                    break
            sleep(1)
        else:
            pytest.fail("Timed out waiting for Lore server to start on gw0")
        yield


def pytest_configure(config):
    """Register the xdist controller cleanup plugin early so its
    pytest_sessionfinish hook fires on the controller process."""
    config.pluginmanager.register(_XdistControllerCleanup(), "lore_xdist_cleanup")
