# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Smoke tests for `lore auth login` over the OIDC path: the device
authorization grant (RFC 8628) against a stub provider.

The module launches one lore server that advertises an OIDC issuer and no
gRPC auth service, so every client takes the OIDC path. `[server.auth]` names
the stub as `jwt_issuer`, which the server resolves through the stub's
discovery document at startup, and `[server.auth.oidc]` names the public
client the CLI presents. The stub (see mock_oidc_provider.py) serves
discovery, the device authorization endpoint, the token endpoint and a JWKS,
and mints RS256 access tokens.

What this ticket can get wrong is the shape of the two requests and the
mapping of the token endpoint's answers, both asserted on the wire here. The
same login against a real provider is the fixture repository's job.
"""

import logging
import os
import socket
import stat
import sys
import time
from pathlib import Path
from types import SimpleNamespace

import pytest
from error_types import LoreException, NotAuthenticatedError
from lore_server import (
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)
from mock_oidc_provider import (
    ACCESS_TOKEN_TYPE,
    API_KEY_TOKEN_TYPE,
    CLIENT_CREDENTIALS_GRANT,
    DEVICE_CODE_GRANT,
    KEY_USER,
    PAIRWISE_USER,
    REFRESH_TOKEN_GRANT,
    SERVICE_ACCOUNT,
    TOKEN_EXCHANGE_GRANT,
    MockOidcProvider,
    OidcUser,
)

from lore import Lore

logger = logging.getLogger(__name__)

pytestmark = pytest.mark.xdist_group("auth_oidc")


OIDC_CONFIG = """

# --- appended by test_auth_oidc.py: an OIDC issuer and no gRPC auth service ---
[server.auth]
jwt_issuer = "{issuer}"
jwt_audience = ["{audience}"]

