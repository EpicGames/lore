# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Smoke test for the machine server's Unix socket, `[server.uds]`, as its
proposal deploys it.

The upstream U authenticates against the mock auth service, as in
test_auth_online.py. The machine server M, whose QUIC and HTTP listeners are
off, serves one socket, keeps a local tier over U, reaches U with a login of
its own, and forwards gRPC by the host a request names, `127.0.0.1` to U and
`localhost` to the auth service. A client process that sets `LORE_UDS_SOCKET`
reaches M through that socket alone.
"""

import os
import sys
import tempfile
import uuid
from pathlib import Path
from time import monotonic, sleep
from types import SimpleNamespace

import pytest
from cleanup_util import remove_tree
from lore_server import _kill_server_by_pid
from mock_auth_server import USER1, MockAuthServer
from store_layout import index_hashes
from test_auth_online import (
    authz_resources,
    login_api_key,
    script_api_key_login,
    script_partition_access,
    script_repository_lifecycle,
)
from uds_servers import (
    LORE_HOST,
    MACHINE,
    UNIX_REMOTE,
    launch_upstream,
    log_in_machine,
    machine_server,
)

API_KEY = "uds-api-key"

FILES = {
    "readme.txt": b"cloned through the machine socket\n",
    "src/main.c": b"int main(void) { return 0; }\n",
    "assets/blob.bin": os.urandom(256 * 1024),
}


@pytest.fixture
def uds_env(
    request,
    tmp_path_factory,
    scratch_dir,
    keep_test_data,
    lore_executable_path,
    lore_server_executable_path,
):
    """U, and M logged in to U as the machine identity and serving one socket.

    Yields a namespace with the stub, U's remote URL, the socket, M and the
    machine identity's login token."""
    mock = MockAuthServer().start()
    cleanups = [mock.stop]
    try:
        upstream = launch_upstream(
            request, tmp_path_factory, mock, lore_server_executable_path
        )
        cleanups.append(upstream.log_fd.close)
        cleanups.append(lambda: _kill_server_by_pid(upstream.proc.pid))
        upstream_url = f"lore://{LORE_HOST}:{upstream.port}/"

        login = log_in_machine(
            mock,
            scratch_dir("uds-machine", create=True),
            lore_executable_path,
            upstream_url,
        )
        # A socket's path must fit in 104 bytes on macOS, whose temporary
        # directory alone takes about 50, so the socket gets a short one.
        socket_dir = Path(
            tempfile.mkdtemp(
                prefix="lore-uds-", dir=None if sys.platform == "win32" else "/tmp"
            )
        )
        if not keep_test_data:
            cleanups.append(lambda: remove_tree(socket_dir, label="socket directory"))
        socket = socket_dir / "lore.sock"
        machine = machine_server(
            request,
            tmp_path_factory,
            mock,
            lore_server_executable_path,
            upstream,
            [socket],
            login.auth,
        )
        cleanups.append(machine.stop)
        machine.start()
        yield SimpleNamespace(
            mock=mock,
            upstream_url=upstream_url,
            socket=socket,
            machine=machine,
            machine_token=login.token,
        )
    finally:
        for cleanup in reversed(cleanups):
            cleanup()


@pytest.mark.smoke
@pytest.mark.runs_in_process(
    "LORE_UDS_SOCKET routes the process that reads it, and a relayed command runs "
    "in the service"
)
def test_clone_and_push_through_the_machine_socket(uds_env, new_lore_repo, scratch_dir):
    """The owner creates a repository on U and pushes to it directly. A second
    client, with a token store of its own and LORE_UDS_SOCKET set, logs in,
    clones the `lore-unix://` remote and pushes a change, all through M: its
    auth calls reach the stub through the socket's `localhost` route, its
    repository and revision calls U through the `127.0.0.1` route, and its
    reads and writes M's local tier over U. M holds what the clone read, and
    the owner sees the change on U."""
    mock = uds_env.mock
    repo_id = uuid.uuid4().hex
    resource_id = f"urc-{repo_id}"
    login_token = mock.mint_token(USER1)
    script_api_key_login(mock, USER1, login_token, API_KEY)
    script_repository_lifecycle(mock, resource_id)
    for user, token in ((USER1, login_token), (MACHINE, uds_env.machine_token)):
        script_partition_access(
            mock,
            user,
            token,
            resource_id,
            mock.mint_token(user, resources=authz_resources(resource_id)),
        )

    def logged_in(remote_url: str, **environment_vars: str) -> dict[str, str]:
        """Logs USER1 in through `remote_url` into a token store of its own, and
        returns the environment that carries the login."""
        environment_vars["LORE_AUTH_PATH"] = str(scratch_dir("auth-store", create=True))
        login_api_key(
            new_lore_repo(create_repo=False, environment_vars=dict(environment_vars)),
            remote_url,
            API_KEY,
        )
        return environment_vars

    owner = new_lore_repo(
        remote_url=uds_env.upstream_url,
        create_repo=False,
        environment_vars=logged_in(uds_env.upstream_url),
    )
    owner.repository_create(repo_id=repo_id, identity=USER1.user_id)
    owner.write_commit_push("files to clone", FILES)

    cloner = new_lore_repo(
        remote_path=f"{UNIX_REMOTE}{owner.name}",
        create_repo=False,
        environment_vars=logged_in(UNIX_REMOTE, LORE_UDS_SOCKET=str(uds_env.socket)),
    )
    clone_path = scratch_dir("socket-clone")
    clone = cloner.clone(path=str(clone_path))
    for name, content in FILES.items():
        assert (clone_path / name).read_bytes() == content, name

    blob = bytes.fromhex(owner.file_info("assets/blob.bin", offline=True)[0].hash)
    deadline = monotonic() + 30
    while blob not in index_hashes(str(uds_env.machine.store)):
        assert monotonic() < deadline, "M's local tier must hold what the clone read"
        sleep(0.2)

    pushed = {"pushed.txt": b"pushed through the machine socket\n"}
    clone.write_commit_push("pushed through the socket", pushed)
    owner.sync()
    assert (Path(owner.path) / "pushed.txt").read_bytes() == pushed["pushed.txt"]
