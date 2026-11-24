#!/usr/bin/python3
# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import base64
import hashlib
import hmac
import json
import logging
import os
import uuid
from pathlib import Path
from time import sleep, time

import grpc
import pytest
from grpc_probe import call, repository_metadata
from lore_server import (
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)
from protobuf_wire import (
    encode_bytes_field,
    field_message,
    field_string,
    parse_fields,
)

logger = logging.getLogger(__name__)

REPOSITORY_GET = "/lore.repository.v1.RepositoryService/RepositoryGet"
LOCK_QUERY = "/urc.lock.LockService/Query"
LOCK_LOCK = "/urc.lock.LockService/Lock"
LOCK_STATUS = "/urc.lock.LockService/Status"
LOCK_UNLOCK = "/urc.lock.LockService/Unlock"


def allocate_server_ports() -> dict[str, int]:
    """Ports for one server. QUIC and gRPC share a number, one being UDP and
    the other TCP."""
    shared_port = allocate_free_port()
    return {
        "quic": shared_port,
        "grpc": shared_port,
        "http": allocate_free_port(),
        "internal": allocate_free_port(),
    }


def delegation_target_config(request, tmp_path_factory):
    """Config for a delegation target. Its internal gRPC server is enabled
    without mTLS so the delegating server can reach it over plain HTTP/2."""
    server_root, server_env = generate_server_config(
        request, tmp_path_factory, allocate_server_ports()
    )
    server_env["LORE__SERVER__GRPC_INTERNAL__ENABLED"] = "true"
    server_env["LORE__SERVER__GRPC_INTERNAL__VERIFY_CLIENT_CERTS"] = "false"
    return server_root, server_env


def delegation_source_config(request, tmp_path_factory, target_config, enabled_rpcs):
    """Config for a delegation source, forwarding `enabled_rpcs` to the target's
    internal gRPC port. No certs are needed because the target's internal
    listener runs without TLS."""
    server_root, server_env = generate_server_config(
        request, tmp_path_factory, allocate_server_ports()
    )

    _, target_env = target_config
    target_internal_port = target_env["LORE__SERVER__GRPC_INTERNAL__PORT"]
    server_hostname = request.config.getoption("--lore-server-hostname")

    with open(
        os.path.join(server_root, "lore-server", "config", "local.toml"),
        "a",
        encoding="utf-8",
    ) as f:
        f.write("[server.grpc_public_services.forwarded_requests.client]\n")
        f.write(f'url = "http://{server_hostname}:{target_internal_port}"\n')
        f.write("[server.grpc_public_services.forwarded_requests.enabled_rpcs]\n")
        f.writelines(f"{rpc} = true\n" for rpc in enabled_rpcs)

    return server_root, server_env


def grpc_target(request, server_config) -> str:
    """The `host:port` of a server's public gRPC endpoint."""
    server_hostname = request.config.getoption("--lore-server-hostname")
    _, server_env = server_config
    return f"{server_hostname}:{server_env['LORE__SERVER__GRPC__PORT']}"


def remote_url(request, server_config) -> str:
    """The `lore://` URL a client uses to reach a server."""
    return f"lore://{grpc_target(request, server_config)}"


def repository_get_by_name_request(name: str) -> bytes:
    """A `RepositoryGetRequest` selecting a repository by name.

    `name` is field 2 of the `query` oneof. The call carries no repository
    metadata: RepositoryGet names its subject in the request body, so the
    handler never reads a repository id from metadata."""
    return encode_bytes_field(2, name.encode("utf-8"))


def repository_name_in_response(body: bytes) -> str:
    """The `name` of the `Repository` a `RepositoryGetResponse` carries.

    `repository` is field 1 of the response and `name` is field 2 of
    `lore.model.v1.Repository`."""
    repository = field_message(parse_fields(body), 1)
    return field_string(repository, 2) if repository else ""


def locked_resources_in_response(body: bytes) -> list[bytes]:
    """The encoded `Resource` of each `Lock` in a `QueryResponse`,
    `LockResponse` or `StatusResponse`, all of which carry their locks as
    repeated field 1."""
    resources = []
    for lock in parse_fields(body).get(1, []):
        resources.extend(parse_fields(lock).get(1, []))
    return resources


def resources_request(resources: list[bytes]) -> bytes:
    """A `LockRequest`, `StatusRequest` or `UnlockRequest`, all of which carry
    their resources as repeated field 1."""
    return b"".join(encode_bytes_field(1, resource) for resource in resources)


