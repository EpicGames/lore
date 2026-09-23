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
import threading
import time
import uuid
from pathlib import Path
from types import SimpleNamespace

import grpc
import pytest
from error_types import LoreException
from grpc_probe import REVISION_INFO, STORAGE_QUERY, call, repository_metadata
from protobuf_wire import encode_bytes_field, field_bytes, field_int, parse_fields
from lore_server import (
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)
from test_forwarded_requests import (
    REPOSITORY_GET,
    repository_get_by_name_request,
    repository_name_in_response,
)
from mock_auth_server import (
    USER1,
    USER2,
    USER2_API_KEY,
    MockAuthServer,
    MockUser,
    check_user_permission_response,
    empty_response,
    lookup_user_permissions_response,
    start_auth_session_response,
    tamper_token,
    user_info_response,
    user_token_response,
)
from thin_client import revision_tree

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


def append_auth_config(server_root: Path, mock: MockAuthServer) -> None:
    """Point a generated server config at the stub.

    jwt_audience is a list, which the LORE__ env source cannot carry, so the
    auth settings ride in the per-test copy of the gha config instead."""
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
    append_auth_config(server_root, mock)

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
    authorizer checks the login token where a request carries it. The access
    token is answered server-side from its own `resources` claim — the auth
    service refuses access tokens as CheckUserPermission credentials, and the
    stub does too — so only the login token gets a check rule."""
    mock.on(
        "ExchangeUserTokenForMultiresourceToken",
        bearer=login_token,
        resource_id=resource_id,
    ).respond(user_token_response(user, authz_token))
    mock.on("CheckUserPermission", bearer=login_token, resource_id=resource_id).respond(
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
def test_user_names_come_from_the_auth_service_without_a_separate_directory(
    auth_env, make_actor, lore_library_path
):
    """A server advertising no `user_url` keeps its auth service as
    the user directory: another user's name is a `GetUserInfo` call there,
    carrying the partition-scoped token the CLI exchanged for it."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "owner", USER1)
    mock.on(
        "GetUserInfo", bearer=owner.authz_token, resource_id=owner.resource_id
    ).respond(user_info_response(USER2))

    result = owner.repo.auth_user_info_capi(lore_library_path, USER2.user_id)

    assert result == 0, (
        f"resolving through the auth service failed with FFI code {result}"
    )
    assert mock.calls["GetUserInfo"] == 1
    assert list(mock.requests_for("GetUserInfo")[0]["user_id"]) == [USER2.user_id]


