# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import base64
import json
import logging
from pathlib import Path

import pytest
from cryptography.hazmat.primitives.asymmetric import ec
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
def test_auth_user_info_answers_from_the_token_only_service_without_auth_endpoint(
    new_lore_repo, lore_library_path
):
    """`authUserInfo` (remote user-info resolution) succeeds against the
    authless test server. With no auth service and no directory advertised
    there is nothing to exchange a token with and nothing to ask, so the
    token-only service answers: every ID is its own display name. Failing
    here, with `NotSupported` or worse `NotAuthenticated`, would send SDK
    consumers chasing login state that cannot exist on this server.

    No CLI command surfaces this call's result directly (the CLI only uses it
    to decorate output with display names), so the test calls the public C
    API — the surface the SDK's `authUserInfo` binding is built on — and
    asserts on the returned FFI code."""

    repo: Lore = new_lore_repo()

    result = repo.auth_user_info_capi(lore_library_path, "some-other-user")

    assert result == 0, (
        f"the token-only service answers an authless server; got FFI code {result}"
    )


def _write_throwaway_jwks(path: Path) -> None:
    """A JWKS with one freshly generated EC public key. The server insists on
    at least one usable signing key at startup. No token is ever minted
    against it, so the private half is dropped on the floor."""

    def b64url(n: int) -> str:
        return base64.urlsafe_b64encode(n.to_bytes(32, "big")).rstrip(b"=").decode()

    numbers = ec.generate_private_key(ec.SECP256R1()).public_key().public_numbers()
    path.write_text(
        json.dumps(
            {
                "keys": [
                    {
                        "kty": "EC",
                        "crv": "P-256",
                        "kid": "throwaway-test-key",
                        "alg": "ES256",
                        "use": "sig",
                        "x": b64url(numbers.x),
                        "y": b64url(numbers.y),
                    }
                ]
            }
        ),
        encoding="utf-8",
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
    never contacted: the client fails at the local token lookup first, and
    the server's OIDC discovery against it is lazy — no token ever arrives
    to verify). The repository is created offline so the first server
    contact is the `authUserInfo` call itself.

    `auth_url` without `[server.auth]` is a config error (tokens would go
    unverified), so the server also gets a `[server.auth]` block naming the
    same never-contacted issuer. Both go in `local.toml` following the
    pattern in test_forwarded_requests.py. The server fetches its JWKS
    eagerly at startup and refuses to start without a usable signing key,
    so the block points `[server.auth.jwk]` at a `file://` JWKS holding a
    throwaway public key that never verifies anything."""

    shared_port = allocate_free_port()
    ports = {
        "quic": shared_port,
        "grpc": shared_port,
        "http": allocate_free_port(),
        "internal": allocate_free_port(),
    }
    (server_root, server_env) = generate_server_config(request, tmp_path_factory, ports)
    auth_url = "https://auth.test.invalid/realms/lore"
    jwks_path = server_root / "jwks.json"
    _write_throwaway_jwks(jwks_path)
    with open(
        server_root / "lore-server" / "config" / "local.toml",
        "a",
        encoding="utf-8",
    ) as f:
        f.write("[environment.endpoint]\n")
        f.write(f'auth_url = "{auth_url}"\n')
        f.write("[server.auth]\n")
        f.write(f'jwt_issuer = "{auth_url}"\n')
        f.write('jwt_audience = ["lore-service"]\n')
        f.write("[server.auth.jwk]\n")
        f.write(f'endpoint = "{jwks_path.as_uri()}"\n')
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