def unsigned_identity_metadata(
    repo_id: str, identity: str
) -> tuple[tuple[str, bytes], ...]:
    """Repository metadata plus a bearer token naming `identity`, built as the
    client builds one for a server with `trust_unsigned_identity`: HS256 under
    an empty key, with the client's unsigned-identity issuer and claims."""

    def part(raw: bytes) -> str:
        return base64.urlsafe_b64encode(raw).decode().rstrip("=")

    def json_part(obj: dict) -> str:
        return part(json.dumps(obj, separators=(",", ":")).encode())

    issuer = "lore-unsigned-identity"
    now = int(time())
    claims = {
        "iss": issuer,
        "aud": [issuer],
        "sub": identity,
        "name": identity,
        "preferred_username": identity,
        "iat": now,
        "exp": now + 3600,
    }
    signing_input = f"{json_part({'alg': 'HS256', 'typ': 'JWT'})}.{json_part(claims)}"
    signature = hmac.new(b"", signing_input.encode(), hashlib.sha256).digest()
    token = f"{signing_input}.{part(signature)}"
    return (*repository_metadata(repo_id), ("authorization", f"Bearer {token}"))


def server_locks(request, server_config, repo_id: str) -> list[bytes]:
    """The encoded `Resource` of every lock a server's own store holds for the
    repository, read from a server that forwards no lock RPC."""
    code, body, details = call(
        grpc_target(request, server_config),
        LOCK_QUERY,
        b"",
        metadata=repository_metadata(repo_id),
    )
    assert code == grpc.StatusCode.OK, (
        f"Lock Query should succeed, got {code} '{details}'"
    )
    return locked_resources_in_response(body)


def log_contains(log_path: Path, expected: str, attempts: int = 30) -> bool:
    """Whether `expected` reaches `log_path`, polling because the line is
    written after the response the test already observed."""
    for _ in range(attempts):
        if log_path.exists() and expected in log_path.read_text(
            encoding="utf-8", errors="ignore"
        ):
            return True
        sleep(1)
    return False


