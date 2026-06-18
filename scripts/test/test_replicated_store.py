#!/usr/bin/python3
# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import json
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


def ensure_fragment_exists_output(wants_exists: bool, json_output: str):
    """Checks to see if the fragment exists in the remote store"""
    num_found = 0
    num_not_found = 0

    events = json_output.splitlines()
    for event in events:
        parsed = json.loads(event)
        if parsed["tagName"] == "repositoryStoreImmutableQuery":
            data = parsed["data"]
            if data["remote"] is True:
                # there are many types of states, but only status 3 is explicitly 'NotFound'
                fragment_found = data["status"] != 3
                if fragment_found:
                    num_found = num_found + 1
                else:
                    num_not_found = num_not_found + 1

    assert (num_found > 0) or (num_not_found > 0), (
        f"Could not confirm exists={wants_exists} in {json_output}"
    )

    if wants_exists:
        assert num_found > 0, "Could not confirm fragment exists"
    if not wants_exists:
        assert num_found == 0, "A fragment was found when it wasn't meant to be"


@pytest.mark.smoke
@pytest.mark.xdist_group("replicated_store")
class TestReplicatedStore:
    @pytest.fixture(scope="class")
    def lore_local_server_2_config(
        self, request, tmp_path_factory, lore_main_server_ports
    ):
        # QUIC and GRPC share one port by convention (UDP vs TCP; no collision)
        shared_port = allocate_free_port()
        server_2_ports = {
            "quic": shared_port,
            "grpc": shared_port,
            "http": allocate_free_port(),
            "internal": allocate_free_port(),
        }
        (new_server_root, server_2_env) = generate_server_config(
            request, tmp_path_factory, server_2_ports
        )
        main_server_internal_port = lore_main_server_ports["internal"]

        # we want to test the replicated store in isolation,
        # but the gRPC internal server (not to be confused with the QUIC internal service)
        # requires a local store, so disable that service.
        # This allows us to also not have to worry about composite local store caching
        # of results when doing immutable get tests
        server_2_env["LORE__IMMUTABLE_STORE__MODE"] = "replicated"
        server_2_env["LORE__SERVER__GRPC_INTERNAL__ENABLED"] = "false"

        # Override the replicated-store settings via local.toml. The server no
        # longer reads default.toml from disk (it is baked into the binary), so
        # local.toml is the override file that gets loaded, layered last.
        with open(
            os.path.join(new_server_root, "lore-server", "config", "local.toml"),
            "a",
            encoding="utf-8",
        ) as server_2_config:
            server_hostname = request.config.getoption("--lore-server-hostname")
            server_2_config.write("[immutable_store.replicated]\n")
            server_2_config.write(
                f'remote_url = "quic://{server_hostname}:{main_server_internal_port}"\n'
            )
            server_2_config.write("regenerate_retry.initial_backoff_ms = 1\n")
            server_2_config.write("regenerate_retry.max_backoff_ms = 1\n")
            server_2_config.write("regenerate_retry.max_attempts = 1\n")
            server_2_config.write("periodic_client_refresh_secs = 180\n")

        return new_server_root, server_2_env

    @pytest.fixture(scope="class")
    def lore_server_with_replicated_store(
        self, lore_local_server_2_config, lore_server_executable_path
    ):
        """
        Runs a loreserver that delegates store operations to the main lore server
        """
        (server_root, server_env) = lore_local_server_2_config
        server_proc, server_log_path, server_log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )

        yield server_proc, server_log_path, server_log_fd

        # Server teardown
        _kill_server_by_pid(
            server_proc.pid, server_log_path, label="replicated store server 2"
        )
        server_log_fd.close()

    @pytest.fixture()
    def same_repo_id_different_remotes(
        self,
        request,
        lore_local_server_config,
        lore_local_server_2_config,
        auto_lore_local_server,
        lore_server_with_replicated_store,
        new_lore_repo,
    ):
        server_host_name = request.config.getoption("--lore-server-hostname")
        (server_1_root, server_1_env) = lore_local_server_config
        (server_2_root, server_2_env) = lore_local_server_2_config

        common_repo_id = uuid.uuid4().hex
        common_repo_name = f"repo-{common_repo_id}"

        # specify a repository URL so the specific Lore Server is used to create it
        remote_url_server_1 = (
            f"lore://{server_host_name}:{server_1_env['LORE__SERVER__GRPC__PORT']}"
        )
        remote_url_server_1_path = f"{remote_url_server_1}/{common_repo_name}"
        remote_url_server_2 = (
            f"lore://{server_host_name}:{server_2_env['LORE__SERVER__GRPC__PORT']}"
        )
        remote_url_server_2_path = f"{remote_url_server_2}/{common_repo_name}"
        server_1_repo = new_lore_repo(
            remote_url=remote_url_server_1,
            remote_path=remote_url_server_1_path,
            repo_id=common_repo_id,
        )
        server_2_repo = new_lore_repo(
            remote_url=remote_url_server_2,
            remote_path=remote_url_server_2_path,
            repo_id=common_repo_id,
        )

        return server_1_repo, server_2_repo, common_repo_name

    def test_immutable_exists(self, same_repo_id_different_remotes):
        server_1_repo, server_2_repo, _common_repo_name = same_repo_id_different_remotes

        with server_1_repo.open_file("some-file-2.txt", "w+") as file:
            file.writelines(["I like to share"])
        server_1_file_address = (
            "a47d0689fdc3af95baf6aa39061cfcc6a863210cf9f60ed3e488d5969bec47e9"
        )

        # server 2 says it does not exist - it is actually delegating to server 1
        # since the CLI will invoke an Immutable ExistsBatch request
        logger.info("pre-commit check server 2")
        server_2_output = server_2_repo.repository_store_immutable_query(
            server_1_file_address, json=True
        )
        ensure_fragment_exists_output(False, server_2_output)

        server_1_repo.stage(scan=True)
        server_1_repo.commit()
        server_1_repo.push()

        # it is committed to server 1, so should exist
        logger.info("Checking server 1")
        server_1_output = server_1_repo.repository_store_immutable_query(
            server_1_file_address, json=True
        )
        ensure_fragment_exists_output(True, server_1_output)

        # and since server 2 is under the hood querying server 1, it should also say it exists
        logger.info("Checking server 2")
        server_2_output = server_2_repo.repository_store_immutable_query(
            server_1_file_address, json=True
        )
        ensure_fragment_exists_output(True, server_2_output)

    def test_immutable_get_and_put(self, same_repo_id_different_remotes):
        _server_1_repo, server_2_repo, common_repo_name = same_repo_id_different_remotes

        text_file = "text-File.txt"

        # Immutable Put works
        server_2_repo.write_commit_push(
            None,
            {text_file: ["One line\n", "Another line\n", "Third line\n"]},
        )

        # Clone involves Immutable Get
        cloned_repo = server_2_repo.clone(debug=True)
        cloned_repo.compare_file(server_2_repo, text_file)

    # todo(plockhart) enable obliterate test once test-suite Auth has been updated
    @pytest.mark.skip(
        reason="Lore Server running without an Auth provider configured would need extended to add obliterate permissions"
    )
    def test_immutable_obliterate(self, same_repo_id_different_remotes):
        server_1_repo, server_2_repo, _common_repo_name = same_repo_id_different_remotes

        file_path = "some-file-3.txt"
        with server_2_repo.open_file(file_path, "w+") as file:
            file.writelines(["I like to share"])
        file_address = (
            "a47d0689fdc3af95baf6aa39061cfcc6a863210cf9f60ed3e488d5969bec47e9"
        )
        server_2_repo.stage(scan=True)
        server_2_repo.commit()
        server_2_repo.push()

        # it is committed to server 1, so should exist
        logger.info("pre obliterate checking server 1")
        server_1_output = server_1_repo.repository_store_immutable_query(
            file_address, json=True
        )
        ensure_fragment_exists_output(True, server_1_output)
        # and since server 2 is under the hood querying server 1, it should also say it exists
        logger.info("pre obliterate checking server 2")
        server_2_output = server_2_repo.repository_store_immutable_query(
            file_address, json=True
        )
        ensure_fragment_exists_output(True, server_2_output)

        # obliterate is received by server 2, but it delegates to server 1
        server_2_repo.file_obliterate(path=file_path)

        # so both servers should now say the address does not exist
        logger.info("Checking server 1")
        server_1_output = server_1_repo.repository_store_immutable_query(
            file_address, json=True
        )
        ensure_fragment_exists_output(False, server_1_output)
        # and since server 2 is under the hood querying server 1, it should also say it exists
        logger.info("Checking server 2")
        server_2_output = server_2_repo.repository_store_immutable_query(
            file_address, json=True
        )
        ensure_fragment_exists_output(False, server_2_output)
