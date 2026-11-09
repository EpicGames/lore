# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Minimal gRPC client for `lore.thin_client.v1.ThinClientService`.

The CLI never calls this service, so a test asserting what reaches its wire has
to talk to it directly. The test server runs gRPC in plaintext, so an insecure
channel carrying the repository-id metadata works; against an auth-enabled
server the caller also passes the bearer token. Only the fields the tests assert
on are decoded.
"""

import logging
from dataclasses import dataclass

import grpc
from protobuf_wire import (
    encode_bytes_field,
    encode_varint_field,
    field_bool,
    field_bytes,
    field_int,
    field_message,
    field_string,
    parse_fields,
)

from lore import Lore

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
_TREE_REQUEST_INCLUDE_LAST_REVISION = 5
_DIFF_REQUEST_SIGNATURE_FROM = 2
_DIFF_REQUEST_SIGNATURE_TO = 4

_TREE_RESPONSE_NODE = 2
_TREE_RESPONSE_REVISION = 3
_TREE_RESPONSE_PARTITION = 4

_DIFF_RESPONSE_CHANGE = 2
_DIFF_RESPONSE_PARTITION = 4

_TREE_NODE_PATH = 1
_TREE_NODE_NODE_TYPE = 2
_TREE_NODE_ADDRESS = 3
_TREE_NODE_TRACKING = 6
_TREE_NODE_LAST_REVISION_INDEX = 7
_TREE_NODE_LINK_REPOSITORY_INDEX = 8

_TREE_REVISION_INDEX = 1
_TREE_REVISION_REVISION = 2
_TREE_REVISION_LINK_REPOSITORY_INDEX = 3

_REVISION_SIGNATURE = 1
_REVISION_IDENTIFIER = 2
_REVISION_COMMIT_MESSAGE = 3
_REVISION_TIMESTAMP = 4
_REVISION_COMMITTED_BY = 6

_REVISION_IDENTIFIER_BRANCH_ID = 1
_REVISION_IDENTIFIER_NUMBER = 2

_DIFF_CHANGE_PATH = 1
_DIFF_CHANGE_ACTION = 3
_DIFF_CHANGE_NODE_TYPE = 4
_DIFF_CHANGE_LINK_REPOSITORY_INDEX = 8
_DIFF_CHANGE_TRACKING = 9
_DIFF_CHANGE_CONTENT_FROM = 10
_DIFF_CHANGE_CONTENT_TO = 11

_DIFF_PARTITION_INDEX = 1
_DIFF_PARTITION_LINK_PARTITION = 2

_ADDRESS_HASH = 1
_ADDRESS_CONTEXT = 2


@dataclass(frozen=True)
class ContentAddress:
    """A decoded `lore.model.v1.Address`, both halves hex-encoded."""

    hash: str
    context: str


@dataclass(frozen=True)
class TreeRevision:
    """One `lore.thin_client.v1.TreeRevision` off a `RevisionTree` stream.

    Attribution-facing subset of `lore.thin_client.v1.Revision`; the server
    leaves `created_by`, `metadata`, `parent_self` and `parent_other` unset.
    `partition` is the hex repository id the revision is in, resolved from
    `TreeRevision.link_repository_index` as `_PartitionTable` resolves it.
    """

    index: int
    partition: str
    signature: bytes
    commit_message: str
    timestamp: int
    branch_id: bytes
    number: int
    committed_by: str


@dataclass(frozen=True)
class TreeNode:
    """One `lore.thin_client.v1.TreeNode` off a `RevisionTree` stream.

    `partition` is the hex repository id the entry's content is in, the
    linked repository for a link, resolved from `TreeNode.link_repository_index`
    as `_PartitionTable` resolves it.
    `last_revision` resolves `TreeNode.last_revision_index` against the
    `TreeRevision` payloads the stream announced ahead of this node. Absent
    when the request did not ask for attribution, or when the walk could
    not attribute this entry (no history record, or its revision failed to load).
    """

    path: str
    node_type: int
    tracking: bool
    partition: str
    address: ContentAddress | None
    last_revision: TreeRevision | None


@dataclass(frozen=True)
class DiffChange:
    """One `lore.thin_client.v1.DiffChange` off a `RevisionDiff` stream."""

    path: str
    action: int
    node_type: int
    tracking: bool
    partition: str
    content_from: ContentAddress | None
    content_to: ContentAddress | None


class _PartitionTable:
    """Resolves the `link_repository_index` of a `DiffChange`, `TreeNode` or
    `TreeRevision` to the hex repository id it names, from the
    `DiffPartition` payloads the stream announces ahead of it. Index 0 is the
    request's own repository and is never announced."""

    def __init__(self, repository_id: bytes):
        self._by_index = {0: repository_id.hex()}

    def announce(self, partition: dict) -> None:
        index = field_int(partition, _DIFF_PARTITION_INDEX)
        raw = field_bytes(partition, _DIFF_PARTITION_LINK_PARTITION)
        self._by_index[index] = raw.hex()

    def resolve(self, index: int) -> str:
        """The repository `index` names. An index that was never announced is
        a wire violation and asserts."""
        repository = self._by_index.get(index)
        assert repository is not None, (
            f"link_repository_index={index} was not announced before this "
            f"item (announced: {sorted(self._by_index)})"
        )
        return repository

    def announced(self) -> list[str]:
        return [repository for index, repository in self._by_index.items() if index]