@pytest.mark.smoke
def test_repository_list_is_the_auth_services_answer(auth_env, make_actor):
    """`lore repository list` on the legacy tier asks `LookupUserPermissions`
    once, with the `urc` filter and no paging, and lists exactly the
    partitions the answer names: a second repository the same user created
    but the answer omits is not listed, and entries that are not partitions
    are skipped."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "lister", USER1)

    unlisted_id = uuid.uuid4().hex
    script_repository_lifecycle(mock, f"urc-{unlisted_id}")
    unlisted = owner.actor.make_repo(repo_id=unlisted_id)
    unlisted.repository_create(repo_id=unlisted_id, identity=USER1.user_id)

    mock.on("LookupUserPermissions", bearer=owner.login_token).respond(
        lookup_user_permissions_response(
            owner.resource_id, "urc-not-a-partition", "something-else"
        )
    )

    listing = owner.repo.repository_list().splitlines()

    assert f"{owner.repo.name} ({owner.repo.get_id()})" in listing
    assert not any(unlisted.get_id() in line for line in listing), (
        "a partition the auth service did not name must not be listed"
    )
    lookups = mock.requests_for("LookupUserPermissions")
    assert len(lookups) == 1
    assert lookups[0]["resource_filter"] == "urc"
    assert lookups[0]["page_token"] == "", "the first page carries no token"


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


# ---------------------------------------------------------------------------
# Partition access on every partition-scoped gRPC service
# ---------------------------------------------------------------------------

# One unary RPC per partition-scoped service. The partition-access check runs
# before the handler and before the body is decoded, so an empty request body
# is enough to observe the verdict: PERMISSION_DENIED is the check, anything
# else means the check passed and the handler answered for the empty body.
# A new partition-scoped service belongs in this table.
PARTITION_SCOPED_PROBES = (
    ("storage v0", "/urc.rpc.StorageService/Query"),
    ("storage v1", STORAGE_QUERY),
    ("revision v0", "/urc.rpc.RevisionService/BranchList"),
    ("revision v1", "/lore.revision.v1.RevisionService/BranchList"),
    ("thin client v1", REVISION_INFO),
    ("lock", "/urc.lock.LockService/Query"),
)

SUBSCRIBE = "/lore.notification.NotificationService/Subscribe"


def grpc_target(remote_url: str) -> str:
    return remote_url.removeprefix("lore://").rstrip("/")


def subscribe_code(target: str, repo_id_hex: str, token: str) -> grpc.StatusCode:
    """Open a notification subscription and report how it ends.

    Subscribe is server-streaming and a granted stream stays open with no
    events, so the deadline is what closes it: DEADLINE_EXCEEDED means the
    subscription was accepted, PERMISSION_DENIED that it was refused."""
    request = encode_bytes_field(1, bytes.fromhex(repo_id_hex))
    with grpc.insecure_channel(target) as channel:
        invoke = channel.unary_stream(SUBSCRIBE, lambda b: b, lambda b: b)
        stream = invoke(
            request,
            metadata=(("authorization", f"Bearer {token}"),),
            timeout=2.0,
        )
        try:
            next(stream)
            return grpc.StatusCode.OK
        except grpc.RpcError as error:
            return error.code()


@pytest.mark.smoke
def test_every_partition_scoped_service_enforces_partition_access(auth_env, make_actor):
    """Every partition-scoped gRPC service sits behind the partition-access
    check: on each one, a verifiable token holding no grant for the partition
    answers PERMISSION_DENIED, the owner's granted token never does, and the
    stub's records show the denials were the online check's verdicts. This is
    the registration proof — a service mounted without the check fails the
    ungranted half of its probe."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "user1", USER1)
    target = grpc_target(auth_env.remote_url)
    repo_id_hex = owner.resource_id.removeprefix("urc-")

    # Verifies against the same issuer and JWKS, but the stub holds no
    # CheckUserPermission rule for it, so every online check denies it.
    ungranted = mock.mint_token(USER2)

    def probe_metadata(token: str):
        return repository_metadata(repo_id_hex) + (
            ("authorization", f"Bearer {token}"),
        )

    for label, method in PARTITION_SCOPED_PROBES:
        code, _body, details = call(target, method, metadata=probe_metadata(ungranted))
        assert code == grpc.StatusCode.PERMISSION_DENIED, (
            f"{label}: an ungranted token must be denied, got {code} '{details}'"
        )

        code, _body, details = call(
            target, method, metadata=probe_metadata(owner.authz_token)
        )
        assert code != grpc.StatusCode.PERMISSION_DENIED, (
            f"{label}: the granted owner token must pass the partition check, "
            f"got {code} '{details}'"
        )

    # Notification reads the partition from the request body, so its check
    # lives in the subscribe handler; probed on the body field, no partition
    # metadata at all.
    assert subscribe_code(target, repo_id_hex, ungranted) == (
        grpc.StatusCode.PERMISSION_DENIED
    ), "notification: an ungranted subscribe must be denied"
    granted_subscribe = subscribe_code(target, repo_id_hex, owner.authz_token)
    assert granted_subscribe == grpc.StatusCode.DEADLINE_EXCEEDED, (
        f"notification: a granted subscribe holds the stream open until the "
        f"probe's deadline, got {granted_subscribe}"
    )

    denied_checks = [
        check
        for check in mock.requests_for("CheckUserPermission")
        if check["bearer"] == ungranted and owner.resource_id in check["resource_id"]
    ]
    assert len(denied_checks) >= len(PARTITION_SCOPED_PROBES) + 1, (
        "every denial must be the online check's verdict: one CheckUserPermission "
        f"per probed service, got {len(denied_checks)}"
    )


