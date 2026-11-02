---
# Copy this file to docs/release-notes/<summary>.md, where <summary> is a few lowercase words
# joined by hyphens that name what changed, such as sync-keeps-view-escapes.md. Don't name it
# after a branch, a ticket or a person.
#
# Set type to one of:
# breaking: something built, scripted or stored against an earlier release stops working, or
#   needs a migration: the public C API (lore.h, events, error codes), CLI commands, flags,
#   output or exit codes, the wire protocol (protobuf, QUIC), the format of stored data
#   (immutable store, .lore/, metadata), server configuration, or a limit or default that
#   existing data depends on (chunking, compression). Say what to do.
# feature: a new capability: a command, flag, C API function, RPC, or configuration option.
# improvement: existing behaviour is faster, uses less, scales further or is clearer, on the
#   client or the server, including storage, replication and data structures.
# fix: something that was wrong now works. Start with "Fixed" and the symptom.
type: fix
---

- `code-area`: description.
