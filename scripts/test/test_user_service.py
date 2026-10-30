# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Smoke tests for a user directory advertised apart from the auth service.

`[environment.endpoint] user_url` defines where clients resolve user
IDs to display names over `lore.user.v1.UserService`, and where the
server lists, registers and unregisters repositories over
`lore.user.v1.RepositoryRegistryService`. Without it the client
speaks the `UrcAuthApi` directory RPCs to `auth_url`, the catalog
`LookupUserPermissions` and the server the rebac resource RPCs, covered
by `test_auth_online.py`.
"""

import logging
import uuid
from types import SimpleNamespace

import grpc
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
    MockAuthServer,
    MockUser,
    empty_response,
    repository_list_response,
    user_list_response,
)
from test_auth_online import (
    authz_resources,
    commit_file,
    provision_owner,
    script_repository_lifecycle,
)

logger = logging.getLogger(__name__)

pytestmark = pytest.mark.xdist_group("user_service")


CONFIG = """

# --- appended by test_user_service.py: auth service and a separate directory ---
[environment.endpoint]
auth_url = "{auth_url}"
user_url = "{user_url}"

[server.auth]
jwt_issuer = "{issuer}"
jwt_audience = ["{audience}"]

[server.auth.jwk]
endpoint = "{jwks_url}"
"""


@pytest.fixture(scope="module")
def stubs():
    """The auth service and the directory, as two independent stubs."""
    auth = MockAuthServer().start()
    directory = MockAuthServer(issuer="urc-mock-directory").start()
    try:
        yield SimpleNamespace(auth=auth, directory=directory)
    finally:
        directory.stop()
        auth.stop()


@pytest.fixture(autouse=True)
def _reset_stubs(stubs):
    stubs.auth.reset()
    stubs.directory.reset()
    # Every repository create and delete registers with the directory.
    stubs.directory.on("RepositoryRegister").respond(empty_response())
    stubs.directory.on("RepositoryUnregister").respond(empty_response())


def _launch(request, tmp_path_factory, executable, stubs, user_url):
    """A lore server authenticating against the auth stub and advertising
    `user_url` as its user directory. Yields the shape `provision_owner`
    reads: the auth stub as `mock`, plus the remote URL."""
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
        + CONFIG.format(
            auth_url=stubs.auth.auth_url,
            user_url=user_url,
            issuer=stubs.auth.issuer,
            audience=stubs.auth.audience[0],
            jwks_url=stubs.auth.jwks_url,
        )
    )
    server_proc, server_log_path, server_log_fd = launch_lore_server(
        server_root, server_env, executable
    )
    try:
        yield SimpleNamespace(
            mock=stubs.auth,
            directory=stubs.directory,
            user_url=user_url,
            remote_url=f"lore://127.0.0.1:{shared_port}/",
            server_log=server_log_path,
        )
    finally:
        _kill_server_by_pid(
            server_proc.pid, server_log_path, label="user-service server"
        )
        server_log_fd.close()


@pytest.fixture(scope="module")
def service_env(request, tmp_path_factory, lore_server_executable_path, stubs):
    """The directory reached at `127.0.0.1`, a host the tokens' `aud` names."""
    yield from _launch(
        request,
        tmp_path_factory,
        lore_server_executable_path,
        stubs,
        stubs.directory.auth_url,
    )


@pytest.fixture(scope="module")
def foreign_service_env(request, tmp_path_factory, lore_server_executable_path, stubs):
    """The same directory reached as `localhost`, which no token names."""
    yield from _launch(
        request,
        tmp_path_factory,
        lore_server_executable_path,
        stubs,
        f"http://localhost:{stubs.directory.grpc_port}",
    )


