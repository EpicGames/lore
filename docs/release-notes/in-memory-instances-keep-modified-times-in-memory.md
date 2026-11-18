---
type: improvement
---

- `lore-revision`: An instance opened with in-memory stores (the `in_memory` global argument of the
  C API), other than an SWFS instance, keeps the modification times it records for its files in
  memory until its stores are released, instead of in its in-memory mutable store, so they take less
  memory.
