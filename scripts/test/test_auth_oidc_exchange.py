# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Smoke tests for Tier 2 over the OIDC path: the partition-scoped token
exchange (RFC 8693) against a stub provider, and the server authorizing from
the `resources` claim the exchanged token carries.

The module launches one lore server per carrier. Each advertises the stub as
its OIDC issuer and names partitions with `resource_template` or
`scope_template`, and `[server.auth] resource_claim` selects
`ResourceGrantsAuthorizer`. The stub (see mock_oidc_provider.py) serves the
device grant for login and the exchange grant, minting a `resources` claim
for the partitions a test grants.

The same path against a real RFC 8693 issuer is the fixture repository's job.
"""

import base64
import json
import time
import uuid
from types import SimpleNamespace

import pytest
from error_types import LoreException
from lore_server import (
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)
from mock_oidc_provider import (
    ACCESS_TOKEN_TYPE,
    REFRESH_TOKEN_GRANT,
    TOKEN_EXCHANGE_GRANT,
    USER,
    MockOidcProvider,
)

from lore import Lore

pytestmark = pytest.mark.xdist_group("auth_oidc_exchange")

RESOURCE_TEMPLATE = "https://lore.test.invalid/partitions/{id}"
SCOPE_TEMPLATE = "partition:{id}"
GRANTED = ["admin", "write", "read"]

OIDC_CONFIG = """

# --- appended by test_auth_oidc_exchange.py: Tier 2 over an OIDC issuer ---
[server.auth]
jwt_issuer = "{issuer}"
jwt_audience = ["{audience}"]
resource_claim = "resources"

[server.auth.oidc]
client_id = "{client_id}"
scopes = ["openid"]
{carrier}_template = "{template}"
"""


@pytest.fixture(scope="module", params=["resource", "scope"])
def exchange_env(request, tmp_path_factory, lore_server_executable_path):
    """A Tier 2 server advertising the stub, naming partitions with the
    carrier the parameter names."""
    carrier = request.param
    template = RESOURCE_TEMPLATE if carrier == "resource" else SCOPE_TEMPLATE
    provider = MockOidcProvider().start()
    provider.exchange_carrier = carrier
    provider.exchange_template = template

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
            carrier=carrier,
            template=template,
        )
    )
    server_proc, server_log_path, server_log_fd = launch_lore_server(
        server_root, server_env, lore_server_executable_path
    )
    try:
        yield SimpleNamespace(
            provider=provider,
            carrier=carrier,
            template=template,
            remote_url=f"lore://127.0.0.1:{shared_port}/",
        )
    finally:
        _kill_server_by_pid(
            server_proc.pid, server_log_path, label="auth-oidc-exchange server"
        )
        server_log_fd.close()
        provider.stop()


@pytest.fixture(autouse=True)
def _reset_provider(exchange_env):
    exchange_env.provider.reset()


@pytest.fixture
def make_repo(exchange_env, new_lore_repo, scratch_dir):
    """A repository against the Tier 2 server, sharing one token store per
    test, so a login reaches every repository the test makes."""
    auth_store = scratch_dir("auth-store", create=True)

    def _make_repo(repo_id: str) -> Lore:
        return new_lore_repo(
            remote_url=exchange_env.remote_url,
            repo_id=repo_id,
            create_repo=False,
            environment_vars={"LORE_AUTH_PATH": str(auth_store)},
        )

    return _make_repo


def granted_repository(exchange_env, make_repo) -> tuple[Lore, str]:
    """A repository created on the server after a login, which the stub
    grants `GRANTED` on, and its id."""
    repo_id = uuid.uuid4().hex
    exchange_env.provider.grants[repo_id] = GRANTED
    repo = make_repo(repo_id)
    repo.run(urc_args=["auth", "login", exchange_env.remote_url, "--no-browser"])
    repo.repository_create(repo_id=repo_id)
    return repo, repo_id


def seed_commit_and_push(repo: Lore, **kwargs) -> None:
    with repo.open_file("seed.txt", "w+") as f:
        f.write("seed\n")
    repo.stage(scan=True, **kwargs)
    repo.commit("seed", **kwargs)
    repo.push(**kwargs)


def exchanges(provider: MockOidcProvider):
    return [
        r
        for r in provider.requests_to("/token")
        if r.form.get("grant_type") == TOKEN_EXCHANGE_GRANT
    ]


def decoded_claims(token: str) -> dict:
    payload = token.split(".")[1]
    return json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))


def resources_token(provider: MockOidcProvider, repo_id: str) -> str:
    """A token granting `GRANTED` on `repo_id` alone, as an exchange mints."""
    return provider.mint_token(
        USER, resources=[{"resource_id": f"urc-{repo_id}", "permission": GRANTED}]
    )


@pytest.mark.smoke
def test_a_push_exchanges_the_login_token_for_the_partition(
    exchange_env, make_repo
):
    """The push exchanges the login token for one naming the partition in
    exactly one parameter, the configured carrier, and the server accepts
    the exchanged token's `resources` claim for it."""
    provider = exchange_env.provider
    repo, repo_id = granted_repository(exchange_env, make_repo)

    seed_commit_and_push(repo)

    sent = exchanges(provider)
    assert sent, "the push exchanged no token"
    other = "scope" if exchange_env.carrier == "resource" else "resource"
    named = exchange_env.template.replace("{id}", repo_id)
    for request in sent:
        carried = [value for name, value in request.fields if name == exchange_env.carrier]
        assert carried == [named], request.fields
        assert other not in request.form, request.fields
        assert request.form["subject_token_type"] == ACCESS_TOKEN_TYPE
        assert request.form["client_id"] == provider.client_id
        assert "resources" not in decoded_claims(request.form["subject_token"])


