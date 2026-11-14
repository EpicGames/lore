---
type: feature
---

- `lore-capi`: `lore_storage_put_file_item_t` and `lore_storage_put_file_resolved_item_t` have a
  new `skip_holes` field. With it set, the put reads only the parts of the file that the file
  system holds data for, and records each hole as a hole in the stored content. A hole is a range
  holding no data: never written, or allocated and never written, such as a range `fallocate`
  reserved. On Linux the holes come from the file system's extent map, so what the page cache
  holds does not change them. Holes are never read, hashed, stored or uploaded, so a large sparse
  image costs what its data costs. Zeros written to the file are data and are stored as such, so a
  file with a hole and the same file with zeros written over it get different addresses. A file
  larger than one fragment that has holes gets a different address than it gets without the field,
  and a file with none gets the same address. `lore_storage_get_file` empties its temporary file
  before writing and leaves each hole unwritten, so holes in the source come back as holes
  wherever the target file system supports them, and written zeros come back as written data. A
  put without `skip_holes` stores holes as data, which a get writes back. Other reads return zeros
  for a hole, and fragment delivery sends a hole as a `GET_FRAGMENT` with no payload. Clients
  built before this change refuse content stored with holes with an error on every read that
  reaches a hole; servers built before it store and serve such content, which those clients still
  refuse. The exception is content that is one hole, a file larger than one fragment that holds no
  data: its list holds one reference, which a server built before this change refuses with
  `InvalidFragment`. A put of such content with `remote_write` to that server still completes,
  reporting `stored_remote` clear with no error, and the content stays local only, so a caller
  that needs it on the server checks `stored_remote`. A deployment upgrades every client that
  reads such content before any caller sets `skip_holes`. Content stored without holes is
  unchanged.
