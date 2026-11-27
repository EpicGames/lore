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
from pathlib import Path
from types import SimpleNamespace

import pytest
from error_types import LocalModificationsError, LoreException
from lore_parsers import parse_jsonl
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
from test_utils import link_entries

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


def client_with_own_token_store(
    exchange_env, new_lore_repo, scratch_dir, store: str, repo_id: str | None = None
) -> Lore:
    """A client logged in from a token store of its own, which holds no
    partition token another client's calls fetched."""
    client = new_lore_repo(
        remote_url=exchange_env.remote_url,
        repo_id=repo_id,
        create_repo=False,
        environment_vars={"LORE_AUTH_PATH": str(scratch_dir(store, create=True))},
    )
    client.run(urc_args=["auth", "login", exchange_env.remote_url, "--no-browser"])
    return client


PIN_FORMS = [
    "signature",
    "name@LATEST",
    "id@LATEST",
    "name@signature",
    "id@signature",
]


def pin_in_form(repo: Lore, form: str) -> str:
    """A pin on the latest revision of `repo`'s current branch, written in
    `form`, one of `PIN_FORMS`."""
    branch = repo.branch_info()
    prefix, _, target = form.rpartition("@")
    revision = "LATEST" if target == "LATEST" else branch.local_latest
    return {
        "": revision,
        "name": f"{branch.name}@{revision}",
        "id": f"{branch.id}@{revision}",
    }[prefix]


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
def test_a_push_exchanges_the_login_token_for_the_partition(exchange_env, make_repo):
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
        carried = [
            value for name, value in request.fields if name == exchange_env.carrier
        ]
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
@pytest.mark.own_vfs(
    "withdraws the grant after the repository is created, where a virtual clone's mount "
    "exchanged its token for the partition when it connected"
)
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
def test_a_partition_scoped_token_reaches_its_partition_alone(exchange_env, make_repo):
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
    repo.run(["repository", "clone", f"{remote_url}{repo_id}", f"{repo.path}-by-id"])
    assert repo_id in repo.repository_info()
    assert repo_id in repo.repository_info(f"{remote_url}{repo.name}")
    repo.repository_metadata_set(["probe", "set-with-the-partition-token"])
    assert "set-with-the-partition-token" in repo.repository_metadata_get("probe")
    repo.repository_delete()

    sent = exchanges(exchange_env.provider)
    named = exchange_env.template.replace("{id}", repo_id)
    assert sent, "no call exchanged the login token"
    assert all(request.form[exchange_env.carrier] == named for request in sent), [
        request.fields for request in sent
    ]


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


@pytest.mark.smoke
@pytest.mark.parametrize("pin_form", [None, *PIN_FORMS])
def test_a_link_to_an_ungranted_repository_is_refused_naming_it(
    exchange_env, make_repo, new_lore_repo, scratch_dir, pin_form
):
    """Adding a link to a repository the stub grants nothing on, at its latest
    or pinned without branching, fails as an authorization failure naming the
    linked repository, with no access hint. The parent runs from a
    token store of its own, which holds no partition token from the linked
    repository's push."""
    provider = exchange_env.provider
    linked, linked_id = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(linked)
    pin = pin_in_form(linked, pin_form) if pin_form else None
    del provider.grants[linked_id]

    parent_id = uuid.uuid4().hex
    provider.grants[parent_id] = GRANTED
    parent = client_with_own_token_store(
        exchange_env, new_lore_repo, scratch_dir, "parent-auth-store", parent_id
    )
    parent.repository_create(repo_id=parent_id)
    seed_commit_and_push(parent)

    with pytest.raises(LoreException) as denied:
        parent.link_add("lib", linked_id, "/", pin=pin, disable_branching=bool(pin))

    assert_refused_naming(denied.value, linked_id)


