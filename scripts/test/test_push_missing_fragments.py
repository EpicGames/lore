# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging

import pytest

from grpc_admin import fetch_server_features
from lore_server import (
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)
from lore import Lore

logger = logging.getLogger(__name__)

# Hashes are tied to the byte sequences this test commits below; change them
# together if you change either.
DROPPED_FRAGMENT_HASHES = (
    "9d3f226e7cb165563394906ba32e9fc3a6d0f9721287a9485d1e0533112bdd01,"
    "df80769bd5cdcb27d9073fb6cca35737e0a0d7d4d6306971923efff5b77b717f"
)


@pytest.fixture()
def missing_fragments_remote_url(
    request, tmp_path_factory, lore_server_executable_path
):
    """Yield a remote URL pointing to a dedicated loreserver that silently
    drops writes for the test's known fragment hashes. Runs alongside the
    autouse server on its own allocated ports so a single pytest invocation
    can exercise both normal-server tests and this missing-fragment scenario."""
    shared_port = allocate_free_port()
    ports = {
        "quic": shared_port,
        "grpc": shared_port,
        "http": allocate_free_port(),
        "internal": allocate_free_port(),
    }
    server_root, server_env = generate_server_config(request, tmp_path_factory, ports)
    server_env["LORE_MISS_FRAGMENT_WRITES"] = DROPPED_FRAGMENT_HASHES
    proc, log_path, log_fd = launch_lore_server(
        server_root, server_env, lore_server_executable_path
    )
    # This is a dedicated server we launch locally on OS-allocated ports
    # (allocate_free_port only supports loopback), so the host is always
    # loopback for both the gRPC feature query and the yielded QUIC URL.
    host = "127.0.0.1"
    try:
        # LORE_MISS_FRAGMENT_WRITES is only honored when the server is built with
        # the `failure_generator` cargo feature. Skip cleanly otherwise — without
        # the feature the writes succeed and the test's premise doesn't hold.
        features = fetch_server_features(f"{host}:{ports['grpc']}")
        if "failure_generator" not in features:
            pytest.skip(
                "server not built with the 'failure_generator' feature "
                "(build loreserver with --features failure_generator)"
            )
        yield f"lore://{host}:{ports['quic']}/"
    finally:
        _kill_server_by_pid(proc.pid, log_path, label="missing-fragments server")
        log_fd.close()


@pytest.mark.smoke
def test_push_missing_fragments(new_lore_repo, missing_fragments_remote_url):
    repo: Lore = new_lore_repo(remote_url=missing_fragments_remote_url)
    # Generate some files
    text_file = "text-File.txt"
    other_file = "some_other.uasset"
    large_file = "some_large_file.uasset"

    with repo.open_file(text_file, "w+", encoding="utf-8") as output_file:
        output_file.writelines(["One line\n", "Another line\n", "Third line\n"])

    # Generate a file that fits into a single fragment. Note: we hardcode the
    # content of this file to ensure we have a consistent hash. The hash is used in
    # `scripts/actions/test.sh` to tell the server which fragments to skip writing.
    with repo.open_file(other_file, "w+b") as output_file:
        output_file.write(b"\x00" * (64 * 1024))

    # Generate a file that must be split into multiple fragments. Note: we hardcode
    # the content of this file to ensure we have a consistent hash. The hash is used
    # in `scripts/actions/test.sh` to tell the server which fragments to skip
    # writing.
    with repo.open_file(large_file, "w+b") as output_file:
        output_file.write(b"\xaa" * (1024 * 1024))

    repo.stage(scan=True)

    # Commit the files, the server should be configured to not write to disk for
    # this test, which should result in the subsequent push failing.
    repo.commit()

    # Push main branch
    output = repo.push(check=False).strip()

    assert "Missing fragment" in output, "Push failed for unrelated reason"

    # Create source repository
    repo = new_lore_repo(remote_url=missing_fragments_remote_url)

    # Generate some files
    text_file = "text-File.txt"

    with repo.open_file(text_file, "w+", encoding="utf-8") as output_file:
        output_file.writelines(["One line\n", "Another line\n", "Third line\n"])

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    clone = repo.clone()
    clone.branch_create("test-branch")
    clone.remove_file(text_file)
    clone.stage(text_file)
    clone.commit("Deleted text-File.txt")

    # This reintroduces the file with a different context.
    clone.file_reset(text_file, last_merged_from="main")
    clone.stage(text_file)
    clone.commit("Reverted text-File.txt")
    clone.branch_push()
    clone.branch_merge_into("main", "CHECK IN CHANGES")
