---
type: fix
---

- `lore-revision`: Fixed `lore reset` and `lore unstage --force` of a link whose mount a file replaced writing the
  linked repository's whole tree into the mount when the link exposes a sub-folder; the mount now holds that
  sub-folder alone.
- `lore-revision`: Fixed a sync to a revision that adds a link leaving an empty mount directory, which
  `lore status --scan` reported as an addition, when the linked repository can't be fetched. That sync, and a reset
  of a removed link that fails to fetch the linked repository, now name the mount in the error.
- `lore-revision`: Fixed the refusal to reset a file added inside a link reading as if no reset inside a link were
  supported; it now reads `Resetting a file added inside a link at <path>`.