@pytest.mark.smoke
def test_a_link_that_cannot_branch_the_linked_repository_is_refused_naming_it(
    exchange_env, make_repo, new_lore_repo, scratch_dir
):
    """Adding a link from a branch the linked repository lacks, with only read
    access to that repository, fails as an authorization failure when creating
    the branch there, naming the linked repository."""
    provider = exchange_env.provider
    linked, linked_id = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(linked)
    provider.grants[linked_id] = ["read"]

    parent_id = uuid.uuid4().hex
    provider.grants[parent_id] = GRANTED
    parent = client_with_own_token_store(
        exchange_env, new_lore_repo, scratch_dir, "parent-auth-store", parent_id
    )
    parent.repository_create(repo_id=parent_id)
    seed_commit_and_push(parent)
    parent.branch_create("feature")

    with pytest.raises(LoreException) as denied:
        parent.link_add("lib", linked_id, "/")

    assert_refused_naming(denied.value, linked_id)


def denied_clone_of_a_link(
    exchange_env, make_repo, new_lore_repo, scratch_dir, prepare=None
) -> tuple[Lore, str, Lore, Lore]:
    """A fresh clone of a parent that linked a repository while the stub
    granted it, made after the grant is revoked, the linked repository's id,
    the linked repository, and the parent. The clone runs from a token store of its own, which
    holds no partition token from the link's addition. `prepare(linked, parent)` runs once the
    link is pushed, while the grant still holds."""
    provider = exchange_env.provider
    linked, linked_id = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(linked)
    parent, _ = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(parent)
    parent.link_add("lib", linked_id, "/")
    parent.commit("link lib")
    parent.push()
    if prepare:
        prepare(linked, parent)
    del provider.grants[linked_id]

    denied = client_with_own_token_store(
        exchange_env, new_lore_repo, scratch_dir, "denied-auth-store"
    )
    Path(denied.path).mkdir()
    denied.run(["repository", "clone", parent.remote_path, denied.path])
    return denied, linked_id, linked, parent


def assert_refused_naming(refused: LoreException, linked_id: str):
    output = str(refused)
    assert refused.returncode in (16, 17), output
    assert f"linked repository {linked_id}" in output, output
    assert "Request access" not in output, output


@pytest.mark.smoke
def test_listing_links_from_a_clone_without_access_to_one_lists_the_others(
    exchange_env, make_repo, new_lore_repo, scratch_dir
):
    """Listing links from a clone without access to one linked repository
    lists the link it can read, and lists both once access is granted again."""
    denied, linked_id, _, parent = denied_clone_of_a_link(
        exchange_env, make_repo, new_lore_repo, scratch_dir
    )
    app, app_id = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(app)
    parent.link_add("app", app_id, "/")
    parent.commit("link app")
    parent.push()
    denied.sync()

    def listed() -> list[tuple[str, str]]:
        links = link_entries(denied)
        return sorted((link["linkPath"], link["link"]) for link in links)

    assert listed() == [("app", app_id)]
    exchange_env.provider.grants[linked_id] = GRANTED
    assert listed() == [("app", app_id), ("lib", linked_id)]


@pytest.mark.smoke
def test_updating_or_removing_a_link_to_a_repository_no_longer_granted_is_refused_naming_it(
    exchange_env, make_repo, new_lore_repo, scratch_dir
):
    """Updating or removing a link from a clone without access to the linked
    repository fails as an authorization failure naming the linked repository
    and the link's path, and leaves the link in place."""
    denied, linked_id, _, parent = denied_clone_of_a_link(
        exchange_env, make_repo, new_lore_repo, scratch_dir
    )

    for change in (denied.link_update, denied.link_remove):
        with pytest.raises(LoreException) as refused:
            change("lib")

        assert_refused_naming(refused.value, linked_id)
    assert "lib" in str(refused.value)
    commit_a_file(denied, "a.txt", "edit\n")
    denied.push()
    parent.sync()
    links = link_entries(parent)
    assert [link["linkPath"] for link in links] == ["lib"]