STORAGE_COPY = "/lore.storage.v1.StorageService/Copy"

# google.rpc.Code values as `ItemStatus.code` carries them.
CODE_NOT_FOUND = 5
CODE_PERMISSION_DENIED = 7


def copy_item_code(
    target: str, destination_hex: str, source_hex: str, token: str
) -> int:
    """One item through the v1 Copy stream, reporting its `ItemStatus.code`.

    The destination rides in the metadata (the partition-access layer's
    check); each item's source rides in its body, and a denied source answers
    in-band in the item's status with the stream itself OK. A stream-level
    error (the destination check) folds into the same numeric code so the
    caller reads one verdict either way."""
    address = encode_bytes_field(1, b"\x00" * 32) + encode_bytes_field(2, b"\x00" * 16)
    request = encode_bytes_field(1, bytes.fromhex(source_hex)) + encode_bytes_field(
        2, address
    )
    metadata = repository_metadata(destination_hex) + (
        ("authorization", f"Bearer {token}"),
    )
    with grpc.insecure_channel(target) as channel:
        invoke = channel.stream_stream(STORAGE_COPY, lambda b: b, lambda b: b)
        stream = invoke(iter([request]), metadata=metadata, timeout=10.0)
        try:
            item = next(stream)
        except grpc.RpcError as error:
            return error.code().value[0]
    return field_int(parse_fields(field_bytes(parse_fields(item), 3)), 1)


@pytest.mark.smoke
def test_cross_partition_copy_requires_a_source_grant(auth_env):
    """Cross-partition copy authorizes its *source* partition per item: an
    access token granting the destination alone is denied, and one granting
    both partitions reaches the store — which answers NOT_FOUND for the absent
    address, proving the denial above was the missing grant rather than the
    missing fragment. Both verdicts come from the token's own `resources`
    claim: no CheckUserPermission rule is scripted, and the stub's records
    prove nothing asked for one."""
    mock = auth_env.mock
    target = grpc_target(auth_env.remote_url)
    destination = uuid.uuid4().hex
    source = uuid.uuid4().hex

    destination_only = mock.mint_token(
        USER1, resources=authz_resources(f"urc-{destination}")
    )
    both = mock.mint_token(
        USER1,
        resources=authz_resources(f"urc-{destination}")
        + authz_resources(f"urc-{source}"),
    )

    denied = copy_item_code(target, destination, source, destination_only)
    assert denied == CODE_PERMISSION_DENIED, (
        f"a source the caller holds no grant for must be denied, got code {denied}"
    )

    granted = copy_item_code(target, destination, source, both)
    assert granted == CODE_NOT_FOUND, (
        f"a granted source must pass the check and reach the store, got code {granted}"
    )

    assert not mock.requests_for("CheckUserPermission"), (
        "both verdicts must come from the access token's resources claim, "
        "not an online check"
    )


LINKER_API_KEY = "linker-api-key"


