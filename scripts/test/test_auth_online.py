# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Smoke tests for the authenticated path, against a declarative auth stub.

The module launches one lore server with authentication enabled — an
`auth_url` pointing at an in-process `MockAuthServer` (see mock_auth_server.py)
plus `[server.auth]` / `[server.auth.jwk]` so the server validates the stub's
RS256 tokens against the stub's JWKS endpoint. With `auth_url` configured the
server selects the online `AuthClientAuthorizer`: every repository access check
is a live `CheckUserPermission` call back into the stub.

The stub has no behavior of its own. Each test mints the tokens its scenario
needs and registers the request → response pairs the CLI and the server are
expected to ask for; anything unregistered is denied. The `script_*` helpers
below name the conversations the standard flows consist of, so a test body
lists which conversations it allows and everything else fails closed.

Two identities are exercised: USER1 logs in through the interactive
device-grant flow (`auth login --no-browser`), USER2 through an API key, so
scenarios can cover one user creating a repository and the other being granted
or denied access. Each user gets an isolated token store (LORE_AUTH_PATH) so
credentials never leak between identities within a test. Stub rules and
records reset between tests.
"""

import logging
import uuid
from pathlib import Path
from types import SimpleNamespace

import pytest
from error_types import LoreException
from lore_server import (
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)
from mock_auth_server import (
    USER1,
    USER2,
    USER2_API_KEY,
    MockAuthServer,
    MockUser,
    check_user_permission_response,
    empty_response,
    start_auth_session_response,
    tamper_token,
    user_token_response,
)

from lore import Lore

logger = logging.getLogger(__name__)

# One authenticated server (and stub) per module. The group keeps every test on
# the same xdist worker so they share it.
pytestmark = pytest.mark.xdist_group("auth_online")


AUTH_CONFIG = """

# --- appended by test_auth_online.py: enable authentication ---
[environment.endpoint]
auth_url = "{auth_url}"

[server.auth]
jwt_issuer = "{issuer}"
jwt_audience = ["{audience}"]

