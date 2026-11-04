# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Smoke test: a server without `[server.auth]` but with
`server.trust_unsigned_identity` records lock owners under the identity the
client sends."""

import pytest
from error_types import LoreException
from lore_server import allocate_free_port, generate_server_config, lore_local_server

from lore import Lore


@pytest.fixture(scope="module")
def unsigned_identity_remote_url(
    request, tmp_path_factory, lore_server_executable_path
):
    shared_port = allocate_free_port()
    ports = {
        "quic": shared_port,
        "grpc": shared_port,
        "http": allocate_free_port(),
        "internal": allocate_free_port(),
    }
    server_root, server_env = generate_server_config(request, tmp_path_factory, ports)
    server_env["LORE__SERVER__TRUST_UNSIGNED_IDENTITY"] = "true"
    server = lore_local_server(server_root, server_env, lore_server_executable_path)
    next(server)
    yield f"lore://127.0.0.1:{ports['quic']}/"
    next(server, None)


@pytest.mark.smoke
def test_lock_owner_is_the_identity_the_client_sends(
    new_lore_repo, unsigned_identity_remote_url
):
    repo: Lore = new_lore_repo(
        "UnsignedIdentity", remote_url=unsigned_identity_remote_url
    )
    repo.write_commit_push(None, {"a.txt": "a", "b.txt": "b"})

    repo.lock_acquire("a.txt", identity="alice")
    assert repo.lock_status("a.txt", identity="bob")[0].owner == "alice"
    with pytest.raises(LoreException):
        repo.lock_release("a.txt", identity="bob")
    repo.lock_release("a.txt", identity="alice")
    assert not repo.lock_query(identity="alice")

    # A client without an identity is recorded as `<unknown>`, and naming that
    # identity is how its locks are released.
    repo.lock_acquire("b.txt", identity="<unknown>")
    assert repo.lock_status("b.txt", identity="bob")[0].owner == "<unknown>"
    with pytest.raises(LoreException):
        repo.lock_release("b.txt", identity="alice")
    repo.lock_release("b.txt", identity="<unknown>")
