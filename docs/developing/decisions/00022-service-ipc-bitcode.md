---
status: accepted
date: 2026-10-09
deciders: Mattias Jansson
---

# ADR-00022: Bitcode as the only encoding between a client and the Lore service

## Context and Problem Statement

A process using the service relays each C API call to it over a local socket: the call's globals
and arguments go to the service, and the call's events and status come back. Each message named
its encoding in its header, JSON or bitcode through its serde integration, and the client always
sent JSON. The arguments derived serde, and kept JSON-only attributes so that a client of an older
build could talk to a newer service.

JSON could not carry every call. `LoreBytes` could not be read back, so a `lore_storage_put` and a
`GET_DATA` event failed to relay. Events could not cross in bitcode at all: `LoreEvent` is
adjacently tagged, and bitcode's serde integration cannot read that. Every argument struct also
carried serde code for both encodings, compiled into every build that links the library.

## Decision Drivers

- Every C API call relays, whatever its arguments carry.
- Encoding and decoding a call cost as little time and as few allocations as possible.
- The argument structs carry no code for an encoding nothing uses.

## Considered Options

- Bitcode through its native derive, for the arguments and the events
- Bitcode through its serde integration, with events re-tagged on the socket
- JSON for everything, with `LoreBytes` encoded as text

## Decision Outcome

Chosen option: "Bitcode through its native derive", because it is the only option that carries
every call without a second representation of the events, and it encodes fastest and smallest.

The socket carries one encoding, bitcode, under message protocol version 2. The argument structs
derive `bitcode::Encode` and `bitcode::Decode` and no serde. The events derive them and serde's
`Serialize`, which the CLI renders them as JSON through, and nothing reads them back from JSON. The C API pointer types implement the traits bitcode's derive builds on:
text and byte buffers cross as their bytes, and a `LoreBytes` decodes as a view of the message it
arrived in, which the receiver keeps alive for as long as the view is read. The service checks the
text of a call it receives as the caller's entry point did.

A call carries its caller's `LORE_GLOBAL_PATH` and `LORE_AUTH_PATH`, which the service reads for
it in place of its own, so a call finds the configuration and credentials it would have found in
the caller's process. What the service caches from a token store, the tokens and the connections
and authorizations obtained with them, it keys by that store.

### Consequences

- Good, because every call relays, including the storage calls that carry buffers.
- Good, because the arguments carry no serde code, and a call is encoded and decoded without the
  intermediate strings JSON needs.
- Bad, because bitcode has no field-level compatibility: a client and the service must come from
  the same build. The protocol version refuses a peer from either side of this change, but not two
  builds after it whose messages differ, and bitcode identifies an enum variant and a field by
  position, so such builds can read each other's messages as different values: a command added
  ahead of another shifts the variant the other decodes as.
- Bad, because the pointer types implement traits bitcode does not cover by its semver guarantees,
  so the workspace pins bitcode's version.
- Neutral, because `data_out` on the storage get items is still not relayed: the service cannot
  write into a caller's buffer, and a relayed get answers with `GET_DATA` events instead.
