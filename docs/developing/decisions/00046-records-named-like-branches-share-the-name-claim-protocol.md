---
status: accepted
date: 2026-11-23
---

# ADR-00046: Records named like branches share the branch name-claim protocol

## Context and Problem Statement

A branch was stored in the mutable store as two kinds of key: a metadata key per branch id, holding the hash of a metadata blob, and a mapping from each name, regardless of case, to the id that held it ([ADR-00002](00002-branch-tracking-in-mutable-store.md)). No compare-and-swap spanned the two, so a name claim, an archive and a restore that raced, or one that stopped between its writes, could leave a name mapped to the wrong branch. The branch protocol ordered the writes to prevent that. A claim recorded the name it claimed in the metadata (`restoring`) before it claimed, and a finishing swap removed the record. An archive stamped `archived` and swapped the metadata before it released the name. The next write of the branch settled a claim that stopped, marking its name `released` and releasing it.

Scopes, which the [successor-locks LEP](../../proposals/2026-06-19-successor-locks-unmergeable-files.md) added, were to be stored the same way: a scope metadata key per scope id and a scope name mapping. They needed the same protection, and they added a rename, which branches didn't have. How should the scope writes get it, without a second protocol beside the branch one?

## Decision Drivers

- One protocol for every record named like a branch, so that a fix to its ordering reaches every kind of record.
- The branch path keeps its behaviour, its futures and its allocations.
- Each kind of record names its refusals in its own error set: a scope refusal mustn't read as a branch one.
- Branch-only steps, such as the remote fallback, the ids derived from a name, latest revisions and dry runs, stay out of the shared code.

## Considered Options

- Generalize the whole branch protocol over a kind of record.
- Generalize the building blocks the protocol is made of, and compose each kind's lifecycle from them in the protocol's order.
- Write the scope protocol beside the branch one.

## Decision Outcome

Chosen option: "generalize the building blocks", because it shares every write the protocol orders and keeps each kind's own checks out of the shared code. Generalizing the whole protocol would thread branch-only steps through every scope write. A second protocol would let the two drift apart.

The building blocks are in `lore-revision/src/branch/record.rs`. They cover:

- the reads of a record's metadata key and of a name's mapping, under the function strings each kind gives `mutable_key` and `mutable_name_key` in `branch.rs`;
- compare-and-swap of a metadata key and a name mapping;
- the stamps and claim entries;
- the settle of a stopped claim;
- the archive's swap and release.

They are generic over a zero-sized `NamedRecord` type, which names a kind's key functions, error set and claim generation, so each kind compiles to code of its own. A swap that finds a metadata key holding another value fails with the kind's own error. The other functions answer the value a key held, or whether a release took the name, and leave the refusal to the caller.

A kind's writes follow these rules:

- A create swaps the record's metadata in from zero, then claims the name, and swaps the metadata back to zero where the claim fails.
- A write that claims a name for a record that has metadata records the name as `restoring` with a compare-and-swap before it claims, and finishes with a swap that removes the record. A scope's restore and rename do both, and a rename then releases its old name, marked `released` in the finishing swap.
- The next write of a record settles a claim that stopped. The settle moves on the time that keeps the record as it was: `archived` for an archived record, `restored` for a live one.
- A mapping to another record that has metadata holds the name, whatever its metadata says. Only that record's writes release the name. Only a mapping to an id without metadata is stale, and a claim swaps from it. Any wider rule, such as one that reads the other record's metadata to decide, lets that record release and claim the name again between the read and the swap.
- A claim succeeds where the mapping already holds the claim's own value.
- A scope's claims carry a generation. A scope name's mapping holds the scope id in bytes 0 to 16, so a reader that takes the id from the first 16 bytes is unaffected, and the generation of the claim that wrote it in the next 8 bytes, as a little-endian `u64`. Each new claim takes the scope's next generation in the same metadata swap that records it: create's first metadata, or the swap that records `restoring`. The metadata records the generation of the claim of the name it names, of the `restoring` claim and of the `released` claim, and a settle marks a stopped claim `released` with the generation that claim recorded. Every release swaps from the exact id and generation the metadata it read records, so a release that races a later claim of the same name by the same scope fails against it. A claim that finds the mapping holding an older claim of its own scope swaps from that value to its own.
- A branch's mapping holds the bare id, and its claims carry no generation: the branch `NamedRecord` has a zero-sized generation and writes the id alone. Servers that predate generations compare the whole mapping value when they release a branch name, so a generation in a branch mapping would make their releases fail in a fleet of mixed versions or after a rollback. This leaves a branch release that races a later claim of the same name by the same branch, which reads the metadata before the claim records itself and swaps after it, able to erase that claim on the settle path.
- Each kind's writes stop at the `lore_revision::test_util::BranchStep` points. A scope's release also stops between its read of the metadata and its swap. Every order of every pair of them is driven in that kind's race tests.

### Consequences

- Good, because a change to the order of the writes is made once, and the race tests of every kind check it.
- Good, because the branch functions keep their signatures, errors and stored mapping values, and the futures that await them hold no more than before.
- Bad, because a name stays taken while a write of its record that stopped before its release is unsettled, until the next write of that record. Until then, a lookup by the name finds nothing.
- Bad, because a create that stops between its two writes leaves metadata that no name maps to. A repeated create of the record claims the name, where no other record took it first.
