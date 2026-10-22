---
status: proposed
date: 2026-10-21
deciders: Matt Hoffman, Mattias Jansson, Raghav Narula
informed: Łukasz Jastrzębski
---

# ADR-00025: Service version compatibility

## Context and Problem Statement

Lore can run in a client-service model, where the CLI binary runs in service mode and clients send commands for it to
handle rather than perform operations on their own. With possibly diverse clients on the user's machine (stale CLI
binaries, lore.dll files from editor plugins, etc.) the service needs to only accept commands from compatible clients.
Given [ADR-00022](00022-service-ipc-bitcode.md), we need to update our versioning to reject non-compatible clients due
to the serialized format no longer being self-describing.

## Decision Drivers

- Lore is designed to be embedded in tools through its various APIs. It's unrealistic to expect all such tools to always
  be on the same Lore version, so our service model needs to handle this.
- The Lore service receives commands from the client through `MessageToServer` structs that have
- `bitcode::Encode`/`bitcode::Decode` attributes.
- bitcode serialization is not self-describing so any changes that change the order of arguments cause
  potentially hidden deserialization issues.
- The commands that are being serialized are the Lore{Noun}{Verb}Args objects used as arguments in the C API, so any
  changes to the C API are also changes to the expected serialization format.
- The wire format for each command also includes a protocol version number, currently always version 2.

## Considered Options

### Versioning

- Message struct version derived at compile time. This requires no future manual work to ensure its accuracy, but has no
  ability to skip a version increment in the case of inconsequential changes like a rename.
- Magic number incremented every time the message type changes. This requires manual upkeep.
- Use client's semver. This makes any change to the Lore version, even between versions that don't change the C API,
  produce incompatible clients/services.

### Version handshake

- Startup handshake between client and service. The client includes versioning based on its `MessageToServer`
  type, and the service replies with versioning based on its `MessageToClient` type. If either the service or client
  receive an incompatible version they fail, with the service sending an error message.
    - The service's error message uses the coarser wire format version.
- Send the version prepended to every message.
    - This works best for the use case of one-shot commands.

Both of these require incrementing the existing wire format version from 2 to 3.

## Decision Outcome

Chosen option: "Startup handshake between client and service" with "Message struct version derived at compile time".
We are expecting use cases where a client will send many commands to the same service, which benefits from the
handshake, while one-shot commands which are impacted aren't as performance sensitive.
