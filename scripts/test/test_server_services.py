#!/usr/bin/python3
# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Test the per-service `enabled` flags under `server.grpc_public_services`.

`TestThinClientServices` probes the registered routes. One block gates a whole
proto family, so disabling `storage_service` removes both its legacy and v1
services. `TestThinClientServesData` pushes data through the full server and
reads it through the thin server.
"""

import time
import uuid

import grpc
import pytest
from grpc_probe import (
    CONTENT_DIFF,
    REVISION_DIFF,
    REVISION_INFO,
    REVISION_TREE,
    STORAGE_QUERY,
    is_served,
    method_details,
    method_status,
    repository_metadata,
    response_carries_a_revision,
    revision_diff,
    revision_info,
    revision_info_raw_signature,
    revision_tree,
)
from lore import Lore
from lore_server import (
    allocate_free_port,
    generate_server_config,
    lore_local_server,
)

# Methods expected to return a status other than UNIMPLEMENTED when mounted.
# `ContentDiff` has a separate test because its mounted handler is unimplemented.
REGISTERED_BY_THIN = [REVISION_INFO, REVISION_DIFF, REVISION_TREE]

# Every service block a thin server disables: all but `thin_client_service`.
DISABLED_BY_THIN = [
    "admin_service",
    "storage_service",
    "revision_service",
    "repository_service",
    "environment_service",
    "lock_service",
    "notification_service",
]

# Every service the thin server must refuse. The `urc.rpc` entries confirm that
# disabling a block also removes its write-capable legacy service.
REFUSED_BY_THIN = [
    STORAGE_QUERY,
    "/lore.revision.v1.RevisionService/BranchGet",
    "/lore.repository.v1.RepositoryService/RepositoryCreate",
    "/lore.environment.v1.EnvironmentService/EnvironmentGet",
    "/urc.rpc.StorageService/Query",
    "/urc.rpc.RevisionService/BranchPush",
    "/urc.rpc.RepositoryService/RepositoryCreate",
    "/urc.rpc.EnvironmentService/Get",
    "/urc.rpc.AdminService/ServerInfo",
    "/urc.lock.LockService/Lock",
    "/lore.notification.NotificationService/Subscribe",
]


# A push returns before the main server flushes it, and `gha.toml` sets
# `flush_delay_seconds = 10`, so reads poll instead of assuming immediacy.
REPLICATION_TIMEOUT_SECONDS = 30.0
POLL_INTERVAL_SECONDS = 0.5


def _still_replicating(result) -> bool:
    """Whether a failed read is the replication lag rather than a defect.

    An absent revision answers NOT_FOUND; an unresolved address answers
    INTERNAL with a message containing "not found". Matching on that message
    keeps the check narrow: an INTERNAL reporting anything else is a real
    failure.
    """
    status = result[0]
    if status == grpc.StatusCode.NOT_FOUND:
        return True

    # Both the unary and streaming helpers put the status message last.
    details = result[-1]
    return status == grpc.StatusCode.INTERNAL and "not found" in details.lower()


def until_found(read, timeout: float = REPLICATION_TIMEOUT_SECONDS):
    """Call `read` until it stops reporting data that has not replicated yet.

    Returns whatever `read` last returned, so a genuine failure reaches the
    assertion instead of being reported as a timeout.
    """
    deadline = time.monotonic() + timeout
    while True:
        result = read()
        if not _still_replicating(result) or time.monotonic() >= deadline:
            return result
        time.sleep(POLL_INTERVAL_SECONDS)


def configure_thin_server(server_env: dict) -> dict:
    """Restrict a generated server to `ThinClientService`.

    The service flags govern only the gRPC router. QUIC and HTTP are separate
    listeners with write routes, so both must be disabled. Each environment
    flag is scalar, and `config` converts the string `"false"` to a boolean.
    """
    for service in DISABLED_BY_THIN:
        key = f"LORE__SERVER__GRPC_PUBLIC_SERVICES__{service.upper()}__ENABLED"
        server_env[key] = "false"

    server_env["LORE__SERVER__QUIC__ENABLED"] = "false"
    server_env["LORE__SERVER__HTTP__ENABLED"] = "false"
    return server_env


def require_thin_services(thin_target: str) -> None:
    """Fail immediately if the server does not honor the service flags.

    A server that ignores them would fail every refusal assertion below. This
    usually means the `loreserver` binary predates the flags. Probe the route so
    the check does not depend on log level.
    """
    if is_served(thin_target, STORAGE_QUERY):
        pytest.fail(
            f"the server at {thin_target} registered StorageService, so it did "
            "not honour server.grpc_public_services.storage_service.enabled. Its "
            "binary most likely predates the flag. Rebuild it, or pass "
            "--lore-server-binary debug (the default is release).",
            pytrace=False,
        )


@pytest.mark.smoke
@pytest.mark.xdist_group("thin_services")
class TestThinClientServices:
    """Run a thin server beside the unrestricted main server."""

    @pytest.fixture(scope="class")
    def thin_ports(self):
        # QUIC and gRPC share one port by convention (UDP vs TCP). QUIC is
        # disabled here, so the entry only satisfies the config generator.
        shared_port = allocate_free_port()
        return {
            "quic": shared_port,
            "grpc": shared_port,
            "http": allocate_free_port(),
            "internal": allocate_free_port(),
        }

    @pytest.fixture(scope="class")
    def thin_server(
        self, request, tmp_path_factory, thin_ports, lore_server_executable_path
    ):
        server_root, server_env = generate_server_config(
            request, tmp_path_factory, thin_ports
        )
        yield from lore_local_server(
            server_root,
            configure_thin_server(server_env),
            lore_server_executable_path,
        )

    @pytest.fixture(scope="class")
    def thin_target(self, thin_ports):
        return f"127.0.0.1:{thin_ports['grpc']}"

    @pytest.fixture(scope="class")
    def full_target(self, lore_main_server_ports):
        return f"127.0.0.1:{lore_main_server_ports['grpc']}"

    @pytest.fixture(scope="class")
    def thin_services_in_effect(self, thin_server, thin_target):
        require_thin_services(thin_target)

    @pytest.mark.parametrize("method", REGISTERED_BY_THIN)
    def test_the_thin_server_serves_the_thin_client_service(
        self, thin_services_in_effect, thin_target, method
    ):
        assert is_served(thin_target, method)

    @pytest.mark.parametrize("method", REFUSED_BY_THIN)
    def test_the_thin_server_refuses_everything_else(
        self, thin_services_in_effect, thin_target, method
    ):
        assert method_status(thin_target, method) == grpc.StatusCode.UNIMPLEMENTED, (
            f"{method} must not be registered when its block sets enabled = false"
        )

    @pytest.mark.parametrize("method", REFUSED_BY_THIN + REGISTERED_BY_THIN)
    def test_absent_service_blocks_still_serve_everything(self, full_target, method):
        """Confirm that an absent block leaves its service enabled.

        This control is independent of the thin server, so it still reports a
        failure when the thin server cannot start.
        """
        assert is_served(full_target, method), (
            f"{method} must remain registered when its block is absent"
        )


@pytest.mark.smoke
@pytest.mark.xdist_group("thin_services")
class TestThinClientServesData:
    """Push through the full server and read through the thin server.

    The thin server runs a `ReplicatedStore` immutable store that reads from
    the main server's internal QUIC listener. Sharing the main server's store
    directory would not work: the local store takes an exclusive file lock,
    so the second process would wait on it and never become healthy.
    """

    @pytest.fixture(scope="class")
    def thin_ports(self):
        shared_port = allocate_free_port()
        return {
            "quic": shared_port,
            "grpc": shared_port,
            "http": allocate_free_port(),
            "internal": allocate_free_port(),
        }

    @pytest.fixture(scope="class")
    def thin_server(
        self,
        request,
        tmp_path_factory,
        thin_ports,
        lore_server_executable_path,
        lore_main_server_ports,
    ):
        server_root, server_env = generate_server_config(
            request, tmp_path_factory, thin_ports
        )
        server_env = configure_thin_server(server_env)

        # Replicated store, so a local read is fetched from the main server over
        # its internal QUIC listener. This is what puts the main server's data
        # behind the thin server without either process touching the other's
        # store files.
        server_env["LORE__IMMUTABLE_STORE__MODE"] = "replicated"
        server_env["LORE__SERVER__GRPC_INTERNAL__ENABLED"] = "false"

        # Replicating the immutable store is not enough on its own. Resolving a
        # revision goes through the mutable store, and this server's own is empty,
        # so a signature written by the main server answers NOT_FOUND until the
        # mutable store points at the main server too.
        server_hostname = request.config.getoption("--lore-server-hostname")
        main_grpc_port = lore_main_server_ports["grpc"]
        server_env["LORE__MUTABLE_STORE__MODE"] = "remote"
        server_env["LORE__MUTABLE_STORE__REMOTE__REMOTE_URL"] = (
            f"lore://{server_hostname}:{main_grpc_port}"
        )

        # Override the replicated-store settings via local.toml. The server no
        # longer reads default.toml from disk (it is baked into the binary), so
        # local.toml is the override file that gets loaded, layered last.
        main_internal_port = lore_main_server_ports["internal"]
        config_path = server_root / "lore-server" / "config" / "local.toml"
        with open(config_path, "a", encoding="utf-8") as thin_config:
            thin_config.write("[immutable_store.replicated]\n")
            thin_config.write(
                f'remote_url = "quic://{server_hostname}:{main_internal_port}"\n'
            )
            thin_config.write("regenerate_retry.initial_backoff_ms = 1\n")
            thin_config.write("regenerate_retry.max_backoff_ms = 1\n")
            thin_config.write("regenerate_retry.max_attempts = 1\n")
            thin_config.write("periodic_client_refresh_secs = 180\n")

        yield from lore_local_server(
            server_root, server_env, lore_server_executable_path
        )

    @pytest.fixture(scope="class")
    def thin_target(self, thin_ports):
        return f"127.0.0.1:{thin_ports['grpc']}"

    @pytest.fixture(scope="class")
    def full_target(self, lore_main_server_ports):
        return f"127.0.0.1:{lore_main_server_ports['grpc']}"

    @pytest.fixture(scope="class")
    def thin_services_in_effect(self, thin_server, thin_target):
        require_thin_services(thin_target)

    @pytest.fixture
    def pushed_revisions(
        self, request, new_lore_repo, auto_lore_local_server, lore_main_server_ports
    ):
        """Create a repository on the main server and push two revisions.

        Two, not one, because `RevisionDiff` needs a pair to compare. Returns the
        repository id and both signatures, oldest first.
        """
        server_host_name = request.config.getoption("--lore-server-hostname")
        repo_id = uuid.uuid4().hex
        main_grpc_port = lore_main_server_ports["grpc"]
        remote_url = f"lore://{server_host_name}:{main_grpc_port}/repo-{repo_id}"

        repo: Lore = new_lore_repo(remote_path=remote_url, repo_id=repo_id)
        signatures = []

        for line in ("first revision\n", "second revision\n"):
            with repo.open_file("thin-client.txt", "a+") as handle:
                handle.writelines([line])

            repo.stage(scan=True)
            repo.commit()
            repo.push()

            info = repo.revision_info()
            assert info.has_valid_signature(), (
                f"expected a 64-character signature, got {info.signature!r}"
            )
            signatures.append(info.signature)

        assert signatures[0] != signatures[1], "both pushes produced one revision"
        return repo_id, signatures[0], signatures[1]

    def test_the_thin_server_returns_the_pushed_revision(
        self, thin_services_in_effect, thin_target, pushed_revisions
    ):
        repo_id, _first, signature = pushed_revisions
        status, body, details = until_found(
            lambda: revision_info(thin_target, repo_id, signature)
        )

        assert status == grpc.StatusCode.OK, f"RevisionInfo failed: {status} {details}"
        assert response_carries_a_revision(body), (
            "the thin server answered successfully without a Revision record"
        )

    def test_the_answer_came_from_the_thin_server(
        self, thin_services_in_effect, thin_target, full_target
    ):
        """Distinguish the thin server from the full server.

        Both servers mount `ThinClientService`, but only the thin server refuses
        storage. Checking both targets proves that the thin address answered.
        """
        assert (
            method_status(thin_target, STORAGE_QUERY) == grpc.StatusCode.UNIMPLEMENTED
        ), "the thin target served StorageService, so it is not a thin process"
        assert is_served(full_target, STORAGE_QUERY), (
            "the full target refused StorageService, so the two targets are not "
            "the two servers this test assumes"
        )
        assert thin_target != full_target

    def test_an_unknown_revision_is_not_reported_as_found(
        self, thin_services_in_effect, thin_target, pushed_revisions
    ):
        """Guards the assertion above. A server that answered the same way for
        any signature would satisfy `test_the_thin_server_returns_the_pushed_revision`
        without looking anything up."""
        repo_id, _first, signature = pushed_revisions
        # The known lookup polls, because the push may not have flushed yet. The
        # fabricated one must not: it is NOT_FOUND forever, and polling it would
        # spend the whole replication timeout on every run.
        known_status, known_body, known_details = until_found(
            lambda: revision_info(thin_target, repo_id, signature)
        )
        unknown_status, unknown_body, _ = revision_info(thin_target, repo_id, "00" * 32)

        assert (known_status, known_body) != (unknown_status, unknown_body), (
            "the thin server answers identically for a real and a fabricated "
            "revision, so the successful lookup above proves nothing. "
            f"known={known_status} {len(known_body)}B {known_details} "
            f"unknown={unknown_status} {len(unknown_body)}B"
        )

    def test_a_partial_signature_is_refused_as_a_failed_precondition(
        self, thin_services_in_effect, thin_target, pushed_revisions
    ):
        """A signature shorter than a whole one is a request the server cannot
        act on, not a revision it looked for and did not find. FAILED_PRECONDITION
        says retrying it unchanged fails the same way, and is distinguishable
        from the NOT_FOUND a whole but unknown signature answers.
        """
        repo_id, _first, signature = pushed_revisions
        # Poll on the whole signature so the push has certainly landed, which
        # keeps the refusal below from passing for want of the revision.
        until_found(lambda: revision_info(thin_target, repo_id, signature))

        partial = bytes.fromhex(signature)[:8]
        status, _body, details = revision_info_raw_signature(
            thin_target, repo_id, partial
        )

        assert status == grpc.StatusCode.FAILED_PRECONDITION, (
            f"expected FAILED_PRECONDITION for a partial signature, got {status} {details}"
        )
        assert "partial revision hash signature" in details, (
            f"the refusal does not say what was wrong with the request: {details}"
        )

    def test_the_thin_server_walks_the_revision_tree(
        self, thin_services_in_effect, thin_target, pushed_revisions
    ):
        """`RevisionTree` streams the tree of a revision. With no `path_prefix`
        the walk covers the whole repository, so the file pushed above must
        produce at least one entry."""
        repo_id, _first, signature = pushed_revisions
        status, messages, details = until_found(
            lambda: revision_tree(thin_target, repo_id, signature)
        )

        assert status == grpc.StatusCode.OK, f"RevisionTree failed: {status} {details}"
        assert messages, "RevisionTree streamed no entries for a revision with a file"

    def test_the_thin_server_diffs_two_revisions(
        self, thin_services_in_effect, thin_target, pushed_revisions
    ):
        """`RevisionDiff` streams the changes between two revisions. The second
        push appended a line to the file the first created, so the diff is
        non-empty."""
        repo_id, first, second = pushed_revisions
        status, messages, details = until_found(
            lambda: revision_diff(thin_target, repo_id, first, second)
        )

        assert status == grpc.StatusCode.OK, f"RevisionDiff failed: {status} {details}"
        assert messages, "RevisionDiff streamed nothing between two differing revisions"

    def test_content_diff_is_mounted_but_unimplemented(
        self, thin_services_in_effect, thin_target, pushed_revisions
    ):
        """Distinguish the mounted handler from an absent route.

        Both return UNIMPLEMENTED. The handler in `thinclient/v1/service.rs`
        includes a message, while tonic's fallback does not.
        """
        repo_id, _first, _second = pushed_revisions
        details = method_details(
            thin_target, CONTENT_DIFF, b"", repository_metadata(repo_id)
        )

        assert "not yet implemented" in details, (
            f"expected the handler's own message, got {details!r} -- an empty "
            "message would mean ContentDiff is not mounted at all"
        )