@pytest.mark.smoke
@pytest.mark.xdist_group("forwarded_requests")
class TestForwardedBranch:
    """
    Smoke tests for the forwarded-request delegation path.

    Two independent Lore servers are started, each with their own mutable store,
    so branch state is fully isolated between them. Server 2 is configured with
    [server.grpc_public_services.forwarded_requests] pointing at Server 1's
    internal gRPC port and revision_branch_create = true. When a client calls
    e.g. BranchCreate on Server 2, Server 2 forwards the request to Server 1 instead
    of executing it locally.

    Because the mutable stores are separate, the store Server 2 writes to is
    determined entirely by which server actually executes the RPC. Checking
    which store ends up with the branch is therefore a reliable, side-effect-
    visible proof that delegation occurred — it cannot be explained by Server 2
    executing the request itself and returning a success response.
    """

    @pytest.fixture(scope="class")
    def server_1_config(self, request, tmp_path_factory):
        """
        Config for Server 1: the delegation *target*. Its internal gRPC server
        is enabled without mTLS so Server 2 can reach it over plain HTTP/2.
        """
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
        # Enable the internal gRPC server without mTLS so Server 2 can reach it
        server_env["LORE__SERVER__GRPC_INTERNAL__ENABLED"] = "true"
        server_env["LORE__SERVER__GRPC_INTERNAL__VERIFY_CLIENT_CERTS"] = "false"
        return server_root, server_env

    @pytest.fixture(scope="class")
    def server_1(self, server_1_config, lore_server_executable_path):
        """Launches Server 1 and tears it down after the class finishes."""
        server_root, server_env = server_1_config
        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc, log_path, log_fd
        _kill_server_by_pid(
            server_proc.pid, log_path, label="forwarded requests server 1"
        )
        log_fd.close()

    @pytest.fixture(scope="class")
    def server_2_config(self, request, tmp_path_factory, server_1_config):
        """
        Config for Server 2: the delegation *source*. Its local.toml is extended
        with the forwarded_requests block that tells it to forward operations
        to Server 1's internal gRPC port. No certs are needed because Server 1's
        internal listener runs without TLS in this test.
        """
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

        _, server_1_env = server_1_config
        server_1_internal_port = server_1_env["LORE__SERVER__GRPC_INTERNAL__PORT"]
        server_hostname = request.config.getoption("--lore-server-hostname")

        # Point Server 2's forwarded_requests client at Server 1's internal gRPC port
        # and enable the branch_create delegation flag
        with open(
            os.path.join(server_root, "lore-server", "config", "local.toml"),
            "a",
            encoding="utf-8",
        ) as f:
            f.write("[server.grpc_public_services.forwarded_requests.client]\n")
            f.write(f'url = "http://{server_hostname}:{server_1_internal_port}"\n')
            f.write("[server.grpc_public_services.forwarded_requests.enabled_rpcs]\n")
            f.write("revision_branch_create = true\n")
            f.write("revision_branch_list = true\n")

        return server_root, server_env

    @pytest.fixture(scope="class")
    def server_2(self, server_2_config, server_1, lore_server_executable_path):
        """
        Launches Server 2 and tears it down after the class finishes.
        Depends on server_1 so that Server 1's internal gRPC port is ready
        before Server 2 starts and attempts its first outbound connection.
        """
        server_root, server_env = server_2_config
        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc, log_path, log_fd
        _kill_server_by_pid(
            server_proc.pid, log_path, label="forwarded requests server 2"
        )
        log_fd.close()

    @pytest.fixture()
    def repos(
        self,
        request,
        server_1_config,
        server_2_config,
        server_1,
        server_2,
        new_lore_repo,
    ):
        """
        Create two lore clients pointing at different servers but sharing the
        same repository ID so that branch state can be compared between them.
        """
        server_hostname = request.config.getoption("--lore-server-hostname")
        _, server_1_env = server_1_config
        _, server_2_env = server_2_config

        common_repo_id = uuid.uuid4().hex
        common_repo_name = f"repo-{common_repo_id}"

        remote_url_server_1 = (
            f"lore://{server_hostname}:{server_1_env['LORE__SERVER__GRPC__PORT']}"
        )
        remote_url_server_2 = (
            f"lore://{server_hostname}:{server_2_env['LORE__SERVER__GRPC__PORT']}"
        )

        server_1_repo = new_lore_repo(
            remote_url=remote_url_server_1,
            remote_path=f"{remote_url_server_1}/{common_repo_name}",
            repo_id=common_repo_id,
        )
        server_2_repo = new_lore_repo(
            remote_url=remote_url_server_2,
            remote_path=f"{remote_url_server_2}/{common_repo_name}",
            repo_id=common_repo_id,
        )

        # branch_create requires at least one pushed revision on each server.
        # Push an initial commit to Server 1 first so it has revision data,
        # then push to Server 2 so the lore client on Server 2 has a revision
        # context from which to construct the BranchCreateRequest.
        server_1_repo.write_commit_push(None, {"init.txt": "initial commit"})
        server_2_repo.write_commit_push(None, {"init.txt": "initial commit"})

        return server_1_repo, server_2_repo

    @pytest.mark.smoke
    def test_branch_create_delegates_write_to_server_1(self, repos):
        """Verify delegation by checking which store holds the branch after the call."""
        server_1_repo, server_2_repo = repos
        branch_name = f"feature-{uuid.uuid4().hex[:8]}"

        # Confirm the branch does not yet exist on either server
        assert not server_1_repo.branch_list(remote=True).has_remote_branch(
            branch_name
        ), f"Branch '{branch_name}' should not exist on server 1 before creation"
        assert not server_2_repo.branch_list(remote=True).has_remote_branch(
            branch_name
        ), f"Branch '{branch_name}' should not exist on server 2 before creation"

        # branch_create is local-only; the BranchCreate RPC is only sent to
        # the server when the branch is explicitly pushed. branch_push triggers
        # that RPC on Server 2, which delegates it to Server 1.
        logger.info(
            "Creating branch '%s' via server 2 (delegates to server 1)", branch_name
        )
        server_2_repo.branch_create(branch_name)
        server_2_repo.branch_push(branch_name)

        # The write went to Server 1's mutable store — branch exists there
        assert server_1_repo.branch_list(remote=True).has_remote_branch(branch_name), (
            f"Branch '{branch_name}' should exist on server 1 after delegated create"
        )

    @pytest.mark.smoke
    def test_branch_list_delegates_read_to_server_1(self, repos):
        """Verify delegation by listing via Server 2 and checking the result comes from Server 1."""
        server_1_repo, server_2_repo = repos
        branch_names = [
            f"feature-{uuid.uuid4().hex[:8]}",
            f"feature-{uuid.uuid4().hex[:8]}",
        ]

        # Push both branches directly to Server 1 — Server 2 never receives
        # BranchCreate RPCs for them, so they are absent from Server 2's store.
        for branch_name in branch_names:
            logger.info("Pushing branch '%s' directly to server 1", branch_name)
            server_1_repo.branch_create(branch_name)
            server_1_repo.branch_push(branch_name)

        # Server 1's store has all branches
        for branch_name in branch_names:
            assert server_1_repo.branch_list(remote=True).has_remote_branch(
                branch_name
            ), f"Branch '{branch_name}' should exist on server 1 after direct push"

        # Listing via Server 2 delegates to Server 1 — all branches appear even
        # though none were written to Server 2's mutable store.
        for branch_name in branch_names:
            assert server_2_repo.branch_list(remote=True).has_remote_branch(
                branch_name
            ), (
                f"Branch '{branch_name}' should appear in server 2's delegated list "
                "(BranchList was forwarded to server 1)"
            )