@pytest.fixture(scope="function")
def make_actor(new_lore_repo, scratch_dir):
    """`(env, label) -> actor`: an isolated token store plus a repo factory
    targeting `env`'s server. Curried per environment, so the two servers in
    this module can share `provision_owner`."""

    def _for(env):
        def _make_actor(label: str):
            auth_store = scratch_dir(f"auth-store-{label}", create=True)
            environment_vars = {"LORE_AUTH_PATH": str(auth_store)}

            def make_repo(**kwargs):
                kwargs.setdefault("remote_url", env.remote_url)
                kwargs.setdefault("create_repo", False)
                kwargs.setdefault("environment_vars", dict(environment_vars))
                return new_lore_repo(**kwargs)

            return SimpleNamespace(make_repo=make_repo, auth_store=auth_store)

        return _make_actor

    return _for


@pytest.mark.smoke
def test_names_are_asked_of_the_advertised_directory(
    service_env, make_actor, lore_library_path
):
    """Another user's name is fetched from `user_url` with `UserGet`, the
    repository id bytes as the partition."""
    owner = provision_owner(service_env, make_actor(service_env), "owner", USER1)
    directory = service_env.directory
    directory.on("UserGet", bearer=owner.authz_token).respond(user_list_response(USER2))

    result = owner.repo.auth_user_info_capi(lore_library_path, USER2.user_id)

    assert result == 0, f"resolving through the directory failed with FFI code {result}"
    [request] = directory.requests_for("UserGet")
    assert list(request["id"]) == [USER2.user_id]
    assert request["partition"].hex() == owner.resource_id.removeprefix("urc-")


@pytest.mark.smoke
def test_a_directory_outside_the_tokens_domains_is_sent_nothing(
    foreign_service_env, make_actor, lore_library_path
):
    """The exchanged token's `aud` does not cover `localhost`, so the client
    refuses to present it there, exactly as it would refuse a Lore server at
    that host."""
    env = foreign_service_env
    owner = provision_owner(env, make_actor(env), "owner", USER1)
    env.directory.on("UserGet").respond(user_list_response(USER2))

    result = owner.repo.auth_user_info_capi(lore_library_path, USER2.user_id)

    assert result != 0, "a token must not reach a host its claims do not name"
    assert env.directory.calls["UserGet"] == 0


def query_locks_by_owner(owner, name: str, supplied_token: str) -> str:
    """`lore lock query --owner <name>` with a caller-supplied access token,
    which skips the exchange. The login token rides along as the identity
    token, since a supplied credential is never looked up in the store."""
    return owner.repo.run(
        urc_args=["lock", "query", "--owner", name],
        identity_token=owner.login_token,
        access_token=supplied_token,
    )


@pytest.mark.smoke
def test_a_supplied_access_token_is_sent_to_a_directory_its_claims_name(
    service_env, make_actor
):
    """A caller-supplied `--access-token` bypasses the exchange and with it
    the exchange's recipient check, so the user service helper checks the token
    against the directory host itself. Inside the token's `aud` the token is
    sent, with no exchange made."""
    env = service_env
    owner = provision_owner(env, make_actor(env), "owner", USER1)
    supplied = env.mock.mint_token(USER1, resources=authz_resources(owner.resource_id))
    env.directory.on("UserFind", bearer=supplied).respond(user_list_response(USER2))
    exchanges_before = env.mock.calls["ExchangeUserTokenForMultiresourceToken"]

    query_locks_by_owner(owner, USER2.preferred_username, supplied)

    assert env.directory.calls["UserFind"] == 1
    assert (
        env.mock.calls["ExchangeUserTokenForMultiresourceToken"] == exchanges_before
    ), "a supplied access token is used as is, never exchanged"