[server.auth.jwk]
endpoint = "{jwks_url}"
"""


@pytest.fixture(scope="module")
def auth_env(request, tmp_path_factory, lore_server_executable_path):
    """A lore server with authentication enabled, backed by the stub.

    Yields a namespace with the stub, the server's remote URL, and the server
    log path. The signing key is per module (the lore server caches the JWKS);
    the stub's rules and records are reset per test by `_reset_stub`.
    """
    mock = MockAuthServer().start()

    shared_port = allocate_free_port()
    ports = {
        "quic": shared_port,
        "grpc": shared_port,
        "http": allocate_free_port(),
        "internal": allocate_free_port(),
    }
    server_root, server_env = generate_server_config(request, tmp_path_factory, ports)

    # jwt_audience is a list, which the LORE__ env source cannot carry, so the
    # auth settings ride in the per-test copy of the gha config instead.
    config_path = server_root / "lore-server" / "config" / "gha.toml"
    config_path.write_text(
        config_path.read_text()
        + AUTH_CONFIG.format(
            auth_url=mock.auth_url,
            issuer=mock.issuer,
            audience=mock.audience[0],
            jwks_url=mock.jwks_url,
        )
    )

    server_proc, server_log_path, server_log_fd = launch_lore_server(
        server_root, server_env, lore_server_executable_path
    )
    try:
        yield SimpleNamespace(
            mock=mock,
            remote_url=f"lore://127.0.0.1:{shared_port}/",
            server_log=server_log_path,
        )
    finally:
        _kill_server_by_pid(
            server_proc.pid, server_log_path, label="auth-online server"
        )
        server_log_fd.close()
        mock.stop()


@pytest.fixture(autouse=True)
def _reset_stub(auth_env):
    """Every test starts from an empty rule table and empty records."""
    auth_env.mock.reset()


@pytest.fixture(scope="function")
def make_actor(auth_env, new_lore_repo, scratch_dir):
    """Factory for per-user actors: an isolated token store plus a repo factory
    whose repositories all target the authenticated server."""

    def _make_actor(label: str):
        auth_store = scratch_dir(f"auth-store-{label}", create=True)
        environment_vars = {"LORE_AUTH_PATH": str(auth_store)}

        def make_repo(**kwargs) -> Lore:
            kwargs.setdefault("remote_url", auth_env.remote_url)
            kwargs.setdefault("create_repo", False)
            # Copied so Lore.__init__'s setdefault of LORE_REMOTE_URL on one
            # repo does not leak into the next.
            kwargs.setdefault("environment_vars", dict(environment_vars))
            return new_lore_repo(**kwargs)

        return SimpleNamespace(make_repo=make_repo, auth_store=auth_store)

    return _make_actor


# ---------------------------------------------------------------------------
# CLI drivers
# ---------------------------------------------------------------------------


def login_interactive(repo: Lore, remote_url: str) -> str:
    """`lore auth login --no-browser`: prints the login URL instead of opening
    a browser, then polls `GetAuthSession` until a token arrives."""
    return repo.run(urc_args=["auth", "login", remote_url, "--no-browser"])


def login_api_key(repo: Lore, remote_url: str, api_key: str) -> str:
    return repo.run(
        urc_args=[
            "auth",
            "login",
            remote_url,
            "--token-type",
            "api-key",
            "--token",
            api_key,
        ]
    )


def commit_file(repo: Lore, name: str = "hello.txt", content: str = "hello") -> None:
    (Path(repo.path) / name).write_text(content)
    repo.file_stage(name)
    repo.revision_commit(f"add {name}")
    repo.push()


def metadata_probe(repo: Lore, token: str, value: str) -> str:
    """A repository metadata write carrying `token` as the request credential.

    The token is supplied as both `--identity-token` and `--access-token`: the
    repository service authenticates with the identity token, while data paths
    use the (normally exchanged) access token — supplying both pins the
    credential the request carries regardless of path, and the supplied-token
    plumbing deliberately skips the client-side expiry checks, so the token
    reaches the server verbatim."""
    return repo.repository_metadata_set(
        ["probe", value], identity_token=token, access_token=token
    )


# ---------------------------------------------------------------------------
# Conversation scripts: the request → response pairs each flow consists of
# ---------------------------------------------------------------------------


def authz_resources(
    resource_id: str, permissions=("admin", "write", "read")
) -> list[dict]:
    """The `resources` claim shape of an authorization token."""
    return [{"resource_id": resource_id, "permission": list(permissions)}]


def script_interactive_login(
    mock: MockAuthServer, user: MockUser, login_token: str, session_code: str
) -> None:
    """The device-grant conversation behind `auth login --no-browser`:
    StartAuthSession hands out a session code and a login URL, and polling
    GetAuthSession with that code returns the user's token."""
    mock.on("StartAuthSession").respond(
        start_auth_session_response(session_code, mock.login_page_url(session_code))
    )
    mock.on("GetAuthSession", session_code=session_code).respond(
        user_token_response(user, login_token)
    )


def script_api_key_login(
    mock: MockAuthServer, user: MockUser, login_token: str, api_key: str
) -> None:
    """The API-key conversation behind `auth login --token-type api-key`."""
    mock.on(
        "ExchangeExternalTokenForUserToken",
        external_token=api_key,
        token_type="api-key",
    ).respond(user_token_response(user, login_token))


def script_partition_access(
    mock: MockAuthServer,
    user: MockUser,
    login_token: str,
    resource_id: str,
    authz_token: str,
    permissions=("admin", "write", "read"),
) -> None:
    """What is asked while `user` works on one partition: the CLI exchanges
    its login token for the partition-scoped one, and the server's online
    authorizer checks whichever of the two tokens a request carried."""
    mock.on(
        "ExchangeUserTokenForMultiresourceToken",
        bearer=login_token,
        resource_id=resource_id,
    ).respond(user_token_response(user, authz_token))
    for token in (login_token, authz_token):
        mock.on("CheckUserPermission", bearer=token, resource_id=resource_id).respond(
            check_user_permission_response(resource_id, permissions)
        )