@pytest.mark.smoke
def test_cross_partition_link_read_follows_the_token_claim(auth_env, make_actor):
    """A revision tree walk follows a link into another partition only when
    the caller's token grants that partition: with the parent's grant alone
    the link node is reported and nothing beneath it, with both grants the
    linked content streams. The walk asks the authorizer synchronously, so
    both verdicts must come from the access token's own `resources` claim:
    no CheckUserPermission rule is scripted for either probe token, and the
    stub's records prove nothing asked for one."""
    mock = auth_env.mock
    parent_id, linked_id = uuid.uuid4().hex, uuid.uuid4().hex
    parent_resource, linked_resource = f"urc-{parent_id}", f"urc-{linked_id}"
    login_token = mock.mint_token(USER1)
    # The CLI's setup work — creating both repositories, mounting the link —
    # exchanges for one partition at a time; a token granting both keeps
    # the mount's read of the linked partition working whichever one it
    # exchanged for.
    setup_token = mock.mint_token(
        USER1,
        resources=authz_resources(parent_resource) + authz_resources(linked_resource),
    )
    script_api_key_login(mock, USER1, login_token, LINKER_API_KEY)
    for resource in (parent_resource, linked_resource):
        script_repository_lifecycle(mock, resource)
        script_partition_access(mock, USER1, login_token, resource, setup_token)

    actor = make_actor("linker")
    login_api_key(actor.make_repo(), auth_env.remote_url, LINKER_API_KEY)
    linked = actor.make_repo(repo_id=linked_id)
    linked.repository_create(repo_id=linked_id, identity=USER1.user_id)
    commit_file(linked, "inner.txt", "linked content")
    parent = actor.make_repo(repo_id=parent_id)
    parent.repository_create(repo_id=parent_id, identity=USER1.user_id)
    commit_file(parent, "own.txt", "parent content")
    parent.link_add("linked", linked_id, "/")
    parent.commit("mount the link")
    parent.push()

    latest = parent.branch_info().local_latest
    assert len(latest) == 64, f"expected a full revision signature, got {latest!r}"
    target = grpc_target(auth_env.remote_url)
    repository_id, signature = bytes.fromhex(parent_id), bytes.fromhex(latest)

    parent_only = mock.mint_token(USER1, resources=authz_resources(parent_resource))
    both = mock.mint_token(
        USER1,
        resources=authz_resources(parent_resource) + authz_resources(linked_resource),
    )

    def tree_paths(token: str) -> set[str]:
        return {
            node.path
            for node in revision_tree(
                target, repository_id, signature, authorization=token
            )
        }

    granted = tree_paths(both)
    assert {"own.txt", "linked", "linked/inner.txt"} <= granted, (
        f"a token granting both partitions must see the linked content, got {granted}"
    )

    denied = tree_paths(parent_only)
    assert {"own.txt", "linked"} <= denied, (
        f"the parent's own files and the link node must still be reported, got {denied}"
    )
    assert not [path for path in denied if path.startswith("linked/")], (
        f"a token without the linked partition's grant must not see beneath the "
        f"link, got {denied}"
    )

    probed = [
        check
        for check in mock.requests_for("CheckUserPermission")
        if check["bearer"] in (parent_only, both)
    ]
    assert not probed, (
        "the link-read verdicts must come from the access token's resources "
        f"claim, not an online check: {probed}"
    )


# ---------------------------------------------------------------------------
# CLI operations against the authorization rules
# ---------------------------------------------------------------------------


def provision_member(auth_env, make_actor, owner, label: str, permissions):
    """Log USER2 in with an API key and grant `permissions` on `owner`'s
    repository: the exchange and the online checks both answer for USER2's
    tokens, and the authorization token carries the matching resources claim
    so the QUIC data paths agree with the online checks."""
    mock = auth_env.mock
    login_token = mock.mint_token(USER2)
    authz_token = mock.mint_token(
        USER2, resources=authz_resources(owner.resource_id, permissions)
    )
    script_api_key_login(mock, USER2, login_token, USER2_API_KEY)
    script_partition_access(
        mock, USER2, login_token, owner.resource_id, authz_token, permissions
    )

    actor = make_actor(label)
    seed = actor.make_repo(remote_path=owner.repo.remote_path)
    login_api_key(seed, auth_env.remote_url, USER2_API_KEY)
    repo = seed.clone()
    repo.environment_vars.update(seed.environment_vars)
    return SimpleNamespace(repo=repo, login_token=login_token, authz_token=authz_token)


def revoke(mock, member, resource_id: str) -> None:
    """Newest rule wins: shadow the member's allows with denies."""
    for token in (member.login_token, member.authz_token):
        mock.on("CheckUserPermission", bearer=token, resource_id=resource_id).deny()


def lock_release_is_denied(repo: Lore, path: str) -> bool:
    """Whether a release failed to release: the CLI surfaces the server's
    ownership refusal either as an error or by not releasing the path."""
    try:
        released = repo.lock_release(path).released
    except LoreException:
        return True
    return path not in released