@pytest.mark.smoke
def test_a_supplied_access_token_is_checked_against_the_directory_host(
    foreign_service_env, make_actor
):
    """The same supplied token against a directory advertised as `localhost`,
    a host its `aud` does not name: refused before any request. A rogue server
    cannot redirect a caller's own token to a host of its choosing by
    advertising it as the directory."""
    env = foreign_service_env
    owner = provision_owner(env, make_actor(env), "owner", USER1)
    supplied = env.mock.mint_token(USER1, resources=authz_resources(owner.resource_id))
    env.directory.on("UserFind").respond(user_list_response(USER2))

    with pytest.raises(LoreException):
        query_locks_by_owner(owner, USER2.preferred_username, supplied)

    assert env.directory.calls["UserFind"] == 0


def lock_query_by_owner_name(env, make_actor, name: str) -> str:
    """USER1 locks a file, then queries the locks owned by `name`. The lock is
    listed only when the directory resolves `name` to USER1. The raw output is
    checked rather than parsed: on macOS the in-process gRPC stub's library
    logs to the child's stderr after fork, and those lines land in the output."""
    owner = provision_owner(env, make_actor(env), "owner", USER1)
    commit_file(owner.repo, "locked.txt", "content")
    owner.repo.lock_acquire("locked.txt")
    return owner.repo.run(["lock", "query", "--owner", name])


@pytest.mark.smoke
def test_a_lock_owner_is_found_by_username(service_env, make_actor):
    service_env.directory.on("UserFind", username=USER1.preferred_username).respond(
        user_list_response(USER1)
    )

    output = lock_query_by_owner_name(service_env, make_actor, USER1.preferred_username)

    assert "locked.txt" in output


@pytest.mark.smoke
def test_a_lock_owner_is_found_by_display_name(service_env, make_actor):
    directory = service_env.directory
    directory.on("UserFind", username=USER1.display_name).respond(user_list_response())
    directory.on("UserFind", display_name=USER1.display_name).respond(
        user_list_response(USER1)
    )

    output = lock_query_by_owner_name(service_env, make_actor, USER1.display_name)

    assert "locked.txt" in output


@pytest.mark.smoke
def test_a_shared_display_name_finds_no_lock_owner(service_env, make_actor):
    """Two users share USER1's display name: the name resolves to no one,
    rather than to whichever user the directory listed first."""
    namesake = MockUser("mock-user-3", USER1.display_name, "namesake")
    directory = service_env.directory
    directory.on("UserFind", username=USER1.display_name).respond(user_list_response())
    directory.on("UserFind", display_name=USER1.display_name).respond(
        user_list_response(USER1, namesake)
    )

    output = lock_query_by_owner_name(service_env, make_actor, USER1.display_name)

    assert "locked.txt" not in output


@pytest.mark.smoke
def test_repository_list_is_the_directorys_answer(service_env, make_actor):
    """`lore repository list` on a server advertising `user_url` asks the
    directory's `RepositoryList` once and lists exactly the repositories it
    streams: a second repository the same user created but the directory
    omits is not listed, and neither stub sees `LookupUserPermissions`."""
    env = service_env
    owner = provision_owner(env, make_actor(env), "lister", USER1)

    unlisted_id = uuid.uuid4().hex
    script_repository_lifecycle(env.mock, f"urc-{unlisted_id}")
    unlisted = owner.actor.make_repo(repo_id=unlisted_id)
    unlisted.repository_create(repo_id=unlisted_id, identity=USER1.user_id)

    env.directory.on("RepositoryList", bearer=owner.login_token).respond(
        repository_list_response(owner.resource_id.removeprefix("urc-"))
    )

    listing = owner.repo.repository_list().splitlines()

    assert f"{owner.repo.name} ({owner.repo.get_id()})" in listing
    assert not any(unlisted.get_id() in line for line in listing), (
        "a repository the directory did not stream must not be listed"
    )
    assert env.directory.calls["RepositoryList"] == 1
    assert env.mock.calls["LookupUserPermissions"] == 0
    assert env.directory.calls["LookupUserPermissions"] == 0


def listing_of(owner, directory, repository_ids) -> str:
    """`lore repository list` with the directory streaming `repository_ids`."""
    directory.on("RepositoryList", bearer=owner.login_token).respond(
        repository_list_response(*repository_ids)
    )
    return owner.repo.repository_list()


