---
status: accepted
date: 2026-10-14
amended: 2026-11-08
deciders: Mattias Jansson
---

# ADR-00023: Data verification and trust contracts

## Context and Problem Statement

Lore moves content-addressed payloads between machines, and a payload is only as good as the check
that it hashes to the address naming it. Lore had no stated contract for when that check is made.
Each read and transfer path grew its own, with its own flag or option:

- `ReadOptions::verify`, on by default, hashed every local load, the path that is taken most and
  has to cost the least.
- `ReadOptions::trust_local` and the repository's `trust_local` setting opted a context out of
  that hash, which was sound only while nothing unchecked had filled its store.
- The client cache took a `verify` parameter. Without it, it stored what it fetched unchecked unless
  a read joined the same fetch, which the read flight carried as deferred states
  (`Flown::Unverified`, `Flown::Rejected`).
- `read_raw` hashed what it loaded under debug assertions, unless its caller said it would.
- The C API's `skip_verify` turned off the local hash for a storage handle.
- A server checked every payload its clients pushed, but a server reaching a backend through the
  storage protocol served what it fetched with only its sizes checked, and hashed it only where
  one of its own reads loaded it.

Whether a payload was ever checked followed from how these combined on its path, not from where it
came from. This record states the contract: which sources are trusted, what is checked on ingress
and how, and what stored data is trusted for. It replaces those flags and options with one rule:
the trust of a payload's source decides whether it is checked.

A server cluster, its servers with their read and write replicas and their durable store, is one
coherent backend: its members are set up by one configuration, replicas reach each other over TLS,
and the durable store is reached by configuration. Clients are outside the backend, and so are
look-aside caches and auxiliary servers, which reach a backend through the storage protocol as its
clients do (a `remote` store, `RemoteImmutableStore`).

**Amended 2026-11-08:** a server that reaches a backend through a `remote` store counts as within
that backend, for data integrity only. The case that settles it is the machine server
([machine-level Lore server
proposal](../../proposals/2026-10-12-machine-level-lore-server-for-containers.md)): a Lore server on
a build machine that caches content for the machine's containers, with its upstream as the durable
tier of a composite store. It fronts that backend for the containers, whose clients still check
what they fetch from it. Authorization does not change: the machine server authorizes each
container's storage sessions with the container's own tokens, and the upstream authorizes the
machine server's identity per partition and every forwarded request for the caller's token. The
trust needs no client certificate, as replicas reach each other with, because it grants no access.

## Decision Drivers

- A payload from outside a machine's backend is not trusted.
- The members of one backend trust each other: they are configured and authenticated as one, and
  checking transfers between them pays for nothing a client's check does not catch.
- A machine trusts the data it has stored, since it was hashed from its content there, checked on
  ingress, or came from a source trusted where it was stored: its own backend, or a remote a
  command declared trusted or a read took payloads from as stored.
- A payload's sizes are a different check from its hash: they are read off its fragment, cost
  nothing beside the load, and are checked on every payload from every source.
- A local read costs the load and the decode, and nothing more.
- Whether a payload is checked follows from the trust of its source, declared for a whole command,
  never chosen per read. A caller that takes payloads as stored checks them itself. A client
  within a backend's own infrastructure, where performance comes first, can declare that backend
  trusted.
- A payload checked on ingress is decompressed once along its call chain, and the content the check
  expanded is handed to a caller that wants it.

## Considered Options

- Keep a check per path, each with its own flag or option, as before this decision.
- Hash every local load and check every fetch.
- Check every payload that moves between machines, within a backend as well.
- Check every payload from an untrusted source on ingress, and trust stored data and the backend.

## Decision Outcome

Chosen option: "Check every payload from an untrusted source on ingress, and trust stored data and
the backend", because it checks each payload once, where it enters from outside a backend, it is
decided by the source alone, and neither local reads nor transfers within a backend pay for it.

The contract:

- Untrusted sources: to a client, every server; to a look-aside cache or an auxiliary server, the
  backend it reaches through the storage protocol; to a server, its clients. **Amended
  2026-11-08:** to a server, the backend it reaches through a `remote` store only where that store
  sets `verify = true`.
- Every payload from an untrusted source is decompressed and its content hashed against its address
  on ingress, before it is stored or handed on. A client checks what it fetches for reads, caches,
  prefetches and diagnostic queries. A server checks what its clients push, and what it fetches
  through the storage protocol. **Amended 2026-11-08:** a server checks what it fetches through a
  `remote` store only where the store sets `verify = true`. A fetched payload that fails is healed once where the source can
  heal it, and is otherwise an error; a pushed payload that fails is refused. A diagnostic query
  logs one that fails and asks no heal.