@pytest.mark.smoke
def test_cli_lock_operations_follow_the_grant(auth_env, make_actor):
    """`lore lock acquire/release/query` under the online rules: a granted
    member locks and releases their own locks; without `owner`/`admin` they
    cannot release the owner's lock; and after revocation every lock command
    is denied by the server, not the client."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "user1", USER1)
    commit_file(owner.repo, "locked.txt", "contested content")
    member = provision_member(
        auth_env, make_actor, owner, "member-rw", ("read", "write")
    )

    # A granted member manages their own locks.
    assert "locked.txt" in member.repo.lock_acquire("locked.txt").acquired
    assert "locked.txt" in member.repo.lock_release("locked.txt").released

    # The owner's lock is not theirs to release: `read`/`write` carry no
    # `owner`/`admin`, so the unlock stays owner-validated on the server.
    assert "locked.txt" in owner.repo.lock_acquire("locked.txt").acquired
    assert lock_release_is_denied(member.repo, "locked.txt")

    # Lock RPCs carry the exchanged access token, which is authorized from
    # its own `resources` claim: revoking the grant upstream does not reach
    # an unexpired access token, so lock commands keep working for the
    # token's lifetime. Revocation is observable on the identity-token paths
    # (see test_cli_push_and_sync_follow_the_grant).
    revoke(mock, member, owner.resource_id)
    assert "locked.txt" in member.repo.run(["lock", "query"])


@pytest.mark.smoke
def test_cli_admin_grant_releases_anothers_lock(auth_env, make_actor):
    """The unlock elevation end to end: `admin` on the partition lets a
    member's `lore lock release` take down the owner's lock, which is the
    LORE-211 owner/admin waiver answered by the online authorizer."""
    owner = provision_owner(auth_env, make_actor, "user1", USER1)
    commit_file(owner.repo, "locked.txt", "contested content")
    member = provision_member(
        auth_env, make_actor, owner, "member-admin", ("read", "write", "admin")
    )

    assert "locked.txt" in owner.repo.lock_acquire("locked.txt").acquired
    assert "locked.txt" in member.repo.lock_release("locked.txt").released


@pytest.mark.smoke
def test_cli_push_and_sync_follow_the_grant(auth_env, make_actor):
    """Push and sync — the revision and storage paths a user actually
    exercises — succeed for a granted member and are refused by the server
    once the grant is revoked, even though the member still holds valid,
    unexpired tokens."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "user1", USER1)
    commit_file(owner.repo, "one.txt", "from the owner")
    member = provision_member(
        auth_env, make_actor, owner, "member-push", ("read", "write")
    )

    # Granted: the member's work flows both ways. Each writer syncs before
    # committing, since both push to the same branch.
    commit_file(owner.repo, "three.txt", "more from the owner")
    member.repo.revision_sync()
    assert (Path(member.repo.path) / "three.txt").read_text() == "more from the owner"
    commit_file(member.repo, "two.txt", "from the member")

    # Revoked: data RPCs carry the exchanged access token, which is
    # authorized from its own `resources` claim — an unexpired token keeps
    # its grants, so push and sync continue for the token's lifetime, on
    # gRPC as on QUIC. Revocation is observable where the identity token is
    # the credential: the online check refuses the metadata write.
    revoke(mock, member, owner.resource_id)
    owner.repo.revision_sync()
    commit_file(owner.repo, "four.txt", "written after revocation")

    member.repo.revision_sync()
    assert (
        Path(member.repo.path) / "four.txt"
    ).read_text() == "written after revocation"
    commit_file(member.repo, "five.txt", "pushed with an unexpired token")

    with pytest.raises(LoreException):
        member.repo.repository_metadata_set(["probe", "refused"])

    denied_checks = [
        check
        for check in mock.requests_for("CheckUserPermission")
        if check["bearer"] in (member.login_token, member.authz_token)
        and owner.resource_id in check["resource_id"]
    ]
    assert denied_checks, "the refusals must be the online check's verdicts"