class _RevisionTable:
    """Accumulates `TreeRevision` payloads for a RevisionTree stream and
    resolves `TreeNode.last_revision_index` against them.

    Enforces the ordering rule `TreeRevision` documents. An index a node
    names must already have been announced on the same stream.
    """

    def __init__(self) -> None:
        self._by_index: dict[int, TreeRevision] = {}

    def announce(self, payload: dict, partitions: _PartitionTable) -> None:
        index = field_int(payload, _TREE_REVISION_INDEX)
        assert index >= 1, (
            f"TreeRevision.index must be >= 1 (0 is reserved); got {index}"
        )
        assert index not in self._by_index, (
            f"TreeRevision index {index} announced twice on the same stream"
        )
        revision = field_message(payload, _TREE_REVISION_REVISION) or {}
        identifier = field_message(revision, _REVISION_IDENTIFIER) or {}
        self._by_index[index] = TreeRevision(
            index=index,
            partition=partitions.resolve(
                field_int(payload, _TREE_REVISION_LINK_REPOSITORY_INDEX)
            ),
            signature=field_bytes(revision, _REVISION_SIGNATURE),
            commit_message=field_string(revision, _REVISION_COMMIT_MESSAGE),
            timestamp=field_int(revision, _REVISION_TIMESTAMP),
            branch_id=field_bytes(identifier, _REVISION_IDENTIFIER_BRANCH_ID),
            number=field_int(identifier, _REVISION_IDENTIFIER_NUMBER),
            committed_by=field_string(revision, _REVISION_COMMITTED_BY),
        )

    def resolve(self, index: int) -> TreeRevision | None:
        """None when `index` is 0, which names no revision. A non-zero index
        that was never announced is a wire violation and asserts."""
        if index == 0:
            return None
        revision = self._by_index.get(index)
        assert revision is not None, (
            f"TreeNode.last_revision_index={index} was not announced "
            f"before this node (announced: {sorted(self._by_index)})"
        )
        return revision

    def announced(self) -> list[TreeRevision]:
        """Every revision announced so far, in wire order."""
        return list(self._by_index.values())


def _payloads(response: bytes, payload_field: int) -> list[dict]:
    """The parsed sub-messages one response message carries under
    `payload_field`. Empty for a message whose oneof holds another payload —
    the stream leads with a header, and a diff also announces partitions."""
    return [
        parse_fields(raw)
        for raw in parse_fields(response).get(payload_field, [])
        if isinstance(raw, bytes)
    ]


def _content_address(message: dict, field_number: int) -> ContentAddress | None:
    address = field_message(message, field_number)
    if address is None:
        return None
    return ContentAddress(
        hash=field_bytes(address, _ADDRESS_HASH).hex(),
        context=field_bytes(address, _ADDRESS_CONTEXT).hex(),
    )


