---
type: fix
---

- `lore-revision`: Fixed setting or removing branch or repository metadata treating a blob as
  already on the server where the server's answer about which blobs it holds left that blob out.
  The change now fails with an internal error rather than set metadata that names a blob the
  server may lack.