@pytest.mark.smoke
@pytest.mark.xdist_group("forwarded_requests")
class TestForwardedRepositoryCreate:
    """
    Smoke tests for the forwarded-request delegation path for RepositoryCreate.

    Two independent Lore servers are started, each with their own mutable store.
    Server 2 is configured with repository_create = true, pointing at
    Server 1's internal gRPC port. When a client calls RepositoryCreate on Server 2,
    Server 2 forwards the request to Server 1 instead of executing it locally.

    The proof of delegation is the state of the two mutable stores after the call.
    Because each server owns a completely separate store, a repository can only appear
    in a store if that server executed the write itself:

      - Server 1's store contains the repository  → Server 1 ran the write
      - Server 2's store does not                 → Server 2 did not run it locally

    A successful response from Server 2 alone would not distinguish delegation from
    local execution; the repository-list assertions are what makes this meaningful.
    """

    @pytest.fixture(scope="class")
    def server_1_config(self, request, tmp_path_factory):
        """
        Config for Server 1: the delegation *target*. Its internal gRPC server
        is enabled without mTLS so Server 2 can reach it over plain HTTP/2.
        """
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
        return server_root, server_env

    @pytest.fixture(scope="class")
    def server_1(self, server_1_config, lore_server_executable_path):
        """Launches Server 1 and tears it down after the class finishes."""
        server_root, server_env = server_1_config
        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc, log_path, log_fd
        _kill_server_by_pid(
            server_proc.pid, log_path, label="forwarded repository server 1"
        )
        log_fd.close()

    @pytest.fixture(scope="class")
    def server_2_config(self, request, tmp_path_factory, server_1_config):
        """
        Config for Server 2: the delegation *source*. Its local.toml is extended
        with the forwarded_requests block that tells it to forward RepositoryCreate
        to Server 1's internal gRPC port.
        """
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

        _, server_1_env = server_1_config
        server_1_internal_port = server_1_env["LORE__SERVER__GRPC_INTERNAL__PORT"]
        server_hostname = request.config.getoption("--lore-server-hostname")

        with open(
            os.path.join(server_root, "lore-server", "config", "local.toml"),
            "a",
            encoding="utf-8",
        ) as f:
            f.write("[server.grpc_public_services.forwarded_requests.client]\n")
            f.write(f'url = "http://{server_hostname}:{server_1_internal_port}"\n')
            f.write("[server.grpc_public_services.forwarded_requests.enabled_rpcs]\n")
            f.write("repository_create = true\n")

        return server_root, server_env

    @pytest.fixture(scope="class")
    def server_2(self, server_2_config, server_1, lore_server_executable_path):
        """
        Launches Server 2 and tears it down after the class finishes.
        Depends on server_1 so that Server 1's internal gRPC port is ready
        before Server 2 starts and attempts its first outbound connection.
        """
        server_root, server_env = server_2_config
        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc, log_path, log_fd
        _kill_server_by_pid(
            server_proc.pid, log_path, label="forwarded repository server 2"
        )
        log_fd.close()

    @pytest.mark.smoke
    def test_repository_create_delegates_write_to_server_1(
        self,
        request,
        server_1_config,
        server_2_config,
        server_1,
        server_2,
        new_lore_repo,
    ):
        """Verify delegation by checking which store holds the repository after the call."""
        server_hostname = request.config.getoption("--lore-server-hostname")
        _, server_1_env = server_1_config
        _, server_2_env = server_2_config

        remote_url_server_1 = (
            f"lore://{server_hostname}:{server_1_env['LORE__SERVER__GRPC__PORT']}"
        )
        remote_url_server_2 = (
            f"lore://{server_hostname}:{server_2_env['LORE__SERVER__GRPC__PORT']}"
        )

        repo_name = f"delegated-repo-{uuid.uuid4().hex[:8]}"
        repo_id = uuid.uuid4().hex

        logger.info(
            "Creating repository '%s' via server 2 (delegates to server 1)", repo_name
        )
        repo_via_server_2 = new_lore_repo(
            remote_url=remote_url_server_2,
            remote_path=f"{remote_url_server_2}/{repo_name}",
            repo_id=repo_id,
        )

        # Create a client pointing at Server 1 (no repo create) for querying its store
        server_1_client = new_lore_repo(
            remote_url=remote_url_server_1,
            create_repo=False,
        )

        # The write went to Server 1's mutable store — repository exists there
        server_1_list = server_1_client.repository_list()
        assert repo_name in server_1_list, (
            f"Repository '{repo_name}' should exist in Server 1's store after delegated create"
        )

        # Server 2's mutable store was never written to — repository absent there
        server_2_list = repo_via_server_2.repository_list()
        assert repo_name not in server_2_list, (
            f"Repository '{repo_name}' should not exist in Server 2's store "
            "(request was delegated, not written locally)"
        )


