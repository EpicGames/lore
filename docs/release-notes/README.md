# Unreleased release notes

Each change adds one note here instead of editing `docs/release-notes.md`, so changes never
conflict over the same lines. When a release is cut, the release tooling collects the notes into
its section of `docs/release-notes.md` and deletes them.

## When to add a note

Add a note for every change to what Lore does, for anyone who runs the client, operates a server,
or builds on its API or data. That includes changes that aren't visible on the command line:
server behaviour, authentication and authorization, replication and storage, the protocol, the
revision data structures such as the merkle tree, performance and resource use, logging and
telemetry, and configuration. A refactor counts when it changes observable behaviour, timing,
errors or resource use.

Skip the note only when the change touches nothing that ships: tests, CI, build scripts and
developer tooling, documentation other than release notes, formatting, or a refactor that leaves
behaviour, output and stored data exactly as they were.

## Adding a note

Copy [`_template.md`](_template.md) to `docs/release-notes/<summary>.md`:

- Name it after what changed, in a few lowercase words joined by hyphens, such as
  `sync-keeps-view-escapes.md`. Don't name it after a branch, a ticket or a person.
- Set `type`. The release puts the note under the matching heading.
- Write the bullets as they should read in the release notes, each as
  `` - `code-area`: description ``, where the code area is the crate or component that changed,
  such as `lore-server`. Continuation
  lines are indented by two spaces, and there are no blank lines between bullets. Remove the
  template's comments.
- Describe the effect that a user, an operator or an integrator sees: the command, the behaviour,
  the result. Leave out crate-internal functions and types, byte sizes and mechanics.

Later work on the same change edits the same note.

## Types

- `breaking`: something built, scripted or stored against an earlier release stops working, or
  needs a migration. That covers the public C API (`lore.h`, its events and error codes); CLI
  commands, flags, output and exit codes; the wire protocol (protobuf definitions and the QUIC
  format); the format of stored data (the immutable store, `.lore/` files, metadata); server
  configuration; and limits or defaults that existing data depends on, such as chunking and
  compression. The code-review standard's
  [Irreversible changes](../developing/code-standards/code-review.md#irreversible-changes) lists
  the same classes. Say what a user, operator or integrator has to do.
- `feature`: a new capability, such as a command, flag, C API function, RPC or configuration
  option.
- `improvement`: existing behaviour is faster, uses less, scales further or is clearer, on the
  client or the server, including storage, replication and data structures.
- `fix`: something that was wrong now works. Start with "Fixed" and describe the symptom.

## Entries from before note files

Entries written into the `## Nightly` section of `docs/release-notes.md` before these notes existed
stay where they are: the next release turns that section into its own and adds the notes to it.