def script_repository_lifecycle(mock: MockAuthServer, resource_id: str) -> None:
    """Repository create and delete register and remove the rebac resource."""
    mock.on("CreateResource", resource_id=resource_id).respond(empty_response())
    mock.on("DeleteResource", resource_id=resource_id).respond(empty_response())


def provision_owner(auth_env, make_actor, label: str, user: MockUser, api_key=None):
    """Script and perform the standard owner scenario: log `user` in
    (interactively, or with `api_key`), create a repository, and allow the
    exchange and online checks that working on it entails.

    Returns the repo plus everything a test needs to reference the scripted
    conversation: the resource id and both minted tokens."""
    mock = auth_env.mock
    repo_id = uuid.uuid4().hex
    resource_id = f"urc-{repo_id}"
    login_token = mock.mint_token(user)
    authz_token = mock.mint_token(user, resources=authz_resources(resource_id))

    if api_key is None:
        script_interactive_login(mock, user, login_token, f"session-{label}")
    else:
        script_api_key_login(mock, user, login_token, api_key)
    script_repository_lifecycle(mock, resource_id)
    script_partition_access(mock, user, login_token, resource_id, authz_token)

    actor = make_actor(label)
    if api_key is None:
        login_interactive(actor.make_repo(), auth_env.remote_url)
    else:
        login_api_key(actor.make_repo(), auth_env.remote_url, api_key)
    repo = actor.make_repo(repo_id=repo_id)
    repo.repository_create(repo_id=repo_id, identity=user.user_id)

    return SimpleNamespace(
        actor=actor,
        repo=repo,
        resource_id=resource_id,
        login_token=login_token,
        authz_token=authz_token,
        user=user,
    )


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_interactive_login_authenticates(auth_env, make_actor):
    """`auth login --no-browser` prints the login URL the auth service handed
    out, completes the device-grant flow (StartAuthSession + GetAuthSession
    polling), and reports success."""
    mock = auth_env.mock
    login_token = mock.mint_token(USER1)
    script_interactive_login(mock, USER1, login_token, "session-login-test")

    actor = make_actor("user1")
    repo = actor.make_repo()

    output = login_interactive(repo, auth_env.remote_url)

    assert "Login at:" in output
    assert mock.login_page_url("session-login-test") in output
    assert "Authentication successful" in output
    assert mock.calls["StartAuthSession"] == 1
    assert mock.calls["GetAuthSession"] >= 1, (
        "the CLI must poll GetAuthSession to obtain the token"
    )

    listing = repo.run(urc_args=["auth", "list"])
    assert USER1.user_id in listing
    assert USER1.preferred_username in listing


@pytest.mark.smoke
def test_api_key_login_authenticates_second_user(auth_env, make_actor):
    """`auth login --token-type api-key` exchanges the key for USER2's token
    without any interactive session."""
    mock = auth_env.mock
    script_api_key_login(mock, USER2, mock.mint_token(USER2), USER2_API_KEY)

    actor = make_actor("user2")
    repo = actor.make_repo()

    output = login_api_key(repo, auth_env.remote_url, USER2_API_KEY)

    assert "Authentication successful" in output
    listing = repo.run(urc_args=["auth", "list"])
    assert USER2.user_id in listing
    assert USER1.user_id not in listing, "the API key must log in the second user"


@pytest.mark.smoke
def test_unknown_api_key_is_rejected(auth_env, make_actor):
    """No rule matches an unknown key, so the exchange is refused."""
    actor = make_actor("intruder")
    repo = actor.make_repo()

    with pytest.raises(LoreException):
        login_api_key(repo, auth_env.remote_url, "not-a-real-key")