@pytest.mark.smoke
@pytest.mark.xdist_group("forwarded_requests")
class TestForwardedRepositoryGet:
    """
    Smoke tests for the forwarded-request delegation path for RepositoryGet.

    Server 2 forwards RepositoryGet and nothing else, so the two repository
    read RPCs disagree about what exists: RepositoryList is answered from
    Server 2's own store while RepositoryGet is answered from Server 1's. A
    repository created only on Server 1 is therefore missing from Server 2's
    list and still resolvable through Server 2's get, which Server 2 could not
    do by executing the read locally.

    The second test pins the opposite direction. An error the peer itself
    produced must reach the client as the peer wrote it, rather than being
    reported as a failure of the forwarding call — the two are distinguished by
    whether the status arrived from the peer or was synthesized locally, so
    over-reporting one as the other is the risk this covers.
    """

    @pytest.fixture(scope="class")
    def server_1_config(self, request, tmp_path_factory):
        return delegation_target_config(request, tmp_path_factory)

    @pytest.fixture(scope="class")
    def server_1(self, server_1_config, lore_server_executable_path):
        server_root, server_env = server_1_config
        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc, log_path, log_fd
        _kill_server_by_pid(
            server_proc.pid, log_path, label="forwarded repository get server 1"
        )
        log_fd.close()

    @pytest.fixture(scope="class")
    def server_2_config(self, request, tmp_path_factory, server_1_config):
        return delegation_source_config(
            request, tmp_path_factory, server_1_config, ["repository_get"]
        )

    @pytest.fixture(scope="class")
    def server_2(self, server_2_config, server_1, lore_server_executable_path):
        """
        Depends on server_1 so the target's internal gRPC port is ready before
        Server 2 starts. Server 2 connects to its peer while starting up and
        fails to start if that connection is refused.
        """
        server_root, server_env = server_2_config
        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc, log_path, log_fd
        _kill_server_by_pid(
            server_proc.pid, log_path, label="forwarded repository get server 2"
        )
        log_fd.close()

    @pytest.fixture()
    def repository_on_server_1(
        self, request, server_1_config, server_1, server_2, new_lore_repo
    ):
        """A repository written only to Server 1's store. Returns its name."""
        server_1_url = remote_url(request, server_1_config)
        repo_name = f"delegated-get-{uuid.uuid4().hex[:8]}"
        new_lore_repo(
            remote_url=server_1_url,
            remote_path=f"{server_1_url}/{repo_name}",
            repo_id=uuid.uuid4().hex,
        )
        return repo_name

    @pytest.mark.smoke
    def test_repository_get_delegates_read_to_server_1(
        self, request, server_1_config, server_2_config, repository_on_server_1
    ):
        """Verify delegation by resolving a repository Server 2's own store never held."""
        repo_name = repository_on_server_1
        server_2_target = grpc_target(request, server_2_config)

        code, body, details = call(
            server_2_target, REPOSITORY_GET, repository_get_by_name_request(repo_name)
        )
        assert code == grpc.StatusCode.OK, (
            f"RepositoryGet for '{repo_name}' via server 2 should succeed, got "
            f"{code} '{details}'"
        )
        assert repository_name_in_response(body) == repo_name, (
            f"The delegated response should carry repository '{repo_name}'"
        )

    @pytest.mark.smoke
    def test_repository_list_on_server_2_does_not_see_it(
        self, request, server_2_config, repository_on_server_1, new_lore_repo
    ):
        """RepositoryList is not forwarded, so Server 2's own store answers it."""
        repo_name = repository_on_server_1
        server_2_client = new_lore_repo(
            remote_url=remote_url(request, server_2_config),
            create_repo=False,
        )

        assert repo_name not in server_2_client.repository_list(), (
            f"Repository '{repo_name}' should be absent from server 2's store — "
            "only the forwarded RepositoryGet can resolve it"
        )

    @pytest.mark.smoke
    def test_peer_error_reaches_the_client_as_the_peer_wrote_it(
        self, request, server_1, server_2, server_2_config
    ):
        """An unknown repository is the peer's NOT_FOUND, not a forwarding failure."""
        unknown_name = f"absent-repo-{uuid.uuid4().hex[:8]}"
        server_2_target = grpc_target(request, server_2_config)

        code, _body, details = call(
            server_2_target,
            REPOSITORY_GET,
            repository_get_by_name_request(unknown_name),
        )
        assert code == grpc.StatusCode.NOT_FOUND, (
            f"An unknown repository should be NOT_FOUND, got {code} '{details}'"
        )
        assert unknown_name in details, (
            "The peer's own message names the repository it could not find; "
            f"got '{details}'"
        )


