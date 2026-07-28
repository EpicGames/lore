#!/usr/bin/python3
# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import json
import logging
import os
import time
import uuid

import pytest
from lore_server import (
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)

logger = logging.getLogger(__name__)


def ensure_fragment_exists_output(
    remote_flag: bool, wants_exists: bool, json_output: str
):
    """Checks to see if the fragment exists in the remote store"""
    num_found = 0
    num_not_found = 0

    events = json_output.splitlines()
    for event in events:
        parsed = json.loads(event)
        if parsed["tagName"] == "repositoryStoreImmutableQuery":
            data = parsed["data"]
            if data["remote"] is remote_flag:
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


def ensure_fragment_exists_locally(wants_exists: bool, json_output: str):
    ensure_fragment_exists_output(False, wants_exists, json_output)


def ensure_fragment_exists_remotely(wants_exists: bool, json_output: str):
    ensure_fragment_exists_output(True, wants_exists, json_output)


@pytest.mark.smoke
@pytest.mark.xdist_group("topology")
class TestTopology:
    """
    Topology backed store related tests. Server 1 is the main server fixture that is run for all tests
    including those outside this - its config remains unchanged.

    Server 2 is a new server with topology configured, fixed to point to Server 1. It explicitly has read
    replicas disabled, to prove out write replication related tests.

    Server 3 is a new server with topology configured, fixed to point to Server 1. It has read and write
    replicas enabled, but (at time if writing) intended for proving out read replica functionality
    """

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
        # set up a composite store for replication
        server_2_env["LORE__IMMUTABLE_STORE__MODE"] = "composite"
        server_2_env["LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__MODE"] = "local"
        server_2_env[
            "LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__LOCAL__FLUSH_DELAY_SECONDS"
        ] = "1"
        server_2_env["LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__LOCAL__PATH"] = "./local"
        server_2_env["LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__MODE"] = "local"
        server_2_env[
            "LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__LOCAL__FLUSH_DELAY_SECONDS"
        ] = "1"
        server_2_env["LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__LOCAL__PATH"] = (
            "./durable"
        )
        # building replicas for composite store based off topology updates
        server_2_env[
            "LORE__IMMUTABLE_STORE__COMPOSITE__REPLICA_FACTORY__CLIENT_MESSAGE_BUFFER"
        ] = "100"
        # read replicas specifically disabled for server 2
        server_2_env[
            "LORE__IMMUTABLE_STORE__COMPOSITE__REPLICA_FACTORY__READ_REPLICAS_ENABLED"
        ] = "false"
        # write a Stanza for topology
        with open(
            os.path.join(new_server_root, "lore-server", "config", "local.toml"),
            "a",
            encoding="utf-8",
        ) as server_2_config:
            # write a stanza to set up Replication to server 1
            server_hostname = request.config.getoption("--lore-server-hostname")
            server_2_config.write("[topology]\n")
            server_2_config.write('provider = "fixed"\n')
            server_2_config.write("\n")
            server_2_config.write("[topology.fixed]\n")
            server_2_config.write(
                f'peers = [{{ address = "{server_hostname}", port = {main_server_internal_port}, locality = "SameRegion" }}]\n'
            )

        return new_server_root, server_2_env

    @pytest.fixture(scope="class")
    def lore_local_server_3_config(
        self, request, tmp_path_factory, lore_main_server_ports
    ):
        # QUIC and GRPC share one port by convention (UDP vs TCP; no collision)
        shared_port = allocate_free_port()
        server_3_ports = {
            "quic": shared_port,
            "grpc": shared_port,
            "http": allocate_free_port(),
            "internal": allocate_free_port(),
        }
        (new_server_root, server_3_env) = generate_server_config(
            request, tmp_path_factory, server_3_ports
        )
        main_server_internal_port = lore_main_server_ports["internal"]
        # set up a composite store for replication
        server_3_env["LORE__IMMUTABLE_STORE__MODE"] = "composite"
        server_3_env["LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__MODE"] = "local"
        server_3_env[
            "LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__LOCAL__FLUSH_DELAY_SECONDS"
        ] = "1"
        server_3_env["LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__LOCAL__PATH"] = "./local"
        server_3_env["LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__MODE"] = "local"
        server_3_env[
            "LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__LOCAL__FLUSH_DELAY_SECONDS"
        ] = "1"
        server_3_env["LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__LOCAL__PATH"] = (
            "./durable"
        )
        # building replicas for composite store based off topology updates
        server_3_env[
            "LORE__IMMUTABLE_STORE__COMPOSITE__REPLICA_FACTORY__CLIENT_MESSAGE_BUFFER"
        ] = "100"
        server_3_env[
            "LORE__IMMUTABLE_STORE__COMPOSITE__REPLICA_FACTORY__READ_REPLICAS_ENABLED"
        ] = "true"
        # write a Stanza for topology
        with open(
            os.path.join(new_server_root, "lore-server", "config", "local.toml"),
            "a",
            encoding="utf-8",
        ) as server_3_config:
            # write a stanza to set up Replication to server 1
            server_hostname = request.config.getoption("--lore-server-hostname")
            server_3_config.write("[topology]\n")
            server_3_config.write('provider = "fixed"\n')
            server_3_config.write("\n")
            server_3_config.write("[topology.fixed]\n")
            server_3_config.write(
                f'peers = [{{ address = "{server_hostname}", port = {main_server_internal_port}, locality = "SameRegion" }}]\n'
            )

        return new_server_root, server_3_env

    @pytest.fixture(scope="class")
    def lore_server_with_no_read_replicas(
        self, lore_local_server_2_config, lore_server_executable_path
    ):
        """
        Runs a loreserver locally that write replicates to the main server
        but does not have read replicas from it
        """
        (server_root, server_env) = lore_local_server_2_config
        server_proc, server_log_path, server_log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )

        yield server_proc, server_log_path, server_log_fd

        # Server teardown
        _kill_server_by_pid(server_proc.pid, server_log_path, label="topology server 2")
        server_log_fd.close()

    @pytest.fixture(scope="class")
    def lore_server_with_read_replicas(
        self, lore_local_server_3_config, lore_server_executable_path
    ):
        """
        Runs a loreserver locally that read and write replicates to the main server
        """
        (server_root, server_env) = lore_local_server_3_config
        server_proc, server_log_path, server_log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )

        yield server_proc, server_log_path, server_log_fd

        # Server teardown
        _kill_server_by_pid(server_proc.pid, server_log_path, label="topology server 3")
        server_log_fd.close()

    @pytest.fixture()
    def same_repo_id_different_remotes(
        self,
        request,
        lore_local_server_config,
        lore_local_server_2_config,
        lore_server_with_no_read_replicas,
        lore_local_server_3_config,
        lore_server_with_read_replicas,
        auto_lore_local_server,
        new_lore_repo,
    ):
        server_host_name = request.config.getoption("--lore-server-hostname")
        (server_1_root, server_1_env) = lore_local_server_config
        (server_2_root, server_2_env) = lore_local_server_2_config
        (server_3_root, server_3_env) = lore_local_server_3_config

        common_repo_id = uuid.uuid4().hex
        # specify a repository URL so the specific Lore Server is used to create it
        remote_url_server_1 = f"lore://{server_host_name}:{server_1_env['LORE__SERVER__GRPC__PORT']}/repo-{common_repo_id}"
        remote_url_server_2 = f"lore://{server_host_name}:{server_2_env['LORE__SERVER__GRPC__PORT']}/repo-{common_repo_id}"
        remote_url_server_3 = f"lore://{server_host_name}:{server_3_env['LORE__SERVER__GRPC__PORT']}/repo-{common_repo_id}"
        server_1_repo = new_lore_repo(
            remote_path=remote_url_server_1, repo_id=common_repo_id
        )
        server_2_repo = new_lore_repo(
            remote_path=remote_url_server_2, repo_id=common_repo_id
        )
        server_3_repo = new_lore_repo(
            remote_path=remote_url_server_3, repo_id=common_repo_id
        )

        return server_1_repo, server_2_repo, server_3_repo

    def test_server_1_doesnt_replicate_to_server_2(
        self, same_repo_id_different_remotes
    ):
        server_1_repo, server_2_repo, _ = same_repo_id_different_remotes

        with server_1_repo.open_file("some-file-1.txt", "w+") as file:
            file.writelines(["I'm selfish"])
        server_1_file_address = (
            "21e3d6b49ab8452dd902c263cbfa96c2808d6333fc66e875bef1e6ab68ea2625"
        )

        server_1_repo.stage(scan=True)
        server_1_repo.commit()
        server_1_repo.push()

        logger.info("Checking server 1")
        server_1_output = server_1_repo.repository_store_immutable_query(
            server_1_file_address, json=True
        )
        ensure_fragment_exists_remotely(True, server_1_output)

        logger.info("Checking server 2")
        server_2_output = server_2_repo.repository_store_immutable_query(
            server_1_file_address, json=True
        )
        ensure_fragment_exists_remotely(False, server_2_output)

    def test_server_2_replicates_to_server_1(self, same_repo_id_different_remotes):
        server_1_repo, server_2_repo, _ = same_repo_id_different_remotes

        with server_2_repo.open_file("some-file-2.txt", "w+") as file:
            file.writelines(["I like to share"])
        server_2_file_address = (
            "a47d0689fdc3af95baf6aa39061cfcc6a863210cf9f60ed3e488d5969bec47e9"
        )

        server_2_repo.stage(scan=True)
        server_2_repo.commit()
        server_2_repo.push()

        logger.info("Checking server 2")
        server_2_output = server_2_repo.repository_store_immutable_query(
            server_2_file_address, json=True
        )
        ensure_fragment_exists_remotely(True, server_2_output)

        # give it time to replicate
        time.sleep(0.1)

        logger.info("Checking server 1")
        server_1_output = server_1_repo.repository_store_immutable_query(
            server_2_file_address, json=True
        )
        ensure_fragment_exists_remotely(True, server_1_output)

    def test_server_2_replicates_large_fragments_to_server_1(
        self, same_repo_id_different_remotes
    ):
        server_1_repo, server_2_repo, _ = same_repo_id_different_remotes

        with server_2_repo.open_file("large_file.uasset", "w+b") as output_file:
            output_file.write(b"\xab" * (1024 * 1024))
        server_2_file_address = (
            "baf31a57cebec1bb4090203b2b53b0a7c07d3e18e9fa36b66b051d5acb7bf86f"
        )

        server_2_repo.stage(scan=True)
        server_2_repo.commit()
        server_2_repo.push()

        logger.info("Checking server 2")
        server_2_output = server_2_repo.repository_store_immutable_query(
            server_2_file_address, json=True
        )
        ensure_fragment_exists_remotely(True, server_2_output)

        # give it time to replicate
        time.sleep(0.1)

        logger.info("Checking server 1")
        server_1_output = server_1_repo.repository_store_immutable_query(
            server_2_file_address, json=True
        )
        ensure_fragment_exists_remotely(True, server_1_output)

    # todo(plockhart) we need to artificially slow down this durable local store to make the test work
    @pytest.mark.skip(
        reason="Durable returning a response so quickly short invalidates this test"
    )
    def test_server_3_queries_server_1_via_read_replica(
        self, same_repo_id_different_remotes
    ):
        """
        Server 3 has topology pointing to server 1, so server 1 is a read replica peer.
        Data pushed to server 1 should be queryable from server 3.
        """
        server_1_repo, _, server_3_repo = same_repo_id_different_remotes

        with server_1_repo.open_file("read-replica-test.txt", "w+") as file:
            file.writelines(["Read me remotely"])
        read_replica_file_address = (
            "7c5613a5e4e01b00bd058b61aa3615c3ba73e1a6f84b86d9c2ddfad682bf5c2f"
        )

        server_1_repo.stage(scan=True)
        server_1_repo.commit()
        server_1_repo.push()

        logger.info("Checking server 1 has the data")
        server_1_output = server_1_repo.repository_store_immutable_query(
            read_replica_file_address, json=True
        )
        ensure_fragment_exists_remotely(True, server_1_output)

        logger.info("Checking server 2 can read from server 1 via read replica")
        server_3_output = server_3_repo.repository_store_immutable_query(
            read_replica_file_address, json=True
        )
        ensure_fragment_exists_remotely(True, server_3_output)

    # todo(plockhart) we need to artificially slow down this durable local store to make the test work
    @pytest.mark.skip(
        reason="Durable returning a response so quickly short invalidates this test"
    )
    def test_server_3_gets_server_1_via_read_replica(
        self, same_repo_id_different_remotes
    ):
        """
        Server 3 has topology pointing to server 1, so server 1 is a read replica peer.
        If we push an address to server 1, then via server 3 the same address we should be able to query / exists / get it.
        If a user goes to push that same address to server 3 via new commit, since it already exists immutably in server 1
        they shouldn't need to upload it again.

        Then, if they reset their local immutable store, reset their local files and sync again, their storage client
        will try to retrieve that address. Since that address doesn't exist in server 3 it will read replica `get` it
        from server 1.
        """
        server_1_repo, _, server_3_repo = same_repo_id_different_remotes

        file_path = "get-text.txt"
        common_file_contents = "I was got by replication"
        file_address = (
            "fa58ca4d540beca84189fd78788bcb551393152d33b6c4ca6d12ec09c391151c"
        )

        # push a file to server 1
        with server_1_repo.open_file(file_path, "w+") as file:
            file.writelines([common_file_contents])
        server_1_repo.stage(scan=True)
        server_1_repo.commit()
        server_1_repo.push()

        # confirm server 3 will see the fragment exists and so doesn't need uploaded
        server_3_output = server_3_repo.repository_store_immutable_query(
            file_address, json=True
        )
        ensure_fragment_exists_remotely(True, server_3_output)
        ensure_fragment_exists_locally(False, server_3_output)

        # write the same file in server 3's repo and push.
        # The address already exists in server 1, so server 3 should find it
        # via read replica and advise the client to not need to upload the payload again.
        with server_3_repo.open_file(file_path, "w+") as file:
            file.writelines([common_file_contents])
        server_3_repo.stage(scan=True)
        server_3_repo.commit()
        server_3_repo.push()

        # delete all of server 3's local synced files and immutable store
        server_3_repo.rmtree(file_path)
        server_3_repo.rmtree(os.path.join(server_3_repo.dot_dir(), "immutable"))
        ensure_fragment_exists_locally(False, server_3_output)

        # The sync needs to retrieve the payload. Server 3 won't have it in its immutable store,
        # so it must `get` it from server 1 via read replica
        server_3_repo.reset(purge=True)
        server_3_repo.sync()

        # Step 4: Verify the file was retrieved correctly
        with server_3_repo.open_file(file_path, "r") as file:
            assert file.read() == common_file_contents


