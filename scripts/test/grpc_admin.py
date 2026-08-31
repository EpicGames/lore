# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Minimal gRPC client for the server's AdminService.ServerInfo RPC.

The server reports the cargo features it was compiled with via
`ServerInfoResponse.features`. Tests that depend on an optional feature (for
example `failure_generator`, which enables fault injection) use this to skip
cleanly when run against a server that wasn't built with the feature.

We avoid generated protobuf stubs: `ServerInfoRequest` is empty (zero bytes on
the wire) and we only need the repeated-string `features` field (field 2), so we
serialize an empty request and read that one field back with `protobuf_wire`.
The gRPC AdminService is registered without an auth interceptor and the test
server runs gRPC in plaintext, so an insecure channel with no metadata works.
"""

import logging

import grpc
from protobuf_wire import field_strings, parse_fields

logger = logging.getLogger(__name__)

_SERVER_INFO_METHOD = "/urc.rpc.AdminService/ServerInfo"
_FEATURES_FIELD = 2


def _parse_features(response: bytes) -> list[str]:
    """Extract repeated-string field 2 (`features`) from a ServerInfoResponse,
    skipping every other field so we stay forward-compatible with new fields."""
    return field_strings(parse_fields(response), _FEATURES_FIELD)


def fetch_server_features(grpc_target: str, timeout: float = 10.0) -> set[str]:
    """Call AdminService.ServerInfo on `grpc_target` (host:port) and return the
    set of cargo features the server binary was compiled with."""
    with grpc.insecure_channel(grpc_target) as channel:
        call = channel.unary_unary(
            _SERVER_INFO_METHOD,
            request_serializer=lambda _request: b"",
            response_deserializer=_parse_features,
        )
        features = set(call(None, timeout=timeout))
    logger.info("Server %s reports compiled features: %s", grpc_target, sorted(features))
    return features
