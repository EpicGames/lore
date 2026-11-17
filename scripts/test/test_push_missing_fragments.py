# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import hashlib
import logging
import os
import uuid

import grpc
import pytest
from error_types import LoreException
from grpc_admin import fetch_server_features
from grpc_probe import STORAGE_QUERY, call, repository_metadata
from lore_server import (
    _kill_server_by_pid,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)
from protobuf_wire import encode_bytes_field, field_packed_ints, parse_fields
from store_layout import index_hashes

from lore import Lore

logger = logging.getLogger(__name__)

# Hashes are tied to the byte sequences this test commits below; change them
# together if you change either.
DROPPED_FRAGMENT_HASHES = (
    "9d3f226e7cb165563394906ba32e9fc3a6d0f9721287a9485d1e0533112bdd01",
    "df80769bd5cdcb27d9073fb6cca35737e0a0d7d4d6306971923efff5b77b717f",
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
    server_env["LORE_MISS_FRAGMENT_WRITES"] = ",".join(DROPPED_FRAGMENT_HASHES)
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
@pytest.mark.requires_instrumented_lore
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

    # The peer carries the address in the status details rather than its message, so
    # what reaches the output is the address the push names alongside it. Both writes
    # above are dropped and the peer stops at the first fragment it finds missing, so
    # which of the two it names is its own business; the push failing for an unrelated
    # reason, or naming neither, is what this catches. The address is reported as
    # `<hash>-<context>`, of which only the hash is fixed by the bytes committed here.
    assert any(
        f"missing fragment {dropped}" in output for dropped in DROPPED_FRAGMENT_HASHES
    ), "Push did not report an injected fragment as missing"

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


# The chunk of `_distinct_content` with the highest hash. A push uploads in hash order, so the
# list of chunks and every other chunk go up before the refused one fails the push. Tied to the
# bytes `_distinct_content` writes; change them together.
REFUSED_CHUNK_HASH = "ec2de87711a81bb44a1a7d72685e494c39e9f66c61105e2e51a80789c4de8452"
# The list of chunks of `_distinct_content`. Tied to its bytes like `REFUSED_CHUNK_HASH`.
LIST_HASH = "c6f1ceb7e5c944cd34fbdc730ada1aba10fe1fb6752d81219ce57f184c66065e"
# The storage query statuses for content held in the asked context, or in another one.
HELD_STATUSES = (0, 1)


def _distinct_content() -> bytes:
    """1 MiB of bytes that split into chunks of distinct content."""
    return b"".join(
        hashlib.sha256(index.to_bytes(4, "little")).digest() for index in range(32768)
    )


@pytest.fixture()
def refusing_server(request, tmp_path_factory, lore_server_executable_path):
    """Yield the remote URL and the gRPC target of a dedicated loreserver that refuses the
    first write of `REFUSED_CHUNK_HASH` and accepts the writes after it."""
    shared_port = allocate_free_port()
    ports = {
        "quic": shared_port,
        "grpc": shared_port,
        "http": allocate_free_port(),
        "internal": allocate_free_port(),
    }
    server_root, server_env = generate_server_config(request, tmp_path_factory, ports)
    server_env["LORE_REFUSE_FRAGMENT_WRITES"] = REFUSED_CHUNK_HASH
    proc, log_path, log_fd = launch_lore_server(
        server_root, server_env, lore_server_executable_path
    )
    host = "127.0.0.1"
    try:
        # LORE_REFUSE_FRAGMENT_WRITES is only honored when the server is built with
        # the `failure_generator` cargo feature.
        features = fetch_server_features(f"{host}:{ports['grpc']}")
        if "failure_generator" not in features:
            pytest.skip(
                "server not built with the 'failure_generator' feature "
                "(build loreserver with --features failure_generator)"
            )
        yield f"lore://{host}:{ports['quic']}/", f"{host}:{ports['grpc']}"
    finally:
        _kill_server_by_pid(proc.pid, log_path, label="refusing server")
        log_fd.close()


def _server_holds(grpc_target: str, repo_id: str, hash_hex: str) -> bool:
    """Whether the server holds content with `hash_hex` in the repository, in any context."""
    address = encode_bytes_field(1, bytes.fromhex(hash_hex)) + encode_bytes_field(
        2, bytes(16)
    )
    status, body, message = call(
        grpc_target,
        STORAGE_QUERY,
        encode_bytes_field(1, address),
        repository_metadata(repo_id),
    )
    assert status == grpc.StatusCode.OK, message
    [state] = field_packed_ints(parse_fields(body), 1)
    return state in HELD_STATUSES


@pytest.mark.smoke
@pytest.mark.requires_instrumented_lore
def test_push_after_a_failed_push_uploads_what_an_evicted_list_names(
    new_lore_repo, refusing_server
):
    """A push that fails part way leaves the server holding the list of a file's chunks but
    not every chunk, and the list durable in the local store, where eviction can remove its
    entry. The next push offers the list alone, the server refuses the branch push for the
    missing chunk, and the push collects again through the server, finds the chunks the list
    names, uploads the one the server lacks and offers the branch push again."""
    remote_url, grpc_target = refusing_server
    repo_id = uuid.uuid4().hex
    repo: Lore = new_lore_repo(remote_url=remote_url, repo_id=repo_id)
    content = _distinct_content()
    with repo.open_file("large.uasset", "w+b") as output_file:
        output_file.write(content)
    repo.stage(scan=True)
    repo.commit("Large file", local=True)

    with pytest.raises(LoreException):
        repo.push()
    assert _server_holds(grpc_target, repo_id, LIST_HASH), "the list of chunks went up"
    assert not _server_holds(grpc_target, repo_id, REFUSED_CHUNK_HASH), (
        "the refused chunk did not"
    )

    local_store = os.path.join(repo.dot_path(), "immutable")
    assert bytes.fromhex(LIST_HASH) in index_hashes(local_store)
    repo.set_store_caps(max_size="1", max_capacity="1")
    repo.repository_gc()
    assert bytes.fromhex(LIST_HASH) not in index_hashes(local_store), (
        "gc evicted the list"
    )
    repo.push()

    assert _server_holds(grpc_target, repo_id, REFUSED_CHUNK_HASH)
    assert "in sync with remote" in repo.status()
    clone = repo.clone()
    with clone.open_file("large.uasset", "rb") as cloned:
        assert cloned.read() == content