@pytest.mark.smoke
@pytest.mark.xdist_group("composite_topology")
class TestCompositeTopology:
    """
    Composite topology tests. Verifies that a server configured with a composite
    topology (multiple sub-topology sources) replicates to peers
    in different localities.

    Server layout:
    - Server 1: the main server fixture (SameRegion peer for the composite server)
    - Other-region server: a standalone server acting as the OtherRegion peer
    - Composite server: uses composite topology with two fixed sources —
      one SameRegion pointing at server 1, one OtherRegion pointing at the
      other-region server
    """

    @pytest.fixture(scope="class")
    def other_region_server_config(self, request, tmp_path_factory):
        """Config for a standalone server that acts as the OtherRegion peer."""
        shared_port = allocate_free_port()
        ports = {
            "quic": shared_port,
            "grpc": shared_port,
            "http": allocate_free_port(),
            "internal": allocate_free_port(),
        }
        return generate_server_config(request, tmp_path_factory, ports), ports

    @pytest.fixture(scope="class")
    def other_region_server(
        self, other_region_server_config, lore_server_executable_path
    ):
        """Launches the other-region server."""
        (server_root, server_env), _ports = other_region_server_config
        server_proc, server_log_path, server_log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )

        yield server_proc, server_log_path, server_log_fd

        _kill_server_by_pid(
            server_proc.pid, server_log_path, label="composite other-region server"
        )
        server_log_fd.close()

    @pytest.fixture(scope="class")
    def composite_server_config(
        self,
        request,
        tmp_path_factory,
        lore_main_server_ports,
        other_region_server_config,
    ):
        """Config for a server with composite topology: SameRegion → server 1,
        OtherRegion → other-region server."""
        shared_port = allocate_free_port()
        composite_ports = {
            "quic": shared_port,
            "grpc": shared_port,
            "http": allocate_free_port(),
            "internal": allocate_free_port(),
        }
        (new_server_root, server_env) = generate_server_config(
            request, tmp_path_factory, composite_ports
        )

        # composite immutable store
        server_env["LORE__IMMUTABLE_STORE__MODE"] = "composite"
        server_env["LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__MODE"] = "local"
        server_env[
            "LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__LOCAL__FLUSH_DELAY_SECONDS"
        ] = "1"
        server_env["LORE__IMMUTABLE_STORE__COMPOSITE__LOCAL__LOCAL__PATH"] = "./local"
        server_env["LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__MODE"] = "local"
        server_env[
            "LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__LOCAL__FLUSH_DELAY_SECONDS"
        ] = "1"
        server_env["LORE__IMMUTABLE_STORE__COMPOSITE__DURABLE__LOCAL__PATH"] = (
            "./durable"
        )
        server_env[
            "LORE__IMMUTABLE_STORE__COMPOSITE__REPLICA_FACTORY__CLIENT_MESSAGE_BUFFER"
        ] = "100"
        server_env[
            "LORE__IMMUTABLE_STORE__COMPOSITE__REPLICA_FACTORY__READ_REPLICAS_ENABLED"
        ] = "false"

        # composite topology with two fixed sources
        server_hostname = request.config.getoption("--lore-server-hostname")
        same_region_port = lore_main_server_ports["internal"]
        (_other_root, _other_env), other_ports = other_region_server_config
        other_region_port = other_ports["internal"]

        with open(
            os.path.join(new_server_root, "lore-server", "config", "local.toml"),
            "a",
            encoding="utf-8",
        ) as cfg:
            cfg.write("[topology]\n")
            cfg.write('provider = "composite"\n')
            cfg.write("\n")
            cfg.write("[[topology.composite.sources]]\n")
            cfg.write('provider = "fixed"\n')
            cfg.write("\n")
            cfg.write("[topology.composite.sources.fixed]\n")
            cfg.write(
                f'peers = [{{ address = "{server_hostname}", port = {same_region_port}, locality = "SameRegion" }}]\n'
            )
            cfg.write("\n")
            cfg.write("[[topology.composite.sources]]\n")
            cfg.write('provider = "fixed"\n')
            cfg.write("\n")
            cfg.write("[topology.composite.sources.fixed]\n")
            cfg.write(
                f'peers = [{{ address = "{server_hostname}", port = {other_region_port}, locality = "OtherRegion" }}]\n'
            )

        return new_server_root, server_env

    @pytest.fixture(scope="class")
    def composite_server(self, composite_server_config, lore_server_executable_path):
        """Launches the composite topology server."""
        (server_root, server_env) = composite_server_config
        server_proc, server_log_path, server_log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )

        yield server_proc, server_log_path, server_log_fd

        _kill_server_by_pid(
            server_proc.pid, server_log_path, label="composite topology server"
        )
        server_log_fd.close()

    @pytest.fixture()
    def composite_repos(
        self,
        request,
        lore_local_server_config,
        other_region_server_config,
        other_region_server,
        composite_server_config,
        composite_server,
        auto_lore_local_server,
        new_lore_repo,
    ):
        """Creates repos on server 1, the other-region server, and the composite
        server, all sharing the same repo ID."""
        server_hostname = request.config.getoption("--lore-server-hostname")
        (_server_1_root, server_1_env) = lore_local_server_config
        (_other_root, other_env), _other_ports = other_region_server_config
        (_comp_root, comp_env) = composite_server_config

        common_repo_id = uuid.uuid4().hex
        remote_server_1 = f"lore://{server_hostname}:{server_1_env['LORE__SERVER__GRPC__PORT']}/repo-{common_repo_id}"
        remote_other = f"lore://{server_hostname}:{other_env['LORE__SERVER__GRPC__PORT']}/repo-{common_repo_id}"
        remote_composite = f"lore://{server_hostname}:{comp_env['LORE__SERVER__GRPC__PORT']}/repo-{common_repo_id}"

        server_1_repo = new_lore_repo(
            remote_path=remote_server_1, repo_id=common_repo_id
        )
        other_region_repo = new_lore_repo(
            remote_path=remote_other, repo_id=common_repo_id
        )
        composite_repo = new_lore_repo(
            remote_path=remote_composite, repo_id=common_repo_id
        )

        return server_1_repo, other_region_repo, composite_repo

    def test_composite_server_write_replicates_to_same_and_other_region(
        self, composite_repos
    ):
        """Writing to the composite server replicates to both the SameRegion
        peer (server 1) and the OtherRegion peer."""
        server_1_repo, other_region_repo, composite_repo = composite_repos

        with composite_repo.open_file("composite-write.txt", "w+") as file:
            file.writelines(["I replicate everywhere"])
        file_address = (
            "d17c7aa6d8a725a35538fc385ea6c90914e009c41043642d8dcf5e5e6092aca1"
        )

        composite_repo.stage(scan=True)
        composite_repo.commit()
        composite_repo.push()

        logger.info("Checking composite server has the data")
        composite_output = composite_repo.repository_store_immutable_query(
            file_address, json=True
        )
        ensure_fragment_exists_remotely(True, composite_output)

        # give it time to replicate
        time.sleep(0.1)

        logger.info("Checking server 1 (SameRegion) received the replica")
        server_1_output = server_1_repo.repository_store_immutable_query(
            file_address, json=True
        )
        ensure_fragment_exists_remotely(True, server_1_output)

        logger.info("Checking other-region server received the replica")
        other_output = other_region_repo.repository_store_immutable_query(
            file_address, json=True
        )
        ensure_fragment_exists_remotely(True, other_output)