@pytest.mark.smoke
def test_cli_notification_subscribe_follows_the_grant(auth_env, make_actor):
    """`lore notification subscribe` under the online rules: a granted
    member's subscription is accepted and receives the owner's lock events
    until its listen window closes; once revoked, the subscribe bails out
    early with an error instead of holding a stream open."""
    mock = auth_env.mock
    owner = provision_owner(auth_env, make_actor, "user1", USER1)
    commit_file(owner.repo, "watched.txt", "watched content")
    member = provision_member(
        auth_env, make_actor, owner, "member-notify", ("read", "write")
    )

    result: dict = {}

    def subscribe():
        try:
            result["output"] = member.repo.run(["notification", "subscribe", "8"])
        except LoreException as error:
            result["error"] = error

    listener = threading.Thread(target=subscribe)
    listener.start()
    time.sleep(2)  # let the subscription establish before the event fires
    owner.repo.lock_acquire("watched.txt")
    time.sleep(1)
    owner.repo.lock_release("watched.txt")
    listener.join(timeout=30)

    assert not listener.is_alive(), "the listen window must close the subscriber"
    assert "error" not in result, f"granted subscribe failed: {result.get('error')}"
    assert "Subscribed to events" in result["output"]
    assert "Resource locked by" in result["output"]

    # An unexpired access token keeps its grants, so the denial to probe is
    # a token the auth service scoped to nothing: the subscribe handler reads
    # its empty `resources` claim, refuses the body-declared partition, and
    # the CLI bails out instead of listening.
    unscoped = mock.mint_token(USER2, resources=[])
    with pytest.raises(LoreException):
        member.repo.run(
            ["notification", "subscribe", "3"],
            identity_token=unscoped,
            access_token=unscoped,
        )


# ---------------------------------------------------------------------------
# Forwarded requests under authentication
# ---------------------------------------------------------------------------


