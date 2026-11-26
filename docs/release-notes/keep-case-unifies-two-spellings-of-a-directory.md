---
type: fix
---

- `lore-revision`: Fixed `lore stage --case keep` (`lore_file_stage` with keep-case) losing files
  on a case-sensitive file system when the paths staged spell one directory in two cases and the
  working tree holds both. A new file under the other spelling was skipped, or without `--force`
  failed the stage with `Invalid path`, and a changed tracked file there was staged as deleted.
  The stage now merges the other spelling into the one the repository holds before staging the
  files below it.
- `lore-revision`: Fixed `lore stage --case keep` on a case-sensitive file system replacing a
  file with another spelling of its name, and deleting that spelling. A rename to another
  spelling of a name, of a file or of a directory merged into the one already there, no longer
  replaces a file the destination holds. A file with identical content is removed from the
  source, and one with different content fails the operation and leaves both files in place.
- `lore-revision`: Fixed `lore stage --case keep --dry-run` on a case-sensitive file system
  reporting files as deleted, or failing with `Invalid path`, below a directory the working tree
  spells in another case than the repository. The dry run moves nothing and reports what the
  stage would: each file is read from where it stands, and a file the stage would refuse to
  replace fails the dry run too.