@pytest.mark.smoke
@pytest.mark.parametrize("pin_form", PIN_FORMS)
def test_updating_a_link_to_a_pin_in_a_repository_no_longer_granted_is_refused_naming_it(
    exchange_env, make_repo, new_lore_repo, scratch_dir, pin_form
):
    """Updating a link to a pinned revision or branch latest from a clone
    without access to the linked repository fails as an authorization failure
    naming the linked repository."""
    denied, linked_id, linked, _ = denied_clone_of_a_link(
        exchange_env, make_repo, new_lore_repo, scratch_dir
    )
    pin = pin_in_form(linked, pin_form)

    with pytest.raises(LoreException) as refused:
        denied.link_update("lib", pin=pin)

    assert_refused_naming(refused.value, linked_id)


@pytest.mark.smoke
def test_listing_staged_links_from_a_repository_no_longer_granted_lists_none(
    exchange_env, make_repo, new_lore_repo, scratch_dir
):
    """Listing links with staged changes from a clone without access to the
    linked repository, with a file of the parent staged, succeeds and lists no
    link."""
    denied, _, _, _ = denied_clone_of_a_link(
        exchange_env, make_repo, new_lore_repo, scratch_dir
    )
    with denied.open_file("staged.txt", "w+") as f:
        f.write("staged\n")
    denied.stage("staged.txt")

    listed = denied.link_list(staged=True, json=True)

    assert parse_jsonl(listed, "linkStagedEntry") == [], listed


def commit_a_file(repo: Lore, name: str, content: str) -> str:
    with repo.open_file(name, "w+") as f:
        f.write(content)
    repo.stage(name)
    return repo.commit(f"edit {name}")


@pytest.mark.smoke
def test_a_clone_without_access_to_a_linked_repository_pushes_what_leaves_the_link_alone(
    exchange_env, make_repo, new_lore_repo, scratch_dir
):
    """A clone without access to a linked repository pushes two commits that
    leave the link's pin as the remote holds it, and the parent's owner syncs
    both, with the link still at its pin."""
    denied, linked_id, linked, parent = denied_clone_of_a_link(
        exchange_env, make_repo, new_lore_repo, scratch_dir
    )
    pin = linked.branch_info().local_latest
    commit_a_file(denied, "a.txt", "first\n")
    commit_a_file(denied, "b.txt", "second\n")

    denied.push()

    assert denied.branch_info().remote_latest == denied.branch_info().local_latest
    parent.sync()
    assert parent.read_files()["a.txt"] == "first\n"
    assert parent.read_files()["b.txt"] == "second\n"
    links = link_entries(parent)
    assert [(link["linkPath"], link["link"], link["revision"]) for link in links] == [
        ("lib", linked_id, pin)
    ]


@pytest.mark.smoke
def test_a_commit_in_a_clone_without_access_to_a_linked_repository_reports_only_its_success(
    exchange_env, make_repo, new_lore_repo, scratch_dir
):
    """A commit in a clone without access to a linked repository reports
    success and nothing else."""
    denied, _, _, _ = denied_clone_of_a_link(
        exchange_env, make_repo, new_lore_repo, scratch_dir
    )

    output = commit_a_file(denied, "a.txt", "edit\n")

    assert output.strip().splitlines()[-1] == "Commit succeeded", output
    assert "Not authorized" not in output, output


@pytest.mark.smoke
def test_a_clone_without_access_to_a_linked_repository_works_around_the_link(
    exchange_env, make_repo, new_lore_repo, scratch_dir
):
    """A clone without access to a linked repository syncs, switches branches,
    reverts, scans and resets the files around the link, leaving what the
    link's directory holds."""
    denied, _, _, parent = denied_clone_of_a_link(
        exchange_env, make_repo, new_lore_repo, scratch_dir
    )
    commit_a_file(parent, "a.txt", "main\n")
    parent.push()
    parent.branch_create("other")
    commit_a_file(parent, "b.txt", "other\n")
    parent.push()

    def edit_a() -> None:
        with denied.open_file("a.txt", "w+") as f:
            f.write("edited\n")

    denied.sync()
    assert denied.read_files()["a.txt"] == "main\n"
    denied.branch_switch("other")
    assert denied.read_files()["b.txt"] == "other\n"

    Path(denied.path, "lib").mkdir(exist_ok=True)
    with denied.open_file("lib/x.txt", "w+") as f:
        f.write("x\n")
    edit_a()
    assert "M a.txt" in denied.status(scan=True)
    denied.reset()
    assert denied.read_files()["a.txt"] == "main\n"
    edit_a()
    denied.sync(reset=True)
    files = denied.read_files()
    assert (files["a.txt"], files["lib/x.txt"]) == ("main\n", "x\n")

    denied.revision_revert(denied.branch_info().local_latest, message="revert b")
    assert "b.txt" not in denied.read_files()


