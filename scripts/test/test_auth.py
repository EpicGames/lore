# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging

import pytest
from error_types import NotSupportedError
from lore_ffi import NOT_AUTHENTICATED, NOT_SUPPORTED
from lore_server import (
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_auth_login_not_supported_without_auth_endpoint(new_lore_repo):
    """The local test server is authless (no auth endpoint configured), so an
    interactive `auth login` against it must fail with `NotSupported` rather
    than an opaque internal error."""

    repo: Lore = new_lore_repo()

    with pytest.raises(NotSupportedError):
        repo.run(urc_args=["auth", "login", repo.remote_path, "--no-browser"])


@pytest.mark.smoke
def test_auth_info_not_supported_without_auth_endpoint(new_lore_repo):
    """`auth info` resolves its auth endpoint from the repository's remote. The
    authless test server advertises no auth endpoint, so there is no URL to key
    a token lookup on and the command must fail with `NotSupported`."""

    repo: Lore = new_lore_repo()

    with pytest.raises(NotSupportedError):
        repo.run(urc_args=["auth", "info"])


@pytest.mark.smoke
def test_auth_user_info_not_supported_without_auth_endpoint(
    new_lore_repo, lore_library_path
):
    """`authUserInfo` (remote user-info resolution) must fail with
    `NotSupported` against the authless test server, not `NotAuthenticated`:
    the real failure is that the server has no auth endpoint at all, and
    replacing it with `NotAuthenticated` sends consumers chasing login state
    that cannot exist.

    No CLI command surfaces this call's errors (the CLI only uses it to
    decorate output with display names and deliberately ignores failures), so
    the test calls the public C API — the surface the SDK's `authUserInfo`
    binding is built on — and asserts on the returned FFI code."""

    repo: Lore = new_lore_repo()

    result = repo.auth_user_info_capi(lore_library_path, "some-other-user")

    assert result != 0, "resolving a user against an authless server must fail"
    assert result != NOT_AUTHENTICATED, (
        "the authless failure must not be masked as NotAuthenticated"
    )
    assert result == NOT_SUPPORTED, (
        f"expected NotSupported ({NOT_SUPPORTED}), got FFI code {result}"
    )


@pytest.mark.smoke
def test_auth_user_info_not_authenticated_with_auth_endpoint(
    request,
    tmp_path_factory,
    lore_server_executable_path,
    new_lore_repo,
    lore_library_path,
):
    """Counterpart to the authless test above: against a server that DOES
    advertise an auth endpoint, a logged-out `authUserInfo` must fail with
    `NotAuthenticated` — the endpoint exists, the caller just holds no token.
    Guards against the authless `NotSupported` mapping leaking into the
    authenticated case, and against the logged-out state (identity saved in
    the repository config by a previous login, tokens removed by logout)
    collapsing into an internal error.

    The session server is authless, so this test launches its own server
    instance whose advertised environment carries an auth URL (the URL is
    never contacted: the client fails at the local token lookup first). The
    repository is created offline so the first server contact is the
    `authUserInfo` call itself."""

    shared_port = allocate_free_port()
    ports = {
        "quic": shared_port,
        "grpc": shared_port,
        "http": allocate_free_port(),
        "internal": allocate_free_port(),
    }
    (server_root, server_env) = generate_server_config(request, tmp_path_factory, ports)
    server_env["LORE__ENVIRONMENT__ENDPOINT__AUTH_URL"] = (
        "https://auth.test.invalid/realms/lore"
    )
    server_proc, server_log_path, server_log_fd = launch_lore_server(
        server_root, server_env, lore_server_executable_path
    )
    try:
        repo: Lore = new_lore_repo(
            remote_url=f"lore://127.0.0.1:{shared_port}/", create_repo=False
        )
        repo.repository_create(offline=True)

        result = repo.auth_user_info_capi(lore_library_path, "some-other-user")

        assert result == NOT_AUTHENTICATED, (
            f"expected NotAuthenticated ({NOT_AUTHENTICATED}), got FFI code {result}"
        )
    finally:
        _kill_server_by_pid(server_proc.pid, server_log_path, label="auth-url server")
        server_log_fd.close()
