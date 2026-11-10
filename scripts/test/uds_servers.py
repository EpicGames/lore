# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""The servers the machine socket tests run: the upstream U, a loreserver that
authenticates against the mock auth service, and the machine server M, which
serves Unix sockets with its QUIC and HTTP listeners off, keeps a local tier
over U and reaches U with a login of its own."""

import os
import socket
from pathlib import Path
from time import monotonic, sleep
from types import SimpleNamespace

from lore_server import (
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)
from mock_auth_server import MockUser
from store_layout import index_hashes
from test_auth_online import append_auth_config, login_api_key, script_api_key_login

from lore import Lore, lore_test_env

# Forwarding is by host alone, and the mock's tokens may only be sent to
# 127.0.0.1, so U goes by that name and the auth service by the other loopback
# name.
LORE_HOST = "127.0.0.1"
AUTH_HOST = "localhost"
UNIX_REMOTE = f"lore-unix://{LORE_HOST}/"

MACHINE = MockUser("mock-machine", "Mock Machine Server", "mockmachine")
MACHINE_API_KEY = "uds-machine-api-key"

# A storage client's hello, as lore-transport's `client_hello` writes it: the
# magic, version 1 and no flags. The server's hello starts with the same magic.
STORAGE_HELLO = b"LOREUDS1" + (1).to_bytes(2, "little") + bytes(6)

# Clients are told the auth service is at `localhost`, which the sockets
# forward; U's own checks reach it there over loopback.
UPSTREAM_CONFIG = """
[environment.endpoint]
auth_url = "http://{auth_host}:{auth_port}"
"""

# M's QUIC and HTTP listeners are turned off through the environment, which
# `launch_lore_server` reads to know what to wait for. Its public gRPC listener
# stays on, and the launch waits for that. A short flush delay lets a test read
# the local tier's size soon after a clone.
MACHINE_CONFIG = """
[environment.endpoint]
auth_url = "http://{auth_host}:{auth_port}"

[server]
upstream_connections = 4

[server.uds]
enabled = true
sockets = [{sockets}]

[server.uds.forward]
"{lore_host}" = "http://127.0.0.1:{upstream_port}"
"{auth_host}" = "http://127.0.0.1:{auth_port}"

[immutable_store]
mode = "composite"

[immutable_store.composite]
cache_metadata = true

[immutable_store.composite.local]
mode = "local"

[immutable_store.composite.local.local]
path = {store}
flush_delay_seconds = 1

[immutable_store.composite.durable]
mode = "remote"

[immutable_store.composite.durable.remote]
remote_url = "lore://{lore_host}:{upstream_port}"

[mutable_store]
mode = "remote"

