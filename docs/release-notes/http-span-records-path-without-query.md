---
type: fix
---

- `lore-server`: Fixed the `urc` request span recording the full request URI, which put the token of a presigned redeem URL into traces. The span's `uri` field now holds the request path without its query.
