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
    field_message,
    field_string,
    parse_fields,
)


def _encode_bool_field(field_number: int, value: bool) -> bytes:
    """Encode one `bool` field. Proto3 default is False, which is not
    serialised - callers should only emit this when the value is True."""
    tag = field_number << 3 | 0  # wire type 0 = VARINT
    return bytes([tag, 1 if value else 0])


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

# RevisionTreeResponse.payload oneof
_TREE_RESPONSE_NODE = 2
_TREE_RESPONSE_REVISION = 3

_DIFF_RESPONSE_CHANGE = 2
_DIFF_RESPONSE_PARTITION = 4

_TREE_NODE_PATH = 1
_TREE_NODE_NODE_TYPE = 2
_TREE_NODE_TRACKING = 6
_TREE_NODE_LAST_REVISION_INDEX = 7

# TreeRevision
_TREE_REVISION_INDEX = 1
_TREE_REVISION_REVISION = 2

# lore.thin_client.v1.Revision (attribution-facing subset; the walker
# populates only these, leaving created_by / metadata / both parents
# unset — see the proto doc on TreeRevision.revision).
_REVISION_SIGNATURE = 1
_REVISION_IDENTIFIER = 2
_REVISION_COMMIT_MESSAGE = 3
_REVISION_TIMESTAMP = 4
_REVISION_COMMITTED_BY = 6
_REVISION_NUMBER = 10

# lore.model.v1.RevisionIdentifier
_REVISION_IDENTIFIER_BRANCH_ID = 1
_REVISION_IDENTIFIER_NUMBER = 2

_DIFF_CHANGE_PATH = 1
_DIFF_CHANGE_ACTION = 3
_DIFF_CHANGE_NODE_TYPE = 4
_DIFF_CHANGE_LINK_REPOSITORY_INDEX = 8
_DIFF_CHANGE_TRACKING = 9

_DIFF_PARTITION_INDEX = 1
_DIFF_PARTITION_LINK_PARTITION = 2


@dataclass(frozen=True)
class TreeRevision:
    """One `lore.thin_client.v1.TreeRevision` off a `RevisionTree` stream.

    Attribution-facing subset of `lore.thin_client.v1.Revision`; the fields
    the walker deliberately leaves unset (`created_by`, `metadata`,
    `parent_self`, `parent_other`) are absent by proto3 rules and read
    back as their type's default via `_revision`.
    """

    index: int
    signature: bytes
    commit_message: str
    timestamp: int
    branch_id: bytes
    number: int
    committed_by: str


@dataclass(frozen=True)
class TreeNode:
    """One `lore.thin_client.v1.TreeNode` off a `RevisionTree` stream.

    `last_revision` resolves `TreeNode.last_revision_index` against the
    `TreeRevision` payloads the stream announced ahead of this node. Absent
    when the request did not ask for attribution, or when the walk could
    not attribute this entry (root, or an entry the resolver skipped).
    """

    path: str
    node_type: int
    tracking: bool
    last_revision: TreeRevision | None


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


class _RevisionTable:
    """Accumulates `TreeRevision` payloads for a RevisionTree stream and
    resolves `TreeNode.last_revision_index` against them.

    The proto contract on `TreeRevision` says any index a TreeNode names
    must have been announced strictly earlier on the same stream (mirrors
    the DiffPartition convention). `resolve()` returns None for an unset
    or zero index; a non-zero index that has not been announced is a wire
    contract violation and asserts.
    """

    def __init__(self) -> None:
        self._by_index: dict[int, TreeRevision] = {}

    def announce(self, payload: dict) -> None:
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
            signature=field_bytes(revision, _REVISION_SIGNATURE),
            commit_message=field_string(revision, _REVISION_COMMIT_MESSAGE),
            timestamp=field_int(revision, _REVISION_TIMESTAMP),
            branch_id=field_bytes(identifier, _REVISION_IDENTIFIER_BRANCH_ID),
            number=field_int(identifier, _REVISION_IDENTIFIER_NUMBER),
            committed_by=field_string(revision, _REVISION_COMMITTED_BY),
        )

    def resolve(self, index: int) -> TreeRevision | None:
        """None when `index` is 0 (proto3 default / unset optional). A
        non-zero index that has not been announced is a wire violation and
        asserts."""
        if index == 0:
            return None
        revision = self._by_index.get(index)
        assert revision is not None, (
            f"TreeNode.last_revision_index={index} was not announced "
            f"before this node (announced: {sorted(self._by_index)})"
        )
        return revision

    def announced_indices(self) -> list[int]:
        """Every index announced so far, in insertion (= wire) order."""
        return list(self._by_index)