@pytest.mark.smoke
def test_repository_create_and_delete_register_with_the_directory(
    service_env, make_actor
):
    """Repository create registers the repository with the directory under
    the creator's credential, and delete unregisters it; the auth service's
    rebac resource RPCs are not called. Answering `RepositoryList` with what
    was registered and not unregistered, as a directory would, lists the
    repository after create and not after delete."""
    env = service_env
    directory = env.directory
    owner = provision_owner(env, make_actor(env), "registrar", USER1)
    repository_id = owner.resource_id.removeprefix("urc-")

    [register] = directory.requests_for("RepositoryRegister")
    assert register["id"].hex() == repository_id
    assert register["name"] == owner.repo.name
    assert register["bearer"] == owner.login_token, (
        "the repository must be registered under the creating user's credential"
    )
    assert owner.repo.get_id() in listing_of(owner, directory, [repository_id])

    owner.repo.repository_delete()

    [unregister] = directory.requests_for("RepositoryUnregister")
    assert unregister["id"].hex() == repository_id
    assert unregister["bearer"] == owner.login_token
    assert owner.repo.get_id() not in listing_of(owner, directory, [])
    assert env.mock.calls["CreateResource"] == 0
    assert env.mock.calls["DeleteResource"] == 0


@pytest.mark.smoke
def test_a_refused_repository_register_leaves_no_repository(service_env, make_actor):
    """A `RepositoryRegister` the directory refuses fails the create before
    anything is stored: listed by the directory anyway, the id has no
    repository to list."""
    env = service_env
    directory = env.directory
    owner = provision_owner(env, make_actor(env), "refused", USER1)

    refused_id = uuid.uuid4().hex
    directory.on("RepositoryRegister", id=bytes.fromhex(refused_id)).deny()
    refused = owner.actor.make_repo(repo_id=refused_id)
    with pytest.raises(LoreException):
        refused.repository_create(repo_id=refused_id, identity=USER1.user_id)

    listing = listing_of(
        owner, directory, [owner.resource_id.removeprefix("urc-"), refused_id]
    )
    assert owner.repo.get_id() in listing
    assert refused_id not in listing


@pytest.mark.smoke
def test_a_refused_repository_unregister_fails_the_delete(service_env, make_actor):
    """`RepositoryUnregister` authorizes the delete, as `DeleteResource` does on
    the `auth_url` path: refused, the delete fails before anything is
    removed, and the repository is still listed."""
    env = service_env
    directory = env.directory
    owner = provision_owner(env, make_actor(env), "kept", USER1)
    repository_id = owner.resource_id.removeprefix("urc-")
    directory.on("RepositoryUnregister", id=bytes.fromhex(repository_id)).deny()

    with pytest.raises(LoreException):
        owner.repo.repository_delete()

    assert owner.repo.get_id() in listing_of(owner, directory, [repository_id])


@pytest.mark.smoke
def test_a_repository_registered_under_another_name_fails_the_create(
    service_env, make_actor
):
    """A repeated `RepositoryRegister` under the same name answers `OK`, so
    `ALREADY_EXISTS` means the directory has the repository registered under
    another name: the create fails and stores no repository under this one."""
    env = service_env
    directory = env.directory
    owner = provision_owner(env, make_actor(env), "conflict", USER1)

    conflicting_id = uuid.uuid4().hex
    directory.on("RepositoryRegister", id=bytes.fromhex(conflicting_id)).deny(
        grpc.StatusCode.ALREADY_EXISTS, "registered under another name"
    )
    conflicting = owner.actor.make_repo(repo_id=conflicting_id)
    with pytest.raises(LoreException):
        conflicting.repository_create(repo_id=conflicting_id, identity=USER1.user_id)

    assert conflicting_id not in listing_of(owner, directory, [conflicting_id])
