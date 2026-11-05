---
type: fix
---

- `lore-revision`: Fixed `lore stage move` of a file or directory out of a directory other than the root keeping it in
  both places: the next commit kept the directory it left as it was. A merge or restore that moved a file or directory
  out of a directory could do the same
- `lore-revision`: Fixed moves dropping changes already recorded on what they move. Moving a directory with
  `lore stage move` or `lore dirty move` no longer turns a child staged or marked for deletion into a moved one, and
  moving a directory over the directory that holds it with `lore stage move` keeps the deletions and additions staged
  below it. A file or directory added but not yet committed stays an addition when moved: `lore dirty move` recorded
  one a merge added as moved, and `lore stage move` did the same to one marked as added with `lore dirty`
- `lore-revision`: Fixed `lore stage move` leaving a directory unmarked when it moved a file or directory marked dirty
  into it and the directory was clean, so `lore status` and `lore stage` of a directory above it missed the moved
  changes
- `lore-revision`: Fixed `lore stage move` of a file or directory renamed in case alone, such as `Dir` to `dir`,
  reporting the file, or every file below the directory, as deleted and leaving each marked dirty
- `lore-revision`: Fixed `lore stage move` and `lore dirty move` of a directory into a directory below it never
  returning; they fail with an invalid argument error
- `lore-revision`: Fixed `lore commit` dropping the dirty mark of an unstaged change below a directory staged as moved
  or deleted, so `lore status` without `--scan` missed the change after the commit
- `lore-revision`: Fixed marking a change below a staged directory, with `lore dirty`, a scan or `lore stage`,
  dropping what was staged for that directory
- `lore-revision`: Fixed a scan limited to a path, such as `lore status --scan a/b/c`, that enters directories the tree
  lacks, leaving the directory above them unmarked, so `lore status` and `lore stage` of a directory above it without
  `--scan` missed the directories it added. Every name on the path is now checked before any is added, so a name the
  repository refuses adds none of the directories above it
- `lore-revision`: Fixed `lore unstage` of an added link clearing the change marks of every directory above it, which
  hid a file changed beside the link from `lore status` without `--scan`, and, once nothing below the root was marked,
  dropped the staged state holding it
- `lore-revision`: Fixed `lore stage --case rename` dropping the dirty mark of a file it takes back from a staged
  deletion and renames in case alone
- `lore-revision`: Fixed committing, `lore reset` and `lore dirty` leaving the nodes below an added directory they
  remove allocated in the saved state; they're now freed with it
- `lore-revision`: Fixed staging a file whose conflict was resolved turning it back into an unresolved conflict: a
  forced `lore stage`, `lore stage --case rename`, `lore stage move` or a staged deletion of the file kept the conflict
  and dropped its resolution and side. Staging a file again that was staged for deletion also keeps its merge state,
  where it cleared it
- `lore-revision`: Fixed `lore branch merge unresolve`, `lore revision cherry-pick unresolve` and
  `lore revision revert unresolve` of a file resolved with `mine` or `theirs` keeping the side the resolve took, so
  `lore status` still reported that side for the file in conflict again
- `lore-revision`: Fixed a directory being added again under a name the tree already holds when reading the tree failed
  part way. `lore status --scan`, `lore stage --scan` and `lore dirty` took a directory whose lookup couldn't be read
  for a missing one and added it a second time, leaving two entries of one name. They now stop with the error the read
  answered
- `lore`: Fixed two `lore_revision_tree_delete` calls on one handle, of a subtree and of a node below it, running at
  once, so the second staged less and reported a smaller count. They run one after the other, and the second rejects
  with `INVALID_ARGUMENTS` a node the first staged or detached. A call reports its events, a rejection included, once
  its batch is done, where it reported a rejection as it planned
- `lore`: Fixed `lore_revision_tree_delete` freeing the nodes of an added subtree at once, while an add or a move on the
  same handle could still reach them. The next `lore_revision_tree_commit` that goes ahead frees them, so adding again
  before that commit takes new nodes
- `lore`: Fixed `lore_revision_tree_move` calls on one handle, and a move beside a `lore_revision_tree_delete`, running
  at once: two calls moving nodes that shared a parent chain could leave a node in one directory's chain while its
  record named another, which failed the next `lore_revision_tree_commit`. They run one after the other, each checking
  its entries against the tree the one before left
- `lore`: Fixed a revision-tree handle committing many times over adds and deletes growing its tree's name tables
  until adding failed: once a commit freed a deleted node, no later commit dropped a name from that node's block while
  the handle kept the block in memory. Each commit now drops the names of freed nodes
- `lore`: Fixed `lore_revision_tree_list_children` failing when a child moved away while it listed the directory; it
  walks the directory again instead, so beside moves or deletions on the same handle it may report a child twice,
  which a caller drops by node ID, or leave out one moved or deleted meanwhile
- `lore`: Fixed the C API's revision-tree verbs reporting an entry the tree gave up on under concurrent changes as
  `LORE_ERROR_CODE_INTERNAL`; they report `LORE_ERROR_CODE_SLOW_DOWN`, which a caller can retry
- `lore-base`: Fixed `lore status --count` and the C API's revision-tree verbs reading the processor count they size
  their parallel work from on every call, on Linux from the cgroup files; they read it once per process
