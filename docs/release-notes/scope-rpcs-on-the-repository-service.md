---
type: feature
---

- `lore-server`: the repository service answers `ScopeCreate`, `ScopeGet`, `ScopeRename` and `ScopeArchive`, which
  manage the scopes of the repository a request's metadata names. A scope has an id its creator generates and a name
  unique among live scopes regardless of case. Creating one needs write access, and a repeated create with the same id
  and name answers the existing scope. A create of an archived scope's id restores the scope under the requested name,
  with write access, as a create of an archived branch's id restores the branch. Renaming and archiving a scope need
  repository administration, or to be the scope's creator. Every repository has a default scope, named `default`,
  whose id derives from the repository's: it is answered without being stored, its name is reserved, and it can't be
  renamed or archived. Nothing enforces scopes yet. The server counts each create, rename, archive and restore that
  succeeds in `scope.lifecycle`, by `verb`. A client against an older server gets `UNIMPLEMENTED`.
- `lore-server`: the generic mutable-store store and compare-and-swap refuse the scope key types, which only the scope
  requests write.
- `lore-capi`: `lore_key_type_t` gains `LORE_KEY_TYPE_SCOPE_METADATA` (8) and `LORE_KEY_TYPE_SCOPE_ID` (9), and the
  error codes gain `AlreadyExists` (59), a generic code for a resource that exists already or is held by another.
  The scope RPCs answer `ALREADY_EXISTS` for a taken name, a create that repeats a live scope's id with another
  request, and a rename or restore that another write of the scope recorded itself ahead of.