[server.auth.oidc]
client_id = "{client_id}"
scopes = ["openid"]
"""


@pytest.fixture(scope="module")
def oidc_env(request, tmp_path_factory, lore_server_executable_path):
    """A lore server advertising the stub as its OIDC provider."""
    provider = MockOidcProvider().start()

    shared_port = allocate_free_port()
    ports = {
        "quic": shared_port,
        "grpc": shared_port,
        "http": allocate_free_port(),
        "internal": allocate_free_port(),
    }
    server_root, server_env = generate_server_config(request, tmp_path_factory, ports)
    config_path = server_root / "lore-server" / "config" / "gha.toml"
    config_path.write_text(
        config_path.read_text()
        + OIDC_CONFIG.format(
            issuer=provider.issuer,
            audience=provider.audience[0],
            client_id=provider.client_id,
        )
    )
    server_proc, server_log_path, server_log_fd = launch_lore_server(
        server_root, server_env, lore_server_executable_path
    )
    try:
        yield SimpleNamespace(
            provider=provider,
            remote_url=f"lore://127.0.0.1:{shared_port}/",
            server_log=server_log_path,
        )
    finally:
        _kill_server_by_pid(server_proc.pid, server_log_path, label="auth-oidc server")
        server_log_fd.close()
        provider.stop()


@pytest.fixture(autouse=True)
def _reset_provider(oidc_env):
    oidc_env.provider.reset()


@pytest.fixture
def make_repo(oidc_env, new_lore_repo, scratch_dir):
    """A repository against the OIDC server, with a token store of its own."""

    def _make_repo(**environment_vars: str) -> Lore:
        auth_store = scratch_dir("auth-store", create=True)
        environment_vars = {"LORE_AUTH_PATH": str(auth_store), **environment_vars}
        return new_lore_repo(
            remote_url=oidc_env.remote_url,
            create_repo=False,
            environment_vars=environment_vars,
        )

    return _make_repo


def assert_device_grant_on_the_wire(provider: MockOidcProvider, polls: int) -> None:
    """One device authorization request naming the client and the scopes,
    then `polls` token requests carrying the device code grant."""
    device = provider.requests_to("/device")
    assert len(device) == 1
    assert device[0].form == {
        "client_id": provider.client_id,
        "scope": "openid offline_access",
    }
    token = provider.requests_to("/token")
    assert len(token) == polls, [r.form.get("grant_type") for r in token]
    for request in token:
        assert request.form == {
            "grant_type": DEVICE_CODE_GRANT,
            "device_code": provider.device_code,
            "client_id": provider.client_id,
        }
        assert "authorization" not in request.headers


def assert_user_logged_in(repo: Lore, provider: MockOidcProvider) -> None:
    """The access token is stored under the user its `sub` names, at the
    endpoint the OIDC path is keyed by."""
    listing = repo.run(urc_args=["auth", "list"])
    assert f"oidc+{provider.issuer}" in listing
    assert provider.user.user_id in listing


@pytest.mark.smoke
def test_login_without_a_browser_completes_the_device_grant(oidc_env, make_repo):
    """`auth login --no-browser` prints the verification URL and the code,
    polls the token endpoint through `authorization_pending` and `slow_down`,
    and stores the access token under the user the token names."""
    provider = oidc_env.provider
    provider.script_polls(["authorization_pending", "slow_down"])
    repo = make_repo()

    output = repo.run(urc_args=["auth", "login", oidc_env.remote_url, "--no-browser"])

    assert "Login at:" in output
    assert provider.verification_uri_complete in output
    assert f"Code: {provider.user_code}" in output
    assert "Authentication successful" in output
    assert_device_grant_on_the_wire(provider, polls=3)
    assert_user_logged_in(repo, provider)


@pytest.mark.smoke
@pytest.mark.skipif(
    sys.platform != "linux", reason="Stubs the browser through xdg-open"
)
@pytest.mark.runs_in_process(
    "stubs the browser on the PATH of the process carrying out the login, which a relayed "
    "call does not carry"
)
def test_login_with_a_browser_opens_the_verification_url(
    oidc_env, make_repo, scratch_dir
):
    """`auth login` hands `verification_uri_complete` to the browser, here an
    `xdg-open` on PATH that records what it was asked to open, and completes
    the same grant."""
    provider = oidc_env.provider
    browser_dir = scratch_dir("browser", create=True)
    opened = Path(browser_dir) / "opened-url"
    xdg_open = Path(browser_dir) / "xdg-open"
    xdg_open.write_text(f'#!/bin/sh\nprintf \'%s\' "$1" > "{opened}"\n')
    xdg_open.chmod(xdg_open.stat().st_mode | stat.S_IXUSR)
    repo = make_repo(PATH=f"{browser_dir}{os.pathsep}{os.environ['PATH']}")

    output = repo.run(urc_args=["auth", "login", oidc_env.remote_url])

    assert opened.read_text() == provider.verification_uri_complete
    # The code is shown so the user can confirm it on the page (RFC 8628
    # §3.3.1); the URL went to the browser rather than the terminal.
    assert f"Code: {provider.user_code}" in output
    assert "Authentication successful" in output
    assert_device_grant_on_the_wire(provider, polls=1)
    assert_user_logged_in(repo, provider)


@pytest.mark.smoke
def test_a_declined_login_stops_polling_at_once(oidc_env, make_repo):
    """`access_denied` ends the login as `NotAuthenticated` on the first poll
    that reports it, rather than polling on until the session expires."""
    provider = oidc_env.provider
    provider.script_polls(["access_denied"])
    repo = make_repo()

    with pytest.raises(NotAuthenticatedError):
        repo.run(urc_args=["auth", "login", oidc_env.remote_url, "--no-browser"])

    assert_device_grant_on_the_wire(provider, polls=1)
    assert provider.user.user_id not in repo.run(urc_args=["auth", "list"])


def login_with_token(repo: Lore, remote_url: str, token_type: str, token: str) -> str:
    return repo.run(
        urc_args=[
            "auth",
            "login",
            remote_url,
            "--token-type",
            token_type,
            "--token",
            token,
        ]
    )


@pytest.mark.smoke
@pytest.mark.parametrize("token_type", ["api-key", API_KEY_TOKEN_TYPE])
def test_an_api_key_login_is_a_token_exchange(oidc_env, make_repo, token_type):
    """`--token-type` reaches the token endpoint verbatim as
    `subject_token_type`, the short name and the URI alike, with no device
    grant, and the token of the user the key is bound to is stored."""
    provider = oidc_env.provider
    provider.api_keys["oidc-api-key"] = KEY_USER
    repo = make_repo()

    output = login_with_token(repo, oidc_env.remote_url, token_type, "oidc-api-key")

    assert "Authentication successful" in output
    assert not provider.requests_to("/device")
    assert [r.form for r in provider.requests_to("/token")] == [
        {
            "grant_type": TOKEN_EXCHANGE_GRANT,
            "client_id": provider.client_id,
            "subject_token": "oidc-api-key",
            "subject_token_type": token_type,
            "requested_token_type": ACCESS_TOKEN_TYPE,
        }
    ]
    listing = repo.run(urc_args=["auth", "list"])
    assert f"oidc+{provider.issuer}" in listing
    assert KEY_USER.user_id in listing


@pytest.mark.smoke
def test_an_unknown_api_key_is_refused(oidc_env, make_repo):
    """The provider's `invalid_grant` for a key it does not know fails the
    login as `NotAuthenticated`, and nothing is stored."""
    repo = make_repo()

    with pytest.raises(NotAuthenticatedError):
        login_with_token(repo, oidc_env.remote_url, "api-key", "not-a-real-key")

    assert len(oidc_env.provider.requests_to("/token")) == 1
    assert "oidc+" not in repo.run(urc_args=["auth", "list"])


@pytest.mark.smoke
def test_a_lore_token_is_stored_without_an_exchange(oidc_env, make_repo):
    """`--token-type lore` validates the token and stores it, asking the
    provider nothing."""
    provider = oidc_env.provider
    repo = make_repo()

    output = login_with_token(
        repo, oidc_env.remote_url, "lore", provider.mint_token(KEY_USER)
    )

    assert "Authentication successful" in output
    assert not provider.requests
    assert KEY_USER.user_id in repo.run(urc_args=["auth", "list"])


CLIENT_ID = "ci-bot"
CLIENT_SECRET = "ci-secret-do-not-log"
LITERAL_WARNING = "is visible to other users in the process table"


def login_as_client(repo: Lore, remote_url: str, *args: str, **kwargs) -> str:
    return repo.run(
        urc_args=["auth", "login", remote_url, "--client-id", CLIENT_ID, *args],
        **kwargs,
    )


@pytest.mark.smoke
@pytest.mark.parametrize(
    "methods", [None, ["client_secret_post"]], ids=["basic", "post"]
)
def test_a_client_login_takes_the_secret_from_the_environment(
    oidc_env, make_repo, methods
):
    """`--client-id` with the secret in `LORE_CLIENT_SECRET` sends the
    client credentials grant, with the credentials where the discovery
    document's auth methods name, Basic when it names none. The token of the
    user the client logs in as is stored, and the secret reaches neither the
    output nor the server log, even at trace level."""
    provider = oidc_env.provider
    provider.clients[CLIENT_ID] = (CLIENT_SECRET, SERVICE_ACCOUNT)
    provider.token_endpoint_auth_methods = methods
    repo = make_repo(LORE_CLIENT_SECRET=CLIENT_SECRET)

    output = login_as_client(repo, oidc_env.remote_url, level="trace")

    assert "Authentication successful" in output
    assert LITERAL_WARNING not in output
    assert CLIENT_SECRET not in output
    assert CLIENT_SECRET not in Path(oidc_env.server_log).read_text(errors="replace")
    assert not provider.requests_to("/device")
    [request] = provider.requests_to("/token")
    if methods is None:
        assert request.form == {"grant_type": CLIENT_CREDENTIALS_GRANT}
        assert request.headers["authorization"].startswith("Basic ")
    else:
        assert request.form == {
            "grant_type": CLIENT_CREDENTIALS_GRANT,
            "client_id": CLIENT_ID,
            "client_secret": CLIENT_SECRET,
        }
        assert "authorization" not in request.headers
    listing = repo.run(urc_args=["auth", "list"])
    assert f"oidc+{provider.issuer}" in listing
    assert SERVICE_ACCOUNT.user_id in listing


@pytest.mark.smoke
def test_a_client_secret_on_the_command_line_warns(oidc_env, make_repo):
    """A literal `--client-secret` still logs in, and warns that the secret
    is exposed."""
    oidc_env.provider.clients[CLIENT_ID] = (CLIENT_SECRET, SERVICE_ACCOUNT)
    repo = make_repo()

    output = login_as_client(
        repo, oidc_env.remote_url, "--client-secret", CLIENT_SECRET
    )

    assert "Authentication successful" in output
    assert LITERAL_WARNING in output
    assert "LORE_CLIENT_SECRET" in output


@pytest.mark.smoke
def test_a_wrong_client_secret_is_refused(oidc_env, make_repo):
    """The provider's `invalid_client` fails the login as `NotAuthenticated`,
    and nothing is stored."""
    oidc_env.provider.clients[CLIENT_ID] = (CLIENT_SECRET, SERVICE_ACCOUNT)
    repo = make_repo(LORE_CLIENT_SECRET="not-the-secret")

    with pytest.raises(NotAuthenticatedError):
        login_as_client(repo, oidc_env.remote_url)

    assert len(oidc_env.provider.requests_to("/token")) == 1
    assert "oidc+" not in repo.run(urc_args=["auth", "list"])


@pytest.mark.smoke
def test_a_client_login_without_a_secret_asks_nothing(oidc_env, make_repo):
    """With no secret from either source the login fails before any request,
    naming where the secret goes."""
    repo = make_repo()

    with pytest.raises(LoreException) as failure:
        login_as_client(repo, oidc_env.remote_url)

    assert "LORE_CLIENT_SECRET" in str(failure.value)
    assert not oidc_env.provider.requests


@pytest.mark.smoke
def test_a_login_token_from_the_environment_does_not_warn(oidc_env, make_repo):
    """`--token-type` takes the token from `LORE_TOKEN` when `--token` is
    absent, without the warning a literal `--token` prints."""
    provider = oidc_env.provider
    provider.api_keys["oidc-api-key"] = KEY_USER
    repo = make_repo(LORE_TOKEN="oidc-api-key")

    output = repo.run(
        urc_args=["auth", "login", oidc_env.remote_url, "--token-type", "api-key"]
    )

    assert "Authentication successful" in output
    assert LITERAL_WARNING not in output
    assert provider.requests_to("/token")[0].form["subject_token"] == "oidc-api-key"

    literal = login_with_token(repo, oidc_env.remote_url, "api-key", "oidc-api-key")
    assert LITERAL_WARNING in literal


@pytest.mark.smoke
def test_an_expired_session_fails_the_login(oidc_env, make_repo):
    """`expired_token` ends the login on the poll that reports it, as a
    failure of its own rather than a denial."""
    provider = oidc_env.provider
    provider.script_polls(["expired_token"])
    repo = make_repo()

    with pytest.raises(LoreException) as failure:
        repo.run(urc_args=["auth", "login", oidc_env.remote_url, "--no-browser"])

    assert not isinstance(failure.value, NotAuthenticatedError)
    assert_device_grant_on_the_wire(provider, polls=1)


def seed_and_commit(repo: Lore, **kwargs) -> None:
    """One file staged and committed, under the global options `kwargs`."""
    with repo.open_file("seed.txt", "w+") as f:
        f.write("seed\n")
    repo.stage(scan=True, **kwargs)
    repo.commit("seed", **kwargs)


@pytest.mark.smoke
def test_login_records_the_tokens_identity_claim(oidc_env, make_repo):
    """The token carries `identity`, so it is stored under that value and
    not the pairwise `sub`, and `auth list` reports it."""
    provider = oidc_env.provider
    provider.user = PAIRWISE_USER
    repo = make_repo()

    output = repo.run(urc_args=["auth", "login", oidc_env.remote_url, "--no-browser"])

    assert "Authentication successful" in output
    listing = repo.run(urc_args=["auth", "list"])
    assert PAIRWISE_USER.identity in listing
    assert PAIRWISE_USER.user_id not in listing


@pytest.mark.smoke
def test_a_commit_after_login_records_the_tokens_identity(
    oidc_env, new_lore_repo, scratch_dir
):
    """A revision made after login names the `identity` value as its creator,
    and the server authorizes the creating request as the same identity."""
    oidc_env.provider.user = PAIRWISE_USER
    auth_store = scratch_dir("auth-store", create=True)
    repo = new_lore_repo(
        remote_url=oidc_env.remote_url,
        environment_vars={"LORE_AUTH_PATH": str(auth_store)},
        create_repo=False,
    )
    repo.run(urc_args=["auth", "login", oidc_env.remote_url, "--no-browser"])
    repo.repository_create()
    seed_and_commit(repo)

    rev = repo.revision_info(metadata=True)
    assert rev.creator == PAIRWISE_USER.identity, rev.creator
    server_log = Path(oidc_env.server_log).read_text(errors="replace")
    assert f'user_id="{PAIRWISE_USER.identity}"' in server_log
    assert f'user_id="{PAIRWISE_USER.user_id}"' not in server_log


@pytest.mark.smoke
def test_a_push_after_login_sends_the_login_token_unexchanged(oidc_env, make_repo):
    """The server advertises no partition template, so the deployment is on
    Tier 1: the login token authorizes the push itself, and the provider is
    asked for no exchange."""
    provider = oidc_env.provider
    repo = make_repo()
    repo.run(urc_args=["auth", "login", oidc_env.remote_url, "--no-browser"])
    repo.repository_create()
    seed_and_commit(repo)

    repo.push()

    grants = [r.form.get("grant_type") for r in provider.requests_to("/token")]
    assert grants == [DEVICE_CODE_GRANT], grants


def refreshes(provider: MockOidcProvider) -> list[dict[str, str]]:
    return [
        r.form
        for r in provider.requests_to("/token")
        if r.form.get("grant_type") == REFRESH_TOKEN_GRANT
    ]


@pytest.mark.smoke
def test_an_expired_login_is_refreshed_without_interaction(oidc_env, make_repo):
    """A login whose access token has expired is refreshed by the next
    operation, with the refresh token the login brought. The provider rotates
    it, so the store holds the new one: the refresh after the next expiry
    presents that, and the provider, which revokes the login when a consumed
    refresh token comes back, still answers."""
    provider = oidc_env.provider
    provider.token_lifetime = -60
    repo = make_repo()
    repo.run(urc_args=["auth", "login", oidc_env.remote_url, "--no-browser"])
    [first] = provider.valid_refresh_tokens()

    # Long enough for the operation, short enough to wait out.
    provider.token_lifetime = 3
    repo.repository_create()
    [second] = provider.valid_refresh_tokens()
    time.sleep(4)
    provider.token_lifetime = 3600
    seed_and_commit(repo)
    repo.push()

    expected = {"grant_type": REFRESH_TOKEN_GRANT, "client_id": provider.client_id}
    assert refreshes(provider) == [
        {**expected, "refresh_token": first},
        {**expected, "refresh_token": second},
    ]
    assert len(provider.valid_refresh_tokens()) == 1
    assert_user_logged_in(repo, provider)


@pytest.mark.smoke
def test_a_refused_refresh_token_is_presented_once(oidc_env, make_repo):
    """A refresh token the provider refuses fails the operation as an expired
    login did before, and is dropped: the next operation asks the provider
    nothing more."""
    provider = oidc_env.provider
    provider.token_lifetime = -60
    repo = make_repo()
    repo.run(urc_args=["auth", "login", oidc_env.remote_url, "--no-browser"])
    provider.revoke_refresh_tokens()

    for _ in range(2):
        with pytest.raises(LoreException):
            repo.repository_create()

    assert len(refreshes(provider)) == 1


@pytest.mark.smoke
def test_a_refresh_whose_answer_is_lost_is_not_presented_again(oidc_env, make_repo):
    """A provider that consumes the refresh token and then fails to answer
    fails the operation. The token may be spent, so the next operation does
    not present it again, which the provider would treat as a replay."""
    provider = oidc_env.provider
    provider.token_lifetime = -60
    repo = make_repo()
    repo.run(urc_args=["auth", "login", oidc_env.remote_url, "--no-browser"])
    provider.lose_refresh_answers = True

    for _ in range(2):
        with pytest.raises(LoreException):
            repo.repository_create()

    assert len(refreshes(provider)) == 1


@pytest.mark.smoke
def test_a_supplied_token_and_an_offline_commit_record_the_identity(
    oidc_env, new_lore_repo, scratch_dir
):
    """A token passed with `--identity-token`, and a commit made offline with
    it, record the token's `identity` value too: the identity is read from
    the token wherever the token is, with no environment response."""
    token = oidc_env.provider.mint_token(PAIRWISE_USER)
    auth_store = scratch_dir("auth-store", create=True)
    repo = new_lore_repo(
        remote_url=oidc_env.remote_url,
        environment_vars={"LORE_AUTH_PATH": str(auth_store)},
        create_repo=False,
    )
    repo.repository_create(identity_token=token)
    seed_and_commit(repo, identity_token=token, offline=True)

    rev = repo.revision_info(metadata=True, identity_token=token, offline=True)
    assert rev.creator == PAIRWISE_USER.identity, rev.creator


@pytest.mark.smoke
def test_login_with_an_empty_identity_claim_is_refused(oidc_env, make_repo):
    """An `identity` claim that is present but empty names no one: the login
    fails, naming the claim, and nothing is stored."""
    provider = oidc_env.provider
    provider.user = OidcUser("empty-identity-sub", "Empty", "empty", identity="")
    repo = make_repo()

    with pytest.raises(LoreException) as refused:
        repo.run(urc_args=["auth", "login", oidc_env.remote_url, "--no-browser"])

    assert "identity" in str(refused.value)
    assert "empty-identity-sub" not in repo.run(urc_args=["auth", "list"])


def dry_run_login_args(
    provider: MockOidcProvider, path: str
) -> tuple[list[str], OidcUser]:
    if path == "device grant":
        return ["--no-browser"], provider.user
    if path == "lore token":
        token = provider.mint_token(KEY_USER)
        return ["--token-type", "lore", "--token", token], KEY_USER
    if path == "api key":
        provider.api_keys["oidc-api-key"] = KEY_USER
        return ["--token-type", "api-key", "--token", "oidc-api-key"], KEY_USER
    provider.clients[CLIENT_ID] = (CLIENT_SECRET, SERVICE_ACCOUNT)
    return ["--client-id", CLIENT_ID], SERVICE_ACCOUNT


@pytest.mark.smoke
@pytest.mark.parametrize(
    "path", ["device grant", "lore token", "api key", "client credentials"]
)
def test_a_dry_run_login_stores_no_token(oidc_env, make_repo, path):
    """`auth login --dry-run` authenticates and stores nothing, where the same
    login without it stores the token."""
    repo = make_repo(LORE_CLIENT_SECRET=CLIENT_SECRET)
    login_args, user = dry_run_login_args(oidc_env.provider, path)
    args = ["auth", "login", oidc_env.remote_url, *login_args]

    assert "Authentication successful" in repo.run(urc_args=args, dry_run=True)
    assert user.user_id not in repo.run(urc_args=["auth", "list"])

    repo.run(urc_args=args)
    assert user.user_id in repo.run(urc_args=["auth", "list"])


@pytest.mark.smoke
@pytest.mark.parametrize("command", ["logout", "clear"])
def test_a_dry_run_logout_removes_no_token(oidc_env, make_repo, command):
    """`auth logout --dry-run` and `auth clear --dry-run` keep the stored
    token, where the same commands without it remove it."""
    provider = oidc_env.provider
    repo = make_repo()
    login_with_token(repo, oidc_env.remote_url, "lore", provider.mint_token(KEY_USER))
    args = ["auth", command]
    if command == "logout":
        args += ["--auth-url", f"oidc+{provider.issuer}"]

    repo.run(urc_args=args, dry_run=True)
    assert KEY_USER.user_id in repo.run(urc_args=["auth", "list"])

    repo.run(urc_args=args)
    assert KEY_USER.user_id not in repo.run(urc_args=["auth", "list"])


def logged_in_repo(oidc_env, make_repo) -> Lore:
    """A repository on the remote, after a device-grant login that brought a
    refresh token."""
    repo = make_repo()
    repo.run(urc_args=["auth", "login", oidc_env.remote_url, "--no-browser"])
    repo.repository_create()
    return repo


@pytest.mark.smoke
def test_logout_revokes_the_refresh_token(oidc_env, make_repo):
    """`auth logout` revokes the login's refresh token at the revocation
    endpoint the provider advertises (RFC 7009), as the public client, and
    removes the login."""
    provider = oidc_env.provider
    provider.revocation_endpoint = f"{provider.issuer}/revoke"
    repo = logged_in_repo(oidc_env, make_repo)
    [refresh_token] = provider.valid_refresh_tokens()

    assert "Logged out" in repo.run(urc_args=["auth", "logout"])

    assert [r.form for r in provider.requests_to("/revoke")] == [
        {
            "token": refresh_token,
            "token_type_hint": "refresh_token",
            "client_id": provider.client_id,
        }
    ]
    assert provider.valid_refresh_tokens() == []
    assert provider.user.user_id not in repo.run(urc_args=["auth", "list"])


@pytest.mark.smoke
def test_logout_without_a_revocation_endpoint_is_local(oidc_env, make_repo):
    """A provider advertising no revocation endpoint is asked nothing: the
    logout only removes the login, as before."""
    provider = oidc_env.provider
    repo = logged_in_repo(oidc_env, make_repo)
    provider.reset_requests()

    output = repo.run(urc_args=["auth", "logout"])

    assert "Logged out" in output
    assert "revoke" not in output.lower()
    assert [r.path for r in provider.requests if r.method == "POST"] == []
    assert len(provider.valid_refresh_tokens()) == 1
    assert provider.user.user_id not in repo.run(urc_args=["auth", "list"])


@pytest.mark.smoke
def test_a_login_without_a_refresh_token_is_not_revoked(oidc_env, make_repo):
    """A client credentials login stores no refresh token, so its logout asks
    the provider nothing: the access token belongs to the confidential client,
    which only that client could revoke."""
    provider = oidc_env.provider
    provider.revocation_endpoint = f"{provider.issuer}/revoke"
    provider.clients[CLIENT_ID] = (CLIENT_SECRET, SERVICE_ACCOUNT)
    repo = make_repo(LORE_CLIENT_SECRET=CLIENT_SECRET)
    login_as_client(repo, oidc_env.remote_url)
    repo.repository_create()

    output = repo.run(urc_args=["auth", "logout"])

    assert "Logged out" in output
    assert "[Warn]" not in output, output
    assert provider.requests_to("/revoke") == []
    assert SERVICE_ACCOUNT.user_id not in repo.run(urc_args=["auth", "list"])


@pytest.mark.smoke
def test_removing_one_authorization_revokes_nothing(oidc_env, make_repo):
    """`auth logout --resource` removes one authorization token and keeps the
    login, so its refresh token is not revoked."""
    provider = oidc_env.provider
    provider.revocation_endpoint = f"{provider.issuer}/revoke"
    repo = logged_in_repo(oidc_env, make_repo)

    repo.run(
        urc_args=[
            "auth",
            "logout",
            "--user-id",
            provider.user.user_id,
            "--resource",
            "00112233445566778899aabbccddeeff",
        ]
    )

    assert provider.requests_to("/revoke") == []
    assert len(provider.valid_refresh_tokens()) == 1
    assert provider.user.user_id in repo.run(urc_args=["auth", "list"])


def closed_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


@pytest.mark.smoke
def test_logout_at_an_unreachable_provider_still_removes_the_login(oidc_env, make_repo):
    """A revocation endpoint that cannot be reached fails the logout, which
    says so, and the login is removed all the same."""
    provider = oidc_env.provider
    provider.revocation_endpoint = f"http://127.0.0.1:{closed_port()}/revoke"
    repo = logged_in_repo(oidc_env, make_repo)

    with pytest.raises(LoreException) as failed:
        repo.run(urc_args=["auth", "logout"])

    message = str(failed.value)
    assert "removed from this machine, but was not revoked" in message, message
    assert "Logged out" not in message
    assert len(provider.valid_refresh_tokens()) == 1
    assert provider.user.user_id not in repo.run(urc_args=["auth", "list"])
