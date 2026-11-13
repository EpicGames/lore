---
type: fix
---

- `lore`: Fixed the revision tree calls checking the payloads they fetch from the remote as each
  call's `globals.trust_remote` said, rather than as the storage handle the tree was loaded on
  trusts its remote. A call that left `globals.trust_remote` unset, on a handle opened with
  `skip_verify` or `globals.trust_remote`, hashed every payload it fetched, and a call that set
  `globals.trust_remote` on a handle that doesn't trust its remote checked only the sizes. Every
  revision tree call trusts the remote as its storage handle does.
