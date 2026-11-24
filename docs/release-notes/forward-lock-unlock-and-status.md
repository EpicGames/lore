---
type: feature
---

- `lore-server`: Forward the lock service's `Lock`, `Status` and `Unlock` to a remote Lore server,
  opt-in via `lock_lock`, `lock_status` and `lock_unlock` under
  `[server.grpc_public_services.forwarded_requests.enabled_rpcs]`. The forwarding server authorizes
  the caller first, and the peer takes or releases the locks and sends their notifications. The
  peer serves them only when it has a lock store and runs this release; upgrade it before turning
  a flag on.