@pytest.mark.smoke
def test_a_push_after_the_login_expires_refreshes_before_the_exchange(
    exchange_env, make_repo
):
    """Once the login token and the partition token have expired, a push
    refreshes the login token before it exchanges it for the partition, and
    goes through without a new login."""
    provider = exchange_env.provider
    # Long enough to log in and create the repository, short enough to wait out.
    provider.token_lifetime = 3
    repo, _ = granted_repository(exchange_env, make_repo)
    time.sleep(4)
    provider.token_lifetime = 3600
    provider.reset_requests()

    seed_commit_and_push(repo)

    grants = [r.form.get("grant_type") for r in provider.requests_to("/token")]
    assert grants and grants[0] == REFRESH_TOKEN_GRANT, grants
    assert grants.count(REFRESH_TOKEN_GRANT) == 1, grants
    assert TOKEN_EXCHANGE_GRANT in grants, grants


@pytest.mark.smoke
def test_a_partition_the_provider_will_not_grant_is_not_reached(
    exchange_env, make_repo
):
    """The stub refuses the exchange for a partition it grants nothing on, so
    the client has no token for it and the server refuses the push."""
    provider = exchange_env.provider
    repo, repo_id = granted_repository(exchange_env, make_repo)
    del provider.grants[repo_id]

    with pytest.raises(LoreException):
        seed_commit_and_push(repo)

    assert exchanges(provider), "the client never asked for the partition"


@pytest.mark.smoke
def test_a_partition_scoped_token_reaches_its_partition_alone(
    exchange_env, make_repo
):
    """A token whose `resources` claim names one partition is accepted for
    that partition and denied for another: it clones the first and not the
    second."""
    provider = exchange_env.provider
    first, first_id = granted_repository(exchange_env, make_repo)
    second, _ = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(first)
    seed_commit_and_push(second)
    scoped = resources_token(provider, first_id)

    first.clone(identity_token=scoped, access_token=scoped)
    with pytest.raises(LoreException):
        second.clone(identity_token=scoped, access_token=scoped)


@pytest.mark.smoke
def test_the_login_token_alone_reads_no_partition(exchange_env, make_repo):
    """Without an exchange the client holds no grant on any partition: the
    login token, sent as the authorization itself, cannot clone one, while a
    token scoped to the partition can."""
    provider = exchange_env.provider
    repo, repo_id = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(repo)
    login_token = provider.mint_token(USER)

    with pytest.raises(LoreException):
        repo.clone(identity_token=login_token, access_token=login_token)

    scoped = resources_token(provider, repo_id)
    repo.clone(identity_token=scoped, access_token=scoped)


@pytest.mark.smoke
def test_repository_operations_need_only_the_stored_login_token(
    exchange_env, make_repo
):
    """With only the login token in the store, the lookups by name and by id
    resolve with it, and the calls about one partition carry that partition's
    exchanged token: clone by name and by id, info with and without a URL,
    metadata set and get, and delete."""
    remote_url = exchange_env.remote_url
    repo, repo_id = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(repo)

    repo.clone()
    repo.run(
        ["repository", "clone", f"{remote_url}{repo_id}", f"{repo.path}-by-id"]
    )
    assert repo_id in repo.repository_info()
    assert repo_id in repo.repository_info(f"{remote_url}{repo.name}")
    repo.repository_metadata_set(["probe", "set-with-the-partition-token"])
    assert "set-with-the-partition-token" in repo.repository_metadata_get("probe")
    repo.repository_delete()

    sent = exchanges(exchange_env.provider)
    named = exchange_env.template.replace("{id}", repo_id)
    assert sent, "no call exchanged the login token"
    assert all(
        request.form[exchange_env.carrier] == named for request in sent
    ), [request.fields for request in sent]


@pytest.mark.smoke
def test_a_lookup_by_name_does_not_open_an_ungranted_partition(
    exchange_env, make_repo, new_lore_repo, scratch_dir
):
    """Any verified caller resolves the name, but the exchange for a partition
    the stub grants nothing on is refused, so the clone fails. The clone runs
    from a client with a token store of its own, which holds no partition
    token from the owner's push."""
    provider = exchange_env.provider
    repo, repo_id = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(repo)
    del provider.grants[repo_id]
    other = new_lore_repo(
        remote_url=exchange_env.remote_url,
        create_repo=False,
        environment_vars={
            "LORE_AUTH_PATH": str(scratch_dir("other-auth-store", create=True))
        },
    )
    other.run(urc_args=["auth", "login", exchange_env.remote_url, "--no-browser"])
    provider.reset_requests()

    with pytest.raises(LoreException):
        other.run(
            [
                "repository",
                "clone",
                f"{exchange_env.remote_url}{repo.name}",
                f"{other.path}-denied",
            ]
        )

    assert exchanges(provider), "the client never asked for the partition"