@pytest.mark.smoke
class TestForwardedRepositoryGetWithAuth:
    """The forwarded repository-get path under authentication.

    The internal endpoint runs no JWT interceptor, so the target server must
    itself verify the end-user token the origin stamped into
    `on-behalf-of-authorization` and make its own access decision. Two
    authenticated servers share the module's stub: the origin forwards
    RepositoryGet to the target, whose store is the only one holding the
    repository, so a get that succeeds through the origin proves the
    delegation. A forwarded access token is answered from its own `resources`
    claim at the target; a token with no claim is checked online there — the
    origin never checks permissions for a get it forwards, so every recorded
    check for these probes is the target's."""

    @pytest.fixture(scope="class")
    def server_hostname(self, request):
        return request.config.getoption("--lore-server-hostname")

    @pytest.fixture(scope="class")
    def target_server(
        self,
        request,
        tmp_path_factory,
        auth_env,
        server_hostname,
        lore_server_executable_path,
    ):
        """The delegation target: authenticated against the module's stub,
        internal gRPC endpoint enabled without mTLS so the origin can reach it
        over plain HTTP/2."""
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
        server_env["LORE__SERVER__GRPC_INTERNAL__ENABLED"] = "true"
        server_env["LORE__SERVER__GRPC_INTERNAL__VERIFY_CLIENT_CERTS"] = "false"
        append_auth_config(server_root, auth_env.mock)

        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        try:
            yield SimpleNamespace(
                remote_url=f"lore://{server_hostname}:{shared_port}/",
                internal_port=ports["internal"],
            )
        finally:
            _kill_server_by_pid(
                server_proc.pid, log_path, label="forwarded-auth target server"
            )
            log_fd.close()

    @pytest.fixture(scope="class")
    def origin_server(
        self,
        request,
        tmp_path_factory,
        auth_env,
        target_server,
        server_hostname,
        lore_server_executable_path,
    ):
        """The delegation origin: authenticated against the same stub, and
        forwarding RepositoryGet to the target's internal port. Depends on
        `target_server` because a delegating server connects to its peer while
        starting up."""
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
        append_auth_config(server_root, auth_env.mock)

        local_toml = server_root / "lore-server" / "config" / "local.toml"
        with open(local_toml, "a", encoding="utf-8") as f:
            f.write("[server.grpc_public_services.forwarded_requests.client]\n")
            f.write(f'url = "http://{server_hostname}:{target_server.internal_port}"\n')
            f.write("[server.grpc_public_services.forwarded_requests.enabled_rpcs]\n")
            f.write("repository_get = true\n")

        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        try:
            yield SimpleNamespace(grpc_target=f"{server_hostname}:{shared_port}")
        finally:
            _kill_server_by_pid(
                server_proc.pid, log_path, label="forwarded-auth origin server"
            )
            log_fd.close()

    @pytest.mark.smoke
    def test_forwarded_get_rechecks_the_token_at_the_target(
        self, auth_env, target_server, origin_server, make_actor
    ):
        """A granted user's get resolves through the origin, with the target
        answering the forwarded access token from its `resources` claim — no
        online check carries it, since the auth service refuses access tokens
        as credentials; a verifiable token with no claim and no grant is
        checked online at the target, denied, and the caller sees NOT_FOUND,
        the same shape a direct denial answers."""
        mock = auth_env.mock

        # USER1 provisions a repository on the target, exactly as
        # provision_owner does against the module server.
        repo_id = uuid.uuid4().hex
        resource_id = f"urc-{repo_id}"
        api_key = "user1-forwarded-key"
        login_token = mock.mint_token(USER1)
        authz_token = mock.mint_token(USER1, resources=authz_resources(resource_id))
        script_api_key_login(mock, USER1, login_token, api_key)
        script_repository_lifecycle(mock, resource_id)
        script_partition_access(mock, USER1, login_token, resource_id, authz_token)

        actor = make_actor("forwarded-user1")
        login_api_key(
            actor.make_repo(remote_url=target_server.remote_url),
            target_server.remote_url,
            api_key,
        )
        repo = actor.make_repo(remote_url=target_server.remote_url, repo_id=repo_id)
        repo.repository_create(repo_id=repo_id, identity=USER1.user_id)

        # Granted: the origin's own store never held the repository, so the
        # answer comes from the delegation.
        checks_before = len(mock.requests_for("CheckUserPermission"))
        code, body, details = call(
            origin_server.grpc_target,
            REPOSITORY_GET,
            repository_get_by_name_request(repo.name),
            metadata=(("authorization", f"Bearer {authz_token}"),),
        )
        assert code == grpc.StatusCode.OK, (
            f"the forwarded get should succeed for a granted user, got {code} '{details}'"
        )
        assert repository_name_in_response(body) == repo.name

        access_token_checks = [
            check
            for check in mock.requests_for("CheckUserPermission")[checks_before:]
            if check["bearer"] == authz_token
        ]
        assert not access_token_checks, (
            "the forwarded access token is answered from its claim at the "
            "target; sending it to CheckUserPermission would be refused"
        )

        # Denied: the token verifies (same issuer and JWKS) but holds no
        # grant, so the target's check denies it. The caller sees NOT_FOUND,
        # not PERMISSION_DENIED. Denial must not disclose that the
        # repository exists, on the forwarded path as on the direct one.
        denied_token = mock.mint_token(USER2)
        code, _body, details = call(
            origin_server.grpc_target,
            REPOSITORY_GET,
            repository_get_by_name_request(repo.name),
            metadata=(("authorization", f"Bearer {denied_token}"),),
        )
        assert code == grpc.StatusCode.NOT_FOUND, (
            f"a denied forwarded get must answer NOT_FOUND, got {code} '{details}'"
        )

        denied_checks = [
            check
            for check in mock.requests_for("CheckUserPermission")
            if check["bearer"] == denied_token
        ]
        assert denied_checks, (
            "the denial must come from the target's online check, not from an "
            "earlier failure on the way there"
        )