- Trusted sources: a server's replicas and peers, trusted by configuration and TLS, and its durable
  store, trusted by configuration. What a server fetches from them or has replicated to it is
  stored and served as it arrives, neither decompressed nor hashed. **Amended 2026-11-08:** so is
  the backend a server reaches through a `remote` store, trusted by configuration unless the store
  sets `verify = true`. The store checks the sizes of what it fetches, and stores and serves it as
  it arrives. A client that finds such a payload corrupt asks the server to heal it, and the server
  passes the heal on to that backend where its own copy was corrupt or missing.
- A client can declare its server a trusted source where both are part of the same infrastructure
  and performance comes first, such as a build server streaming from a server in its own backend,
  with the global `trust_remote` (`--trust-remote`), which a storage handle also takes as
  `skip_verify`. Its fetches then have only their sizes checked against their length, and are
  decompressed only where the caller reads their content. What such a command stores is the
  machine's own stored data from then on, trusted by every later read.
- A read that takes payloads as stored, the C API's `fragments` delivery, trusts the remote: no
  leaf is decoded or hashed on its way, and the caller checks what it receives. What it stores is
  trusted as what a trusting command stores is.
- A machine trusts its own stored data, and no local store read hashes its payload. A payload whose
  sizes disagree or that fails to decompress is fetched again and overwritten where an upstream
  holds it.
- Every payload, trusted or not and stored data among it, has the sizes its fragment gives
  checked against its length and, for a plain payload, which is the content, against each other.
  The check inspects the fragment alone, never the content.
- Explicit verification of a store (`lore repository verify`, a server's heal) hashes every
  payload it reads.

`trust_remote` is the one switch the contract leaves. `ReadOptions::verify`,
`ReadOptions::trust_local`, the repository's `trust_local` setting, the cache's `verify` parameter,
the read flight's deferred states and `read_raw`'s debug hash are removed, and `skip_verify` is kept
as an alias of `trust_remote`. **Amended 2026-11-08:** a server's `remote` store has the
counterpart, its `verify` setting, off by default, which makes the backend it reaches untrusted.

The integrity of stored data over time, a payload that decays on its media and still decodes, is a
separate concern, left to the data store integrity feature.

### Consequences

- Good, because every payload a machine stores was hashed from its content there, checked on
  ingress, or came from a source trusted where it was stored, and which applies follows from its
  source alone.
- Good, because local reads, ProjFS hydration among them, do not hash, and neither do transfers
  within a backend.
- Good, because a payload corrupted within a backend is still caught by the client that fetches it,
  which asks the server to heal it.
- Good, because the scattered flags, options and deferred states are gone, together with the
  combinations of them a path had to be reasoned through.
- Neutral, because a storage handle's `skip_verify` keeps its field, as an alias of
  `trust_remote`.
- Bad, because a payload corrupted within a backend is served from it until a client catches it or
  the store holding it is verified, and a client that declared the backend trusted does not catch
  it.
- Bad, because caches, prefetches and fetches through the storage protocol decompress and hash
  every payload they receive from an untrusted source. Caches and prefetches run off the path a
  command waits on.
- **Amended 2026-11-08:** Good, because a machine server's misses reach its containers without a
  decode and a hash on the server, and each container's client still checks what it fetches.
- **Amended 2026-11-08:** Bad, because a payload corrupted at the upstream reaches a machine
  server's local tier as it arrived, and the machine server serves it to every container until a
  client that checks what it fetches catches it and asks the heal. A client that trusts its remote
  does not catch it.
- **Amended 2026-11-08:** Bad, because a `remote` store that checked every payload before this
  amendment trusts its backend after it, unless its configuration sets `verify = true`.
- Bad, because a payload is checked by decoding it: a machine built without the codec a payload
  from an untrusted source is stored in refuses that payload.
- Bad, because a server's own reads through a remote store take a payload that still fails its
  check after the heal as missing, as they take any failure of that store other than
  `NotSupported`, so its HTTP endpoints answer a root that fails as not found, and a leaf that
  fails as an error in the content stream. A client fetching through the server receives a store
  failure, which it retries as a lost session and then reports as not connected.

## More Information

The contract lands in two changes: the client side together with this record, and the check of what
a server fetches through the storage protocol.
