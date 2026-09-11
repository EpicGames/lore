# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Call gRPC methods on a Lore server without generated stubs.

There are no Python protobuf stubs in this repo, so requests are built and
responses read with `protobuf_wire`'s wire-format helpers.

An empty request -- zero bytes for any proto3 message -- distinguishes an absent
route from a mounted handler known not to return UNIMPLEMENTED. UNIMPLEMENTED
alone does not prove that a service is absent because an unfinished handler may
also return it.

Checking response data requires valid requests. The three request types below
expose revision signatures as top-level `bytes` fields, allowing the helpers to
encode them without generated stubs. For example:

    message RevisionInfoRequest {
      oneof query {
        lore.model.v1.RevisionIdentifier identifier = 1;
        bytes signature = 2;
      }
    }
"""

import grpc
from protobuf_wire import encode_bytes_field, parse_fields

THIN_CLIENT_SERVICE = "/lore.thin_client.v1.ThinClientService"
REVISION_INFO = f"{THIN_CLIENT_SERVICE}/RevisionInfo"
REVISION_DIFF = f"{THIN_CLIENT_SERVICE}/RevisionDiff"
REVISION_TREE = f"{THIN_CLIENT_SERVICE}/RevisionTree"
CONTENT_DIFF = f"{THIN_CLIENT_SERVICE}/ContentDiff"

# Registered by default and absent from a thin-client deployment. Used to tell
# which kind of process answered.
STORAGE_QUERY = "/lore.storage.v1.StorageService/Query"


def _identity(payload: bytes) -> bytes:
    """grpc handles the length-prefixed framing; the message body passes through."""
    return payload


# Handlers read the repository from binary request metadata rather than from the
# request body (`lore-transport/src/grpc/mod.rs`, consumed by `get_repository`
# in `lore-server/src/grpc/mod.rs`). Without it every call fails with
# "Missing repository ID".
PARTITION_ID_KEY = "lore-partition-bin"


def repository_metadata(repo_id_hex: str) -> tuple[tuple[str, bytes], ...]:
    """Metadata naming the repository a call applies to."""
    return ((PARTITION_ID_KEY, bytes.fromhex(repo_id_hex)),)


def call(
    target: str,
    method: str,
    request: bytes = b"",
    metadata: tuple[tuple[str, bytes], ...] = (),
    timeout: float = 10.0,
) -> tuple[grpc.StatusCode, bytes, str]:
    """Invoke `method` on `target` and return its status, body and message.

    The body is empty for anything other than a successful call, and the message
    is empty for a successful one. The message is returned alongside so a caller
    can tell transient failures from real ones without a second call.
    """
    with grpc.insecure_channel(target) as channel:
        invoke = channel.unary_unary(method, _identity, _identity)
        try:
            return (
                grpc.StatusCode.OK,
                invoke(request, metadata=metadata, timeout=timeout),
                "",
            )
        except grpc.RpcError as error:
            return error.code(), b"", error.details() or ""


def call_stream(
    target: str,
    method: str,
    request: bytes = b"",
    metadata: tuple[tuple[str, bytes], ...] = (),
    timeout: float = 30.0,
) -> tuple[grpc.StatusCode, list[bytes], str]:
    """Invoke a server-streaming `method` and drain it.

    Returns the terminating status, received messages, and status message. A
    partial failure preserves the messages already received. The status message
    keeps assertion failures diagnosable.
    """
    received: list[bytes] = []
    with grpc.insecure_channel(target) as channel:
        invoke = channel.unary_stream(method, _identity, _identity)
        try:
            for message in invoke(request, metadata=metadata, timeout=timeout):
                received.append(message)
        except grpc.RpcError as error:
            return error.code(), received, error.details() or ""
    return grpc.StatusCode.OK, received, ""


def method_status(target: str, method: str, request: bytes = b"") -> grpc.StatusCode:
    """The status `method` answers on `target`."""
    return call(target, method, request)[0]


def method_details(
    target: str,
    method: str,
    request: bytes = b"",
    metadata: tuple[tuple[str, bytes], ...] = (),
) -> str:
    """The message accompanying `method`'s status, empty when it succeeded.

    Needed to tell a mounted-but-unimplemented handler from an absent service:
    both answer UNIMPLEMENTED, but tonic's fallback for an unrouted path carries
    no message while a handler's `Status::unimplemented` carries its own.
    """
    return call(target, method, request, metadata)[2]


def is_served(target: str, method: str) -> bool:
    """Whether `method` avoids UNIMPLEMENTED for an empty request.

    Use only for methods known not to return UNIMPLEMENTED from their mounted
    handler. For other methods, inspect the status details or send a valid
    request.
    """
    return method_status(target, method) != grpc.StatusCode.UNIMPLEMENTED


def _signature_bytes(signature_hex: str) -> bytes:
    """The 64-character hex signature the CLI reports, as its 32 wire bytes."""
    signature = bytes.fromhex(signature_hex)
    if len(signature) != 32:
        raise ValueError(f"expected a 32-byte signature, got {len(signature)}")
    return signature


def revision_info_request(signature_hex: str) -> bytes:
    """A `RevisionInfoRequest` selecting a revision by signature."""
    return encode_bytes_field(2, _signature_bytes(signature_hex))


def revision_tree_request(signature_hex: str) -> bytes:
    """A `RevisionTreeRequest` selecting a revision by signature.

    `signature` is field 2 here as well, so this is the same shape as
    `RevisionInfoRequest`. `path_prefix` and `max_depth` are left unset, which
    walks the whole tree from the repository root.
    """
    return encode_bytes_field(2, _signature_bytes(signature_hex))


def revision_diff_request(from_signature_hex: str, to_signature_hex: str) -> bytes:
    """A `RevisionDiffRequest` diffing two revisions by signature.

    `signature_from` is field 2 and `signature_to` is field 4. `autoresolve`
    (field 5) is left at its default, which the server ignores outside 3-way mode.
    """
    return encode_bytes_field(2, _signature_bytes(from_signature_hex)) + (
        encode_bytes_field(4, _signature_bytes(to_signature_hex))
    )


def revision_info(
    target: str, repo_id_hex: str, signature_hex: str
) -> tuple[grpc.StatusCode, bytes, str]:
    """Ask `target` for the revision with this signature, in this repository."""
    return call(
        target,
        REVISION_INFO,
        revision_info_request(signature_hex),
        repository_metadata(repo_id_hex),
    )


def revision_info_raw_signature(
    target: str, repo_id_hex: str, signature: bytes
) -> tuple[grpc.StatusCode, bytes, str]:
    """Ask for a revision by a signature of any length, whole or not.

    `revision_info` goes through `_signature_bytes`, which refuses anything but a
    whole signature. This is the way to put a partial one on the wire and read
    what the server answers to it.
    """
    return call(
        target,
        REVISION_INFO,
        encode_bytes_field(2, signature),
        repository_metadata(repo_id_hex),
    )


def revision_tree(
    target: str, repo_id_hex: str, signature_hex: str
) -> tuple[grpc.StatusCode, list[bytes], str]:
    """Walk the tree of a revision, draining the stream."""
    return call_stream(
        target,
        REVISION_TREE,
        revision_tree_request(signature_hex),
        repository_metadata(repo_id_hex),
    )


def revision_diff(
    target: str, repo_id_hex: str, from_signature_hex: str, to_signature_hex: str
) -> tuple[grpc.StatusCode, list[bytes], str]:
    """Diff two revisions, draining the stream."""
    return call_stream(
        target,
        REVISION_DIFF,
        revision_diff_request(from_signature_hex, to_signature_hex),
        repository_metadata(repo_id_hex),
    )


def response_carries_a_revision(body: bytes) -> bool:
    """Whether a `RevisionInfoResponse` contains a `Revision`.

    `Revision revision = 1` is length-delimited. A missing or zero-length field
    does not carry a revision.
    """
    values = parse_fields(body).get(1, [])
    return bool(values and values[-1])