def _payloads(response: bytes, payload_field: int) -> list[dict]:
    """The parsed sub-messages one response message carries under
    `payload_field`. Empty for a message whose oneof holds another payload —
    the stream leads with a header, and a diff also announces partitions."""
    return [
        parse_fields(raw)
        for raw in parse_fields(response).get(payload_field, [])
        if isinstance(raw, bytes)
    ]


def _tree_items(response: bytes, revisions: _RevisionTable) -> list[TreeNode]:
    """Consume `TreeRevision` payloads for the ordering side-effect, then
    return this response's `TreeNode` payloads. Interleaving is preserved
    across messages: the caller receives nodes as they arrive, but a node
    referencing an index announced earlier in the same or a prior message
    resolves correctly because `revisions` is accumulated across the
    stream."""
    for payload in _payloads(response, _TREE_RESPONSE_REVISION):
        revisions.announce(payload)
    return [
        TreeNode(
            path=field_string(node, _TREE_NODE_PATH),
            node_type=field_int(node, _TREE_NODE_NODE_TYPE),
            tracking=field_bool(node, _TREE_NODE_TRACKING),
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


@dataclass(frozen=True)
class RevisionTreeResult:
    """A RevisionTree walk's wire output, ready for assertions.

    `nodes` is every `TreeNode` in stream order. `revisions` is every
    unique `TreeRevision` the server announced, in the order it was
    announced — the sequence a wire-ordering assertion inspects. `nodes`
    already carry their resolved `TreeRevision` via `TreeNode.last_revision`;
    the two are consistent because both are populated as the stream
    interleaves them.

    Iterating the result yields nodes, matching the pre-attribution shape
    (`for node in revision_tree(...)`) so existing callers stay working.
    Tests that also want the announced revisions read `.revisions`, and
    a length-of-result check reads `.nodes` explicitly.
    """

    nodes: list[TreeNode]
    revisions: list[TreeRevision]

    def __iter__(self):
        return iter(self.nodes)

    def __len__(self) -> int:
        return len(self.nodes)


def revision_tree(
    grpc_target: str,
    repository_id: bytes,
    signature: bytes,
    timeout: float = 30.0,
    include_last_revision: bool = False,
) -> RevisionTreeResult:
    """Every `TreeNode` the server streams for `signature`, in stream order,
    together with the `TreeRevision` payloads it announced.

    Setting `include_last_revision` opts in to attribution: `TreeNode.last_revision`
    resolves to a `TreeRevision` for entries the walker can attribute (files,
    links, and directories that live in the walked repository, plus everything
    under a link that lives in the linked repository)."""
    request = encode_bytes_field(_TREE_REQUEST_SIGNATURE, signature)
    # Proto3 default for bool is False, so only serialise when True.
    if include_last_revision:
        request += _encode_bool_field(_TREE_REQUEST_INCLUDE_LAST_REVISION, True)
    revisions = _RevisionTable()
    nodes = _collect_stream(
        grpc_target,
        _REVISION_TREE_METHOD,
        request,
        repository_id,
        lambda response: _tree_items(response, revisions),
        timeout,
    )
    announced = [revisions.resolve(i) for i in revisions.announced_indices()]
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