def repin(linked: Lore, parent: Lore, name: str) -> None:
    commit_a_file(linked, f"{name}.txt", f"{name}\n")
    linked.push()
    parent.link_update("lib", pin=linked.branch_info().local_latest)
    parent.commit(f"repin lib to {name}")
    parent.push()


@pytest.mark.smoke
def test_a_clone_without_access_to_a_linked_repository_takes_pins_others_moved(
    exchange_env, make_repo, new_lore_repo, scratch_dir
):
    """A branch switch, a sync, with and without a reset, and a revert that
    move the pin of a link the clone cannot read succeed, leaving what the
    link's directory holds."""

    def branch_then_repin(linked: Lore, parent: Lore) -> None:
        parent.branch_create("before-repin")
        parent.push()
        parent.branch_switch("main")
        repin(linked, parent, "first")

    denied, linked_id, linked, parent = denied_clone_of_a_link(
        exchange_env, make_repo, new_lore_repo, scratch_dir, branch_then_repin
    )
    exchange_env.provider.grants[linked_id] = GRANTED
    repin(linked, parent, "second")
    del exchange_env.provider.grants[linked_id]
    Path(denied.path, "lib").mkdir(exist_ok=True)
    with denied.open_file("lib/x.txt", "w+") as f:
        f.write("x\n")

    denied.branch_switch("before-repin")
    assert denied.branch_info().name == "before-repin"
    denied.branch_switch("main")
    denied.sync()
    assert denied.branch_info().local_latest == parent.branch_info().local_latest
    denied.sync(reset=True)
    assert denied.read_files()["lib/x.txt"] == "x\n"
    assert "Committed reverted repository state" in denied.revision_revert(
        denied.branch_info().local_latest, message="unpin"
    )


@pytest.mark.smoke
def test_a_sync_removing_a_link_around_one_the_clone_cannot_read_keeps_what_its_mount_holds(
    exchange_env, make_repo, new_lore_repo, scratch_dir
):
    """A sync removing a link that holds a link the clone cannot read keeps
    the outer mount while the inner one holds files, and removes it once the
    inner mount is empty."""
    provider = exchange_env.provider
    inner, inner_id = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(inner)
    outer, outer_id = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(outer)
    outer.link_add("inner", inner_id, "/")
    outer.commit("link inner")
    outer.push()
    parent, _ = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(parent)
    parent.link_add("lib", outer_id, "/")
    parent.commit("link lib")
    parent.push()
    del provider.grants[inner_id]
    denied = client_with_own_token_store(
        exchange_env, new_lore_repo, scratch_dir, "denied-auth-store"
    )
    Path(denied.path).mkdir()
    denied.run(["repository", "clone", parent.remote_path, denied.path])
    Path(denied.path, "lib", "inner").mkdir(exist_ok=True)
    with denied.open_file("lib/inner/x.txt", "w+") as f:
        f.write("x\n")
    provider.grants[inner_id] = GRANTED
    parent.link_remove("lib")
    parent.commit("unlink lib")
    parent.push()
    del provider.grants[inner_id]

    with pytest.raises(LocalModificationsError):
        denied.sync()
    assert denied.read_files()["lib/inner/x.txt"] == "x\n"
    denied.remove_file("lib/inner/x.txt")
    denied.sync()

    assert denied.branch_info().local_latest == parent.branch_info().local_latest
    assert "lib/seed.txt" not in denied.read_files()