@pytest.mark.smoke
@pytest.mark.xdist_group("forwarded_requests")
class TestForwardedLocks:
    """
    Smoke test for the forwarded-request delegation path for the lock service's
    Query, Lock, Status and Unlock.

    Each server keeps its own local lock store. Locks are taken through
    Server 1, for a repository Server 2's stores never held, so a lock Server 2
    reports, takes or releases can only have come from Server 1's store by
    forwarding. Server 1 forwards nothing, so querying it directly shows what
    its store holds.
    """

    @pytest.fixture(scope="class")
    def server_1_config(self, request, tmp_path_factory):
        return delegation_target_config(request, tmp_path_factory)

    @pytest.fixture(scope="class")
    def server_1(self, server_1_config, lore_server_executable_path):
        server_root, server_env = server_1_config
        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc, log_path, log_fd
        _kill_server_by_pid(server_proc.pid, log_path, label="forwarded locks server 1")
        log_fd.close()

    @pytest.fixture(scope="class")
    def server_2_config(self, request, tmp_path_factory, server_1_config):
        return delegation_source_config(
            request,
            tmp_path_factory,
            server_1_config,
            ["lock_query", "lock_lock", "lock_status", "lock_unlock"],
        )

    @pytest.fixture(scope="class")
    def server_2(self, server_2_config, server_1, lore_server_executable_path):
        """
        Depends on server_1 so the target's internal gRPC port is ready before
        Server 2 starts. Server 2 connects to its peer while starting up and
        fails to start if that connection is refused.
        """
        server_root, server_env = server_2_config
        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc, log_path, log_fd
        _kill_server_by_pid(server_proc.pid, log_path, label="forwarded locks server 2")
        log_fd.close()

    @pytest.fixture()
    def locked_on_server_1(
        self, request, server_1_config, server_1, server_2, new_lore_repo
    ):
        """A lock on `locked.txt` taken through Server 1 only. Returns the
        repository id and the lock's encoded `Resource`, as Server 1's store
        holds it."""
        server_1_url = remote_url(request, server_1_config)
        repo_id = uuid.uuid4().hex
        repo = new_lore_repo(
            remote_url=server_1_url,
            remote_path=f"{server_1_url}/delegated-lock-{repo_id[:8]}",
            repo_id=repo_id,
        )
        repo.write_commit_push(None, {"locked.txt": "locked content"})
        repo.lock_acquire("locked.txt")

        resources = server_locks(request, server_1_config, repo_id)
        assert len(resources) == 1, f"Server 1 should hold one lock, got {resources}"
        return repo_id, resources[0]

    @pytest.mark.smoke
    def test_lock_query_delegates_read_to_server_1(
        self, request, server_2_config, locked_on_server_1
    ):
        """Verify delegation by finding a lock only Server 1's store holds."""
        repo_id, resource = locked_on_server_1

        # An empty QueryRequest asks for every lock in the repository the
        # metadata names.
        code, body, details = call(
            grpc_target(request, server_2_config),
            LOCK_QUERY,
            b"",
            metadata=repository_metadata(repo_id),
        )
        assert code == grpc.StatusCode.OK, (
            f"Lock Query via server 2 should succeed, got {code} '{details}'"
        )
        assert locked_resources_in_response(body) == [resource], (
            "The delegated response should carry the lock acquired on server 1"
        )

    @pytest.mark.smoke
    def test_lock_status_delegates_read_to_server_1(
        self, request, server_2_config, locked_on_server_1
    ):
        """Verify delegation by reporting a lock only Server 1's store holds."""
        repo_id, resource = locked_on_server_1

        code, body, details = call(
            grpc_target(request, server_2_config),
            LOCK_STATUS,
            resources_request([resource]),
            metadata=repository_metadata(repo_id),
        )
        assert code == grpc.StatusCode.OK, (
            f"Lock Status via server 2 should succeed, got {code} '{details}'"
        )
        assert locked_resources_in_response(body) == [resource], (
            "The delegated status should report the lock acquired on server 1"
        )

    @pytest.mark.smoke
    def test_lock_unlock_delegates_write_to_server_1(
        self, request, server_1_config, server_2_config, locked_on_server_1
    ):
        """Verify delegation by releasing, through Server 2, a lock Server 1's
        store holds."""
        repo_id, resource = locked_on_server_1

        code, body, details = call(
            grpc_target(request, server_2_config),
            LOCK_UNLOCK,
            resources_request([resource]),
            metadata=repository_metadata(repo_id),
        )
        assert code == grpc.StatusCode.OK, (
            f"Unlock via server 2 should succeed, got {code} '{details}'"
        )
        # `resources` is repeated field 1 of UnlockResponse.
        assert parse_fields(body).get(1, []) == [resource], (
            "The delegated response should name the released resource"
        )
        assert server_locks(request, server_1_config, repo_id) == [], (
            "The lock should be gone from server 1's store"
        )

    @pytest.mark.smoke
    def test_lock_lock_delegates_write_to_server_1(
        self, request, server_1_config, server_2_config, locked_on_server_1
    ):
        """Verify delegation by taking a lock through Server 2 that lands in
        Server 1's store."""
        repo_id, resource = locked_on_server_1
        server_1_target = grpc_target(request, server_1_config)
        metadata = repository_metadata(repo_id)

        # Released on server 1 directly, so the lock below is the only one.
        code, _body, details = call(
            server_1_target, LOCK_UNLOCK, resources_request([resource]), metadata
        )
        assert code == grpc.StatusCode.OK, (
            f"Unlock on server 1 should succeed, got {code} '{details}'"
        )
        assert server_locks(request, server_1_config, repo_id) == [], (
            "The lock should be released from server 1's store before relocking"
        )

        code, body, details = call(
            grpc_target(request, server_2_config),
            LOCK_LOCK,
            resources_request([resource]),
            metadata=metadata,
        )
        assert code == grpc.StatusCode.OK, (
            f"Lock via server 2 should succeed, got {code} '{details}'"
        )
        assert locked_resources_in_response(body) == [resource], (
            "The delegated response should carry the lock taken"
        )
        assert server_locks(request, server_1_config, repo_id) == [resource], (
            "The lock should be in server 1's store"
        )


