---
type: breaking
---

- `lore-client`: `lore branch list` lists the local branches only. Pass `--remote` to list the
  remote's branches in their place. Scripts that read the `Remote branches:` section or the
  `remote` entries of the default listing must run `lore branch list --remote` for them.
- `lore`: `lore_branch_list` sends the `LORE_EVENT_BRANCH_LIST_BEGIN`, `ENTRY` and `END` events for
  the local branches only, unless `remote` is set in `lore_global_args_t`, which lists the remote's
  branches in their place. Callers that need both lists make a second call with `remote` set.
