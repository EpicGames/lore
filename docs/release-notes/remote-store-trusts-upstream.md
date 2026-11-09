---
type: breaking
---

- `lore-server`: a `remote` immutable store trusts its upstream by default. It checks only the
  sizes of a payload it fetches, where it decoded and hashed every payload against its address.
  Clients still check each payload they fetch, except a client that trusts its remote
  (`--trust-remote`, or `trust_remote` or `skip_verify` through the C API), which checks only its
  sizes. A heal a client asks for now reaches the upstream too: the server drops its own copy of
  the payload only if that copy is corrupt, and asks the upstream to heal its copy where the
  server's own was corrupt or missing. A heal that arrives while the server writes a fetched copy
  of the payload to its local tier waits for that write, so it checks the copy the write leaves.
  To keep checking each payload on the server, set `verify = true` in the store's `remote` table.