@pytest.mark.smoke
@pytest.mark.xdist_group("forwarded_requests")
class TestForwardedUnlockChecksOwnership:
    """
    A forwarded Unlock from a caller who is not elevated releases only the
    caller's own locks.

    Both servers trust unsigned identities, so each caller is named by the
    token it sends and none holds `owner` or `admin`. The lock is taken through
    Server 1 as one user, and released through Server 2, which forwards
    Unlock, so only Server 1 can check who holds it.
    """

    @pytest.fixture(scope="class")
    def server_1_config(self, request, tmp_path_factory):
        server_root, server_env = delegation_target_config(request, tmp_path_factory)
        server_env["LORE__SERVER__TRUST_UNSIGNED_IDENTITY"] = "true"
        return server_root, server_env

    @pytest.fixture(scope="class")
    def server_1(self, server_1_config, lore_server_executable_path):
        server_root, server_env = server_1_config
        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc, log_path, log_fd
        _kill_server_by_pid(
            server_proc.pid, log_path, label="forwarded unlock ownership server 1"
        )
        log_fd.close()

    @pytest.fixture(scope="class")
    def server_2_config(self, request, tmp_path_factory, server_1_config):
        server_root, server_env = delegation_source_config(
            request, tmp_path_factory, server_1_config, ["lock_query", "lock_unlock"]
        )
        server_env["LORE__SERVER__TRUST_UNSIGNED_IDENTITY"] = "true"
        return server_root, server_env

    @pytest.fixture(scope="class")
    def server_2(self, server_2_config, server_1, lore_server_executable_path):
        """
        Depends on server_1 so the target's internal gRPC port is ready before
        Server 2 starts. Server 2 connects to its peer while starting up and
        fails to start if that connection is refused.
        """
        server_root, server_env = server_2_config
        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc, log_path, log_fd
        _kill_server_by_pid(
            server_proc.pid, log_path, label="forwarded unlock ownership server 2"
        )
        log_fd.close()

    @pytest.mark.smoke
    def test_forwarded_unlock_releases_only_the_callers_own_lock(
        self,
        request,
        server_1_config,
        server_2_config,
        server_1,
        server_2,
        new_lore_repo,
    ):
        server_1_url = remote_url(request, server_1_config)
        repo_id = uuid.uuid4().hex
        repo = new_lore_repo(
            remote_url=server_1_url,
            remote_path=f"{server_1_url}/delegated-ownership-{repo_id[:8]}",
            repo_id=repo_id,
        )
        repo.write_commit_push(None, {"locked.txt": "locked content"})
        repo.lock_acquire("locked.txt", identity="alice")

        resources = server_locks(request, server_1_config, repo_id)
        assert len(resources) == 1, f"Server 1 should hold one lock, got {resources}"
        unlock_request = resources_request(resources)
        server_2_target = grpc_target(request, server_2_config)

        code, _body, details = call(
            server_2_target,
            LOCK_UNLOCK,
            unlock_request,
            metadata=unsigned_identity_metadata(repo_id, "bob"),
        )
        assert code == grpc.StatusCode.FAILED_PRECONDITION, (
            "Bob's unlock of Alice's lock should be refused as not owned, got "
            f"{code} '{details}'"
        )
        assert repo.lock_status("locked.txt", identity="bob")[0].owner == "alice", (
            "Alice's lock should still be held on server 1"
        )

        code, body, details = call(
            server_2_target,
            LOCK_UNLOCK,
            unlock_request,
            metadata=unsigned_identity_metadata(repo_id, "alice"),
        )
        assert code == grpc.StatusCode.OK, (
            f"Alice's unlock of her own lock should succeed, got {code} '{details}'"
        )
        # `resources` is repeated field 1 of UnlockResponse.
        assert parse_fields(body).get(1, []) == resources, (
            "The delegated response should name the released resource"
        )
        assert server_locks(request, server_1_config, repo_id) == [], (
            "Alice's lock should be gone from server 1's store"
        )