@pytest.mark.smoke
def test_a_reset_in_a_clone_that_lost_access_skips_a_link_staged_at_a_pin_it_cannot_read(
    exchange_env, make_repo, new_lore_repo, scratch_dir
):
    """A reset in a clone that held a link's pin before losing access to the
    linked repository, with a merge staging a pin it never read, leaves the
    link's mount alone."""
    provider = exchange_env.provider
    linked, linked_id = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(linked)
    parent, _ = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(parent)
    parent.link_add("lib", linked_id, "/")
    parent.commit("link lib")
    parent.push()
    parent.branch_create("feature")
    parent.push()
    repin(linked, parent, "first")
    parent.branch_switch("main")
    commit_a_file(parent, "m.txt", "m\n")
    parent.push()
    clone = client_with_own_token_store(
        exchange_env, new_lore_repo, scratch_dir, "granted-auth-store"
    )
    Path(clone.path).mkdir()
    clone.run(["repository", "clone", parent.remote_path, clone.path])
    assert clone.read_files()["lib/seed.txt"] == "seed\n"
    del provider.grants[linked_id]
    clone.environment_vars["LORE_AUTH_PATH"] = str(
        scratch_dir("denied-auth-store", create=True)
    )
    clone.run(urc_args=["auth", "login", exchange_env.remote_url, "--no-browser"])
    clone.branch_merge_start("feature", no_commit=True)

    clone.reset(".")

    assert clone.read_files()["lib/seed.txt"] == "seed\n"


@pytest.mark.smoke
def test_a_switch_in_a_clone_that_lost_access_leaves_a_folder_link_pinned_where_it_cannot_read(
    exchange_env, make_repo, new_lore_repo, scratch_dir
):
    """A switch in a clone that lost access to a linked repository, from a pin
    of a folder link it never read to one it holds, succeeds."""
    provider = exchange_env.provider
    linked, linked_id = granted_repository(exchange_env, make_repo)
    Path(linked.path, "a").mkdir()
    commit_a_file(linked, "a/a.txt", "a\n")
    linked.push()
    parent, _ = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(parent)
    parent.link_add("lib", linked_id, "/a")
    parent.commit("link lib")
    parent.push()
    parent.branch_create("other")
    parent.push()
    repin(linked, parent, "other")
    parent.branch_switch("main")
    clone = client_with_own_token_store(
        exchange_env, new_lore_repo, scratch_dir, "granted-auth-store"
    )
    Path(clone.path).mkdir()
    clone.run(["repository", "clone", parent.remote_path, clone.path])
    clone.branch_switch("other")
    clone.branch_switch("main")
    del provider.grants[linked_id]
    clone.environment_vars["LORE_AUTH_PATH"] = str(
        scratch_dir("denied-auth-store", create=True)
    )
    clone.run(urc_args=["auth", "login", exchange_env.remote_url, "--no-browser"])
    provider.grants[linked_id] = GRANTED
    repin(linked, parent, "main")
    del provider.grants[linked_id]
    clone.sync()

    clone.branch_switch("other")

    assert clone.branch_info().name == "other"


@pytest.mark.smoke
def test_a_sync_replacing_a_link_with_one_the_clone_cannot_read_removes_the_old_mount(
    exchange_env, make_repo, new_lore_repo, scratch_dir
):
    """A sync that replaces a link with one to a repository the clone cannot
    read removes the replaced link's mount, and a switch back to a branch
    holding the replaced link materializes it again."""
    provider = exchange_env.provider
    readable, readable_id = granted_repository(exchange_env, make_repo)
    refusing, refusing_id = granted_repository(exchange_env, make_repo)
    parent, _ = granted_repository(exchange_env, make_repo)
    for repo in (readable, refusing, parent):
        seed_commit_and_push(repo)
    parent.link_add("lib", readable_id, "/")
    parent.commit("link lib")
    parent.push()
    parent.branch_create("readable")
    parent.push()
    parent.branch_switch("main")
    parent.link_remove("lib")
    parent.link_add("lib", refusing_id, "/")
    parent.commit("relink lib")
    del provider.grants[refusing_id]
    clone = client_with_own_token_store(
        exchange_env, new_lore_repo, scratch_dir, "denied-auth-store"
    )
    Path(clone.path).mkdir()
    clone.run(["repository", "clone", parent.remote_path, clone.path])
    clone.branch_switch("readable")
    assert clone.read_files()["lib/seed.txt"] == "seed\n"
    parent.push()

    clone.branch_switch("main")

    assert clone.branch_info().local_latest == parent.branch_info().local_latest
    assert sorted(clone.read_files()) == ["seed.txt"]
    clone.branch_switch("readable")
    assert clone.read_files()["lib/seed.txt"] == "seed\n"


