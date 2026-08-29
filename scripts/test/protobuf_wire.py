# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Protobuf wire-format helpers for tests that speak gRPC directly.

The test environment ships `grpcio` but no protobuf runtime and no generated
message stubs, so a test needing an RPC the CLI does not expose encodes it by
hand. These helpers cover the subset those tests use: varints, length-delimited
fields, and a generic field walk that keeps unknown fields instead of rejecting
them, so callers stay forward-compatible as messages gain fields.
"""

_WIRE_VARINT = 0
_WIRE_64BIT = 1
_WIRE_LENGTH_DELIMITED = 2
_WIRE_32BIT = 5

Fields = dict[int, list[int | bytes]]


def _read_varint(data: bytes, pos: int) -> tuple[int, int]:
    """Decode a base-128 varint at `pos`; return (value, next_pos)."""
    result = 0
    shift = 0
    while True:
        byte = data[pos]
        pos += 1
        result |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return result, pos
        shift += 7


def _encode_varint(value: int) -> bytes:
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if not value:
            out.append(byte)
            return bytes(out)
        out.append(byte | 0x80)


def encode_bytes_field(field_number: int, value: bytes) -> bytes:
    """Encode one length-delimited field (bytes, string or embedded message)."""
    return (
        _encode_varint(field_number << 3 | _WIRE_LENGTH_DELIMITED)
        + _encode_varint(len(value))
        + value
    )


def parse_fields(message: bytes) -> Fields:
    """Split a message into `{field_number: [values]}`, repeated fields in wire
    order. Varint and fixed-width fields decode to ints, length-delimited fields
    to raw bytes."""
    fields: Fields = {}
    pos = 0
    while pos < len(message):
        tag, pos = _read_varint(message, pos)
        field_number = tag >> 3
        wire_type = tag & 0x07
        value: int | bytes
        if wire_type == _WIRE_VARINT:
            value, pos = _read_varint(message, pos)
        elif wire_type == _WIRE_64BIT:
            value = int.from_bytes(message[pos : pos + 8], "little")
            pos += 8
        elif wire_type == _WIRE_LENGTH_DELIMITED:
            length, pos = _read_varint(message, pos)
            value = message[pos : pos + length]
            pos += length
        elif wire_type == _WIRE_32BIT:
            value = int.from_bytes(message[pos : pos + 4], "little")
            pos += 4
        else:
            raise ValueError(f"Unsupported protobuf wire type {wire_type}")
        fields.setdefault(field_number, []).append(value)
    return fields


def field_int(fields: Fields, field_number: int) -> int:
    """Value of a varint field. A proto3 field carrying its type's default is
    not put on the wire, so an absent field reads back as 0."""
    values = fields.get(field_number)
    if not values:
        return 0
    value = values[-1]
    if not isinstance(value, int):
        raise TypeError(f"Field {field_number} is length-delimited, not a varint")
    return value


def field_bool(fields: Fields, field_number: int) -> bool:
    """Value of a `bool` field; absent reads back as False."""
    return field_int(fields, field_number) != 0


def field_bytes(fields: Fields, field_number: int) -> bytes:
    """Value of a `bytes` field; absent reads back as empty."""
    values = fields.get(field_number)
    if not values:
        return b""
    value = values[-1]
    if not isinstance(value, bytes):
        raise TypeError(f"Field {field_number} is a varint, not bytes")
    return value


def field_string(fields: Fields, field_number: int) -> str:
    """Value of a `string` field; absent reads back as the empty string."""
    strings = field_strings(fields, field_number)
    return strings[-1] if strings else ""


def field_strings(fields: Fields, field_number: int) -> list[str]:
    """Values of a `repeated string` field, in wire order."""
    values = fields.get(field_number, [])
    decoded = []
    for value in values:
        if not isinstance(value, bytes):
            raise TypeError(f"Field {field_number} is a varint, not a string")
        decoded.append(value.decode("utf-8"))
    return decoded
