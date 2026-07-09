#!/usr/bin/python3
# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import uuid

import pytest
from lore_server import (
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)

logger = logging.getLogger(__name__)


@pytest.mark.smoke
@pytest.mark.xdist_group("forwarded_requests")
class TestForwardedBranchCreate:
    """
    Smoke tests for the forwarded-request delegation path.

    Two independent Lore servers are started, each with their own mutable store,
    so branch state is fully isolated between them. Server 2 is configured with
    [server.grpc_public_services.forwarded_requests] pointing at Server 1's
    internal gRPC port and revision_branch_create = true. When a client calls
    BranchCreate on Server 2, Server 2 forwards the request to Server 1 instead
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
        _kill_server_by_pid(server_proc.pid, log_path, label="forwarded requests server 1")
        log_fd.close()

    @pytest.fixture(scope="class")
    def server_2_config(self, request, tmp_path_factory, server_1_config):
        """
        Config for Server 2: the delegation *source*. Its local.toml is extended
        with the forwarded_requests block that tells it to forward BranchCreate
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
        _kill_server_by_pid(server_proc.pid, log_path, label="forwarded requests server 2")
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
        assert not server_1_repo.branch_list().has_remote_branch(branch_name), (
            f"Branch '{branch_name}' should not exist on server 1 before creation"
        )
        assert not server_2_repo.branch_list().has_remote_branch(branch_name), (
            f"Branch '{branch_name}' should not exist on server 2 before creation"
        )

        # branch_create is local-only; the BranchCreate RPC is only sent to
        # the server when the branch is explicitly pushed. branch_push triggers
        # that RPC on Server 2, which delegates it to Server 1.
        logger.info("Creating branch '%s' via server 2 (delegates to server 1)", branch_name)
        server_2_repo.branch_create(branch_name)
        server_2_repo.branch_push(branch_name)

        # The write went to Server 1's mutable store — branch exists there
        assert server_1_repo.branch_list().has_remote_branch(branch_name), (
            f"Branch '{branch_name}' should exist on server 1 after delegated create"
        )

        # Server 2's mutable store was never written to — branch absent there
        assert not server_2_repo.branch_list().has_remote_branch(branch_name), (
            f"Branch '{branch_name}' should not exist on server 2's store "
            "(request was delegated, not written locally)"
        )