@pytest.mark.smoke
@pytest.mark.xdist_group("forwarded_requests")
class TestForwardedRequestUnreachablePeer:
    """
    Smoke test for what a client sees when the server it called cannot reach the
    server that server forwards to.

    The peer is killed part-way through the test rather than never started: a
    delegating server connects to its peer while starting up and refuses to
    start if that connection is refused, so a peer that was never there cannot
    be configured in the first place.

    This class owns its own pair of servers because the test kills one of them.
    The other classes in this file keep their pair for the whole class and would
    find the target gone underneath them.
    """

    @pytest.fixture(scope="class")
    def server_1_config(self, request, tmp_path_factory):
        return delegation_target_config(request, tmp_path_factory)

    @pytest.fixture(scope="class")
    def server_1(self, server_1_config, lore_server_executable_path):
        server_root, server_env = server_1_config
        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc, log_path, log_fd
        # Safe to call for a process the test already killed.
        _kill_server_by_pid(
            server_proc.pid, log_path, label="unreachable peer server 1"
        )
        log_fd.close()

    @pytest.fixture(scope="class")
    def server_2_config(self, request, tmp_path_factory, server_1_config):
        return delegation_source_config(
            request, tmp_path_factory, server_1_config, ["repository_get"]
        )

    @pytest.fixture(scope="class")
    def server_2(self, server_2_config, server_1, lore_server_executable_path):
        server_root, server_env = server_2_config
        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        yield server_proc, log_path, log_fd
        _kill_server_by_pid(
            server_proc.pid, log_path, label="unreachable peer server 2"
        )
        log_fd.close()

    @pytest.mark.smoke
    def test_unreachable_peer_is_reported_as_the_origin_own_error(
        self,
        request,
        server_1_config,
        server_2_config,
        server_1,
        server_2,
        new_lore_repo,
    ):
        """The client gets the origin's INTERNAL, and the origin logs the cause."""
        server_1_proc, _server_1_log, _server_1_fd = server_1
        _server_2_proc, server_2_log, _server_2_fd = server_2
        server_2_target = grpc_target(request, server_2_config)

        server_1_url = remote_url(request, server_1_config)
        repo_name = f"unreachable-peer-{uuid.uuid4().hex[:8]}"
        new_lore_repo(
            remote_url=server_1_url,
            remote_path=f"{server_1_url}/{repo_name}",
            repo_id=uuid.uuid4().hex,
        )

        # Forwarding answers while the peer is up, so a failure after the kill
        # is the peer being gone rather than a misconfigured delegation.
        code, _body, details = call(
            server_2_target, REPOSITORY_GET, repository_get_by_name_request(repo_name)
        )
        assert code == grpc.StatusCode.OK, (
            f"Delegation should work before the peer is killed, got {code} '{details}'"
        )

        logger.info("Killing server 1 to make server 2's forwarding peer unreachable")
        _kill_server_by_pid(server_1_proc.pid, label="unreachable peer server 1")

        code, _body, details = call(
            server_2_target, REPOSITORY_GET, repository_get_by_name_request(repo_name)
        )
        assert code == grpc.StatusCode.INTERNAL, (
            "A peer that cannot be reached is the origin's own failure, not an "
            f"answer from the peer; got {code} '{details}'"
        )
        assert "Error making forwarded request" in details, (
            f"The client should get the origin's own message, got '{details}'"
        )

        assert log_contains(server_2_log, "forwarded request did not reach the peer"), (
            "Server 2 should record why the forwarded request failed; a status "
            "the client can act on is not enough to diagnose a peer outage"
        )