def _tree_items(
    response: bytes, revisions: _RevisionTable, partitions: _PartitionTable
) -> list[TreeNode]:
    """Announce this response's `DiffPartition` or `TreeRevision` payload in
    `partitions` or `revisions`, which accumulate across the stream, and return
    its `TreeNode` payloads resolved against them."""
    for partition in _payloads(response, _TREE_RESPONSE_PARTITION):
        partitions.announce(partition)
    for payload in _payloads(response, _TREE_RESPONSE_REVISION):
        revisions.announce(payload, partitions)
    return [
        TreeNode(
            path=field_string(node, _TREE_NODE_PATH),
            node_type=field_int(node, _TREE_NODE_NODE_TYPE),
            tracking=field_bool(node, _TREE_NODE_TRACKING),
            partition=partitions.resolve(
                field_int(node, _TREE_NODE_LINK_REPOSITORY_INDEX)
            ),
            address=_content_address(node, _TREE_NODE_ADDRESS),
            last_revision=revisions.resolve(
                field_int(node, _TREE_NODE_LAST_REVISION_INDEX)
            ),
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
            content_from=_content_address(change, _DIFF_CHANGE_CONTENT_FROM),
            content_to=_content_address(change, _DIFF_CHANGE_CONTENT_TO),
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
    authorization: str | None,
) -> list:
    metadata: tuple[tuple[str, str | bytes], ...] = (
        (_REPOSITORY_ID_METADATA_KEY, repository_id),
    )
    if authorization is not None:
        metadata += (("authorization", f"Bearer {authorization}"),)
    with grpc.insecure_channel(grpc_target) as channel:
        call = channel.unary_stream(
            method,
            request_serializer=_already_encoded,
            response_deserializer=deserializer,
        )
        responses = call(request, timeout=timeout, metadata=metadata)
        return [item for message in responses for item in message]


@dataclass(frozen=True)
class RevisionTreeResult:
    """A RevisionTree walk's wire output, ready for assertions.

    `nodes` is every `TreeNode` in stream order. `revisions` is every unique
    `TreeRevision` the server announced, in announcement order, which is what
    a wire-ordering assertion inspects.
    """

    nodes: list[TreeNode]
    revisions: list[TreeRevision]


def revision_tree(
    grpc_target: str,
    repository_id: bytes,
    signature: bytes,
    timeout: float = 30.0,
    authorization: str | None = None,
    include_last_revision: bool = False,
) -> RevisionTreeResult:
    """Every `TreeNode` the server streams for `signature`, in stream order,
    and the `TreeRevision` payloads it announced. `authorization` is the bearer
    token for an auth-enabled server. With `include_last_revision`, each node's
    `last_revision` is the revision the server attributes the entry to."""
    request = encode_bytes_field(_TREE_REQUEST_SIGNATURE, signature)
    if include_last_revision:
        request += encode_varint_field(_TREE_REQUEST_INCLUDE_LAST_REVISION, 1)
    revisions = _RevisionTable()
    partitions = _PartitionTable(repository_id)
    nodes = _collect_stream(
        grpc_target,
        _REVISION_TREE_METHOD,
        request,
        repository_id,
        lambda response: _tree_items(response, revisions, partitions),
        timeout,
        authorization,
    )
    announced = revisions.announced()
    logger.info(
        "RevisionTree(%s) returned %d nodes, %d revisions",
        signature.hex(),
        len(nodes),
        len(announced),
    )
    return RevisionTreeResult(nodes=nodes, revisions=announced)


def revision_diff(
    grpc_target: str,
    repository_id: bytes,
    signature_from: bytes,
    signature_to: bytes,
    timeout: float = 30.0,
    authorization: str | None = None,
) -> list[DiffChange]:
    """Every `DiffChange` the server streams between the two revisions, in
    stream order. `authorization` is the bearer token for an auth-enabled
    server."""
    changes, _ = revision_diff_with_partitions(
        grpc_target,
        repository_id,
        signature_from,
        signature_to,
        timeout,
        authorization,
    )
    return changes


def revision_diff_with_partitions(
    grpc_target: str,
    repository_id: bytes,
    signature_from: bytes,
    signature_to: bytes,
    timeout: float = 30.0,
    authorization: str | None = None,
) -> tuple[list[DiffChange], list[str]]:
    """`revision_diff`, with the hex repository ids the stream announces as
    partitions, in announcement order."""
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
        authorization,
    )
    logger.info(
        "RevisionDiff(%s -> %s) returned %d changes",
        signature_from.hex(),
        signature_to.hex(),
        len(changes),
    )
    return changes, partitions.announced()


def wire_identity(repo: Lore) -> tuple[bytes, bytes]:
    """The repository id and latest revision signature of `repo` as the raw
    bytes the thin-client wire expects."""
    latest = repo.branch_info().local_latest
    assert len(latest) == 64, f"Expected a full revision signature, got {latest!r}"
    return bytes.fromhex(repo.get_id()), bytes.fromhex(latest)
