---
type: improvement
---

- `lore-server`: Each presigned redeem request that carries a token logs one INFO line, `Presigned URL redemption`, whatever its outcome, so refused and not-found redemptions are logged above DEBUG. A request abandoned before its response, as by a timeout, is logged without a `status`. The line has the response `status`, `bytes_served` (the response's `Content-Length`), `client_ip` (the connection's peer address, with an IPv4-mapped IPv6 address shown as IPv4) and `token_hash`, the hex SHA-256 of the token. Once the token verifies, or fails only because it has expired, the line also has its `key_id` and `expires_at`. Minting a presigned URL logs `Minted presigned URL` at INFO with the minter's `user_id`, the repository, address, `key_id`, `expires_at` and the same `token_hash`. Searching the logs for a token's hash finds who minted it and every redemption of it.