@pytest.mark.smoke
def test_repository_create_and_delete_manage_the_auth_resource(auth_env, make_actor):
    """Repository create registers the rebac resource under the creator's
    credential; delete removes it. Asserted from the requests the stub saw."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "user1", USER1)

    creations = mock.requests_for("CreateResource")
    assert [c["resource_id"] for c in creations] == [owner.resource_id]
    assert creations[0]["resource_name"] == owner.repo.name
    assert creations[0]["bearer"] == owner.login_token, (
        "the resource must be created under the creating user's credential"
    )

    owner.repo.repository_delete()

    deletions = mock.requests_for("DeleteResource")
    assert [d["resource_id"] for d in deletions] == [owner.resource_id]
    assert deletions[0]["bearer"] == owner.login_token


@pytest.mark.smoke
def test_remote_write_uses_token_exchange_and_online_permission_checks(
    auth_env, make_actor
):
    """A push to the repository forces the CLI through the multiresource token
    exchange. A repository metadata write forces the server through online
    CheckUserPermission validation."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "user1", USER1)

    exchanges_before = mock.calls["ExchangeUserTokenForMultiresourceToken"]

    commit_file(owner.repo)

    assert mock.calls["ExchangeUserTokenForMultiresourceToken"] > exchanges_before, (
        "the CLI must exchange its login token for a repository-scoped token"
    )

    checks_before = mock.calls["CheckUserPermission"]

    owner.repo.repository_metadata_set(["team", "blue"])

    assert mock.calls["CheckUserPermission"] > checks_before, (
        "the server must validate access online against the auth service"
    )


@pytest.mark.smoke
def test_user_without_grant_is_denied(auth_env, make_actor):
    """USER2 holds no exchange or permission rule for USER1's repository A:
    the clone fails, and the stub records the denied permission check."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "user1", USER1)
    commit_file(owner.repo)

    user2 = make_actor("user2")
    token2 = mock.mint_token(USER2)
    script_api_key_login(mock, USER2, token2, USER2_API_KEY)
    viewer = user2.make_repo(remote_path=owner.repo.remote_path)
    login_api_key(viewer, auth_env.remote_url, USER2_API_KEY)

    with pytest.raises(LoreException):
        viewer.clone()

    denied_checks = [
        request
        for request in mock.requests_for("CheckUserPermission")
        if request["bearer"] == token2 and owner.resource_id in request["resource_id"]
    ]
    assert denied_checks, "the denied user's permission check must reach the stub"


@pytest.mark.smoke
def test_granted_user_can_access_shared_repository(auth_env, make_actor, scratch_dir):
    """Allowing USER2's exchange and permission checks on USER1's repository B
    makes the clone that would otherwise be denied succeed."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "user1", USER1)
    commit_file(owner.repo, "shared.txt", "shared content")

    user2 = make_actor("user2")
    token2 = mock.mint_token(USER2)
    authz2 = mock.mint_token(
        USER2, resources=authz_resources(owner.resource_id, ("read", "write"))
    )
    script_api_key_login(mock, USER2, token2, USER2_API_KEY)
    script_partition_access(
        mock, USER2, token2, owner.resource_id, authz2, ("read", "write")
    )

    viewer = user2.make_repo(remote_path=owner.repo.remote_path)
    login_api_key(viewer, auth_env.remote_url, USER2_API_KEY)

    clone_path = scratch_dir("user2-clone-of-b")
    viewer.clone(path=str(clone_path))

    assert (clone_path / "shared.txt").read_text() == "shared content"


@pytest.mark.smoke
def test_each_user_owns_their_created_repositories(auth_env, make_actor):
    """USER2 creates repository C with an API-key login: the rebac
    registration carries USER2's credential, not USER1's."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "user2", USER2, api_key=USER2_API_KEY)
    commit_file(owner.repo)

    creations = mock.requests_for("CreateResource")
    assert [c["resource_id"] for c in creations] == [owner.resource_id]
    assert creations[0]["bearer"] == owner.login_token
    assert mock.verify_token(creations[0]["bearer"])["sub"] == USER2.user_id


@pytest.mark.smoke
def test_revoked_grant_is_denied_by_the_online_check(auth_env, make_actor):
    """Re-registering the permission checks as deny revokes access: the next
    online-checked operation fails even though the client still holds a valid,
    unexpired authorization token.

    This is a property of the `UrcAuthApi` configuration's online
    `AuthClientAuthorizer`. The OIDC implementations do not have online
    checks."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "user1", USER1)
    commit_file(owner.repo, "before-revocation.txt")
    owner.repo.repository_metadata_set(["stage", "before-revocation"])

    # Newest rule wins, so these shadow the allows provision_owner registered.
    for token in (owner.login_token, owner.authz_token):
        mock.on(
            "CheckUserPermission", bearer=token, resource_id=owner.resource_id
        ).deny()

    with pytest.raises(LoreException):
        owner.repo.repository_metadata_set(["stage", "after-revocation"])