@pytest.mark.smoke
def test_a_push_repinning_a_link_the_token_cannot_read_is_refused_naming_it(
    exchange_env, make_repo
):
    """A push of a revision that moves a link's pin, with a token granting the
    parent alone, is refused with the authorization failure's exit code before
    anything is pushed, and names the link and the linked repository."""
    provider = exchange_env.provider
    linked, linked_id = granted_repository(exchange_env, make_repo)
    parent, parent_id = granted_repository(exchange_env, make_repo)
    seed_commit_and_push(linked)
    seed_commit_and_push(parent)
    parent.link_add("lib", linked_id, "/")
    parent.commit("link lib")
    parent.push()
    pushed = parent.branch_info().local_latest
    commit_a_file(linked, "next.txt", "next\n")
    linked.push()
    parent.link_update("lib", pin=linked.branch_info().local_latest)
    parent.commit("repin lib")
    scoped = provider.mint_token(
        USER,
        resources=[{"resource_id": f"urc-{parent_id}", "permission": GRANTED}],
    )

    with pytest.raises(LoreException) as refused:
        parent.push(identity_token=scoped, access_token=scoped)

    assert refused.value.returncode == 17, "the refusal must keep its exit code"
    assert "link at lib" in str(refused.value), str(refused.value)
    assert f"linked repository {linked_id}" in str(refused.value), str(refused.value)
    assert parent.clone().branch_info().local_latest == pushed


@pytest.mark.smoke
def test_a_branch_create_skips_the_linked_repositories_that_refuse_it_and_keeps_their_pins(
    exchange_env, make_repo
):
    """A branch create whose token grants nothing on two linked repositories
    switches the parent to the new branch, makes it in the other linked
    repository, takes it back from a repository linked only below a refusing
    one, and leaves the links to the two at their pins and branch."""
    provider = exchange_env.provider
    parent, parent_id = granted_repository(exchange_env, make_repo)
    healthy, healthy_id = granted_repository(exchange_env, make_repo)
    refusing = [granted_repository(exchange_env, make_repo)[0] for _ in range(2)]
    nested, nested_id = granted_repository(exchange_env, make_repo)
    for repo in (parent, healthy, nested, *refusing):
        seed_commit_and_push(repo)
    refusing[0].link_add("nested", nested_id, "/")
    refusing[0].commit("Link nested")
    refusing[0].push()
    parent.link_add("vendor/healthy", healthy.get_id(), "/")
    for position, repo in enumerate(refusing):
        parent.link_add(f"vendor/refusing{position}", repo.get_id(), "/")
    parent.commit("Link three repositories")
    parent.push()
    pins = {repo.get_id(): repo.branch_info().local_latest for repo in refusing}
    scoped = provider.mint_token(
        USER,
        resources=[
            {"resource_id": f"urc-{repo_id}", "permission": GRANTED}
            for repo_id in (parent_id, healthy_id, nested_id)
        ],
    )
    branch_id = uuid.uuid4().hex

    parent.branch_create(
        "feature", id=branch_id, identity_token=scoped, access_token=scoped
    )

    assert parent.branch_info().name == "feature"
    assert healthy.branch_info("feature").id == branch_id
    assert "Restoring deleted branch 'feature'" in nested.branch_create(
        "feature", id=branch_id
    ), "the branch made below a refusing repository must have been taken back"
    main = parent.branch_info("main").id
    links = link_entries(parent)
    assert {
        link["link"]: (link["revision"], link["branch"])
        for link in links
        if link["link"] in pins
    } == {repo_id: (pin, main) for repo_id, pin in pins.items()}
