# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Minimal gRPC client for `lore.thin_client.v1.ThinClientService`.

The CLI never calls this service, so a test asserting what reaches its wire has
to talk to it directly. The test server runs gRPC in plaintext and registers the
service without an auth interceptor, so an insecure channel carrying only the
repository-id metadata works. Only the fields the tests assert on are decoded.
"""

import logging
from dataclasses import dataclass

import grpc
from protobuf_wire import (
    encode_bytes_field,
    field_bool,
    field_bytes,
    field_int,
    field_string,
    parse_fields,
)

logger = logging.getLogger(__name__)

_REVISION_TREE_METHOD = "/lore.thin_client.v1.ThinClientService/RevisionTree"
_REVISION_DIFF_METHOD = "/lore.thin_client.v1.ThinClientService/RevisionDiff"
_REPOSITORY_ID_METADATA_KEY = "urc-repository-id-bin"

# lore.thin_client.v1.NodeType
NODE_TYPE_DIRECTORY = 0
NODE_TYPE_FILE = 1
NODE_TYPE_LINK = 2

# lore.thin_client.v1.Action
ACTION_KEEP = 0
ACTION_ADD = 1
ACTION_DELETE = 2

_TREE_REQUEST_SIGNATURE = 2
_DIFF_REQUEST_SIGNATURE_FROM = 2
_DIFF_REQUEST_SIGNATURE_TO = 4

_TREE_RESPONSE_NODE = 2
_DIFF_RESPONSE_CHANGE = 2
_DIFF_RESPONSE_PARTITION = 4

_TREE_NODE_PATH = 1
_TREE_NODE_NODE_TYPE = 2
_TREE_NODE_TRACKING = 6

_DIFF_CHANGE_PATH = 1
_DIFF_CHANGE_ACTION = 3
_DIFF_CHANGE_NODE_TYPE = 4
_DIFF_CHANGE_LINK_REPOSITORY_INDEX = 8
_DIFF_CHANGE_TRACKING = 9

_DIFF_PARTITION_INDEX = 1
_DIFF_PARTITION_LINK_PARTITION = 2


@dataclass(frozen=True)
class TreeNode:
    """One `lore.thin_client.v1.TreeNode` off a `RevisionTree` stream."""

    path: str
    node_type: int
    tracking: bool


@dataclass(frozen=True)
class DiffChange:
    """One `lore.thin_client.v1.DiffChange` off a `RevisionDiff` stream."""

    path: str
    action: int
    node_type: int
    tracking: bool
    partition: str


class _PartitionTable:
    """Resolves a `DiffChange.link_repository_index` to the hex repository id its
    content lives in, from the `DiffPartition` payloads the stream announces
    ahead of it. Index 0 is the request's own repository and is never
    announced."""

    def __init__(self, repository_id: bytes):
        self._by_index = {0: repository_id.hex()}

    def announce(self, partition: dict) -> None:
        index = field_int(partition, _DIFF_PARTITION_INDEX)
        raw = field_bytes(partition, _DIFF_PARTITION_LINK_PARTITION)
        self._by_index[index] = raw.hex()

    def resolve(self, index: int) -> str:
        return self._by_index.get(index, f"unannounced-index-{index}")


def _payloads(response: bytes, payload_field: int) -> list[dict]:
    """The parsed sub-messages one response message carries under
    `payload_field`. Empty for a message whose oneof holds another payload —
    the stream leads with a header, and a diff also announces partitions."""
    return [
        parse_fields(raw)
        for raw in parse_fields(response).get(payload_field, [])
        if isinstance(raw, bytes)
    ]


def _tree_nodes(response: bytes) -> list[TreeNode]:
    return [
        TreeNode(
            path=field_string(node, _TREE_NODE_PATH),
            node_type=field_int(node, _TREE_NODE_NODE_TYPE),
            tracking=field_bool(node, _TREE_NODE_TRACKING),
        )
        for node in _payloads(response, _TREE_RESPONSE_NODE)
    ]


def _diff_changes(response: bytes, partitions: _PartitionTable) -> list[DiffChange]:
    for partition in _payloads(response, _DIFF_RESPONSE_PARTITION):
        partitions.announce(partition)
    return [
        DiffChange(
            path=field_string(change, _DIFF_CHANGE_PATH),
            action=field_int(change, _DIFF_CHANGE_ACTION),
            node_type=field_int(change, _DIFF_CHANGE_NODE_TYPE),
            tracking=field_bool(change, _DIFF_CHANGE_TRACKING),
            partition=partitions.resolve(
                field_int(change, _DIFF_CHANGE_LINK_REPOSITORY_INDEX)
            ),
        )
        for change in _payloads(response, _DIFF_RESPONSE_CHANGE)
    ]


def _already_encoded(request: bytes) -> bytes:
    """The request is built as wire bytes, but gRPC still wants a serializer."""
    return request


def _collect_stream(
    grpc_target: str,
    method: str,
    request: bytes,
    repository_id: bytes,
    deserializer,
    timeout: float,
) -> list:
    with grpc.insecure_channel(grpc_target) as channel:
        call = channel.unary_stream(
            method,
            request_serializer=_already_encoded,
            response_deserializer=deserializer,
        )
        responses = call(
            request,
            timeout=timeout,
            metadata=((_REPOSITORY_ID_METADATA_KEY, repository_id),),
        )
        return [item for message in responses for item in message]


def revision_tree(
    grpc_target: str,
    repository_id: bytes,
    signature: bytes,
    timeout: float = 30.0,
) -> list[TreeNode]:
    """Every `TreeNode` the server streams for `signature`, in stream order."""
    nodes = _collect_stream(
        grpc_target,
        _REVISION_TREE_METHOD,
        encode_bytes_field(_TREE_REQUEST_SIGNATURE, signature),
        repository_id,
        _tree_nodes,
        timeout,
    )
    logger.info("RevisionTree(%s) returned %d nodes", signature.hex(), len(nodes))
    return nodes


def revision_diff(
    grpc_target: str,
    repository_id: bytes,
    signature_from: bytes,
    signature_to: bytes,
    timeout: float = 30.0,
) -> list[DiffChange]:
    """Every `DiffChange` the server streams between the two revisions, in
    stream order."""
    request = encode_bytes_field(
        _DIFF_REQUEST_SIGNATURE_FROM, signature_from
    ) + encode_bytes_field(_DIFF_REQUEST_SIGNATURE_TO, signature_to)
    partitions = _PartitionTable(repository_id)
    changes = _collect_stream(
        grpc_target,
        _REVISION_DIFF_METHOD,
        request,
        repository_id,
        lambda response: _diff_changes(response, partitions),
        timeout,
    )
    logger.info(
        "RevisionDiff(%s -> %s) returned %d changes",
        signature_from.hex(),
        signature_to.hex(),
        len(changes),
    )
    return changes