@pytest.mark.smoke
def test_expired_login_token_is_skipped_client_side(auth_env, make_actor):
    """An expired authentication token in the store is skipped by the CLI's
    identity resolution: the operation fails without the CLI ever trading the
    stale credential in for an authorization token."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "user1", USER1)
    commit_file(owner.repo)

    # A separate token store holding only an expired login token for USER1.
    # `--token-type lore` stores the supplied token without asking the stub.
    stale = make_actor("stale-user1")
    holder = stale.make_repo(remote_path=owner.repo.remote_path)
    expired_authn = mock.mint_token(USER1, lifetime_seconds=-300)
    holder.run(
        urc_args=[
            "auth",
            "login",
            auth_env.remote_url,
            "--token-type",
            "lore",
            "--token",
            expired_authn,
        ]
    )

    exchanges_before = mock.calls["ExchangeUserTokenForMultiresourceToken"]

    with pytest.raises(LoreException):
        holder.clone()

    assert mock.calls["ExchangeUserTokenForMultiresourceToken"] == exchanges_before, (
        "an expired stored login token must be skipped, not exchanged"
    )


@pytest.mark.smoke
def test_expired_access_token_is_rejected_by_the_server(auth_env, make_actor):
    """A supplied expired authorization token reaches the server verbatim
    (`--access-token` bypasses the client-side exchange and its expiry checks)
    and the server's verifier rejects it. The same call with a freshly minted
    token succeeds, so the rejection is the expiry, not the plumbing."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "user1", USER1)

    valid = mock.mint_token(USER1, resources=authz_resources(owner.resource_id))
    mock.on("CheckUserPermission", bearer=valid, resource_id=owner.resource_id).respond(
        check_user_permission_response(owner.resource_id, ("admin", "write", "read"))
    )
    metadata_probe(owner.repo, valid, "valid")

    expired = mock.mint_token(
        USER1, resources=authz_resources(owner.resource_id), lifetime_seconds=-300
    )
    with pytest.raises(LoreException):
        metadata_probe(owner.repo, expired, "expired")


@pytest.mark.smoke
def test_tampered_token_is_rejected_by_the_server(auth_env, make_actor):
    """A well-formed token whose claims were altered after signing fails the
    server's signature verification."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "user1", USER1)

    valid = mock.mint_token(USER1, resources=authz_resources(owner.resource_id))
    mock.on("CheckUserPermission", bearer=valid, resource_id=owner.resource_id).respond(
        check_user_permission_response(owner.resource_id, ("admin", "write", "read"))
    )
    metadata_probe(owner.repo, valid, "valid")

    with pytest.raises(LoreException):
        metadata_probe(owner.repo, tamper_token(valid), "tampered")


@pytest.mark.smoke
def test_token_signed_by_wrong_key_is_rejected(auth_env, make_actor):
    """A token with the right claims, issuer, audience and key id, but signed
    by a different key, fails the server's signature verification — the forgery
    a stolen JWKS `kid` alone cannot help with. The imposter issuer is never
    started; only its signing key differs from the real stub's."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "user1", USER1)

    imposter = MockAuthServer(
        issuer=mock.issuer, audience=tuple(mock.audience), kid=mock.kid
    )
    forged = imposter.mint_token(USER1, resources=authz_resources(owner.resource_id))

    with pytest.raises(LoreException):
        metadata_probe(owner.repo, forged, "forged")


@pytest.mark.smoke
def test_garbage_token_is_rejected(auth_env, make_actor):
    """Strings that are not JWTs at all are refused. `aaaa.bbbb.cccc` is
    shaped like a JWT without decoding as one; `not-a-jwt` is not even that."""
    owner = provision_owner(auth_env, make_actor, "user1", USER1)

    for garbage in ("not-a-jwt", "aaaa.bbbb.cccc"):
        with pytest.raises(LoreException):
            metadata_probe(owner.repo, garbage, "garbage")