[mutable_store.remote]
remote_url = "lore://{lore_host}:{upstream_port}"
"""


def toml_string(value) -> str:
    """`value` as a TOML basic string, in which a backslash, as in a Windows
    path, starts an escape."""
    return '"' + str(value).replace("\\", "\\\\").replace('"', '\\"') + '"'


def footprint(store: Path) -> SimpleNamespace:
    """What M's local tier holds: its indexed hashes and its files' bytes."""
    sizes = []
    for directory, _, names in os.walk(store):
        for name in names:
            # A flush may replace a file between listing and reading it.
            try:
                sizes.append(os.stat(os.path.join(directory, name)).st_size)
            except FileNotFoundError:
                pass
    return SimpleNamespace(hashes=index_hashes(str(store)), bytes=sum(sizes))


def settled_footprint(store: Path, timeout: float = 60) -> SimpleNamespace:
    """The local tier's footprint once two reads 3 seconds apart match. The tier
    flushes every second (`flush_delay_seconds` in MACHINE_CONFIG), so a match
    means no flush is pending. A test that checks the tier took in nothing new
    has no condition to poll for, so it waits for stillness instead."""
    deadline = monotonic() + timeout
    previous = footprint(store)
    while True:
        sleep(3)
        current = footprint(store)
        if (current.hashes, current.bytes) == (previous.hashes, previous.bytes):
            return current
        assert monotonic() < deadline, f"{store} kept changing for {timeout}s"
        previous = current


def answers_hello(path: Path) -> bool:
    """Whether a server answers a storage hello on the socket at `path`."""
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(5)
            connection.connect(str(path))
            connection.sendall(STORAGE_HELLO)
            return connection.recv(8, socket.MSG_WAITALL) == STORAGE_HELLO[:8]
    except OSError:
        return False


# What the server logs once every socket it serves listens.
LISTENING = "Lore socket server listening"


class MachineServer:
    """M, which a test may restart."""

    def __init__(
        self, server_root: Path, env: dict[str, str], binary: Path, sockets: list[Path]
    ):
        self.server_root = server_root
        self.env = env
        self.binary = binary
        self.sockets = sockets
        # The local tier is configured at `store`, and keeps its index here.
        self.store = server_root / "store" / "immutable"
        # Where `launch_lore_server` writes the log.
        self.log = server_root / "server.log"
        self.running = None
        self.launches = 0

    def start(self) -> None:
        # Each launch truncates the log, so the previous one is kept beside it.
        if self.launches:
            self.log.rename(self.server_root / f"server-{self.launches}.log")
        self.launches += 1
        proc, _, log_fd = launch_lore_server(self.server_root, self.env, self.binary)
        self.running = (proc, log_fd)
        # The gRPC port the launch waits for can open before the sockets do.
        deadline = monotonic() + 30
        while not self.listening():
            assert proc.poll() is None, f"M exited:\n{self.log_text()}"
            assert monotonic() < deadline, (
                f"M's sockets never listened:\n{self.log_text()}"
            )
            sleep(0.2)

    def listening(self) -> bool:
        """Whether this launch's sockets listen: each answers a storage hello,
        or, where Python has no AF_UNIX, as on Windows, the server has logged
        that its sockets listen, which it does once every one of them does. A
        socket's file is no sign: it exists from the bind, before the socket
        listens, and a connection made in between is refused."""
        if not hasattr(socket, "AF_UNIX"):
            return LISTENING in self.log_text()
        return all(answers_hello(path) for path in self.sockets)

    def stop(self) -> None:
        if self.running is not None:
            proc, log_fd = self.running
            _kill_server_by_pid(proc.pid)
            log_fd.close()
            self.running = None

    def restart(self) -> None:
        self.stop()
        self.start()

    def log_text(self) -> str:
        return self.log.read_text(encoding="utf-8", errors="replace")

    def refusals(self) -> int:
        """Storage requests this launch of M refused for want of authorization."""
        log = self.log_text()
        return sum(
            log.count(f'handler_error_label="{label}"')
            for label in ("MissingToken", "AuthorizationFailure")
        )


def _configured_server(request, tmp_path_factory, mock, config):
    """A generated server root that authenticates against `mock`, with the text
    `config` makes from the root appended to its local configuration. Returns
    the root, its environment and its gRPC port."""
    ports = {"quic": allocate_free_port(), "http": allocate_free_port()}
    ports["grpc"] = ports["quic"]
    ports["internal"] = allocate_free_port()
    server_root, server_env = generate_server_config(request, tmp_path_factory, ports)
    append_auth_config(server_root, mock)
    with open(
        server_root / "lore-server" / "config" / "local.toml", "a", encoding="utf-8"
    ) as f:
        f.write(config(server_root))
    return server_root, server_env, ports["grpc"]


def launch_upstream(request, tmp_path_factory, mock, binary):
    server_root, server_env, port = _configured_server(
        request,
        tmp_path_factory,
        mock,
        lambda _: UPSTREAM_CONFIG.format(auth_host=AUTH_HOST, auth_port=mock.grpc_port),
    )
    proc, log, log_fd = launch_lore_server(server_root, server_env, binary)
    return SimpleNamespace(
        proc=proc, log=log, log_fd=log_fd, port=port, root=server_root
    )


def log_in_machine(mock, root: Path, executable: str, upstream_url: str):
    """Logs the machine identity in to U with its API key, into a token store of
    its own under `root`, for M to reach U with. Returns its login token and the
    token store."""
    token = mock.mint_token(MACHINE)
    script_api_key_login(mock, MACHINE, token, MACHINE_API_KEY)
    auth = root / "auth" / "machine"
    auth.mkdir(parents=True)
    login_api_key(
        host_client(root, executable, auth, upstream_url), upstream_url, MACHINE_API_KEY
    )
    return SimpleNamespace(token=token, auth=auth)


def machine_server(request, tmp_path_factory, mock, binary, upstream, sockets, auth):
    server_root, server_env, _ = _configured_server(
        request,
        tmp_path_factory,
        mock,
        lambda root: MACHINE_CONFIG.format(
            auth_host=AUTH_HOST,
            auth_port=mock.grpc_port,
            lore_host=LORE_HOST,
            upstream_port=upstream.port,
            sockets=", ".join(toml_string(path) for path in sockets),
            store=toml_string(root / "store"),
        ),
    )
    server_env.update(
        {
            "LORE__SERVER__QUIC__ENABLED": "false",
            "LORE__SERVER__QUIC_INTERNAL__ENABLED": "false",
            "LORE__SERVER__HTTP__ENABLED": "false",
            # M reaches U with the login made into this store.
            "LORE_AUTH_STORE": "fallback",
            "LORE_AUTH_PATH": str(auth),
        }
    )
    server_env.pop("LORE_UDS_SOCKET", None)
    # The tests count the storage requests M refuses, which it logs at debug.
    server_env["RUST_LOG"] += ",lore_server::quic=debug"
    return MachineServer(server_root, server_env, binary, list(sockets))


def host_client(root: Path, executable: str, auth: Path, upstream_url: str, **kwargs):
    """A host client reaching U directly with the login in `auth`."""
    name = kwargs.pop("name", None) or Lore.generate_random_name("host")
    global_dir = root / "host-global"
    global_dir.mkdir(exist_ok=True)
    return Lore(
        lore_executable_path=executable,
        path=str(root / "host" / name),
        name=name,
        base_env=lore_test_env(str(global_dir)),
        environment_vars={"LORE_AUTH_PATH": str(auth)},
        remote_url=upstream_url,
        create_repo=False,
        **kwargs,
    )
