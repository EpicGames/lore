---
type: breaking
---

- `lore`: `lore_revision_tree_add` and `lore_revision_tree_move` answer an entry whose name is longer than a node name may be with `LORE_ERROR_CODE_OVERSIZED` (118), in the entry's event, the batch terminal and the call's status, rather than `LORE_ERROR_CODE_INVALID_ARGUMENTS` (3). `lore_error_code_t` gains `LORE_ERROR_CODE_OVERSIZED` for it. Check for 118 where a caller checked for 3 on a long name; any other bad name still answers 3.
- `lore-revision`: A move, rename or staged move to a name longer than a node name may be, or one its node's name table has no room for, fails as `Oversized` (118) rather than `InvalidArguments` (3), as an add of such a name does. `lore_revision_tree_move` reports a move that meets a full name table as `LORE_ERROR_CODE_INTERNAL`, as before, and names `Oversized` in its completion detail.
